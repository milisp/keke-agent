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
use keke_protocol::StopReason;
use tokio::sync::mpsc::UnboundedReceiver;

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

/// The config option id keke offers a model under. The client sends it back,
/// so it is the wire contract for what was changed.
const MODEL: &str = "model";

/// One live ACP session.
struct Entry {
    conversation: Arc<dyn Conversation>,
    /// What the provider serves, for answering `session/set_config_option` and
    /// for describing the choice in the first place.
    models: Vec<String>,
    /// Fed by the pump when a turn ends, read by the prompt handler. Carried in
    /// keke's own terms rather than either wire's: v1 wants the reason as the
    /// response to `session/prompt` and v2 wants it on the update stream, and
    /// this must serve both.
    outcomes: tokio::sync::Mutex<UnboundedReceiver<StopReason>>,
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

/// The models a session may be switched to, or the reason it may not.
///
/// Shared so the two versions cannot disagree about what a client is allowed to
/// ask for — only about how the refusal is spelled.
fn chosen_model(entry: &Entry, chosen: Option<String>) -> Option<String> {
    chosen.filter(|model| entry.models.contains(model))
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
}
