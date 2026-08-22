//! Runs plugin-contributed hook programs at turn and tool lifecycle points.
//!
//! A hook is an arbitrary shell command line, contributed by a plugin the
//! person chose to install and run with that person's own privileges. This
//! crate schedules those commands and reads their answers; it is not a sandbox
//! and claims no containment property. See [`run`] for the same warning where
//! the spawning actually happens.
//!
//! # Denial is monotonic
//!
//! The only hook that can affect a decision is `PreToolUse`, and the only thing
//! it can do is deny: a non-zero exit stops the call, and a zero exit *declines
//! to deny* rather than allowing anything. There is no result a hook can return
//! that undoes another hook's denial, so no installation order and no
//! deliberately permissive plugin can reopen what something else closed
//! (`AGENTS.md` invariant 7).
//!
//! A hook that cannot answer — it fails to spawn, or runs past its declared
//! timeout — denies for the same reason. Silence is not consent.
//!
//! Every other event is observation only. When one of those fails there is no
//! decision to affect, so the failure is logged and the turn continues.

mod run;

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use keke_config_types::PluginTimeouts;
use keke_plugin::HookEvent;
use keke_plugin::PluginSet;
use keke_plugin::ResolvedHook;
use keke_plugin_api::ExtFuture;
use keke_plugin_api::ExtensionContext;
use keke_plugin_api::ExtensionRegistryBuilder;
use keke_plugin_api::ToolLifecycleContributor;
use keke_plugin_api::TurnLifecycleContributor;
use keke_protocol::StopReason;
use keke_protocol::ToolCall;
use keke_protocol::ToolCallId;
use keke_protocol::TurnId;
use serde_json::Value;
use serde_json::json;

/// Register the hook runner against every point it participates in.
///
/// One value backs all three registrations, because the guard has to read what
/// the tool lifecycle contributor recorded — splitting them would put a channel
/// between a decision and the decision's evidence.
pub fn install(registry: &mut ExtensionRegistryBuilder, plugins: &PluginSet) {
    install_with(registry, plugins, PluginTimeouts::default());
}

/// Register the hook runner under `timeouts`.
pub fn install_with(
    registry: &mut ExtensionRegistryBuilder,
    plugins: &PluginSet,
    timeouts: PluginTimeouts,
) {
    let hooks = Arc::new(Hooks::new(plugins, timeouts.hook_millis));
    registry.turn_lifecycle_contributor(Arc::clone(&hooks) as Arc<dyn TurnLifecycleContributor>);
    registry.tool_lifecycle_contributor(Arc::clone(&hooks) as Arc<dyn ToolLifecycleContributor>);
    registry.tool_guard(Box::new(move |call| hooks.verdict(call)));
}

/// What running one call's `PreToolUse` hooks concluded.
///
/// `Declined` is not permission. It records only that the hooks which ran chose
/// not to deny, which is why the guard's answer for it is `None` — "this guard
/// has nothing to say" — and never an allow the engine could act on.
#[derive(Clone, Debug)]
enum Verdict {
    Declined,
    Denied(String),
}

struct Hooks {
    hooks: Vec<ResolvedHook>,
    /// Verdicts keyed by the model's own call id, so one tool call can never
    /// read the answer that belonged to another.
    verdicts: Mutex<HashMap<ToolCallId, Verdict>>,
    session_started: AtomicBool,
    /// Applied to a hook that declares no timeout of its own.
    default_millis: u64,
}

impl Hooks {
    fn new(plugins: &PluginSet, default_millis: u64) -> Self {
        // `Unsupported` hooks are dropped here and never run. Resolution keeps
        // them so a surface can tell the person their hook is inert; running a
        // command bound to an event this host does not implement would be
        // guessing at what the plugin meant.
        let hooks = plugins
            .plugins()
            .flat_map(|plugin| plugin.hooks.iter())
            .filter(|hook| hook.event.is_supported())
            .cloned()
            .collect();
        Self {
            hooks,
            verdicts: Mutex::new(HashMap::new()),
            session_started: AtomicBool::new(false),
            default_millis,
        }
    }

    fn for_event<'a>(&'a self, event: &'a HookEvent) -> impl Iterator<Item = &'a ResolvedHook> {
        self.hooks.iter().filter(move |hook| &hook.event == event)
    }

    fn for_tool<'a>(&'a self, event: &'a HookEvent, tool: &'a str) -> Vec<&'a ResolvedHook> {
        self.for_event(event)
            .filter(|hook| hook.matches(tool))
            .collect()
    }

    /// The guard's answer for `call`.
    ///
    /// Synchronous by contract, and running a program is not — so nothing is
    /// run here. The programs already ran in `on_tool_start`, which the engine
    /// awaits before consulting any guard, and this only reads what they
    /// concluded. That keeps the guard from blocking a runtime worker on a
    /// subprocess, which a `block_on` inside this closure would do.
    ///
    /// Every path that is not a recorded `Declined` denies:
    ///
    /// - a missing record means the hooks never ran, and an unasked hook has
    ///   not consented;
    /// - a poisoned lock means a previous run panicked, and a broken hook
    ///   runner does not get to wave calls through.
    fn verdict(&self, call: &ToolCall) -> Option<String> {
        if self.for_tool(&HookEvent::PreToolUse, &call.name).is_empty() {
            return None;
        }
        let Ok(verdicts) = self.verdicts.lock() else {
            return Some(format!(
                "denied: the hook runner failed while deciding on {}",
                call.name
            ));
        };
        match verdicts.get(&call.id) {
            Some(Verdict::Declined) => None,
            Some(Verdict::Denied(reason)) => Some(reason.clone()),
            None => Some(format!(
                "denied: PreToolUse hooks for {} did not run",
                call.name
            )),
        }
    }

    /// Run the `PreToolUse` hooks for `call` and record what they concluded.
    async fn decide(&self, ctx: &ExtensionContext, call: &ToolCall) {
        let payload = tool_payload(ctx, "PreToolUse", call);
        let mut verdict = Verdict::Declined;
        for hook in self.for_tool(&HookEvent::PreToolUse, &call.name) {
            match run::run(hook, &payload, self.default_millis).await {
                Ok(completed) if completed.success => continue,
                Ok(completed) => {
                    // A denial with nothing to say still denies: an empty
                    // reason must never be mistaken for an allow.
                    let reason = if completed.stdout.is_empty() {
                        format!("denied by a PreToolUse hook from plugin {}", hook.plugin)
                    } else {
                        completed.stdout
                    };
                    verdict = Verdict::Denied(reason);
                    break;
                }
                Err(failure) => {
                    verdict = Verdict::Denied(failure);
                    break;
                }
            }
        }
        if let Ok(mut verdicts) = self.verdicts.lock() {
            verdicts.insert(call.id.clone(), verdict);
        }
        // A poisoned lock is deliberately not repaired: `verdict` reads the
        // same lock and denies when it cannot, so losing the record fails
        // closed.
    }

    /// Run observation-only hooks, reporting failures and nothing more.
    async fn observe(&self, event: &HookEvent, payload: Value) {
        let hooks: Vec<&ResolvedHook> = self.for_event(event).collect();
        self.run_all(&hooks, event, payload).await;
    }

    /// The tool-scoped counterpart of [`Hooks::observe`].
    ///
    /// A `PostToolUse` hook carries the same matcher a `PreToolUse` one does,
    /// so it must be filtered the same way: a hook that asked to see `Bash`
    /// firing on every tool is a hook reporting on calls its author never
    /// claimed to be watching.
    async fn observe_tool(&self, event: &HookEvent, tool: &str, payload: Value) {
        let hooks = self.for_tool(event, tool);
        self.run_all(&hooks, event, payload).await;
    }

    async fn run_all(&self, hooks: &[&ResolvedHook], event: &HookEvent, payload: Value) {
        for hook in hooks {
            match run::run(hook, &payload, self.default_millis).await {
                Ok(completed) if completed.success => {}
                Ok(completed) => tracing::warn!(
                    plugin = %hook.plugin,
                    event = ?event,
                    output = %completed.stdout,
                    "hook exited non-zero; this event cannot deny, so the turn continues"
                ),
                Err(failure) => tracing::warn!(
                    plugin = %hook.plugin,
                    event = ?event,
                    "{failure}"
                ),
            }
        }
    }
}

impl TurnLifecycleContributor for Hooks {
    fn on_turn_start<'a>(&'a self, ctx: &'a ExtensionContext, turn: TurnId) -> ExtFuture<'a, ()> {
        Box::pin(async move {
            // There is no session-start callback to hang `SessionStart` on, and
            // the first turn is the first moment the session is observably
            // alive. Running it once here is closer to the ecosystem's meaning
            // than dropping the event.
            if !self.session_started.swap(true, Ordering::SeqCst) {
                self.observe(
                    &HookEvent::SessionStart,
                    turn_payload(ctx, "SessionStart", turn),
                )
                .await;
            }
            self.observe(
                &HookEvent::UserPromptSubmit,
                turn_payload(ctx, "UserPromptSubmit", turn),
            )
            .await;
        })
    }

    fn on_turn_end<'a>(
        &'a self,
        ctx: &'a ExtensionContext,
        turn: TurnId,
        reason: &'a StopReason,
    ) -> ExtFuture<'a, ()> {
        Box::pin(async move {
            let mut payload = turn_payload(ctx, "Stop", turn);
            if let Value::Object(fields) = &mut payload {
                fields.insert("stop_reason".into(), json!(format!("{reason:?}")));
            }
            self.observe(&HookEvent::Stop, payload).await;
        })
    }
}

impl ToolLifecycleContributor for Hooks {
    fn on_tool_start<'a>(
        &'a self,
        ctx: &'a ExtensionContext,
        call: &'a ToolCall,
    ) -> ExtFuture<'a, ()> {
        Box::pin(async move { self.decide(ctx, call).await })
    }

    fn on_tool_finish<'a>(
        &'a self,
        ctx: &'a ExtensionContext,
        call: &'a ToolCall,
        outcome: Result<(), &'a keke_tool::ToolError>,
    ) -> ExtFuture<'a, ()> {
        Box::pin(async move {
            // The verdict has served its purpose the moment the call is over.
            // Dropping it here is what keeps the map from growing for the life
            // of the session, one entry per tool call.
            if let Ok(mut verdicts) = self.verdicts.lock() {
                verdicts.remove(&call.id);
            }

            let mut payload = tool_payload(ctx, "PostToolUse", call);
            if let Some(fields) = payload.as_object_mut() {
                fields.insert("success".into(), json!(outcome.is_ok()));
                if let Err(error) = outcome {
                    fields.insert("error".into(), json!(error.to_string()));
                }
            }
            // Observation only. This event runs after the body, so there is
            // nothing left for it to deny even if it exits non-zero.
            self.observe_tool(&HookEvent::PostToolUse, &call.name, payload)
                .await;
        })
    }
}

/// The object a hook reads on stdin for a tool event.
fn tool_payload(ctx: &ExtensionContext, event: &str, call: &ToolCall) -> Value {
    json!({
        "hook_event_name": event,
        "session_id": ctx.session.to_string(),
        "thread_id": ctx.thread.to_string(),
        "tool_call_id": call.id.as_str(),
        "tool_name": call.name,
        "tool_input": call.arguments,
    })
}

fn turn_payload(ctx: &ExtensionContext, event: &str, turn: TurnId) -> Value {
    json!({
        "hook_event_name": event,
        "session_id": ctx.session.to_string(),
        "thread_id": ctx.thread.to_string(),
        "turn_id": turn.to_string(),
    })
}
