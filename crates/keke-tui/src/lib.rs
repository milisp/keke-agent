//! The terminal interface.
//!
//! Everything here is written against [`keke_acp::Conversation`], never against
//! the engine: the same interface has to work attached to an in-process session
//! and to an agent across a pipe. That is also what makes it testable — the
//! state tests below drive a [`keke_acp::ScriptedConversation`] and never open a
//! terminal.

mod app;
pub(crate) mod draw;
mod history;
mod input;
mod keys;
mod login;
mod scroll;
pub mod slash;
mod transcript;

use std::io;
use std::sync::Arc;

use crossterm::event::DisableMouseCapture;
use crossterm::event::EnableMouseCapture;
use crossterm::event::Event;
use crossterm::event::EventStream;
use crossterm::execute;
use crossterm::terminal::EnterAlternateScreen;
use crossterm::terminal::LeaveAlternateScreen;
use crossterm::terminal::disable_raw_mode;
use crossterm::terminal::enable_raw_mode;
use futures::StreamExt;
use keke_acp::Conversation;
use keke_acp::Update;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use tokio::sync::mpsc::UnboundedReceiver;

pub use app::App;
pub use app::Turn;
pub use history::PromptHistory;
pub use history::PromptRecorder;
pub use input::InputBox;
pub use login::Notice;

pub use login::TuiLoginUi;
pub use scroll::Scrollback;
pub use slash::PluginCommand;
pub use slash::SlashCommand;
pub use slash::SlashCommands;
pub use transcript::CallState;
pub use transcript::Cell;
pub use transcript::PermissionCell;
pub use transcript::ToolCell;
pub use transcript::Transcript;

/// What a resumed session hands the interface: what was said, what it spent,
/// and one line saying where it came from. Empty for a fresh session, which is
/// why it is a value rather than an `Option` at every call site.
#[derive(Debug, Default)]
pub struct Resumed {
    pub history: Vec<keke_protocol::Message>,
    pub usage: keke_protocol::Usage,
    /// Shown once at the top, e.g. `resumed session … (12 messages)`.
    pub notice: Option<String>,
}

/// Run the interface until the person quits.
///
/// `updates` is the agent's stream; the app also produces its own, so both are
/// drained here rather than merged upstream. `commands` and `approval` come
/// from the composition root: nothing here knows what a plugin is, or what the
/// configured policy was. `history` is what was typed in this project before,
/// which the host reads and writes because only it knows where that lives.
pub async fn run(
    conversation: Arc<dyn Conversation>,
    updates: UnboundedReceiver<Update>,
    commands: SlashCommands,
    approval: keke_config_types::ApprovalPolicy,
    resumed: Resumed,
    history: PromptHistory,
) -> anyhow::Result<()> {
    let (app, local) = App::new(conversation);
    let mut app = app
        .with_commands(commands)
        .with_approval_policy(approval)
        .with_prompt_history(history);
    if !resumed.history.is_empty() || resumed.usage.total() > 0 {
        app = app.with_history(&resumed.history, resumed.usage);
    }
    if let Some(notice) = resumed.notice {
        app.apply_notice(Notice::Message(notice));
    }
    let mut terminal = enter()?;
    // Restore the terminal even on error: leaving a person in raw mode with no
    // echo is worse than whatever went wrong.
    let outcome = event_loop(&mut terminal, app, updates, local).await;
    leave(&mut terminal)?;
    outcome
}

type Tui = Terminal<CrosstermBackend<io::Stdout>>;

fn enter() -> anyhow::Result<Tui> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    Ok(Terminal::new(CrosstermBackend::new(stdout))?)
}

fn leave(terminal: &mut Tui) -> anyhow::Result<()> {
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    terminal.show_cursor()?;
    Ok(())
}

/// How often the status bar's clock is redrawn while a turn runs.
///
/// Under a second so the number never appears to skip one, and only while a
/// turn is on the clock: an idle interface must not wake the terminal up at
/// all, which is what a person notices on a laptop.
const TICK: std::time::Duration = std::time::Duration::from_millis(250);

async fn event_loop(
    terminal: &mut Tui,
    mut app: App,
    mut updates: UnboundedReceiver<Update>,
    mut local: UnboundedReceiver<Update>,
) -> anyhow::Result<()> {
    let mut input = EventStream::new();
    terminal.draw(|frame| draw::draw(frame, &mut app))?;

    while !app.should_quit() {
        // `pending` rather than a long sleep when nothing is timing: the arm is
        // then never ready, so an idle interface blocks until something happens.
        let timing = app.is_timing();
        let tick = async move {
            if timing {
                tokio::time::sleep(TICK).await;
            } else {
                std::future::pending::<()>().await;
            }
        };
        tokio::select! {
            () = tick => {}
            Some(update) = updates.recv() => app.apply(update),
            Some(update) = local.recv() => app.apply(update),
            event = input.next() => match event {
                Some(Ok(Event::Key(key))) => app.handle_key(key),
                Some(Ok(Event::Mouse(mouse))) => app.handle_mouse(mouse),
                Some(Ok(_)) => {}
                // The terminal went away; there is nothing left to draw on.
                Some(Err(error)) => return Err(error.into()),
                None => break,
            },
            else => break,
        }
        terminal.draw(|frame| draw::draw(frame, &mut app))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests;
