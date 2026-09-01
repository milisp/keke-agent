//! A [`Conversation`] backed by a session in this process.
//!
//! The session is owned by one task and spoken to over a channel rather than
//! held behind a lock: `run_turn` takes `&mut Session` and runs for as long as
//! the model does, so a mutex would mean the surface blocking on a redraw
//! behind a turn that is still streaming.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use keke_config_types::ApprovalPolicy;
use keke_config_types::SessionMode;
use keke_core::ApprovalSwitch;
use keke_core::CoreError;
use keke_core::EffortSwitch;
use keke_core::ModelSwitch;
use keke_core::SessionBuilder;
use keke_core::SessionModeSwitch;
use keke_core::TurnUpdate;
use keke_plugin_api::ApprovalDecision;
use keke_plugin_api::ApprovalRequest;
use keke_plugin_api::ApprovalReviewContributor;
use keke_plugin_api::ExtFuture;
use keke_plugin_api::ExtensionContext;
use keke_plugin_api::ExtensionRegistryBuilder;
use keke_protocol::Message;
use keke_protocol::ReasoningEffort;
use keke_protocol::RewindScope;
use keke_protocol::ToolCall;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::oneshot;

use crate::Conversation;
use crate::ConversationError;
use crate::ConversationFuture;
use crate::Opened;
use crate::PermissionAnswer;
use crate::PermissionId;
use crate::RewindPoint;
use crate::Rewound;
use crate::Update;
use crate::conversation::SubagentView;

/// One request awaiting a person.
struct Pending {
    id: PermissionId,
    call: ToolCall,
    reason: String,
}

/// The stream of requests the bridge raises. Handed straight to [`local`].
pub struct ApprovalRequests(UnboundedReceiver<Pending>);

/// Turns the engine's approval reviews into something a surface can answer.
///
/// Registered like any other extension, because that is the only way the engine
/// learns about it — nothing in `keke-core` knows a surface exists.
pub struct Approvals {
    requests: UnboundedSender<Pending>,
    waiting: Mutex<HashMap<PermissionId, oneshot::Sender<(PermissionAnswer, Option<String>)>>>,
    next: AtomicU64,
}

/// Build the bridge and the stream of requests it will raise.
#[must_use]
pub fn approvals() -> (Arc<Approvals>, ApprovalRequests) {
    let (requests, receiver) = tokio::sync::mpsc::unbounded_channel();
    (
        Arc::new(Approvals {
            requests,
            waiting: Mutex::new(HashMap::new()),
            next: AtomicU64::new(1),
        }),
        ApprovalRequests(receiver),
    )
}

/// Register the bridge, following the extension convention.
pub fn install(registry: &mut ExtensionRegistryBuilder, approvals: Arc<Approvals>) {
    registry.approval_review_contributor(approvals);
}

impl Approvals {
    /// Deliver a person's answer. Answering an unknown or already-answered
    /// request is ignored rather than an error: a second keypress on a prompt
    /// that has just been withdrawn is not a mistake worth reporting.
    pub fn answer(&self, id: &PermissionId, answer: PermissionAnswer, note: Option<String>) {
        let responder = self
            .waiting
            .lock()
            .ok()
            .and_then(|mut waiting| waiting.remove(id));
        if let Some(responder) = responder {
            let _ = responder.send((answer, note));
        }
    }

    /// Refuse everything outstanding.
    ///
    /// A cancelled turn must not leave the engine parked on a prompt nobody
    /// will ever see again — that is a hang, not an abort.
    pub fn withdraw_all(&self) {
        let Ok(mut waiting) = self.waiting.lock() else {
            return;
        };
        for (_, responder) in waiting.drain() {
            let _ = responder.send((PermissionAnswer::Deny, None));
        }
    }
}

impl ApprovalReviewContributor for Approvals {
    fn review<'a>(
        &'a self,
        _ctx: &'a ExtensionContext,
        request: &'a ApprovalRequest,
    ) -> ExtFuture<'a, Option<ApprovalDecision>> {
        Box::pin(async move {
            let id = PermissionId(format!(
                "perm-{}",
                self.next.fetch_add(1, Ordering::Relaxed)
            ));
            let (responder, answer) = oneshot::channel();
            // Registered before the request is announced, so an answer that
            // arrives the instant the prompt is drawn still finds its slot.
            self.waiting.lock().ok()?.insert(id.clone(), responder);
            self.requests
                .send(Pending {
                    id: id.clone(),
                    call: request.call.clone(),
                    reason: request.reason.clone(),
                })
                .ok()?;

            // No answer means the surface went away. Abstaining rather than
            // allowing leaves the engine's own default — denial — in charge.
            match answer.await.ok()? {
                (PermissionAnswer::Allow, note) => Some(ApprovalDecision::Allow { note }),
                (PermissionAnswer::AllowAlways, _) => Some(ApprovalDecision::AllowAlways),
                // What the person said *is* the refusal, when they said
                // anything: the reason reaches the model as the tool's result,
                // so "declined" is the answer only for someone who declined
                // without a word.
                (PermissionAnswer::Deny, note) => Some(ApprovalDecision::Deny {
                    reason: note.unwrap_or_else(|| "declined".to_string()),
                }),
            }
        })
    }
}

/// What the session task is asked to do.
enum Command {
    Prompt {
        text: String,
        done: oneshot::Sender<Result<(), String>>,
    },
    /// Replace the running session with a fresh one built from `recipe`.
    NewSession {
        done: oneshot::Sender<Result<Switches, String>>,
    },
    /// Wind the session back to just before its `nth` user turn.
    Rewind {
        nth: usize,
        scope: RewindScope,
        done: oneshot::Sender<Result<Option<Rewound>, String>>,
    },
    /// What the session says can be wound back to, and what a restore to one
    /// of those points would touch.
    RewindPoints {
        done: oneshot::Sender<Vec<RewindPoint>>,
    },
    ChangedSince {
        nth: usize,
        done: oneshot::Sender<Result<Vec<String>, String>>,
    },
}

/// The live switches a rebuilt session hands back, so [`LocalConversation`]
/// can point its own handles — the ones `set_model`, `set_reasoning_effort`
/// and `set_approval_policy` write through, and the one `cancel` calls — at
/// the session actually running instead of the one it replaced.
struct Switches {
    cancel: Box<dyn Fn() + Send + Sync>,
    approval: Arc<ApprovalSwitch>,
    mode: Arc<SessionModeSwitch>,
    effort: Arc<EffortSwitch>,
    model: Arc<ModelSwitch>,
}

/// A conversation with a session running in this process.
pub struct LocalConversation {
    commands: UnboundedSender<Command>,
    /// Behind a lock rather than fixed at construction: `new_session` swaps it
    /// for the replacement session's own canceller, so a Ctrl-C reaches
    /// whichever session is actually running.
    cancel: Mutex<Box<dyn Fn() + Send + Sync>>,
    approvals: Arc<Approvals>,
    /// Held rather than sent as a command: the session task is busy for as long
    /// as a turn runs, so a queued mode change would arrive after the calls it
    /// was meant to govern. Behind a lock for the same reason `cancel` is: a
    /// fresh session from `new_session` has its own switch to write through.
    approval: Mutex<Arc<ApprovalSwitch>>,
    /// Held for the same reason as `approval`: a queued effort change would
    /// arrive after the steps it was meant to govern.
    effort: Mutex<Arc<EffortSwitch>>,
    /// Held for the same reason again: a model change queued behind a running
    /// turn would take effect an answer too late.
    model: Mutex<Arc<ModelSwitch>>,
    /// Held for the strongest version of that reason: a mode change queued
    /// behind a running turn would arrive after the edits it was meant to
    /// stop.
    mode: Mutex<Arc<SessionModeSwitch>>,
    /// So a mode change a surface makes comes back on the update stream the
    /// same way one the agent made does — a surface draws from
    /// [`Update::ModeChanged`] and must not have to special-case its own.
    updates: UnboundedSender<Update>,
}

/// Start a session and hand back the conversation and its updates.
///
/// The builder must already carry the extensions — including the bridge from
/// [`install`] — because an [`keke_plugin_api::ExtensionRegistry`] is frozen
/// once built and a surface must not be able to add to it afterwards.
pub async fn local(
    builder: SessionBuilder,
    approvals: Arc<Approvals>,
    requests: ApprovalRequests,
) -> Result<Opened, CoreError> {
    local_with(builder, approvals, requests, None).await
}

/// [`local`], plus a stream of subagent snapshots to relay to the surface.
///
/// Taken as a receiver rather than as a handle to whatever produces it: this
/// crate does not know that subagents exist beyond the shape of a row, which is
/// what keeps the same `Update` stream honest when the surface is across a pipe
/// and there is nothing in this process to poll.
pub async fn local_with(
    builder: SessionBuilder,
    approvals: Arc<Approvals>,
    requests: ApprovalRequests,
    subagents: Option<UnboundedReceiver<Vec<SubagentView>>>,
) -> Result<Opened, CoreError> {
    let (turn_tx, turn_rx) = tokio::sync::mpsc::unbounded_channel();
    let with_updates = builder.updates(turn_tx);
    // Cloned before the resume this build may carry is consumed: `new_session`
    // rebuilds from this recipe, and a session started fresh must not silently
    // resume the log the first one did.
    let recipe = with_updates.clone().fresh();
    let mut session = with_updates.build().await?;
    let id = session.id().to_string();
    let cancel = session.canceller();
    let approval = session.approval_switch();
    let configured_approval = approval.get();
    let effort = session.effort_switch();
    let configured_effort = effort.get();
    let model_switch = session.model_switch();
    let mode_switch = session.mode_switch();
    let configured_mode = mode_switch.get();
    let model = session.model().to_string();

    let (updates, update_rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(publish(turn_rx, requests, subagents, updates.clone()));

    let (commands, mut inbox) = tokio::sync::mpsc::unbounded_channel();
    let echo = updates.clone();
    tokio::spawn(async move {
        while let Some(command) = inbox.recv().await {
            match command {
                Command::Prompt { text, done } => {
                    let outcome = session.run_turn(Message::user(text)).await;
                    let answer = match outcome {
                        Ok(_) => Ok(()),
                        Err(error) => {
                            // Reported on the update stream too: a surface
                            // renders the failure from there, and the caller
                            // of `prompt` may have stopped listening.
                            let _ = updates.send(Update::Failed(error.to_string()));
                            Err(error.to_string())
                        }
                    };
                    let _ = done.send(answer);
                }
                Command::Rewind { nth, scope, done } => {
                    let outcome = session
                        .rewind_to_user_turn(nth, scope)
                        .await
                        .map(|rewound| {
                            rewound.map(|rewound| Rewound {
                                prompt: rewound.prompt,
                                removed_messages: rewound.removed_messages,
                                restored_files: rewound.restored_files,
                            })
                        })
                        .map_err(|error| error.to_string());
                    let _ = done.send(outcome);
                }
                Command::RewindPoints { done } => {
                    let points = session
                        .rewind_points()
                        .into_iter()
                        .map(|point| RewindPoint {
                            turn: point.turn,
                            prompt: point.prompt,
                            has_snapshot: point.has_snapshot,
                        })
                        .collect();
                    let _ = done.send(points);
                }
                Command::ChangedSince { nth, done } => {
                    let outcome = session
                        .changed_since_turn(nth)
                        .await
                        .map_err(|error| error.to_string());
                    let _ = done.send(outcome);
                }
                Command::NewSession { done } => match recipe.clone().build().await {
                    Ok(fresh) => {
                        let switches = Switches {
                            cancel: Box::new(fresh.canceller()),
                            approval: fresh.approval_switch(),
                            effort: fresh.effort_switch(),
                            model: fresh.model_switch(),
                            mode: fresh.mode_switch(),
                        };
                        // A rebuild shares the recipe's switch, so a session
                        // that was planning would come back planning. Starting
                        // over means starting over: the mode goes back to what
                        // a fresh launch has, and the surface is told, because
                        // nothing else would tell it.
                        fresh.mode_switch().set(SessionMode::default());
                        let _ = updates.send(Update::ModeChanged(SessionMode::default()));
                        session = fresh;
                        // `new_session` only swaps what a surface writes
                        // through; without this, nothing ever tells it the
                        // swap happened, so what it draws never changes.
                        let _ = updates.send(Update::SessionReset);
                        let _ = done.send(Ok(switches));
                    }
                    Err(error) => {
                        let _ = done.send(Err(error.to_string()));
                    }
                },
            }
        }
    });

    Ok(Opened {
        id,
        model,
        // The composition root knows what the provider serves; `local` only
        // starts the session.
        models: Vec::new(),
        effort: configured_effort,
        approval_policy: configured_approval,
        mode: configured_mode,
        conversation: Arc::new(LocalConversation {
            commands,
            cancel: Mutex::new(Box::new(cancel)),
            approvals,
            approval: Mutex::new(approval),
            effort: Mutex::new(effort),
            model: Mutex::new(model_switch),
            mode: Mutex::new(mode_switch),
            updates: echo,
        }),
        updates: update_rx,
        // Whoever rebuilt the history is the one that has it; `local` only
        // starts what the builder was already told to continue.
        history: Vec::new(),
        // Same as `models`: the composition root knows what the plugins
        // contributed, `local` only starts the session.
        commands: Vec::new(),
    })
}

/// Merge the engine's turn updates with the approval prompts.
///
/// Biased towards turn updates so everything already emitted is published
/// before a prompt raised during dispatch: a request to approve a call whose
/// "started" line has not been drawn yet reads as a prompt about nothing.
async fn publish(
    mut turns: UnboundedReceiver<TurnUpdate>,
    ApprovalRequests(mut requests): ApprovalRequests,
    subagents: Option<UnboundedReceiver<Vec<SubagentView>>>,
    updates: UnboundedSender<Update>,
) {
    // A composition with no subagent host gets a channel whose sender is held
    // here for the life of the loop: its `recv` is then pending forever, which
    // is what the select arm needs. A dropped sender would resolve immediately
    // and spin.
    let (_never, mut subagents) = match subagents {
        Some(rx) => (None, rx),
        None => {
            let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
            (Some(tx), rx)
        }
    };
    loop {
        let update = tokio::select! {
            biased;
            turn = turns.recv() => match turn {
                Some(turn) => translate(turn),
                None => break,
            },
            request = requests.recv() => match request {
                Some(Pending { id, call, reason }) => {
                    Update::PermissionRequested { id, call, reason }
                }
                None => continue,
            },
            rows = subagents.recv() => match rows {
                Some(rows) => Update::Subagents(rows),
                None => continue,
            },
        };
        if updates.send(update).is_err() {
            break;
        }
    }
}

fn translate(turn: TurnUpdate) -> Update {
    match turn {
        TurnUpdate::TurnStarted { .. } => Update::TurnStarted,
        TurnUpdate::TextDelta { delta, .. } => Update::TextDelta(delta),
        TurnUpdate::ThinkingDelta { delta, .. } => Update::ThinkingDelta(delta),
        TurnUpdate::ToolCallStarted { call } => Update::ToolCallStarted(call),
        TurnUpdate::ToolCallEnded { result } => Update::ToolCallEnded(result),
        TurnUpdate::HostedToolCall { name, query, .. } => Update::HostedToolCall { name, query },
        TurnUpdate::StepUsage { usage, .. } => Update::TokensUsed(usage),
        TurnUpdate::TurnEnded { stop_reason, .. } => Update::TurnEnded(stop_reason),
    }
}

impl Conversation for LocalConversation {
    fn prompt<'a>(&'a self, text: String) -> ConversationFuture<'a, Result<(), ConversationError>> {
        Box::pin(async move {
            let (done, answer) = oneshot::channel();
            self.commands
                .send(Command::Prompt { text, done })
                .map_err(|_| ConversationError::Disconnected("the session ended".to_string()))?;
            answer
                .await
                .map_err(|_| ConversationError::Disconnected("the session ended".to_string()))?
                .map_err(ConversationError::Agent)
        })
    }

    fn cancel(&self) {
        if let Ok(cancel) = self.cancel.lock() {
            (cancel)();
        }
        // Order matters: the flag is set first, so a turn released from its
        // prompt finds the cancel already in place and stops instead of
        // carrying on with the next tool.
        self.approvals.withdraw_all();
    }

    fn respond_to_permission(
        &self,
        id: &PermissionId,
        answer: PermissionAnswer,
        note: Option<String>,
    ) {
        self.approvals.answer(id, answer, note);
    }

    fn set_session_mode(&self, mode: SessionMode) {
        if let Ok(switch) = self.mode.lock() {
            switch.set(mode);
        }
        // Echoed rather than assumed: what a surface draws comes from the
        // update stream, so its own toggle and one the agent made arrive by
        // the same route.
        let _ = self.updates.send(Update::ModeChanged(mode));
    }

    fn set_approval_policy(&self, policy: ApprovalPolicy) {
        if let Ok(approval) = self.approval.lock() {
            approval.set(policy);
        }
    }

    fn set_model(&self, model: String) {
        if let Ok(switch) = self.model.lock() {
            switch.set(model.as_str());
        }
    }

    fn set_reasoning_effort(&self, effort: Option<ReasoningEffort>) {
        if let Ok(switch) = self.effort.lock() {
            switch.set(effort);
        }
    }

    fn rewind_points(&self) -> ConversationFuture<'_, Result<Vec<RewindPoint>, ConversationError>> {
        Box::pin(async move {
            let (done, answer) = oneshot::channel();
            self.commands
                .send(Command::RewindPoints { done })
                .map_err(|_| ConversationError::Disconnected("the session ended".to_string()))?;
            answer
                .await
                .map_err(|_| ConversationError::Disconnected("the session ended".to_string()))
        })
    }

    fn changed_since(
        &self,
        nth: usize,
    ) -> ConversationFuture<'_, Result<Vec<String>, ConversationError>> {
        Box::pin(async move {
            let (done, answer) = oneshot::channel();
            self.commands
                .send(Command::ChangedSince { nth, done })
                .map_err(|_| ConversationError::Disconnected("the session ended".to_string()))?;
            answer
                .await
                .map_err(|_| ConversationError::Disconnected("the session ended".to_string()))?
                .map_err(ConversationError::Agent)
        })
    }

    fn rewind(
        &self,
        nth: usize,
        scope: RewindScope,
    ) -> ConversationFuture<'_, Result<Option<Rewound>, ConversationError>> {
        Box::pin(async move {
            let (done, answer) = oneshot::channel();
            self.commands
                .send(Command::Rewind { nth, scope, done })
                .map_err(|_| ConversationError::Disconnected("the session ended".to_string()))?;
            answer
                .await
                .map_err(|_| ConversationError::Disconnected("the session ended".to_string()))?
                .map_err(ConversationError::Agent)
        })
    }

    fn new_session(&self) -> ConversationFuture<'_, Result<(), ConversationError>> {
        Box::pin(async move {
            let (done, answer) = oneshot::channel();
            self.commands
                .send(Command::NewSession { done })
                .map_err(|_| ConversationError::Disconnected("the session ended".to_string()))?;
            let switches = answer
                .await
                .map_err(|_| ConversationError::Disconnected("the session ended".to_string()))?
                .map_err(ConversationError::Agent)?;
            // A turn that was still running against the old session is now
            // parked on a prompt or a cancel flag nobody will ever check
            // again — withdraw it before this conversation starts pointing at
            // a different session entirely.
            self.approvals.withdraw_all();
            if let Ok(mut cancel) = self.cancel.lock() {
                *cancel = switches.cancel;
            }
            if let Ok(mut approval) = self.approval.lock() {
                *approval = switches.approval;
            }
            if let Ok(mut effort) = self.effort.lock() {
                *effort = switches.effort;
            }
            if let Ok(mut model) = self.model.lock() {
                *model = switches.model;
            }
            if let Ok(mut mode) = self.mode.lock() {
                *mode = switches.mode;
            }
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use keke_protocol::SessionId;
    use keke_protocol::ThreadId;
    use keke_protocol::ToolCallId;

    use super::*;

    fn context() -> ExtensionContext {
        ExtensionContext::new(SessionId::new(), ThreadId::new())
    }

    fn request() -> ApprovalRequest {
        ApprovalRequest {
            call: ToolCall {
                id: ToolCallId::new("c1"),
                name: "bash".to_string(),
                arguments: serde_json::json!({ "command": "rm -rf /" }),
            },
            reason: "runs a command".to_string(),
        }
    }

    #[tokio::test]
    async fn a_review_waits_for_the_person_and_returns_their_answer() {
        let (approvals, ApprovalRequests(mut requests)) = approvals();
        let ctx = context();
        let review = {
            let approvals = Arc::clone(&approvals);
            tokio::spawn(async move {
                let ctx = context();
                approvals.review(&ctx, &request()).await
            })
        };
        drop(ctx);

        let pending = requests
            .recv()
            .await
            .expect("a request reaches the surface");
        assert_eq!(pending.call.name, "bash");
        approvals.answer(&pending.id, PermissionAnswer::AllowAlways, None);

        assert_eq!(
            review.await.expect("join"),
            Some(ApprovalDecision::AllowAlways)
        );
    }

    #[tokio::test]
    async fn a_denial_carries_a_reason_the_model_can_read() {
        let (approvals, ApprovalRequests(mut requests)) = approvals();
        let review = {
            let approvals = Arc::clone(&approvals);
            tokio::spawn(async move {
                let ctx = context();
                approvals.review(&ctx, &request()).await
            })
        };

        let pending = requests.recv().await.expect("a request");
        approvals.answer(&pending.id, PermissionAnswer::Deny, None);
        assert!(matches!(
            review.await.expect("join"),
            Some(ApprovalDecision::Deny { .. })
        ));
    }

    /// A cancelled turn parked on a prompt nobody will answer is a hang, not an
    /// abort.
    #[tokio::test]
    async fn withdrawing_releases_a_turn_blocked_on_a_prompt() {
        let (approvals, ApprovalRequests(mut requests)) = approvals();
        let review = {
            let approvals = Arc::clone(&approvals);
            tokio::spawn(async move {
                let ctx = context();
                approvals.review(&ctx, &request()).await
            })
        };

        requests.recv().await.expect("a request");
        approvals.withdraw_all();
        assert!(matches!(
            review.await.expect("join"),
            Some(ApprovalDecision::Deny { .. })
        ));
    }

    /// The surface is gone. Abstaining leaves the engine's own default — a
    /// denial — in charge, rather than quietly allowing.
    #[tokio::test]
    async fn a_vanished_surface_abstains_rather_than_allowing() {
        let (approvals, requests) = approvals();
        drop(requests);
        let ctx = context();
        assert_eq!(approvals.review(&ctx, &request()).await, None);
    }

    #[test]
    fn answering_an_unknown_prompt_is_ignored() {
        let (approvals, _requests) = approvals();
        approvals.answer(
            &PermissionId("nope".to_string()),
            PermissionAnswer::Allow,
            None,
        );
    }
}
