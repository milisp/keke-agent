//! Commands a person types instead of sending to the model.
//!
//! Two kinds share one namespace: the ones the surface carries out itself
//! (`/help`, `/effort`), and the ones a plugin contributes as a prompt file. They
//! share it because a person typing `/` wants one list, not two — but they stay
//! distinct in the type, because a plugin must never be able to redefine what
//! `/quit` does.
//!
//! A plugin's prompt files arrive as two kinds. A *command* is one a person was
//! always meant to type. A *skill* is written for the model, and it is here too
//! because a skill that only the model can reach is one a person cannot try:
//! they read its one-line description in the menu and have no way to run it. So
//! a skill is offered under its own name as well, and which kind a row is stays
//! on screen — running someone else's procedure should not be a surprise.

use std::collections::BTreeMap;
use std::path::PathBuf;

use keke_config_types::ApprovalPolicy;
use keke_config_types::ReasoningEffort;
use keke_config_types::ServiceTier;

/// What running a command does.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SlashAction {
    /// The surface carries it out. Never contributed by a plugin.
    Builtin(Builtin),
    /// A plugin-contributed prompt file; its body is sent as the prompt, with
    /// whatever the person typed after the name appended.
    Prompt { path: PathBuf, kind: PromptKind },
}

/// Which kind of prompt file a row runs, for the badge beside it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PromptKind {
    /// `commands/<name>.md`: written to be typed.
    Command,
    /// `skills/<name>/SKILL.md`: written for the model, offered here too.
    Skill,
}

impl PromptKind {
    /// How the kind is written in the menu, beside the plugin that brought it.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Command => "command",
            Self::Skill => "skill",
        }
    }
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
    /// Turns fast mode on and off, or sets the queue named as an argument.
    Fast,
    /// Opens the model picker, or switches straight to the model named as an
    /// argument.
    Model,
    /// Opens the provider picker, or points the next session straight at the
    /// route named as an argument.
    Provider,
    /// Puts the last reply on the system clipboard.
    Copy,
    /// Writes the messages so far to the file named as an argument.
    Export,
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
    /// The plugin that brought it, for the badge. `None` for a builtin.
    pub plugin: Option<String>,
}

impl SlashCommand {
    /// A command file a plugin ships. `plugin` is kept so the name can be
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
            kind: PromptKind::Command,
        }
    }

    /// A skill a plugin ships, offered under its own name so a person can run
    /// the procedure the model was told about.
    #[must_use]
    pub fn skill(
        plugin: impl Into<String>,
        name: impl Into<String>,
        description: impl Into<String>,
        path: impl Into<PathBuf>,
    ) -> PluginCommand {
        PluginCommand {
            kind: PromptKind::Skill,
            ..Self::prompt(plugin, name, description, path)
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
    pub kind: PromptKind,
}

/// The command list a person completes against.
#[derive(Debug, Default)]
pub struct SlashCommands {
    entries: Vec<SlashCommand>,
}

impl SlashCommands {
    /// Builtins plus whatever the plugins contribute.
    ///
    /// Skills and commands share this namespace with the builtins, so a
    /// skill named `review` and a command named `review` contest the bare name
    /// the same way two commands would.
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
                action: SlashAction::Prompt {
                    path: command.path.clone(),
                    kind: command.kind,
                },
                plugin: Some(command.plugin.clone()),
            });
        }
        entries.sort_by(|left, right| {
            left.plugin
                .is_some()
                .cmp(&right.plugin.is_some())
                .then_with(|| left.name.cmp(&right.name))
        });
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
            Builtin::Fast,
            "fast",
            "toggle fast mode, or name a queue: fast, flex, off",
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
            Builtin::Export,
            "export",
            "write the messages so far to a file — `/export <path>`",
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
        plugin: None,
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

/// The three approval policies, strictest first: the order the plan panel
/// lists them in, so the row a person lands on without reading is the one
/// that asks the most rather than the one that asks the least.
pub const POLICIES: [ApprovalPolicy; 3] = [
    ApprovalPolicy::OnRequest,
    ApprovalPolicy::OnFailure,
    ApprovalPolicy::Never,
];

/// What a policy says beside its name, since the name alone does not say what
/// a person is agreeing to let happen while the plan is carried out.
#[must_use]
pub fn policy_detail(policy: ApprovalPolicy) -> &'static str {
    match policy {
        ApprovalPolicy::OnRequest => "ask before each command",
        ApprovalPolicy::OnFailure => "ask only when a command fails",
        ApprovalPolicy::Never => "never ask \u{2014} run everything",
    }
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

/// Read the argument to `/fast`. `Ok(None)` means "no argument, toggle".
///
/// `off` is spelled out rather than inferred from an empty argument, so a
/// person who means "stop paying for the fast queue" can say it outright
/// instead of tapping the toggle and hoping it landed the right way.
///
/// A queue nobody recognizes is an error rather than a fallback: a typo that
/// quietly bought a different speed is invisible until the bill.
pub fn service_tier(argument: &str) -> Result<Option<Option<ServiceTier>>, String> {
    match argument.trim() {
        "" => Ok(None),
        "off" | "default" | "unset" | "standard" => Ok(Some(None)),
        other => ServiceTier::parse(other)
            .map(|tier| Some(Some(tier)))
            .ok_or_else(|| format!("no such queue: `{other}` — try fast, flex, or off")),
    }
}

/// How the queue reads in the bar and in `/help`.
#[must_use]
pub fn tier_name(tier: Option<ServiceTier>) -> &'static str {
    match tier {
        None => "standard",
        Some(tier) => tier.as_str(),
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

    fn contributed_skill(plugin: &str, name: &str) -> PluginCommand {
        SlashCommand::skill(
            plugin,
            name,
            "does a thing",
            format!("/tmp/{plugin}/{name}/SKILL.md"),
        )
    }

    /// A skill only the model can reach is one a person cannot try.
    #[test]
    fn a_skill_is_offered_under_its_own_name() {
        let commands = SlashCommands::new(vec![contributed_skill("acme", "review")]);
        let entry = commands.find("review").expect("the skill is offered");
        assert_eq!(entry.plugin.as_deref(), Some("acme"));
        assert!(matches!(
            entry.action,
            SlashAction::Prompt {
                kind: PromptKind::Skill,
                ..
            }
        ));
    }

    /// Skills share the one namespace, so a skill cannot quietly take a name a
    /// command already claims — or the other way round.
    #[test]
    fn a_skill_and_a_command_claiming_one_name_are_both_qualified() {
        let commands = SlashCommands::new(vec![
            contributed("reviewer", "review"),
            contributed_skill("acme", "review"),
        ]);
        assert!(commands.find("review").is_none());
        assert!(commands.find("reviewer:review").is_some());
        assert!(commands.find("acme:review").is_some());
    }

    #[test]
    fn a_skill_cannot_take_a_builtin_name() {
        let commands = SlashCommands::new(vec![contributed_skill("sneaky", "quit")]);
        assert_eq!(
            commands.find("quit").map(|entry| entry.action.clone()),
            Some(SlashAction::Builtin(Builtin::Quit))
        );
        assert!(commands.find("sneaky:quit").is_some());
    }

    /// Builtins come first so the commands the surface itself guarantees are
    /// never pushed down the list by an alphabetically-earlier plugin name.
    #[test]
    fn builtins_sort_before_plugin_commands() {
        let commands = SlashCommands::new(vec![contributed("aaa", "aaa")]);
        let names: Vec<&str> = commands
            .entries()
            .iter()
            .map(|entry| entry.name.as_str())
            .collect();
        assert_eq!(names.last(), Some(&"aaa"));
        assert!(commands.entries()[..names.len() - 1]
            .iter()
            .all(|entry| entry.plugin.is_none()));
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
