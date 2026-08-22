//! Composing contributors.
//!
//! [`ExtensionRegistryBuilder`] collects contributions; [`ExtensionRegistry`] is
//! the immutable, `Arc`-shared result the engine consults. Building is a
//! one-way transition, so the set of extensions cannot change mid-session and
//! the engine never needs a lock to read it.

use std::sync::Arc;

use crate::ApprovalReviewContributor;
use crate::ContextContributor;
use crate::ToolContributor;
use crate::ToolGuard;
use crate::ToolLifecycleContributor;
use crate::TurnLifecycleContributor;

/// Collects contributions before the session starts.
///
/// The convention every extension crate follows is a free function
/// `pub fn install(registry: &mut ExtensionRegistryBuilder, ..)` that registers
/// one `Arc`-shared value against each point it implements.
#[derive(Default)]
pub struct ExtensionRegistryBuilder {
    tools: Vec<Arc<dyn ToolContributor>>,
    context: Vec<Arc<dyn ContextContributor>>,
    turn_lifecycle: Vec<Arc<dyn TurnLifecycleContributor>>,
    tool_lifecycle: Vec<Arc<dyn ToolLifecycleContributor>>,
    approval: Vec<Arc<dyn ApprovalReviewContributor>>,
    guards: Vec<ToolGuard>,
}

impl ExtensionRegistryBuilder {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn tool_contributor(&mut self, contributor: Arc<dyn ToolContributor>) -> &mut Self {
        self.tools.push(contributor);
        self
    }

    pub fn context_contributor(&mut self, contributor: Arc<dyn ContextContributor>) -> &mut Self {
        self.context.push(contributor);
        self
    }

    pub fn turn_lifecycle_contributor(
        &mut self,
        contributor: Arc<dyn TurnLifecycleContributor>,
    ) -> &mut Self {
        self.turn_lifecycle.push(contributor);
        self
    }

    pub fn tool_lifecycle_contributor(
        &mut self,
        contributor: Arc<dyn ToolLifecycleContributor>,
    ) -> &mut Self {
        self.tool_lifecycle.push(contributor);
        self
    }

    /// Register an approval reviewer. Registration order is priority order: the
    /// first reviewer returning `Some` decides.
    pub fn approval_review_contributor(
        &mut self,
        contributor: Arc<dyn ApprovalReviewContributor>,
    ) -> &mut Self {
        self.approval.push(contributor);
        self
    }

    /// Register a denial-only guard.
    pub fn tool_guard(&mut self, guard: ToolGuard) -> &mut Self {
        self.guards.push(guard);
        self
    }

    #[must_use]
    pub fn build(self) -> ExtensionRegistry {
        ExtensionRegistry {
            tools: self.tools,
            context: self.context,
            turn_lifecycle: self.turn_lifecycle,
            tool_lifecycle: self.tool_lifecycle,
            approval: self.approval,
            guards: Arc::new(self.guards),
        }
    }
}

/// The composed, immutable extension set.
#[derive(Clone, Default)]
pub struct ExtensionRegistry {
    tools: Vec<Arc<dyn ToolContributor>>,
    context: Vec<Arc<dyn ContextContributor>>,
    turn_lifecycle: Vec<Arc<dyn TurnLifecycleContributor>>,
    tool_lifecycle: Vec<Arc<dyn ToolLifecycleContributor>>,
    approval: Vec<Arc<dyn ApprovalReviewContributor>>,
    guards: Arc<Vec<ToolGuard>>,
}

impl ExtensionRegistry {
    pub fn tool_contributors(&self) -> impl Iterator<Item = &Arc<dyn ToolContributor>> {
        self.tools.iter()
    }

    pub fn context_contributors(&self) -> impl Iterator<Item = &Arc<dyn ContextContributor>> {
        self.context.iter()
    }

    pub fn turn_lifecycle_contributors(
        &self,
    ) -> impl Iterator<Item = &Arc<dyn TurnLifecycleContributor>> {
        self.turn_lifecycle.iter()
    }

    pub fn tool_lifecycle_contributors(
        &self,
    ) -> impl Iterator<Item = &Arc<dyn ToolLifecycleContributor>> {
        self.tool_lifecycle.iter()
    }

    pub fn approval_contributors(
        &self,
    ) -> impl Iterator<Item = &Arc<dyn ApprovalReviewContributor>> {
        self.approval.iter()
    }

    /// Run every guard, returning the first denial reason.
    ///
    /// Guards cannot allow, so a single denial is final regardless of what any
    /// other guard or contributor thinks.
    #[must_use]
    pub fn first_denial(&self, call: &keke_protocol::ToolCall) -> Option<String> {
        self.guards.iter().find_map(|guard| guard(call))
    }
}

impl std::fmt::Debug for ExtensionRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExtensionRegistry")
            .field("tools", &self.tools.len())
            .field("context", &self.context.len())
            .field("turn_lifecycle", &self.turn_lifecycle.len())
            .field("tool_lifecycle", &self.tool_lifecycle.len())
            .field("approval", &self.approval.len())
            .field("guards", &self.guards.len())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use keke_protocol::ToolCall;
    use keke_protocol::ToolCallId;

    use super::*;

    fn call(name: &str) -> ToolCall {
        ToolCall {
            id: ToolCallId::new("c1"),
            name: name.to_string(),
            arguments: serde_json::Value::Null,
        }
    }

    #[test]
    fn a_permissive_guard_cannot_undo_a_restrictive_one() {
        let mut builder = ExtensionRegistryBuilder::new();
        // Registered first and denies.
        builder.tool_guard(Box::new(|call| {
            (call.name == "bash").then(|| "shell is disabled".to_string())
        }));
        // Registered second; has no way to express "allow".
        builder.tool_guard(Box::new(|_| None));
        let registry = builder.build();

        assert_eq!(
            registry.first_denial(&call("bash")).as_deref(),
            Some("shell is disabled")
        );
        assert!(registry.first_denial(&call("read_file")).is_none());
    }
}
