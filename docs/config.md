# Configuration

keke is configured through a TOML file at `$KEKE_HOME/config.toml` (defaults to `~/.keke/config.toml`). All settings are optional — built-in providers work without any configuration.

## What You Can Point It At

Built-in providers can be used with the following credentials:

| Provider | Authentication | Notes |
| --- | --- | --- |
| OpenAI / ChatGPT | `keke login codex`, or `OPENAI_API_KEY` | Subscription OAuth or API key |
| Anthropic Claude | `ANTHROPIC_API_KEY` | API key only; the default provider |
| xAI Grok | `keke login grok`, or `XAI_API_KEY` | Subscription login or API key |
| Local (Ollama, vLLM, …) | none | Requests stay on the machine |
| OpenAI-compatible gateway | `env_key` | Company proxies, NVIDIA NIM, and routers |

For example, a local Ollama endpoint needs only a provider declaration:

```toml
[providers.ollama]
base_url = "http://localhost:11434/v1"
default_model = "qwen3.8"
```

## Provider Declaration

Any OpenAI-compatible endpoint can be added by declaring a provider. This is how you use company proxies, NVIDIA NIM, Ollama, vLLM, or any custom gateway without rebuilding keke.

```toml
[providers.my-gateway]
base_url = "https://gateway.example.com/v1"
default_model = "gpt-4o"
env_key = "MY_GATEWAY_API_KEY"
wire = "chat_completions"  # or "responses", "messages"
```

### Built-in Vendors, Twice

A provider entry is an *instance*, not a vendor. `kind` names which built-in
implementation serves it, so one vendor can be registered more than once — at
two addresses, on two accounts — with neither being the other's special case.

xAI is the clearest case: a subscription login and an API key are spent at two
different addresses, and each is refused at the other's.

```toml
# The subscription login, at the proxy where its included hours are spent.
[providers.grok]
kind = "grok"

# The pay-per-token API, on the key in $XAI_API_KEY.
[providers.xai]
kind = "grok"
base_url = "https://api.x.ai/v1"
wire = "chat_completions"
account = "apikey"

# The same vendor again, as a different person.
[providers.grok-work]
kind = "grok"
account = "work@corp.com"
```

`keke --provider xai` then spends the key and `keke --provider grok` the login.

Known kinds are `grok`, `codex`, `anthropic`, `ollama`, and
`openai-compatible`. Leaving `kind` out means `openai-compatible` — except on a
route a built-in already answers to, where it means that built-in, so
`[providers.grok]` configures grok rather than quietly becoming a nameless
endpoint that happens to be called grok.

### Accounts

`keke login` files each login under the identity its token carries (an email,
usually), so one vendor's credential file holds as many accounts as you have.
`account` picks which one an instance authenticates as; leaving it out uses
whichever the file records as active.

The name `apikey` means the long-lived key from `env_key` rather than a login.
An instance naming any *other* account will not fall back to that key: an
instance configured as `work@corp.com` authenticating as whatever key happened
to be exported would spend the wrong quota under the wrong identity.

### Provider Fields

| Field | Required | Description |
|-------|----------|-------------|
| `kind` | No | Built-in implementation serving this instance: `grok`, `codex`, `anthropic`, `ollama`, `openai-compatible` (default) |
| `account` | No | Which stored account to authenticate as; `apikey` means the key from `env_key` |
| `base_url` | Unless `kind` is set | Base URL of the API endpoint |
| `default_model` | No | Model used when none is specified |
| `env_key` | No | Environment variable holding the API key (e.g., `NVIDIA_API_KEY`) |
| `wire` | No | Wire format: `chat_completions` (default), `responses`, or `messages` |
| `ca_cert_path` | No | Path to PEM-encoded CA certificate for corporate TLS-intercepting gateways |
| `proxy` | No | Outbound proxy URL (e.g., `http://proxy.internal:8080`) |
| `proxy_username` | No | Basic-auth username for the proxy |
| `proxy_password_env_key` | No | Environment variable holding the proxy password |
| `headers` | No | Extra HTTP headers sent with every request |
| `web_search` | No | The vendor's own web search — see below. Off unless set |
| `service_tier` | No | Which queue turns start out routed at — `fast` or `flex`. Unset leaves the endpoint's own routing |
| `prompt_cache_ttl` | No | Messages-wire cache breakpoints: `5m` (default), `1h`, or `off`. Other wire formats keep their own cache routing |

A block named after a built-in route — `grok`, `codex`, `anthropic`, `ollama` —
configures that built-in rather than replacing it, so `kind` only needs stating
when you want a different one. Every other route defaults to
`openai-compatible` and needs a `base_url`.

### Prompt Caching

Messages endpoints cache the final tool definition, the system prompt, and recent
completed message boundaries, with at most four breakpoints. Thinking blocks keep
their original signatures and receive no direct cache marker. Set
`prompt_cache_ttl = "off"` for gateways that do not support explicit caching.

```toml
[providers.anthropic]
prompt_cache_ttl = "5m"   # "1h" extends retention; "off" omits markers
```

Codex, Grok, and OpenRouter use the session identity for their respective routing
fields instead. A routing key helps reuse but cannot guarantee a cache hit.

NVIDIA NIM's hosted endpoint (`integrate.api.nvidia.com`) automatically receives
the session ID in `x-multi-turn-session-id` for Chat Completions and Responses
requests, keeping tool steps and resumed turns on the same routing identity.
This is a backend affinity hint, not a guarantee that caching is enabled or hits.
See [NVIDIA session stickiness](https://docs.nvidia.com/nvcf/llm-gateway).
For self-hosted gateways, configure the header through the provider's `headers`.

After an observed cache hit, a request with nonzero input and zero cached tokens
shows a yellow `new session to save {n} tokens` line directly above the TUI input.
The count is the most recent successful hit's cached-input-token count, representing
the cached prefix that is no longer being reused. The notice clears after the next
hit or `/new`; `/clear` retains it because the session continues. This applies to
all providers, including OpenRouter. A miss alone does not identify expiration or
eviction, and the count is not a guarantee of savings from starting a new session.

### Fast Mode

A `codex` instance can buy its turns at a different speed. `fast` asks for
priority routing — the same model at the same reasoning effort, answered
sooner, against a larger share of the account's allowance — and `flex` asks for
the deferred queue, which is cheaper and slower under load.

```toml
[providers.codex]
service_tier = "fast"      # fast (priority routing), flex (deferred)
```

In the TUI, `/fast` turns it on and off for the conversation in front of you,
and `/fast flex` or `/fast off` names a queue outright. The setting above is
only where a session *starts*: unlike `/effort`, a typed `/fast` is not written
back to `config.toml`, because a speed bought for one conversation that quietly
stayed on for every later one is what a person enables once and then pays for
forever. The bar says which queue is in force whenever one is named.

Left unset, keke names no queue at all, which is not the same as asking for the
standard one: the endpoint applies whatever default the model and the account
already have. An account that cannot buy the queue it named is refused by the
vendor, naming the tier, rather than being quietly downgraded here. Only
providers whose vendor sells more than one speed act on it — today that is
`codex`; the rest ignore it rather than failing.

### Web Search

An instance of `kind = "codex"`, `kind = "grok"`, or `kind = "anthropic"`, or a
declared route whose `base_url` is on `openrouter.ai`, can offer the vendor's
web search. On every one but `codex` this becomes an ordinary `web_search` tool
in the model's tool list, dispatched and logged like any other tool call and
answered by a separate request to the vendor; a `codex` instance still hands the
search to the vendor to run inside the model call.

```toml
[providers.grok.web_search]
mode = "live"              # that is the whole of a minimal grok setup

[providers.codex.web_search]
mode = "live"              # disabled (default), cached, indexed, live
context_size = "medium"    # low, medium, high
allowed_domains = ["docs.rs", "rust-lang.org"]
include_images = false
model = "grok-4-fast"      # grok only: which model runs the search

[providers.codex.web_search.user_location]
country = "US"
city = "San Francisco"
timezone = "America/Los_Angeles"
```

`mode` is what the search may reach, and the levels are not degrees of one
setting: `cached` answers only from what the vendor already holds, `indexed`
permits live fetches but confines them to pages it has indexed, and `live`
permits them anywhere. It is off by default because the search happens at the
vendor — no tool call reaches keke, so nothing you approve or guard locally
sees it. `allowed_domains` takes hostnames, not URLs, and a restriction written
against `mode = "disabled"` is rejected at startup rather than silently doing
nothing.

Not every vendor can express every level. xAI's search always fetches live, so a
`grok` instance takes `live` or `disabled`; `cached` and `indexed` are refused at
startup, because approximating either would hand live web access to the
deployment that wrote down it may not have it. On that vendor `context_size`
chooses how many results are paid for, `allowed_domains` confines the search to
those sites (and, with it, to the web — xAI otherwise searches X as well),
`user_location.country` localizes results, and `include_images` has no
counterpart and is ignored.

xAI's search is not reaching a grok *subscription* login as of this writing.
The proxy at `https://cli-chat-proxy.grok.com/v1` accepts a request carrying
the `web_search` tool and answers it without searching — `num_sources_used: 0`,
`num_server_side_tools_used: 0`, and the model replies from what it already
knew. The setting is still accepted there rather than refused, since the shape
keke sends matches xAI's own CLI and the gate is somewhere this end cannot see.
An xAI API key at `https://api.x.ai/v1` is the address to use if you have one:

```toml
[providers.xai]
kind = "grok"
base_url = "https://api.x.ai/v1"

[providers.xai.web_search]
mode = "live"
```

On `grok`, `allowed_domains` is the deployment's policy and the model's own
per-call list can only narrow it — a model naming a site the config left out
does not get it. `model` names which model summarizes the results, since a
search is a self-contained call and need not be answered at conversation rates;
unset, the route's `default_model` runs it, and failing that whatever xAI lists
first.

On `anthropic`, the search runs Anthropic's server-side `web_search` tool.
Only `live` and `disabled` are accepted, for the same reason as on `grok`.
`context_size` sets how many searches one call may make (low 1, medium 3,
high 5), `allowed_domains` and `user_location` are passed through, and
`include_images` is ignored. `model` names which Claude model runs the search;
unset, the route's `default_model` does, and failing that the newest model
Anthropic lists.

```toml
[providers.anthropic.web_search]
mode = "live"
model = "claude-haiku-5-5"   # optional: a cheaper model for the search
```

On an OpenRouter route the search runs OpenRouter's `openrouter:web_search`
server tool, which works for any model: OpenRouter uses the model vendor's own
search where there is one and Exa otherwise. Again only `live` and `disabled`
are accepted, and the mapping is the same as on `anthropic`. OpenRouter lists
too many models for "the first one" to mean anything, so `web_search.model` or
the route's `default_model` must be set, and keke refuses to start without one.

```toml
[providers.openrouter]
base_url = "https://openrouter.ai/api/v1"
env_key = "OPENROUTER_API_KEY"
default_model = "anthropic/claude-sonnet-5.5"

[providers.openrouter.web_search]
mode = "live"
```

On `anthropic` and OpenRouter, as on `grok`, the model's own per-call domain
list can only narrow `allowed_domains`. A call that asks only for domains the
config excludes fails rather than searching the whole web — on `grok` too.

Any other route (`ollama`, or a declared endpoint not on OpenRouter) has no
search to run, and turning one on there is refused at startup rather than
silently ignored.

### Header Values

Header values can be literal strings or environment variable references:

```toml
[providers.internal-gateway.headers]
X-Department-Token = "env:DEPT_TOKEN"
X-Company-User-Id = "milisp-labs"
```

A value of the form `env:VAR_NAME` is resolved from the environment at startup rather than taken literally, so a secret header need not sit in the config file in the clear. `authorization` is reserved for the provider's own credential and cannot be set here.

## Complete Example

```toml
# Local Ollama
[providers.ollama]
base_url = "http://localhost:11434/v1"
default_model = "gpt-oss:20b"

# NVIDIA NIM
[providers.nvidia]
base_url = "https://integrate.api.nvidia.com/v1"
env_key = "NVIDIA_API_KEY"
wire = "responses"

# Corporate gateway with TLS interception and proxy
[providers.internal-gateway]
base_url = "https://gateway.corp.internal/v1"
env_key = "INTERNAL_GATEWAY_API_KEY"
ca_cert_path = "/etc/ssl/certs/corp-root-ca.pem"
proxy = "http://proxy.corp.internal:8080"
proxy_username = "svc-keke"
proxy_password_env_key = "CORP_PROXY_PASSWORD"

[providers.internal-gateway.headers]
X-Department-Token = "env:DEPT_TOKEN"
X-Company-User-Id = "milisp-labs"
```

## Per-Directory Providers

Which account you want is usually a property of the repository you are in, not of the command you are typing. `[[dir]]` entries say that once, and keke picks the provider (and optionally the model) for every session started in a matching tree — the same idea as git's `includeIf gitdir:`.

```toml
[[dir]]
match = "~/work/**"
provider = "grok-work"
model = "grok-4.6"    # optional

[[dir]]
match = "~/oss/**"
provider = "xai"
```

| Field | Required | Description |
|-------|----------|-------------|
| `match` | Yes | Glob matched against the workspace root |
| `provider` | No | Provider route to use in matching directories |
| `model` | No | Model to use in matching directories |

Rules:

- The pattern is matched against the **workspace root** (the enclosing git repository, or the working directory if there is none), so an override follows the repo rather than which subdirectory your shell is in.
- A leading `~` expands to your home directory. Paths are compared with `/` separators on every platform.
- Globbing is deliberately small: `?` is one character, `*` is any run of characters *within* one path segment, and a `**` segment matches any run of segments including none — so `~/work/**` matches `~/work` itself and everything under it.
- When several entries match, **the last one wins**. Write the broad rule first and the narrow rule that refines it below.
- An entry that sets neither `provider` nor `model` is rejected at startup, and so is one naming a provider that is not configured — it names the route and lists the ones that exist.
- `--provider` and `--model` on the command line still win over a directory override.

## Core Settings

These settings live at the top level of `config.toml`:

```toml
# Approval policy: "on_request" (default), "accept_edits", "auto", or "never".
# Old "on_failure" configurations still load, but new setups should use
# "on_request" with sandboxed Bash auto-approval below.
approval_policy = "on_request"

# Sandbox mode: "workspace_write" (default), "read_only", "danger_full_access".
# See "Sandbox" below for what each one enforces.
sandbox_mode = "workspace_write"

# Reasoning effort: "low", "medium", "high", "xhigh", "max" (default: "medium")
reasoning_effort = "medium"

# A persona: who the agent is and how it should behave. It leads the system
# prompt, after keke's own identity and before the project's AGENTS.md, so a
# repository still has the last word on how it wants to be worked on. Blank is
# the same as unset.
#
# Prefer `--instructions` or KEKE_INSTRUCTIONS when one installation drives
# several named agents at once: a persona is usually a property of the run
# rather than of the machine.
instructions = "You are Ada. Review before you write, and say what you changed."

# Maximum output tokens per model reply (256-200000)
max_output_tokens = 8192

# Compaction configuration
[compaction]
trigger_percent = 80          # Percentage of context window at which compaction triggers
keep_recent_messages = 4      # Messages at the tail always kept verbatim
context_window = 128000       # Context window size compaction measures against

# Checkpoints: a snapshot of the working tree per turn, so a rewind (esc esc)
# can put the files back and not only the conversation.
#
# Snapshots live under $KEKE_HOME beside the session log, in a git repository of
# keke's own — never the project's, which keke does not commit to, stash, or
# stage. The project's .gitignore is honoured, so build output and node_modules
# stay out. A turn that only talks costs nothing: the snapshot is taken before
# the turn first runs a tool that can write.
#
# `keep_days` bounds the store: a snapshot older than this is dropped when a
# session opens the store, unless the session that took it is still working. A
# rewind that reaches back past the window finds the snapshot gone and says so.
#
# `max_tree_mb` is the working tree keke refuses to snapshot at all. A snapshot
# holds everything git would not ignore, so a project with a dataset, model
# weights or video in it that no .gitignore covers would be copied whole into
# $KEKE_HOME. Measured once, when the project's store is first created; over the
# limit, keke says so and runs with snapshots off for that project.
[checkpoints]
enabled = true
keep_days = 14
max_tree_mb = 2048

# Getting your attention when the terminal is not in front of you: a tool is
# waiting for approval, or a turn has finished (not one you stopped yourself).
#   method    "auto" (default) | "osc9" | "bel" | "off"
#             auto sends an OSC 9 desktop notification in terminals known to show
#             one (Ghostty, iTerm2, kitty, WezTerm, Warp) and the bell elsewhere.
#             Inside tmux, OSC 9 needs `set -g allow-passthrough on`.
#   condition "unfocused" (default) | "always"
#             unfocused needs a terminal that reports focus changes; one that
#             does not is treated as focused, so it never notifies.
[notifications]
method = "auto"
condition = "unfocused"

# What the model ends its commit messages with, in a git repository. Set it to
# "" to turn signing off — a project's .keke/config.toml can do that for every
# session in it. At most 512 bytes; several trailers go on separate lines.
[attribution]
commit = "Co-Authored-By: keke <noreply@milisp.dev>"

# Model catalog TTL in seconds (0 = ask every time, max 604800 = 7 days)
model_catalog_ttl_seconds = 21600  # 6 hours default

# Subagents: isolated child sessions `spawn_agent` starts
[subagents]
max_concurrent = 3        # How many run at once (1-16); further spawns queue
timeout_millis = 600000   # Wall-clock ceiling per subagent (60000-3600000)
collect_timeout_millis = 600000 # Default cancellable `collect_agent` wait window
                                # (10000-3600000, capped by timeout_millis).
                                # Returns as soon as any child finishes.
                                # Long waits avoid empty parent-model polling;
                                # user cancellation interrupts the wait.

# There is no depth setting: a subagent is never offered `spawn_agent` at all,
# so the tree is one level deep by construction rather than by configuration.

# Skills a plugin ships that this deployment does not want
# The TUI /skills panel also saves source and individual choices for the next
# launch. Configuration denials below cannot be overridden from the panel.
[skills]
disabled = ["acme:review", "deploy", "noisy-plugin:*"]
# Each entry is `plugin:name`, a bare `name` matching that skill in every
# plugin, or `plugin:*` for all of one plugin's. A disabled skill is not listed
# for the model, not offered as a slash command, and cannot be read by name.
# `disabled` accumulates across layers: a project can turn one off without
# restating the user's list, and no layer can re-enable what a broader one
# refused. An empty entry is an error rather than a request to disable
# everything.

# Plugin timeouts in milliseconds
[plugins]
hook_millis = 30000       # Hook timeout (100-3600000)
mcp_startup_millis = 15000  # MCP server startup timeout
mcp_call_millis = 120000    # Single MCP tools/call timeout
```

## Sandbox

Every shell command the model runs — `bash`, in the foreground or the
background — runs under a sandbox the operating system enforces: Seatbelt
(`/usr/bin/sandbox-exec`) on macOS, Landlock plus seccomp on Linux. The
defaults follow codex's.

| Mode | Reads | Writes | Network |
|---|---|---|---|
| `workspace_write` (default) | anywhere | the workspace, the temporary directory, `/tmp`, and `writable_roots` | off unless `network_access = true`; per-command domain and port approvals need macOS, or Linux with the namespace backend (see below) |
| `read_only` | anywhere | nowhere | off |
| `danger_full_access` | anywhere | anywhere | on |

```toml
sandbox_mode = "workspace_write"

[sandbox_workspace_write]
auto_approve_bash = true       # default; sandboxed Bash skips ordinary approval
network_access = false          # true grants unrestricted networking
allow_local_binding = true      # macOS: localhost TCP listeners, no outbound grant
redirect_package_caches = true  # cache-only paths in temp; toolchains/credentials unchanged
proxy_allowed_ports = [80, 443] # user may add 22/8443; projects may only remove ports
allowed_domains = ["github.com", "api.github.com", "registry.npmjs.org", "crates.io"]
denied_domains = []             # deny wins over allow, including wildcard matches
deny_read = ["/Users/me/private-credentials"] # absolute paths
writable_roots = ["/Users/me/.cache/sccache"]   # absolute paths, writable too
```

On macOS, workspace-write Bash uses a command-scoped CONNECT proxy automatically;
no hand-written allow list is required. Bash accepts a per-command `allowed_domains`
argument naming exact HTTPS hosts, for example `api.github.com`. There is no `gh`
special case. In Auto mode the guardian reviews the command and declared domains
together; grants last for that execution only. An undeclared destination is denied
without another guardian review. The result names the domain and instructs the
model to add it to `allowed_domains` and rerun inside the sandbox.

In human approval modes, a new domain prompts during execution and an ordinary
allow lasts for the session. “Allow this domain permanently” records it under
`$KEKE_HOME/network-permissions.json` for future human-mode sessions. Auto does not
reuse human session or persistent grants. Missing reviewers and refusals fail
closed. Never automatically approves declared and runtime domains without prompting,
but explicit denials and public-address checks still apply. Static `allowed_domains`
configuration remains optional for trusted endpoints. Explicit denials always
win, including over approved command endpoints.

Seatbelt permits only its `127.0.0.1` port, not arbitrary outbound connections.
Separately, `allow_local_binding = true` (default) permits localhost TCP servers
in macOS workspace-write mode. Use `127.0.0.1` or `::1` as the dev-server host;
wildcard/LAN binds such as `0.0.0.0` are not granted. A host browser can access
the server, including WebSocket traffic. Binding permission does not grant a
sandboxed `curl`, test runner, or browser client access to arbitrary localhost
services. Bash can declare `allowed_local_ports: [3000]` to approve localhost TCP
client access together with the command, for example `curl http://127.0.0.1:3000`.
This supports direct HTTP, HTTPS and WebSocket traffic on the approved ports,
including clients that do not use the domain proxy. Each call requires approval
(including Auto guardian review, except Never automatically authorizes); no port consent is remembered for the session
or permanently. Grants apply only to the command and its descendants, including
background commands until they exit. A grant can reach any host service owning
that port, not just a workspace dev server, so do not approve sensitive local
services casually. Port 0 and read-only are rejected. On Linux the grant needs the
namespace backend described below; without it Linux rejects the capability rather
than ignoring it. Projects may disable
binding but cannot restore it when the user disabled it. Read-only remains closed.
The child receives upper- and lowercase HTTP/HTTPS/ALL proxy variables and empty
NO_PROXY variables, except command-approved local access sets loopback exclusions;
Seatbelt still denies every other local port. Clients that ignore these settings remain blocked rather
than running unconfined. CONNECT and plain HTTP GET/HEAD downloads use only
`proxy_allowed_ports` (default `[80, 443]`). This is a user-authorized transport
ceiling separate from domain consent: granting a domain permits the configured
ports, not just HTTPS. Explicit `[]` denies all proxy transports; zero/duplicate
ports are invalid. Projects may only remove inherited ports, never add them.
HTTP bodies, pipelining, upgrades, SOCKS, IP literals and private DNS destinations
are refused. Use separately approved
local TCP ports for loopback services. HTTP downloads are not encrypted.
For macOS Git SSH, the default transport is routed through `/usr/bin/ssh` with an
`nc -X connect` ProxyCommand to the same domain gate. Add port 22 in the user config
for ordinary SSH, or use an SSH endpoint on an already permitted port. Domain
approval still applies and direct SSH remains blocked. Existing `GIT_SSH` or
`GIT_SSH_COMMAND` overrides are preserved, but cannot bypass Seatbelt. Standalone
SSH is not rewritten; configure its ProxyCommand explicitly. Host-key checks and
agent consent remain intact, and agent sockets still require `allowed_unix_sockets`.
CONNECT is an opaque TCP tunnel: allowing port 22/8443 does not verify the remote
protocol, and the proxy is not a TLS inspection boundary.
Domain names match case-insensitively; `*.example.com` matches subdomains, not
the apex. Redirects, download CDNs, enterprise hosts, and wrapped `gh` invocations
request additional domains during execution in human modes; Auto must declare
them and rerun instead of requiring an escape.
An allowed destination can still receive data from the workspace: the proxy
checks destinations, not tunnel contents, and does not inspect TLS.

**Host services and cache writes.** Bash accepts `allowed_unix_sockets`, exact
absolute socket paths such as the resolved Docker socket or `$SSH_AUTH_SOCK`.
macOS workspace-write grants outbound connection access only to reviewed paths;
other sockets remain blocked. Resolve shell variables before declaring paths.
The consent is per command, never stored. Docker access can control containers
and host mounts; SSH agent access can request signatures, so neither is a harmless
cache permission. Read-only rejects scoped socket grants. On Linux they need the
namespace backend and a kernel whose Landlock supports ABI 9 (`ResolveUnix`); on an
older kernel the call fails before the command starts rather than allowing every
socket, and abstract Unix sockets are scoped away from the command.

`redirect_package_caches = true` redirects only npm, Bun, pip, uv and XDG cache
environment variables to a stable per-workspace temporary location. The tools
create their caches there; keke does not copy or change the user's existing caches.
Cargo/Rust toolchain roots, credentials, HOME, PATH and install destinations stay
unchanged. Temp cleanup may remove the cache. Shell-level overrides remain subject
to filesystem permissions. The user may disable the redirects; projects may only
disable, never restore an inherited disabled setting.

Bash also accepts `additional_write_roots`, exact absolute existing directories,
reviewed with the command. This allows Cargo's existing registry/cache directories
or a chosen install destination without escaping the filesystem sandbox. Keke
canonicalizes approved roots, refuses missing directories rather than creating them
on the host, and keeps grants command-scoped; existing metadata protections apply
on macOS (Linux Landlock cannot carve out metadata). Grant narrow cache directories,
not HOME, the whole filesystem or directories holding toolchains/credentials.
Read-only rejects extra roots. Never authorizes them without review; Auto reviews
them with the command, and human modes ask afresh, without permanent or session grants.

Do not combine domain lists with `network_access = true`; configuration rejects
that bypass. Nonempty allow lists replace grants from earlier layers; denials
accumulate. Project configuration may add denials but cannot grant domains.
`deny_read` blocks file reads through Seatbelt, including configured paths and
their resolved targets; it does not remove inherited environment secrets or
implement credential masking. No credential paths are denied by default.
Linux supports an opt-in namespace filesystem backend configured by the user
with `linux_namespace_launcher = "/usr/bin/bwrap"` in
`[sandbox_workspace_write]`. Install non-setuid bubblewrap first. The launcher
and its ancestors must be root-owned and not group/world writable; repository
configuration cannot select it. Namespace creation is actually probed and
failure is fatal. This backend masks `deny_read` paths and overlays protected
metadata read-only, retaining Landlock/seccomp. It currently masks `/proc`
entirely, so workflows requiring procfs are not supported by this first stage.
With this backend the command runs in a private network namespace whose only
loopback is its own. A broker outside the namespace relays what the command may
reach: the domain proxy, and the ports approved through `allowed_local_ports`,
which are forwarded to the host's loopback. A command that was not granted a
destination cannot connect to it, and a host service is reachable only on a port
approved with that command.

Because that loopback is private, a dev server bound to `127.0.0.1` inside the
sandbox is not visible to a host browser. Bash declares `exposed_local_ports:
[3000]` to publish named ports on the host's loopback for that command only. It
requires `workspace-write` with `allow_local_binding` enabled, rejects port 0,
and is approved with the command like `allowed_local_ports` (never remembered).
It exposes the server to host processes; it gives the command no access to host
services. Without the namespace backend Linux rejects it.

Without that backend, Linux refuses domain networking or read denials rather than claiming
restrictions its launcher cannot enforce.

**Stopping a command stops its processes.** Every command leads its own process
group, and keke signals the group rather than only the shell. A timeout, a
cancelled turn, `kill_task`, clearing the task list and keke exiting all
reach a dev server the shell started. Foreground commands are sent SIGKILL;
background tasks get SIGTERM first and SIGKILL after `background.kill_grace_millis`,
so a server can remove its socket. A process that detaches into a new session or
group has left the group and is out of reach of this; on the Linux namespace
backend the namespace ends with the launcher, which takes such a process with it.
A foreground command returns when its shell exits, and anything it left running
in the background is stopped at that point, so a stray `&` neither holds the call
open until its timeout nor outlives it. A server meant to outlive the call needs
`background: true`. Plugin hooks and MCP servers get the same treatment: a hook
that times out, or an MCP server whose connection ends, takes down the launcher's
descendants (`npx`, `uvx`) with it.

Proxy resource budgets are configurable in the same section:
`proxy_max_connections = 32`, `proxy_header_timeout_ms = 10000`,
`proxy_connect_timeout_ms = 10000`, `proxy_idle_timeout_ms = 60000`,
and `proxy_poll_interval_ms = 10`. Values must be positive and are bounded;
projects cannot change them. Active workers are stopped and joined when the
sandbox is dropped; system DNS resolution may delay shutdown because the OS
resolver is not cancellable.

With `approval_policy = "on_request"` and `workspace_write`, enforced sandboxed Bash runs without
an ordinary approval prompt when `auto_approve_bash = true`, except when the call
predeclares network access: that access is reviewed together with the command.
Set it to `false`
to put Bash through ordinary approval before it runs. Edit tools still follow the
approval policy. This setting does not approve sandbox escapes: every
`bash_unsandboxed` call requires review unless the user selected Never. A repository may turn auto-approval
off, but may not turn it back on after the user turned it off.

`approval_policy = "auto"` sends ordinary approval requests to the model-backed
guardian reviewer. Its model defaults to the session model and can be selected
with `[guardian] provider` and `model`. A failed or unclear review denies the
call. Sandbox escapes also go to the guardian, on every call. `never` automatically
authorizes execution permissions, including escapes and scoped domain/port/socket/write
grants, without review; it is not Auto mode. It does not remove explicit guard
denials, sandbox restrictions, tools requiring a person's answer, or repository
plugin-content execution consent. Unconfined execution still requires using the
escape tool; Never does not silently run a failed sandboxed command unrestricted.

In Auto mode, `write_file`, `edit`, and `apply_patch` are decided by the paths
they write rather than by the reviewer: a call whose every path — for a patch,
every added, deleted, updated, and moved-to file — resolves inside the
workspace, after following symlinks, and outside `.git`, `.agents`, and
`.keke`, runs without review. Any other edit goes to the reviewer as before.
Other policies are unaffected; `on_request` still asks about every edit.

`approval_policy = "accept_edits"` accepts the built-in file edit tools
without an ordinary prompt. Commands and sandbox escapes keep their normal
approval rules.

```toml
[guardian]
provider = "codex"
model = "your-reviewer-model"
```

The optional guardian model selection should be a model available through the
named provider. `[guardian] enabled = true` also lets it answer ordinary
`on_request` approvals; Auto mode uses it regardless of that setting. A
repository's config cannot choose or enable the guardian reviewer.

The old `on_failure` policy is accepted for existing configurations and
sessions, but is no longer offered in the interactive policy cycle. It skips
ordinary approval checks even for operations outside sandboxed Bash, so
`on_request` plus `auto_approve_bash` is the clearer choice.
Repository config cannot change `on_request` to `accept_edits`, `auto`,
`on_failure`, or `never`;
that choice belongs in the user's config.

### Calls Only a Person May Approve

`require_approval` names tools whose every call needs a person's answer, under
every approval policy:

```toml
require_approval = ["*stripe*", "acp:gmail:send_*", "*purchase*"]
```

- **Patterns.** Matched against the full tool id, case-insensitively; `*`
  stands for any run of characters, `:` included. MCP tools are
  `<plugin>:<server>:<tool>`, and servers sent by an ACP client are
  `acp:<server>:<tool>`.
- **Every policy.** `never` still asks. `auto` does not let the guardian
  answer: an automatic reviewer is not a person. Where nobody can be asked (a
  non-interactive run, a client that refuses), the call is denied with a reason.
- **Once.** A standing "allow always" never answers for a matching call, and
  surfaces do not offer one: the terminal hides `a`, and ACP requests omit the
  allow-always option.
- **Layers.** Rules accumulate. A project may add rules but cannot remove the
  person's. `--require-approval PATTERN` (repeatable) and
  `KEKE_REQUIRE_APPROVAL` (comma-separated) add to the configured rules, for a
  program that launches keke for a particular agent. An empty entry in a file is
  a configuration error.
- **Over ACP.** A request only a person may answer carries
  `_meta["keke.dev/person-required"] = true`, and `initialize` reports
  `_meta["keke.dev/require-approval"] = true`. Both are keke extensions, not
  standard ACP fields. A client that answers on someone's behalf (an unattended
  run, say) must pass such a request to a person or refuse it.
  Every permission request also names its tool in `_meta["keke.dev/tool"]`
  (`bash`, `acp:shop:create_payment`): ACP's `kind` is `other` for every MCP
  tool, too coarse for a client to remember an answer by.

Rules match names, not effects. They cannot recognize a payment made through a
generic tool such as a shell command or a browser click; pair them with the
sandbox, network rules and the provider's own account scopes.

**Workspace metadata stays read-only.** Inside a writable root, `.agents` and
`.keke` cannot be written by a sandboxed command, and neither can the parts of
the workspace's `.git` that Git executes or obeys: `hooks`, `config`,
`config.worktree`, `info`, and `modules`. A hook written there would run
outside the sandbox the next time you commit. The rest of `.git` is writable,
so `git add` and `git commit` are ordinary sandboxed Bash commands and need no
escape. `.keke` in the workspace and the protected Git paths are protected even
before they exist. This is enforced on macOS; Landlock cannot carve an
exception out of a grant, so on Linux those directories are as writable as the
rest of the workspace.

**Stepping outside.** When a command fails because of the sandbox — it needs
the network or writes outside the workspace — the model can
rerun it with `bash_unsandboxed`, giving a reason. In Auto mode the guardian
reviews every escape; human policies ask you every time, and Never skips execution
review. `background: true` explicitly requests a tracked unconfined task, with that
lifetime included in review; it ends on exit or `kill_task`. A standing "allow
always" never skips this review. If the required reviewer is unavailable,
the call is refused; `keke exec` can review escapes in Auto mode.

**`read_only` writes nowhere.** `write_file`, `edit`, and `apply_patch` write
from keke's own process, where no sandbox reaches, so under `read_only` they
are not offered at all — nor is `bash_unsandboxed` — and a guard denies those
tool names even if a plugin supplies one. Asking a person is not a boundary a
client that approves on your behalf can be held to. If you want writes you
review, use `workspace_write` with `approval_policy = "on_request"`.

**Choosing the mode per run.** `--sandbox <mode>` (or `KEKE_SANDBOX`) overrides
`sandbox_mode` for one run. It is your own command line, so it may name any
mode, including `danger_full_access`; `read-only` spelled with hyphens works
too.

**Choosing the mode over ACP.** An ACP client sees a `sandbox_mode` config
option ("Sandbox": `read_only`, `workspace_write`, and `danger_full_access` as the
configured mode allows) and may switch it with `session/set_config_option`,
taking effect on the next command or tool listing. The configured mode is a
ceiling: a client may narrow it, never widen it, so a `workspace_write`
deployment offers `read_only` and `workspace_write`, and a `read_only`
deployment offers only itself. A mode this machine cannot enforce is refused
and the previous one stays. The choice is not written to the session log: a
resumed session starts at the configured mode again.

**Where there is no sandbox.** Windows has none — as in codex, where it is
off unless enabled — because what a non-administrator can build there cannot
block the network and leaves any directory others may write writable. So on
Windows, under any mode but `danger_full_access`, every `bash` command asks you
first, and `keke exec` refuses to run commands. Set `sandbox_mode =
"danger_full_access"` to run them without asking. On Linux, a kernel without
Landlock (before 5.13, or with it disabled) refuses to start a session and
says why, rather than running commands unconfined.

**A repository cannot loosen the sandbox.** `.keke/config.toml` arrives with
`git clone`, so a project layer may tighten `sandbox_mode` or turn
`network_access` off, but asking for a looser mode, network access, or extra
`writable_roots` is an error naming the file. Put those in
`$KEKE_HOME/config.toml` to grant them. (codex instead ignores an untrusted
project's config and lets a trusted one loosen anything; keke has no project
trust store for configuration, and tighten-only needs none.)

## MCP Servers From an ACP Client

An editor speaking ACP can send `mcpServers` with `session/new`,
`session/load` and `session/resume`, and keke connects them like any other MCP
server. `initialize` advertises what is accepted: stdio and streamable HTTP in
both protocol versions, and HTTP+SSE in v1 (v2 has no SSE).

- **Per session.** The servers sent with a request belong to that session. On
  load or resume the servers in that request are used, not the ones the
  original session had.
- **Tool names.** Tools appear as `acp:<server>:<tool>`.
- **Not trust-gated.** A plugin under the workspace is withheld until a person
  approves it, because the repository chose it. A client's servers are chosen by
  the program that launched keke, not by the repository, so the workspace trust
  gate does not apply and there is no approval step.
- **Name collisions are errors.** Two client servers with one name, or a client
  server named like an enabled configured (plugin or `keke mcp add`) server,
  fail the request with an error naming the server. An empty name, command or
  url is an error too. keke never picks one silently.
- Values may use `${VAR}` references, expanded at spawn or request time as for
  configured servers.

## Memory

Persistent, per-agent memory: markdown notes the model reads and writes across
sessions. Off unless a directory is named.

```toml
[memory]
dir = "/var/lib/bots/ada/memory"   # absolute; unset = memory off
summary_max_bytes = 8192            # 0..=65536; 0 injects no summary
entry_max_bytes = 32768             # 1024..=1048576; larger writes are refused
```

`--memory-dir <PATH>` (env `KEKE_MEMORY_DIR`) overrides `dir` for one run, in
the TUI, `keke resume`, `keke exec`, and `keke agent stdio` alike. A relative
path is resolved against the process's current directory. `dir` in a file must
be absolute, and a repository's `.keke/config.toml` may not set it.

Use one directory per bot: a host running several agents (Codexia, for
example) passes each its own `--memory-dir`. Each entry is `<dir>/<name>.md`,
where `name` is lowercase letters, digits, `-` and `_` (up to 64 characters,
starting with a letter or digit). The model gets `memory_read` and
`memory_write`; neither needs approval.

The system prompt carries a short summary — the entry names and first lines,
cut to `summary_max_bytes` — taken **once when the session starts**. Writes
during the session do not change it (that keeps the prompt cache warm), but
`memory_read` always sees the current files and the next session's summary
includes them.

`sandbox_mode = "read_only"` governs the workspace and the commands the model
runs. Memory is the agent's own state, outside the workspace, so the memory
tools keep working under it.

## Config Layers

Configuration is loaded from multiple layers, with later layers overriding earlier ones:

1. **Built-in defaults** (compiled in)
2. **User config** — `$KEKE_HOME/config.toml` or `~/.keke/config.toml`
3. **Project config** — `.keke/config.toml` in the workspace root
4. **Environment variables** — `KEKE_*` prefixed (e.g., `KEKE_APPROVAL_POLICY=never`)

`[skills] disabled` accumulates across layers too. Provider declarations accumulate across layers and are keyed by route, so redeclaring a provider replaces that entry rather than the whole set. `[[dir]]` entries accumulate the same way, and are applied on top of the merged layers — above every file, below anything typed on the command line.

## Image inputs

`view_image` and images attached in the terminal use the same validated budgets:

```toml
[images]
read_bytes = 20971520
decoded_pixels = 40000000
max_dimension = 2048
encoded_bytes = 3145728
```

`read_bytes` bounds the source file before decoding; `decoded_pixels` bounds
its decoded pixel count. Images exceeding `max_dimension` are resized while
preserving aspect ratio, and output must fit `encoded_bytes` before base64
encoding. Invalid files fail before entering a model request. These limits
apply to newly loaded images, including files outside the workspace that the
process can read.
