//! Serving ACP to an editor.

use std::sync::Arc;

use anyhow::Context as _;
use anyhow::Result;
use anyhow::bail;
use keke_auth_api::CredentialRef;
use keke_config::Config;

use super::models_for;
use super::plugin_commands;
use super::session_builder;
use super::slash_commands;
use super::unusable_key;
use crate::compose::Composed;

/// Serve ACP to an editor.
///
/// The editor is the one asking a person, so approval requests travel to it
/// over the protocol rather than being answered here.
pub(super) async fn agent(
    transport: crate::cli::AgentTransport,
    config: Config,
    cwd: std::path::PathBuf,
) -> Result<()> {
    let crate::cli::AgentTransport::Stdio = transport;
    let factory = Arc::new(EditorSessions {
        config,
        cwd,
        route: std::sync::Mutex::new(None),
    });
    keke_acp::serve_stdio(factory)
        .await
        .map_err(|error| anyhow::anyhow!("the ACP connection failed: {error}"))
}
/// Opens one keke session per ACP session.
///
/// The vendors are composed per session rather than once, because the approval
/// bridge is part of the frozen extension set and two sessions sharing one
/// would route a prompt raised in one to whoever answered in the other.
struct EditorSessions {
    config: Config,
    cwd: std::path::PathBuf,
    /// The route `authenticate` last signed the client in to, overriding
    /// `config.model.provider` for every session opened afterward on this
    /// connection.
    ///
    /// Behind a lock because `authenticate` and `session/new` are separate
    /// RPCs on the same connection: the client authenticates once, then opens
    /// however many sessions follow, and each must see the same answer.
    route: std::sync::Mutex<Option<String>>,
}

impl EditorSessions {
    /// The directory to root a session in.
    ///
    /// The client names it; keke's own `--cwd` is the fallback for a client
    /// that does not.
    fn rooted_at(&self, cwd: std::path::PathBuf) -> std::path::PathBuf {
        if cwd.as_os_str().is_empty() {
            self.cwd.clone()
        } else {
            cwd
        }
    }

    /// The route to open a session against: what `authenticate` last chose,
    /// or the server's own configured default.
    fn active_route(&self) -> String {
        self.route
            .lock()
            .ok()
            .and_then(|route| route.clone())
            .unwrap_or_else(|| self.config.model.provider.clone())
    }

    /// Write the chosen route to `$KEKE_HOME/config.toml`, the way the TUI
    /// persists `/model`.
    ///
    /// Without this, signing in over ACP lasts exactly as long as the
    /// connection: a client that reconnects — for a new working directory, or
    /// because the person reloaded a page — lands back on the configured
    /// default route and is told to sign in to a vendor it already has
    /// credentials for.
    ///
    /// The model travels with the route. A model name belongs to one vendor,
    /// so carrying the old one over would persist a pairing that cannot serve
    /// a single request; the route's own first model is a wrong-but-working
    /// starting point the client can then change, and the effort goes with it
    /// because a ladder is a property of the model that named it.
    ///
    /// Best-effort throughout: a home that cannot be written is a worse
    /// session next time, not a failed login now.
    async fn remember_route(&self, composed: &Composed, route: &str) {
        let models = models_for(composed, route).await;
        let replacement = (!models
            .iter()
            .any(|model| model.id == self.config.model.model))
        .then(|| models.first().map(|model| model.id.clone()))
        .flatten();
        if let Err(error) = keke_config::persist_user_override(&self.config.home.home, |file| {
            file.provider = Some(route.to_string());
            if let Some(model) = replacement {
                file.model = Some(model);
                file.reasoning_effort = None;
            }
        }) {
            tracing::warn!(%route, %error, "could not persist the route to config.toml");
        }
    }

    /// What the session's provider serves, for the client to choose between.
    ///
    /// A provider that cannot be asked leaves the list empty rather than
    /// failing the session: not being able to switch models is a smaller loss
    /// than not being able to open the conversation at all.
    async fn models(&self, composed: &Composed, route: &str) -> Vec<keke_provider_api::ModelInfo> {
        models_for(composed, route).await
    }

    /// Build one session, new or continuing, and start it.
    async fn start(
        &self,
        cwd: std::path::PathBuf,
        resume: Option<keke_core::ResumedSession>,
    ) -> Result<keke_acp::Opened> {
        let (approvals, requests) = keke_acp::approvals();
        let composed = Composed::build(
            &self.config.home,
            &self.config.providers,
            self.config.plugins,
            self.config.model_catalog_ttl,
            self.config.subagents,
            Some(Arc::clone(&approvals)),
            // One switch per session, made here because this is where a
            // session is: an ACP client opens several, and they plan
            // independently.
            Some(crate::compose::PlanSetup::for_session(
                &self.config.home.home,
                &cwd,
                Arc::new(keke_core::SessionModeSwitch::default()),
            )),
        )?;
        // What the session was last talking to wins over the server's config
        // default: a client reopening a session means to continue it, not to
        // switch it back to whatever keke was started with.
        let mut config = self.config.clone();
        config.model.provider = self.active_route();
        if let Some(model) = resume.as_ref().and_then(|resumed| resumed.model.clone()) {
            config.model.model = model;
        }
        if let Some(effort) = resume.as_ref().and_then(|resumed| resumed.reasoning_effort) {
            config.reasoning_effort = Some(effort);
        }
        if let Some(policy) = resume.as_ref().and_then(|resumed| resumed.approval_policy) {
            config.approval_policy = policy;
        }
        let mut builder = session_builder(&config, &composed, cwd, config.approval_policy).await?;
        let history = resume.as_ref().map(|resumed| resumed.history.clone());
        if let Some(resumed) = resume {
            builder = builder.resume(resumed.id, resumed.history);
        }
        let mut opened = keke_acp::local(builder, approvals, requests).await?;
        opened.history = history.unwrap_or_default();
        opened.models = self.models(&composed, &config.model.provider).await;
        // The same resolved list the TUI would complete against, so an ACP
        // editor's own autocomplete offers exactly what keke's own interface
        // would — see `slash_commands`.
        opened.commands = plugin_commands(&slash_commands(&composed));
        Ok(opened)
    }
}

impl keke_acp::SessionFactory for EditorSessions {
    fn open(
        &self,
        cwd: std::path::PathBuf,
    ) -> keke_acp::ConversationFuture<'_, Result<keke_acp::Opened, keke_acp::ConversationError>>
    {
        Box::pin(async move {
            self.start(self.rooted_at(cwd), None)
                .await
                .map_err(|error| keke_acp::ConversationError::Agent(error.to_string()))
        })
    }

    /// Every route there is to authenticate to, not only the ones with a
    /// login flow — a route wanting an API key, or wanting nothing at all
    /// (a local server), is just as much an answer to "how do I sign in?" as
    /// one with OAuth behind it. Mirrors the three-way split
    /// `first_run::pick` offers interactively, minus the local-server presets
    /// that only exist once someone has declared them.
    fn auth_methods(&self) -> Vec<keke_acp::AuthMethodDescriptor> {
        let Ok(composed) = Composed::build(
            &self.config.home,
            &self.config.providers,
            self.config.plugins,
            self.config.model_catalog_ttl,
            self.config.subagents,
            None,
            // Not a session: nothing to plan in, so nothing to install.
            None,
        ) else {
            return Vec::new();
        };
        composed
            .providers
            .routes()
            .filter_map(|route| {
                let handle = composed.providers.get(route).ok()?;
                let info = handle.info();
                // The same three-way split the description makes, answered
                // for the credential that is actually there: a client showing
                // a sign-in menu has to be able to tell "sign in" from
                // "already signed in, through this".
                let (signed_in, source, description) = match composed.auth_for(route) {
                    Some(auth) if auth.has_usable_credential() => {
                        let source = auth.snapshot().source;
                        (
                            true,
                            Some(source.clone()),
                            format!("Signed in with {} ({source})", info.display_name),
                        )
                    }
                    Some(_) => (false, None, format!("Sign in with {}", info.display_name)),
                    None => match info.env_key.as_deref() {
                        Some(env_key) => {
                            let stored = unusable_key(&composed, info).is_none();
                            (
                                stored,
                                stored.then(|| env_key.to_string()),
                                if stored {
                                    format!("API key ({env_key}) is set")
                                } else {
                                    format!("API key ({env_key})")
                                },
                            )
                        }
                        // Takes no credential at all (a local server): usable
                        // by definition, so it is signed in by definition.
                        None => (true, None, "No credential needed".to_string()),
                    },
                };
                Some(keke_acp::AuthMethodDescriptor {
                    id: route.to_string(),
                    name: info.display_name.clone(),
                    description: Some(description),
                    signed_in,
                    source,
                })
            })
            .collect()
    }

    /// Settle one route's credential, then make it the route new sessions
    /// open against — on this connection, and on the next one.
    ///
    /// `meta.apiKey` lets a client that collected a key itself hand it
    /// straight over — the same act as `keke login`'s key prompt, just spoken
    /// over ACP instead of a terminal — but is never required: a route whose
    /// credential is already resolvable (stored, or in the environment) or
    /// that needs none at all succeeds without it.
    ///
    /// `meta.force` re-runs the login flow even then. A stored credential is
    /// only ever known to be *present*, never to be *good* — an expired
    /// token, or a key some other tool left behind — so a person who asks to
    /// sign in again must get a login rather than a silent success that
    /// leaves the bad credential in place.
    fn authenticate(
        &self,
        method_id: &str,
        meta: Option<serde_json::Map<String, serde_json::Value>>,
    ) -> keke_acp::ConversationFuture<'_, Result<(), keke_acp::ConversationError>> {
        let method_id = method_id.to_string();
        let pasted_key = meta
            .as_ref()
            .and_then(|meta| meta.get("apiKey"))
            .and_then(|value| value.as_str())
            .map(str::to_string);
        let force = meta
            .as_ref()
            .and_then(|meta| meta.get("force"))
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        Box::pin(async move {
            let composed = Composed::build(
                &self.config.home,
                &self.config.providers,
                self.config.plugins,
                self.config.model_catalog_ttl,
                self.config.subagents,
                None,
                None,
            )
            .map_err(|error| keke_acp::ConversationError::Agent(error.to_string()))?;
            let provider = composed.providers.get(&method_id).map_err(|_| {
                keke_acp::ConversationError::Agent(format!(
                    "unknown authentication method `{method_id}`"
                ))
            })?;

            if let Some(auth) = composed.auth_for(&method_id) {
                if force || !auth.has_usable_credential() {
                    auth.login(Arc::new(crate::ui::AcpLoginUi))
                        .await
                        .map_err(|error| keke_acp::ConversationError::Agent(error.to_string()))?;
                }
            } else if let Some(env_key) = provider.info().env_key.clone() {
                let reference = CredentialRef::new(env_key.clone())
                    .map_err(|error| keke_acp::ConversationError::Agent(error.to_string()))?;
                let already_usable = composed
                    .credentials
                    .load(&reference)
                    .ok()
                    .flatten()
                    .is_some();
                if let Some(key) = pasted_key {
                    composed
                        .credentials
                        .save(&reference, &key)
                        .map_err(|error| keke_acp::ConversationError::Agent(error.to_string()))?;
                } else if force || !already_usable {
                    let stored = if already_usable {
                        format!("the stored {env_key} was not replaced")
                    } else {
                        format!("no {env_key} stored")
                    };
                    return Err(keke_acp::ConversationError::Agent(format!(
                        "{stored}; pass one in `authenticate`'s `apiKey` field, \
                         or export {env_key} and try again"
                    )));
                }
            }
            // Neither branch: the route takes no credential (a local server),
            // and is usable by definition.

            if let Ok(mut route) = self.route.lock() {
                *route = Some(method_id.clone());
            }
            self.remember_route(&composed, &method_id).await;
            Ok(())
        })
    }

    fn list(
        &self,
        cwd: Option<std::path::PathBuf>,
    ) -> keke_acp::ConversationFuture<
        '_,
        Result<Vec<keke_acp::SessionListing>, keke_acp::ConversationError>,
    > {
        Box::pin(async move {
            let sessions = keke_core::list_sessions(&self.config.home.home)
                .map_err(|error| keke_acp::ConversationError::Agent(error.to_string()))?;
            Ok(sessions
                .into_iter()
                // A log with no turns is an interface someone opened and
                // closed; listing them buries the conversations under the
                // empty files, exactly as `keke resume --list` decided.
                .filter(|session| session.turns > 0)
                .filter(|session| match (&cwd, &session.cwd) {
                    (Some(wanted), Some(started_in)) => std::path::Path::new(started_in) == wanted,
                    // A filter the log cannot answer excludes the session
                    // rather than guessing it matches.
                    (Some(_), None) => false,
                    (None, _) => true,
                })
                .map(|session| keke_acp::SessionListing {
                    id: session.id.to_string(),
                    cwd: session
                        .cwd
                        .as_ref()
                        .map_or_else(|| self.cwd.clone(), std::path::PathBuf::from),
                    title: session.summary,
                    updated_at: session.updated_at,
                })
                .collect())
        })
    }

    fn resume(
        &self,
        id: String,
        cwd: std::path::PathBuf,
    ) -> keke_acp::ConversationFuture<'_, Result<keke_acp::Opened, keke_acp::ConversationError>>
    {
        Box::pin(async move {
            self.reopen(id, cwd)
                .await
                .map_err(|error| keke_acp::ConversationError::Agent(error.to_string()))
        })
    }
}

impl EditorSessions {
    /// Resolve what the client sent back to one session, and continue it.
    async fn reopen(&self, id: String, cwd: std::path::PathBuf) -> Result<keke_acp::Opened> {
        let home = &self.config.home.home;
        // The same prefix matching `keke resume` takes, so a client may hand
        // back either the id it was shown or the whole thing.
        let session = match keke_core::find_session(home, &id)? {
            keke_core::SessionMatch::One(session) => session,
            // Invariant 8: two claimants and no way to choose is an error, not
            // a pick.
            keke_core::SessionMatch::Ambiguous(candidates) => {
                bail!("`{id}` matches {} sessions", candidates.len())
            }
            keke_core::SessionMatch::None => bail!("no session `{id}`"),
        };
        let resumed = keke_core::load_session(home, session.id)
            .with_context(|| format!("reading the log for session {}", session.id))?;
        // Where the session was started wins over where the client says it is:
        // pointing a resumed conversation's tools at another directory is a
        // different session wearing the same name.
        let cwd = resumed
            .cwd
            .as_ref()
            .map_or_else(|| self.rooted_at(cwd), std::path::PathBuf::from);
        self.start(cwd, Some(resumed)).await
    }
}
