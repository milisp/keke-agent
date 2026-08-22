//! Provider failures, classified by what the engine should do next.
//!
//! The classification is the point. A provider that collapses everything into
//! one "request failed" variant forces the engine to string-match, so each
//! variant here corresponds to a distinct engine response: retry after a delay,
//! refresh credentials and retry once, or surface to the user.

/// Why a model call failed.
#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    /// Credentials are missing, expired, or rejected. The engine refreshes and
    /// retries exactly once before surfacing this.
    #[error("authentication failed: {0}")]
    Unauthorized(String),

    /// The provider asked us to slow down. `retry_after_millis` is its stated
    /// delay when it gave one.
    #[error("rate limited{}", .retry_after_millis.map(|ms| format!(" (retry after {ms}ms)")).unwrap_or_default())]
    RateLimited { retry_after_millis: Option<u64> },

    /// A transient network or 5xx failure. Safe to retry with backoff.
    #[error("transient provider failure: {0}")]
    Transient(String),

    /// The request was malformed or asked for something unsupported. Retrying
    /// unchanged will fail again, so the engine surfaces it immediately.
    #[error("invalid request: {0}")]
    InvalidRequest(String),

    /// The named model is unknown to this provider.
    #[error("unknown model `{0}`")]
    UnknownModel(String),

    /// The provider's response did not match its documented shape.
    #[error("malformed provider response: {0}")]
    Protocol(String),

    /// The turn was aborted.
    ///
    /// Constructed by the engine, not by a provider: [`ModelProvider::stream`]
    /// takes no cancellation signal, so the engine cancels by dropping the
    /// stream. A provider needs no cancellation handling of its own.
    ///
    /// [`ModelProvider::stream`]: crate::ModelProvider::stream
    #[error("cancelled")]
    Cancelled,
}

impl ProviderError {
    /// Whether retrying the identical request could succeed.
    #[must_use]
    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            Self::RateLimited { .. } | Self::Transient(_) | Self::Protocol(_)
        )
    }

    /// Whether the engine should refresh credentials and retry once.
    #[must_use]
    pub fn needs_reauth(&self) -> bool {
        matches!(self, Self::Unauthorized(_))
    }
}
