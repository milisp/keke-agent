//! The interactive surface: opening it, and resuming into it.

use std::sync::Arc;

use anyhow::Context as _;
use anyhow::Result;
use anyhow::bail;
use keke_config::Config;
use keke_paths::AbsPath;

use super::models_for;
use super::provider_choices;
use super::session_builder;
use super::slash_commands;
use crate::cli::ResumeArgs;
use crate::compose::Composed;

/// Reopen a previous session, or list what there is to reopen.
///
/// The history comes from the rollout log and nowhere else: what keke can
/// replay is what keke can continue, so there is no second record for the two
/// to disagree about.
pub(super) async fn resume(
    args: ResumeArgs,
    mut config: Config,
    composed: Composed,
    cwd: std::path::PathBuf,
    approvals: Arc<keke_acp::Approvals>,
    requests: keke_acp::ApprovalRequests,
    model_explicit: bool,
    effort_explicit: bool,
) -> Result<()> {
    let home = &config.home.home;
    let sessions = keke_core::list_sessions(home)?;
    let cwd_str = cwd.display().to_string();
    // A log with no turns is an interface someone opened and closed; listing
    // them buries the conversations under the empty files. Scoped to the
    // current directory unless `--all` asks to see every project's sessions.
    let conversations: Vec<_> = sessions
        .iter()
        .filter(|session| args.all || session.turns > 0)
        .filter(|session| args.all || session.cwd.as_deref() == Some(cwd_str.as_str()))
        .collect();

    if args.list {
        if conversations.is_empty() {
            println!(
                "no sessions under {} for {cwd_str}",
                keke_core::sessions_dir(home).display()
            );
            return Ok(());
        }
        // Wide enough that every row names one session and no wider. What is
        // printed is what `keke resume` takes back, so a listing that printed
        // the same id on two rows would be inviting a person to type something
        // that cannot resolve.
        let width = keke_core::abbreviation(conversations.iter().map(|it| it.id));
        println!(
            "{:<width$} {:<20} {:>5}  STARTED WITH",
            "ID", "UPDATED", "TURNS"
        );
        for session in conversations {
            println!(
                "{:<width$} {:<20} {:>5}  {}",
                session.abbreviated(width),
                session.updated_at.get(..19).unwrap_or("-"),
                session.turns,
                session.summary
            );
        }
        println!(
            "\nresume one with `keke resume <id>`, or the last one here with `keke resume --last`"
        );
        return Ok(());
    }

    let id = match &args.session {
        // Any prefix of an id, because nobody retypes a UUID: `--list` prints
        // the short form and this takes it back.
        Some(typed) => match keke_core::find_session(home, typed)? {
            keke_core::SessionMatch::One(session) => session.id,
            // Invariant 8: two claimants and no way to choose is an error, not
            // a pick — continuing the wrong conversation is silent and costly.
            keke_core::SessionMatch::Ambiguous(candidates) => {
                // Printed long enough to tell the candidates apart, which is
                // the whole use of this message: naming them all the same way
                // reports the problem and withholds the fix.
                let width = keke_core::abbreviation(candidates.iter().map(|it| it.id));
                let named = candidates
                    .iter()
                    .map(|session| format!("  {}  {}", session.abbreviated(width), session.summary))
                    .collect::<Vec<_>>()
                    .join("\n");
                bail!("`{typed}` matches {} sessions:\n{named}", candidates.len());
            }
            keke_core::SessionMatch::None => {
                bail!("no session starts with `{typed}`; `keke resume --list` shows what there is");
            }
        },
        None if args.last => {
            sessions
                .iter()
                .find(|session| {
                    session.turns > 0 && session.cwd.as_deref() == Some(cwd_str.as_str())
                })
                .ok_or_else(|| anyhow::anyhow!("no session to resume under {cwd_str}"))?
                .id
        }
        None => {
            bail!(
                "specify a session id, `--last` for this directory's most recent session, or `--list` to see what there is"
            );
        }
    };

    let resumed = keke_core::load_session(home, id)
        .with_context(|| format!("reading the log for session {id}"))?;
    // Where the session was started wins over where keke was invoked: resuming
    // a conversation about another directory and silently pointing its tools at
    // this one would be a different session wearing the same name.
    let cwd = resumed.cwd.as_ref().map_or(cwd, std::path::PathBuf::from);
    // What the session was last talking to wins over the config default — a
    // flag typed for this run still wins over that, since it is the more
    // specific instruction.
    if !model_explicit && let Some(model) = &resumed.model {
        config.model.model = model.clone();
    }
    if !effort_explicit && resumed.reasoning_effort.is_some() {
        config.reasoning_effort = resumed.reasoning_effort;
    }
    // No flag overrides this one yet, so what the session was last set to
    // always wins over the config default.
    if let Some(policy) = resumed.approval_policy {
        config.approval_policy = policy;
    }
    let seed = keke_tui::Resumed {
        history: resumed.history.clone(),
        usage: resumed.usage,
        context_input: resumed.context_input,
    };
    tui(
        config,
        composed,
        cwd,
        approvals,
        requests,
        seed,
        Some((id, resumed.history)),
    )
    .await
}

/// Open the interactive interface.
pub(super) async fn tui(
    config: Config,
    composed: Composed,
    cwd: std::path::PathBuf,
    approvals: Arc<keke_acp::Approvals>,
    requests: keke_acp::ApprovalRequests,
    seed: keke_tui::Resumed,
    resume: Option<(keke_protocol::SessionId, Vec<keke_protocol::Message>)>,
) -> Result<()> {
    // The directory the typing history belongs to: for a resumed session, the
    // one it was started in rather than wherever keke was invoked.
    let history_cwd = cwd.clone();
    let mut builder = session_builder(&config, &composed, cwd, config.approval_policy).await?;
    if let Some((id, history)) = resume {
        builder = builder.resume(id, history);
    }
    let commands = slash_commands(&composed);
    // Asked before the interface opens so `/model` can answer without a round
    // trip mid-conversation. It costs at most one request, and usually none:
    // the compiled-in vendors cache what they serve between runs.
    let models = models_for(&composed, &config.model.provider).await;
    let opened = keke_acp::local_with(
        builder,
        approvals,
        requests,
        Some(subagent_views(&composed.subagents)),
    )
    .await?;
    // Read only once the session has an id: a fresh session's id is minted
    // inside `session_builder`/`local`, and every recorded prompt should carry
    // the session it was actually typed in, not none at all.
    let session_id = opened
        .id
        .parse::<uuid::Uuid>()
        .map(keke_protocol::SessionId::from)
        .ok();
    let prompts = prompt_history(&config.home.home, &history_cwd, session_id);
    let (conversation, updates) = (opened.conversation, opened.updates);
    let result = keke_tui::run(
        conversation,
        updates,
        commands,
        keke_tui::SessionDefaults {
            approval: config.approval_policy,
            // What the session is actually in, not what the config said: a
            // `--plan` start, and a resumed session that was planning, both
            // reach the surface through the switch rather than through a
            // second copy of the answer.
            mode: composed
                .plan_mode
                .as_ref()
                .map_or_else(Default::default, |mode| mode.get()),
            effort: config.reasoning_effort,
            config_home: config.home.home.clone(),
        },
        keke_tui::Models {
            provider: config.model.provider.clone(),
            current: opened.model,
            available: models,
            routes: provider_choices(&composed),
        },
        seed,
        prompts,
        keke_tui::Mcp {
            // A row that cannot be built is no reason to refuse a session:
            // `/mcp` then reports nothing, which is what a person with no
            // servers sees anyway.
            servers: super::mcp::statuses(&config.home).unwrap_or_default(),
            sign_in: Some(std::sync::Arc::new(super::mcp::SignIn {
                home: config.home.clone(),
            })),
            manage: Some(std::sync::Arc::new(super::mcp::Manage {
                home: config.home.clone(),
            })),
        },
    )
    .await;
    // Printed after the terminal is restored, so a person who quit with
    // Ctrl-C/Ctrl-D lands back at a shell prompt that already tells them how
    // to pick the conversation back up.
    if let Some(id) = session_id {
        println!("keke resume {id}");
    }
    result
}
/// This project's past prompts, plus the sink new ones are appended to.
///
/// A history that cannot be read is an empty one: somebody opening keke wants
/// their session, not a startup failure over a convenience file.
/// Relay the subagent host's live rows onto the surface's update stream.
///
/// The mapping lives here because the composition root is the only place that
/// can see both ends: `keke-acp` does not know the engine has subagents, and
/// `keke-subagent` must not know a surface exists.
fn subagent_views(
    host: &std::sync::Arc<keke_subagent::SubagentHost>,
) -> tokio::sync::mpsc::UnboundedReceiver<Vec<keke_acp::SubagentView>> {
    let mut rows = host.subscribe();
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Some(rows) = rows.recv().await {
            let views = rows
                .into_iter()
                .map(|row| keke_acp::SubagentView {
                    id: row.id,
                    task: row.task,
                    status: row.status.map(|status| status.as_str().to_string()),
                    input_tokens: row.input_tokens,
                })
                .collect();
            if tx.send(views).is_err() {
                break;
            }
        }
    });
    rx
}

fn prompt_history(
    home: &AbsPath,
    cwd: &std::path::Path,
    session: Option<keke_protocol::SessionId>,
) -> keke_tui::PromptHistory {
    let mut log = keke_core::PromptHistory::new(home, cwd);
    if let Some(session) = session {
        log = log.in_session(session);
    }
    let entries = log.load().unwrap_or_else(|error| {
        tracing::warn!(%error, "could not read the prompt history");
        Vec::new()
    });
    keke_tui::PromptHistory::new(entries).with_recorder(Arc::new(PromptLog(log)))
}

/// Appends what was typed to the project's history file.
struct PromptLog(keke_core::PromptHistory);

impl keke_tui::PromptRecorder for PromptLog {
    fn record(&self, prompt: &str) {
        // Losing a line of history is not worth failing the turn a person just
        // started, so this is reported and dropped.
        if let Err(error) = self.0.record(prompt) {
            tracing::warn!(%error, "could not record the prompt history");
        }
    }
}
