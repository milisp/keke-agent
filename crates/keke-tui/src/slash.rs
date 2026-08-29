//! Commands a person types instead of sending to the model.
//!
//! Two kinds share one namespace: the ones the surface carries out itself
//! (`/help`, `/effort`), and the ones a plugin contributes as a prompt file. They
//! share it because a person typing `/` wants one list, not two — but they stay
//! distinct in the type, because a plugin must never be able to redefine what
//! `/quit` does.

use std::collections::BTreeMap;
use std::path::PathBuf;

use keke_config_types::ApprovalPolicy;
use keke_config_types::ReasoningEffort;

/// What running a command does.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SlashAction {
    /// The surface carries it out. Never contributed by a plugin.
    Builtin(Builtin),
    /// A plugin-contributed prompt file; its body is sent as the prompt, with
    /// whatever the person typed after the name appended.
    Prompt(PathBuf),
}

/// The commands the surface implements itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Builtin {
    Help,
    Clear,
    /// Retires the conversation the agent is holding and starts a fresh one:
    /// history and usage go to zero, not just what is drawn on screen.
    New,
    Quit,
    /// Cycles the reasoning effort, or sets the level named as an argument.
    Effort,
    /// Opens the model picker, or switches straight to the model named as an
    /// argument.
    Model,
    /// Opens the provider picker, or points the next session straight at the
    /// route named as an argument.
    Provider,
    /// Puts the last reply on the system clipboard.
    Copy,
    /// Lists the MCP servers and their state, and signs in to a remote one.
    Mcp,
    /// Asks the session to plan first, optionally with the prompt to plan
    /// about in the same breath.
    Plan,
    /// Reopens the last plan this session saw, as a record of what was
    /// decided rather than a question to answer again.
    ViewPlan,
}

/// One entry in the command list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SlashCommand {
    /// What a person types after the `/`.
    pub name: String,
    pub description: String,
    pub action: SlashAction,
}

impl SlashCommand {
    /// A prompt file a plugin ships. `plugin` is kept so the name can be
    /// qualified if another plugin contributes the same one.
    #[must_use]
    pub fn prompt(
        plugin: impl Into<String>,
        name: impl Into<String>,
        description: impl Into<String>,
        path: impl Into<PathBuf>,
    ) -> PluginCommand {
        PluginCommand {
            plugin: plugin.into(),
            name: name.into(),
            description: description.into(),
            path: path.into(),
        }
    }
}

/// A plugin's contribution, before it is given a name in the shared namespace.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PluginCommand {
    pub plugin: String,
    pub name: String,
    pub description: String,
    pub path: PathBuf,
}

/// The command list a person completes against.
#[derive(Debug, Default)]
pub struct SlashCommands {
    entries: Vec<SlashCommand>,
}

impl SlashCommands {
    /// Builtins plus whatever the plugins contribute.
    ///
    /// A bare name is given to a plugin command only when it is the sole
    /// claimant; two plugins contributing `review` both become
    /// `plugin:review`, and neither gets `/review`. Silently picking one would
    /// make which plugin ran depend on discovery order — the same ambiguity the
    /// engine refuses for provider routes.
    #[must_use]
    pub fn new(plugins: Vec<PluginCommand>) -> Self {
        let mut entries = builtins();

        let mut claims: BTreeMap<&str, usize> = BTreeMap::new();
        for command in &plugins {
            *claims.entry(command.name.as_str()).or_default() += 1;
        }
        let taken: Vec<String> = entries.iter().map(|entry| entry.name.clone()).collect();

        for command in &plugins {
            let contested = claims.get(command.name.as_str()).copied().unwrap_or(0) > 1
                || taken.contains(&command.name);
            let name = if contested {
                format!("{}:{}", command.plugin, command.name)
            } else {
                command.name.clone()
            };
            entries.push(SlashCommand {
                name,
                description: command.description.clone(),
                action: SlashAction::Prompt(command.path.clone()),
            });
        }
        entries.sort_by(|left, right| left.name.cmp(&right.name));
        Self { entries }
    }

    #[must_use]
    pub fn entries(&self) -> &[SlashCommand] {
        &self.entries
    }

    #[must_use]
    pub fn find(&self, name: &str) -> Option<&SlashCommand> {
        self.entries.iter().find(|entry| entry.name == name)
    }

    /// Everything a person could mean by what they have typed so far.
    #[must_use]
    pub fn matching(&self, prefix: &str) -> Vec<&SlashCommand> {
        self.entries
            .iter()
            .filter(|entry| entry.name.starts_with(prefix))
            .collect()
    }
}

fn builtins() -> Vec<SlashCommand> {
    [
        (Builtin::Help, "help", "list the commands"),
        (Builtin::Clear, "clear", "clear the transcript on screen"),
        (
            Builtin::New,
            "new",
            "start a new session — history, usage, and the transcript all reset",
        ),
        (
            Builtin::Effort,
            "effort",
            "cycle the reasoning effort, or name one: low, medium, high, xhigh, max, ultra, default",
        ),
        (
            Builtin::Model,
            "model",
            "pick a model from this provider, or name one to switch to it",
        ),
        (
            Builtin::Provider,
            "provider",
            "pick which registered provider serves the next session, or name one",
        ),
        (
            Builtin::Copy,
            "copy",
            "copy the last reply to the clipboard",
        ),
        (
            Builtin::Mcp,
            "mcp",
            "manage the MCP servers, or `login <name>` to authorize a remote one",
        ),
        (
            Builtin::Plan,
            "plan",
            "plan before building — `/plan <what to do>` starts the turn too",
        ),
        (
            Builtin::ViewPlan,
            "view-plan",
            "reopen the last plan, as a record",
        ),
        (Builtin::ViewPlan, "show-plan", "alias for /view-plan"),
        (Builtin::ViewPlan, "plan-view", "alias for /view-plan"),
        (Builtin::Quit, "quit", "leave keke"),
    ]
    .into_iter()
    .map(|(builtin, name, description)| SlashCommand {
        name: name.to_string(),
        description: description.to_string(),
        action: SlashAction::Builtin(builtin),
    })
    .collect()
}

/// Split `/name rest` into the name and whatever follows it.
///
/// Returns `None` for anything that is not a command, so a prompt that happens
/// to begin with a path — `/usr/bin/env is missing` — is still a prompt.
#[must_use]
pub fn parse(text: &str) -> Option<(&str, &str)> {
    let rest = text.strip_prefix('/')?;
    let (name, arguments) = match rest.find(char::is_whitespace) {
        Some(at) => (&rest[..at], rest[at..].trim_start()),
        None => (rest, ""),
    };
    if name.is_empty() || !name.chars().all(is_name_char) {
        return None;
    }
    Some((name, arguments))
}

fn is_name_char(ch: char) -> bool {
    ch.is_alphanumeric() || matches!(ch, '-' | '_' | ':')
}

/// How an approval policy is written wherever a person sees or types one.
#[must_use]
pub fn policy_name(policy: ApprovalPolicy) -> &'static str {
    match policy {
        ApprovalPolicy::OnRequest => "on-request",
        ApprovalPolicy::OnFailure => "on-failure",
        ApprovalPolicy::Never => "never",
    }
}

/// How an effort level is written wherever a person sees or types one.
///
/// Unset has a name of its own — the model's own default is not the bottom
/// rung, and a person must be able to say either.
#[must_use]
pub fn effort_name(effort: Option<ReasoningEffort>) -> &'static str {
    match effort {
        None => "default",
        Some(level) => level.as_str(),
    }
}

/// Read the argument to `/effort`. `Ok(None)` means "no argument, cycle".
///
/// A level nobody recognizes is an error rather than a fallback: a typo that
/// quietly bought less thinking is invisible until the answers are worse.
pub fn effort(argument: &str) -> Result<Option<Option<ReasoningEffort>>, String> {
    match argument.trim() {
        "" => Ok(None),
        "default" | "unset" => Ok(Some(None)),
        other => ReasoningEffort::parse(other).map(|level| Some(Some(level))),
    }
}

/// Every rung there is, weakest first. The ladder a person cycles through when
/// the model has not said which of them it takes.
pub const LADDER: [ReasoningEffort; 6] = [
    ReasoningEffort::Low,
    ReasoningEffort::Medium,
    ReasoningEffort::High,
    ReasoningEffort::XHigh,
    ReasoningEffort::Max,
    ReasoningEffort::Ultra,
];

/// The next level after this one, wrapping past the top back to the default.
///
/// Unset enters the ladder at its bottom rung, so tapping through visits every
/// level and returns to leaving the choice to the model.
///
/// `offered` is what the current model actually takes. Cycling through levels a
/// model does not accept is a tour of requests that will be rejected, so when
/// the vendor published a ladder that is the one a person walks; an empty
/// `offered` means nobody said, and then every rung is on the table.
#[must_use]
pub fn next_effort(
    effort: Option<ReasoningEffort>,
    offered: &[ReasoningEffort],
) -> Option<ReasoningEffort> {
    let rungs: &[ReasoningEffort] = if offered.is_empty() { &LADDER } else { offered };
    match effort {
        // Unset is a rung of its own at the bottom, so tapping through visits
        // every level and returns to leaving the choice to the model.
        None => rungs.first().copied(),
        Some(current) => rungs
            .iter()
            .position(|rung| *rung == current)
            .and_then(|at| rungs.get(at + 1))
            .copied(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn contributed(plugin: &str, name: &str) -> PluginCommand {
        SlashCommand::prompt(
            plugin,
            name,
            "does a thing",
            format!("/tmp/{plugin}/{name}.md"),
        )
    }

    #[test]
    fn a_sole_claimant_keeps_the_bare_name() {
        let commands = SlashCommands::new(vec![contributed("reviewer", "review")]);
        assert!(commands.find("review").is_some());
    }

    /// Invariant 8: ambiguity fails loud. Neither plugin wins `/review`.
    #[test]
    fn two_plugins_claiming_one_name_are_both_qualified() {
        let commands = SlashCommands::new(vec![
            contributed("reviewer", "review"),
            contributed("auditor", "review"),
        ]);
        assert!(commands.find("review").is_none());
        assert!(commands.find("reviewer:review").is_some());
        assert!(commands.find("auditor:review").is_some());
    }

    /// A plugin must not be able to change what a builtin does.
    #[test]
    fn a_plugin_cannot_take_a_builtin_name() {
        let commands = SlashCommands::new(vec![contributed("sneaky", "quit")]);
        assert_eq!(
            commands.find("quit").map(|entry| entry.action.clone()),
            Some(SlashAction::Builtin(Builtin::Quit))
        );
        assert!(commands.find("sneaky:quit").is_some());
    }

    #[test]
    fn matching_completes_on_the_prefix() {
        let commands = SlashCommands::new(Vec::new());
        let names: Vec<&str> = commands
            .matching("q")
            .iter()
            .map(|entry| entry.name.as_str())
            .collect();
        assert_eq!(names, vec!["quit"]);
    }

    #[test]
    fn an_unknown_effort_is_refused_rather_than_defaulted() {
        assert!(effort("hgih").is_err());
        assert_eq!(effort(""), Ok(None));
        assert_eq!(effort("xhigh"), Ok(Some(Some(ReasoningEffort::XHigh))));
        assert_eq!(effort("default"), Ok(Some(None)));
    }

    /// Unset is a rung a person can get back to, not a state they leave once.
    #[test]
    fn cycling_the_effort_returns_to_the_default() {
        let mut level = None;
        for _ in 0..LADDER.len() {
            level = next_effort(level, &[]);
        }
        assert_eq!(level, Some(ReasoningEffort::Ultra));
        assert_eq!(next_effort(level, &[]), None);
    }

    /// Cycling through levels the model does not take is a tour of requests
    /// that will be rejected.
    #[test]
    fn cycling_walks_the_ladder_the_model_published() {
        let offered = [ReasoningEffort::Low, ReasoningEffort::High];
        assert_eq!(next_effort(None, &offered), Some(ReasoningEffort::Low));
        assert_eq!(
            next_effort(Some(ReasoningEffort::Low), &offered),
            Some(ReasoningEffort::High)
        );
        assert_eq!(next_effort(Some(ReasoningEffort::High), &offered), None);
    }

    /// A level left over from another model is not one to keep cycling from:
    /// the next tap leaves it rather than walking on from a rung that is not
    /// on this ladder.
    #[test]
    fn a_level_this_model_does_not_offer_cycles_back_to_unset() {
        let offered = [ReasoningEffort::Low, ReasoningEffort::High];
        assert_eq!(next_effort(Some(ReasoningEffort::Max), &offered), None);
    }

    #[test]
    fn a_path_is_not_a_command() {
        assert_eq!(parse("/usr/bin/env is missing"), None);
        assert_eq!(parse("/effort high"), Some(("effort", "high")));
        assert_eq!(parse("/help"), Some(("help", "")));
        assert_eq!(parse("hello"), None);
    }
}
