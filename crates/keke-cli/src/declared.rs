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
}

/// A provider assembled from configuration.
struct DeclaredProvider {
    info: ProviderInfo,
    api: WireApi,
    client: WireClient,
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
        Box::pin(async move { self.client.list_models().await })
    }
}

/// Wrap a `WireClient` as a provider, for a vendor whose only behavior is its
/// endpoint and credential — including compiled-in ones like codex.
pub(crate) fn wire_provider(info: ProviderInfo, auth: Arc<dyn AuthProvider>) -> ArcProvider {
    let api = info.wire_api;
    let client = WireClient::new(info.base_url.clone(), auth);
    Arc::new(DeclaredProvider { info, api, client })
}

/// Build a route from one declaration.
///
/// The resulting provider reports `auth_id: None`: it has no login flow, so
/// there is nothing for `keke login` to name. Surfaces report on it through
/// `ProviderInfo::env_key` instead — see `crate::api_key`.
pub(crate) fn provider_for(
    declaration: &ProviderDeclaration,
    credentials: &Arc<dyn CredentialStore>,
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

    Ok(wire_provider(
        ProviderInfo {
            route: declaration.route.clone(),
            display_name: declaration
                .display_name
                .clone()
                .unwrap_or_else(|| declaration.route.clone()),
            base_url: declaration.base_url.clone(),
            wire_api: wire_api(declaration.wire),
            auth_id: None,
            env_key: declaration.env_key.clone(),
        },
        auth,
    ))
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
