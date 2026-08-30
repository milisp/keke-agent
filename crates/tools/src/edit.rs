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
pub struct EditArgs {
    /// Path to the file to edit, absolute or relative to the workspace root.
    pub path: String,
    /// Exact text to find. Must match exactly once unless `replace_all` is set.
    pub old_string: String,
    /// Text to replace it with.
    pub new_string: String,
    /// Replace every occurrence of `old_string` instead of requiring exactly one.
    #[serde(default)]
    pub replace_all: bool,
}

#[derive(Debug, Serialize)]
pub struct EditOutput {
    pub path: String,
    pub replacements: usize,
    pub diff: support::LineDiff,
}

impl ToolOutput for EditOutput {
    fn render(&self) -> Vec<ContentBlock> {
        vec![ContentBlock::text(format!(
            "edited {} (+{} -{}, {} replacement{})",
            self.path,
            self.diff.added,
            self.diff.removed,
            self.replacements,
            if self.replacements == 1 { "" } else { "s" }
        ))]
    }
}

/// Replaces an exact substring within a file.
pub struct Edit;

impl Tool for Edit {
    type Args = EditArgs;
    type Output = EditOutput;

    fn id(&self) -> ToolId {
        ToolId::new("edit")
    }

    fn description(&self, _ctx: &ListToolsContext) -> ToolDescription {
        ToolDescription::new(
            "Replace an exact text match inside an existing file. `old_string` must match \
             exactly once in the file unless `replace_all` is set, and must differ from \
             `new_string`. Use `write_file` instead for a new file or a full rewrite.",
        )
    }

    fn capabilities(&self) -> ToolCapabilities {
        ToolCapabilities::of_kind(ToolKind::Edit)
    }

    async fn run(&self, ctx: ToolCallContext, args: Self::Args) -> Result<Self::Output, ToolError> {
        if args.old_string == args.new_string {
            return Err(ToolError::custom(
                "no_op_edit",
                "old_string and new_string are identical",
            ));
        }
        if args.old_string.is_empty() {
            return Err(ToolError::custom(
                "bad_edit",
                "old_string must not be empty",
            ));
        }

        let path = support::resolve(&ctx, &args.path)?;
        let display = support::display(&ctx.workspace_root, &path);

        let contents =
            tokio::fs::read_to_string(path.as_path())
                .await
                .map_err(|error| match error.kind() {
                    std::io::ErrorKind::NotFound => {
                        ToolError::custom("file_not_found", format!("{display}: no such file"))
                    }
                    _ => ToolError::custom("read_failed", format!("{display}: {error}")),
                })?;

        let occurrences = contents.matches(args.old_string.as_str()).count();
        if occurrences == 0 {
            return Err(ToolError::custom(
                "no_match",
                format!("{display}: old_string not found"),
            ));
        }
        if occurrences > 1 && !args.replace_all {
            return Err(ToolError::custom(
                "ambiguous_match",
                format!(
                    "{display}: old_string matches {occurrences} times; pass replace_all or \
                     narrow the match"
                ),
            ));
        }

        let replacements = if args.replace_all { occurrences } else { 1 };
        let updated = if args.replace_all {
            contents.replace(args.old_string.as_str(), &args.new_string)
        } else {
            contents.replacen(args.old_string.as_str(), &args.new_string, 1)
        };

        tokio::fs::write(path.as_path(), updated.as_bytes())
            .await
            .map_err(|error| ToolError::custom("write_failed", format!("{display}: {error}")))?;

        Ok(EditOutput {
            path: display,
            replacements,
            diff: support::line_diff(&contents, &updated),
        })
    }
}
