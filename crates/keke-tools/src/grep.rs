use ignore::WalkBuilder;
use ignore::overrides::OverrideBuilder;
use keke_paths::AbsPath;
use keke_protocol::ContentBlock;
use keke_tool::ListToolsContext;
use keke_tool::Tool;
use keke_tool::ToolCallContext;
use keke_tool::ToolCapabilities;
use keke_tool::ToolDescription;
use keke_tool::ToolError;
use keke_tool::ToolId;
use keke_tool::ToolKind;
use keke_tool::ToolOutput;
use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;
use std::io::BufRead;
use std::io::BufReader;

use crate::support;

const MAX_HITS: usize = 200;
/// Files above this size are almost never source, and scanning them is how a
/// search turns into a minutes-long stall.
const MAX_FILE_BYTES: u64 = 2 * 1024 * 1024;
const MAX_LINE_BYTES: usize = 400;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GrepArgs {
    /// Literal substring to look for. Not a regular expression.
    pub pattern: String,
    /// Directory or file to search. Defaults to the workspace root.
    #[serde(default)]
    pub path: Option<String>,
    /// Restrict the search to paths matching this glob, e.g. `*.rs`.
    #[serde(default)]
    pub glob: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct GrepOutput {
    pub pattern: String,
    /// `file:line:text`, one per hit.
    pub hits: Vec<String>,
    pub truncated: bool,
}

impl ToolOutput for GrepOutput {
    fn render(&self) -> Vec<ContentBlock> {
        if self.hits.is_empty() {
            return vec![ContentBlock::text(format!(
                "no matches for `{}`",
                self.pattern
            ))];
        }
        let mut text = self.hits.join("\n");
        if self.truncated {
            text.push_str(&format!(
                "\n… stopped at {MAX_HITS} matches, narrow the search"
            ));
        }
        vec![ContentBlock::text(text)]
    }
}

/// Recursive literal search across the workspace.
pub struct Grep;

impl Tool for Grep {
    type Args = GrepArgs;
    type Output = GrepOutput;

    fn id(&self) -> ToolId {
        ToolId::new("grep")
    }

    fn description(&self, ctx: &ListToolsContext) -> ToolDescription {
        let mut text = String::from(
            "Search workspace files for a literal substring, skipping gitignored, binary, and \
             very large files. Returns `file:line:text` hits.",
        );
        if ctx.has("read_file") {
            text.push_str(" Follow a hit with `read_file` to see its surrounding lines.");
        }
        ToolDescription::new(text)
    }

    fn capabilities(&self) -> ToolCapabilities {
        ToolCapabilities::of_kind(ToolKind::Search)
    }

    async fn run(&self, ctx: ToolCallContext, args: Self::Args) -> Result<Self::Output, ToolError> {
        if args.pattern.is_empty() {
            return Err(ToolError::custom(
                "empty_pattern",
                "pattern must not be empty",
            ));
        }

        let root = match args.path.as_deref() {
            Some(path) => support::resolve(&ctx, path)?,
            None => ctx.workspace_root.clone(),
        };
        if !root.as_path().exists() {
            return Err(ToolError::custom(
                "path_not_found",
                format!(
                    "{}: no such path",
                    support::display(&ctx.workspace_root, &root)
                ),
            ));
        }

        let pattern = args.pattern.clone();
        let glob = args.glob.clone();
        let workspace_root = ctx.workspace_root.clone();
        let cancelled = ctx.cancelled.clone();

        let found = tokio::task::spawn_blocking(move || {
            search(
                &workspace_root,
                &root,
                &pattern,
                glob.as_deref(),
                &*cancelled,
            )
        })
        .await
        .map_err(|error| ToolError::custom("search_failed", error.to_string()))??;

        let (hits, truncated) = found;
        Ok(GrepOutput {
            pattern: args.pattern,
            hits,
            truncated,
        })
    }
}

fn search(
    workspace_root: &AbsPath,
    root: &AbsPath,
    pattern: &str,
    glob: Option<&str>,
    cancelled: &(dyn Fn() -> bool + Send + Sync),
) -> Result<(Vec<String>, bool), ToolError> {
    let mut builder = WalkBuilder::new(root.as_path());
    builder.require_git(false);
    if let Some(glob) = glob {
        let mut overrides = OverrideBuilder::new(root.as_path());
        overrides
            .add(glob)
            .map_err(|error| ToolError::custom("bad_glob", format!("{glob}: {error}")))?;
        let overrides = overrides
            .build()
            .map_err(|error| ToolError::custom("bad_glob", format!("{glob}: {error}")))?;
        builder.overrides(overrides);
    }

    let mut hits = Vec::new();
    for entry in builder.build().flatten() {
        if cancelled() {
            return Err(ToolError::Cancelled);
        }
        if !entry.file_type().is_some_and(|kind| kind.is_file()) {
            continue;
        }
        if entry
            .metadata()
            .is_ok_and(|meta| meta.len() > MAX_FILE_BYTES)
        {
            continue;
        }

        let Ok(file) = std::fs::File::open(entry.path()) else {
            continue;
        };
        let label = AbsPath::new(entry.path())
            .map(|path| support::display(workspace_root, &path))
            .unwrap_or_else(|_| entry.path().to_string_lossy().to_string());

        for (index, line) in BufReader::new(file).lines().enumerate() {
            // A read error here means the file is binary or vanished; either way
            // the rest of the search is still worth finishing.
            let Ok(line) = line else { break };
            if !line.contains(pattern) {
                continue;
            }
            if hits.len() >= MAX_HITS {
                return Ok((hits, true));
            }
            let mut text = line.trim_end().to_string();
            if text.len() > MAX_LINE_BYTES {
                let mut end = MAX_LINE_BYTES;
                while end > 0 && !text.is_char_boundary(end) {
                    end -= 1;
                }
                text.truncate(end);
                text.push('…');
            }
            hits.push(format!("{label}:{}:{text}", index + 1));
        }
    }

    Ok((hits, false))
}
