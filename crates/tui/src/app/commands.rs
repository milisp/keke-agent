//! Slash commands: `/help`, `/mcp`, and file-backed prompt commands.

use std::sync::Arc;

use crate::login::Notice;
use crate::slash::Builtin;
use crate::slash::SlashAction;
use crate::transcript::Cell;

use super::App;
use super::Update;

impl App {
    pub(super) fn run_command(&mut self, typed: &str, name: &str, arguments: &str) {
        let Some(command) = self.commands.find(name) else {
            self.transcript.push(Cell::Error(format!(
                "unknown command /{name} — /help lists them"
            )));
            return;
        };
        match command.action.clone() {
            SlashAction::Builtin(Builtin::Help) => {
                let text = self.help_text();
                self.transcript.push(Cell::Notice(text));
            }
            SlashAction::Builtin(Builtin::Clear) => {
                // On screen only: the rollout log is the record, and a person
                // clearing the view is not asking the agent to forget.
                self.transcript.clear();
                self.scroll.follow();
            }
            SlashAction::Builtin(Builtin::New) => self.start_new_session(),
            SlashAction::Builtin(Builtin::Quit) => self.should_quit = true,
            SlashAction::Builtin(Builtin::Copy) => self.copy_last_reply(),
            SlashAction::Builtin(Builtin::Export) => self.export_command(arguments),
            SlashAction::Builtin(Builtin::Mcp) => self.mcp_command(arguments),
            SlashAction::Builtin(Builtin::Plan) => self.plan_command(arguments),
            SlashAction::Builtin(Builtin::ViewPlan) => self.view_plan_command(),
            SlashAction::Builtin(Builtin::Effort) => match crate::slash::effort(arguments) {
                Ok(Some(effort)) => self.set_reasoning_effort_aloud(effort),
                Ok(None) => {
                    let next = crate::slash::next_effort(self.effort, &self.offered_efforts());
                    self.set_reasoning_effort_aloud(next);
                }
                Err(unknown) => self.transcript.push(Cell::Error(unknown)),
            },
            SlashAction::Builtin(Builtin::Model) => {
                let wanted = arguments.trim().to_string();
                if wanted.is_empty() {
                    self.open_model_picker();
                } else {
                    self.set_model_aloud(&wanted);
                }
            }
            SlashAction::Builtin(Builtin::Provider) => {
                let wanted = arguments.trim().to_string();
                if wanted.is_empty() {
                    self.open_provider_picker();
                } else {
                    self.set_provider_aloud(&wanted);
                }
            }
            SlashAction::Prompt(path) => match std::fs::read_to_string(&path) {
                Ok(body) => {
                    let text = if arguments.is_empty() {
                        body
                    } else {
                        format!("{body}\n\n{arguments}")
                    };
                    // What the person typed is what they should see; the body
                    // goes to the model, not onto their screen.
                    self.transcript.push(Cell::User(typed.trim().to_string()));
                    self.send_text(text);
                }
                Err(error) => self
                    .transcript
                    .push(Cell::Error(format!("reading {}: {error}", path.display()))),
            },
        }
    }

    /// `/plan`, and `/plan <what to do>`.
    ///
    /// The bare form only asks for the mode — the agent starts planning at the
    /// next prompt, whatever that turns out to be. With a description it is one
    /// step, because "plan this" is a single thought and making a person send
    /// the mode and then the work separately is a chance to forget the second.
    fn plan_command(&mut self, arguments: &str) {
        self.request_session_mode(keke_config_types::SessionMode::Plan);
        let task = arguments.trim();
        if task.is_empty() {
            return;
        }
        self.transcript.push(Cell::User(task.to_string()));
        self.send_text(task.to_string());
    }

    /// `/export <path>` — the messages so far, as Markdown.
    ///
    /// The path is reported back in full: a relative one was resolved against
    /// the working directory, and a person who has to guess where the file
    /// landed has not really been given it.
    fn export_command(&mut self, arguments: &str) {
        let outcome = crate::export::destination(arguments, self.cwd())
            .and_then(|path| crate::export::write(self.transcript.cells(), &path).map(|()| path));
        match outcome {
            Ok(path) => self
                .transcript
                .push(Cell::Notice(format!("exported to {}", path.display()))),
            Err(refusal) => self.transcript.push(Cell::Error(refusal)),
        }
    }

    /// `/mcp`, and `/mcp login <name>`.
    ///
    /// The bare form opens the overlay, because "which servers are there and is
    /// anything wrong with them" is a question, and an answer printed into the
    /// transcript scrolls away while the person is still acting on it. The
    /// spelled-out form stays: a name typed in full is an instruction.
    fn mcp_command(&mut self, arguments: &str) {
        let arguments = arguments.trim();
        if arguments.is_empty() {
            self.open_mcp_picker();
            return;
        }

        let Some(name) = arguments.strip_prefix("login").map(str::trim) else {
            self.transcript.push(Cell::Error(format!(
                "/mcp takes nothing, or `login <name>` — not {arguments:?}"
            )));
            return;
        };
        if name.is_empty() {
            self.transcript
                .push(Cell::Error("which server? `/mcp login <name>`".to_string()));
            return;
        }

        if let Err(refusal) = self.mcp_login(name) {
            self.transcript.push(Cell::Error(refusal));
        } else {
            self.transcript
                .push(Cell::Notice(format!("authorizing `{name}`...")));
        }
    }

    /// Start the browser flow for one server, or say why it cannot start.
    ///
    /// Shared by `/mcp login <name>` and by enter on an overlay row so the two
    /// cannot come to different conclusions about whether a server can be
    /// signed in to. The refusal is returned rather than printed because those
    /// two callers show it in different places: one in the transcript, where
    /// the command was typed, and one on the row it is about.
    pub(super) fn mcp_login(&mut self, name: &str) -> Result<(), String> {
        let Some(server) = self.mcp.iter().find(|server| server.name == name) else {
            return Err(format!("no MCP server named `{name}` — /mcp lists them"));
        };
        if !server.allowed {
            return Err(format!(
                "`{name}` is held back until trusted — `keke plugin trust {}`",
                server.plugin
            ));
        }
        if !server.remote {
            return Err(format!(
                "`{name}` is a program on this machine; there is nothing to sign in to"
            ));
        }

        let (Some(sign_in), Some(notices)) = (self.sign_in.clone(), self.notices.clone()) else {
            return Err(format!(
                "this interface cannot sign in — run `keke mcp login {name}` in a terminal"
            ));
        };

        let name = name.to_string();
        self.mcp_activity
            .insert(name.clone(), "authorizing...".to_string());
        tokio::spawn(async move {
            let ui = Arc::new(crate::login::McpLoginUi::new(name.clone(), notices.clone()));
            let outcome = sign_in.sign_in(name.clone(), ui).await;
            let _ = notices.send(match outcome {
                Ok(()) => Notice::SignedIn(name.clone()),
                Err(reason) => Notice::McpProgress {
                    name,
                    message: format!("could not sign in: {reason}"),
                },
            });
        });
        Ok(())
    }

    /// What a login is saying about `name` right now, if anything.
    #[must_use]
    pub fn mcp_activity(&self, name: &str) -> Option<&str> {
        self.mcp_activity.get(name).map(String::as_str)
    }

    fn help_text(&self) -> String {
        let mut text = String::from(
            "keys:\n  ctrl-o — expand or collapse the newest thought or run of calls\n  \
             ctrl-t — show or hide reasoning\n  \
             drag to select and copy; click a tool call to expand it\n\ncommands:",
        );
        for entry in self.commands.entries() {
            text.push_str(&format!("\n  /{} — {}", entry.name, entry.description));
        }
        text
    }

    /// Start a turn with text that did not come from the input box.
    pub(super) fn send_text(&mut self, text: String) {
        self.scroll.follow();
        self.begin_turn();
        let conversation = Arc::clone(&self.conversation);
        let local = self.local.clone();
        tokio::spawn(async move {
            if let Err(error) = conversation.prompt(text).await {
                let _ = local.send(Update::Failed(error.to_string()));
            }
        });
    }
}
