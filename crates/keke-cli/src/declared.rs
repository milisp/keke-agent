//! Registering endpoints declared in configuration.
//!
//! The three wire formats are implemented once in `keke-wire`, so a vendor that
//! needs no behavior of its own — an ollama box, an OpenAI-compatible gateway —
//! is a base URL, a credential name, and a format. That is a config entry, not a
//! crate, and this turns each entry into a registered route.

use std::sync::Arc;

use keke_auth_api::AuthProvider;
use keke_auth_api::CredentialRef;
use keke_auth_api::CredentialStore;
use keke_catalog::CatalogCache;
use keke_config_types::DeclaredWireApi;
use keke_config_types::ProviderDeclaration;
use keke_provider_api::ArcProvider;
use keke_provider_api::ModelInfo;
use keke_provider_api::ModelProvider;
use keke_provider_api::ModelRequest;
use keke_provider_api::ProviderError;
use keke_provider_api::ProviderFuture;
use keke_provider_api::ProviderInfo;
use keke_provider_api::StreamEvent;
use keke_provider_api::WireApi;
use keke_wire::WireClient;

use crate::api_key::ApiKeyAuth;
use crate::api_key::NoAuth;

/// Why a declaration could not become a route.
#[derive(Debug, thiserror::Error)]
pub(crate) enum DeclarationError {
    /// A credential name that is not a shell identifier cannot be resolved from
    /// the environment, so it would silently never authenticate.
    #[error("provider `{route}`: `{key}` is not a usable credential name")]
    BadCredentialName { route: String, key: String },
    /// `ca_cert_path` did not name a readable, parseable PEM file.
    #[error("provider `{route}`: could not load CA certificate `{path}`: {error}")]
    BadCaCert {
        route: String,
        path: String,
        error: String,
    },
    /// `proxy` was not a usable proxy URL.
    #[error("provider `{route}`: invalid proxy `{proxy}`: {error}")]
    BadProxy {
        route: String,
        proxy: String,
        error: String,
    },
    /// The HTTP client for this provider could not be built from its
    /// configured CA certificate and proxy.
    #[error("provider `{route}`: could not build an HTTP client: {error}")]
    BadHttpClient { route: String, error: String },
    /// `headers` named `authorization`, which is reserved for the provider's
    /// own credential — a custom header can never be allowed to shadow it.
    #[error(
        "provider `{route}`: `headers` may not set `{header}`, which is reserved for the provider's credential"
    )]
    ReservedHeader { route: String, header: String },
    /// A header value of the form `env:VAR_NAME` named an environment
    /// variable that is not set.
    #[error(
        "provider `{route}`: header `{header}` names environment variable `{key}`, which is not set"
    )]
    MissingHeaderEnv {
        route: String,
        header: String,
        key: String,
    },
}

/// A provider assembled from configuration.
struct DeclaredProvider {
    info: ProviderInfo,
    api: WireApi,
    client: WireClient,
    /// `None` disables caching, which is what a surface with no home
    /// directory — a test — gets.
    cache: Option<CatalogCache>,
}

impl ModelProvider for DeclaredProvider {
    fn info(&self) -> &ProviderInfo {
        &self.info
    }

    fn stream<'a>(
        &'a self,
        request: ModelRequest,
    ) -> ProviderFuture<'a, Result<StreamEvent, ProviderError>> {
        Box::pin(async move { self.client.stream(self.api, request).await })
    }

    fn list_models(&self) -> ProviderFuture<'_, Result<Vec<ModelInfo>, ProviderError>> {
        Box::pin(async move {
            let route = self.info.route.as_str();
            let cached = self.cache.as_ref().and_then(|cache| cache.load(route));
            if let Some(cached) = &cached
                && cached.fresh
            {
                return Ok(cached.models.clone());
            }
            match self.client.list_models(self.api).await {
                // A remote gateway can answer slowly or not at all, and this
                // sits on the path of session start — so what it said last
                // time is shown while the ask is retried next turn, rather
                // than the interface waiting on it again.
                Ok(models) if !models.is_empty() => {
                    if let Some(cache) = &self.cache {
                        cache.store(route, &models);
                    }
                    Ok(models)
                }
                listing => {
                    if let Err(error) = &listing {
                        tracing::debug!(%route, %error, "could not list models; using what is on hand");
                    }
                    Ok(cached.map(|cached| cached.models).unwrap_or_default())
                }
            }
        })
    }
}

/// Wrap a `WireClient` as a provider, for a vendor whose only behavior is its
/// endpoint and credential — including compiled-in ones like codex.
pub(crate) fn wire_provider(info: ProviderInfo, auth: Arc<dyn AuthProvider>) -> ArcProvider {
    wire_provider_with(info, auth, false)
}

/// [`wire_provider`] for an endpoint that fixes its own sampling — a
/// subscription backend, which refuses a request that names a reply budget or
/// a temperature. Only the composition root knows which addresses those are.
pub(crate) fn wire_provider_with(
    info: ProviderInfo,
    auth: Arc<dyn AuthProvider>,
    sampling_is_fixed: bool,
) -> ArcProvider {
    wire_provider_cached(info, auth, sampling_is_fixed, None)
}

/// [`wire_provider_with`] with a model-list cache under keke's home.
pub(crate) fn wire_provider_cached(
    info: ProviderInfo,
    auth: Arc<dyn AuthProvider>,
    sampling_is_fixed: bool,
    cache: Option<CatalogCache>,
) -> ArcProvider {
    let api = info.wire_api;
    let mut client = WireClient::new(info.base_url.clone(), auth);
    if sampling_is_fixed {
        client = client.with_fixed_sampling();
    }
    Arc::new(DeclaredProvider {
        info,
        api,
        client,
        cache,
    })
}

/// Build a route from one declaration.
///
/// The resulting provider reports `auth_id: None`: it has no login flow, so
/// there is nothing for `keke login` to name. Surfaces report on it through
/// `ProviderInfo::env_key` instead — see `crate::api_key`.
pub(crate) fn provider_for_cached(
    declaration: &ProviderDeclaration,
    credentials: &Arc<dyn CredentialStore>,
    cache: Option<CatalogCache>,
) -> Result<ArcProvider, DeclarationError> {
    // A declaration with no credential name names a local endpoint that wants
    // none. Demanding one here would make the commonest declared provider — an
    // ollama server on the same machine — impossible to configure.
    let auth: Arc<dyn AuthProvider> = match declaration.env_key.as_deref() {
        None => Arc::new(NoAuth),
        Some(key) => {
            let reference =
                CredentialRef::new(key).map_err(|_| DeclarationError::BadCredentialName {
                    route: declaration.route.clone(),
                    key: key.to_string(),
                })?;
            Arc::new(ApiKeyAuth::new(reference, Arc::clone(credentials)))
        }
    };

    let info = ProviderInfo {
        route: declaration.route.clone(),
        display_name: declaration
            .display_name
            .clone()
            .unwrap_or_else(|| declaration.route.clone()),
        base_url: declaration.base_url.clone(),
        wire_api: wire_api(declaration.wire),
        auth_id: None,
        env_key: declaration.env_key.clone(),
    };

    let http = build_http_client(declaration)?;
    let extra_headers = extra_headers(declaration)?;
    let api = info.wire_api;
    let client = WireClient::with_http_client(info.base_url.clone(), auth, http)
        .with_extra_headers(extra_headers);
    Ok(Arc::new(DeclaredProvider {
        info,
        api,
        client,
        cache,
    }))
}

/// Build the `reqwest::Client` for one declaration, applying its CA
/// certificate and proxy — the two settings that decide whether a declared
/// endpoint is reachable at all from behind a corporate network.
fn build_http_client(
    declaration: &ProviderDeclaration,
) -> Result<reqwest::Client, DeclarationError> {
    let mut builder = reqwest::Client::builder();

    if let Some(path) = &declaration.ca_cert_path {
        let pem = std::fs::read(path).map_err(|error| DeclarationError::BadCaCert {
            route: declaration.route.clone(),
            path: path.clone(),
            error: error.to_string(),
        })?;
        let cert =
            reqwest::Certificate::from_pem(&pem).map_err(|error| DeclarationError::BadCaCert {
                route: declaration.route.clone(),
                path: path.clone(),
                error: error.to_string(),
            })?;
        builder = builder.add_root_certificate(cert);
    }

    if let Some(proxy_url) = &declaration.proxy {
        let mut proxy =
            reqwest::Proxy::all(proxy_url).map_err(|error| DeclarationError::BadProxy {
                route: declaration.route.clone(),
                proxy: proxy_url.clone(),
                error: error.to_string(),
            })?;
        if let Some(username) = &declaration.proxy_username {
            let password = match &declaration.proxy_password_env_key {
                Some(key) => std::env::var(key).unwrap_or_default(),
                None => String::new(),
            };
            proxy = proxy.basic_auth(username, &password);
        }
        builder = builder.proxy(proxy);
    }

    builder
        .build()
        .map_err(|error| DeclarationError::BadHttpClient {
            route: declaration.route.clone(),
            error: error.to_string(),
        })
}

/// Resolve a declaration's custom headers, expanding an `env:VAR_NAME` value
/// from the environment so a header carrying a secret need not sit in the
/// config file in the clear.
fn extra_headers(
    declaration: &ProviderDeclaration,
) -> Result<Vec<(String, String)>, DeclarationError> {
    declaration
        .headers
        .iter()
        .map(|(name, value)| {
            if name.eq_ignore_ascii_case("authorization") {
                return Err(DeclarationError::ReservedHeader {
                    route: declaration.route.clone(),
                    header: name.clone(),
                });
            }
            let resolved = match value.strip_prefix("env:") {
                Some(key) => {
                    std::env::var(key).map_err(|_| DeclarationError::MissingHeaderEnv {
                        route: declaration.route.clone(),
                        header: name.clone(),
                        key: key.to_string(),
                    })?
                }
                None => value.clone(),
            };
            Ok((name.clone(), resolved))
        })
        .collect()
}

/// Config states its own format enum so `keke-config-types` need not depend on
/// the provider contract; this is the one place the two meet.
fn wire_api(declared: DeclaredWireApi) -> WireApi {
    match declared {
        DeclaredWireApi::ChatCompletions => WireApi::ChatCompletions,
        DeclaredWireApi::Responses => WireApi::Responses,
        DeclaredWireApi::Messages => WireApi::Messages,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    fn declaration(headers: BTreeMap<String, String>) -> ProviderDeclaration {
        ProviderDeclaration {
            route: "gateway".to_string(),
            display_name: None,
            base_url: "https://gateway.example/v1".to_string(),
            wire: DeclaredWireApi::ChatCompletions,
            env_key: None,
            default_model: None,
            ca_cert_path: None,
            proxy: None,
            proxy_username: None,
            proxy_password_env_key: None,
            headers,
        }
    }

    #[test]
    fn a_literal_header_is_sent_as_written() {
        let declared = declaration(BTreeMap::from([(
            "X-Company-User-Id".to_string(),
            "milisp-labs".to_string(),
        )]));
        let headers = extra_headers(&declared).expect("resolves");
        assert_eq!(
            headers,
            vec![("X-Company-User-Id".to_string(), "milisp-labs".to_string())]
        );
    }

    #[test]
    fn an_env_header_is_resolved_from_the_environment() {
        // `PATH` is set in every test process, so this exercises the `env:`
        // prefix without mutating shared process state.
        let declared = declaration(BTreeMap::from([(
            "X-Forwarded-Path".to_string(),
            "env:PATH".to_string(),
        )]));
        let headers = extra_headers(&declared).expect("resolves");
        let expected = std::env::var("PATH").expect("set in test process");
        assert_eq!(headers, vec![("X-Forwarded-Path".to_string(), expected)]);
    }

    #[test]
    fn a_missing_env_header_is_an_error_not_an_empty_header() {
        let declared = declaration(BTreeMap::from([(
            "X-Department-Token".to_string(),
            "env:KEKE_TEST_DEFINITELY_UNSET".to_string(),
        )]));
        assert!(matches!(
            extra_headers(&declared),
            Err(DeclarationError::MissingHeaderEnv { .. })
        ));
    }

    #[test]
    fn authorization_cannot_be_set_as_a_custom_header() {
        let declared = declaration(BTreeMap::from([(
            "Authorization".to_string(),
            "Bearer forged".to_string(),
        )]));
        assert!(matches!(
            extra_headers(&declared),
            Err(DeclarationError::ReservedHeader { .. })
        ));
    }

    #[test]
    fn a_missing_ca_cert_file_is_reported_by_route() {
        let mut declared = declaration(BTreeMap::new());
        declared.ca_cert_path = Some("/nonexistent/path/to/ca.pem".to_string());
        let error = build_http_client(&declared).expect_err("no such file");
        assert!(matches!(error, DeclarationError::BadCaCert { .. }));
    }
}
