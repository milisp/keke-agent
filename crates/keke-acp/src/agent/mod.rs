//! keke as an ACP agent, spoken over stdio.
//!
//! The editor is the client and keke is the agent: it receives prompts and
//! emits session notifications. Nothing here reaches into the engine — it
//! drives a [`Conversation`], the same thing the terminal interface drives, so
//! the two surfaces cannot drift into different behaviour.
//!
//! Two protocol versions are served, because both exist in the wild: v1 is what
//! every released client speaks today, and v2 is the draft that folded
//! `session/load` into `session/resume` and moved the turn's outcome onto the
//! update stream. The client picks during `initialize` and the router hands the
//! connection to that implementation — no traffic is translated afterwards.
//! Both implementations drive the same [`SessionFactory`], so the versions
//! cannot drift into offering different sessions.

mod v1;
mod v2;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;

use agent_client_protocol::Agent;
use agent_client_protocol::ConnectTo;
use agent_client_protocol::Stdio;
use keke_config_types::ApprovalPolicy;
use keke_protocol::ReasoningEffort;
use keke_protocol::StopReason;
use keke_provider_api::ModelInfo;
use tokio::sync::mpsc::UnboundedReceiver;

use crate::AuthMethodDescriptor;
use crate::Conversation;
use crate::ConversationError;
use crate::ConversationFuture;
use crate::Opened;
use crate::PermissionAnswer;
use crate::SessionListing;

/// Makes a conversation for one ACP session.
///
/// A trait rather than a closure because the composition root is the only place
/// that knows how to build a session, and `keke-acp` must not learn.
pub trait SessionFactory: Send + Sync + 'static {
    /// Open a conversation rooted at `cwd`, as the client asked.
    fn open(&self, cwd: PathBuf) -> ConversationFuture<'_, Result<Opened, ConversationError>>;

    /// Every session there is to resume, newest first.
    ///
    /// `cwd` filters when the client asked it to. Listing is separate from
    /// opening because a client draws a picker before it has chosen anything,
    /// and building a session to describe one would start a turn nobody asked
    /// for.
    fn list(
        &self,
        cwd: Option<PathBuf>,
    ) -> ConversationFuture<'_, Result<Vec<SessionListing>, ConversationError>>;

    /// Reopen a previous session so it can be prompted again.
    ///
    /// The id is whatever the client sent back; resolving it — including
    /// deciding that it names nothing — belongs to whoever keeps the sessions.
    fn resume(
        &self,
        id: String,
        cwd: PathBuf,
    ) -> ConversationFuture<'_, Result<Opened, ConversationError>>;

    /// Authentication methods to offer before any session exists.
    ///
    /// Empty by default: a factory with no login flow to offer need not name
    /// one, and `initialize` must not advertise `authMethods` a client would
    /// then be entitled to call. Each descriptor carries whether that route
    /// already holds a credential, so a client can show what is signed in
    /// rather than offering every login as if none had happened.
    fn auth_methods(&self) -> Vec<AuthMethodDescriptor> {
        Vec::new()
    }

    /// Run one auth method's login flow.
    ///
    /// Only ever called with an id `auth_methods` advertised. `meta` is
    /// whatever the client attached to the request under ACP's own
    /// extensibility mechanism — a pasted API key, for the methods that take
    /// one — and is a client convenience, not a substitute for it: a method
    /// that needs a key and got none still resolves it the way `keke login`
    /// does (the environment, or a prior `keke login`), and fails only if
    /// neither produced one.
    ///
    /// `meta.force` asks for the login flow itself even when the route
    /// already resolves a credential — what a person clicking "sign in"
    /// again means, and the only way past a credential that is present but
    /// no longer good.
    ///
    /// The default implementation refuses everything, which is correct for a
    /// factory that also advertises no auth methods — keeping refusal paired
    /// with the lookup here is what stops the two from drifting apart.
    fn authenticate(
        &self,
        method_id: &str,
        meta: Option<serde_json::Map<String, serde_json::Value>>,
    ) -> ConversationFuture<'_, Result<(), ConversationError>> {
        let _ = meta;
        let error =
            ConversationError::Agent(format!("unknown authentication method `{method_id}`"));
        Box::pin(async move { Err(error) })
    }
}

/// Serve the ACP protocol on stdin and stdout until the client disconnects.
///
/// The version is the client's to choose. Serving only the newest would refuse
/// every client that exists today; serving only the oldest would make keke the
/// reason a client cannot use what it already implements.
pub async fn serve_stdio(
    factory: Arc<dyn SessionFactory>,
) -> Result<(), agent_client_protocol::Error> {
    Agent
        .protocol_router()
        .with_v1(v1::agent(Arc::clone(&factory)))
        .with_v2(v2::agent(factory))
        .connect_to(Stdio::new())
        .await
}

/// The identifiers the option ids are built from.
///
/// The ACP client sends back an option id, so these strings are the wire
/// contract for what a person chose.
const ALLOW: &str = "allow";
const ALLOW_ALWAYS: &str = "allow-always";
const DENY: &str = "deny";

/// An option id keke did not offer is a refusal, not a permission.
fn answer_for(option_id: &str) -> PermissionAnswer {
    match option_id {
        ALLOW => PermissionAnswer::Allow,
        ALLOW_ALWAYS => PermissionAnswer::AllowAlways,
        _ => PermissionAnswer::Deny,
    }
}

/// The config option ids keke offers. A client sends one back to say what it
/// changed, so these strings are the wire contract.
const MODEL: &str = "model";
const REASONING_EFFORT: &str = "reasoning_effort";
const APPROVAL_POLICY: &str = "approval_policy";

/// The value that means "no level; let the model decide".
///
/// A named option rather than an absent one, because unset is a state a person
/// must be able to get back to — see
/// [`ReasoningEffort`](keke_protocol::ReasoningEffort) on why it is not the
/// bottom rung.
const DEFAULT_EFFORT: &str = "default";

/// One config option, described in keke's own terms.
///
/// The two protocol versions declare separate types with the same names, so
/// what a client is offered is decided once here and rendered twice. Without
/// this the versions could quietly come to offer different things, which is
/// exactly the drift a client switching between them would report as a bug in
/// whichever it tried second.
struct Choice {
    id: &'static str,
    name: &'static str,
    current: String,
    /// `(value, label)`. The value is what comes back; the label is what a
    /// person reads.
    options: Vec<(String, String)>,
}

/// One live ACP session.
struct Entry {
    conversation: Arc<dyn Conversation>,
    /// What the provider serves. Fixed for the session's life: it is what the
    /// session was opened against, and a list that changed underneath a client
    /// would make a selection it just made invalid.
    models: Vec<ModelInfo>,
    /// What is selected now. Behind a lock because `session/set_config_option`
    /// changes it from the dispatch loop while the prompt handler reads it.
    selected: Mutex<Selected>,
    /// Fed by the pump when a turn ends, read by the prompt handler. Carried in
    /// keke's own terms rather than either wire's: v1 wants the reason as the
    /// response to `session/prompt` and v2 wants it on the update stream, and
    /// this must serve both.
    outcomes: tokio::sync::Mutex<UnboundedReceiver<StopReason>>,
}

#[derive(Clone, Debug)]
struct Selected {
    model: String,
    effort: Option<ReasoningEffort>,
    approval_policy: ApprovalPolicy,
}

impl Entry {
    fn selected(&self) -> Selected {
        self.selected
            .lock()
            .map(|selected| selected.clone())
            // A poisoned lock means another thread panicked mid-change. The
            // session is still answerable, and reporting the model as unset is
            // a smaller lie than refusing every later request.
            .unwrap_or_else(|_| Selected {
                model: String::new(),
                effort: None,
                approval_policy: ApprovalPolicy::default(),
            })
    }

    /// The levels the selected model takes, or nothing when it published none.
    fn offered_efforts(&self) -> Vec<ReasoningEffort> {
        let model = self.selected().model;
        self.models
            .iter()
            .find(|candidate| candidate.id == model)
            .map(|candidate| candidate.reasoning_efforts.clone())
            .unwrap_or_default()
    }
}

#[derive(Default)]
struct Sessions(Mutex<HashMap<String, Arc<Entry>>>);

impl Sessions {
    fn get(&self, id: &str) -> Option<Arc<Entry>> {
        self.0.lock().ok()?.get(id).cloned()
    }

    fn insert(&self, id: &str, entry: Arc<Entry>) {
        if let Ok(mut sessions) = self.0.lock() {
            sessions.insert(id.to_string(), entry);
        }
    }
}

/// Register an opened conversation, and describe what a client may change.
///
/// Shared by both versions so the two cannot disagree about what a session is.
fn enrol(
    sessions: &Sessions,
    opened: &Opened,
    outcomes: UnboundedReceiver<StopReason>,
) -> Arc<Entry> {
    let entry = Arc::new(Entry {
        conversation: Arc::clone(&opened.conversation),
        models: opened.models.clone(),
        selected: Mutex::new(Selected {
            model: opened.model.clone(),
            effort: opened.effort,
            approval_policy: opened.approval_policy,
        }),
        outcomes: tokio::sync::Mutex::new(outcomes),
    });
    sessions.insert(&opened.id, Arc::clone(&entry));
    entry
}

/// What a client may change about a session, and what it is set to now.
///
/// An empty list is the honest answer when the provider could not be asked what
/// it serves: offering a choice keke cannot honour would put the refusal after
/// the click rather than before it. The effort option appears only when the
/// selected model published a ladder — a menu of levels the endpoint will
/// reject is worse than no menu.
fn choices(entry: &Entry) -> Vec<Choice> {
    let selected = entry.selected();
    let mut choices = vec![Choice {
        id: APPROVAL_POLICY,
        name: "Approval mode",
        current: policy_value(selected.approval_policy).to_string(),
        options: [
            ApprovalPolicy::OnRequest,
            ApprovalPolicy::OnFailure,
            ApprovalPolicy::Never,
        ]
        .into_iter()
        .map(|policy| {
            (
                policy_value(policy).to_string(),
                policy_label(policy).to_string(),
            )
        })
        .collect(),
    }];
    if entry.models.is_empty() {
        return choices;
    }
    choices.push(Choice {
        id: MODEL,
        name: "Model",
        current: selected.model,
        options: entry
            .models
            .iter()
            // The display name is what the vendor calls it. Showing the slug
            // twice is what a client falls back to when keke says nothing, and
            // it is the difference between "GPT-5.6-Sol" and "gpt-5.6-sol" in
            // a menu someone has to read.
            .map(|model| (model.id.clone(), model.display_name.clone()))
            .collect(),
    });

    let offered = entry.offered_efforts();
    if !offered.is_empty() {
        let mut options = vec![(DEFAULT_EFFORT.to_string(), "Auto".to_string())];
        options.extend(
            offered
                .iter()
                .map(|effort| (effort.to_string(), effort_label(*effort))),
        );
        choices.push(Choice {
            id: REASONING_EFFORT,
            name: "Reasoning effort",
            current: selected
                .effort
                .map_or_else(|| DEFAULT_EFFORT.to_string(), |effort| effort.to_string()),
            options,
        });
    }
    choices
}

/// The wire value for one approval policy — `ApprovalPolicy::as_str`, the same
/// spelling the session log uses, so a client's picker and a resumed log never
/// disagree about what a mode is called.
fn policy_value(policy: ApprovalPolicy) -> &'static str {
    policy.as_str()
}

fn policy_label(policy: ApprovalPolicy) -> &'static str {
    match policy {
        ApprovalPolicy::OnRequest => "Ask for approval",
        ApprovalPolicy::OnFailure => "Ask on failure",
        ApprovalPolicy::Never => "Full access",
    }
}

/// How a level is written in a menu. Capitalised because it sits beside model
/// names in the same list, and `xhigh` reads as a typo next to `GPT-5.6-Sol`.
fn effort_label(effort: ReasoningEffort) -> String {
    match effort {
        ReasoningEffort::Low => "Low",
        ReasoningEffort::Medium => "Medium",
        ReasoningEffort::High => "High",
        ReasoningEffort::XHigh => "Extra high",
        ReasoningEffort::Max => "Maximum",
        ReasoningEffort::Ultra => "Ultra",
    }
    .to_string()
}

/// Apply one `session/set_config_option`, or say why it cannot be.
///
/// Invariant 8: an option keke was not offering is an error rather than a
/// silent no-op, which would leave the client showing a selection the session
/// does not have. Shared so the two versions cannot disagree about what a
/// client is allowed to ask for — only about how the refusal is spelled.
fn apply(entry: &Entry, config_id: &str, value: Option<String>) -> Result<Vec<Choice>, String> {
    match config_id {
        APPROVAL_POLICY => {
            let wanted = value.ok_or_else(|| "no approval mode named".to_string())?;
            let policy = ApprovalPolicy::parse(&wanted)
                .ok_or_else(|| "not an approval mode this session offers".to_string())?;
            entry.conversation.set_approval_policy(policy);
            if let Ok(mut selected) = entry.selected.lock() {
                selected.approval_policy = policy;
            }
            Ok(choices(entry))
        }
        MODEL => {
            let wanted = value.ok_or_else(|| "no model named".to_string())?;
            if !entry.models.iter().any(|model| model.id == wanted) {
                return Err("not a model this session offers".to_string());
            }
            entry.conversation.set_model(wanted.clone());
            if let Ok(mut selected) = entry.selected.lock() {
                selected.model = wanted;
            }
            // A level the newly selected model does not take would be sent
            // anyway and rejected on the next prompt, long after the change
            // that caused it. Dropping it here is why the client is handed the
            // whole option set back rather than just the one it changed.
            let offered = entry.offered_efforts();
            let stale = entry
                .selected()
                .effort
                .is_some_and(|effort| !offered.is_empty() && !offered.contains(&effort));
            if stale {
                entry.conversation.set_reasoning_effort(None);
                if let Ok(mut selected) = entry.selected.lock() {
                    selected.effort = None;
                }
            }
            Ok(choices(entry))
        }
        REASONING_EFFORT => {
            let wanted = value.ok_or_else(|| "no reasoning effort named".to_string())?;
            let effort = if wanted == DEFAULT_EFFORT {
                None
            } else {
                let parsed = ReasoningEffort::parse(&wanted)
                    .map_err(|_| "not a reasoning effort this session offers".to_string())?;
                if !entry.offered_efforts().contains(&parsed) {
                    return Err("not a reasoning effort this model offers".to_string());
                }
                Some(parsed)
            };
            entry.conversation.set_reasoning_effort(effort);
            if let Ok(mut selected) = entry.selected.lock() {
                selected.effort = effort;
            }
            Ok(choices(entry))
        }
        other => Err(format!("no config option `{other}`")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unrecognised_option_is_a_denial() {
        assert_eq!(answer_for("allow"), PermissionAnswer::Allow);
        assert_eq!(answer_for("allow-always"), PermissionAnswer::AllowAlways);
        assert_eq!(answer_for("deny"), PermissionAnswer::Deny);
        assert_eq!(answer_for("something-else"), PermissionAnswer::Deny);
    }

    fn served(id: &str, name: &str, efforts: &[ReasoningEffort]) -> ModelInfo {
        let mut model = ModelInfo::new(id);
        name.clone_into(&mut model.display_name);
        model.reasoning_efforts = efforts.to_vec();
        model
    }

    fn entry(models: Vec<ModelInfo>) -> (Entry, Arc<crate::ScriptedConversation>) {
        let (scripted, _updates) = crate::ScriptedConversation::new(Vec::new());
        let scripted = Arc::new(scripted);
        let model = models
            .first()
            .map(|model| model.id.clone())
            .unwrap_or_default();
        let (_tx, outcomes) = tokio::sync::mpsc::unbounded_channel();
        (
            Entry {
                conversation: Arc::clone(&scripted) as Arc<dyn Conversation>,
                models,
                selected: Mutex::new(Selected {
                    model,
                    effort: None,
                    approval_policy: ApprovalPolicy::default(),
                }),
                outcomes: tokio::sync::Mutex::new(outcomes),
            },
            scripted,
        )
    }

    fn option<'a>(choices: &'a [Choice], id: &str) -> Option<&'a Choice> {
        choices.iter().find(|choice| choice.id == id)
    }

    /// A client draws its picker from the labels. Sending the slug as the
    /// label is what makes a menu of `gpt-5.6-*` unreadable.
    #[test]
    fn the_model_option_is_labelled_with_what_the_vendor_calls_it() {
        let (entry, _scripted) = entry(vec![
            served("gpt-5.6-sol", "GPT-5.6-Sol", &[ReasoningEffort::Low]),
            served("gpt-5.2", "GPT-5.2", &[]),
        ]);
        let choices = choices(&entry);
        let models = option(&choices, MODEL).expect("a model option");

        assert_eq!(models.current, "gpt-5.6-sol");
        assert_eq!(
            models.options,
            vec![
                ("gpt-5.6-sol".to_string(), "GPT-5.6-Sol".to_string()),
                ("gpt-5.2".to_string(), "GPT-5.2".to_string()),
            ]
        );
    }

    /// The levels a model takes are a choice a client can draw, and until now
    /// keke told it about none of them.
    #[test]
    fn a_model_that_publishes_a_ladder_gets_an_effort_option() {
        let (entry, _scripted) = entry(vec![served(
            "gpt-5.6-sol",
            "GPT-5.6-Sol",
            &[ReasoningEffort::Low, ReasoningEffort::Ultra],
        )]);
        let choices = choices(&entry);
        let efforts = option(&choices, REASONING_EFFORT).expect("an effort option");

        assert_eq!(efforts.current, DEFAULT_EFFORT);
        let values: Vec<&str> = efforts
            .options
            .iter()
            .map(|(value, _)| value.as_str())
            .collect();
        assert_eq!(values, vec![DEFAULT_EFFORT, "low", "ultra"]);
    }

    /// A menu of levels the endpoint will reject is worse than no menu.
    #[test]
    fn a_model_with_no_ladder_is_offered_no_effort_option() {
        let (entry, _scripted) = entry(vec![served("grok-3-mini", "grok-3-mini", &[])]);
        assert!(option(&choices(&entry), REASONING_EFFORT).is_none());
    }

    /// Invariant 8: a selection keke was not offering is an error, not a
    /// silent no-op that leaves the client showing something untrue.
    #[test]
    fn a_model_this_session_does_not_offer_is_refused() {
        let (entry, scripted) = entry(vec![served("gpt-5.2", "GPT-5.2", &[])]);

        assert!(apply(&entry, MODEL, Some("gpt-4o".to_string())).is_err());
        assert!(apply(&entry, "colour-scheme", Some("dark".to_string())).is_err());
        assert_eq!(entry.selected().model, "gpt-5.2");
        assert!(scripted.models().is_empty());
    }

    #[test]
    fn a_level_this_model_does_not_offer_is_refused() {
        let (entry, scripted) = entry(vec![served(
            "gpt-5.2",
            "GPT-5.2",
            &[ReasoningEffort::Low, ReasoningEffort::High],
        )]);

        assert!(apply(&entry, REASONING_EFFORT, Some("ultra".to_string())).is_err());
        assert!(apply(&entry, REASONING_EFFORT, Some("hgih".to_string())).is_err());
        assert!(scripted.efforts().is_empty());

        apply(&entry, REASONING_EFFORT, Some("high".to_string())).expect("high is offered");
        assert_eq!(entry.selected().effort, Some(ReasoningEffort::High));
        assert_eq!(scripted.efforts(), vec![Some(ReasoningEffort::High)]);
    }

    /// Unset is a level a person must be able to get back to, so it is an
    /// option with a name rather than the absence of one.
    #[test]
    fn the_default_level_is_reachable_again() {
        let (entry, scripted) = entry(vec![served("gpt-5.2", "GPT-5.2", &[ReasoningEffort::High])]);
        apply(&entry, REASONING_EFFORT, Some("high".to_string())).expect("offered");
        apply(&entry, REASONING_EFFORT, Some(DEFAULT_EFFORT.to_string())).expect("unset");

        assert_eq!(entry.selected().effort, None);
        assert_eq!(scripted.efforts(), vec![Some(ReasoningEffort::High), None]);
    }

    /// Carrying a level onto a model that does not take it would fail the next
    /// prompt, long after the change that caused it.
    #[test]
    fn switching_models_drops_a_level_the_new_one_does_not_take() {
        let (entry, scripted) = entry(vec![
            served("gpt-5.6-sol", "GPT-5.6-Sol", &[ReasoningEffort::Ultra]),
            served("gpt-5.2", "GPT-5.2", &[ReasoningEffort::High]),
        ]);
        apply(&entry, REASONING_EFFORT, Some("ultra".to_string())).expect("offered");

        let choices = apply(&entry, MODEL, Some("gpt-5.2".to_string())).expect("offered");

        assert_eq!(entry.selected().effort, None);
        assert_eq!(
            option(&choices, REASONING_EFFORT).map(|choice| choice.current.as_str()),
            Some(DEFAULT_EFFORT),
            "the client is handed the whole option set back so its picker follows"
        );
        assert_eq!(scripted.efforts(), vec![Some(ReasoningEffort::Ultra), None]);
    }

    /// Offering a model choice keke cannot honour puts the refusal after the
    /// click rather than before it — but the approval mode is not the
    /// provider's to serve, so it is still offered.
    #[test]
    fn a_provider_that_could_not_be_asked_offers_no_model_choice() {
        let (entry, _scripted) = entry(Vec::new());
        let choices = choices(&entry);
        assert!(option(&choices, MODEL).is_none());
        assert!(option(&choices, APPROVAL_POLICY).is_some());
    }
}
