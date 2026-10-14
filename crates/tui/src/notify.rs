//! Getting a person's attention when they are not looking at the terminal.
//!
//! Escapes rather than a desktop-notification crate, for the reason
//! [`crate::clipboard`] gives: the terminal a person is sitting at is not
//! always the machine keke runs on, and over ssh a native call would notify
//! the wrong side of the connection. OSC 9 is honoured by the terminals that
//! can show a banner and the bell by nearly all of the rest, so the choice is
//! only which of the two to ask.
//!
//! Approach after codex's `tui/src/notifications`, which found the same split.

use std::io::Write;

use keke_config_types::NotificationCondition;
use keke_config_types::NotificationConfig;
use keke_config_types::NotificationMethod;

/// The longest message sent. A banner shows a line; a tool call's argument can
/// be a page.
const MAX_CHARS: usize = 80;

/// What a notification is delivered as, once `auto` has been resolved.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Delivery {
    Osc9 { tmux: bool },
    Bel,
}

/// Decides whether to interrupt, and how, and does it.
pub(crate) struct Notifier {
    delivery: Option<Delivery>,
    condition: NotificationCondition,
    /// Whether the terminal has focus. Starts true: a terminal that never
    /// reports focus changes would otherwise look permanently unfocused, and
    /// a person at the keyboard would be belled at on every turn.
    focused: bool,
}

impl Notifier {
    pub(crate) fn new(config: NotificationConfig) -> Self {
        Self::with_env(config, |name| std::env::var(name).ok())
    }

    fn with_env(config: NotificationConfig, env: impl Fn(&str) -> Option<String>) -> Self {
        Self {
            delivery: delivery_for(config.method, &env),
            condition: config.condition,
            focused: true,
        }
    }

    pub(crate) fn set_focused(&mut self, focused: bool) {
        self.focused = focused;
    }

    /// Whether this is worth sending right now.
    fn due(&self) -> bool {
        self.delivery.is_some()
            && match self.condition {
                NotificationCondition::Always => true,
                NotificationCondition::Unfocused => !self.focused,
            }
    }

    /// Send `message` if the configuration and focus say so. Failure is silent
    /// by design: a terminal that drops the escape gives no way to know it did.
    pub(crate) fn notify(&self, message: &str) {
        if !self.due() {
            return;
        }
        let Some(delivery) = self.delivery else {
            return;
        };
        let mut stdout = std::io::stdout();
        let _ = stdout.write_all(escape(delivery, message).as_bytes());
        let _ = stdout.flush();
    }
}

fn delivery_for(
    method: NotificationMethod,
    env: &impl Fn(&str) -> Option<String>,
) -> Option<Delivery> {
    let tmux = env("TMUX").is_some_and(|value| !value.is_empty());
    match method {
        NotificationMethod::Off => None,
        NotificationMethod::Bel => Some(Delivery::Bel),
        NotificationMethod::Osc9 => Some(Delivery::Osc9 { tmux }),
        NotificationMethod::Auto => Some(if shows_osc9(env) {
            Delivery::Osc9 { tmux }
        } else {
            Delivery::Bel
        }),
    }
}

/// Terminals known to turn OSC 9 into a banner. Anything else gets the bell,
/// because an unrecognised OSC is at best ignored and at worst printed.
///
/// Read from the environment the terminal sets, and for the outer terminal
/// when under tmux: `TERM_PROGRAM` then names tmux, but the variables the
/// terminal itself exported are inherited.
fn shows_osc9(env: &impl Fn(&str) -> Option<String>) -> bool {
    let set = |name: &str| env(name).is_some_and(|value| !value.is_empty());
    let program = env("TERM_PROGRAM").unwrap_or_default();
    matches!(
        program.as_str(),
        "ghostty" | "iTerm.app" | "WezTerm" | "WarpTerminal"
    ) || env("LC_TERMINAL").as_deref() == Some("iTerm2")
        || set("KITTY_WINDOW_ID")
        || set("GHOSTTY_RESOURCES_DIR")
        || set("WEZTERM_PANE")
        || set("ITERM_SESSION_ID")
}

fn escape(delivery: Delivery, message: &str) -> String {
    match delivery {
        Delivery::Bel => "\x07".to_string(),
        Delivery::Osc9 { tmux: false } => format!("\x1b]9;{}\x07", clean(message)),
        // tmux only forwards what arrives wrapped in a DCS passthrough, with
        // every ESC inside doubled. It also has to be allowed to
        // (`allow-passthrough on`); without that this is dropped, not shown.
        Delivery::Osc9 { tmux: true } => {
            format!("\x1bPtmux;\x1b\x1b]9;{}\x07\x1b\\", clean(message))
        }
    }
}

/// `message` as a single line of printable text, shortened.
///
/// Part of it can come from the model or a tool, and an escape sequence inside
/// would end the OSC early and let the rest through as terminal commands.
fn clean(message: &str) -> String {
    let flat: String = message
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let flat = flat.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() > MAX_CHARS {
        let kept: String = flat.chars().take(MAX_CHARS - 1).collect();
        format!("{kept}…")
    } else {
        flat
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |name| {
            pairs
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| (*value).to_string())
        }
    }

    fn config(method: NotificationMethod, condition: NotificationCondition) -> NotificationConfig {
        NotificationConfig { method, condition }
    }

    #[test]
    fn a_terminal_known_to_show_banners_gets_one_and_any_other_gets_the_bell() {
        for pairs in [
            &[("TERM_PROGRAM", "ghostty")][..],
            &[("TERM_PROGRAM", "iTerm.app")],
            &[("KITTY_WINDOW_ID", "3")],
            &[("WEZTERM_PANE", "0")],
        ] {
            assert_eq!(
                delivery_for(NotificationMethod::Auto, &env(pairs)),
                Some(Delivery::Osc9 { tmux: false }),
                "{pairs:?}"
            );
        }
        assert_eq!(
            delivery_for(
                NotificationMethod::Auto,
                &env(&[("TERM_PROGRAM", "Apple_Terminal")])
            ),
            Some(Delivery::Bel)
        );
    }

    #[test]
    fn off_never_delivers_whatever_the_terminal() {
        let notifier = Notifier::with_env(
            config(NotificationMethod::Off, NotificationCondition::Always),
            env(&[("TERM_PROGRAM", "ghostty")]),
        );
        assert!(!notifier.due());
    }

    #[test]
    fn unfocused_waits_for_the_terminal_to_lose_focus_and_always_does_not() {
        let mut unfocused = Notifier::with_env(
            config(NotificationMethod::Bel, NotificationCondition::Unfocused),
            env(&[]),
        );
        assert!(!unfocused.due(), "a terminal that never reported focus");
        unfocused.set_focused(false);
        assert!(unfocused.due());
        unfocused.set_focused(true);
        assert!(!unfocused.due());

        let always = Notifier::with_env(
            config(NotificationMethod::Bel, NotificationCondition::Always),
            env(&[]),
        );
        assert!(always.due());
    }

    #[test]
    fn under_tmux_the_banner_is_wrapped_so_tmux_forwards_it() {
        let delivery = delivery_for(
            NotificationMethod::Osc9,
            &env(&[("TMUX", "/tmp/tmux-1/default,1,0")]),
        )
        .expect("delivery");
        assert_eq!(escape(delivery, "hi"), "\x1bPtmux;\x1b\x1b]9;hi\x07\x1b\\");
    }

    #[test]
    fn an_escape_in_the_message_cannot_end_the_sequence_early() {
        let sent = escape(
            Delivery::Osc9 { tmux: false },
            "run \x07\x1b]52;c;Zm9v\x07 now",
        );
        assert_eq!(sent.matches('\x07').count(), 1, "{sent:?}");
        assert_eq!(sent.matches('\x1b').count(), 1, "{sent:?}");
    }

    #[test]
    fn a_long_message_is_shortened_to_one_line() {
        let cleaned = clean(&"word ".repeat(100));
        assert_eq!(cleaned.chars().count(), MAX_CHARS);
        assert!(cleaned.ends_with('…'));
    }
}
