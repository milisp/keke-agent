//! Runtime-installable plugins, in the Claude Code plugin format.
//!
//! keke has two different things called plugins, and the distinction is
//! load-bearing:
//!
//! - **Code-extensions** are crates compiled into the binary, registering
//!   through the contributor traits in `keke-plugin-api`. Adding one means
//!   rebuilding.
//! - **Data-plugins**, this crate, are installed at runtime and contain no code
//!   the host executes in-process. They contribute skills, slash commands, MCP
//!   servers, and hook programs — and nothing else.
//!
//! The split exists because Rust has no stable dynamic-library ABI. A plugin
//! system that loaded code in-process would mean freezing a C ABI forever, and
//! everything a plugin actually wants to ship turns out to be declarative or to
//! live in another process anyway.
//!
//! The *format* is not keke's. `plugin.json` and its convention directories
//! come from the Claude Code ecosystem, so plugins that already exist install
//! here unchanged. A plugin system's worth is the plugins available on day one;
//! a better schema with an empty catalog is worth nothing.
//!
//! What keke does not adopt is the ecosystem's permissiveness about failure.
//! Resolution here is inert, containment is checked after canonicalization, and
//! a contribution this host cannot honor is reported rather than dropped.
//!
//! Consumers do not reach this crate from the engine. The extension crate for
//! each contribution kind reads a [`PluginSet`] and registers through the
//! ordinary contributor traits, so `keke-core` never learns runtime plugins
//! exist.

pub(crate) use keke_paths::AbsPath;

mod contributions;
mod manifest;
mod marketplace;
mod resolve;
mod trust;

pub use contributions::HookEvent;
pub use contributions::ResolvedCommand;
pub use contributions::ResolvedHook;
pub use contributions::ResolvedMcpServer;
pub use contributions::ResolvedSkill;
pub use manifest::Author;
pub use manifest::COMMANDS_DIR;
pub use manifest::HOOKS_FILE;
pub use manifest::KNOWN_CONTRIBUTIONS;
pub use manifest::MANIFEST_PATHS;
pub use manifest::MCP_FILE;
pub use manifest::ManifestContributions;
pub use manifest::PathOrInline;
pub use manifest::PathOrPaths;
pub use manifest::PluginManifest;
pub use manifest::SKILLS_DIR;
pub use manifest::is_valid_plugin_name;
pub use marketplace::EntrySource;
pub use marketplace::ForeignInstall;
pub use marketplace::GitRef;
pub use marketplace::MARKETPLACE_PATHS;
pub use marketplace::Marketplace;
pub use marketplace::MarketplaceEntry;
pub use marketplace::foreign_installs;
pub use resolve::PluginError;
pub use resolve::PluginScope;
pub use resolve::PluginSet;
pub use resolve::ResolvedPlugin;
pub use resolve::discover;
pub use resolve::load;
pub use trust::InstallSource;
pub use trust::PluginRecord;
pub use trust::Trust;
pub use trust::TrustStore;
pub use trust::Withheld;
