//! Configuration loading.
//!
//! Settings come from three layers, later ones overriding earlier:
//! managed (deployment policy), user (`$KEKE_HOME/config.toml`), and project
//! (`.keke/config.toml` beside the workspace root). Splitting the *values*
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
use keke_config_types::CompactionConfig;
use keke_config_types::HomeLayout;
use keke_config_types::ModelSelection;
use keke_config_types::ProviderDeclaration;
use keke_config_types::SandboxMode;
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
    pub sandbox_mode: SandboxMode,
    pub compaction: CompactionConfig,
    /// Endpoints declared from configuration, in addition to the compiled-in
    /// vendors.
    pub providers: Vec<ProviderDeclaration>,
    /// Which layers contributed, in application order. Kept so `keke doctor`
    /// can answer "where did this value come from" without re-reading disk.
    pub sources: Vec<LayerSource>,
}

/// The on-disk shape. Every field is optional: a layer states only what it
/// overrides, and the merge below is a field-wise override rather than a
/// whole-document replacement.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct ConfigFile {
    pub provider: Option<String>,
    pub model: Option<String>,
    pub approval_policy: Option<ApprovalPolicy>,
    pub sandbox_mode: Option<SandboxMode>,
    pub compaction: Option<CompactionFile>,
    /// Extra endpoints, keyed by route: `[providers.nvidia]`. Accumulated
    /// across layers rather than replaced, so a project can add one without
    /// restating the user's.
    #[serde(default)]
    pub providers: BTreeMap<String, ProviderDeclaration>,
}

/// The compaction section, separated so a layer can override one field of it.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct CompactionFile {
    pub trigger_percent: Option<u8>,
    pub keep_recent_messages: Option<usize>,
}

/// Values applied when no layer states them.
///
/// These are defaults, not policy: every one of them is overridable from a
/// config file, which is what invariant 9 in `AGENTS.md` requires.
const DEFAULT_PROVIDER: &str = "grok";
const DEFAULT_MODEL: &str = "grok-4";

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
            merged.sandbox_mode = layer.file.sandbox_mode.or(merged.sandbox_mode);

            if let Some(compaction) = layer.file.compaction {
                let base = merged
                    .compaction
                    .get_or_insert_with(CompactionFile::default);
                base.trigger_percent = compaction.trigger_percent.or(base.trigger_percent);
                base.keep_recent_messages = compaction
                    .keep_recent_messages
                    .or(base.keep_recent_messages);
            }
            // Declarations accumulate; a later layer redeclaring a route
            // replaces that one entry rather than the whole set.
            for (route, declaration) in &layer.file.providers {
                merged.providers.insert(route.clone(), declaration.clone());
            }
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
        };

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

        Ok(Self {
            home,
            model: ModelSelection {
                provider: merged
                    .provider
                    .unwrap_or_else(|| DEFAULT_PROVIDER.to_string()),
                model: merged.model.unwrap_or_else(|| DEFAULT_MODEL.to_string()),
            },
            approval_policy: merged.approval_policy.unwrap_or_default(),
            sandbox_mode: merged.sandbox_mode.unwrap_or_default(),
            compaction,
            providers: merged
                .providers
                .into_iter()
                .map(|(route, declaration)| ProviderDeclaration {
                    route,
                    ..declaration
                })
                .collect(),
            sources,
        })
    }
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
            layer("user", "provider = \"grok\"\nmodel = \"grok-4\"\n"),
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
                "[compaction]\ntrigger-percent = 70\nkeep-recent-messages = 8\n",
            ),
            layer("project", "[compaction]\ntrigger-percent = 60\n"),
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
    fn declared_providers_accumulate_across_layers() {
        let layers = vec![
            layer(
                "user",
                "[providers.nvidia]\nbase-url = \"https://integrate.api.nvidia.com/v1\"\nenv-key = \"NVIDIA_API_KEY\"\n",
            ),
            layer(
                "project",
                "[providers.ollama]\nbase-url = \"http://localhost:11434/v1\"\n",
            ),
        ];
        let config = Config::from_layers(home(), &layers).expect("merges");

        let routes: Vec<&str> = config
            .providers
            .iter()
            .map(|provider| provider.route.as_str())
            .collect();
        assert_eq!(routes, vec!["nvidia", "ollama"]);
        assert_eq!(config.providers[1].wire, DeclaredWireApi::ChatCompletions);
    }

    /// Redeclaring a route overrides that one entry, not the whole list — the
    /// same field-wise rule the scalar settings follow.
    #[test]
    fn redeclaring_a_route_replaces_only_that_entry() {
        let layers = vec![
            layer(
                "user",
                "[providers.nvidia]\nbase-url = \"https://integrate.api.nvidia.com/v1\"\n\n[providers.ollama]\nbase-url = \"http://localhost:11434/v1\"\n",
            ),
            layer(
                "project",
                "[providers.ollama]\nbase-url = \"http://gpu-box:11434/v1\"\nwire = \"responses\"\n",
            ),
        ];
        let config = Config::from_layers(home(), &layers).expect("merges");

        assert_eq!(config.providers.len(), 2);
        let ollama = config
            .providers
            .iter()
            .find(|provider| provider.route == "ollama")
            .expect("ollama survives");
        assert_eq!(ollama.base_url, "http://gpu-box:11434/v1");
        assert_eq!(ollama.wire, DeclaredWireApi::Responses);
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
    fn an_out_of_range_value_fails_at_load() {
        let layers = vec![layer("user", "[compaction]\ntrigger-percent = 0\n")];
        let error = Config::from_layers(home(), &layers).expect_err("rejected");
        assert!(matches!(error, ConfigError::Invalid { .. }), "{error}");
    }
}
