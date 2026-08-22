//! The NVIDIA NIM model provider.
//!
//! NIM hosts many vendors' open models behind one OpenAI-compatible endpoint,
//! and serves both `/chat/completions` and `/responses` at it. Which one a
//! deployment wants is not something this crate can know — some hosted models
//! only implement the older path — so it is a constructor argument rather than
//! a constant, and everything past that choice is [`keke_wire`]'s job.

use std::sync::Arc;

use keke_auth_api::AuthProvider;
use keke_provider_api::ModelInfo;
use keke_provider_api::ModelProvider;
use keke_provider_api::ModelRequest;
use keke_provider_api::ProviderError;
use keke_provider_api::ProviderFuture;
use keke_provider_api::ProviderInfo;
use keke_provider_api::StreamEvent;
use keke_provider_api::WireApi;
use keke_wire::WireClient;

/// NVIDIA's hosted NIM endpoint. Overridable through the constructors, which is
/// how a self-hosted NIM container or a test server is pointed at.
const DEFAULT_BASE_URL: &str = "https://integrate.api.nvidia.com/v1";

/// The model a caller gets when configuration names none.
///
/// Public because a surface offering a model picker should show what it would
/// fall back to, and because it is the one value here a deployment is likely to
/// want to override.
pub const DEFAULT_MODEL: &str = "nvidia/nemotron-3-ultra-550b-a55b";

/// A wire format NIM does not serve.
#[derive(Debug, thiserror::Error)]
#[error("NVIDIA NIM serves chat completions and responses, not {0:?}")]
pub struct UnsupportedWireApi(WireApi);

/// Open models hosted on NVIDIA NIM.
pub struct NvidiaProvider {
    info: ProviderInfo,
    wire: WireClient,
}

impl NvidiaProvider {
    /// Build a provider over `/chat/completions`, the endpoint every hosted NIM
    /// model implements.
    #[must_use]
    pub fn new(auth: Arc<dyn AuthProvider>, base_url: Option<String>) -> Self {
        Self::build(auth, base_url, WireApi::ChatCompletions)
    }

    /// Build a provider over a named endpoint.
    ///
    /// An unsupported format is an error rather than a silent fall back to
    /// chat completions: a caller that asked for `/responses` and got the other
    /// one would see the difference only as missing reasoning output, long
    /// after the misconfiguration was introduced.
    pub fn with_wire_api(
        auth: Arc<dyn AuthProvider>,
        base_url: Option<String>,
        wire_api: WireApi,
    ) -> Result<Self, UnsupportedWireApi> {
        match wire_api {
            WireApi::ChatCompletions | WireApi::Responses => {
                Ok(Self::build(auth, base_url, wire_api))
            }
            other => Err(UnsupportedWireApi(other)),
        }
    }

    fn build(auth: Arc<dyn AuthProvider>, base_url: Option<String>, wire_api: WireApi) -> Self {
        let base_url = base_url
            .filter(|url| !url.trim().is_empty())
            .unwrap_or_else(|| DEFAULT_BASE_URL.to_string());
        let wire = WireClient::new(base_url, auth);
        Self {
            info: ProviderInfo {
                route: "nvidia".to_string(),
                display_name: "NVIDIA NIM".to_string(),
                base_url: wire.base_url().to_string(),
                wire_api,
                auth_id: Some("nvidia".to_string()),
                env_key: Some("NVIDIA_API_KEY".to_string()),
            },
            wire,
        }
    }
}

impl ModelProvider for NvidiaProvider {
    fn info(&self) -> &ProviderInfo {
        &self.info
    }

    fn stream<'a>(
        &'a self,
        request: ModelRequest,
    ) -> ProviderFuture<'a, Result<StreamEvent, ProviderError>> {
        Box::pin(self.wire.stream(self.info.wire_api, request))
    }

    /// NIM enumerates its catalog, so a failure is reported rather than
    /// flattened to an empty list, which would present a rejected key as an
    /// account with no models.
    fn list_models(&self) -> ProviderFuture<'_, Result<Vec<ModelInfo>, ProviderError>> {
        Box::pin(self.wire.list_models())
    }
}

#[cfg(test)]
mod tests;
