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
`openai-compatible`. Leaving `kind` out means `openai-compatible`, which is
what every declaration meant before instances existed — so nothing about an
existing config changes.

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

### Web Search

An instance of `kind = "codex"` or `kind = "grok"` can offer the vendor's hosted
web search, which the vendor runs inside the model call:

```toml
[providers.codex.web_search]
mode = "live"              # disabled (default), cached, indexed, live
context_size = "medium"    # low, medium, high
allowed_domains = ["docs.rs", "rust-lang.org"]
include_images = false

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
# Approval policy: "on_request" (default), "on_failure", "never"
approval_policy = "on_request"

# Sandbox mode: "workspace_write" (default), "read_only", "danger_full_access"
sandbox_mode = "workspace_write"

# Reasoning effort: "low", "medium", "high", "xhigh", "max" (default: "medium")
reasoning_effort = "medium"

# Maximum output tokens per model reply (256-200000)
max_output_tokens = 8192

# Compaction configuration
[compaction]
trigger_percent = 80          # Percentage of context window at which compaction triggers
keep_recent_messages = 4      # Messages at the tail always kept verbatim
context_window = 128000       # Context window size compaction measures against

# Model catalog TTL in seconds (0 = ask every time, max 604800 = 7 days)
model_catalog_ttl_seconds = 21600  # 6 hours default

# Subagents: isolated child sessions `spawn_agent` starts
[subagents]
max_concurrent = 3        # How many run at once (1-16); further spawns queue
timeout_millis = 600000   # Wall-clock ceiling per subagent (60000-3600000)

# There is no depth setting: a subagent is never offered `spawn_agent` at all,
# so the tree is one level deep by construction rather than by configuration.

# Plugin timeouts in milliseconds
[plugins]
hook_millis = 30000       # Hook timeout (100-3600000)
mcp_startup_millis = 15000  # MCP server startup timeout
mcp_call_millis = 120000    # Single MCP tools/call timeout
```

## Config Layers

Configuration is loaded from multiple layers, with later layers overriding earlier ones:

1. **Built-in defaults** (compiled in)
2. **User config** — `$KEKE_HOME/config.toml` or `~/.keke/config.toml`
3. **Project config** — `.keke/config.toml` in the workspace root
4. **Environment variables** — `KEKE_*` prefixed (e.g., `KEKE_APPROVAL_POLICY=never`)

Provider declarations accumulate across layers and are keyed by route, so redeclaring a provider replaces that entry rather than the whole set. `[[dir]]` entries accumulate the same way, and are applied on top of the merged layers — above every file, below anything typed on the command line.