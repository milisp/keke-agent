# keke

[English](README.md) | [架构设计](docs/architecture.md) | [配置文档](docs/config.md) | [路线图](docs/ROADMAP.md)

keke 是一个专为“零厂商锁定”打造的本地终端编码 agent。
可直接搭配你已有的付费订阅、标准 API Key 或自托管的本地模型使用。

**当前状态：早期阶段（v0.1.x），已可用于日常开发。** 适合个人日常主力使用、脚本/CI 自动化及小团队试用。目前尚不适合用于强监管或要求不可中断的生产流水线——详见[生产与安全性](#生产与安全性)。

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

该脚本会下载适合你平台的最新预编译二进制文件至 `~/.local/bin`（可通过 `KEKE_INSTALL_DIR` 覆盖）。将远程脚本管道传输到 `sh` 会以你的权限执行——如介意可先检查脚本：`curl -fsSL .../install.sh | less`。

### npm 安装

```sh
npm install -g @milisp/keke
```

你也可以直接从 [最新 Release](https://github.com/milisp/keke-agent/releases/latest) 下载二进制文件，或使用 `cargo build --release` 从源码构建。

## 快速试用（30 秒）

```sh
keke doctor                              # 在使用前检查哪些 provider/登录配置能正常解析

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

## 现状

日常开发已完全可用。`keke exec`、TUI 和 ACP 服务器均支持端到端完整运行会话；运行时插件（skills、commands、hooks、MCP servers）遵循 Claude Code 格式安装，仓库自带的插件在你显式批准前会保持非激活状态。在运行中的会话内可通过 `/model` 切换模型（模型与 provider 绑定，避免配置持久化无效组合），并且 agent 支持生成子 agent——分配独立任务的子会话，仅汇总返回最终结果而不会污染完整的搜索 trace。后续规划请参见 [`docs/ROADMAP.md`](docs/ROADMAP.md)。

## 生产与安全性

目前非常适合个人使用、本地/自托管模型以及 CI 一次性任务。在将其托付给任何不容出错的关键场景前，请评估以下事项：

- **沙盒与审批** — 默认的 `approval_policy` 和 `sandbox_mode` 策略较为保守，但建议确认其符合你的运行环境与需求（[`docs/config.md`](docs/config.md)）。
- **插件信任机制** — 仓库自带的插件（hooks、MCP servers）在单纯 `git clone` 后绝不会自动执行；必须由人工确认批准，且该授权基于命令的具体内容而非文件路径。系统未提供任何可全局绕过此门控的开关。
- **成熟度** — 项目处于早期阶段（v0.1.x），目前由单人维护，暂无正式的安全审计或 SLA。路线图中的待补齐项（完整的端到端 MCP 工具调用校验）记录在 [`docs/ROADMAP.md`](docs/ROADMAP.md) 中。

**适合的场景：** 希望摆脱厂商锁定、需要脚本/CI 友好的一次性运行模式、使用本地模型，或希望将 ACP 集成至自己的编辑器/客户端中。  
**暂不适合的场景：** 需要针对所有厂商的即插即用式开箱即用 OAuth、成熟的插件生态市场，或需要商业支持合同。

## 灵感与来源

keke 是一个全新的实现，参考并吸取了 OpenAI 的 **codex**、xAI 的 **grok-build** 以及 **deepseek-harness** 的设计思路。少数模块直接移植并在对应 crate 中注明来源；大部分代码均为原创。设计理念与 CI 强制执行的不变量详见：[`docs/architecture.md`](docs/architecture.md)、[`AGENTS.md`](AGENTS.md)。

## 许可证

Apache-2.0。详见 [`LICENSE`](LICENSE) 与 [`NOTICE`](NOTICE)；从其他项目移植的代码在相应 crate 的 `THIRD_PARTY_NOTICES.md` 中注明来源。
