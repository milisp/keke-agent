//! OIDC discovery, so the endpoints are the issuer's rather than this crate's
//! guess at them.
//!
//! The paths derived in [`crate::GrokAuthConfig::new`] are a starting point, not
//! a contract: an issuer is free to serve its token endpoint anywhere, and a
//! refresh posted to a path that answers 404 fails in exactly the way an expired
//! refresh token does. Discovery replaces the guess where the deployment has not
//! named an endpoint itself, and a deployment that *has* named one keeps it —
//! that is the whole reason those fields exist.
//!
//! A failed discovery is not an error. It falls back to the derived path, so an
//! issuer that serves no discovery document behaves exactly as it did before.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;
use std::time::Instant;

use reqwest::Client;
use serde::Deserialize;

use crate::config::GrokAuthConfig;

/// How long a discovery document is reused. An issuer moving an endpoint is
/// rare; asking again on every refresh is a round trip per turn.
const TTL: Duration = Duration::from_secs(3600);

/// The request may not outlive the operation it is meant to speed up.
const TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone, Debug, Default, Deserialize)]
pub(crate) struct Document {
    #[serde(default)]
    pub authorization_endpoint: Option<String>,
    #[serde(default)]
    pub token_endpoint: Option<String>,
    #[serde(default)]
    pub device_authorization_endpoint: Option<String>,
}

/// Documents already fetched, held by the [`crate::GrokAuth`] that fetched
/// them rather than in a static: a process-wide cache keyed by issuer would let
/// one instance's answer decide another's endpoints.
#[derive(Debug, Default)]
pub(crate) struct Cache(Mutex<HashMap<String, (Document, Instant)>>);

async fn document(http: &Client, cache: &Cache, issuer: &str) -> Option<Document> {
    let key = issuer.trim_end_matches('/').to_string();
    if let Ok(cached) = cache.0.lock()
        && let Some((document, at)) = cached.get(&key)
        && at.elapsed() < TTL
    {
        return Some(document.clone());
    }

    let url = format!("{key}/.well-known/openid-configuration");
    let response = http.get(&url).timeout(TIMEOUT).send().await.ok()?;
    if !response.status().is_success() {
        return None;
    }
    let document: Document = response.json().await.ok()?;

    if let Ok(mut cached) = cache.0.lock() {
        cached.insert(key, (document.clone(), Instant::now()));
    }
    Some(document)
}

/// Where to post a token request.
/// `issuer` is the credential's own when it recorded one, which outranks the
/// build's constant: a login imported from another CLI, or made against a
/// private deployment, must be renewed by whoever signed it. Posting to the
/// constant instead fails as an unreachable host or a 404, and both of those
/// read as a revoked login rather than as the wrong address.
pub(crate) async fn token_endpoint(
    http: &Client,
    cache: &Cache,
    config: &GrokAuthConfig,
    issuer: Option<&str>,
) -> String {
    let issuer = issuer.unwrap_or(&config.issuer);
    resolve(
        http,
        cache,
        issuer,
        &config.token_endpoint,
        &config.derived_token_endpoint_for(issuer),
        |document| document.token_endpoint,
    )
    .await
}

/// Where to send a person to authorize.
pub(crate) async fn authorize_endpoint(
    http: &Client,
    cache: &Cache,
    config: &GrokAuthConfig,
) -> String {
    resolve(
        http,
        cache,
        &config.issuer,
        &config.authorize_endpoint,
        &config.derived_authorize_endpoint(),
        |document| document.authorization_endpoint,
    )
    .await
}

/// Where to start a device-code flow.
pub(crate) async fn device_authorization_endpoint(
    http: &Client,
    cache: &Cache,
    config: &GrokAuthConfig,
) -> String {
    resolve(
        http,
        cache,
        &config.issuer,
        &config.device_authorization_endpoint,
        &config.derived_device_authorization_endpoint(),
        |document| document.device_authorization_endpoint,
    )
    .await
}

/// Discovery answers only for an endpoint nobody configured: `configured`
/// differing from `derived` means a deployment stated where the endpoint is,
/// and an issuer does not get to overrule that.
async fn resolve(
    http: &Client,
    cache: &Cache,
    issuer: &str,
    configured: &str,
    derived: &str,
    pick: impl FnOnce(Document) -> Option<String>,
) -> String {
    if configured != derived {
        return configured.to_string();
    }
    match document(http, cache, issuer).await.and_then(pick) {
        Some(discovered) => discovered,
        None => configured.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::Mock;
    use wiremock::MockServer;
    use wiremock::ResponseTemplate;
    use wiremock::matchers::method;
    use wiremock::matchers::path;

    async fn issuer_serving(document: serde_json::Value) -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/.well-known/openid-configuration"))
            .respond_with(ResponseTemplate::new(200).set_body_json(document))
            .mount(&server)
            .await;
        server
    }

    #[tokio::test]
    async fn the_issuers_own_token_endpoint_replaces_the_derived_guess() {
        let server = issuer_serving(serde_json::json!({
            "token_endpoint": "https://auth.example/somewhere/else",
        }))
        .await;
        let config = GrokAuthConfig::new(server.uri(), "client-1");

        assert_eq!(
            token_endpoint(&Client::new(), &Cache::default(), &config, None).await,
            "https://auth.example/somewhere/else"
        );
    }

    #[tokio::test]
    async fn a_configured_endpoint_outranks_the_issuers() {
        let server = issuer_serving(serde_json::json!({
            "token_endpoint": "https://auth.example/somewhere/else",
        }))
        .await;
        let mut config = GrokAuthConfig::new(server.uri(), "client-1");
        config.token_endpoint = "https://gateway.internal/token".to_string();

        assert_eq!(
            token_endpoint(&Client::new(), &Cache::default(), &config, None).await,
            "https://gateway.internal/token",
            "a deployment that named an endpoint must keep it"
        );
    }

    #[tokio::test]
    async fn an_issuer_without_a_discovery_document_keeps_the_derived_path() {
        let server = MockServer::start().await;
        let config = GrokAuthConfig::new(server.uri(), "client-1");

        assert_eq!(
            token_endpoint(&Client::new(), &Cache::default(), &config, None).await,
            config.token_endpoint
        );
    }
}
