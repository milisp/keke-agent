# keke

[English](README.md) | [架构设计](docs/architecture.md) | [配置文档](docs/config.md) | [路线图](docs/ROADMAP.md)

keke 是一个专为“零厂商锁定”打造的本地终端编码 agent。
可直接搭配你已有的付费订阅、标准 API Key 或自托管的本地模型使用。

## 为什么选择 keke？

- **协议：为所有客户端提供 ACP 支持**
  无论是外部客户端集成，还是内部 TUI 与 agent 之间的解耦边界，统一基于开放的 Agent Client Protocol (ACP)。
- **多账户与按目录路由**
  登录多个订阅账户（例如工作账户和个人账户），并根据工作区目录路径自动路由请求。
- **脚本与 CI 优先（`keke exec`）**
  开箱即用支持非交互式一次性执行，专为脚本自动化与 CI 流水线设计。
- **与厂商隔离的引擎核心**
  `keke-core` 内部没有任何厂商特定的逻辑。添加标准模型端点无需修改任何引擎代码——只需在 `config.toml` 中简单声明配置即可。

## 安装

### 脚本安装（推荐）

```sh
curl -fsSL https://raw.githubusercontent.com/milisp/keke-agent/main/scripts/install.sh | sh
```

该脚本会下载适合你平台的最新预编译二进制文件至 `~/.local/bin`，也可以通过 `KEKE_INSTALL_DIR` 指定安装目录。

### npm 安装

```sh
npm install -g @milisp/keke
```

你也可以直接从 [最新 Release](https://github.com/milisp/keke-agent/releases/latest) 下载二进制文件，或使用 `cargo build --release` 从源码构建。

## 快速试用（30 秒）

```sh
keke doctor                              # 检查 provider 和登录配置

# 使用已有付费订阅登录……
keke login codex
keke login grok
# ……或直接配置 API Key
export ANTHROPIC_API_KEY=sk-ant-...

keke exec "what does this project do?"   # 一次性运行，适合脚本与 CI
keke                                     # 交互式 TUI
keke resume                              # 恢复上一次的对话
```

配置文件位置、Provider 声明、API Key、本地模型、多账号、按目录切换账号及其他设置，详见 [`docs/config.md`](docs/config.md)。

## 功能状态

keke 已支持日常开发所需的完整会话。你可以使用 `keke exec` 执行一次性任务，也可以通过 TUI 或 ACP 接入自己的编辑器和客户端。运行时插件（skills、commands、hooks、MCP servers）遵循 Claude Code 格式安装；会话中还可以使用 `/model` 切换模型，并生成独立的子 agent 处理分配的任务。

后续规划请参见 [`docs/ROADMAP.md`](docs/ROADMAP.md)。

## 许可证

Apache-2.0。详见 [`LICENSE`](LICENSE) 与 [`NOTICE`](NOTICE)。设计背景与代码来源说明见 [`docs/architecture.md`](docs/architecture.md#why-it-is-shaped-this-way) 及相关 crate 中的 `THIRD_PARTY_NOTICES.md`。
