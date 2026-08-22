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

/// How much the harness may do without asking.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ApprovalPolicy {
    /// Ask before anything with an effect outside the workspace.
    #[default]
    OnRequest,
    /// Ask only when a command fails and wants to escalate.
    OnFailure,
    /// Never ask. Intended for CI, not for interactive use.
    Never,
}

/// How tightly spawned processes are confined.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
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
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
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
}

/// The wire format a declared provider speaks.
///
/// Mirrors `keke_provider_api::WireApi`, restated here so `keke-config-types`
/// need not depend on the provider contract — a config value type must not drag
/// in the runtime that consumes it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
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
