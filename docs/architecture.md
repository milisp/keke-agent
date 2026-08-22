# Architecture

keke is a multi-vendor terminal coding agent. The design goal is narrow and
specific: **adding a vendor should mean adding two small crates and one line in
the composition root**, with no change to the engine.

## Why it is shaped this way

Three implementations informed the design.

**OpenAI codex-rs** is the cautionary tale. Its own `AGENTS.md` says *"resist
adding code to codex-core"*, and `codex-core` still grew to depend on roughly
fifty internal crates. The lesson is not that codex made a mistake — it is that a
layering rule expressed only in prose does not survive contact with feature work.
keke encodes the rule in `scripts/check-layering.py` and runs it in CI.

What codex got right and keke borrows: **many narrow contributor traits plus one
builder registry** instead of a single god `Plugin` trait, and the split between
*code-extensions* (compiled in) and *data-plugins* (installed at runtime, pure
data). The second sidesteps the Rust dynamic-library ABI problem entirely.

**xAI grok-build** has no `core` crate at all: the core role is split across a
tool ABI crate, a routing crate, and an agent-runtime crate. It also treats ACP
as the *internal* client↔agent protocol rather than an external adapter, and
isolates auth behind a trait-only "dependency-inversion seam" so nothing links
the runtime merely to attach a token. keke borrows the tool trait shape, the
seam-crate technique, and the ACP decision.

**deepseek-harness** supplies the philosophy: everything is a plugin,
registrations are reversible effects, load order is expressed as capability
availability rather than a boot sequence, and every capability is a *seam* with
three roles — definition, provider, consumer. keke adopts the seam discipline and
two of its sharper rules: monotonic denial, and ambiguity as an error.

Where keke deliberately diverges from deepseek-harness: the agent loop itself is
**not** swappable. That is the right call for a TypeScript DI kernel and the
wrong one for a Rust workspace at this scale. One loop, many seams.

## Tiers

Dependencies point strictly downward. Rank is enforced by
`scripts/check-layering.py`, including ordering *within* a tier — two contract
crates growing a mutual dependency is exactly the failure a tier-only check would
miss.

### Tier 0 — contract crates

Types and traits, minimal dependencies. This is the stable surface everything
else is written against.

| Crate | Owns |
|---|---|
| `keke-paths` | `AbsPath` / `RelPath`: absoluteness and UTF-8 checked once, at construction |
| `keke-protocol` | `Message`, `ContentBlock`, `ToolCall`, `SessionEvent` — the vocabulary |
| `keke-tool` | `Tool`, `ToolDyn`, `ToolStream`, `ToolError` — the tool ABI |
| `keke-config-types` | leaf configuration values (`ApprovalPolicy`, `SandboxMode`, …) |
| `keke-provider-api` | `ModelProvider`, `ProviderRegistry`, `StreamChunk` |
| `keke-auth-api` | `AuthProvider`, `CredentialStore`, `LoginUi` |
| `keke-plugin-api` | contributor traits + `ExtensionRegistryBuilder` |

### Tier 1 — engine

`keke-core` (session lifecycle, turn loop, context assembly, tool dispatch,
compaction, rollout log), `keke-config` (layered load), `keke-credentials`
(keyring/file store), `keke-workspace` (filesystem, VCS, process execution).

`keke-core` depends only on tier 0 and contains nothing vendor-specific.

### Tier 2 — plugins

Compiled-in crates that register through tier 0 traits:
`keke-provider-xai`, `keke-provider-chatgpt`, `keke-auth-xai`,
`keke-auth-chatgpt`, `keke-tools`, `keke-mcp`, `keke-hooks`, `keke-skills`,
`keke-plugin`.

### Tier 3 — surfaces

`keke-acp` (protocol), `keke-tui` (ratatui), `keke-cli` (the single binary and
the single composition root).

## The three seams

Each follows the definition / provider / consumer pattern, and each is complete
only when all three roles exist.

### Model providers

`keke-provider-api` defines `ModelProvider`: translate neutral messages into a
vendor's wire format, stream the reply back as neutral `StreamChunk`s. A provider
owns no conversation state, makes no policy decisions, and never runs a tool.

`ProviderRegistry::resolve` returns an error when several providers are
registered and none is configured — a silent pick would turn a misconfiguration
into a mysterious behavior change.

**One client, three formats.** Nearly every vendor speaks one of three wire
formats — OpenAI chat completions, OpenAI Responses, or Anthropic Messages — so
`keke-wire` implements all three once and providers configure it. grok-build
reached the same conclusion; its sampler client speaks all three from one place
rather than one client per vendor.

That makes most vendors *declarative*. A route with a base URL, a credential
name, and a format needs no crate:

```toml
[providers.ollama]
base-url = "http://localhost:11434/v1"
default-model = "gpt-oss:20b-cloud"

[providers.nvidia]
base-url = "https://integrate.api.nvidia.com/v1"
env-key = "NVIDIA_API_KEY"
wire = "responses"
```

A compiled-in provider crate is for a vendor with real behavior of its own — an
OAuth flow, a non-standard error shape, an endpoint outside the three formats.
Declarations accumulate across config layers and are keyed by route, so
redeclaring one replaces that entry rather than the whole set.

### Authentication

`keke-auth-api` is trait-only so vendor auth plugins, the credential store, and
the HTTP layer can all depend on it without any of them pulling in the engine.

Two rules the implementations must honor:

- **Resolve per operation, never cache across operations.** A refreshed token
  must reach the next request without a restart. That only holds if callers
  re-read rather than snapshotting at startup.
- **An empty stored value is absent everywhere.** A blank must never look like a
  configured secret.

Configuration carries `CredentialRef`s — shell-identifier names like
`XAI_API_KEY` — never values, so a settings surface can describe a credential
without ever seeing it.

Login interaction goes through `LoginUi`, supplied by the host. A provider never
touches the terminal, which is what lets the identical flow work in the TUI,
headless, and from an editor over ACP.

**One auth file per vendor**, `$KEKE_HOME/auth.<vendor>.json`, following the
shape codex and grok already use — a `schema_version`, an `auth_mode`
discriminator (`"chatgpt"`, `"oidc"`, `"apikey"`), and the token set. Per-vendor
rather than combined so two vendors refreshing concurrently cannot interleave
writes, and so revoking one does not rewrite the others. fx does the same, and
adds a rule worth having: a file whose permissions are wider than `0600` is
refused rather than read.

If someone has already logged in with the codex or grok CLI, keke reads that
rather than making them log in again. Importing is read-only — keke never writes
to another tool's file — and an explicit `keke login` always wins over an import.

The OS keyring is *shared machine state*, which makes it a hazard for tests: a
suite that reads it passes or fails depending on who is logged in on the machine.
`KEKE_CREDENTIAL_STORE=file` excludes that layer, which is also what a CI runner
or a container with no keyring daemon needs.

### Tools

`keke-tool` defines one `Tool` trait implemented by every tool regardless of
origin — built in, ported, discovered over MCP, or contributed by a plugin.

Two shape decisions are load-bearing:

- `Tool::execute` is an RPITIT returning `impl Future + Send`, so the hot path
  allocates no box. Object safety comes from a separate `ToolDyn` with a blanket
  impl that boxes exactly once, at the erasure boundary. That blanket impl is
  also the single place argument decoding and error mapping happen, so no
  individual tool can get them subtly wrong.
- Execution is a stream with a documented invariant:
  `[Progress(_)*, Terminal(_)]`. The constructors are the only way to build one,
  so the invariant holds by construction; a stream ending without a terminal is
  reported as `tool_stream_no_terminal` rather than treated as an empty success.

## Protocol: ACP, and only ACP

keke speaks the [Agent Client Protocol](https://agentclientprotocol.com) as both
its external editor-embedding protocol and its internal TUI↔agent seam — the
arrangement grok-build uses.

The alternative considered was a private JSON-RPC "app-server", as codex has.
It was rejected: it means owning a schema, a versioning policy, and a TypeScript
codegen pipeline, in exchange for nothing keke needs. codex maintains a whole
crate of no-op derive macros purely to make that codegen cheap enough to compile.

ACP extends by adding methods, which is how login prompts, approvals, and plugin
management reach a client without a second protocol. If a GUI ever needs a
server, it can be an ACP client.

### The surface seam

Surfaces do not speak ACP. They speak `keke_acp::Conversation` — `prompt`,
`cancel`, `respond_to_permission` — and render a stream of `Update`s. Two things
implement it: `LocalConversation`, which drives a session in this process, and
whatever an ACP client attaches to. The terminal interface is written against
the trait alone, which is what lets it work identically attached to a local
session and to an agent across a pipe, and what lets its tests run with no
model, no network, and no terminal (`ScriptedConversation`).

`keke agent stdio` is the mirror image: keke as the ACP *agent*, driving the same
`Conversation` on behalf of an editor. Because both surfaces drive the same
seam, they cannot drift into offering different tools or different policy.

### Asking a person

`ApprovalPolicy` decides *whether* to ask, from the tool's own declared
`ToolKind`; the reviewers decide *what the answer is*. `keke-acp`'s `Approvals`
is an `ApprovalReviewContributor` like any other, which is the only way the
engine learns a surface exists — nothing in `keke-core` knows one does.

Approval runs after the guards and before the tool body. That order is what
keeps denial monotonic: a guard's denial is already final by the time anyone is
asked. **Nobody answering is a denial**, not a permission: `keke exec` has no one
to ask, so a call needing approval is refused unless it is told `--approval
never`.

## Extension points

`keke-plugin-api` defines narrow contributor traits — `ToolContributor`,
`ContextContributor`, `TurnLifecycleContributor`, `ToolLifecycleContributor`,
`ApprovalReviewContributor` — each with defaulted methods, so an extension
implements only what it cares about and adding a point breaks nothing.

Composition is explicit and compile-time. Each extension crate exposes
`pub fn install(registry: &mut ExtensionRegistryBuilder, ..)`, and `keke-cli`
calls them in order. Building the registry is a one-way transition, so the
extension set cannot change mid-session and the engine never locks to read it.

**Denial is monotonic.** Approval reviewers may allow or deny; `ToolGuard`s may
only deny — they have no "allow" result. No ordering of extensions can turn a
denial back into permission. This asymmetry is deliberate: it means a permissive
extension can never override a restrictive one.

## Runtime plugins are data

Code-extensions are compiled in. Runtime-installable plugins are *data* — a
manifest contributing skills, MCP servers, hooks, and slash commands, and nothing
else. Resolution is inert (resolving activates nothing), and every manifest
resource must be contained under its package root.

This is codex's split, and it is what lets keke have runtime plugins at all
without a stable dynamic-library ABI.

## Testing

Three layers, each catching what the others cannot.

**Unit tests** live beside the code and pin behavior described in prose, not
implementation details. `a_permissive_guard_cannot_undo_a_restrictive_one` is
the model: the name states the rule, and the test would fail if the rule broke
however the code was rearranged.

**`keke-test-support`** is a mock inference backend. One scripted `Reply` renders
into all three wire formats, so a provider test asserts the same intent against
whichever format its vendor speaks rather than against a hand-written SSE
fixture. It scripts the cases providers get wrong on their own: tool arguments
split across frames, a 429 carrying `retry-after`, and a stream that never sends
its terminal frame.

**End-to-end tests** launch the real binary. `crates/keke-cli/tests/end_to_end.rs`
runs `keke exec` against the mock and asserts that the model was offered the
tools that exist, that the tool ran against the real workspace, and that the
session log replays the exchange. Every other test exercises one crate; this one
exercises the wiring, which is the part that can be connected to nothing and
still compile.

Two rules the suite must keep:

- **No test may read shared machine state.** The OS keyring is the trap: a suite
  that reads it passes or fails depending on who is logged in on the machine.
  `KEKE_CREDENTIAL_STORE=file` excludes that layer, and the end-to-end fixture
  sets it. The same applies to another tool's `~/.codex` — imports are read in
  tests only from a fixture directory.
- **A layering violation is a test.** `scripts/check-layering.py` runs in CI and
  fails on a dependency pointing upward or sideways, including within the
  contract tier. The rule that lives only in prose is the rule that gets lost.

## Sourcing code from the references

codex-rs and grok-build are both Apache-2.0. grok-build's method — source porting
with a prominent per-crate notice rather than a dependency — is the one keke
follows. See invariant 10 in `AGENTS.md`.
