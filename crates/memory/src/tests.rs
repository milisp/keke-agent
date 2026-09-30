//! What the model can do with memory, and what it cannot.

use std::sync::Arc;

use keke_config_types::MemoryConfig;
use keke_paths::AbsPath;
use keke_plugin_api::ExtensionContext;
use keke_plugin_api::ExtensionRegistryBuilder;
use keke_protocol::SessionId;
use keke_protocol::ThreadId;
use keke_protocol::ToolCallId;
use keke_tool::ApprovalRequirement;
use keke_tool::Tool;
use keke_tool::ToolCallContext;
use keke_tool::ToolError;
use keke_tool::ToolKind;

use crate::MemoryRead;
use crate::MemoryWrite;
use crate::store::Entry;
use crate::store::Store;
use crate::summary;
use crate::tools::MemoryReadArgs;
use crate::tools::MemoryWriteArgs;
use crate::tools::WriteMode;

struct Fixture {
    /// The parent, so escapes from `dir` are observable.
    root: tempfile::TempDir,
    dir: AbsPath,
    read: MemoryRead,
    write: MemoryWrite,
}

fn fixture(entry_max: u32) -> Fixture {
    let root = tempfile::tempdir().expect("tempdir");
    let dir = AbsPath::new(root.path().join("bot")).expect("absolute");
    let store = Arc::new(Store::new(dir.clone(), entry_max));
    Fixture {
        root,
        dir,
        read: MemoryRead {
            store: Arc::clone(&store),
        },
        write: MemoryWrite { store },
    }
}

fn context() -> ToolCallContext {
    ToolCallContext {
        call_id: ToolCallId::new("call-1"),
        workspace_root: AbsPath::new(std::env::temp_dir()).expect("absolute"),
        timeout_millis: None,
        cancelled: Arc::new(|| false),
    }
}

impl Fixture {
    async fn write(&self, name: &str, content: &str, mode: WriteMode) -> Result<String, ToolError> {
        self.write
            .run(
                context(),
                MemoryWriteArgs {
                    name: name.to_string(),
                    content: content.to_string(),
                    mode,
                },
            )
            .await
            .map(|out| out.text)
    }

    async fn read(&self, name: Option<&str>) -> Result<String, ToolError> {
        self.read
            .run(
                context(),
                MemoryReadArgs {
                    name: name.map(str::to_string),
                },
            )
            .await
            .map(|out| out.text)
    }

    /// Every file under the fixture's root, relative to it.
    fn files(&self) -> Vec<String> {
        fn walk(dir: &std::path::Path, base: &std::path::Path, out: &mut Vec<String>) {
            let Ok(read) = std::fs::read_dir(dir) else {
                return;
            };
            for item in read.flatten() {
                let path = item.path();
                if path.is_dir() {
                    walk(&path, base, out);
                } else {
                    out.push(
                        path.strip_prefix(base)
                            .expect("under base")
                            .display()
                            .to_string(),
                    );
                }
            }
        }
        let mut out = Vec::new();
        walk(self.root.path(), self.root.path(), &mut out);
        out.sort();
        out
    }
}

#[tokio::test]
async fn a_name_that_could_leave_the_directory_is_refused_and_nothing_is_written() {
    let memory = fixture(1024);
    for name in [
        "..",
        "../escape",
        "a/b",
        "a\\b",
        "Upper",
        ".hidden",
        "-leading",
        "",
        "has space",
        "dot.md",
        &"x".repeat(65),
    ] {
        let error = memory
            .write(name, "text", WriteMode::Replace)
            .await
            .expect_err(name);
        assert!(
            error.to_string().contains("not a valid memory name"),
            "{name}: {error}"
        );
        assert!(
            memory.read(Some(name)).await.is_err(),
            "{name} was readable"
        );
    }
    assert!(memory.files().is_empty(), "wrote {:?}", memory.files());
    assert!(
        !memory.dir.as_path().exists(),
        "created the directory for nothing"
    );
}

#[tokio::test]
async fn what_is_written_can_be_read_back() {
    let memory = fixture(1024);
    memory
        .write("user-prefs", "# Prefs\nlikes tabs\n", WriteMode::Replace)
        .await
        .expect("writes");
    assert_eq!(
        memory.read(Some("user-prefs")).await.expect("reads"),
        "# Prefs\nlikes tabs\n"
    );
    // The entry is a markdown file in the memory directory and nowhere else,
    // and no temp file is left behind by the atomic write.
    assert_eq!(memory.files(), vec!["bot/user-prefs.md".to_string()]);

    memory
        .write("user-prefs", "changed", WriteMode::Replace)
        .await
        .expect("replaces");
    assert_eq!(
        memory.read(Some("user-prefs")).await.expect("reads"),
        "changed"
    );
}

#[tokio::test]
async fn append_extends_an_entry_and_creates_a_missing_one() {
    let memory = fixture(1024);
    memory
        .write("log", "one", WriteMode::Append)
        .await
        .expect("creates");
    memory
        .write("log", "two\n", WriteMode::Append)
        .await
        .expect("appends");
    assert_eq!(memory.read(Some("log")).await.expect("reads"), "one\ntwo\n");
}

#[tokio::test]
async fn replacing_with_nothing_deletes_the_entry() {
    let memory = fixture(1024);
    memory
        .write("gone", "soon", WriteMode::Replace)
        .await
        .expect("writes");
    let message = memory
        .write("gone", "", WriteMode::Replace)
        .await
        .expect("deletes");
    assert!(message.contains("Deleted"), "{message}");
    assert!(memory.files().is_empty());
    let error = memory.read(Some("gone")).await.expect_err("gone");
    assert!(
        error.to_string().contains("no memory named `gone`"),
        "{error}"
    );
}

#[tokio::test]
async fn a_missing_entry_error_lists_what_exists() {
    let memory = fixture(1024);
    memory
        .write("alpha", "a", WriteMode::Replace)
        .await
        .expect("writes");
    memory
        .write("beta", "b", WriteMode::Replace)
        .await
        .expect("writes");
    let error = memory.read(Some("gamma")).await.expect_err("missing");
    let text = error.to_string();
    assert!(text.contains("alpha") && text.contains("beta"), "{text}");
}

#[tokio::test]
async fn an_oversize_entry_is_refused_and_the_existing_one_is_untouched() {
    let memory = fixture(1024);
    memory
        .write("note", "small", WriteMode::Replace)
        .await
        .expect("writes");

    let error = memory
        .write("note", &"x".repeat(1025), WriteMode::Replace)
        .await
        .expect_err("too big");
    assert!(error.to_string().contains("1024-byte limit"), "{error}");
    assert_eq!(memory.read(Some("note")).await.expect("reads"), "small");

    // An append that would cross the limit is refused whole, not truncated.
    let error = memory
        .write("note", &"y".repeat(1024), WriteMode::Append)
        .await
        .expect_err("too big");
    assert!(error.to_string().contains("limit"), "{error}");
    assert_eq!(memory.read(Some("note")).await.expect("reads"), "small");
    assert_eq!(memory.files(), vec!["bot/note.md".to_string()]);
}

#[tokio::test]
async fn listing_is_sorted_and_shows_each_first_line() {
    let memory = fixture(1024);
    assert_eq!(
        memory.read(None).await.expect("lists"),
        "No memories saved yet."
    );
    memory
        .write("zeta", "\n\n# Last one\nbody", WriteMode::Replace)
        .await
        .expect("writes");
    memory
        .write("alpha", "First\nsecond", WriteMode::Replace)
        .await
        .expect("writes");
    // Files that are not entries are not listed.
    std::fs::write(memory.dir.as_path().join("Notes.md"), "no").expect("stray");
    std::fs::write(memory.dir.as_path().join("readme.txt"), "no").expect("stray");

    let listing = memory.read(None).await.expect("lists");
    let lines: Vec<&str> = listing.lines().collect();
    assert_eq!(
        lines,
        vec!["alpha (12 bytes): First", "zeta (17 bytes): Last one"]
    );
}

fn entries(count: usize) -> Vec<Entry> {
    (0..count)
        .map(|n| Entry {
            name: format!("entry-{n:02}"),
            first_line: "a fact worth remembering".to_string(),
            bytes: 10,
        })
        .collect()
}

#[test]
fn the_summary_says_how_many_entries_the_budget_dropped() {
    let entries = entries(20);
    let full = summary::render("/mem", &entries, usize::MAX).expect("rendered");
    assert!(full.contains("entry-19") && !full.contains("truncated"));

    let budget = full.len() / 2;
    let cut = summary::render("/mem", &entries, budget).expect("rendered");
    assert!(cut.len() <= budget, "{} > {budget}", cut.len());
    let kept = cut.matches("- entry-").count();
    assert!(kept > 0 && kept < 20);
    assert!(
        cut.ends_with(&format!(
            "(truncated — {} more entries; use memory_read)",
            20 - kept
        )),
        "{cut}"
    );
    assert!(cut.contains("memory_write") && cut.contains("/mem"));
}

#[test]
fn a_budget_smaller_than_the_preamble_cuts_on_a_character_boundary() {
    // Multi-byte characters in the path make an unlucky cut land mid-character
    // for some budgets; every budget must still produce valid text in bounds.
    let dir = "/記憶/🧠/bot";
    for budget in 1..400 {
        let text = summary::render(dir, &entries(3), budget).expect("rendered");
        assert!(text.len() <= budget, "{} > {budget}", text.len());
    }
    assert_eq!(summary::truncate_on_boundary("aé", 2), "a");
}

#[test]
fn a_summary_budget_of_zero_injects_no_fragment() {
    assert!(summary::render("/mem", &entries(2), 0).is_none());

    let root = tempfile::tempdir().expect("tempdir");
    let dir = AbsPath::new(root.path()).expect("absolute");
    let mut builder = ExtensionRegistryBuilder::new();
    crate::install(
        &mut builder,
        dir,
        &MemoryConfig {
            summary_max_bytes: 0,
            ..MemoryConfig::default()
        },
    );
    let registry = builder.build();
    let ctx = ExtensionContext::new(SessionId::new(), ThreadId::new());
    let fragments = fragments(&registry, &ctx);
    assert!(fragments.is_empty());
    // The tools are still there: the budget is about the prompt, not the feature.
    assert_eq!(
        registry
            .tool_contributors()
            .flat_map(|c| c.tools(&ctx))
            .count(),
        2
    );
}

fn fragments(
    registry: &keke_plugin_api::ExtensionRegistry,
    ctx: &ExtensionContext,
) -> Vec<keke_plugin_api::ContextFragment> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("runtime");
    registry
        .context_contributors()
        .flat_map(|c| runtime.block_on(c.contribute_turn_context(ctx)))
        .collect()
}

#[test]
fn the_summary_is_frozen_when_the_session_starts() {
    let root = tempfile::tempdir().expect("tempdir");
    let dir = AbsPath::new(root.path()).expect("absolute");
    std::fs::write(root.path().join("before.md"), "existed at startup").expect("seed");

    let mut builder = ExtensionRegistryBuilder::new();
    crate::install(&mut builder, dir, &MemoryConfig::default());
    let registry = builder.build();
    let ctx = ExtensionContext::new(SessionId::new(), ThreadId::new());

    std::fs::write(root.path().join("after.md"), "written mid-session").expect("write");
    let fragments = fragments(&registry, &ctx);
    assert_eq!(fragments.len(), 1);
    assert_eq!(fragments[0].name, "memory");
    assert!(fragments[0].text.contains("before: existed at startup"));
    assert!(!fragments[0].text.contains("after"));
}

/// Memory is the agent's own state, not the workspace: it must not be an edit
/// that a read-only or approval-gated deployment would stop.
#[test]
fn writing_memory_is_not_a_workspace_edit() {
    let memory = fixture(1024);
    let capabilities = memory.write.capabilities();
    assert_ne!(capabilities.kind, ToolKind::Edit);
    assert_eq!(capabilities.approval, ApprovalRequirement::AutoApproved);
    assert!(!capabilities.concurrency_safe);
}
