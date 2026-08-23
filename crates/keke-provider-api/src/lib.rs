//! The model provider seam.
//!
//! A provider translates neutral [`keke_protocol`] messages into a vendor's wire
//! format and streams the reply back as neutral [`StreamChunk`]s. That is its
//! entire job: it owns no conversation state, makes no policy decisions, and
//! never runs a tool.
//!
//! Adding a vendor means adding one crate implementing [`ModelProvider`] and one
//! line in the composition root. Nothing in the engine changes.

mod error;
mod info;
mod stream;

pub use error::ProviderError;
pub use info::ModelInfo;
pub use info::ProviderInfo;
pub use info::WireApi;
pub use stream::StreamChunk;
pub use stream::StreamEvent;

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use keke_protocol::Message;
use keke_protocol::ReasoningEffort;
use keke_protocol::ToolCallId;
use serde_json::Value;

/// A boxed future, used where a method must stay dyn-compatible.
pub type ProviderFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// A tool as advertised to the model.
#[derive(Clone, Debug, PartialEq)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    /// JSON Schema for the tool's arguments.
    pub input_schema: Value,
}

/// One model call.
#[derive(Clone, Debug, Default)]
pub struct ModelRequest {
    pub model: String,
    /// System prompt, kept separate because vendors place it differently.
    pub system: Option<String>,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolSpec>,
    pub max_output_tokens: Option<u32>,
    pub temperature: Option<f32>,
    /// How hard the model should think, when a level was chosen. `None` leaves
    /// the vendor's own default in place, which is not the same as asking for
    /// the least thinking on offer — see
    /// [`ReasoningEffort`](keke_protocol::ReasoningEffort).
    pub reasoning_effort: Option<ReasoningEffort>,
}

/// A vendor backend.
#[allow(clippy::module_name_repetitions)]
pub trait ModelProvider: Send + Sync + 'static {
    /// Static facts about this provider, including its route key.
    fn info(&self) -> &ProviderInfo;

    /// Stream one model call.
    ///
    /// Errors that the engine should retry must be reported as the specific
    /// [`ProviderError`] variants rather than as a generic failure, because
    /// retry policy is decided by the engine and needs to tell a rate limit
    /// apart from a bad request.
    fn stream<'a>(
        &'a self,
        request: ModelRequest,
    ) -> ProviderFuture<'a, Result<StreamEvent, ProviderError>>;

    /// List the models this provider can serve.
    ///
    /// Providers that cannot enumerate models return an empty list rather than
    /// an error, so a surface falls back to hand-entry instead of reporting an
    /// authentication failure as "no models".
    fn list_models(&self) -> ProviderFuture<'_, Result<Vec<ModelInfo>, ProviderError>> {
        Box::pin(async { Ok(Vec::new()) })
    }
}

/// A provider plus the route key it was registered under.
pub type ArcProvider = Arc<dyn ModelProvider>;

/// The set of registered providers.
///
/// Registration returns a disposer: dropping it removes the route. Every
/// registry in the workspace follows this shape so an unloaded plugin cannot
/// leave a contribution behind.
#[derive(Default)]
pub struct ProviderRegistry {
    routes: std::collections::BTreeMap<String, ArcProvider>,
}

/// Why a provider could not be resolved.
#[derive(Debug, thiserror::Error)]
pub enum RouteError {
    #[error("no provider is registered for route `{0}`")]
    Unknown(String),
    #[error("route `{0}` is already registered")]
    Duplicate(String),
    /// Several providers are usable and none was configured. Ambiguity is an
    /// error rather than a silent pick, so a misconfiguration is visible at the
    /// point it is introduced.
    #[error("no provider configured and {0} are available; set one explicitly")]
    Ambiguous(String),
}

impl ProviderRegistry {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register `provider` under its declared route key.
    pub fn register(&mut self, provider: ArcProvider) -> Result<(), RouteError> {
        let route = provider.info().route.clone();
        if self.routes.contains_key(&route) {
            return Err(RouteError::Duplicate(route));
        }
        self.routes.insert(route, provider);
        Ok(())
    }

    /// Look up a configured route.
    pub fn get(&self, route: &str) -> Result<ArcProvider, RouteError> {
        self.routes
            .get(route)
            .cloned()
            .ok_or_else(|| RouteError::Unknown(route.to_string()))
    }

    /// Resolve the provider to use when configuration names one, or when
    /// exactly one is registered.
    pub fn resolve(&self, configured: Option<&str>) -> Result<ArcProvider, RouteError> {
        if let Some(route) = configured {
            return self.get(route);
        }
        let mut routes = self.routes.keys();
        match (routes.next(), routes.next()) {
            (Some(only), None) => self.get(only),
            (Some(_), Some(_)) => Err(RouteError::Ambiguous(
                self.routes.keys().cloned().collect::<Vec<_>>().join(", "),
            )),
            _ => Err(RouteError::Unknown("<none registered>".to_string())),
        }
    }

    /// Every registered route key, sorted.
    pub fn routes(&self) -> impl Iterator<Item = &str> {
        self.routes.keys().map(String::as_str)
    }
}

/// A partially-received tool call, assembled from streamed argument deltas.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PartialToolCall {
    pub id: Option<ToolCallId>,
    pub name: String,
    /// Concatenated argument fragments; parsed only once the call completes.
    pub arguments: String,
}
