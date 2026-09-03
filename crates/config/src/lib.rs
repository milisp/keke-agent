//! Configuration loading.
//!
//! Settings come from three layers, later ones overriding earlier:
//! managed (deployment policy), user (`$KEKE_HOME/config.toml`), and project
//! (`.keke/config.toml` beside the workspace root). On top of the merged
//! result — and beneath any flag typed on the command line — come the
//! `[[dir]]` entries, which pick a provider and model by which repository the
//! session is in. Splitting the *values*
//! into `keke-config-types` and the *loading* here is what lets a plugin name a
//! setting without depending on how settings reach disk.
//!
//! Misconfiguration fails at load, naming the file and the field. A config
//! error discovered three turns into a session is a config error discovered too
//! late.

mod layer;
mod resolve;

pub use layer::ConfigLayer;
pub use layer::LayerSource;
pub use resolve::keke_home;
pub use resolve::resolve_workspace_root;

use std::path::Path;

use keke_config_types::ApprovalPolicy;
use keke_config_types::BackgroundLimits;
use keke_config_types::CheckpointConfig;
use keke_config_types::CompactionConfig;
use keke_config_types::DirectoryOverride;
use keke_config_types::HomeLayout;
use keke_config_types::MaxOutputTokens;
use keke_config_types::ModelCatalogTtl;
use keke_config_types::ModelSelection;
use keke_config_types::PluginTimeouts;
use keke_config_types::ProviderDeclaration;
use keke_config_types::ReasoningEffort;
use keke_config_types::SandboxMode;
use keke_config_types::SkillSelection;
use keke_config_types::SubagentLimits;
use keke_paths::AbsPath;
use serde::Deserialize;
use serde::Serialize;
use std::collections::BTreeMap;

/// Why configuration could not be loaded.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("{path}: {message}")]
    Parse { path: String, message: String },
    #[error("{path}: {message}")]
    Invalid { path: String, message: String },
    #[error("could not read {path}: {source}")]
    Read {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("could not determine {0}")]
    Unresolvable(String),
    #[error(transparent)]
    Path(#[from] keke_paths::PathError),
}

/// The effective configuration for a session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    pub home: HomeLayout,
    pub model: ModelSelection,
    pub approval_policy: ApprovalPolicy,
    /// Whether leaving plan mode needs a person's answer even where the
    /// approval policy would not ask. Off by default, matching the policy: a
    /// deployment that turned approvals off has said it does not want to be
    /// asked, and plan mode still refuses edits either way — only the exit
    /// stops being a question.
    pub require_plan_approval: bool,
    pub sandbox_mode: SandboxMode,
    pub max_output_tokens: MaxOutputTokens,
    /// How hard the model is asked to think. `None` leaves each vendor's own
    /// default in place, which is not the same as asking for the least
    /// thinking on offer.
    pub reasoning_effort: Option<ReasoningEffort>,
    pub compaction: CompactionConfig,
    pub checkpoints: CheckpointConfig,
    /// Budgets for plugin-supplied programs.
    pub plugins: PluginTimeouts,
    /// Bounds on the subagents a session may run at once.
    pub subagents: SubagentLimits,
    /// Bounds on the shell commands a session may leave running.
    pub background: BackgroundLimits,
    /// Which plugin-contributed skills this deployment wants.
    pub skills: SkillSelection,
    /// How long a fetched model catalog stays usable before the vendor is
    /// asked again.
    pub model_catalog_ttl: ModelCatalogTtl,
    /// Endpoints declared from configuration, in addition to the compiled-in
    /// vendors.
    pub providers: Vec<ProviderDeclaration>,
    /// The directory override that applied, if one did. Kept so the composition
    /// root can check the route it names against the registry — and so `keke
    /// doctor` can say which pattern moved a person off their default.
    pub directory_override: Option<DirectoryOverride>,
    /// Which layers contributed, in application order. Kept so `keke doctor`
    /// can answer "where did this value come from" without re-reading disk.
    pub sources: Vec<LayerSource>,
}

/// The on-disk shape. Every field is optional: a layer states only what it
/// overrides, and the merge below is a field-wise override rather than a
/// whole-document replacement.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub struct ConfigFile {
    pub provider: Option<String>,
    pub model: Option<String>,
    pub approval_policy: Option<ApprovalPolicy>,
    pub require_plan_approval: Option<bool>,
    pub sandbox_mode: Option<SandboxMode>,
    pub max_output_tokens: Option<u32>,
    /// Read as a string rather than as the enum so a misspelled level names
    /// itself in the error, instead of arriving as serde's list of variants
    /// for a field the reader may not recognize.
    pub reasoning_effort: Option<String>,
    pub compaction: Option<CompactionFile>,
    pub checkpoints: Option<CheckpointFile>,
    pub plugins: Option<PluginsFile>,
    pub subagents: Option<SubagentsFile>,
    pub background: Option<BackgroundFile>,
    pub skills: Option<SkillsFile>,
    /// Seconds. `0` asks the vendor every time.
    pub model_catalog_ttl_seconds: Option<u64>,
    /// Extra endpoints, keyed by route: `[providers.nvidia]`. Accumulated
    /// across layers rather than replaced, so a project can add one without
    /// restating the user's.
    #[serde(default)]
    pub providers: BTreeMap<String, ProviderDeclaration>,
    /// Per-directory choices: `[[dir]]`. Accumulated across layers in the order
    /// the layers apply, and applied after all of them — see
    /// [`Config::from_layers`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dir: Vec<DirectoryOverride>,
}

/// The checkpoint section, separated for the same reason the compaction one is.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub struct CheckpointFile {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keep: Option<usize>,
}

/// The compaction section, separated so a layer can override one field of it.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub struct CompactionFile {
    pub trigger_percent: Option<u8>,
    pub keep_recent_messages: Option<usize>,
    pub context_window: Option<u32>,
}

/// The plugins section, separated so a layer can override one field of it.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub struct PluginsFile {
    pub hook_timeout_millis: Option<u64>,
    pub mcp_startup_timeout_millis: Option<u64>,
    pub mcp_call_timeout_millis: Option<u64>,
}

/// The skills section: which of the discovered skills a person wants.
///
/// `disabled` accumulates across layers rather than replacing, so a project can
/// turn one off without restating the user's list — the same rule `providers`
/// and `dir` follow, and the one that matches what disabling means. A layer
/// cannot re-enable what a broader one refused, which is denial staying
/// monotonic.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub struct SkillsFile {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub disabled: Vec<String>,
}

/// The subagents section, separated so a layer can override one field of it.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub struct SubagentsFile {
    pub max_concurrent: Option<u8>,
    pub timeout_millis: Option<u64>,
}

/// The background-command section, separated for the same reason.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub struct BackgroundFile {
    pub max_concurrent: Option<u8>,
    pub output_bytes: Option<u64>,
    pub kill_grace_millis: Option<u64>,
}

/// Values applied when no layer states them.
///
/// These are defaults, not policy: every one of them is overridable from a
/// config file, which is what invariant 9 in `AGENTS.md` requires.
///
/// There is deliberately no default *model*. A model id compiled in here is a
/// guess about a vendor's catalog on the day the binary was built, and it goes
/// stale silently: the name keeps being sent long after the vendor renamed or
/// retired it, and what a person sees is a rejected request rather than a
/// wrong constant. Unset means "ask the provider", which the composition root
/// does through `/models` before a session opens.
const DEFAULT_PROVIDER: &str = "anthropic";

impl Config {
    /// Load and merge every layer for `workspace_root`.
    pub fn load(workspace_root: &Path) -> Result<Self, ConfigError> {
        let workspace_root = AbsPath::new(workspace_root)?;
        let home = keke_home()?;
        let layers = ConfigLayer::discover(&home, &workspace_root)?;
        Self::from_layers(
            HomeLayout {
                home,
                workspace_root,
            },
            &layers,
        )
    }

    /// Merge pre-read layers. Separated from [`Config::load`] so tests and
    /// `--dump-config` compose the identical merge without touching disk.
    pub fn from_layers(home: HomeLayout, layers: &[ConfigLayer]) -> Result<Self, ConfigError> {
        let mut merged = ConfigFile::default();
        let mut sources = Vec::new();

        for layer in layers {
            merged.provider = layer.file.provider.clone().or(merged.provider);
            merged.model = layer.file.model.clone().or(merged.model);
            merged.approval_policy = layer.file.approval_policy.or(merged.approval_policy);
            merged.require_plan_approval = layer
                .file
                .require_plan_approval
                .or(merged.require_plan_approval);
            merged.sandbox_mode = layer.file.sandbox_mode.or(merged.sandbox_mode);
            merged.max_output_tokens = layer.file.max_output_tokens.or(merged.max_output_tokens);
            merged.reasoning_effort = layer
                .file
                .reasoning_effort
                .clone()
                .or(merged.reasoning_effort);
            merged.model_catalog_ttl_seconds = layer
                .file
                .model_catalog_ttl_seconds
                .or(merged.model_catalog_ttl_seconds);

            if let Some(compaction) = layer.file.compaction {
                let base = merged
                    .compaction
                    .get_or_insert_with(CompactionFile::default);
                base.trigger_percent = compaction.trigger_percent.or(base.trigger_percent);
                base.keep_recent_messages = compaction
                    .keep_recent_messages
                    .or(base.keep_recent_messages);
                base.context_window = compaction.context_window.or(base.context_window);
            }
            if let Some(checkpoints) = layer.file.checkpoints {
                let base = merged
                    .checkpoints
                    .get_or_insert_with(CheckpointFile::default);
                base.enabled = checkpoints.enabled.or(base.enabled);
                base.keep = checkpoints.keep.or(base.keep);
            }
            if let Some(plugins) = layer.file.plugins {
                let base = merged.plugins.get_or_insert_with(PluginsFile::default);
                base.hook_timeout_millis = plugins.hook_timeout_millis.or(base.hook_timeout_millis);
                base.mcp_startup_timeout_millis = plugins
                    .mcp_startup_timeout_millis
                    .or(base.mcp_startup_timeout_millis);
                base.mcp_call_timeout_millis = plugins
                    .mcp_call_timeout_millis
                    .or(base.mcp_call_timeout_millis);
            }
            if let Some(skills) = &layer.file.skills {
                let base = merged.skills.get_or_insert_with(SkillsFile::default);
                base.disabled.extend(skills.disabled.iter().cloned());
            }
            if let Some(background) = layer.file.background {
                let base = merged
                    .background
                    .get_or_insert_with(BackgroundFile::default);
                base.max_concurrent = background.max_concurrent.or(base.max_concurrent);
                base.output_bytes = background.output_bytes.or(base.output_bytes);
                base.kill_grace_millis = background.kill_grace_millis.or(base.kill_grace_millis);
            }
            if let Some(subagents) = layer.file.subagents {
                let base = merged.subagents.get_or_insert_with(SubagentsFile::default);
                base.max_concurrent = subagents.max_concurrent.or(base.max_concurrent);
                base.timeout_millis = subagents.timeout_millis.or(base.timeout_millis);
            }
            // Declarations accumulate; a later layer redeclaring a route
            // replaces that one entry rather than the whole set.
            for (route, declaration) in &layer.file.providers {
                merged.providers.insert(route.clone(), declaration.clone());
            }
            // Appended rather than replaced, for the same reason declarations
            // accumulate: a project may add a rule without restating the
            // user's. Order is layer order, and the last match wins.
            merged.dir.extend(layer.file.dir.iter().cloned());
            sources.push(layer.source.clone());
        }

        let compaction_file = merged.compaction.unwrap_or_default();
        let compaction = CompactionConfig {
            trigger_percent: compaction_file
                .trigger_percent
                .unwrap_or(CompactionConfig::default().trigger_percent),
            keep_recent_messages: compaction_file
                .keep_recent_messages
                .unwrap_or(CompactionConfig::default().keep_recent_messages),
            context_window: compaction_file
                .context_window
                .unwrap_or(CompactionConfig::default().context_window),
        };

        let checkpoint_file = merged.checkpoints.unwrap_or_default();
        let checkpoints = CheckpointConfig {
            enabled: checkpoint_file
                .enabled
                .unwrap_or(CheckpointConfig::default().enabled),
            keep: checkpoint_file
                .keep
                .unwrap_or(CheckpointConfig::default().keep),
        };

        // Zero would mean a store that prunes every snapshot the moment it
        // opens, which is checkpoints being off by another name — and off has
        // its own setting that says so plainly.
        if checkpoints.keep == 0 {
            return Err(ConfigError::Invalid {
                path: sources
                    .last()
                    .map(LayerSource::describe)
                    .unwrap_or_else(|| "<defaults>".to_string()),
                message: "checkpoints.keep must be at least 1; set checkpoints.enabled = false to turn snapshots off".to_string(),
            });
        }

        if !(1..=99).contains(&compaction.trigger_percent) {
            return Err(ConfigError::Invalid {
                path: sources
                    .last()
                    .map(LayerSource::describe)
                    .unwrap_or_else(|| "<defaults>".to_string()),
                message: format!(
                    "compaction.trigger-percent must be between 1 and 99, got {}",
                    compaction.trigger_percent
                ),
            });
        }

        let skills = SkillSelection::new(merged.skills.take().unwrap_or_default().disabled)
            .map_err(|message| ConfigError::Invalid {
                path: sources
                    .last()
                    .map(LayerSource::describe)
                    .unwrap_or_else(|| "<defaults>".to_string()),
                message,
            })?;

        let plugins_file = merged.plugins.unwrap_or_default();
        let defaults = PluginTimeouts::default();
        let invalid = |message: String| ConfigError::Invalid {
            path: sources
                .last()
                .map(LayerSource::describe)
                .unwrap_or_else(|| "<defaults>".to_string()),
            message,
        };
        let plugins = PluginTimeouts {
            hook_millis: match plugins_file.hook_timeout_millis {
                Some(value) => {
                    PluginTimeouts::check("hook-timeout-millis", value).map_err(invalid)?
                }
                None => defaults.hook_millis,
            },
            mcp_startup_millis: match plugins_file.mcp_startup_timeout_millis {
                Some(value) => {
                    PluginTimeouts::check("mcp-startup-timeout-millis", value).map_err(invalid)?
                }
                None => defaults.mcp_startup_millis,
            },
            mcp_call_millis: match plugins_file.mcp_call_timeout_millis {
                Some(value) => {
                    PluginTimeouts::check("mcp-call-timeout-millis", value).map_err(invalid)?
                }
                None => defaults.mcp_call_millis,
            },
        };

        let subagent_defaults = SubagentLimits::default();
        let subagents_file = merged.subagents.unwrap_or_default();
        let subagents = SubagentLimits {
            max_concurrent: match subagents_file.max_concurrent {
                Some(value) => SubagentLimits::check_concurrent(value).map_err(invalid)?,
                None => subagent_defaults.max_concurrent,
            },
            timeout_millis: match subagents_file.timeout_millis {
                Some(value) => SubagentLimits::check_timeout(value).map_err(invalid)?,
                None => subagent_defaults.timeout_millis,
            },
        };

        let background_defaults = BackgroundLimits::default();
        let background_file = merged.background.unwrap_or_default();
        let background = BackgroundLimits {
            max_concurrent: match background_file.max_concurrent {
                Some(value) => BackgroundLimits::check_concurrent(value).map_err(invalid)?,
                None => background_defaults.max_concurrent,
            },
            output_bytes: match background_file.output_bytes {
                Some(value) => BackgroundLimits::check_output_bytes(value).map_err(invalid)?,
                None => background_defaults.output_bytes,
            },
            kill_grace_millis: match background_file.kill_grace_millis {
                Some(value) => BackgroundLimits::check_kill_grace(value).map_err(invalid)?,
                None => background_defaults.kill_grace_millis,
            },
        };

        let max_output_tokens = match merged.max_output_tokens {
            Some(value) => MaxOutputTokens::new(value).map_err(|message| ConfigError::Invalid {
                path: sources
                    .last()
                    .map(LayerSource::describe)
                    .unwrap_or_else(|| "<defaults>".to_string()),
                message,
            })?,
            None => MaxOutputTokens::default(),
        };

        let model_catalog_ttl = match merged.model_catalog_ttl_seconds {
            Some(value) => ModelCatalogTtl::new(value).map_err(invalid)?,
            None => ModelCatalogTtl::default(),
        };

        let reasoning_effort = match merged.reasoning_effort.as_deref() {
            Some(value) => Some(ReasoningEffort::parse(value).map_err(invalid)?),
            None => None,
        };

        let mut model = ModelSelection {
            provider: merged
                .provider
                .unwrap_or_else(|| DEFAULT_PROVIDER.to_string()),
            // Empty is "not chosen yet", resolved from what the provider
            // serves rather than from a constant that can only rot.
            model: merged.model.unwrap_or_default(),
        };

        for entry in &merged.dir {
            entry.check().map_err(invalid)?;
        }
        // A search restriction that cannot take effect is refused here rather
        // than dropped at the provider, where the only evidence of it would be
        // the vendor's bill.
        for (route, declaration) in &merged.providers {
            if let Some(web_search) = &declaration.web_search {
                web_search
                    .check()
                    .map_err(|error| invalid(format!("providers.{route}: {error}")))?;
            }
        }
        // Applied on top of the merged file layers and beneath the CLI flags the
        // composition root applies afterwards: where a person is standing is a
        // better answer than a global default, and a worse one than what they
        // just typed. When several entries match, the last one wins, so a
        // narrower rule is written below the broader rule it refines.
        let user_home = dirs::home_dir();
        let directory_override = merged
            .dir
            .iter()
            .rfind(|entry| entry.matches(home.workspace_root.as_path(), user_home.as_deref()))
            .cloned();
        if let Some(applied) = &directory_override {
            if let Some(provider) = &applied.provider {
                // A model carried over from the layers names a model on the
                // route that was in force before this override, which is a pair
                // no configuration chose. Dropping it lets the new route's own
                // default answer, exactly as `--provider` does.
                if *provider != model.provider {
                    model.model.clear();
                }
                model.provider = provider.clone();
            }
            if let Some(chosen) = &applied.model {
                model.model = chosen.clone();
            }
        }

        Ok(Self {
            home,
            model,
            approval_policy: merged.approval_policy.unwrap_or_default(),
            require_plan_approval: merged.require_plan_approval.unwrap_or(false),
            sandbox_mode: merged.sandbox_mode.unwrap_or_default(),
            max_output_tokens,
            reasoning_effort,
            compaction,
            checkpoints,
            plugins,
            subagents,
            background,
            skills,
            model_catalog_ttl,
            providers: merged
                .providers
                .into_iter()
                .map(|(route, declaration)| ProviderDeclaration {
                    route,
                    ..declaration
                })
                .collect(),
            directory_override,
            sources,
        })
    }
}

/// Update one field of `$KEKE_HOME/config.toml`, so a switch a person makes at
/// the keyboard (`/model`, `/mode`, `/effort`) survives past this process
/// without them hand-editing the file.
///
/// Reads the existing user layer first so an unrelated field — a declared
/// provider, plugin timeouts — is carried forward rather than dropped; a
/// missing file is treated as an empty one, since the first switch a fresh
/// install makes is exactly what should create it.
pub fn persist_user_override(
    home: &AbsPath,
    patch: impl FnOnce(&mut ConfigFile),
) -> Result<(), ConfigError> {
    let path = home.as_path().join("config.toml");
    let mut file = match std::fs::read_to_string(&path) {
        Ok(text) => toml::from_str(&text).map_err(|error| ConfigError::Parse {
            path: path.display().to_string(),
            message: error.to_string(),
        })?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => ConfigFile::default(),
        Err(source) => {
            return Err(ConfigError::Read {
                path: path.display().to_string(),
                source,
            });
        }
    };
    patch(&mut file);
    let text = toml::to_string(&file).map_err(|error| ConfigError::Invalid {
        path: path.display().to_string(),
        message: format!("rendering config: {error}"),
    })?;
    std::fs::create_dir_all(home.as_path()).map_err(|source| ConfigError::Read {
        path: home.as_str().to_string(),
        source,
    })?;
    std::fs::write(&path, text).map_err(|source| ConfigError::Read {
        path: path.display().to_string(),
        source,
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use keke_config_types::DeclaredWireApi;

    use super::*;

    #[cfg(unix)]
    const ROOT: &str = "/tmp/keke-test";
    #[cfg(windows)]
    const ROOT: &str = r"C:\tmp\keke-test";

    fn home() -> HomeLayout {
        let root = AbsPath::new(ROOT).expect("absolute");
        HomeLayout {
            home: root.clone(),
            workspace_root: root,
        }
    }

    fn layer(label: &str, text: &str) -> ConfigLayer {
        ConfigLayer::parse(LayerSource::Inline(label.to_string()), text).expect("parses")
    }

    #[test]
    fn later_layers_override_earlier_ones_field_by_field() {
        let layers = vec![
            layer("user", "provider = \"grok\"\nmodel = \"grok-4.6\"\n"),
            layer("project", "model = \"grok-4-fast\"\n"),
        ];
        let config = Config::from_layers(home(), &layers).expect("merges");

        // The project layer overrode only `model`; `provider` survived.
        assert_eq!(config.model.provider, "grok");
        assert_eq!(config.model.model, "grok-4-fast");
    }

    #[test]
    fn a_section_merges_per_field_rather_than_wholesale() {
        let layers = vec![
            layer(
                "user",
                "[compaction]\ntrigger_percent = 70\nkeep_recent_messages = 8\n",
            ),
            layer("project", "[compaction]\ntrigger_percent = 60\n"),
        ];
        let config = Config::from_layers(home(), &layers).expect("merges");

        assert_eq!(config.compaction.trigger_percent, 60);
        assert_eq!(config.compaction.keep_recent_messages, 8);
    }

    #[test]
    fn defaults_apply_when_no_layer_states_a_value() {
        let config = Config::from_layers(home(), &[]).expect("merges");
        assert_eq!(config.model.provider, DEFAULT_PROVIDER);
        assert_eq!(config.approval_policy, ApprovalPolicy::OnRequest);
        assert_eq!(config.sandbox_mode, SandboxMode::WorkspaceWrite);
    }

    #[test]
    fn an_effort_is_read_and_overridden_like_any_other_field() {
        let layers = vec![
            layer("user", "reasoning_effort = \"low\"\n"),
            layer("project", "reasoning_effort = \"xhigh\"\n"),
        ];
        let config = Config::from_layers(home(), &layers).expect("merges");
        assert_eq!(config.reasoning_effort, Some(ReasoningEffort::XHigh));
    }

    /// Unset is not a level: it leaves each vendor's own default in place.
    #[test]
    fn no_effort_is_configured_by_default() {
        let config = Config::from_layers(home(), &[]).expect("merges");
        assert_eq!(config.reasoning_effort, None);
    }

    /// A misspelled level fails at load, naming what it should have been. The
    /// alternative is a setting that silently did nothing all session.
    #[test]
    fn a_misspelled_effort_fails_at_load() {
        let layers = vec![layer("user", "reasoning_effort = \"maximum\"\n")];
        let error = Config::from_layers(home(), &layers).expect_err("rejected");
        assert!(
            error.to_string().contains("low, medium, high, xhigh, max"),
            "{error}"
        );
    }

    #[test]
    fn declared_providers_accumulate_across_layers() {
        let layers = vec![
            layer(
                "user",
                "[providers.nvidia]\nbase_url = \"https://integrate.api.nvidia.com/v1\"\nenv_key = \"NVIDIA_API_KEY\"\n",
            ),
            layer(
                "project",
                "[providers.ollama]\nbase_url = \"http://localhost:11434/v1\"\n",
            ),
        ];
        let config = Config::from_layers(home(), &layers).expect("merges");

        let routes: Vec<&str> = config
            .providers
            .iter()
            .map(|provider| provider.route.as_str())
            .collect();
        assert_eq!(routes, vec!["nvidia", "ollama"]);
        assert_eq!(config.providers[1].wire, None);
    }

    /// Redeclaring a route overrides that one entry, not the whole list — the
    /// same field-wise rule the scalar settings follow.
    #[test]
    fn redeclaring_a_route_replaces_only_that_entry() {
        let layers = vec![
            layer(
                "user",
                "[providers.nvidia]\nbase_url = \"https://integrate.api.nvidia.com/v1\"\n\n[providers.ollama]\nbase_url = \"http://localhost:11434/v1\"\n",
            ),
            layer(
                "project",
                "[providers.ollama]\nbase_url = \"http://gpu-box:11434/v1\"\nwire = \"responses\"\n",
            ),
        ];
        let config = Config::from_layers(home(), &layers).expect("merges");

        assert_eq!(config.providers.len(), 2);
        let ollama = config
            .providers
            .iter()
            .find(|provider| provider.route == "ollama")
            .expect("ollama survives");
        assert_eq!(ollama.base_url.as_deref(), Some("http://gpu-box:11434/v1"));
        assert_eq!(ollama.wire, Some(DeclaredWireApi::Responses));
    }

    /// `home()` puts the workspace root at `ROOT`, so a pattern naming that
    /// tree is what a matching entry looks like here.
    #[test]
    fn a_directory_override_chooses_the_provider_for_the_tree_it_matches() {
        let layers = vec![layer(
            "user",
            &format!(
                "provider = \"anthropic\"\n\n[[dir]]\nmatch = \"{ROOT}/**\"\nprovider = \"grok-work\"\nmodel = \"grok-4.6\"\n"
            ),
        )];
        let config = Config::from_layers(home(), &layers).expect("merges");
        assert_eq!(config.model.provider, "grok-work");
        assert_eq!(config.model.model, "grok-4.6");
    }

    #[test]
    fn a_directory_override_for_another_tree_leaves_the_default_alone() {
        let layers = vec![layer(
            "user",
            "provider = \"anthropic\"\n\n[[dir]]\nmatch = \"/somewhere/else/**\"\nprovider = \"grok-work\"\n",
        )];
        let config = Config::from_layers(home(), &layers).expect("merges");
        assert_eq!(config.model.provider, "anthropic");
        assert!(config.directory_override.is_none());
    }

    /// Later entries win, so a narrow rule is written below the broad one it
    /// refines rather than having to be reordered by specificity.
    #[test]
    fn the_last_matching_directory_override_wins() {
        let layers = vec![layer(
            "user",
            &format!(
                "[[dir]]\nmatch = \"{ROOT}/**\"\nprovider = \"xai\"\n\n[[dir]]\nmatch = \"{ROOT}\"\nprovider = \"grok-work\"\n"
            ),
        )];
        let config = Config::from_layers(home(), &layers).expect("merges");
        assert_eq!(config.model.provider, "grok-work");
    }

    /// The flag is applied by the composition root after the merge, which is
    /// what makes a one-off override still possible inside a matched tree.
    #[test]
    fn an_explicit_flag_beats_a_directory_override() {
        let layers = vec![layer(
            "user",
            &format!("[[dir]]\nmatch = \"{ROOT}/**\"\nprovider = \"grok-work\"\n"),
        )];
        let mut config = Config::from_layers(home(), &layers).expect("merges");
        assert_eq!(config.model.provider, "grok-work");

        config.model.provider = "xai".to_string();
        assert_eq!(config.model.provider, "xai");
    }

    /// An entry that could not do anything is a half-finished edit, and
    /// applying nothing quietly is the failure it would cause.
    #[test]
    fn a_directory_override_stating_nothing_fails_at_load() {
        let layers = vec![layer("user", "[[dir]]\nmatch = \"~/work/**\"\n")];
        let error = Config::from_layers(home(), &layers).expect_err("rejected");
        assert!(
            error.to_string().contains("neither provider nor model"),
            "{error}"
        );
    }

    #[test]
    fn an_unknown_field_is_rejected_rather_than_ignored() {
        let error = ConfigLayer::parse(
            LayerSource::Inline("user".to_string()),
            "privider = \"xai\"\n",
        )
        .expect_err("typo is rejected");
        assert!(matches!(error, ConfigError::Parse { .. }), "{error}");
    }

    #[test]
    fn a_configured_output_budget_is_validated_at_load() {
        let layers = vec![layer("user", "max_output_tokens = 16000\n")];
        let config = Config::from_layers(home(), &layers).expect("merges");
        assert_eq!(config.max_output_tokens.get(), 16_000);

        let layers = vec![layer("user", "max_output_tokens = 12\n")];
        let error = Config::from_layers(home(), &layers).expect_err("too small");
        assert!(matches!(error, ConfigError::Invalid { .. }), "{error}");
    }

    #[test]
    fn an_out_of_range_value_fails_at_load() {
        let layers = vec![layer("user", "[compaction]\ntrigger_percent = 0\n")];
        let error = Config::from_layers(home(), &layers).expect_err("rejected");
        assert!(matches!(error, ConfigError::Invalid { .. }), "{error}");
    }

    #[test]
    fn disabled_skills_accumulate_across_layers() {
        let layers = vec![
            layer("user", "[skills]\ndisabled = [\"acme:review\"]\n"),
            layer("project", "[skills]\ndisabled = [\"deploy\"]\n"),
        ];

        let config = Config::from_layers(home(), &layers).expect("merges");

        assert!(config.skills.is_disabled("acme", "review"));
        assert!(config.skills.is_disabled("other", "deploy"));
        assert!(!config.skills.is_disabled("acme", "ship"));
    }

    /// Invariant 8: an entry that names nothing is a mistake, not a request to
    /// turn every skill off.
    #[test]
    fn an_empty_disabled_entry_is_refused() {
        let layers = vec![layer("user", "[skills]\ndisabled = [\"\"]\n")];

        assert!(Config::from_layers(home(), &layers).is_err());
    }

    #[test]
    fn a_deployment_can_set_what_a_plugins_program_may_hold_up_a_turn_for() {
        let layers = vec![layer(
            "user",
            "[plugins]\nhook_timeout_millis = 5000\nmcp_call_timeout_millis = 300000\n",
        )];
        let config = Config::from_layers(home(), &layers).expect("merges");
        assert_eq!(config.plugins.hook_millis, 5_000);
        assert_eq!(config.plugins.mcp_call_millis, 300_000);
        // The field nobody stated keeps its default rather than being zeroed.
        assert_eq!(
            config.plugins.mcp_startup_millis,
            PluginTimeouts::default().mcp_startup_millis
        );

        let layers = vec![layer("user", "[plugins]\nhook_timeout_millis = 30\n")];
        let error = Config::from_layers(home(), &layers).expect_err("too short");
        assert!(matches!(error, ConfigError::Invalid { .. }), "{error}");
    }

    #[test]
    fn a_deployment_can_bound_how_many_subagents_run_at_once() {
        let layers = vec![layer("user", "[subagents]\nmax_concurrent = 8\n")];
        let config = Config::from_layers(home(), &layers).expect("merges");
        assert_eq!(config.subagents.max_concurrent, 8);
        // The field nobody stated keeps its default rather than being zeroed.
        assert_eq!(
            config.subagents.timeout_millis,
            SubagentLimits::default().timeout_millis
        );

        // Out of range fails loud rather than being clamped: a deployment that
        // asked for 64 subagents wanted something this cannot give it, and
        // silently giving it 16 answers a question nobody asked.
        let layers = vec![layer("user", "[subagents]\nmax_concurrent = 64\n")];
        let error = Config::from_layers(home(), &layers).expect_err("too many");
        assert!(matches!(error, ConfigError::Invalid { .. }), "{error}");
    }

    /// A switch made at the keyboard must survive a restart, and must not
    /// clobber an unrelated field already on disk.
    #[test]
    fn persisting_an_override_keeps_the_rest_of_the_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let home = AbsPath::new(dir.path()).expect("absolute");
        std::fs::write(
            dir.path().join("config.toml"),
            "provider = \"grok\"\nmodel = \"grok-4.6\"\n",
        )
        .expect("seed file");

        persist_user_override(&home, |file| {
            file.reasoning_effort = Some("high".to_string());
        })
        .expect("persists");

        let written = std::fs::read_to_string(dir.path().join("config.toml")).expect("read back");
        let file: ConfigFile = toml::from_str(&written).expect("parses");
        assert_eq!(file.provider.as_deref(), Some("grok"));
        assert_eq!(file.model.as_deref(), Some("grok-4.6"));
        assert_eq!(file.reasoning_effort.as_deref(), Some("high"));
    }

    /// The first switch on a fresh install has no file to read yet.
    #[test]
    fn persisting_an_override_creates_a_missing_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let home = AbsPath::new(dir.path()).expect("absolute");

        persist_user_override(&home, |file| {
            file.model = Some("gpt-5.2".to_string());
        })
        .expect("persists");

        let written = std::fs::read_to_string(dir.path().join("config.toml")).expect("read back");
        let file: ConfigFile = toml::from_str(&written).expect("parses");
        assert_eq!(file.model.as_deref(), Some("gpt-5.2"));
    }
}
