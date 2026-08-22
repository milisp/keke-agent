//! Deciding when to ask, and remembering the answer.
//!
//! Approval reviewers were registered by extensions long before anything
//! consulted them; this is the piece that does. It sits between the guards and
//! the tool body, which is what keeps denial monotonic: a guard's denial is
//! already final by the time anyone is asked, so no answer here can restore a
//! permission a guard took away.

use std::collections::BTreeSet;
use std::sync::Mutex;

use keke_config_types::ApprovalPolicy;
use keke_tool::ToolCapabilities;
use keke_tool::ToolKind;

/// Whether a call needs a person's assent, and why.
///
/// The reason is the text whoever decides will read, so it names the effect
/// rather than the policy: "may write outside the workspace" is actionable,
/// "policy is on-request" is not.
#[must_use]
pub fn approval_reason(policy: ApprovalPolicy, capabilities: &ToolCapabilities) -> Option<String> {
    match policy {
        // Non-interactive by construction. A deployment that sets this has
        // accepted the consequences elsewhere — usually a sandbox.
        ApprovalPolicy::Never => None,
        // Escalation-after-failure is decided by the tool, which asks by
        // failing; there is nothing to ask before the call.
        ApprovalPolicy::OnFailure => None,
        ApprovalPolicy::OnRequest => match capabilities.kind {
            ToolKind::Edit => Some("modifies files".to_string()),
            ToolKind::Execute => Some("runs a command".to_string()),
            ToolKind::Network => Some("reaches the network".to_string()),
            ToolKind::Read | ToolKind::Search | ToolKind::Meta => None,
        },
    }
}

/// What a session remembers about approvals already given.
///
/// "The same shape" is the tool, not the arguments: a person who allowed
/// `bash` once and is asked again for every distinct command has not been
/// given a standing permission, they have been given a slower prompt. Narrowing
/// this later is a policy change, and it belongs here rather than in a surface.
#[derive(Debug, Default)]
pub struct ApprovalMemory {
    always: Mutex<BTreeSet<String>>,
}

impl ApprovalMemory {
    #[must_use]
    pub fn is_always_allowed(&self, tool: &str) -> bool {
        self.always
            .lock()
            .is_ok_and(|allowed| allowed.contains(tool))
    }

    pub fn allow_always(&self, tool: &str) {
        if let Ok(mut allowed) = self.always.lock() {
            allowed.insert(tool.to_string());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn capabilities(kind: ToolKind) -> ToolCapabilities {
        ToolCapabilities {
            kind,
            concurrency_safe: kind.is_read_only(),
            timeout_millis: None,
        }
    }

    #[test]
    fn reading_never_needs_permission() {
        assert!(
            approval_reason(ApprovalPolicy::OnRequest, &capabilities(ToolKind::Read)).is_none()
        );
    }

    #[test]
    fn running_a_command_does() {
        assert!(
            approval_reason(ApprovalPolicy::OnRequest, &capabilities(ToolKind::Execute)).is_some()
        );
    }

    #[test]
    fn a_policy_of_never_asks_about_nothing() {
        for kind in [ToolKind::Edit, ToolKind::Execute, ToolKind::Network] {
            assert!(approval_reason(ApprovalPolicy::Never, &capabilities(kind)).is_none());
        }
    }

    #[test]
    fn always_allowing_is_remembered_for_the_tool() {
        let memory = ApprovalMemory::default();
        assert!(!memory.is_always_allowed("bash"));
        memory.allow_always("bash");
        assert!(memory.is_always_allowed("bash"));
        assert!(!memory.is_always_allowed("write_file"));
    }
}
