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
use keke_core::ApprovalSwitch;
use keke_core::CoreError;
use keke_core::SessionBuilder;
use keke_core::TurnUpdate;
use keke_plugin_api::ApprovalDecision;
use keke_plugin_api::ApprovalRequest;
use keke_plugin_api::ApprovalReviewContributor;
use keke_plugin_api::ExtFuture;
use keke_plugin_api::ExtensionContext;
use keke_plugin_api::ExtensionRegistryBuilder;
use keke_protocol::Message;
use keke_protocol::ToolCall;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::oneshot;

use crate::Conversation;
use crate::ConversationError;
use crate::ConversationFuture;
use crate::PermissionAnswer;
use crate::PermissionId;
use crate::Update;

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
    waiting: Mutex<HashMap<PermissionId, oneshot::Sender<PermissionAnswer>>>,
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
    pub fn answer(&self, id: &PermissionId, answer: PermissionAnswer) {
        let responder = self
            .waiting
            .lock()
            .ok()
            .and_then(|mut waiting| waiting.remove(id));
        if let Some(responder) = responder {
            let _ = responder.send(answer);
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
            let _ = responder.send(PermissionAnswer::Deny);
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
                PermissionAnswer::Allow => Some(ApprovalDecision::Allow),
                PermissionAnswer::AllowAlways => Some(ApprovalDecision::AllowAlways),
                PermissionAnswer::Deny => Some(ApprovalDecision::Deny {
                    reason: "declined".to_string(),
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
}

/// A conversation with a session running in this process.
pub struct LocalConversation {
    commands: UnboundedSender<Command>,
    cancel: Box<dyn Fn() + Send + Sync>,
    approvals: Arc<Approvals>,
    /// Held rather than sent as a command: the session task is busy for as long
    /// as a turn runs, so a queued mode change would arrive after the calls it
    /// was meant to govern.
    approval: Arc<ApprovalSwitch>,
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
) -> Result<(Arc<dyn Conversation>, UnboundedReceiver<Update>), CoreError> {
    let (turn_tx, turn_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut session = builder.updates(turn_tx).build().await?;
    let cancel = session.canceller();
    let approval = session.approval_switch();

    let (updates, update_rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(publish(turn_rx, requests, updates.clone()));

    let (commands, mut inbox) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Some(Command::Prompt { text, done }) = inbox.recv().await {
            let outcome = session.run_turn(Message::user(text)).await;
            let answer = match outcome {
                Ok(_) => Ok(()),
                Err(error) => {
                    // Reported on the update stream too: a surface renders the
                    // failure from there, and the caller of `prompt` may have
                    // stopped listening.
                    let _ = updates.send(Update::Failed(error.to_string()));
                    Err(error.to_string())
                }
            };
            let _ = done.send(answer);
        }
    });

    Ok((
        Arc::new(LocalConversation {
            commands,
            cancel: Box::new(cancel),
            approvals,
            approval,
        }),
        update_rx,
    ))
}

/// Merge the engine's turn updates with the approval prompts.
///
/// Biased towards turn updates so everything already emitted is published
/// before a prompt raised during dispatch: a request to approve a call whose
/// "started" line has not been drawn yet reads as a prompt about nothing.
async fn publish(
    mut turns: UnboundedReceiver<TurnUpdate>,
    ApprovalRequests(mut requests): ApprovalRequests,
    updates: UnboundedSender<Update>,
) {
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
        (self.cancel)();
        // Order matters: the flag is set first, so a turn released from its
        // prompt finds the cancel already in place and stops instead of
        // carrying on with the next tool.
        self.approvals.withdraw_all();
    }

    fn respond_to_permission(&self, id: &PermissionId, answer: PermissionAnswer) {
        self.approvals.answer(id, answer);
    }

    fn set_approval_policy(&self, policy: ApprovalPolicy) {
        self.approval.set(policy);
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
        approvals.answer(&pending.id, PermissionAnswer::AllowAlways);

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
        approvals.answer(&pending.id, PermissionAnswer::Deny);
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
        approvals.answer(&PermissionId("nope".to_string()), PermissionAnswer::Allow);
    }
}
