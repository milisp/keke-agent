//! Keeping the conversation inside the context window.
//!
//! A session that never compacts works until it doesn't: the provider rejects
//! the request, mid-conversation, with no way forward except starting over. So
//! compaction is not an optimization — it is what makes a long session possible
//! at all.
//!
//! The summary replaces the messages it covers, and the replacement is recorded
//! as [`SessionEvent::Compacted`]. That keeps the log's promise: what the model
//! saw after compaction is reconstructable from the log, including which
//! messages stopped being visible.

use keke_config_types::CompactionConfig;
use keke_protocol::ContentBlock;
use keke_protocol::Message;
use keke_protocol::Role;

/// Rough token count for a message.
///
/// Four bytes per token, the usual English-text approximation. Deliberately
/// crude and deliberately an *over*-estimate of nothing: compaction triggering
/// slightly early costs one summarization, while triggering late costs the turn.
/// A real tokenizer would be per-model and per-vendor, and would still be an
/// estimate for the parts of the request keke does not build.
#[must_use]
pub fn estimate_tokens(message: &Message) -> usize {
    let bytes: usize = message
        .content
        .iter()
        .map(|block| match block {
            ContentBlock::Text { text } | ContentBlock::Thinking { text, .. } => text.len(),
            // An image costs far more than its base64 length suggests, but the
            // number is vendor-specific; this at least stops one from reading
            // as free.
            ContentBlock::Image(image) => image.data.len() / 3,
            ContentBlock::ToolCall(call) => call.name.len() + call.arguments.to_string().len(),
            ContentBlock::ToolResult(result) => result
                .content
                .iter()
                .map(|block| match block {
                    ContentBlock::Text { text } => text.len(),
                    _ => 0,
                })
                .sum(),
        })
        .sum();
    bytes / 4
}

/// Total estimate for a conversation.
#[must_use]
pub fn estimate_history(history: &[Message]) -> usize {
    history.iter().map(estimate_tokens).sum()
}

/// Whether `history` has grown past the configured trigger.
#[must_use]
pub fn should_compact(history: &[Message], config: &CompactionConfig) -> bool {
    let budget =
        (u64::from(config.context_window) * u64::from(config.trigger_percent) / 100) as usize;
    estimate_history(history) > budget
}

/// The messages to summarize, and the ones to keep verbatim.
///
/// Returns `None` when there is nothing worth compacting — a history shorter
/// than the tail it must preserve would summarize itself away.
#[must_use]
pub(crate) fn split_for_compaction<'a>(
    history: &'a [Message],
    config: &CompactionConfig,
) -> Option<(&'a [Message], &'a [Message])> {
    if history.len() <= config.keep_recent_messages {
        return None;
    }
    let cut = history.len() - config.keep_recent_messages;
    let (older, recent) = history.split_at(cut);
    // Compacting one message into a summary of one message is pure loss.
    (older.len() > 1).then_some((older, recent))
}

/// The instruction that produces a summary.
///
/// Asks for the facts a continuation needs rather than prose: what was decided,
/// what was tried, what is still open. A narrative summary reads well and is
/// useless to the next turn.
pub(crate) const SUMMARY_INSTRUCTION: &str = "\
Summarize the conversation so far so that it can continue without the original \
messages. Record: what the user asked for, decisions made and why, files and \
symbols touched, commands run and their outcomes, what has been verified, and \
what is still outstanding. Preserve exact names, paths, and identifiers. Omit \
pleasantries and restatements. Write it as notes to yourself, not as prose.";

/// Wrap a summary as the message that stands in for what it replaced.
///
/// Marked as a user message rather than an assistant one: it is context handed
/// to the model, not something the model said, and a model that mistakes its own
/// summary for its own earlier reasoning tends to repeat it.
#[must_use]
pub(crate) fn summary_message(summary: &str) -> Message {
    Message {
        role: Role::User,
        content: vec![ContentBlock::text(format!(
            "Summary of the earlier conversation:\n\n{summary}"
        ))],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> CompactionConfig {
        CompactionConfig {
            trigger_percent: 80,
            keep_recent_messages: 2,
            context_window: 1_000,
        }
    }

    fn filler(tokens: usize) -> Message {
        Message::user("x".repeat(tokens * 4))
    }

    #[test]
    fn compaction_triggers_only_past_the_configured_fraction() {
        let config = config();
        assert!(!should_compact(&[filler(700)], &config));
        assert!(should_compact(&[filler(801)], &config));
    }

    #[test]
    fn the_recent_tail_is_kept_verbatim() {
        let config = config();
        let history: Vec<Message> = (0..5).map(|_| filler(10)).collect();
        let (older, recent) = split_for_compaction(&history, &config).expect("splits");
        assert_eq!(older.len(), 3);
        assert_eq!(recent.len(), 2);
    }

    /// Summarizing one message into a summary of one message is pure loss, and
    /// a history shorter than its own tail would summarize itself away.
    #[test]
    fn a_history_too_short_to_gain_anything_is_left_alone() {
        let config = config();
        let short: Vec<Message> = (0..2).map(|_| filler(10)).collect();
        assert!(split_for_compaction(&short, &config).is_none());

        let barely: Vec<Message> = (0..3).map(|_| filler(10)).collect();
        assert!(split_for_compaction(&barely, &config).is_none());
    }

    #[test]
    fn a_summary_is_context_handed_to_the_model_not_something_it_said() {
        let message = summary_message("we fixed the parser");
        assert_eq!(message.role, Role::User);
        assert!(message.text().contains("we fixed the parser"));
    }
}
