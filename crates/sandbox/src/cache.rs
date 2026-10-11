//! Cache-only redirects; toolchains, credentials and installation roots stay
//! where the user put them. Tools create these paths inside the existing temp
//! grant, so preparing a command does not write host directories.

use std::hash::{Hash, Hasher};
use std::process::Command;

use keke_paths::AbsPath;

pub(crate) fn redirect(command: &mut Command, workspace: &AbsPath) {
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    workspace
        .as_path()
        .canonicalize()
        .unwrap_or_else(|_| workspace.as_path().to_path_buf())
        .hash(&mut hash);
    let root = std::env::temp_dir().join(format!("keke-package-cache-{:016x}", hash.finish()));
    command.envs([
        ("npm_config_cache", root.join("npm")),
        ("BUN_INSTALL_CACHE_DIR", root.join("bun")),
        ("PIP_CACHE_DIR", root.join("pip")),
        ("UV_CACHE_DIR", root.join("uv")),
        ("XDG_CACHE_HOME", root.join("xdg")),
    ]);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redirects_only_caches_and_is_stable_per_workspace() {
        let workspace = AbsPath::new(std::env::current_dir().unwrap()).unwrap();
        let mut first = Command::new("true");
        let mut second = Command::new("true");
        redirect(&mut first, &workspace);
        redirect(&mut second, &workspace);
        assert_eq!(
            first.get_envs().collect::<Vec<_>>(),
            second.get_envs().collect::<Vec<_>>()
        );
        for (name, value) in first.get_envs() {
            assert!(
                !["CARGO_HOME", "RUSTUP_HOME", "BUN_INSTALL", "PATH", "HOME"]
                    .contains(&name.to_str().unwrap())
            );
            assert!(std::path::Path::new(value.unwrap()).starts_with(std::env::temp_dir()));
        }
        let other = AbsPath::new(workspace.as_path().join("other-workspace")).unwrap();
        redirect(&mut second, &other);
        assert_ne!(
            first.get_envs().collect::<Vec<_>>(),
            second.get_envs().collect::<Vec<_>>()
        );
    }

    #[test]
    fn cache_redirects_follow_mode_and_user_setting() {
        use crate::Sandbox;
        use keke_config_types::{SandboxMode, SandboxPolicy};
        let root = AbsPath::new(std::env::current_dir().unwrap()).unwrap();
        for (mode, enabled, expected) in [
            (SandboxMode::WorkspaceWrite, true, true),
            (SandboxMode::WorkspaceWrite, false, false),
            (SandboxMode::ReadOnly, true, false),
            (SandboxMode::DangerFullAccess, true, false),
        ] {
            let sandbox = Sandbox::new(
                SandboxPolicy {
                    mode,
                    redirect_package_caches: enabled,
                    ..Default::default()
                },
                Some(std::env::current_exe().unwrap()),
            )
            .unwrap();
            let command = sandbox.shell("true", &root);
            assert_eq!(
                command.get_envs().any(|(key, _)| key == "npm_config_cache"),
                expected
            );
        }
    }
}
