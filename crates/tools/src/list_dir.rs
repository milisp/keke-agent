use ignore::WalkBuilder;
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

use crate::support;

const MAX_ENTRIES: usize = 500;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ListDirArgs {
    /// Directory to list. Defaults to the workspace root.
    #[serde(default)]
    pub path: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ListDirOutput {
    pub path: String,
    pub entries: Vec<String>,
    pub truncated: bool,
}

impl ToolOutput for ListDirOutput {
    fn render(&self) -> Vec<ContentBlock> {
        let mut text = format!("{}:\n", self.path);
        if self.entries.is_empty() {
            text.push_str("(empty)\n");
        }
        for entry in &self.entries {
            text.push_str(entry);
            text.push('\n');
        }
        if self.truncated {
            text.push_str(&format!(
                "… more than {MAX_ENTRIES} entries, list truncated\n"
            ));
        }
        vec![ContentBlock::text(text)]
    }
}

/// Lists one directory level, honoring `.gitignore`.
pub struct ListDir;

impl Tool for ListDir {
    type Args = ListDirArgs;
    type Output = ListDirOutput;

    fn id(&self) -> ToolId {
        ToolId::new("list_dir")
    }

    fn description(&self, _ctx: &ListToolsContext) -> ToolDescription {
        ToolDescription::new(
            "List the immediate entries of a workspace directory, one per line, with a trailing \
             `/` on directories. Gitignored and hidden entries are omitted.",
        )
    }

    fn capabilities(&self) -> ToolCapabilities {
        ToolCapabilities::of_kind(ToolKind::Read)
    }

    async fn run(&self, ctx: ToolCallContext, args: Self::Args) -> Result<Self::Output, ToolError> {
        let target = match args.path.as_deref() {
            Some(path) => support::resolve(&ctx, path)?,
            None => ctx.workspace_root.clone(),
        };
        let display = support::display(&ctx.workspace_root, &target);

        if !target.as_path().is_dir() {
            return Err(ToolError::custom(
                "not_a_directory",
                format!("{display}: not a directory"),
            ));
        }

        // `ignore` is synchronous and hits the filesystem, so it belongs on the
        // blocking pool rather than on a runtime worker.
        let listing = tokio::task::spawn_blocking(move || collect(&target))
            .await
            .map_err(|error| ToolError::custom("list_failed", error.to_string()))?;

        let (mut entries, truncated) = listing;
        entries.sort();

        Ok(ListDirOutput {
            path: display,
            entries,
            truncated,
        })
    }
}

fn collect(dir: &AbsPath) -> (Vec<String>, bool) {
    let mut entries = Vec::new();
    let mut truncated = false;

    // `.gitignore` files are honored even when the workspace is not a git
    // checkout: a scratch directory with an ignore file still means it.
    let walk = WalkBuilder::new(dir.as_path())
        .max_depth(Some(1))
        .require_git(false)
        .build();
    for entry in walk.flatten() {
        if entry.depth() == 0 {
            continue;
        }
        if entries.len() >= MAX_ENTRIES {
            truncated = true;
            break;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        let is_dir = entry.file_type().is_some_and(|kind| kind.is_dir());
        entries.push(if is_dir { format!("{name}/") } else { name });
    }

    (entries, truncated)
}
