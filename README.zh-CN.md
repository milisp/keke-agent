# keke

[English](README.md) | [架构设计](docs/architecture.md) | [配置文档](docs/config.md) | [路线图](docs/ROADMAP.md)

**keke** 是用 Rust 写的本地编码 agent。用你已经在付费的 ChatGPT 或 Grok 订阅登录，或任意 API Key，都在同一个工具里完成。
TUI、`keke exec` 和桌面应用（通过 ACP）共用同一个会话。

子 agent、MCP、skills、兼容 Claude 目录结构的插件、hooks、规划模式、
按轮次的检查点和操作系统级沙箱都已内置。
记忆和仓库自带的插件默认关闭，直到你打开它们。

[![asciicast](https://asciinema.org/a/1266474.svg)](https://asciinema.org/a/1266474)

## 为什么选择 keke？

- **一个会话，所有入口。** 交互式 TUI 与面向脚本和 CI 的 `keke exec`
  共用一套会话协议。并发子 agent（有上限，子 agent 不能再派生子 agent）、后台
  shell 任务，以及 `/loop`：一条稍后会再送回来的提示。规划模式可以调研并写下计划，但不能改仓库。

- **用你已经在付的账。** 用 ChatGPT（Codex）或 Grok 订阅登录，或任意 API Key。
  Codex、Grok、Claude、OpenAI 兼容端点、Ollama、vLLM —— Chat Completions、
  Responses 或 Anthropic Messages。同一供应商可登录多个账户，按所在目录选用。
  兼容端点在 `config.toml` 里写几行即可，不必重新编译。引擎 crate 不能依赖任何厂商插件，
  违反时 CI 会直接失败（[`scripts/check-layering.py`](scripts/check-layering.py)）。

- **默认关闭。** Skills 可以禁用。持久记忆在设定目录之前是关闭的。
  随仓库提供的 hooks 和 MCP 服务器不会因为 `git clone` 就执行（见[安全](#安全)）。

- **检查点在仓库之外。** 每一轮都把工作区快照存到仓库之外的存储里，
  回退 agent 的修改既不是 stash，也不是一次提交。

## 安装

### npm

```sh
npm install -g @milisp/keke
```

也可以从
[最新 Release](https://github.com/milisp/keke-agent/releases/latest) 直接下载二进制，
或用 `cargo build --release` 从源码构建。

### Shell

```sh
curl -fsSL https://raw.githubusercontent.com/milisp/keke-agent/main/scripts/install.sh | sh
```

脚本会把适合当前平台的最新预编译二进制下载到
`~/.local/bin`（可用 `KEKE_INSTALL_DIR` 覆盖）。把远程脚本管道进
`sh` 会以你的权限执行——若在意这一点，先检查内容：
`curl -fsSL .../install.sh | less`。

## 快速试用（30 秒）

```sh
keke doctor                              # 先看清哪些 provider / 登录能解析，再依赖其中之一

# 用已有付费订阅登录……
keke login codex
keke login grok
# ……或直接提供 Key
export ANTHROPIC_API_KEY=sk-ant-...

keke exec "what does this project do?"   # 一次性运行，适合脚本与 CI
keke                                     # 交互式 TUI
keke resume                              # 接上一次对话
```

Provider 路由、API Key、本地模型、网关、按目录账户及其他设置，见 [`docs/config.md`](docs/config.md)。

## 安全

- **沙箱与审批** — shell 命令运行在操作系统级沙箱中（macOS 用 Seatbelt，Linux 用 Landlock + seccomp）：默认只能写入工作区、没有直接联网能力；macOS 上，以及 Linux 启用可选的 bubblewrap 后端时，域名和端口可按命令逐次审批；Git 的 hooks 和 config 只读（macOS 上强制）。越过沙箱时每次都会审查：Auto 模式由 guardian 审查，其他模式由你确认；`approval_policy = "never"` 会跳过审查。Windows 没有沙箱，每条命令都会先询问你。仓库自带的配置只能收紧、不能放宽沙箱。运行环境无法强制执行沙箱时，CI 会失败而不是跳过。详见 [`docs/config.md`](docs/config.md#sandbox)。
- **插件信任** — 仓库自带的插件（hooks、MCP servers）不会仅因 `git clone` 就执行；必须由人按**内容**（而非路径）批准，且它们不能把已有的拒绝改回允许。没有关闭该门禁的开关。

## 许可证

Apache-2.0。见 [`LICENSE`](LICENSE) 与 [`NOTICE`](NOTICE)。设计理由与源码归属见
[`docs/architecture.md`](docs/architecture.md#why-it-is-shaped-this-way)
以及相关 crate 中的 `THIRD_PARTY_NOTICES.md`。
