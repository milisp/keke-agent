//! Configuration value types.
//!
//! These are separated from the loader (`keke-config`) on purpose: a provider
//! plugin needs to name a setting without depending on how settings are read
//! from disk, merged across layers, or hot-reloaded.
//!
//! Every field here is deployment-varying by intent. A constant that a
//! deployment might reasonably want to change does not belong in a plugin as a
//! `DEFAULT_*` — it belongs here, validated.

use keke_paths::AbsPath;
use serde::Deserialize;
use serde::Serialize;

/// Re-exported rather than restated, unlike [`DeclaredWireApi`]: the level a
/// deployment configures is written verbatim into the session log, so a second
/// enum here could disagree with the one the log is defined in terms of.
/// `keke-protocol` sits beneath every tier, so naming it costs nothing.
pub use keke_protocol::ReasoningEffort;

/// How much the harness may do without asking.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalPolicy {
    /// Ask before anything with an effect outside the workspace.
    #[default]
    OnRequest,
    /// Ask only when a command fails and wants to escalate.
    OnFailure,
    /// Never ask. Intended for CI, not for interactive use.
    Never,
}

impl ApprovalPolicy {
    /// The wire spelling, for anywhere that needs it outside a `serde` path —
    /// the session log included, which is written by `keke-core` and read
    /// back by `keke-core` too, but through `keke-protocol`'s `SessionEvent`,
    /// a crate ranked below this one that cannot name this type directly.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::OnRequest => "on-request",
            Self::OnFailure => "on-failure",
            Self::Never => "never",
        }
    }

    /// The inverse of [`Self::as_str`]. `None` for anything else, including a
    /// spelling from a future build this one does not know — the caller
    /// decides whether that is an error or a value to fall back from.
    #[must_use]
    pub fn parse(wire: &str) -> Option<Self> {
        match wire {
            "on-request" => Some(Self::OnRequest),
            "on-failure" => Some(Self::OnFailure),
            "never" => Some(Self::Never),
            _ => None,
        }
    }
}

/// How tightly spawned processes are confined.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SandboxMode {
    /// No filesystem writes outside the workspace, no network.
    #[default]
    WorkspaceWrite,
    /// Reads only.
    ReadOnly,
    /// No confinement.
    DangerFullAccess,
}

/// Which model to run, and where.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelSelection {
    /// Provider route key, matching a registered
    /// [`ProviderInfo::route`](../keke_provider_api/struct.ProviderInfo.html).
    pub provider: String,
    pub model: String,
}

/// A provider route declared from configuration rather than compiled in.
///
/// The three wire formats are implemented once, so most vendors are a base URL,
/// a credential name, and a choice of format — not a crate. Compiled-in
/// providers exist for vendors that need real behavior of their own (an OAuth
/// flow, a non-standard error shape); everything else can be declared here, and
/// a person can add an endpoint keke has never heard of without rebuilding.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub struct ProviderDeclaration {
    /// The route key, which is what `--provider` and `keke login` name. Comes
    /// from the table key in configuration, not from a field.
    #[serde(skip)]
    pub route: String,
    /// Shown in surfaces; defaults to the route.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    pub base_url: String,
    /// Which inference format the endpoint speaks.
    #[serde(default)]
    pub wire: DeclaredWireApi,
    /// Environment variable holding the API key, e.g. `NVIDIA_API_KEY`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env_key: Option<String>,
    /// The model used when none is given.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_model: Option<String>,
    /// Path to a PEM-encoded CA certificate trusted in addition to the system
    /// roots, for an endpoint behind a corporate TLS-intercepting gateway or
    /// serving a self-signed certificate. Without this, such an endpoint is
    /// simply unreachable — this is the field that decides whether keke runs
    /// on a locked-down corporate network at all.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ca_cert_path: Option<String>,
    /// Outbound proxy this provider's requests are sent through, e.g.
    /// `http://proxy.internal:8080`. Unset means whatever `reqwest` picks up
    /// from the environment on its own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proxy: Option<String>,
    /// Basic-auth username for `proxy`, if it requires one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proxy_username: Option<String>,
    /// Environment variable holding the basic-auth password for `proxy`. An
    /// env_key indirection rather than a literal field, matching `env_key`:
    /// a proxy credential is a secret and does not belong in a config file
    /// either.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proxy_password_env_key: Option<String>,
    /// Extra HTTP headers sent with every request to this provider, e.g. for
    /// a gateway that identifies the caller for quota or audit purposes
    /// (`X-Company-User-Id`, `X-Department-Token`). A value of the form
    /// `env:VAR_NAME` is resolved from the environment at startup rather
    /// than taken literally, so a header carrying a secret need not be
    /// written into the config file in the clear. `authorization` is
    /// reserved for the provider's own credential and may not be set here.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub headers: std::collections::BTreeMap<String, String>,
}

/// The wire format a declared provider speaks.
///
/// Mirrors `keke_provider_api::WireApi`, restated here so `keke-config-types`
/// need not depend on the provider contract — a config value type must not drag
/// in the runtime that consumes it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeclaredWireApi {
    #[default]
    ChatCompletions,
    Responses,
    Messages,
}

/// Where the harness keeps its state.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HomeLayout {
    /// Root of the harness's own state, `$KEKE_HOME` or `~/.keke`.
    pub home: AbsPath,
    /// The project being worked in.
    pub workspace_root: AbsPath,
}

/// How many tokens a single model reply may produce.
///
/// Every request carries one. Anthropic's wire rejects a request that omits it,
/// and leaving the choice to each provider would mean the same conversation got
/// a different budget depending on which vendor served it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct MaxOutputTokens(u32);

/// Refuses to name a budget so small no reply fits, or so large no provider
/// accepts it. Both fail at the vendor, far from the setting that caused it.
impl MaxOutputTokens {
    pub const MIN: u32 = 256;
    pub const MAX: u32 = 200_000;

    /// Validate a configured budget.
    pub fn new(value: u32) -> Result<Self, String> {
        if (Self::MIN..=Self::MAX).contains(&value) {
            Ok(Self(value))
        } else {
            Err(format!(
                "max-output-tokens must be between {} and {}, got {value}",
                Self::MIN,
                Self::MAX
            ))
        }
    }

    #[must_use]
    pub fn get(self) -> u32 {
        self.0
    }
}

impl Default for MaxOutputTokens {
    fn default() -> Self {
        Self(8192)
    }
}

/// Context window management.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompactionConfig {
    /// Fraction of the context window at which compaction triggers, in
    /// percent. Compacting too late risks a hard overflow mid-turn.
    pub trigger_percent: u8,
    /// Messages at the tail always kept verbatim.
    pub keep_recent_messages: usize,
    /// The window compaction measures against.
    ///
    /// Configured rather than read from the provider: a model list is a network
    /// call, and a session that could not reach it would silently never compact
    /// — the failure mode compaction exists to prevent.
    pub context_window: u32,
}

impl Default for CompactionConfig {
    fn default() -> Self {
        Self {
            trigger_percent: 80,
            keep_recent_messages: 4,
            context_window: 128_000,
        }
    }
}

/// How long a fetched model catalog stays usable without asking the vendor
/// again.
///
/// A validated field rather than a constant in whichever plugin does the
/// fetching: how long a deployment is willing to show yesterday's model list is
/// exactly the kind of number one deployment sets differently from another. An
/// air-gapped install wants a long life; someone tracking a vendor's weekly
/// releases wants a short one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ModelCatalogTtl(u64);

impl ModelCatalogTtl {
    /// Zero is a real setting — "ask every time" — so the floor is zero rather
    /// than a minimum age. The ceiling exists because a lifetime longer than a
    /// week is indistinguishable from never refreshing, and a picker that never
    /// learns about a new model looks like keke not supporting it.
    pub const MAX_SECONDS: u64 = 7 * 24 * 60 * 60;

    /// Validate a configured lifetime, in seconds.
    pub fn new(seconds: u64) -> Result<Self, String> {
        if seconds <= Self::MAX_SECONDS {
            Ok(Self(seconds))
        } else {
            Err(format!(
                "model-catalog-ttl must be at most {} seconds, got {seconds}",
                Self::MAX_SECONDS
            ))
        }
    }

    #[must_use]
    pub fn get(self) -> std::time::Duration {
        std::time::Duration::from_secs(self.0)
    }

    #[must_use]
    pub fn seconds(self) -> u64 {
        self.0
    }
}

/// Six hours: long enough that opening the interface a dozen times in an
/// afternoon costs one request, short enough that a model released this morning
/// is selectable this evening.
impl Default for ModelCatalogTtl {
    fn default() -> Self {
        Self(6 * 60 * 60)
    }
}

/// Budgets for the programs runtime plugins bring with them.
///
/// A hook and an MCP server are both someone else's process running inside a
/// turn, and how long either may take is exactly the kind of number one
/// deployment sets differently from another: a server that shells out to a
/// package manager needs a longer budget than one that reads a file. That is
/// why these live here rather than as `DEFAULT_*` constants inside the crates
/// that use them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginTimeouts {
    /// Applied to a hook that declares no timeout of its own. There is no
    /// "wait forever" setting: a hook runs before the tool it guards, so one
    /// that never returns does not slow the turn down, it stops it.
    pub hook_millis: u64,
    /// How long an MCP server has to answer `initialize` and `tools/list`.
    pub mcp_startup_millis: u64,
    /// The ceiling for a single `tools/call`.
    pub mcp_call_millis: u64,
}

impl PluginTimeouts {
    /// The shortest budget worth naming. Below this the timeout is likelier to
    /// be a unit mistake — seconds written where milliseconds were meant — than
    /// a deliberate setting, and every hook would deny.
    pub const MIN_MILLIS: u64 = 100;
    /// An hour. A budget beyond this is indistinguishable from none, which is
    /// the state these fields exist to prevent.
    pub const MAX_MILLIS: u64 = 3_600_000;

    /// Validate one budget, naming the field so the message points at the line
    /// that has to change.
    pub fn check(field: &str, value: u64) -> Result<u64, String> {
        if (Self::MIN_MILLIS..=Self::MAX_MILLIS).contains(&value) {
            Ok(value)
        } else {
            Err(format!(
                "plugins.{field} must be between {} and {} milliseconds, got {value}",
                Self::MIN_MILLIS,
                Self::MAX_MILLIS
            ))
        }
    }
}

impl Default for PluginTimeouts {
    fn default() -> Self {
        Self {
            hook_millis: 30_000,
            mcp_startup_millis: 15_000,
            mcp_call_millis: 120_000,
        }
    }
}

/// Bounds on the subagents a session may run.
///
/// Both numbers are deployment-varying in the way invariant 9 in `AGENTS.md`
/// means: how many models a person is willing to pay for at once, and how long
/// they are willing to let one run unattended, are answered differently by a
/// laptop on a metered key and by a CI runner.
///
/// There is deliberately no depth setting. A subagent cannot spawn a subagent —
/// the tools are not advertised to it at all — so the tree is one level deep by
/// construction rather than by a number someone can raise until a session forks
/// without bound. A limit that exists only when configured correctly is the
/// failure mode the limit was for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubagentLimits {
    /// How many subagents may be running at once. Spawns beyond this queue
    /// rather than fail: a rejected spawn is something the model retries, and a
    /// retry loop costs more than the wait it was avoiding.
    pub max_concurrent: u8,
    /// The wall-clock ceiling for one subagent's turn. It is cancelled at the
    /// limit and reported as timed out, so the parent gets an answer either way.
    pub timeout_millis: u64,
}

impl SubagentLimits {
    /// One at a time is a real setting — serial subagents still isolate context,
    /// which is half of what they are for.
    pub const MIN_CONCURRENT: u8 = 1;
    /// Beyond this the bound stops being about the machine and starts being
    /// about the vendor's rate limit, which answers faster and less politely.
    pub const MAX_CONCURRENT: u8 = 16;
    /// A minute. Below this a subagent doing real work times out mid-thought
    /// and the parent pays for the tokens without getting the answer.
    pub const MIN_TIMEOUT_MILLIS: u64 = 60_000;
    /// An hour, for the same reason `PluginTimeouts` stops there: a longer
    /// budget is indistinguishable from none.
    pub const MAX_TIMEOUT_MILLIS: u64 = 3_600_000;

    /// Validate a concurrency bound.
    pub fn check_concurrent(value: u8) -> Result<u8, String> {
        if (Self::MIN_CONCURRENT..=Self::MAX_CONCURRENT).contains(&value) {
            Ok(value)
        } else {
            Err(format!(
                "subagents.max_concurrent must be between {} and {}, got {value}",
                Self::MIN_CONCURRENT,
                Self::MAX_CONCURRENT
            ))
        }
    }

    /// Validate a per-subagent budget, in milliseconds.
    pub fn check_timeout(value: u64) -> Result<u64, String> {
        if (Self::MIN_TIMEOUT_MILLIS..=Self::MAX_TIMEOUT_MILLIS).contains(&value) {
            Ok(value)
        } else {
            Err(format!(
                "subagents.timeout_millis must be between {} and {} milliseconds, got {value}",
                Self::MIN_TIMEOUT_MILLIS,
                Self::MAX_TIMEOUT_MILLIS
            ))
        }
    }

    #[must_use]
    pub fn timeout(self) -> std::time::Duration {
        std::time::Duration::from_millis(self.timeout_millis)
    }
}

impl Default for SubagentLimits {
    /// Three at once, ten minutes each. Three is what fits a single screen of
    /// reported results and what most vendors' concurrent-request allowances
    /// tolerate without shaping; ten minutes is longer than any search-shaped
    /// task and shorter than a person's patience.
    fn default() -> Self {
        Self {
            max_concurrent: 3,
            timeout_millis: 600_000,
        }
    }
}
