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
use tokio::io::AsyncReadExt;

use crate::support;

/// Never hand back more than this many lines from one call, even unbounded.
const MAX_LINES: usize = 2_000;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ReadFileArgs {
    /// Path to the file, absolute or relative to the workspace root.
    pub path: String,
    /// 1-based line to start from. Defaults to the first line.
    #[serde(default)]
    pub offset: Option<usize>,
    /// Maximum number of lines to return.
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Debug, Serialize)]
pub struct ReadFileOutput {
    pub path: String,
    pub start_line: usize,
    pub line_count: usize,
    /// True when the file continues past what is shown.
    pub truncated: bool,
    /// Lines with their numbers, ready for the model.
    pub text: String,
}

impl ToolOutput for ReadFileOutput {
    fn render(&self) -> Vec<ContentBlock> {
        let mut text = self.text.clone();
        if self.truncated {
            text.push_str(&format!(
                "\n… truncated; continue with offset {}",
                self.start_line + self.line_count
            ));
        }
        vec![ContentBlock::text(text)]
    }
}

/// Reads a slice of a text file with line numbers.
pub struct ReadFile;

impl Tool for ReadFile {
    type Args = ReadFileArgs;
    type Output = ReadFileOutput;

    fn id(&self) -> ToolId {
        ToolId::new("read_file")
    }

    fn description(&self, _ctx: &ListToolsContext) -> ToolDescription {
        ToolDescription::new(
            "Read a UTF-8 text file inside the workspace. Returns lines prefixed with their \
             1-based numbers. Use `offset` and `limit` to page through a long file.",
        )
    }

    fn capabilities(&self) -> ToolCapabilities {
        ToolCapabilities::of_kind(ToolKind::Read)
    }

    async fn run(&self, ctx: ToolCallContext, args: Self::Args) -> Result<Self::Output, ToolError> {
        let path = support::resolve(&ctx, &args.path)?;
        let display = support::display(&ctx.workspace_root, &path);

        let mut file = tokio::fs::File::open(path.as_path())
            .await
            .map_err(|error| match error.kind() {
                std::io::ErrorKind::NotFound => {
                    ToolError::custom("file_not_found", format!("{display}: no such file"))
                }
                _ => ToolError::custom("read_failed", format!("{display}: {error}")),
            })?;

        // Reading through a bounded `take` keeps a multi-gigabyte file from
        // being materialized just to show its first hundred lines.
        let mut buffer = Vec::new();
        (&mut file)
            .take((support::MAX_OUTPUT_BYTES * 4) as u64)
            .read_to_end(&mut buffer)
            .await
            .map_err(|error| ToolError::custom("read_failed", format!("{display}: {error}")))?;

        if buffer.contains(&0) {
            return Err(ToolError::custom(
                "binary_file",
                format!("{display}: looks binary, not reading"),
            ));
        }

        let content = String::from_utf8_lossy(&buffer);
        let start = args.offset.unwrap_or(1).max(1);
        let limit = args.limit.unwrap_or(MAX_LINES).min(MAX_LINES);

        let mut rendered = String::new();
        let mut count = 0;
        let mut more = false;
        for (index, line) in content.lines().enumerate() {
            let number = index + 1;
            if number < start {
                continue;
            }
            // Stopping on the byte budget as well as the line limit keeps a
            // minified one-line bundle from costing the whole context window.
            if count == limit || rendered.len() >= support::MAX_OUTPUT_BYTES {
                more = true;
                break;
            }
            rendered.push_str(&format!("{number:>6}\t{line}\n"));
            count += 1;
        }

        if count == 0 && !more {
            rendered.push_str("(no lines in range)\n");
        }

        Ok(ReadFileOutput {
            path: display,
            start_line: start,
            line_count: count,
            truncated: more,
            text: rendered,
        })
    }
}
