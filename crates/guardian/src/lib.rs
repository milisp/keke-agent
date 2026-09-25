//! A model-backed approval reviewer.
//!
//! `ApprovalReviewContributor` (`keke-plugin-api`) is the seam every reviewer
//! answers through, human or otherwise. This crate implements one that asks a
//! configured model to judge a pending [`ApprovalRequest`] instead of asking a
//! person — useful when the deployment would rather have a fast, cheap model
//! pre-screen risky calls than always block on a human, or always trust the
//! main conversation model's own (self-interested) judgment about its own
//! call.
//!
//! # Denial is monotonic
//!
//! This reviewer can only ever turn an ambiguous case into a `Deny`: a
//! provider error, a timeout, or a reply that does not parse as a clear
//! verdict all deny rather than falling through as `None` (`AGENTS.md`
//! invariant 8, "ambiguity fails loud"). It can produce an `Allow`, but never
//! by guessing — only by parsing an explicit verdict out of the model's reply.

use std::sync::Arc;

use futures::StreamExt;
use keke_config_types::GuardianReviewConfig;
use keke_config_types::ModelSelection;
use keke_plugin_api::ApprovalDecision;
use keke_plugin_api::ApprovalRequest;
use keke_plugin_api::ApprovalReviewContributor;
use keke_plugin_api::ExtFuture;
use keke_plugin_api::ExtensionContext;
use keke_plugin_api::ExtensionRegistryBuilder;
use keke_protocol::Message;
use keke_protocol::SessionEvent;
use keke_provider_api::ArcProvider;
use keke_provider_api::ModelRequest;
use keke_provider_api::ProviderRegistry;
use keke_provider_api::RouteError;
use keke_provider_api::StreamChunk;

/// Register the guardian reviewer for Auto mode and, when enabled, on-request.
///
/// A disabled guardian does not participate in on-request approval.
/// `fallback` is the session's own model, used when `config.model` is unset.
///
/// The route is resolved here, at composition time, rather than on the first
/// review: a guardian configured against a route nobody registered is a
/// misconfiguration, and it should fail the way every other misconfiguration
/// in the composition root does — loudly, before a turn ever runs — rather
/// than as a `Deny` a person has to trace back to a typo'd provider name.
pub fn install(
    registry: &mut ExtensionRegistryBuilder,
    config: GuardianReviewConfig,
    providers: &ProviderRegistry,
    fallback: &ModelSelection,
) -> Result<(), RouteError> {
    let explicit_model = config.model.is_some();
    let selection = config.model.unwrap_or_else(|| fallback.clone());
    let provider = match providers.get(&selection.provider) {
        Ok(provider) => provider,
        Err(_) if !config.enabled && !explicit_model => return Ok(()),
        Err(error) => return Err(error),
    };
    let reviewer = Arc::new(GuardianReviewer {
        model: selection.model,
        reasoning_effort: config.reasoning_effort,
        on_request: config.enabled,
        provider,
    });
    registry.approval_review_contributor(reviewer);
    Ok(())
}

struct GuardianReviewer {
    model: String,
    reasoning_effort: Option<keke_protocol::ReasoningEffort>,
    on_request: bool,
    provider: ArcProvider,
}

impl GuardianReviewer {
    fn prompt(request: &ApprovalRequest) -> String {
        format!(
            "A tool call needs approval before it runs.\n\
             Tool: {}\n\
             Arguments: {}\n\
             Reason approval is required: {}\n\n\
             Reply with exactly one line: `ALLOW` or `DENY: <short reason>`.",
            request.call.name, request.call.arguments, request.reason,
        )
    }

    /// Parse a reviewer's reply into a decision, or `None` when it is not a
    /// clear verdict — the caller turns `None` into `Deny`, never `Allow`.
    fn parse(reply: &str) -> Option<ApprovalDecision> {
        let reply = reply.trim();
        if reply == "ALLOW" {
            return Some(ApprovalDecision::Allow { note: None });
        }
        if let Some(rest) = reply.strip_prefix("ALLOW:") {
            let rest = rest.trim();
            return Some(ApprovalDecision::Allow {
                note: (!rest.is_empty()).then(|| rest.to_string()),
            });
        }
        if reply == "DENY" || reply.starts_with("DENY:") {
            let reason = reply.strip_prefix("DENY:").unwrap_or("").trim();
            return Some(ApprovalDecision::Deny {
                reason: if reason.is_empty() {
                    "the guardian reviewer denied this call".to_string()
                } else {
                    reason.to_string()
                },
            });
        }
        None
    }
}

impl ApprovalReviewContributor for GuardianReviewer {
    fn automatic(&self) -> bool {
        true
    }

    fn on_request(&self) -> bool {
        self.on_request
    }

    fn review<'a>(
        &'a self,
        ctx: &'a ExtensionContext,
        request: &'a ApprovalRequest,
    ) -> ExtFuture<'a, Option<ApprovalDecision>> {
        Box::pin(async move {
            let model_request = ModelRequest {
                model: self.model.clone(),
                messages: vec![Message::user(Self::prompt(request))],
                reasoning_effort: self.reasoning_effort,
                ..ModelRequest::default()
            };

            let stream = match self.provider.stream(model_request).await {
                Ok(stream) => stream,
                Err(error) => {
                    return Some(deny_and_log(
                        ctx,
                        &self.model,
                        &request.call.name,
                        format!("guardian model call failed: {error}"),
                    ));
                }
            };

            let mut text = String::new();
            let mut stream = stream;
            while let Some(chunk) = stream.next().await {
                match chunk {
                    Ok(StreamChunk::TextDelta(delta)) => text.push_str(&delta),
                    Ok(StreamChunk::Done(_)) => break,
                    Ok(_) => {}
                    Err(error) => {
                        return Some(deny_and_log(
                            ctx,
                            &self.model,
                            &request.call.name,
                            format!("guardian model call failed: {error}"),
                        ));
                    }
                }
            }

            let decision = Self::parse(&text).unwrap_or_else(|| ApprovalDecision::Deny {
                reason: "the guardian reviewer's reply did not parse as a verdict".to_string(),
            });
            record(ctx, &self.model, &request.call.name, &decision);
            Some(decision)
        })
    }
}

fn deny_and_log(
    ctx: &ExtensionContext,
    model: &str,
    tool_name: &str,
    reason: String,
) -> ApprovalDecision {
    let decision = ApprovalDecision::Deny {
        reason: reason.clone(),
    };
    record(ctx, model, tool_name, &decision);
    decision
}

fn record(ctx: &ExtensionContext, model: &str, tool_name: &str, decision: &ApprovalDecision) {
    let Some(turn) = ctx.turn() else {
        return;
    };
    let (allow, reason) = match decision {
        ApprovalDecision::Allow { note } => (true, note.clone()),
        ApprovalDecision::AllowAlways => (true, None),
        ApprovalDecision::Deny { reason } | ApprovalDecision::Abort { reason } => {
            (false, Some(reason.clone()))
        }
    };
    ctx.record(SessionEvent::GuardianReview {
        turn,
        tool_name: tool_name.to_string(),
        model: model.to_string(),
        allow,
        reason,
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allow_with_no_note_parses() {
        assert_eq!(
            GuardianReviewer::parse("ALLOW"),
            Some(ApprovalDecision::Allow { note: None })
        );
    }

    #[test]
    fn allow_with_a_note_parses() {
        assert_eq!(
            GuardianReviewer::parse("ALLOW: looks safe"),
            Some(ApprovalDecision::Allow {
                note: Some("looks safe".to_string())
            })
        );
    }

    #[test]
    fn deny_with_a_reason_parses() {
        assert_eq!(
            GuardianReviewer::parse("DENY: touches production credentials"),
            Some(ApprovalDecision::Deny {
                reason: "touches production credentials".to_string()
            })
        );
    }

    #[test]
    fn an_unparsable_reply_is_none_not_allow() {
        // The caller turns `None` into `Deny`; a reviewer must never be able to
        // manufacture an allow out of a reply it could not understand.
        assert_eq!(GuardianReviewer::parse("uh, maybe?"), None);
        assert_eq!(GuardianReviewer::parse("ALLOWANCE"), None);
    }
}
