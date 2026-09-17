//! A denial guard for credential files.
//!
//! Reads are no longer contained to the workspace, and `ToolKind::Read` never
//! asks anyone, so nothing else stands between the model and `~/.ssh/id_rsa`.
//! codex's sandbox has the same shape — every profile grants read access to
//! `/` — and narrows it back with explicit deny entries; this is that deny
//! list, expressed as a [`ToolGuard`](keke_plugin_api::ToolGuard) so it can
//! only ever subtract. A permissive extension cannot undo it, and it needs no
//! configuration to turn on, because a setting for "let the agent read my
//! private keys" is one a person enables once and then leaves on.
//!
//! What it does *not* claim: this stops a plausible accident, not a determined
//! exfiltration. `bash` can read a key through a pipeline no token scan will
//! recognize. Sandboxing the child process is the answer to that, and this
//! guard is not a substitute for it.

use std::path::Component;
use std::path::PathBuf;

use keke_protocol::ToolCall;
use serde_json::Value;

/// Directory names whose contents are credentials whatever they are called.
const SECRET_DIRECTORIES: &[&str] = &[".ssh", ".aws", ".gnupg", ".kube", ".docker"];

/// Paths that are only sensitive deeper than their first component.
const SECRET_SUBPATHS: &[&[&str]] = &[&[".config", "gcloud"], &[".config", "gh"]];

/// File names that carry a secret regardless of where they sit.
const SECRET_FILES: &[&str] = &[
    ".netrc",
    "_netrc",
    ".pgpass",
    ".npmrc",
    ".pypirc",
    "id_rsa",
    "id_dsa",
    "id_ecdsa",
    "id_ed25519",
];

/// Extensions that mean "this file is a key".
///
/// Deliberately not `.key`: too much ordinary content — shader keys, test
/// fixtures, translation tables — carries it for a denial to be fair.
const SECRET_EXTENSIONS: &[&str] = &["pem", "p12", "pfx", "keystore", "jks"];

/// The harness's own store, which holds every vendor token this session uses.
const KEKE_CREDENTIALS: &str = "credentials.json";

/// Suffixes that make a `.env`-named file a template rather than a secret.
///
/// The whole point of `.env.example` is to be read — it is what a person
/// commits so the next one knows which variables to set — so denying it would
/// cost something real and protect nothing. Everything else named `.env` is
/// assumed to hold the values that example describes.
const ENV_TEMPLATE_SUFFIXES: &[&str] = &["example", "sample", "template", "dist", "defaults"];

/// Why this call is refused, or `None` when nothing in it names a secret.
pub(crate) fn denial(call: &ToolCall) -> Option<String> {
    let mut found = None;
    walk_strings(&call.arguments, &mut |text| {
        if found.is_some() {
            return;
        }
        // A `path` argument is one token; a `bash` command line is many, and
        // the file at stake is the same either way.
        for token in tokenize(text) {
            if let Some(reason) = sensitive(token) {
                found = Some(format!(
                    "{}: {reason}. Reading credentials is refused; ask the person for what you \
                     need instead.",
                    token
                ));
                return;
            }
        }
    });
    found
}

/// What makes `token` a credential, if anything.
fn sensitive(token: &str) -> Option<&'static str> {
    let path = expand_home(token);
    let components: Vec<String> = path
        .components()
        .filter_map(|component| match component {
            Component::Normal(part) => Some(part.to_string_lossy().to_lowercase()),
            _ => None,
        })
        .collect();

    if let Some(name) = components.last() {
        if SECRET_FILES.contains(&name.as_str()) {
            return Some("a credential file");
        }
        if let Some(extension) = name.rsplit_once('.').map(|(_, ext)| ext)
            && SECRET_EXTENSIONS.contains(&extension)
        {
            return Some("a private key");
        }
        if is_secret_env_file(name) {
            return Some("an environment file, which holds configured secrets");
        }
    }

    for directory in SECRET_DIRECTORIES {
        if components.iter().any(|part| part == directory) {
            return Some("under a credential directory");
        }
    }

    for subpath in SECRET_SUBPATHS {
        if components
            .windows(subpath.len())
            .any(|window| window.iter().zip(*subpath).all(|(part, want)| part == want))
        {
            return Some("under a credential directory");
        }
    }

    // `$KEKE_HOME/credentials.json` specifically, not every file named that:
    // a project's own `credentials.json` fixture is not the harness's store.
    if components.last().map(String::as_str) == Some(KEKE_CREDENTIALS)
        && let Some(home) = keke_home()
        && path.starts_with(&home)
    {
        return Some("the harness's own credential store");
    }

    None
}

/// Whether `name` is a `.env` file holding values rather than describing them.
///
/// Both orders are in the wild — `.env.production` and `.env.local` as well as
/// `production.env` — so the check is on either end. A name is a template only
/// when the *whole* trailing segment says so: `.env.example` is a template,
/// `.env.example-staging` is somebody's real staging file.
fn is_secret_env_file(name: &str) -> bool {
    let is_env = name == ".env"
        || name.strip_prefix(".env.").is_some()
        || name.strip_suffix(".env").is_some();
    if !is_env {
        return false;
    }
    let last_segment = name.rsplit('.').next().unwrap_or_default();
    !ENV_TEMPLATE_SUFFIXES.contains(&last_segment)
}

/// `$KEKE_HOME`, else `~/.keke`.
///
/// Duplicated from `keke-credentials` rather than depended on: that crate links
/// the OS keyring, and the tool pack should not link a keychain to decide
/// whether a path is a secret.
fn keke_home() -> Option<PathBuf> {
    match std::env::var("KEKE_HOME") {
        Ok(value) if !value.trim().is_empty() => Some(PathBuf::from(value)),
        _ => dirs::home_dir().map(|home| home.join(".keke")),
    }
}

fn expand_home(token: &str) -> PathBuf {
    let rest = token
        .strip_prefix("~/")
        .or_else(|| token.strip_prefix("$HOME/"));
    match (rest, dirs::home_dir()) {
        (Some(rest), Some(home)) => home.join(rest),
        _ => PathBuf::from(token),
    }
}

/// Split a string into things that could be paths.
///
/// Shell punctuation is stripped rather than parsed: `cat ~/.ssh/id_rsa;` and
/// `cat "$HOME/.ssh/id_rsa"` must both surrender the same token, and a real
/// shell parser here would be a second implementation of `bash` to keep
/// correct.
fn tokenize(text: &str) -> impl Iterator<Item = &str> {
    text.split(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | ';' | '|' | '&' | '=' | ','))
        .map(|token| token.trim_matches(|c: char| matches!(c, '(' | ')' | '<' | '>' | '`')))
        .filter(|token| !token.is_empty())
}

fn walk_strings(value: &Value, visit: &mut impl FnMut(&str)) {
    match value {
        Value::String(text) => visit(text),
        Value::Array(items) => items.iter().for_each(|item| walk_strings(item, visit)),
        Value::Object(fields) => fields.values().for_each(|field| walk_strings(field, visit)),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use keke_protocol::ToolCallId;
    use serde_json::json;

    fn call(name: &str, arguments: Value) -> ToolCall {
        ToolCall {
            id: ToolCallId::new("call-1"),
            name: name.to_string(),
            arguments,
        }
    }

    #[test]
    fn an_ordinary_source_file_is_allowed() {
        let denial = denial(&call(
            "read_file",
            json!({ "path": "/tmp/other/src/main.rs" }),
        ));

        assert!(denial.is_none(), "got {denial:?}");
    }

    #[test]
    fn a_private_key_outside_the_workspace_is_denied() {
        let denial = denial(&call("read_file", json!({ "path": "~/.ssh/id_rsa" })));

        assert!(denial.is_some_and(|text| text.contains("credential")));
    }

    #[test]
    fn a_credential_directory_is_denied_whatever_the_file_is_called() {
        let denial = denial(&call(
            "list_dir",
            json!({ "path": "/Users/x/.aws/notes.txt" }),
        ));

        assert!(denial.is_some());
    }

    #[test]
    fn a_shell_command_is_searched_token_by_token() {
        let denial = denial(&call(
            "bash",
            json!({ "command": "cat ~/.aws/credentials | pbcopy" }),
        ));

        assert!(denial.is_some(), "a pipeline hides the same file");
    }

    #[test]
    fn a_quoted_shell_path_is_still_found() {
        let denial = denial(&call(
            "bash",
            json!({ "command": "cat \"$HOME/.ssh/config\"" }),
        ));

        assert!(denial.is_some());
    }

    #[test]
    fn a_project_file_named_credentials_json_is_not_the_harness_store() {
        let denial = denial(&call(
            "read_file",
            json!({ "path": "/work/repo/tests/credentials.json" }),
        ));

        assert!(denial.is_none(), "got {denial:?}");
    }

    #[test]
    fn a_pem_file_is_denied_wherever_it_lives() {
        let denial = denial(&call("read_file", json!({ "path": "certs/server.pem" })));

        assert!(denial.is_some());
    }

    #[test]
    fn an_env_file_in_the_workspace_is_denied() {
        for path in [
            ".env",
            "services/api/.env",
            ".env.production",
            "staging.env",
        ] {
            let denial = denial(&call("read_file", json!({ "path": path })));

            assert!(denial.is_some(), "{path} holds configured secrets");
        }
    }

    #[test]
    fn an_env_template_is_allowed() {
        for path in [".env.example", ".env.sample", ".env.template", ".env.dist"] {
            let denial = denial(&call("read_file", json!({ "path": path })));

            assert!(
                denial.is_none(),
                "{path} is committed to be read: {denial:?}"
            );
        }
    }

    #[test]
    fn a_template_suffix_only_counts_as_the_whole_segment() {
        let denial = denial(&call(
            "read_file",
            json!({ "path": ".env.example-staging" }),
        ));

        assert!(denial.is_some(), "that is somebody's real staging file");
    }

    #[test]
    fn a_file_whose_name_merely_contains_a_secret_word_is_allowed() {
        let denial = denial(&call("read_file", json!({ "path": "src/ssh_client.rs" })));

        assert!(denial.is_none(), "got {denial:?}");
    }
}
