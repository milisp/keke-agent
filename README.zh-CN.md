# keke

[English](README.md) | [架构设计](docs/architecture.md) | [配置文档](docs/config.md) | [路线图](docs/ROADMAP.md)

**keke** 是用 Rust 编写的本地终端编码 agent，目标是零厂商锁定。  
**下载约 7 MB · 无外部运行时依赖 · 即开即用。**

[![asciicast](https://asciinema.org/a/eUqMzR5n59Pfsta5.svg)](https://asciinema.org/a/eUqMzR5n59Pfsta5)

## 为什么选择 keke？

- **用你已经在付的账**  
  用 ChatGPT（Codex）或 Grok 订阅登录，或直接填入任意 API Key。不必为了试另一个模型而换供应商。

- **多账户、按目录路由**  
  工作和私人账户各登录一次。keke 会根据当前目录自动选用对应账户。

- **适合脚本和 CI**  
  `keke exec "..."` 以非交互方式跑一次性任务，可直接放进脚本和流水线。

- **与厂商隔离的核心**  
  `keke-core` 不含任何厂商特定逻辑。在 `config.toml` 里写几行，即可指向任意 OpenAI 兼容端点。

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

- **沙箱与审批** — `approval_policy` 和 `sandbox_mode` 可按使用方式配置（[`docs/config.md`](docs/config.md)）。
- **插件信任** — 仓库自带的插件（hooks、MCP servers）不会仅因 `git clone` 就执行；必须由人按**内容**（而非路径）批准。没有关闭该门禁的开关。

## 许可证

Apache-2.0。见 [`LICENSE`](LICENSE) 与 [`NOTICE`](NOTICE)。设计理由与源码归属见
[`docs/architecture.md`](docs/architecture.md#why-it-is-shaped-this-way)
以及相关 crate 中的 `THIRD_PARTY_NOTICES.md`。
