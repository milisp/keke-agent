//! Deciding when to ask, and remembering the answer.
//!
//! Approval reviewers were registered by extensions long before anything
//! consulted them; this is the piece that does. It sits between the guards and
//! the tool body, which is what keeps denial monotonic: a guard's denial is
//! already final by the time anyone is asked, so no answer here can restore a
//! permission a guard took away.

use std::collections::BTreeSet;
use std::sync::Mutex;
use std::sync::atomic::AtomicU8;
use std::sync::atomic::Ordering;

use keke_config_types::ApprovalPolicy;
use keke_tool::ApprovalRequirement;
use keke_tool::ToolCapabilities;
use keke_tool::ToolKind;

/// Whether a call needs a person's assent, and why.
///
/// The reason is the text whoever decides will read, so it names the effect
/// rather than the policy: "may write outside the workspace" is actionable,
/// "policy is on-request" is not.
#[must_use]
pub fn approval_reason(policy: ApprovalPolicy, capabilities: &ToolCapabilities) -> Option<String> {
    // Before the policy, because this is the tool saying the policy cannot
    // answer for a person here — a call whose whole purpose is to be decided.
    // Where there is nobody to ask, `dispatch` denies it with a reason, so a
    // non-interactive run stops rather than proceeds unasked.
    if capabilities.approval == ApprovalRequirement::Always {
        return Some("needs a person's answer".to_string());
    }
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
    use super::ApprovalSwitch;

    /// The switch is what the turn loop reads, so a change has to be visible
    /// through a shared handle rather than only to whoever set it.
    #[test]
    fn a_switch_shares_the_policy_it_is_given() {
        let switch = std::sync::Arc::new(ApprovalSwitch::new(ApprovalPolicy::OnRequest));
        let held = std::sync::Arc::clone(&switch);
        switch.set(ApprovalPolicy::Never);
        assert_eq!(held.get(), ApprovalPolicy::Never);
    }

    use super::*;

    fn capabilities(kind: ToolKind) -> ToolCapabilities {
        ToolCapabilities {
            approval: ApprovalRequirement::ByPolicy,
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

    /// A tool whose call is the question is asked about under every policy —
    /// including the one that exists so nothing is asked.
    #[test]
    fn a_tool_that_always_asks_is_asked_about_under_every_policy() {
        let always = capabilities(ToolKind::Meta).always_asks();
        for policy in [
            ApprovalPolicy::OnRequest,
            ApprovalPolicy::OnFailure,
            ApprovalPolicy::Never,
        ] {
            assert!(
                approval_reason(policy, &always).is_some(),
                "{policy:?} answered for a person it should not have"
            );
        }
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

/// The approval policy a session is running under, shared so it can change
/// while a turn is in flight.
///
/// A person who raises the bar mid-turn means "ask me about the next call", not
/// "ask me once this turn is over", so the loop has to read the live value
/// rather than a copy taken when the turn began. Copyable handles also let a
/// surface hold the switch without holding the session, the same way
/// [`Session::canceller`](crate::Session::canceller) works.
#[derive(Debug)]
pub struct ApprovalSwitch(AtomicU8);

impl ApprovalSwitch {
    #[must_use]
    pub fn new(policy: ApprovalPolicy) -> Self {
        Self(AtomicU8::new(encode(policy)))
    }

    #[must_use]
    pub fn get(&self) -> ApprovalPolicy {
        decode(self.0.load(Ordering::Relaxed))
    }

    pub fn set(&self, policy: ApprovalPolicy) {
        self.0.store(encode(policy), Ordering::Relaxed);
    }
}

fn encode(policy: ApprovalPolicy) -> u8 {
    match policy {
        ApprovalPolicy::OnRequest => 0,
        ApprovalPolicy::OnFailure => 1,
        ApprovalPolicy::Never => 2,
    }
}

fn decode(raw: u8) -> ApprovalPolicy {
    match raw {
        1 => ApprovalPolicy::OnFailure,
        2 => ApprovalPolicy::Never,
        // Only `encode` ever writes the cell, so anything else is impossible;
        // the strictest policy is the safe reading if it ever happened.
        _ => ApprovalPolicy::OnRequest,
    }
}
