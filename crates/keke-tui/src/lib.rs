//! The terminal interface.
//!
//! Everything here is written against [`keke_acp::Conversation`], never against
//! the engine: the same interface has to work attached to an in-process session
//! and to an agent across a pipe. That is also what makes it testable — the
//! state tests below drive a [`keke_acp::ScriptedConversation`] and never open a
//! terminal.

mod app;
mod clipboard;
pub(crate) mod draw;
mod history;
mod input;
mod keys;
mod login;
mod scroll;
mod selection;
pub mod slash;
mod transcript;

use std::io;
use std::io::Write;
use std::sync::Arc;

use crossterm::event::DisableBracketedPaste;
use crossterm::event::DisableMouseCapture;
use crossterm::event::EnableBracketedPaste;
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

/// Which model a session is asking, and what its provider serves.
///
/// A value rather than two arguments because the two are only ever meaningful
/// together: a current model with no list means "no choice to offer", and a
/// list without a current one has nothing to mark.
#[derive(Debug, Default)]
pub struct Models {
    pub current: String,
    /// Empty when the provider could not be asked and had nothing to fall back
    /// on. The surface then offers no choice rather than a wrong one.
    pub available: Vec<keke_provider_api::ModelInfo>,
}

/// What a resumed session hands the interface: what was said and what it
/// spent. Empty for a fresh session, which is why it is a value rather than an
/// `Option` at every call site.
#[derive(Debug, Default)]
pub struct Resumed {
    pub history: Vec<keke_protocol::Message>,
    pub usage: keke_protocol::Usage,
}

/// The values the session was configured with, plus where to write them back
/// to when a person switches one from the keyboard. Grouped because they are
/// only ever handed to `run` together.
pub struct SessionDefaults {
    pub approval: keke_config_types::ApprovalPolicy,
    pub effort: Option<keke_config_types::ReasoningEffort>,
    /// `$KEKE_HOME`, so `/model`, `/mode`, and `/effort` persist past this
    /// process.
    pub config_home: keke_paths::AbsPath,
}

/// Run the interface until the person quits.
///
/// `updates` is the agent's stream; the app also produces its own, so both are
/// drained here rather than merged upstream. `commands`, `defaults` and
/// `models` come from the composition root: nothing here knows what a plugin
/// is, what the configured policy or effort level was, or how to ask a provider
/// what it serves. `history` is what was typed in this project before, which
/// the host reads and writes because only it knows where that lives.
pub async fn run(
    conversation: Arc<dyn Conversation>,
    updates: UnboundedReceiver<Update>,
    commands: SlashCommands,
    defaults: SessionDefaults,
    models: Models,
    resumed: Resumed,
    history: PromptHistory,
) -> anyhow::Result<()> {
    let (app, local) = App::new(conversation);
    let mut app = app
        .with_commands(commands)
        .with_approval_policy(defaults.approval)
        .with_reasoning_effort(defaults.effort)
        .with_models(models.current, models.available)
        .with_prompt_history(history)
        .with_config_home(defaults.config_home);
    if !resumed.history.is_empty() || resumed.usage.total() > 0 {
        app = app.with_history(&resumed.history, resumed.usage);
    }
    let mut terminal = enter()?;
    // Restore the terminal even on error: leaving a person in raw mode with no
    // echo is worse than whatever went wrong.
    let outcome = event_loop(&mut terminal, app, updates, local).await;
    leave(&mut terminal)?;
    outcome
}

type Tui = Terminal<CrosstermBackend<io::Stdout>>;

/// Alternate scroll mode: the wheel arrives as arrow keys, which an empty
/// composer gives to the transcript. This is what makes scrolling work without
/// capturing the mouse, and it is harmless where capture is later switched on
/// by `/mouse` — capture wins.
const ALTERNATE_SCROLL_ON: &str = "\x1b[?1007h";
const ALTERNATE_SCROLL_OFF: &str = "\x1b[?1007l";

/// Every mouse-reporting mode, off.
///
/// `DisableMouseCapture` is not enough to get drag-select back. It is
/// winapi-only on Windows and emits no escape at all there, and a terminal left
/// reporting by an earlier run — a crash, a killed process, an embedded
/// terminal that missed the teardown — keeps eating the selection until it
/// receives these bytes. So the state is asserted, never assumed: keke writes
/// this whenever the mouse is meant to belong to the terminal.
const MOUSE_TRACKING_OFF: &str = "\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1015l\x1b[?1006l";

/// Put the mouse where it belongs and make sure the terminal has heard.
fn set_mouse_capture(stdout: &mut io::Stdout, capture: bool) -> io::Result<()> {
    if capture {
        execute!(stdout, EnableMouseCapture)?;
    } else {
        execute!(stdout, DisableMouseCapture)?;
        stdout.write_all(MOUSE_TRACKING_OFF.as_bytes())?;
    }
    stdout.flush()
}

fn enter() -> anyhow::Result<Tui> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableBracketedPaste)?;
    // keke answers the drag itself, so capturing the mouse costs the reader
    // nothing: see `crate::selection`.
    set_mouse_capture(&mut stdout, true)?;
    stdout.write_all(ALTERNATE_SCROLL_ON.as_bytes())?;
    stdout.flush()?;
    Ok(Terminal::new(CrosstermBackend::new(stdout))?)
}

fn leave(terminal: &mut Tui) -> anyhow::Result<()> {
    disable_raw_mode()?;
    let mut stdout = io::stdout();
    stdout.write_all(ALTERNATE_SCROLL_OFF.as_bytes())?;
    set_mouse_capture(&mut stdout, false)?;
    execute!(
        terminal.backend_mut(),
        DisableBracketedPaste,
        LeaveAlternateScreen
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
    let mut capturing = app.mouse_capture();
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
                Some(Ok(Event::Paste(text))) => app.handle_paste(&text),
                Some(Ok(_)) => {}
                // The terminal went away; there is nothing left to draw on.
                Some(Err(error)) => return Err(error.into()),
                None => break,
            },
            else => break,
        }
        // The one thing the app cannot do for itself: the clipboard is the
        // terminal's, and the terminal is the event loop's.
        if let Some(text) = app.take_pending_copy() {
            clipboard::copy(&text);
        }
        if app.mouse_capture() != capturing {
            capturing = app.mouse_capture();
            set_mouse_capture(&mut io::stdout(), capturing)?;
        }
        terminal.draw(|frame| draw::draw(frame, &mut app))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests;
