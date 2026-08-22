//! Declarative facts about a provider and its models.

use serde::Deserialize;
use serde::Serialize;

/// The request/response shape a provider speaks.
///
/// This exists so shared HTTP plumbing can be reused across vendors that follow
/// the same schema, without pretending vendors with genuinely different schemas
/// are interchangeable.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WireApi {
    /// OpenAI-compatible `/chat/completions`.
    ChatCompletions,
    /// OpenAI `/responses`.
    Responses,
    /// Anthropic `/messages`.
    Messages,
    /// Something else the provider handles entirely on its own.
    Custom,
}

/// Static facts about a provider.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderInfo {
    /// The registry key, e.g. `"xai"` or `"chatgpt"`. Stable; config names it.
    pub route: String,
    /// Human-readable name for surfaces.
    pub display_name: String,
    pub base_url: String,
    pub wire_api: WireApi,
    /// Which [`AuthProvider`](../keke_auth_api/trait.AuthProvider.html) id
    /// supplies this provider's credentials. Keeping it a plain string is what
    /// lets `keke-provider-api` stay independent of `keke-auth-api`.
    pub auth_id: Option<String>,
    /// Environment variable holding an API key, when the provider accepts one.
    pub env_key: Option<String>,
}

/// One model a provider can serve.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelInfo {
    pub id: String,
    pub display_name: String,
    /// Total context window in tokens.
    pub context_window: Option<u64>,
    pub max_output_tokens: Option<u64>,
    pub supports_tools: bool,
    pub supports_vision: bool,
    /// Whether the model exposes reasoning content.
    pub supports_reasoning: bool,
}
