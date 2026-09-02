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

/// What kind of turn the session is running.
///
/// Distinct from [`ApprovalPolicy`], which says *when to ask*. This says *what
/// the agent is for* right now: in [`SessionMode::Plan`] the agent is
/// researching and writing a proposal, so edits are refused outright rather
/// than queued behind a prompt a person could wave through. The two compose —
/// a session can be in plan mode under any policy, and always-approve stays
/// armed underneath for everything plan mode does not block.
///
/// Carried on the seam rather than in startup configuration because a person
/// switches it mid-conversation, about the work in front of them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionMode {
    /// No plan-mode constraints.
    #[default]
    Default,
    /// Read-only except for the plan file, until a person approves the plan.
    Plan,
}

impl SessionMode {
    /// The wire spelling, for anywhere outside a `serde` path — the session log
    /// included, which is written and read by `keke-core` through
    /// `keke-protocol`, a crate ranked below this one that cannot name this
    /// type directly.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Plan => "plan",
        }
    }

    /// The inverse of [`Self::as_str`]. `None` for anything else, including a
    /// spelling from a future build this one does not know — a mode keke cannot
    /// enforce must not be silently treated as one it can.
    #[must_use]
    pub fn parse(wire: &str) -> Option<Self> {
        match wire {
            "default" => Some(Self::Default),
            "plan" => Some(Self::Plan),
            _ => None,
        }
    }

    #[must_use]
    pub fn is_plan(self) -> bool {
        matches!(self, Self::Plan)
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
///
/// A declaration is an *instance*, not a vendor. [`Self::kind`] names which
/// implementation serves it, and the table key names this particular
/// configuration of it — so one vendor can appear twice, at two addresses, on
/// two accounts, without either instance being the other's special case.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub struct ProviderDeclaration {
    /// The route key, which is what `--provider` and `keke login` name. Comes
    /// from the table key in configuration, not from a field.
    #[serde(skip)]
    pub route: String,
    /// Which compiled-in implementation serves this instance — `"grok"`,
    /// `"codex"`, `"ollama"`. Unset means the generic wire provider, which is
    /// what a plain endpoint needs and what every declaration meant before
    /// instances existed.
    ///
    /// A kind is what lets `[providers.xai]` and `[providers.grok]` both be
    /// served by the xAI plugin while differing in address and credential. The
    /// set of names lives in the composition root, since it is the only place
    /// allowed to know a vendor exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// Which stored account this instance authenticates as, for a kind whose
    /// credential file holds more than one. Unset means whichever the file
    /// records as active.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
    /// Shown in surfaces; defaults to the route.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// Where this instance sends requests. Optional only when [`Self::kind`] is
    /// set, since a compiled-in implementation knows its vendor's own address;
    /// a declaration with neither is rejected rather than pointed somewhere
    /// guessed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    /// Which inference format the endpoint speaks. Unset lets the kind decide,
    /// which matters because one vendor's two addresses do not agree: xAI's
    /// subscription proxy speaks `responses` where its public API speaks
    /// `chat_completions`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wire: Option<DeclaredWireApi>,
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
    /// Whether this instance offers the vendor's own web search, and on what
    /// terms. Unset means it does not — see [`WebSearchConfig`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub web_search: Option<WebSearchConfig>,
    /// Which service tier this instance's requests are routed at. Unset
    /// leaves the routing to the endpoint's own default for the model, which
    /// is not the same as asking for the standard tier — see [`ServiceTier`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service_tier: Option<ServiceTier>,
}

/// Re-exported so a config file naming a tier and a request carrying one name
/// the same type — see [`keke_protocol::ServiceTier`].
pub use keke_protocol::ServiceTier;

/// How much of the web a vendor-hosted search may reach.
///
/// Not a boolean because the access levels are not degrees of the same thing: a
/// deployment that may not make live outbound fetches on a person's behalf can
/// still answer from an index the vendor already holds, and one that permits
/// live fetches may still want them confined to pages the vendor has indexed.
/// Collapsing the three would leave the strictest deployments with nothing.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WebSearchMode {
    /// No search tool is offered at all.
    ///
    /// The default, unlike upstream codex: a hosted tool is a request the
    /// vendor executes and bills without the harness seeing it, so it is opted
    /// into rather than out of.
    #[default]
    Disabled,
    /// Only what the vendor has already indexed. No live fetches.
    Cached,
    /// Live fetches, restricted to indexed URLs.
    Indexed,
    /// Live fetches, unrestricted.
    Live,
}

impl WebSearchMode {
    /// The wire spelling, for anywhere outside a `serde` path.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Disabled => "disabled",
            Self::Cached => "cached",
            Self::Indexed => "indexed",
            Self::Live => "live",
        }
    }

    /// The inverse of [`Self::as_str`]. `None` for anything else, including a
    /// spelling from a future build — a mode this build cannot enforce must
    /// never be read as one it can.
    #[must_use]
    pub fn parse(wire: &str) -> Option<Self> {
        match wire {
            "disabled" => Some(Self::Disabled),
            "cached" => Some(Self::Cached),
            "indexed" => Some(Self::Indexed),
            "live" => Some(Self::Live),
            _ => None,
        }
    }

    /// The access permitted by both this mode and `requested`.
    ///
    /// Narrowing only, in the spirit of the approval seam: composing two
    /// opinions about how far a search may reach can restrict it and can never
    /// widen it, so no ordering of them grants access neither one allowed.
    #[must_use]
    pub fn restrict_to(self, requested: Self) -> Self {
        match (self, requested) {
            (Self::Disabled, _) | (_, Self::Disabled) => Self::Disabled,
            (Self::Cached, _) | (_, Self::Cached) => Self::Cached,
            (Self::Indexed, _) | (_, Self::Indexed) => Self::Indexed,
            (Self::Live, Self::Live) => Self::Live,
        }
    }

    #[must_use]
    pub fn is_enabled(self) -> bool {
        !matches!(self, Self::Disabled)
    }
}

/// How much search context a hosted search pulls into the turn.
///
/// More context is a better answer and a larger bill, and which trade a
/// deployment wants is exactly the kind of choice that does not belong in a
/// plugin constant.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WebSearchContextSize {
    Low,
    #[default]
    Medium,
    High,
}

impl WebSearchContextSize {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        }
    }
}

/// Roughly where the person searching is, which localizes results.
///
/// Every field is optional and none is inferred: a location keke guessed from
/// the machine's timezone would be sent to the vendor with every search without
/// anyone having asked for it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub struct WebSearchLocation {
    /// Two-letter ISO country code, e.g. `US`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub country: Option<String>,
    /// Region or state, e.g. `California`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub city: Option<String>,
    /// IANA timezone, e.g. `America/Los_Angeles`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timezone: Option<String>,
}

impl WebSearchLocation {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.country.is_none()
            && self.region.is_none()
            && self.city.is_none()
            && self.timezone.is_none()
    }
}

/// A provider instance's hosted web search.
///
/// Hosted means the vendor runs the search itself, inside the model call: no
/// tool call reaches the harness, so neither the approval seam nor a
/// [`ToolGuard`](../keke_tool/index.html) sees it. That is why the mode
/// defaults to [`WebSearchMode::Disabled`] and why the domain lists are here —
/// a deployment that may only consult approved sources cannot express that
/// anywhere else once the search is the vendor's to run.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub struct WebSearchConfig {
    #[serde(default)]
    pub mode: WebSearchMode,
    #[serde(default)]
    pub context_size: WebSearchContextSize,
    /// Domains results are confined to. Empty means the whole web.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed_domains: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_location: Option<WebSearchLocation>,
    /// Whether the search may return images as well as text. Costs context, so
    /// it is stated rather than assumed.
    #[serde(default)]
    pub include_images: bool,
    /// Which model runs the search, when it should not be the session's own.
    ///
    /// The search is a second, self-contained model call — a query in, a
    /// summary and its sources out — so the model that answers it need not be
    /// the one holding the conversation, and paying conversation rates to
    /// summarize five search results is a deployment's money to save. Unset
    /// means the session's model does it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

impl WebSearchConfig {
    /// A config that offers search at `mode` and nothing else set.
    #[must_use]
    pub fn enabled(mode: WebSearchMode) -> Self {
        Self {
            mode,
            ..Self::default()
        }
    }

    /// Reject a config whose settings cannot take effect, naming why.
    ///
    /// A restriction that is silently ignored is worse than one that is
    /// refused: someone who wrote `allowed_domains` and left the mode unset
    /// would otherwise read the file as "search, confined to these domains"
    /// when it means "no search at all", and would find out which it was from
    /// the bill.
    pub fn check(&self) -> Result<(), String> {
        if !self.mode.is_enabled() {
            if !self.allowed_domains.is_empty() {
                return Err(
                    "web_search.allowed_domains is set but web_search.mode is disabled".to_string(),
                );
            }
            if self.user_location.as_ref().is_some_and(|l| !l.is_empty()) {
                return Err(
                    "web_search.user_location is set but web_search.mode is disabled".to_string(),
                );
            }
            if self.model.is_some() {
                return Err("web_search.model is set but web_search.mode is disabled".to_string());
            }
        }
        if self
            .model
            .as_ref()
            .is_some_and(|model| model.trim().is_empty())
        {
            return Err("web_search.model is empty; remove it or name a model".to_string());
        }
        for domain in &self.allowed_domains {
            let domain = domain.trim();
            if domain.is_empty() {
                return Err("web_search.allowed_domains contains an empty entry".to_string());
            }
            if domain.contains('/') || domain.contains(' ') {
                return Err(format!(
                    "web_search.allowed_domains takes hostnames, not URLs or paths: {domain}"
                ));
            }
        }
        Ok(())
    }
}

/// A provider (and optionally a model) chosen by where the session is running.
///
/// Which account someone wants is almost never a property of the invocation and
/// almost always a property of the repository: a work checkout wants the work
/// instance, a personal one wants the personal instance. Expressing that as
/// configuration is what removes `--provider` from every command, in the same
/// spirit as git's `includeIf gitdir:`.
///
/// The pattern is matched against the session's workspace root, so an override
/// follows the repository rather than the shell's current subdirectory.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub struct DirectoryOverride {
    /// The glob the workspace root is matched against. `match` is a Rust
    /// keyword, hence the rename; the configuration spelling is what a person
    /// reads, so the file keeps the shorter word.
    #[serde(rename = "match")]
    pub pattern: String,
    /// The provider route to use in matching directories. Must name a
    /// registered route: an override pointing at a provider that does not exist
    /// fails loud rather than leaving a person on an account they did not pick.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// The model to use in matching directories.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

impl DirectoryOverride {
    /// Reject an entry that cannot do anything, naming why.
    ///
    /// An override stating neither a provider nor a model is almost certainly a
    /// half-finished edit, and silently applying nothing is the failure mode
    /// that costs an afternoon.
    pub fn check(&self) -> Result<(), String> {
        if self.pattern.trim().is_empty() {
            return Err("dir.match must not be empty".to_string());
        }
        if self.provider.as_ref().is_some_and(|route| route.is_empty()) {
            return Err(format!(
                "dir.provider must not be empty (match = \"{}\")",
                self.pattern
            ));
        }
        if self.provider.is_none() && self.model.is_none() {
            return Err(format!(
                "dir entry for match = \"{}\" sets neither provider nor model",
                self.pattern
            ));
        }
        Ok(())
    }

    /// Whether this entry applies to `directory`.
    ///
    /// `home` expands a leading `~`; passing `None` leaves such a pattern
    /// unexpanded, which simply will not match an absolute path.
    #[must_use]
    pub fn matches(&self, directory: &std::path::Path, home: Option<&std::path::Path>) -> bool {
        let pattern = match self.pattern.strip_prefix('~') {
            Some(rest) => {
                let Some(home) = home else { return false };
                let rest = rest.trim_start_matches(['/', '\\']);
                let mut expanded = normalize(home);
                if !rest.is_empty() {
                    expanded.push('/');
                    expanded.push_str(&rest.replace('\\', "/"));
                }
                expanded
            }
            None => self.pattern.replace('\\', "/"),
        };
        glob_match(&pattern, &normalize(directory))
    }
}

/// Paths are compared as `/`-separated text, so one pattern spelling works on
/// every platform and Windows' separator is not mistaken for an escape.
fn normalize(path: &std::path::Path) -> String {
    let text = path.to_string_lossy().replace('\\', "/");
    let trimmed = text.trim_end_matches('/');
    if trimmed.is_empty() {
        text.to_string()
    } else {
        trimmed.to_string()
    }
}

/// A deliberately small glob: `?` is one character, `*` is any run within one
/// path segment, and a `**` segment is any run of segments including none.
///
/// Small enough to keep this crate dependency-light, which is invariant 3, and
/// large enough for what these patterns are actually for — naming a directory
/// and everything under it. `~/work/**` therefore matches `~/work` itself as
/// well as everything below it, because a person who wrote that meant the tree.
fn glob_match(pattern: &str, path: &str) -> bool {
    let pattern: Vec<&str> = pattern.split('/').collect();
    let path: Vec<&str> = path.split('/').collect();
    segments_match(&pattern, &path)
}

fn segments_match(pattern: &[&str], path: &[&str]) -> bool {
    match pattern.split_first() {
        None => path.is_empty(),
        Some((&"**", rest)) => {
            // Try consuming zero, then one, then more path segments.
            (0..=path.len()).any(|taken| segments_match(rest, &path[taken..]))
        }
        Some((head, rest)) => match path.split_first() {
            Some((first, tail)) if segment_match(head, first) => segments_match(rest, tail),
            _ => false,
        },
    }
}

/// `*` and `?` within one segment, matched greedily with backtracking.
fn segment_match(pattern: &str, segment: &str) -> bool {
    let pattern: Vec<char> = pattern.chars().collect();
    let segment: Vec<char> = segment.chars().collect();
    let (mut p, mut s) = (0, 0);
    let (mut star, mut resume) = (None, 0);
    while s < segment.len() {
        match pattern.get(p) {
            Some('*') => {
                star = Some(p);
                resume = s;
                p += 1;
            }
            Some('?') => {
                p += 1;
                s += 1;
            }
            Some(&literal) if literal == segment[s] => {
                p += 1;
                s += 1;
            }
            _ => match star {
                Some(index) => {
                    p = index + 1;
                    resume += 1;
                    s = resume;
                }
                None => return false,
            },
        }
    }
    pattern[p..].iter().all(|&character| character == '*')
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

/// Whether keke keeps a snapshot of the working tree per turn, so a rewind can
/// put the files back and not only the conversation.
///
/// A validated field rather than a constant in the engine because a deployment
/// really does decide this: the snapshots live under `$KEKE_HOME` and cost a
/// staged git index per turn that writes, which is nothing on a normal project
/// and is not nothing on a very large one. Somebody working in a tree they
/// already have their own discipline around must be able to say no.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckpointConfig {
    /// Off means no snapshot is ever taken, and a rewind then offers to wind
    /// the conversation back and says plainly that the files cannot follow.
    pub enabled: bool,
}

impl Default for CheckpointConfig {
    /// On: a person who winds a conversation back and finds the files still
    /// changed has been given half an undo, and the half that is missing is
    /// the one that touched their disk.
    fn default() -> Self {
        Self { enabled: true }
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

/// Which of the skills a plugin ships this deployment actually wants.
///
/// A skill costs context every turn — its index line is in every request — and
/// costs a name in the slash namespace. Whether a particular one earns that is
/// exactly a deployment's call, so it is a validated field here rather than
/// something a plugin decides for everyone who installs it (`AGENTS.md`
/// invariant 9).
///
/// Disabling is total: a disabled skill is not listed for the model, is not
/// offered as a command, and cannot be read by name. A skill a person can
/// still reach after turning it off has not been turned off.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillSelection {
    /// Skills to leave out, each written as `plugin:name`, as a bare `name`
    /// matching that skill in every plugin, or as `plugin:*` for all of one
    /// plugin's.
    disabled: Vec<String>,
}

impl SkillSelection {
    /// Build a selection, refusing a pattern that names nothing.
    ///
    /// An empty entry is a typo or a half-written line, never a request to
    /// disable everything: silently reading it as "match all" would turn a
    /// stray comma into a session with no skills at all (`AGENTS.md`
    /// invariant 8).
    pub fn new(disabled: Vec<String>) -> Result<Self, String> {
        for pattern in &disabled {
            if pattern.trim().is_empty() {
                return Err(
                    "skills.disabled has an empty entry; write plugin:name, name, or plugin:*"
                        .to_string(),
                );
            }
            if let Some((plugin, name)) = pattern.split_once(':')
                && (plugin.trim().is_empty() || name.trim().is_empty())
            {
                return Err(format!(
                    "skills.disabled entry {pattern:?} is missing a side of its `plugin:name`"
                ));
            }
        }
        Ok(Self { disabled })
    }

    /// Whether `plugin:name` is one a person asked not to have.
    #[must_use]
    pub fn is_disabled(&self, plugin: &str, name: &str) -> bool {
        self.disabled.iter().any(|pattern| {
            let pattern = pattern.trim();
            match pattern.split_once(':') {
                Some((wanted_plugin, wanted_name)) => {
                    wanted_plugin.trim() == plugin
                        && (wanted_name.trim() == "*" || wanted_name.trim() == name)
                }
                // A bare name is the form a person types after reading the
                // slash menu, where the plugin is not on screen.
                None => pattern == name,
            }
        })
    }

    /// The patterns as configured, for `keke doctor` and for round-tripping a
    /// config file without inventing entries.
    #[must_use]
    pub fn patterns(&self) -> &[String] {
        &self.disabled
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

/// Bounds on the shell commands a session may leave running in the background.
///
/// Deployment-varying in the way invariant 9 in `AGENTS.md` means: how many
/// long-lived children a machine tolerates, and how much of a chatty dev
/// server's output is worth keeping in memory, are answered differently by a
/// laptop and by a CI runner.
///
/// There is no "unbounded output" setting. A background task's buffer is the
/// one thing in a session that grows without anyone asking it to, so the cap
/// is a number rather than an option — a dev server left running overnight
/// must not be able to end the session by filling it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackgroundLimits {
    /// How many background commands may run at once. Unlike a subagent spawn,
    /// one past the limit is refused rather than queued: the model asked to
    /// start something and carry on, and a start that silently waits is the
    /// opposite of what it asked for.
    pub max_concurrent: u8,
    /// How much of one task's output is kept. Oldest bytes are dropped first —
    /// what a person or a model wants from a long-running command is the tail.
    pub output_bytes: u64,
    /// How long a kill waits after SIGTERM before sending SIGKILL. A child
    /// with cleanup to do gets this long to do it.
    pub kill_grace_millis: u64,
}

impl BackgroundLimits {
    /// One at a time is a real setting: a dev server and nothing else is a
    /// perfectly ordinary way to work.
    pub const MIN_CONCURRENT: u8 = 1;
    /// Past this the limit stops being about the machine and starts being
    /// about how many process trees a person can still reason about.
    pub const MAX_CONCURRENT: u8 = 32;
    /// Under 4 KiB the tail is too short to hold a stack trace, which is the
    /// thing most often being fished out of a background task.
    pub const MIN_OUTPUT_BYTES: u64 = 4 * 1024;
    /// 8 MiB per task. Above this the buffer is no longer a tail, and the
    /// model could not be shown it in one piece anyway.
    pub const MAX_OUTPUT_BYTES: u64 = 8 * 1024 * 1024;
    /// Immediate is allowed: a grace period is a courtesy to the child, and a
    /// deployment that knows its children have no cleanup may skip it.
    pub const MAX_KILL_GRACE_MILLIS: u64 = 30_000;

    /// Validate a concurrency bound.
    pub fn check_concurrent(value: u8) -> Result<u8, String> {
        if (Self::MIN_CONCURRENT..=Self::MAX_CONCURRENT).contains(&value) {
            Ok(value)
        } else {
            Err(format!(
                "background.max_concurrent must be between {} and {}, got {value}",
                Self::MIN_CONCURRENT,
                Self::MAX_CONCURRENT
            ))
        }
    }

    /// Validate a per-task output cap, in bytes.
    pub fn check_output_bytes(value: u64) -> Result<u64, String> {
        if (Self::MIN_OUTPUT_BYTES..=Self::MAX_OUTPUT_BYTES).contains(&value) {
            Ok(value)
        } else {
            Err(format!(
                "background.output_bytes must be between {} and {}, got {value}",
                Self::MIN_OUTPUT_BYTES,
                Self::MAX_OUTPUT_BYTES
            ))
        }
    }

    /// Validate the grace period between SIGTERM and SIGKILL.
    pub fn check_kill_grace(value: u64) -> Result<u64, String> {
        if value <= Self::MAX_KILL_GRACE_MILLIS {
            Ok(value)
        } else {
            Err(format!(
                "background.kill_grace_millis must be at most {}, got {value}",
                Self::MAX_KILL_GRACE_MILLIS
            ))
        }
    }

    #[must_use]
    pub fn kill_grace(self) -> std::time::Duration {
        std::time::Duration::from_millis(self.kill_grace_millis)
    }
}

impl Default for BackgroundLimits {
    /// Eight at once, 256 KiB of tail each, two seconds to shut down. Eight is
    /// more servers and watchers than a single workspace usually has; 256 KiB
    /// holds a long test run's tail without being something a session notices
    /// carrying.
    fn default() -> Self {
        Self {
            max_concurrent: 8,
            output_bytes: 256 * 1024,
            kill_grace_millis: 2_000,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    /// Composing two opinions about how far a search may reach can only narrow
    /// it, so no ordering of them grants access neither one allowed.
    #[test]
    fn a_permissive_search_mode_cannot_widen_a_restrictive_one() {
        use WebSearchMode::Cached;
        use WebSearchMode::Disabled;
        use WebSearchMode::Indexed;
        use WebSearchMode::Live;

        for (a, b) in [
            (Disabled, Live),
            (Cached, Live),
            (Indexed, Live),
            (Cached, Indexed),
        ] {
            assert_eq!(a.restrict_to(b), a, "{a:?} widened by {b:?}");
            assert_eq!(b.restrict_to(a), a, "{a:?} widened by {b:?}, reversed");
        }
        assert_eq!(Live.restrict_to(Live), Live);
    }

    /// Someone who wrote a domain list and left the mode unset meant "search,
    /// confined to these" — telling them it means "no search" is cheaper than
    /// letting them find out from the bill.
    #[test]
    fn a_restriction_that_cannot_take_effect_is_refused() {
        let config = WebSearchConfig {
            allowed_domains: vec!["docs.rs".to_string()],
            ..WebSearchConfig::default()
        };
        assert!(config.check().is_err());

        let enabled = WebSearchConfig {
            mode: WebSearchMode::Live,
            ..config
        };
        assert!(enabled.check().is_ok());
    }

    /// The filter takes hostnames; a URL written there matches nothing and
    /// silently lifts the restriction it was meant to impose.
    #[test]
    fn a_url_in_the_domain_list_is_refused() {
        let config = WebSearchConfig {
            mode: WebSearchMode::Live,
            allowed_domains: vec!["https://docs.rs/serde".to_string()],
            ..WebSearchConfig::default()
        };
        assert!(config.check().is_err());
    }

    fn entry(pattern: &str) -> DirectoryOverride {
        DirectoryOverride {
            pattern: pattern.to_string(),
            provider: Some("grok-work".to_string()),
            model: None,
        }
    }

    /// A leading `~` names the person's home directory, as it does everywhere
    /// else they type a path.
    #[test]
    fn a_tilde_pattern_matches_below_the_home_directory() {
        let home = Path::new("/home/ada");
        assert!(entry("~/work/**").matches(Path::new("/home/ada/work/api"), Some(home)));
        assert!(!entry("~/work/**").matches(Path::new("/home/ada/oss/keke"), Some(home)));
    }

    /// Someone who wrote `~/work/**` meant the tree, the root of it included.
    #[test]
    fn a_trailing_double_star_matches_the_directory_itself() {
        let home = Path::new("/home/ada");
        assert!(entry("~/work/**").matches(Path::new("/home/ada/work"), Some(home)));
    }

    /// A single `*` stays inside one path segment, so a broad rule cannot
    /// reach into a tree the person did not name.
    #[test]
    fn a_single_star_does_not_cross_a_path_separator() {
        assert!(entry("/srv/*").matches(Path::new("/srv/api"), None));
        assert!(!entry("/srv/*").matches(Path::new("/srv/api/nested"), None));
    }

    #[test]
    fn an_entry_stating_nothing_is_rejected() {
        let empty = DirectoryOverride {
            pattern: "~/work/**".to_string(),
            provider: None,
            model: None,
        };
        assert!(empty.check().is_err());
        assert!(entry("~/work/**").check().is_ok());
    }
}
