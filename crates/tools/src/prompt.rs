//! Turn context the built-in tools need in front of the model.
//!
//! `apply_patch` takes one string in a grammar the model must produce exactly,
//! and a tool description is a poor place for a grammar — it is advertised once
//! per schema and competes with the argument docs for the model's attention.
//! The format therefore travels as a context fragment, contributed by the crate
//! that owns the tool rather than hardcoded in the engine: `keke-core` must not
//! learn what any particular tool's arguments look like.

use keke_plugin_api::ContextContributor;
use keke_plugin_api::ContextFragment;
use keke_plugin_api::ExtFuture;
use keke_plugin_api::ExtensionContext;

/// Tool guidance sorts after the harness identity and the deployment persona.
const ORDER_TOOL_GUIDANCE: i32 = 100;

/// The `apply_patch` patch language.
///
/// This is the envelope OpenAI's codex and xAI's grok-build both put in front
/// of their models, reproduced because frontier models have been trained on it.
/// Wording that drifts from what they were trained on costs accuracy, so this
/// text tracks the published format rather than being rephrased for taste.
pub(crate) const APPLY_PATCH_FORMAT: &str = r#"# `apply_patch`

Use the `apply_patch` tool to edit files. Its `patch` argument is a
file-oriented diff format — an envelope holding one or more file sections:

*** Begin Patch
[ one or more file sections ]
*** End Patch

Each section starts with one of three headers:

*** Add File: <path> - create a new file. Every following line is a `+` line holding the initial contents.
*** Delete File: <path> - remove an existing file. Nothing follows.
*** Update File: <path> - patch an existing file in place.

An update may be followed immediately by `*** Move to: <new path>` to rename
the file, and then by one or more hunks. Each hunk opens with `@@`, optionally
followed by the name of the enclosing function or class. Within a hunk every
line starts with ` ` for context, `-` to remove, or `+` to add.

Give three lines of context above and below each change. If a change is within
three lines of a previous change, do not repeat the shared lines in both hunks.
If three lines of context do not uniquely identify the location, name the
enclosing scope after `@@`, and repeat `@@` to narrow further:

@@ class BaseClass
@@     def method():
[3 lines of pre-context]
-[old_code]
+[new_code]
[3 lines of post-context]

End a hunk with `*** End of File` when it matches the end of the file.

The full grammar:

Patch := Begin { FileOp } End
Begin := "*** Begin Patch" NEWLINE
End := "*** End Patch" NEWLINE
FileOp := AddFile | DeleteFile | UpdateFile
AddFile := "*** Add File: " path NEWLINE { "+" line NEWLINE }
DeleteFile := "*** Delete File: " path NEWLINE
UpdateFile := "*** Update File: " path NEWLINE [ MoveTo ] { Hunk }
MoveTo := "*** Move to: " newPath NEWLINE
Hunk := "@@" [ header ] NEWLINE { HunkLine } [ "*** End of File" NEWLINE ]
HunkLine := (" " | "-" | "+") text NEWLINE

One patch may combine several operations:

*** Begin Patch
*** Add File: hello.txt
+Hello world
*** Update File: src/app.py
*** Move to: src/main.py
@@ def greet():
-print("Hi")
+print("Hello, world!")
*** Delete File: obsolete.txt
*** End Patch

Paths are relative to the workspace root. A patch applies whole or not at all,
so a failure leaves every file untouched — do not re-read files to check
whether a patch landed; the call fails if it did not. Prefer `apply_patch` for
a change spanning several files, and `edit` for a single exact replacement."#;

/// Contributes the patch-format guidance for [`crate::ApplyPatch`].
pub(crate) struct BuiltinToolGuidance;

impl ContextContributor for BuiltinToolGuidance {
    fn contribute_turn_context<'a>(
        &'a self,
        _ctx: &'a ExtensionContext,
    ) -> ExtFuture<'a, Vec<ContextFragment>> {
        Box::pin(async move {
            vec![ContextFragment::new(
                "tools/apply-patch-format",
                ORDER_TOOL_GUIDANCE,
                APPLY_PATCH_FORMAT,
            )]
        })
    }
}
