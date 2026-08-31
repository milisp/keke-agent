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

```mermaid
graph TD
    subgraph Tier 3 [Tier 3 - Surfaces]
        keke-cli --> keke-tui
        keke-cli --> keke-acp
    end

    subgraph Tier 2 [Tier 2 - Plugins]
        keke-provider-grok
        keke-provider-codex
        keke-plan
        keke-mcp
    end

    subgraph Tier 1.5 [Tier 1.5 - Engine-dependent Tools]
        keke-subagent
    end

    subgraph Tier 1 [Tier 1 - Engine]
        keke-core
        keke-config
        keke-workspace
    end

    subgraph Tier 0.5 [Tier 0.5 - Shared Implementation]
        keke-wire
        keke-catalog
        keke-oauth
    end

    subgraph Tier 0 [Tier 0 - Contract Crates]
        keke-paths
        keke-protocol
        keke-tool
        keke-provider-api
        keke-auth-api
        keke-plugin-api
    end

    Tier 3 --> Tier 2
    Tier 3 --> Tier 1
    Tier 2 --> Tier 0
    Tier 1.5 --> Tier 1
    Tier 1 --> Tier 0.5
    Tier 1 --> Tier 0
    Tier 0.5 --> Tier 0
```

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

### Tier 0.5 — shared implementation

Above the contracts and below the vendor plugins that configure them.
`keke-wire` speaks the three inference wire formats; `keke-catalog` keeps a
provider's model list on disk so a picker is drawn without a round trip, and is
still drawn when the vendor cannot be reached at all. `keke-oauth` holds PKCE
and the loopback redirect — the parts of a login the RFCs decide rather than an
issuer. It sits here rather than in a vendor auth crate because an MCP server
behind OAuth is not a vendor and cannot depend on one; before it existed, that
code was in `keke-auth-codex` and `keke-auth-grok` twice, byte-identical apart
from which config struct it read.

### Tier 1 — engine

`keke-core` (session lifecycle, turn loop, context assembly, tool dispatch,
compaction, rollout log), `keke-config` (layered load), `keke-credentials`
(keyring/file store), `keke-workspace` (filesystem, VCS, process execution).

`keke-core` depends only on tier 0 and contains nothing vendor-specific.

### Tier 1.5 — engine-dependent tools

`keke-subagent` sits between the engine and the plugins because it is the one
tool pack that needs the engine itself: a subagent *is* a session, built from a
`SessionBuilder` the composition root already assembled. It stays vendor-free —
the builder carries an `Arc<dyn ModelProvider>` and this crate cannot see which
vendor is behind it.

Two shapes there are worth knowing about before changing it. The recipe is
attached after installation rather than passed to it, because the recipe
contains the very registry installation is contributing to; nothing can hold a
finished builder while that builder's extensions are still being collected. And
a subagent cannot start a subagent — not by a configurable depth limit but by
construction: the host remembers which sessions it created, and a child asking
for its tool set is answered without these tools at all.

A running subagent is also drawn while it runs, and that goes through the same
seam every other live notification does rather than around it. The host
publishes whole snapshots of its rows; `keke-cli`, the only place that can see
both ends, maps them to `keke_acp::SubagentView` and hands the receiver to
`keke_acp::local_with`, which merges them into the surface's `Update` stream.
`keke-acp` therefore does not depend on `keke-subagent`, and `keke-tui` stays
written against `Conversation` alone — a surface across a pipe is served by the
same code path as the in-process one.

### Tier 2 — plugins

Compiled-in crates that register through tier 0 traits:
`keke-provider-grok`, `keke-provider-codex`, `keke-auth-grok`,
`keke-auth-codex`, `keke-tools`, `keke-mcp`, `keke-hooks`, `keke-skills`,
`keke-plugin`, `keke-plan`.

`keke-plan` is the worked example of the split the engine is built around.
`keke-core` carries one bit — `SessionModeSwitch`, is this session planning? —
and `keke-plan` carries what that *means*: the lifecycle, the reminders the
model reads, the guard that refuses edits outside the plan file, the reviewer
that waves the plan file itself through, and `enter_plan_mode` /
`exit_plan_mode`. The switch is authoritative for the coarse bit and the
extension's finer tracker reconciles *to* it at a turn boundary, so a person's
toggle and the extension's own transitions are one fact rather than two that can
disagree. A deployment wanting a different planning discipline replaces the
crate and changes no engine code.

Two consequences are deliberate. Plan mode blocks the *edit* tools and not
`bash`, because a planning agent still has to run `cargo check` and `git log` to
write a plan worth approving. And the guard passes a write to the plan file
through rather than allowing it — guards may only deny — so the approval
reviewer is what auto-approves it, in that order.

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
base_url = "http://localhost:11434/v1"
default_model = "gpt-oss:20b-cloud"

[providers.nvidia]
base_url = "https://integrate.api.nvidia.com/v1"
env_key = "NVIDIA_API_KEY"
wire = "responses"
```

A compiled-in provider crate is for a vendor with real behavior of its own — an
OAuth flow, a non-standard error shape, an endpoint outside the three formats.
Declarations accumulate across config layers and are keyed by route, so
redeclaring one replaces that entry rather than the whole set.

**What a provider serves is part of what it is.** `ModelInfo` carries a display
name, a context window, and the reasoning levels the model accepts — not just an
id. A surface that only has ids makes a person guess which `gpt-5.6-*` is which,
and cannot tell that one of them takes a level the others do not. The listings
disagree about how much they say: the plain OpenAI `/models` is a bag of ids,
while both subscription backends publish the ladder, so decoding the richer
shape lives in the vendor crate and the neutral one lives in `keke-wire`.

A compiled-in vendor also ships its own catalog as a file, and answers from it
when the endpoint cannot be reached. `list_models` on such a provider therefore
never fails and never comes back empty: a person offline, behind a proxy, or not
yet signed in still gets a picker, and one that lists models the endpoint will
honour. The order it tries is cache (while fresh) → vendor → cache (stale) →
compiled-in. How long "fresh" lasts is `model-catalog-ttl-seconds`, a validated
`keke-config-types` field rather than a constant in a plugin, because an
air-gapped install and someone tracking a vendor's weekly releases want
different answers.

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

### Which version of ACP

Both. v1 is what every released client speaks today; v2 is the draft that
folded v1's `session/load` into `session/resume` — one method that restores the
session, replaying the transcript only when the client names a `replayFrom`
cursor — moved the turn's stop reason off the `session/prompt` response onto
the `session/update` state stream, and flattened tool-call creation and update
into one message.

Serving only v2 would refuse every client that exists; serving only v1 would
make keke the reason a client cannot use what it has already implemented. So
`Agent::protocol_router` reads `initialize`, the client picks, and the
connection goes to that implementation — `agent/v1.rs` or `agent/v2.rs`. No
traffic is translated afterwards, which is why they are two handler sets rather
than one with branches. Both drive the same `SessionFactory`, so the versions
cannot drift into offering different sessions.

`Agent.builder()` declares a *v1* endpoint whatever types the handlers are
written against, and `Agent.v2()` a v2 one. Nothing but the wire catches
getting that wrong — the SDK's own client rewrites `protocolVersion` to
whichever version it was built for — so the endpoint is tested by piping bytes
at the binary.

The model a session asks is a `session/set_config_option` under the `model`
category — the picker every v2 client already knows how to draw — offered from
what the provider's own model list returns. Only the model: the provider was
chosen with the credentials the session authenticates with, so routing
elsewhere is a session rather than a setting. It is an `Arc<ModelSwitch>` beside
the config for the same reason the approval policy and effort level are, and
`SessionEvent::ModelRequest` records the model each step named, since a log that
only said what the session opened with could not say which model answered.

`session/list` and `session/resume` are served straight from the rollout logs,
so what a client can list is what `keke resume --list` lists and what it resumes
is what `keke resume` resumes. There is no second record for the two to
disagree about.

A session owns a directory — `sessions/<project>/<session-id>/` — holding
`rollout.jsonl` and a `meta.json` beside it. The log is the record; `meta.json`
is a fold of it, and deleting one costs a rescan and changes no answer. It
exists because a log carries the whole model-visible history on every step and
therefore grows with the square of the turns: listing sessions by parsing them
meant reading every byte of every conversation to print four columns. The fold
is incremental — it records how far it has read — and it notes the offset of the
last `ModelRequest`, which is where the history a resume rebuilds begins, so
continuing a long session reads its last turn rather than all of them.

A subagent's log is one of these too, and `SessionStart` names its parent. A
child's log otherwise looks exactly like a person's, and a listing that could
not tell them apart would offer to continue a conversation nobody had. Its
tokens are billed to the parent, once, where the parent logged `SubagentEnd`.

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

### The format is not keke's

`keke-plugin` reads `plugin.json` and the convention directories the Claude Code
plugin ecosystem already uses — `skills/<name>/SKILL.md`, `commands/*.md`,
`.mcp.json`, `hooks/hooks.json` — plus `.claude-plugin/plugin.json` as a manifest
location. A plugin published for that ecosystem installs here unchanged.

This is the one place keke does not design its own thing, and the reason is not
taste. A plugin system's worth is the plugins available on the day it ships; a
better schema with an empty catalog is worth nothing, and asking authors to
publish twice is asking for the catalog to stay empty. grok-build reached the
same conclusion and reads the same files.

What keke does *not* adopt is the ecosystem's permissiveness about failure:

- **Unknown metadata is ignored; an unknown contribution is reported.** A
  manifest written for a newer host must still load, so a stray `homepage` costs
  nothing. But a `lspServers` block keke does not implement is surfaced to the
  person rather than dropped — silently ignoring it is how an author comes to
  believe a capability is active when nothing runs it. The same applies to a
  hook bound to an event keke has no lifecycle point for.
- **Containment is checked after canonicalization.** A `..` segment or a symlink
  out of the package passes a textual prefix test. Escaping is an error, not a
  warning that skips the entry.
- **Precedence where the ecosystem has precedence, ambiguity everywhere else.**
  The same plugin name in the project and in the user's home is layering, and
  the project copy wins. The same plugin name twice within one scope is an
  error. Contributions never collide across plugins because they are namespaced
  by plugin — `acme:ship`, not `ship` — which removes the class of error rather
  than reporting it.

Discovery covers `$KEKE_HOME/plugins/` and `~/.claude/plugins/` at user scope,
and `.keke/plugins/` and `.claude/plugins/` at project scope. The scope survives
resolution because a plugin the repository controls is not equivalent to one the
person installed, and a trust decision needs that distinction.

### A person's own directory is read as a package

Wanting one slash command or one MCP server should not mean authoring a plugin.
So `$KEKE_HOME` itself, `~/.claude`, `.keke/`, and `.claude/` are read as
packages too, under the names `local`, `claude`, `workspace`, and
`claude-workspace` — a `commands/review.md` dropped into `~/.keke` is `/review`,
and `keke mcp add` writes an ordinary `.mcp.json` beside it.

There is deliberately no second format and no second discovery path. The
consequence that matters is the one at the workspace: a server added with
`keke mcp add --scope project` is in the repository, so it is held back by the
same gate as anything else the repository ships, and `keke plugin trust
workspace` is how a person allows it.

### Signing in to a remote server

An MCP server behind OAuth answers with `401` and a `WWW-Authenticate` header
naming where its metadata lives, and from there the spec is ordinary OAuth 2.1
with three RFCs on top: protected-resource metadata (RFC 9728) names the
authorization server, dynamic client registration (RFC 7591) obtains a
`client_id` — these servers issue none in advance, so there is nothing for a
person to configure — and resource indicators (RFC 8707) keep the token bound to
the server it was minted for.

Two rules shape it:

- **A browser never opens by itself.** Discovery happens inside a turn, and a
  turn that takes over the screen of someone reading something else is a worse
  failure than one that says it needs a login. So a 401 during a turn reports
  `keke mcp login <name>` and stops there; the flow runs only when a person asks
  for it. In the TUI it also does not *launch* a browser — the alternate screen
  owns the terminal, which is the same reason a provider login shows its URL
  instead of opening one.
- **A registration is not a credential.** A `client_id` from dynamic
  registration is public by construction, so it lives in a plain
  `mcp/clients.json` and survives `logout`; the tokens go to the same 0600
  per-vendor store shape every provider login writes to, but under their own
  `mcp/` subdirectory — a project can configure far more remote MCP servers
  than it has providers, and a flat listing shared with provider logins does
  not scale to that. Filed under the server's name *and* a digest of its URL —
  two projects each with a `github` server must not share a token.

### A server is a URL or a program

`.mcp.json` entries carry the ecosystem's `type`: `stdio` (the default, and what
every entry written before remote transports existed is), `http` for the
streamable HTTP transport, and `sse` for the older long-lived-`GET` one. Which
of the two remote shapes a server implements is not something a person should
have to know before they can add it, so keke speaks both.

`McpTransport` is an enum rather than a struct of optional fields, because the
two shapes share nothing: a stdio server has no URL and a remote one has no
child to spawn. Above the connection, nothing knows the difference — the
protocol eras, the framing, and the budgets are properties of MCP, not of the
pipe.

Secrets stay references. A `${VAR}` in an environment value or a header is
expanded at the moment of use and the result is never stored, so a resolved
plugin set — long-lived, cloned, logged — never holds one, and what a person is
asked to approve names variables and headers without their values.

### Cloning a repository is not consent

A plugin under the workspace is content the repository controls. Without a gate,
a `.claude/plugins/*/hooks/hooks.json` in someone else's project runs on the
first turn with everything the agent process has — arbitrary execution from
`git clone`. So a project-scope plugin contributes nothing executable until a
person says otherwise, and `keke plugin trust <name>` is how they say it.

Three properties make that gate worth having:

- **Only execution is gated.** Withholding removes hooks and MCP servers and
  leaves skills and commands. Those are text, and the harness already reads the
  repository's own `AGENTS.md` into the prompt without asking; gating repository
  text *here* would be a policy the rest of keke does not have, applied at the
  one place nobody would look for it.
- **Approval is of contents, not of a path.** The store records the command
  lines themselves, and a plugin that gains a hook after being trusted is
  untrusted again. Otherwise saying yes once is a blank cheque on every future
  commit to that repository — exactly what an attacker would need.
- **There is no flag that turns it off.** A global bypass is what a person
  reaches for once and then leaves on. A deployment that means to run a
  project's plugins says so per plugin, and CI runs `keke plugin trust` as a
  step like any other.

A plugin the person installed into their own directory is not interrogated:
asking about something they placed there themselves would train the answer to
the question that matters into a reflex.

`keke-plugin` sits below the extension crates that consume it: it depends on
`keke-paths` and nothing else, so a manifest can be parsed and listed without
linking the engine. `keke-skills`, `keke-hooks`, and `keke-mcp` each read a
resolved `PluginSet` and register through the ordinary contributor traits, which
is how `keke-core` avoids ever learning that runtime plugins exist.

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
