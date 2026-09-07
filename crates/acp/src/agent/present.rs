//! How a tool call is described to an ACP client.
//!
//! A client draws a tool call from the fields the protocol gives it: a title,
//! a kind, the file locations it touched, and the raw input. Sending only the
//! tool's name leaves an editor showing `bash` with no way to see the command,
//! so the interesting argument is lifted into the title here — the same thing
//! keke's own transcript does for its collapsed row.
//!
//! Nothing here is keyed on a vendor. `SALIENT` and the kind table are
//! argument and tool *names*, so a tool keke has never heard of still gets a
//! title and falls back to [`Facet::Other`] rather than to nothing.

use std::path::Path;
use std::path::PathBuf;

use keke_protocol::ToolCall;

/// What a tool does, in the terms both ACP versions share. Mapped to each
/// version's own `ToolKind` at the notification, since the two are distinct
/// types that cannot be named once.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Facet {
    Read,
    Edit,
    Delete,
    Move,
    Search,
    Execute,
    Fetch,
    Think,
    Other,
}

/// A tool call in the shape a client draws it.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct Presented {
    pub title: String,
    pub kind: Facet,
    /// Absolute, so a client's follow-along can open them. Empty when the call
    /// named no file.
    pub locations: Vec<PathBuf>,
}

/// Arguments worth reading on their own, most specific first.
const SALIENT: [&str; 6] = ["command", "path", "file_path", "pattern", "query", "url"];

/// Arguments that name a file, and so become a location.
const PATH_KEYS: [&str; 2] = ["path", "file_path"];

/// How long a title may run before it is cut. A client shows it on one row.
const TITLE_LIMIT: usize = 120;

pub(super) fn describe(call: &ToolCall, cwd: &Path) -> Presented {
    Presented {
        title: title(&call.name, &call.arguments),
        kind: facet(&call.name),
        locations: locations(&call.arguments, cwd),
    }
}

/// `bash: cargo test`, rather than `bash`.
///
/// The name stays in front of the argument because a client groups and filters
/// by what it reads here, and a bare command line does not say which tool ran
/// it.
fn title(name: &str, arguments: &serde_json::Value) -> String {
    match salient(arguments) {
        Some(value) => format!("{name}: {}", one_line(&value, TITLE_LIMIT)),
        None => name.to_string(),
    }
}

fn salient(arguments: &serde_json::Value) -> Option<String> {
    let serde_json::Value::Object(fields) = arguments else {
        return None;
    };
    SALIENT.into_iter().find_map(|key| match fields.get(key) {
        Some(serde_json::Value::String(text)) if !text.trim().is_empty() => Some(text.clone()),
        _ => None,
    })
}

/// The kind a client picks an icon and a UI treatment from.
///
/// Matched on the last segment of the name so an MCP tool arrives here as the
/// tool it is rather than as its server's prefix.
fn facet(name: &str) -> Facet {
    let leaf = name.rsplit_once("__").map_or(name, |(_, leaf)| leaf);
    let leaf = leaf.rsplit_once('.').map_or(leaf, |(_, leaf)| leaf);
    match leaf {
        "bash" | "shell" | "execute" | "run" | "run_command" | "terminal" => Facet::Execute,
        "read" | "read_file" | "view" | "cat" => Facet::Read,
        "edit" | "write" | "write_file" | "apply_patch" | "multi_edit" | "create_file" => {
            Facet::Edit
        }
        "delete" | "delete_file" | "remove" | "rm" => Facet::Delete,
        "move" | "move_file" | "rename" => Facet::Move,
        "grep" | "search" | "glob" | "list_dir" | "find" | "codebase_search" => Facet::Search,
        "web_search" | "web_fetch" | "fetch" => Facet::Fetch,
        "think" | "plan" | "todo" | "update_plan" => Facet::Think,
        _ => Facet::Other,
    }
}

/// Every file the call named, made absolute against the session's directory.
///
/// A relative path is what keke's own tools take, and a client that resolved
/// it against its own working directory would open the wrong file — or, more
/// often, none.
fn locations(arguments: &serde_json::Value, cwd: &Path) -> Vec<PathBuf> {
    let serde_json::Value::Object(fields) = arguments else {
        return Vec::new();
    };
    PATH_KEYS
        .into_iter()
        .filter_map(|key| match fields.get(key) {
            Some(serde_json::Value::String(text)) if !text.trim().is_empty() => {
                let path = Path::new(text.trim());
                Some(if path.is_absolute() {
                    path.to_path_buf()
                } else {
                    cwd.join(path)
                })
            }
            _ => None,
        })
        .collect()
}

/// Whitespace collapsed and the tail cut, so a multi-line heredoc still reads
/// as one row instead of breaking the client's layout.
fn one_line(text: &str, limit: usize) -> String {
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() <= limit {
        return collapsed;
    }
    let kept: String = collapsed.chars().take(limit.saturating_sub(1)).collect();
    format!("{kept}…")
}

#[cfg(test)]
mod tests {
    use super::*;
    use keke_protocol::ToolCallId;

    fn call(name: &str, arguments: serde_json::Value) -> ToolCall {
        ToolCall {
            id: ToolCallId::new("c1"),
            name: name.to_string(),
            arguments,
        }
    }

    #[test]
    fn a_shell_call_puts_the_command_in_the_title() {
        let described = describe(
            &call(
                "bash",
                serde_json::json!({"command": "cargo test", "timeout_ms": 5}),
            ),
            Path::new("/work"),
        );
        assert_eq!(described.title, "bash: cargo test");
        assert_eq!(described.kind, Facet::Execute);
        assert!(described.locations.is_empty());
    }

    #[test]
    fn a_multi_line_command_still_reads_as_one_row() {
        let described = describe(
            &call("bash", serde_json::json!({"command": "echo hi\nsleep 1"})),
            Path::new("/work"),
        );
        assert_eq!(described.title, "bash: echo hi sleep 1");
    }

    #[test]
    fn a_file_argument_becomes_an_absolute_location() {
        let described = describe(
            &call("edit", serde_json::json!({"path": "src/lib.rs"})),
            Path::new("/work"),
        );
        assert_eq!(described.kind, Facet::Edit);
        assert_eq!(described.locations, vec![PathBuf::from("/work/src/lib.rs")]);
    }

    #[test]
    fn an_absolute_file_argument_is_left_alone() {
        let described = describe(
            &call("read_file", serde_json::json!({"path": "/elsewhere/x.rs"})),
            Path::new("/work"),
        );
        assert_eq!(described.kind, Facet::Read);
        assert_eq!(described.locations, vec![PathBuf::from("/elsewhere/x.rs")]);
    }

    #[test]
    fn an_mcp_tool_is_matched_on_its_leaf_name() {
        assert_eq!(facet("mcp__github__search"), Facet::Search);
        assert_eq!(facet("mcp__github__create_issue"), Facet::Other);
    }

    #[test]
    fn a_tool_keke_has_never_heard_of_still_gets_a_title() {
        let described = describe(
            &call("stranger", serde_json::json!({"query": "who"})),
            Path::new("/work"),
        );
        assert_eq!(described.title, "stranger: who");
        assert_eq!(described.kind, Facet::Other);
    }

    #[test]
    fn a_call_with_nothing_salient_falls_back_to_the_name() {
        let described = describe(
            &call("todo", serde_json::json!({"items": []})),
            Path::new("/work"),
        );
        assert_eq!(described.title, "todo");
        assert_eq!(described.kind, Facet::Think);
    }
}
