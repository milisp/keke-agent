//! The built-in tool pack: filesystem reads, search, and shell execution.
//!
//! Every tool here keeps its effects under `ToolCallContext::workspace_root`.
//! Containment is enforced per call rather than at registration because the
//! root is a property of the session, not of the tool.

mod apply_patch;
mod bash;
mod edit;
mod escalate;
mod git;
mod grep;
mod list_dir;
mod prompt;
mod read_file;
mod secrets;
mod support;
mod web_search;
mod write_file;

pub use apply_patch::ApplyPatch;
pub use apply_patch::ApplyPatchArgs;
pub use apply_patch::ApplyPatchOutput;
pub use apply_patch::ChangeKind;
pub use apply_patch::FileChange;

pub use bash::Bash;
pub use bash::BashArgs;
pub use bash::BashOutput;
pub use edit::Edit;
pub use edit::EditArgs;
pub use edit::EditOutput;
pub use escalate::BashUnsandboxed;
pub use escalate::BashUnsandboxedArgs;
use escalate::PersonDecides;
pub use git::GitAdd;
pub use git::GitAddArgs;
pub use git::GitCommit;
pub use git::GitCommitArgs;
pub use git::GitOutput;
pub use grep::Grep;
pub use grep::GrepArgs;
pub use grep::GrepOutput;
pub use list_dir::ListDir;
pub use list_dir::ListDirArgs;
pub use list_dir::ListDirOutput;
pub use read_file::ReadFile;
pub use read_file::ReadFileArgs;
pub use read_file::ReadFileOutput;
pub use web_search::WebSearch;
pub use web_search::WebSearchArgs;
pub use web_search::WebSearchOutput;
pub use web_search::install_web_search;
pub use write_file::WriteFile;
pub use write_file::WriteFileArgs;
pub use write_file::WriteFileOutput;

use std::sync::Arc;

use keke_config_types::SandboxMode;
use keke_plugin_api::ExtensionContext;
use keke_plugin_api::ExtensionRegistryBuilder;
use keke_plugin_api::ToolContributor;
use keke_sandbox::Sandbox;
use keke_tool::ArcTool;

/// Every tool in this pack, in the order they are advertised.
///
/// `sandbox` confines what `bash` runs. `background` is where a backgrounded
/// shell command goes; `None` builds a pack whose `bash` can only run in the
/// foreground.
///
/// Two tools depend on the sandbox. `bash_unsandboxed` is offered only where
/// there is a sandbox to step outside of. And under `read_only` the edit
/// tools ask a person every time: they write from this process, where no
/// sandbox reaches, so asking is how the mode's promise is kept for them.
#[must_use]
pub fn builtin_tools(
    sandbox: Arc<Sandbox>,
    background: Option<Arc<keke_tasks::BackgroundTasks>>,
) -> Vec<ArcTool> {
    let read_only = sandbox.policy().mode == SandboxMode::ReadOnly;
    let confines = sandbox.confines();
    let mut tools: Vec<ArcTool> = vec![
        Arc::new(ReadFile),
        Arc::new(ListDir),
        Arc::new(Grep),
        Arc::new(Bash {
            sandbox: Arc::clone(&sandbox),
            background,
        }),
    ];
    if sandbox.policy().mode == SandboxMode::WorkspaceWrite {
        tools.push(Arc::new(GitAdd {
            sandbox: Arc::clone(&sandbox),
        }));
        tools.push(Arc::new(GitCommit {
            sandbox: Arc::clone(&sandbox),
        }));
    }
    if confines {
        tools.push(Arc::new(BashUnsandboxed::new()));
    }
    if read_only {
        tools.push(Arc::new(PersonDecides(WriteFile)));
        tools.push(Arc::new(PersonDecides(Edit)));
        tools.push(Arc::new(PersonDecides(ApplyPatch)));
    } else {
        tools.push(Arc::new(WriteFile));
        tools.push(Arc::new(Edit));
        tools.push(Arc::new(ApplyPatch));
    }
    tools
}

struct BuiltinTools {
    sandbox: Arc<Sandbox>,
    background: Option<Arc<keke_tasks::BackgroundTasks>>,
}

impl ToolContributor for BuiltinTools {
    fn tools(&self, _ctx: &ExtensionContext) -> Vec<ArcTool> {
        builtin_tools(Arc::clone(&self.sandbox), self.background.clone())
    }
}

/// Register the built-in tool pack, and the turn context it needs.
///
/// `sandbox` confines every shell command the pack runs. Pass the background
/// registry to let `bash` start commands that outlive the turn; pass `None`
/// for a composition that has none.
pub fn install(
    registry: &mut ExtensionRegistryBuilder,
    sandbox: Arc<Sandbox>,
    background: Option<Arc<keke_tasks::BackgroundTasks>>,
) {
    registry.tool_contributor(Arc::new(BuiltinTools {
        sandbox,
        background,
    }));
    registry.context_contributor(Arc::new(prompt::BuiltinToolGuidance));
    // Guards subtract only, so this one is registered unconditionally: there is
    // no composition in which reading a private key is what the person meant.
    registry.tool_guard(Box::new(secrets::denial));
}

#[cfg(test)]
mod tests {
    use super::*;

    use keke_paths::AbsPath;
    use keke_protocol::ContentBlock;
    use keke_protocol::ToolCallId;
    use keke_tool::ListToolsContext;
    use keke_tool::Tool;
    use keke_tool::ToolCallContext;
    use keke_tool::ToolError;
    use keke_tool::ToolId;
    use keke_tool::ToolOutput;
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::Ordering;
    use tempfile::TempDir;

    fn unconfined() -> Arc<Sandbox> {
        Arc::new(Sandbox::unconfined())
    }

    fn workspace() -> (TempDir, ToolCallContext) {
        let dir = tempfile::tempdir().expect("tempdir");
        // macOS hands out `/var/...` symlinks; canonicalizing keeps the root
        // comparable with paths resolved through it.
        let root = dir.path().canonicalize().expect("canonicalize");
        let ctx = ToolCallContext {
            call_id: ToolCallId::new("call-1"),
            workspace_root: AbsPath::new(root).expect("absolute"),
            timeout_millis: None,
            cancelled: Arc::new(|| false),
        };
        (dir, ctx)
    }

    fn write(ctx: &ToolCallContext, rel: &str, content: &str) {
        let path = ctx.workspace_root.as_path().join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("parents");
        }
        std::fs::write(path, content).expect("write");
    }

    fn rendered<T: ToolOutput>(output: &T) -> String {
        output
            .render()
            .iter()
            .map(|block| match block {
                ContentBlock::Text { text } => text.clone(),
                other => format!("{other:?}"),
            })
            .collect()
    }

    #[tokio::test]
    async fn read_file_numbers_lines() {
        let (_dir, ctx) = workspace();
        write(&ctx, "a.txt", "one\ntwo\nthree\n");

        let out = ReadFile
            .run(
                ctx,
                ReadFileArgs {
                    path: "a.txt".into(),
                    offset: None,
                    limit: None,
                },
            )
            .await
            .expect("read");

        assert_eq!(out.line_count, 3);
        assert!(!out.truncated);
        assert!(out.text.contains("     1\tone"));
        assert!(out.text.contains("     3\tthree"));
    }

    #[tokio::test]
    async fn read_file_honors_offset_and_limit() {
        let (_dir, ctx) = workspace();
        write(&ctx, "a.txt", "one\ntwo\nthree\nfour\n");

        let out = ReadFile
            .run(
                ctx,
                ReadFileArgs {
                    path: "a.txt".into(),
                    offset: Some(2),
                    limit: Some(2),
                },
            )
            .await
            .expect("read");

        assert_eq!(out.start_line, 2);
        assert_eq!(out.line_count, 2);
        assert!(out.truncated, "a fourth line remains");
        assert!(out.text.contains("two"));
        assert!(out.text.contains("three"));
        assert!(!out.text.contains("four"));
        assert!(rendered(&out).contains("continue with offset 4"));
    }

    #[test]
    fn installing_the_pack_registers_the_credential_guard() {
        let mut builder = keke_plugin_api::ExtensionRegistryBuilder::new();
        install(&mut builder, unconfined(), None);
        let registry = builder.build();

        let denial = registry.first_denial(&keke_protocol::ToolCall {
            id: ToolCallId::new("call-1"),
            name: "read_file".into(),
            arguments: serde_json::json!({ "path": "~/.ssh/id_rsa" }),
        });

        assert!(
            denial.is_some(),
            "reads are uncontained, so this is the gate"
        );
    }

    fn sandbox(mode: SandboxMode) -> Arc<Sandbox> {
        let policy = keke_config_types::SandboxPolicy {
            mode,
            ..Default::default()
        };
        Arc::new(
            Sandbox::new(policy, Some(std::path::PathBuf::from("/proc/self/exe")))
                .expect("this machine must be able to build the sandbox"),
        )
    }

    fn approvals(tools: &[ArcTool]) -> Vec<(String, keke_tool::ApprovalRequirement)> {
        tools
            .iter()
            .map(|tool| (tool.id().to_string(), tool.capabilities().approval))
            .collect()
    }

    /// `read_only` is a promise about the files, not about `bash`: the edit
    /// tools write from this process, where no sandbox reaches, so a person
    /// answers for each of them — as codex asks rather than refuses.
    #[test]
    fn read_only_puts_every_in_process_write_in_front_of_a_person() {
        use keke_tool::ApprovalRequirement::Always;
        use keke_tool::ApprovalRequirement::ByPolicy;

        let tools = approvals(&builtin_tools(sandbox(SandboxMode::ReadOnly), None));
        for name in ["write_file", "edit", "apply_patch"] {
            assert!(
                tools.contains(&(name.to_string(), Always)),
                "{name}: {tools:?}"
            );
        }
        assert!(tools.contains(&("read_file".to_string(), ByPolicy)));

        let tools = approvals(&builtin_tools(sandbox(SandboxMode::WorkspaceWrite), None));
        assert!(tools.contains(&("write_file".to_string(), ByPolicy)));
        for name in ["git_add", "git_commit"] {
            assert!(tools.contains(&(name.to_string(), ByPolicy)));
        }
    }

    /// Stepping outside the sandbox is offered where there is one, and asks
    /// every time; with nothing confined there is nothing to step outside of.
    #[test]
    fn stepping_outside_the_sandbox_is_offered_only_where_there_is_one() {
        let offered = |sandbox: Arc<Sandbox>| {
            approvals(&builtin_tools(sandbox, None))
                .into_iter()
                .find(|(id, _)| id == "bash_unsandboxed")
        };
        assert_eq!(offered(unconfined()), None);
        let confining = sandbox(SandboxMode::WorkspaceWrite);
        if confining.confines() {
            assert_eq!(
                offered(confining),
                Some((
                    "bash_unsandboxed".to_string(),
                    keke_tool::ApprovalRequirement::Always
                ))
            );
        } else {
            assert_eq!(offered(confining), None, "no sandbox to step outside of");
        }
    }

    /// Where no sandbox exists, a person stands in for it on every command.
    #[test]
    fn bash_asks_every_time_where_the_sandbox_is_not_enforced() {
        let confining = sandbox(SandboxMode::WorkspaceWrite);
        let bash = Bash {
            sandbox: Arc::clone(&confining),
            background: None,
        };
        let expected = if confining.is_enforced() {
            keke_tool::ApprovalRequirement::AutoApproved
        } else {
            keke_tool::ApprovalRequirement::Always
        };
        assert_eq!(bash.capabilities().approval, expected);
    }

    #[test]
    fn sandboxed_bash_approval_can_be_restored_by_config() {
        let policy = keke_config_types::SandboxPolicy {
            auto_approve_bash: false,
            ..Default::default()
        };
        let sandbox = Arc::new(
            Sandbox::new(policy, Some(std::path::PathBuf::from("/proc/self/exe")))
                .expect("this machine must be able to build the sandbox"),
        );
        if sandbox.is_enforced() {
            let bash = Bash {
                sandbox,
                background: None,
            };
            assert_eq!(
                bash.capabilities().approval,
                keke_tool::ApprovalRequirement::ByPolicy
            );
        }
    }

    #[tokio::test]
    async fn a_file_outside_the_workspace_can_be_read() {
        let (_dir, ctx) = workspace();
        let outside = tempfile::tempdir().expect("tempdir");
        let path = outside.path().join("sibling.rs");
        std::fs::write(&path, "fn main() {}\n").expect("write");

        let out = ReadFile
            .run(
                ctx,
                ReadFileArgs {
                    path: path.to_string_lossy().into_owned(),
                    offset: None,
                    limit: None,
                },
            )
            .await
            .expect("a sibling checkout is ordinary code");

        assert!(out.text.contains("fn main"));
        assert!(
            out.path.starts_with('/'),
            "a path outside the root reads back absolute, got {}",
            out.path
        );
    }

    #[tokio::test]
    async fn a_write_outside_the_workspace_is_refused() {
        let (_dir, ctx) = workspace();

        let error = WriteFile
            .run(
                ctx,
                WriteFileArgs {
                    path: "../escape.txt".into(),
                    content: "no".into(),
                },
            )
            .await
            .expect_err("escape refused");

        assert!(matches!(error, ToolError::Denied { .. }), "got {error:?}");
    }

    #[tokio::test]
    async fn a_missing_file_names_itself() {
        let (_dir, ctx) = workspace();

        let error = ReadFile
            .run(
                ctx,
                ReadFileArgs {
                    path: "nope.txt".into(),
                    offset: None,
                    limit: None,
                },
            )
            .await
            .expect_err("missing");

        assert!(
            matches!(&error, ToolError::Execution { code, message }
                if code == "file_not_found" && message.contains("nope.txt")),
            "got {error:?}"
        );
    }

    #[tokio::test]
    async fn list_dir_marks_directories_and_skips_ignored_files() {
        let (_dir, ctx) = workspace();
        write(&ctx, ".gitignore", "secret.txt\n");
        write(&ctx, "secret.txt", "shh");
        write(&ctx, "kept.txt", "hi");
        write(&ctx, "sub/inner.txt", "hi");

        let out = ListDir
            .run(ctx, ListDirArgs { path: None })
            .await
            .expect("list");

        assert!(out.entries.contains(&"kept.txt".to_string()));
        assert!(out.entries.contains(&"sub/".to_string()));
        assert!(!out.entries.contains(&"secret.txt".to_string()));
    }

    /// A regex pattern is the point of the tool; a bad one is the model's
    /// mistake and must come back as such rather than as an empty result.
    #[tokio::test]
    async fn grep_matches_a_regular_expression() {
        let (_dir, ctx) = workspace();
        std::fs::write(
            ctx.workspace_root.as_path().join("src.rs"),
            "fn alpha() {}\nfn beta() {}\nlet gamma = 1;\n",
        )
        .expect("write");

        let out = Grep
            .run(
                ctx.clone(),
                GrepArgs {
                    pattern: r"^fn \w+\(".to_string(),
                    path: None,
                    glob: None,
                },
            )
            .await
            .expect("searches");

        assert_eq!(out.hits.len(), 2, "{:?}", out.hits);
        assert!(out.hits.iter().all(|hit| hit.contains("fn ")));
    }

    #[tokio::test]
    async fn grep_reports_an_invalid_pattern_rather_than_finding_nothing() {
        let (_dir, ctx) = workspace();
        let error = Grep
            .run(
                ctx,
                GrepArgs {
                    pattern: "unclosed(".to_string(),
                    path: None,
                    glob: None,
                },
            )
            .await
            .expect_err("rejected");

        assert!(
            matches!(&error, ToolError::Execution { code, .. } if code == "invalid_pattern"),
            "{error}"
        );
    }

    #[tokio::test]
    async fn grep_reports_the_matching_line_number() {
        let (_dir, ctx) = workspace();
        write(&ctx, "src/main.rs", "fn main() {}\n// needle here\n");
        write(&ctx, "notes.md", "needle in markdown\n");

        let out = Grep
            .run(
                ctx,
                GrepArgs {
                    pattern: "needle".into(),
                    path: None,
                    glob: Some("*.rs".into()),
                },
            )
            .await
            .expect("grep");

        assert_eq!(out.hits, vec!["src/main.rs:2:// needle here".to_string()]);
    }

    #[test]
    fn grep_points_at_read_file_only_when_it_is_listed() {
        let alone = ListToolsContext {
            siblings: vec![ToolId::new("grep")],
            ..ListToolsContext::default()
        };
        let together = ListToolsContext {
            siblings: vec![ToolId::new("grep"), ToolId::new("read_file")],
            ..ListToolsContext::default()
        };

        assert!(!Tool::description(&Grep, &alone).text.contains("read_file"));
        assert!(
            Tool::description(&Grep, &together)
                .text
                .contains("read_file")
        );
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn bash_captures_output_and_a_failing_exit_code() {
        let (_dir, ctx) = workspace();

        let out = Bash {
            sandbox: unconfined(),
            background: None,
        }
        .run(
            ctx,
            BashArgs {
                command: "echo out; echo err >&2; exit 3".into(),
                background: false,
                timeout_ms: None,
            },
        )
        .await
        .expect("ran");

        let BashOutput::Finished {
            exit_code, output, ..
        } = &out
        else {
            panic!("a foreground command must return its output, not a task id");
        };
        assert_eq!(*exit_code, 3);
        assert!(output.contains("out"));
        assert!(output.contains("err"));
        assert!(rendered(&out).contains("[exit 3]"));
    }

    #[tokio::test(flavor = "multi_thread")]
    #[cfg(unix)]
    async fn bash_reports_a_timeout_as_a_timeout() {
        let (_dir, ctx) = workspace();

        let error = Bash {
            sandbox: unconfined(),
            background: None,
        }
        .run(
            ctx,
            BashArgs {
                command: "sleep 5".into(),
                background: false,
                timeout_ms: Some(100),
            },
        )
        .await
        .expect_err("times out");

        assert!(
            matches!(error, ToolError::Timeout { millis } if millis == 100),
            "got {error:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    #[cfg(unix)]
    async fn bash_stops_when_the_turn_is_cancelled() {
        let (_dir, mut ctx) = workspace();
        let flag = Arc::new(AtomicBool::new(false));
        let observed = Arc::clone(&flag);
        ctx.cancelled = Arc::new(move || observed.load(Ordering::SeqCst));

        let abort = Arc::clone(&flag);
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            abort.store(true, Ordering::SeqCst);
        });

        let error = Bash {
            sandbox: unconfined(),
            background: None,
        }
        .run(
            ctx,
            BashArgs {
                command: "sleep 5".into(),
                background: false,
                timeout_ms: Some(30_000),
            },
        )
        .await
        .expect_err("cancelled");

        assert!(matches!(error, ToolError::Cancelled), "got {error:?}");
    }

    #[tokio::test]
    async fn apply_patch_lands_every_section_of_one_patch() {
        let (_dir, ctx) = workspace();
        write(&ctx, "keep.txt", "one\ntwo\n");
        write(&ctx, "old.txt", "gone\n");

        let out = ApplyPatch
            .run(
                ctx.clone(),
                ApplyPatchArgs {
                    patch: concat!(
                        "*** Begin Patch\n",
                        "*** Add File: new.txt\n",
                        "+fresh\n",
                        "*** Update File: keep.txt\n",
                        "*** Move to: moved.txt\n",
                        "@@\n",
                        " one\n",
                        "-two\n",
                        "+three\n",
                        "*** Delete File: old.txt\n",
                        "*** End Patch\n",
                    )
                    .into(),
                },
            )
            .await
            .expect("patch applies");

        let root = ctx.workspace_root.as_path();
        assert_eq!(
            std::fs::read_to_string(root.join("new.txt")).expect("added"),
            "fresh\n"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("moved.txt")).expect("moved"),
            "one\nthree\n"
        );
        assert!(!root.join("keep.txt").exists());
        assert!(!root.join("old.txt").exists());
        assert_eq!(out.changes.len(), 3);
        assert!(rendered(&out).contains("keep.txt -> moved.txt"));
    }

    #[tokio::test]
    async fn a_patch_that_cannot_apply_leaves_nothing_behind() {
        let (_dir, ctx) = workspace();
        write(&ctx, "a.txt", "unrelated\n");

        let error = ApplyPatch
            .run(
                ctx.clone(),
                ApplyPatchArgs {
                    patch: concat!(
                        "*** Begin Patch\n",
                        "*** Add File: new.txt\n",
                        "+fresh\n",
                        "*** Update File: a.txt\n",
                        "@@\n",
                        "-missing\n",
                        "+replaced\n",
                        "*** End Patch\n",
                    )
                    .into(),
                },
            )
            .await
            .expect_err("second section cannot apply");

        assert!(
            matches!(error, ToolError::Execution { .. }),
            "got {error:?}"
        );
        // The add section came first, so a non-atomic implementation would
        // have written it before reaching the failure.
        assert!(!ctx.workspace_root.as_path().join("new.txt").exists());
    }

    #[tokio::test]
    async fn write_file_creates_parents_and_reads_back() {
        let (_dir, ctx) = workspace();

        let out = WriteFile
            .run(
                ctx.clone(),
                WriteFileArgs {
                    path: "deep/nested/file.txt".into(),
                    content: "hello\n".into(),
                },
            )
            .await
            .expect("write");

        assert!(out.created);
        assert_eq!(out.bytes, 6);
        let back =
            std::fs::read_to_string(ctx.workspace_root.as_path().join("deep/nested/file.txt"))
                .expect("read back");
        assert_eq!(back, "hello\n");
        assert!(out.diff.is_none());
    }

    #[tokio::test]
    async fn write_file_reports_line_diff_on_overwrite() {
        let (_dir, ctx) = workspace();
        WriteFile
            .run(
                ctx.clone(),
                WriteFileArgs {
                    path: "file.txt".into(),
                    content: "a\nb\nc\n".into(),
                },
            )
            .await
            .expect("write");

        let out = WriteFile
            .run(
                ctx.clone(),
                WriteFileArgs {
                    path: "file.txt".into(),
                    content: "a\nx\nc\nd\n".into(),
                },
            )
            .await
            .expect("write");

        assert!(!out.created);
        let diff = out.diff.expect("diff");
        assert_eq!(diff.added, 2);
        assert_eq!(diff.removed, 1);
    }

    /// The engine only ever calls tools through `ToolDyn`, so at least one test
    /// has to go the whole way: JSON in, rendered blocks out.
    #[tokio::test]
    async fn tools_run_through_the_dyn_json_path() {
        let (_dir, ctx) = workspace();
        write(&ctx, "a.txt", "alpha\nbeta\n");

        let read: ArcTool = Arc::new(ReadFile);
        let out = read
            .call(ctx, serde_json::json!({ "path": "a.txt", "offset": 2 }))
            .await
            .expect("call");

        assert_eq!(out.tool_id, ToolId::new("read_file"));
        assert_eq!(out.value["line_count"], 1);
        assert_eq!(out.model_output, vec![ContentBlock::text("     2\tbeta\n")]);
        assert!(read.input_schema()["properties"]["path"].is_object());
    }

    #[tokio::test]
    async fn bad_json_arguments_name_the_tool() {
        let (_dir, ctx) = workspace();
        let read: ArcTool = Arc::new(ReadFile);

        let error = read
            .call(ctx, serde_json::json!({ "offset": 2 }))
            .await
            .expect_err("no path");

        assert!(matches!(error, ToolError::InvalidArgs { ref tool, .. } if tool == "read_file"));
    }

    /// The whole point of the flag: the call returns while the command is
    /// still running, and hands back the id everything later uses to name it.
    #[tokio::test(flavor = "multi_thread")]
    #[cfg(unix)]
    async fn a_backgrounded_command_returns_an_id_instead_of_waiting() {
        let (_dir, ctx) = workspace();
        let tasks = Arc::new(keke_tasks::BackgroundTasks::new(
            keke_config_types::BackgroundLimits::default(),
            unconfined(),
        ));

        let out = Bash {
            sandbox: unconfined(),
            background: Some(Arc::clone(&tasks)),
        }
        .run(
            ctx,
            BashArgs {
                command: "sleep 30".into(),
                background: true,
                timeout_ms: None,
            },
        )
        .await
        .expect("started");

        let BashOutput::Started { task_id } = &out else {
            panic!("a background command must return an id, not output");
        };
        assert!(keke_tasks::TaskSource::owns(tasks.as_ref(), task_id));
        keke_tasks::TaskSource::kill(tasks.as_ref(), task_id);
    }

    /// A composition with no registry says so rather than quietly running the
    /// command in the foreground, which is a different answer to a different
    /// question.
    #[tokio::test]
    async fn backgrounding_without_a_registry_is_an_error_not_a_silent_wait() {
        let (_dir, ctx) = workspace();
        let error = Bash {
            sandbox: unconfined(),
            background: None,
        }
        .run(
            ctx,
            BashArgs {
                command: "true".into(),
                background: true,
                timeout_ms: None,
            },
        )
        .await
        .expect_err("no registry");

        assert!(
            matches!(error, ToolError::Execution { ref code, .. } if code == "background_unavailable"),
            "{error:?}"
        );
    }

    #[test]
    fn the_pack_installs_every_builtin_tool() {
        let mut builder = ExtensionRegistryBuilder::new();
        install(&mut builder, unconfined(), None);
        let registry = builder.build();

        let ctx = ExtensionContext::new(
            keke_protocol::SessionId::new(),
            keke_protocol::ThreadId::new(),
        );
        let ids: Vec<String> = registry
            .tool_contributors()
            .flat_map(|contributor| contributor.tools(&ctx))
            .map(|tool| tool.id().to_string())
            .collect();

        assert_eq!(
            ids,
            vec![
                "read_file",
                "list_dir",
                "grep",
                "bash",
                "write_file",
                "edit",
                "apply_patch"
            ]
        );
    }

    #[tokio::test]
    async fn edit_replaces_a_unique_match() {
        let (_dir, ctx) = workspace();
        write(&ctx, "a.txt", "one\ntwo\nthree\n");

        let out = Edit
            .run(
                ctx.clone(),
                EditArgs {
                    path: "a.txt".into(),
                    old_string: "two".into(),
                    new_string: "TWO".into(),
                    replace_all: false,
                },
            )
            .await
            .expect("edit");

        assert_eq!(out.replacements, 1);
        let back =
            std::fs::read_to_string(ctx.workspace_root.as_path().join("a.txt")).expect("read back");
        assert_eq!(back, "one\nTWO\nthree\n");
    }

    #[tokio::test]
    async fn edit_refuses_an_ambiguous_match() {
        let (_dir, ctx) = workspace();
        write(&ctx, "a.txt", "dup\ndup\n");

        let error = Edit
            .run(
                ctx,
                EditArgs {
                    path: "a.txt".into(),
                    old_string: "dup".into(),
                    new_string: "x".into(),
                    replace_all: false,
                },
            )
            .await
            .expect_err("ambiguous");

        assert!(
            matches!(&error, ToolError::Execution { code, .. } if code == "ambiguous_match"),
            "{error:?}"
        );
    }

    #[tokio::test]
    async fn edit_replace_all_replaces_every_occurrence() {
        let (_dir, ctx) = workspace();
        write(&ctx, "a.txt", "dup\ndup\n");

        let out = Edit
            .run(
                ctx.clone(),
                EditArgs {
                    path: "a.txt".into(),
                    old_string: "dup".into(),
                    new_string: "x".into(),
                    replace_all: true,
                },
            )
            .await
            .expect("edit");

        assert_eq!(out.replacements, 2);
        let back =
            std::fs::read_to_string(ctx.workspace_root.as_path().join("a.txt")).expect("read back");
        assert_eq!(back, "x\nx\n");
    }

    #[tokio::test]
    async fn edit_rejects_a_missing_match() {
        let (_dir, ctx) = workspace();
        write(&ctx, "a.txt", "one\n");

        let error = Edit
            .run(
                ctx,
                EditArgs {
                    path: "a.txt".into(),
                    old_string: "missing".into(),
                    new_string: "x".into(),
                    replace_all: false,
                },
            )
            .await
            .expect_err("no match");

        assert!(
            matches!(&error, ToolError::Execution { code, .. } if code == "no_match"),
            "{error:?}"
        );
    }

    /// The model's own domain list reaches the backend, and the sources come
    /// back where the model will read them: a summary with no URLs under it is
    /// a claim nobody can check.
    #[tokio::test]
    async fn web_search_passes_the_query_through_and_renders_its_sources() {
        struct Stub;
        impl keke_provider_api::WebSearchBackend for Stub {
            fn search<'a>(
                &'a self,
                query: &'a str,
                allowed_domains: &'a [String],
            ) -> keke_provider_api::ProviderFuture<
                'a,
                Result<keke_provider_api::WebSearchResults, keke_provider_api::ProviderError>,
            > {
                Box::pin(async move {
                    Ok(keke_provider_api::WebSearchResults {
                        summary: format!("{query} confined to {allowed_domains:?}"),
                        citations: vec![keke_provider_api::WebSearchCitation {
                            url: "https://example.test/a".to_string(),
                            title: Some("A".to_string()),
                        }],
                    })
                })
            }
        }

        let (_dir, ctx) = workspace();
        let out = WebSearch::new(Arc::new(Stub))
            .run(
                ctx,
                WebSearchArgs {
                    query: "grok-bot".into(),
                    allowed_domains: vec!["x.ai".into()],
                },
            )
            .await
            .expect("a search");

        assert!(out.summary.contains("grok-bot"));
        assert!(out.summary.contains("x.ai"));
        let rendered = match &out.render()[0] {
            ContentBlock::Text { text } => text.clone(),
            block => panic!("{block:?}"),
        };
        assert!(rendered.contains("https://example.test/a"));
        assert!(rendered.contains('A'));
    }

    /// An empty query is the model's mistake to fix, not a search to run.
    #[tokio::test]
    async fn web_search_refuses_an_empty_query() {
        struct Never;
        impl keke_provider_api::WebSearchBackend for Never {
            fn search<'a>(
                &'a self,
                _query: &'a str,
                _allowed_domains: &'a [String],
            ) -> keke_provider_api::ProviderFuture<
                'a,
                Result<keke_provider_api::WebSearchResults, keke_provider_api::ProviderError>,
            > {
                unreachable!("an empty query must never reach the backend")
            }
        }

        let (_dir, ctx) = workspace();
        let error = WebSearch::new(Arc::new(Never))
            .run(
                ctx,
                WebSearchArgs {
                    query: "  ".into(),
                    allowed_domains: Vec::new(),
                },
            )
            .await
            .expect_err("an empty query");
        assert!(matches!(error, ToolError::InvalidArgs { .. }), "{error:?}");
    }
}
