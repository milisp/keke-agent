//! Approval policy, reasoning effort, model, and provider switching.
//!
//! These share `persist_override`: every switch here is also a write to
//! `config.toml`, so the choice outlives this process.

use keke_config_types::ApprovalPolicy;
use keke_config_types::ReasoningEffort;
use keke_config_types::SessionMode;

use crate::transcript::Cell;

use super::App;

impl App {
    /// Cycle the session's strictness: the shift-tab gesture.
    ///
    /// One ladder, not two toggles. Plan mode and the approval policy both
    /// answer "how much may the agent do without me", and a person tapping
    /// through two independent switches has to work out how they stack —
    /// so they are laid out as a single ordering, loosest last:
    /// `plan → on-request → on-failure → never → plan`. Plan mode is the
    /// tightest rung because it refuses edits outright rather than offering
    /// them for approval, so entering it also brings the policy back to
    /// `on-request`: a rung must mean one thing, not one thing plus whatever
    /// was underneath it.
    ///
    /// Silent by design. The gesture is meant to be tapped through while
    /// looking at the status bar, and a line per tap would push the
    /// conversation off screen to say what the bar is already saying.
    pub fn cycle_session_rung(&mut self) {
        if self.mode.is_plan() {
            self.request_session_mode(SessionMode::Default);
            self.set_approval_policy_aloud(ApprovalPolicy::OnRequest);
            return;
        }
        match self.approval {
            ApprovalPolicy::OnRequest => self.set_approval_policy_aloud(ApprovalPolicy::OnFailure),
            ApprovalPolicy::OnFailure => self.set_approval_policy_aloud(ApprovalPolicy::Never),
            ApprovalPolicy::Never => {
                self.request_session_mode(SessionMode::Plan);
                self.set_approval_policy_aloud(ApprovalPolicy::OnRequest);
            }
        }
    }

    /// Ask the session to plan, or to stop planning.
    ///
    /// Nothing is stored here and nothing is written to `config.toml`. What the
    /// surface draws comes back over [`keke_acp::Update::ModeChanged`], because
    /// the agent enters and leaves plan mode on its own and a locally-set flag
    /// would go stale the first time it did. Nor is the mode persisted: it is
    /// an answer about the work in front of a person, and a session that came
    /// back planning weeks later would be answering a question nobody asked.
    pub fn request_session_mode(&mut self, mode: SessionMode) {
        self.conversation.set_session_mode(mode);
    }

    /// Set the policy and remember it past this process.
    pub(super) fn set_approval_policy_aloud(&mut self, policy: ApprovalPolicy) {
        self.set_approval_policy(policy);
        self.persist_override(|file| {
            file.approval_policy = Some(policy);
        });
    }

    /// Write one field of `$KEKE_HOME/config.toml`, so the switch a person
    /// just made outlives this process instead of reverting on the next
    /// launch. Best-effort: a write that fails (read-only home, no disk) is
    /// logged rather than surfaced, since the switch already took effect for
    /// this session and a transcript error over a convenience write would be
    /// out of proportion.
    fn persist_override(&self, patch: impl FnOnce(&mut keke_config::ConfigFile)) {
        let Some(home) = &self.config_home else {
            return;
        };
        if let Err(error) = keke_config::persist_user_override(home, patch) {
            tracing::warn!(%error, "could not persist the switch to config.toml");
        }
    }

    pub fn set_approval_policy(&mut self, policy: ApprovalPolicy) {
        self.approval = policy;
        self.conversation.set_approval_policy(policy);
    }

    #[must_use]
    pub fn reasoning_effort(&self) -> Option<ReasoningEffort> {
        self.effort
    }

    pub fn set_reasoning_effort(&mut self, effort: Option<ReasoningEffort>) {
        self.effort = effort;
        self.conversation.set_reasoning_effort(effort);
    }

    /// Set the level, which is what a typed `/effort` does. Silent in the
    /// transcript: the input box already shows what was typed.
    pub(super) fn set_reasoning_effort_aloud(&mut self, effort: Option<ReasoningEffort>) {
        self.set_reasoning_effort(effort);
        self.persist_override(|file| {
            file.reasoning_effort = effort.map(|level| level.as_str().to_string());
        });
    }

    /// Which model is answering, for the status bar.
    #[must_use]
    pub fn model(&self) -> &str {
        &self.model
    }

    /// The directory the session was launched from, for the header bar.
    #[must_use]
    pub fn cwd(&self) -> &std::path::Path {
        self.file_search.root()
    }

    /// The context window of the model in force, when its provider said.
    #[must_use]
    pub fn context_window(&self) -> Option<u64> {
        self.models
            .iter()
            .find(|model| model.id == self.model)
            .and_then(|model| model.context_window)
    }

    /// What this session's provider serves.
    #[must_use]
    pub fn models(&self) -> &[keke_provider_api::ModelInfo] {
        &self.models
    }

    /// The levels the current model takes, or nothing when it did not say.
    ///
    /// Empty is not "no reasoning": it is "the vendor published no ladder", and
    /// the difference matters because the first would hide `/effort` and the
    /// second must leave every rung available.
    pub(super) fn offered_efforts(&self) -> Vec<ReasoningEffort> {
        self.models
            .iter()
            .find(|model| model.id == self.model)
            .map(|model| model.reasoning_efforts.clone())
            .unwrap_or_default()
    }

    /// Switch models, or say why not.
    ///
    /// A model the provider does not serve is refused rather than sent: the
    /// rejection would otherwise land on the next prompt, long after the
    /// command that caused it. When the provider could not be asked at all the
    /// list is empty and nothing is refused — keke has no grounds to.
    pub(super) fn set_model_aloud(&mut self, wanted: &str) {
        if !self.models.is_empty() && !self.models.iter().any(|model| model.id == wanted) {
            self.transcript.push(Cell::Error(format!(
                "no model {wanted:?} on this provider — /model lists them"
            )));
            return;
        }
        self.model = wanted.to_string();
        self.conversation.set_model(wanted.to_string());
        // The pair or nothing: a model written under the previous launch's
        // provider is a combination no run ever used, and it fails on the next
        // bare `keke`.
        if let Some(provider) = &self.provider {
            let provider = provider.clone();
            self.persist_override(move |file| {
                file.provider = Some(provider);
                file.model = Some(wanted.to_string());
            });
        }

        // A level the new model does not take would be sent anyway and
        // rejected, so it is dropped here where the cause is still on screen.
        let offered = self.offered_efforts();
        if let Some(level) = self.effort
            && !offered.is_empty()
            && !offered.contains(&level)
        {
            self.set_reasoning_effort(None);
            self.transcript.push(Cell::Notice(format!(
                "{wanted} does not take {level} — reasoning effort is back to the model's default"
            )));
        }
    }

    /// Which provider route is in force, for the status bar and the overlay's
    /// current-row mark.
    #[must_use]
    pub fn provider(&self) -> Option<&str> {
        self.provider.as_deref()
    }

    /// Point the next session at another provider instance, or say why not.
    ///
    /// A route nothing is registered under is refused rather than written, for
    /// the same reason `/model` refuses a model the provider does not serve: a
    /// name that only fails on the next launch fails long after the command
    /// that caused it.
    ///
    /// The running conversation keeps the provider it was built with — a
    /// session's route is settled when its provider is handed to it, and
    /// re-pointing one mid-turn would leave the transcript half-answered by
    /// each. So this records the choice and says plainly when it takes effect,
    /// rather than pretending a switch that did not happen.
    pub(super) fn set_provider_aloud(&mut self, wanted: &str) {
        if !self.routes.is_empty() && !self.routes.iter().any(|route| route.route == wanted) {
            self.transcript.push(Cell::Error(format!(
                "no provider {wanted:?} on this build — /provider lists them"
            )));
            return;
        }
        if self.provider.as_deref() == Some(wanted) {
            self.transcript
                .push(Cell::Notice(format!("already on provider {wanted}")));
            return;
        }
        let previous = self.provider.replace(wanted.to_string());
        // A model id belongs to the provider that serves it, so one carried
        // across is a pair no run ever used. The list goes with it: what this
        // session knows is what the *old* route published, and keeping it would
        // have `/model` refuse names the new route does serve.
        self.model.clear();
        self.models.clear();
        let route = wanted.to_string();
        self.persist_override(move |file| {
            file.provider = Some(route);
            file.model = None;
        });

        let mut notice = format!("provider is now {wanted}");
        if let Some(previous) = previous {
            notice.push_str(&format!(
                " — this session keeps talking to {previous}; restart keke to use it"
            ));
        }
        notice.push_str(
            ".\nThe model is unset, since an id from the old provider need not exist on this one.",
        );
        self.transcript.push(Cell::Notice(notice));
    }
    /// What `/model` says when there is no list to open.
    pub(super) fn model_list(&self) -> String {
        if self.models.is_empty() {
            return format!(
                "model: {}\n\nThis provider published no model list, so there is nothing to \n\
                 choose between here. `/model <id>` still switches to whatever you name.",
                if self.model.is_empty() {
                    "(unset)"
                } else {
                    &self.model
                }
            );
        }
        let mut text = String::from("models:");
        for model in &self.models {
            let current = if model.id == self.model { "*" } else { " " };
            text.push_str(&format!(
                "\n {current} {} ({})",
                model.display_name, model.id
            ));
            if let Some(window) = model.context_window {
                text.push_str(&format!("  ·  {}k context", window / 1_000));
            }
            if model.supports_reasoning() {
                let levels: Vec<&str> = model
                    .reasoning_efforts
                    .iter()
                    .map(|effort| effort.as_str())
                    .collect();
                text.push_str(&format!("  ·  effort: {}", levels.join(", ")));
            }
            if let Some(description) = &model.description {
                text.push_str(&format!("\n      {description}"));
            }
        }
        text.push_str("\n\n/model <id> switches; /effort sets how hard it thinks.");
        text
    }

    /// What `/provider` says when there is no list to open.
    pub(super) fn provider_list(&self) -> String {
        format!(
            "provider: {}\n\nThis session was not told which providers are registered, so there \n\
             is nothing to choose between here. `/provider <name>` still points the next \n\
             session at whatever you name.",
            self.provider.as_deref().unwrap_or("(unset)")
        )
    }
}
