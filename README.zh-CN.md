# keke

[![CI](https://github.com/milisp/keke/actions/workflows/ci.yml/badge.svg)](https://github.com/milisp/keke/actions)

[English](README.md)

keke 是一个运行在本地终端的编码 agent，可以搭配任意模型使用——你已经订阅的
服务、一个 API key，或者你自己机器上跑的模型。

如果你想在编辑器里用 keke，它通过 stdio 提供 Agent Client Protocol。如果想
在脚本或 CI 里用，用 `keke exec`。如果想用一个它还不认识的模型，在
`config.toml` 里声明即可——引擎里不需要改任何厂商相关的代码。

## 安装

从 [latest release](https://github.com/milisp/keke/releases/latest) 下载
预编译二进制文件，或者用 `cargo build --release` 从源码构建。

## 试一试

```sh
# 用你已经付费的订阅登录
keke login codex
keke login grok

# ……或者直接用 key
export XAI_API_KEY=xai-...

keke exec "what does this project do?"   # 一次性运行，适合脚本和 CI
keke                                     # 交互式 TUI
keke resume                              # 接着上一次的对话继续
keke doctor                              # 查看哪些 provider 和登录能解析成功
```

## 你能接入什么

| Provider | 如何认证 | 说明 |
| --- | --- | --- |
| OpenAI / ChatGPT | `keke login codex`，或 `OPENAI_API_KEY` | OAuth 流程从 codex 移植而来 |
| xAI Grok | `keke login grok`，或 `XAI_API_KEY` | 内置，默认 provider |
| Anthropic | `config.toml` 中的 `env-key` | 声明时用 `wire = "messages"` |
| 本地模型（Ollama、vLLM 等） | 无需认证 | 代码不会离开这台机器 |
| 任意 OpenAI 兼容网关 | `config.toml` 中的 `env-key` | 公司代理、NVIDIA NIM、路由器等 |

内置支持之外的服务，只需要在 `$KEKE_HOME/config.toml` 里加几行，不需要改代码：

```toml
[providers.ollama]
base-url = "http://localhost:11434/v1"
default-model = "gpt-oss:20b"

[providers.anthropic]
base-url = "https://api.anthropic.com"
env-key = "ANTHROPIC_API_KEY"
wire = "messages"
```

`wire` 决定请求格式——`chat-completions`（默认）、`responses`，或
`messages`——所以接入新接口只是一条配置，而不是一次发版。

## 现状

日常使用已经可用。`keke exec`、TUI 和 ACP server 都能完整跑通一次会话；
运行时插件（skills、commands、hooks、MCP servers）按 Claude Code 的格式
安装，仓库自带的插件在你批准之前始终处于未激活状态。在运行中的会话里切换
模型这个功能还没实现——详见 [`docs/ROADMAP.md`](docs/ROADMAP.md)。

## 灵感来源

keke 建立在三个已经验证过的开源 agent 之上：OpenAI **codex** 的 OAuth
登录流程（已移植，并在承载它的 crate 中注明来源）、xAI **grok-build** 的
模型与 wire 覆盖范围，以及 **deepseek-harness** 所倡导的 seam-first 架构。

keke 的不同之处在于，坚决不让厂商相关的知识渗入引擎中间层。codex 自己的
贡献者指南写着 *"resist adding code to codex-core"*，而 `codex-core` 如今
仍然依赖 68 个内部 crate——光靠文字约束没能守住这条线。在 keke 里，
`scripts/check-layering.py` 会在 CI 里强制执行这条规则，所以接入一个新厂商
始终只是一个插件加组合根里的一行代码。详见
[`docs/architecture.md`](docs/architecture.md) 了解设计理由，以及
[`AGENTS.md`](AGENTS.md) 了解不变量。

## 许可证

Apache-2.0。详见 [`LICENSE`](LICENSE) 与 [`NOTICE`](NOTICE)；从其他项目移植
的代码会在承载它的 crate 的 `THIRD_PARTY_NOTICES.md` 中注明来源。
