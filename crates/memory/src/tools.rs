//! `memory_read` and `memory_write`.

use std::sync::Arc;

use keke_protocol::ContentBlock;
use keke_tool::ApprovalRequirement;
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

use crate::store::Store;
use crate::store::StoreError;
use crate::store::Written;

fn refuse(error: StoreError) -> ToolError {
    let code = match &error {
        StoreError::BadName(_) => "invalid_name",
        StoreError::Missing { .. } => "no_such_memory",
        StoreError::TooLarge { .. } => "entry_too_large",
        StoreError::Io(_) => "memory_io",
    };
    ToolError::custom(code, error.to_string())
}

/// Run blocking file work off the async executor.
async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, StoreError> + Send + 'static,
) -> Result<T, ToolError> {
    match tokio::task::spawn_blocking(work).await {
        Ok(result) => result.map_err(refuse),
        Err(error) => Err(ToolError::custom("memory_io", error.to_string())),
    }
}

#[derive(Debug, Serialize)]
pub struct MemoryText {
    pub text: String,
}

impl ToolOutput for MemoryText {
    fn render(&self) -> Vec<ContentBlock> {
        vec![ContentBlock::text(self.text.clone())]
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct MemoryReadArgs {
    /// The entry to read. Omit to list every entry.
    #[serde(default)]
    pub name: Option<String>,
}

/// List the memory entries, or read one.
pub struct MemoryRead {
    pub(crate) store: Arc<Store>,
}

impl Tool for MemoryRead {
    type Args = MemoryReadArgs;
    type Output = MemoryText;

    fn id(&self) -> ToolId {
        ToolId::new("memory_read")
    }

    fn description(&self, _ctx: &ListToolsContext) -> ToolDescription {
        ToolDescription::new(
            "Read your persistent memory: durable facts about the person, the project, and your \
             own role that outlive this session. With no `name`, lists every entry with its first \
             line and size; with a `name`, returns that entry in full. Check it when earlier \
             context would help, and before writing so you update an entry rather than duplicate \
             it.",
        )
    }

    fn capabilities(&self) -> ToolCapabilities {
        ToolCapabilities::of_kind(ToolKind::Meta)
    }

    async fn run(
        &self,
        _ctx: ToolCallContext,
        args: Self::Args,
    ) -> Result<Self::Output, ToolError> {
        let store = Arc::clone(&self.store);
        let text = match args.name {
            Some(name) => blocking(move || store.read(&name)).await?,
            None => {
                let entries = blocking(move || store.list()).await?;
                if entries.is_empty() {
                    "No memories saved yet.".to_string()
                } else {
                    entries
                        .iter()
                        .map(|entry| {
                            format!(
                                "{} ({} bytes): {}",
                                entry.name, entry.bytes, entry.first_line
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("\n")
                }
            }
        };
        Ok(MemoryText { text })
    }
}

/// How `memory_write` combines `content` with what is already there.
#[derive(Debug, Default, Clone, Copy, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WriteMode {
    /// Replace the entry. Empty `content` deletes it.
    #[default]
    Replace,
    /// Add `content` to the end of the entry, creating it if needed.
    Append,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct MemoryWriteArgs {
    /// Entry name: lowercase letters, digits, `-` and `_`, starting with a letter
    /// or digit, at most 64 characters.
    pub name: String,
    /// Markdown text. With mode `replace`, empty content deletes the entry.
    pub content: String,
    /// `replace` (default) or `append`.
    #[serde(default)]
    pub mode: WriteMode,
}

/// Save, extend, or delete a memory entry.
pub struct MemoryWrite {
    pub(crate) store: Arc<Store>,
}

impl Tool for MemoryWrite {
    type Args = MemoryWriteArgs;
    type Output = MemoryText;

    fn id(&self) -> ToolId {
        ToolId::new("memory_write")
    }

    fn description(&self, _ctx: &ListToolsContext) -> ToolDescription {
        ToolDescription::new(
            "Save something to your persistent memory so it survives this session: durable facts \
             about the person, the project, or your own role — preferences, decisions, standing \
             instructions. It is not for scratch notes or anything only this task needs. Keep \
             each entry short and focused; one topic per entry. `mode` `replace` (the default) \
             overwrites the entry and empty `content` deletes it; `append` adds to the end. \
             Entries over the size limit are refused. What you write is visible to you in future \
             sessions, so write it as a note to your future self.",
        )
    }

    fn capabilities(&self) -> ToolCapabilities {
        ToolCapabilities {
            // Not `Edit`: this writes only into the operator-chosen memory
            // directory — keke's own state, like the session log — and never
            // the workspace, so approval policy and the read-only sandbox,
            // which govern the workspace, have nothing to protect here. `Meta`
            // is "affects only harness state", and `AutoApproved` says so
            // explicitly rather than leaving it to the kind. Guards still run.
            kind: ToolKind::Meta,
            approval: ApprovalRequirement::AutoApproved,
            // Two appends to one entry must not interleave their read and
            // their rename.
            concurrency_safe: false,
            timeout_millis: None,
        }
    }

    async fn run(
        &self,
        _ctx: ToolCallContext,
        args: Self::Args,
    ) -> Result<Self::Output, ToolError> {
        let store = Arc::clone(&self.store);
        let MemoryWriteArgs {
            name,
            content,
            mode,
        } = args;
        let note = name.clone();
        let written = blocking(move || match mode {
            WriteMode::Replace => store.replace(&name, &content),
            WriteMode::Append => store.append(&name, &content),
        })
        .await?;
        let text = match written {
            Written::Saved(bytes) => format!("Saved memory `{note}` ({bytes} bytes)."),
            Written::Deleted => format!("Deleted memory `{note}`."),
        };
        Ok(MemoryText { text })
    }
}
