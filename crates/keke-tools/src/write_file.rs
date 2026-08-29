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

#[derive(Debug, Deserialize, JsonSchema)]
pub struct WriteFileArgs {
    /// Path to write, absolute or relative to the workspace root.
    pub path: String,
    /// Full new contents of the file.
    pub content: String,
}

#[derive(Debug, Serialize)]
pub struct WriteFileOutput {
    pub path: String,
    pub bytes: usize,
    /// Whether the file existed before this call.
    pub created: bool,
    /// Lines added and removed relative to the previous contents.
    ///
    /// `None` for a new file — there is nothing to diff against, so "N lines
    /// added, 0 removed" would just restate the line count under a busier
    /// name.
    pub diff: Option<support::LineDiff>,
}

impl ToolOutput for WriteFileOutput {
    fn render(&self) -> Vec<ContentBlock> {
        let verb = if self.created { "created" } else { "updated" };
        let changes = match &self.diff {
            Some(diff) if diff.added == 0 && diff.removed == 0 => String::new(),
            Some(diff) => format!(", +{} -{}", diff.added, diff.removed),
            None => String::new(),
        };
        vec![ContentBlock::text(format!(
            "{verb} {} ({} bytes{changes})",
            self.path, self.bytes
        ))]
    }
}

/// Writes a whole file, creating parents as needed.
pub struct WriteFile;

impl Tool for WriteFile {
    type Args = WriteFileArgs;
    type Output = WriteFileOutput;

    fn id(&self) -> ToolId {
        ToolId::new("write_file")
    }

    fn description(&self, _ctx: &ListToolsContext) -> ToolDescription {
        ToolDescription::new(
            "Write a file inside the workspace, replacing any existing contents and creating \
             parent directories. Supply the complete file, not a fragment.",
        )
    }

    fn capabilities(&self) -> ToolCapabilities {
        ToolCapabilities::of_kind(ToolKind::Edit)
    }

    async fn run(&self, ctx: ToolCallContext, args: Self::Args) -> Result<Self::Output, ToolError> {
        let path = support::resolve(&ctx, &args.path)?;
        let display = support::display(&ctx.workspace_root, &path);
        let created = !path.as_path().exists();
        let previous = if created {
            None
        } else {
            tokio::fs::read_to_string(path.as_path()).await.ok()
        };

        if let Some(parent) = path.as_path().parent() {
            tokio::fs::create_dir_all(parent).await.map_err(|error| {
                ToolError::custom("write_failed", format!("{display}: {error}"))
            })?;
        }

        tokio::fs::write(path.as_path(), args.content.as_bytes())
            .await
            .map_err(|error| ToolError::custom("write_failed", format!("{display}: {error}")))?;

        Ok(WriteFileOutput {
            path: display,
            bytes: args.content.len(),
            created,
            diff: previous.map(|previous| support::line_diff(&previous, &args.content)),
        })
    }
}
