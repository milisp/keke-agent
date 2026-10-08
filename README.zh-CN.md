# keke

[English](README.md) | [架构设计](docs/architecture.md) | [配置文档](docs/config.md) | [路线图](docs/ROADMAP.md)

**keke** 是本地编码 agent：约 7 MB 的原生二进制，不需要 Node 或 Python
运行时，带的是完整的 agent 能力，而不只是一个能跑 shell 的聊天框。

子 agent、MCP、skills、兼容 Claude 目录结构的插件、hooks、规划模式、
按轮次的检查点、操作系统级沙箱、ACP（桌面应用与 TUI 走同一套会话），以及多供应商路由。
记忆和仓库自带的插件默认关闭，直到你打开它们。

[![asciicast](https://asciinema.org/a/1266474.svg)](https://asciinema.org/a/1266474)

## 为什么选择 keke？

- **完整的 agent，不是薄 CLI。** 交互式 TUI 与面向脚本和 CI 的 `keke exec`
  共用一套会话协议。并发子 agent（有上限，子 agent 不能再派生子 agent）、后台
  shell 任务，以及 `/loop`：一条稍后会再送回来的提示。规划模式可以调研并写下计划，但不能改仓库。

- **用你已经在付的账。** 用 ChatGPT（Codex）或 Grok 订阅登录，或任意 API Key。
  Codex、Grok、Claude、OpenAI 兼容端点、Ollama、vLLM —— Chat Completions、
  Responses 或 Anthropic Messages。同一供应商可登录多个账户，按所在目录选用。
  兼容端点在 `config.toml` 里写几行即可，不必重新编译；`keke-core` 不含厂商特定代码。

- **能力在，但默认不自动启用。** Skills 可以禁用。持久记忆在设定目录之前是关闭的。
  随仓库提供的 hooks 和 MCP 服务器不会因为 `git clone` 就执行；必须由人批准其确切的命令行，
  而且它们不能把已有的拒绝改回允许。

- **撤销，但不碰你的 git 历史。** 每一轮都把工作区快照存到仓库之外的存储里，
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

- **沙箱与审批** — shell 命令运行在操作系统级沙箱中（macOS 用 Seatbelt，Linux 用 Landlock + seccomp）：默认只能写入工作区、不能改动 `.git`、不能访问网络；需要越过沙箱时每次都会询问你。Windows 没有沙箱，每条命令都会先询问你。仓库自带的配置只能收紧、不能放宽沙箱。详见 [`docs/config.md`](docs/config.md#sandbox)。
- **插件信任** — 仓库自带的插件（hooks、MCP servers）不会仅因 `git clone` 就执行；必须由人按**内容**（而非路径）批准。没有关闭该门禁的开关。

## 许可证

Apache-2.0。见 [`LICENSE`](LICENSE) 与 [`NOTICE`](NOTICE)。设计理由与源码归属见
[`docs/architecture.md`](docs/architecture.md#why-it-is-shaped-this-way)
以及相关 crate 中的 `THIRD_PARTY_NOTICES.md`。
