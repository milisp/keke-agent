//! Declarative facts about a provider and its models.

use keke_protocol::ReasoningEffort;
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
    /// The registry key, e.g. `"grok"` or `"chatgpt"`. Stable; config names it.
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
///
/// Everything past the id is optional because listings disagree about how much
/// they say: the plain OpenAI `/models` is a bag of ids, while a subscription
/// backend publishes display names, context windows, and the reasoning levels
/// it accepts. A field nobody stated is absent rather than guessed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelInfo {
    pub id: String,
    pub display_name: String,
    /// One line about what the model is for, when the vendor publishes one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Total context window in tokens.
    pub context_window: Option<u64>,
    pub max_output_tokens: Option<u64>,
    pub supports_tools: bool,
    pub supports_vision: bool,
    /// The effort levels this model accepts, weakest first.
    ///
    /// Empty means there is nothing to offer — either the model has no such
    /// knob, or the listing did not say. Those are the same thing to a surface,
    /// and a level invented here would be one a person could select and the
    /// endpoint would then reject.
    #[serde(default)]
    pub reasoning_efforts: Vec<ReasoningEffort>,
    /// Where the vendor starts when the request names no level. `None` leaves
    /// that to the endpoint, which is not the same as the weakest rung.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_reasoning_effort: Option<ReasoningEffort>,
}

impl ModelInfo {
    /// A model known only by its id, which is all a plain listing gives.
    #[must_use]
    pub fn new(id: impl Into<String>) -> Self {
        let id = id.into();
        Self {
            display_name: id.clone(),
            id,
            description: None,
            context_window: None,
            max_output_tokens: None,
            // Claiming otherwise would make the engine withhold tools it could
            // have sent. A model that cannot use them rejects the request; a
            // model that can, and was not offered them, silently answers worse.
            supports_tools: true,
            supports_vision: false,
            reasoning_efforts: Vec::new(),
            default_reasoning_effort: None,
        }
    }

    /// Whether a surface has reasoning levels to offer for this model.
    ///
    /// Derived rather than stored: a separate flag could disagree with the list
    /// it describes, and then a picker would either show an empty menu or hide
    /// levels the endpoint accepts.
    #[must_use]
    pub fn supports_reasoning(&self) -> bool {
        !self.reasoning_efforts.is_empty()
    }

    /// The level to start at: the vendor's own default when it published one,
    /// and otherwise nothing — see [`ReasoningEffort`] on why absence is not
    /// the bottom rung.
    #[must_use]
    pub fn starting_effort(&self) -> Option<ReasoningEffort> {
        self.default_reasoning_effort
            .filter(|effort| self.reasoning_efforts.contains(effort))
    }
}
