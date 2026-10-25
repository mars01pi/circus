# Repository Guidelines

## Project Overview

Circus 是面向 AI Agents 的沙箱项目：`circus-sandbox` 承担 Rust 核心，`circus-paseo` 提供桌面与 provider 接入。沙箱目前提供 workspace 文件库、受预算约束的进程内代码快照及其版本化有界文件归档、资源请求校验、本地 OCI 镜像解析与有界预启动池、固定提交 Git clone/worktree、Docker 工作区文件快照、调用方持久化恢复标识的孤儿恢复，以及显式配置的本地 Linux Docker Engine + `runc` 异步后端（含每沙箱一个交互进程会话）；`agent-runner` 支持调用方用明文工作区归档和 Codex 原生线程 ID 串行多轮，Paseo sandbox provider 也通过 stdio bridge 的可选字段按每次提示重建沙箱并延续已完成轮次，但不恢复进行中的 turn。不要把完整交付规划写成现有能力。

- **当前开发阶段**：工作聚焦 `circus-sandbox` 的功能开发；现有工程门禁继续适用，不为新增功能重复建设工程基础设施。除非用户明确要求，不主动修改 `circus-paseo`。
- **Paseo 暂不开发**：说明与开发规则见 [circus-paseo/CLAUDE.md](circus-paseo/CLAUDE.md)。除非任务明确要求，不修改其源码、依赖或配置。
- **沙箱开发入口**：[circus-sandbox/AGENTS.md](circus-sandbox/AGENTS.md)，当前 API、配置与限制见 [circus-sandbox/README.md](circus-sandbox/README.md)。七个领域及 Kubernetes、gVisor/Kata/Firecracker 均保留在[完整交付计划](circus-sandbox/docs/delivery-plan.md)中；当前有 Docker、containerd + runc 与 Kubernetes workload 三个后端切片，不代表全范围完成。门禁与命令见[工程说明](circus-sandbox/docs/engineering.md)，安全扫描约定见[安全说明](.github/sandbox-security.md)。按当前检出描述实现，不以规划或工作流存在代替交付与运行证据。

## Architecture & Data Flow

- 现有 Paseo：移动端/Web/桌面/CLI → 客户端协议层 → Node.js daemon，负责 Agent 管理及实时输出；这不是已接入 Rust 沙箱的链路。
- 当前沙箱：文件调用方 → `Workspace` → cap-std `Dir`；Docker 调用方 → `api` → 私有 `lifecycle` → `infrastructure::Engine` → 本地 Linux Docker Engine；containerd 调用方 → `api` → 私有 `lifecycle` → `infrastructure::Backend` → 本地 containerd；Kubernetes 调用方 → `api` → 私有 `lifecycle` → `infrastructure::Backend` → Kubernetes API（显式 kubeconfig，namespace 作用域）。交互进程调用方 → `Sandbox::spawn_process` → `lifecycle::process` → 后端 attach 传输（每沙箱一个进程，I/O 任务只报告结果，删除仍由 lifecycle 拥有；Docker/containerd 用原生 exec，Kubernetes 经 `pods/exec` 的 argv trampoline）。`ResourceLimits` 校验请求值，Docker/containerd/Kubernetes 后端将显式资源与隔离配置映射到容器或 Pod；校验本身不构成强制、预留或聚合准入。
- 目标沙箱：统一 API → 生命周期管理 → 基础设施调度 → 运行时隔离；镜像环境、代码工作区提供执行资源，预启动/快照/沙箱池优化启动。
- 目标约束：对外 API 屏蔽后端差异；领域层与部署、隔离适配器分离。生命周期统一拥有状态与回收逻辑，调用方不得绕过它直接操作后端。已实现：Docker/containerd 后端、Kubernetes workload Pod（创建/就绪/exec/有界文件/工作区文件快照/删除/池租约/交互进程）、环境准备计划与稳定标识、固定提交 Git clone/worktree、工作区文件快照与预启动池、交互进程会话。Kubernetes 真正 pause/resume（B03）、gVisor、Kata、Firecracker、容器/VM 运行时快照（进程或客户机内存检查点）仍待实现；没有 RPC 或 Paseo 链路。

## Key Directories

| 路径                    | 用途                                                                                                                                                                                                                                                                              |
| ----------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `circus-sandbox/`       | Rust library crate；workspace、进程内代码快照及其有界文件归档、资源/配置校验与 Linux Docker / containerd + runc、Kubernetes workload API，及对应 examples/tests、许可证策略。                                                                                                     |
| `circus-sandbox/src/`   | 公开 `api`、`runtime` 及根门面；私有 workspace、lifecycle、infrastructure 实现。其余领域目录不代表能力已交付。                                                                                                                                                                    |
| `circus-sandbox/tests/` | 公开文件、代码快照与配置测试；Docker、containerd 与 Kubernetes 集成测试默认忽略，须显式连接真实 daemon 或集群。                                                                                                                                                                   |
| `circus-sandbox/docs/`  | 交付计划与工程事实。架构细节不在根指南复制。                                                                                                                                                                                                                                      |
| `circus-paseo/`         | 已有 npm workspace 应用；内部结构与开发流程交由其 `CLAUDE.md` 说明。                                                                                                                                                                                                              |
| `.github/workflows/`    | Paseo CI 仍按路径触发。沙箱包含 `sandbox-metadata.yml`、`sandbox-quality.yml`、`sandbox-security.yml`、`sandbox-rust.yml`、`sandbox-docker.yml`、`sandbox-containerd.yml`、`sandbox-kubernetes.yml` 和 `sandbox-architecture.yml`。各自范围见工程说明；它们不证明进程或容器隔离。 |

## Development Commands

根目录没有统一构建入口。沙箱命令在 `circus-sandbox/` 执行；rustup 读取该目录的 `rust-toolchain.toml`（Rust 1.99.1）。`Cargo.lock` 由 Cargo 生成后提交，检查使用 `--locked`。

| 目的              | 命令                                                                                                             |
| ----------------- | ---------------------------------------------------------------------------------------------------------------- |
| 格式检查 / 格式化 | `cargo fmt --check` / `cargo fmt`                                                                                |
| Clippy            | `cargo clippy --locked --all-targets -- -D warnings`                                                             |
| 构建              | `cargo build --locked`                                                                                           |
| 测试              | `cargo test --locked`                                                                                            |
| 文档              | `RUSTDOCFLAGS=-D warnings cargo doc --locked --no-deps`                                                          |
| 运行示例          | `cargo run --locked --example workspace`                                                                         |
| 资源请求示例      | `cargo run --locked --example resource_policy`                                                                   |
| 依赖策略          | 先 `cargo install --locked --version 0.20.2 cargo-deny`，再在 `circus-sandbox/` 执行 `cargo deny --locked check` |

上表的 fmt、clippy、build、test、doc 与 example 也是 `Sandbox Rust` 在 Windows 与 Linux 上的源码检查。cargo-deny 与 CodeQL 的范围、许可证 allow，以及 `io-lifetimes` / `windows-sys` 传递重复见[沙箱指南](circus-sandbox/AGENTS.md)。

文档与 QA 命令在仓库根目录执行：

```text
npm --prefix circus-sandbox/qa ci
npm --prefix circus-sandbox/qa audit --audit-level=low
npm --prefix circus-sandbox/qa run check
npm --prefix circus-sandbox/qa test
```

QA 固定 Node.js 24.18.0 与 npm，不继承 Paseo 的 Node 版本。`ci` 和 `audit` 是 npm 内置命令，不是 package script。检查使用现有 `check.mjs`、markdownlint promise API 与锁文件，不重新引入 markdownlint-cli2/braces。白名单、`format` 和安全扫描入口统一见工程说明。

语义架构域检查另需 `cargo install --locked --version 0.28.0 cargo-modules`，然后在仓库根目录执行 `npm --prefix circus-sandbox/qa run architecture`。固定域矩阵与分析边界只维护在[工程说明](circus-sandbox/docs/engineering.md#架构域门禁)；不要使用该版本检查未过滤图的 `--acyclic` 代替域门禁。

以下仅是已有 Paseo 命令，**在 `circus-paseo/` 下执行，且仅用于明确要求的 Paseo 任务**：

| 目的               | 命令                                          |
| ------------------ | --------------------------------------------- |
| 安装依赖 / 构建    | `npm ci` / `npm run build`                    |
| 启动 daemon / 前端 | `npm run dev` / `npm run dev:app`（分别启动） |
| 类型检查 / lint    | `npm run typecheck` / `npm run lint`          |
| 格式检查           | `npm run format:check`                        |
| 单文件测试         | `npx vitest run <file> --bail=1`              |

## Code Conventions & Common Patterns

- 沙箱采用 Rust 职责模块、显式 `Result` 错误及构造参数依赖注入。文件 API 同步阻塞，异步调用必须 `spawn_blocking`；Docker API 使用 Tokio，不引入第二套异步运行时。工具链、当前直接依赖及选型原则见[沙箱指南](circus-sandbox/AGENTS.md)，版本以 `Cargo.toml` 和 `Cargo.lock` 为准。后端配置已显式建模，配置文件格式与持久化方案尚未选定。
- 外部后端适配与领域逻辑分离；失败、取消和销毁均需明确资源回收责任。安全与状态不变量见沙箱指南。
- 不复制 Paseo 的 TypeScript 状态模型或框架到 Rust 核心；涉及 Paseo 时遵循其本地指南及 `docs/coding-standards.md`。
- 文档区分“当前”与“规划”；子项目细节只维护一份，根指南保留边界与链接。

## Important Files

- [circus-sandbox/AGENTS.md](circus-sandbox/AGENTS.md)：沙箱架构约定、1.99.1 基线与命令。库和 examples 是调用入口，没有 HTTP/RPC 服务。
- [circus-paseo/CLAUDE.md](circus-paseo/CLAUDE.md)：Paseo 权威入口；进一步指向架构、开发、测试与 QA 文档。
- `circus-paseo/package.json`、`circus-paseo/package-lock.json`：现有 npm workspace 命令及依赖锁。
- `circus-paseo/.tool-versions`：Paseo 工具版本声明，不作为沙箱 Rust 版本依据。
- `lefthook.yml`：根提交钩子目前仅覆盖 Paseo 的格式、lint 与类型检查，不覆盖沙箱。
- [.github/sandbox-security.md](.github/sandbox-security.md)：Gitleaks、Dependency review、Actions CodeQL、工具前提和受保护审查边界。安全相关目录由 `.github/CODEOWNERS` 管理。

## Runtime/Tooling Preferences

- Windows 下开发时，如遇环境兼容性、工具链或运行调试问题，可以在 WSL 中开发、调试或运行相关工具。
- 沙箱使用 Rust 1.99.1 / Cargo；版本基线及依赖约束统一维护在[沙箱指南](circus-sandbox/AGENTS.md)。已实现目录不保留 `.gitkeep`；其余占位目录的 `.gitkeep` 仍只用于跟踪空目录。
- Paseo 使用 Node.js（当前声明 `22.20.0`）与 npm workspaces；保留 `package-lock.json`，不用 Bun/Yarn/pnpm 替换。
- Paseo lint/格式化统一走 npm scripts；工具实现以 `package.json` 为准，避免文档中的旧工具名称误导。
- 未获许可不得重启端口 `6767` 的 Paseo 主 daemon，避免中断已有 Agent。

## Testing & QA

- 沙箱测试范围及环境前提见[沙箱指南](circus-sandbox/AGENTS.md)与[用户说明](circus-sandbox/README.md)，没有覆盖率百分比门槛。workspace 与代码快照测试使用真实临时目录，配置测试验证请求边界；默认忽略的 Docker 集成测试必须显式运行，普通 Cargo 检查不证明隔离。文件权限测试不证明同账户或特权进程被隔离；未实现的后端和运行时不能用 mock 声称通过。
- 手写 `.rs` 单文件不超过 1200 行（含 1200）。文档、链接、格式、行数与 QA 回归测试由质量门禁覆盖；这些检查不代替 Cargo 行为测试或真实后端证据。检查结果记在对应 PR。
- Paseo 使用 Vitest、浏览器测试与 Playwright E2E；完整规则见 [测试指南](circus-paseo/docs/testing.md) 和 [QA 指南](circus-paseo/docs/qa.md)。
- 本地仅运行受影响测试文件；禁止默认执行 `npm test` 或整个工作区测试，全量验证交给 CI。Paseo 改动按其指南执行类型、lint 与格式检查。

## Commit & Pull Request Workflow

采用短生命周期分支、小范围 PR；CI 验证可自动检查的契约，Sourcery 辅助代码审查，作者负责确认正确性与风险，不以机器人通过代替实际验证。

### 1. 实现与本地验证

- 从目标分支建立任务分支，一个 PR 只解决一个明确问题；避免混入无关重构、格式化或依赖升级。
- 修改前确认作用域与子项目约定；提交前检查 diff，只暂存本任务文件或片段，不覆盖、提交或清理用户的无关改动。
- 按 `Testing & QA` 执行受影响的检查，并实际运行改变的行为；文档改动只检查事实、路径与引用。未运行或受环境限制的项目必须如实记录。

### 2. 提交与创建 PR

- 提交按逻辑拆分，每个提交保持可理解、可验证；推荐 `type(scope): 简述`，如 `fix(lifecycle): 修复销毁时的资源回收`、`docs(workflow): 补充 PR 流程`。提交说明交代必要的原因与约束，不只复述文件列表。
- 推送前完成本地自审：检查遗漏的调用方、失败路径、测试与文档，以及密钥、临时文件和调试代码。不得用跳过 hooks 的方式掩盖失败。
- PR 标题必须使用 `type: 简述` 或 `type(scope): 简述`：type 以小写 ASCII 字母开头，后续只允许小写 ASCII 字母、数字和连字符，不设类型白名单；scope 可含中文，只要求非空且无括号或换行。冒号后恰好一个空格，随后必须是非空白开头的单行描述。正文包含：问题与改动、作用域/非目标、验证命令与结果、风险及必要的回滚方式；有关联 issue 时链接。未完成实现或验证时保留 Draft，不声明可合并。
- 只有任务明确授权时才执行 commit、push、创建 PR 或合并；要求修改文件不等于授权发布。不得直接推送目标分支。

### 3. CI 与 Sourcery 审查闭环

- PR 提交后并行关注 GitHub Actions CI 和 Sourcery review；两者职责不同，不能互相替代。准备合并前，必须确认审查覆盖最新改动；Sourcery 未触发或未覆盖新增改动时，按其实际支持的方式请求审查，不假定它已经完成。
- Paseo CI 按路径触发，规则见 `.github/workflows/paseo-ci.yml` 与 `.github/paseo-ci-paths.yml`。根文档或沙箱改动可能不触发它；未触发或跳过不等于 Paseo 通过，也不能代替沙箱检查。
- 沙箱五套工作流均在本检出，范围见 [工程说明](circus-sandbox/docs/engineering.md)：元数据门禁为 `PR title`；质量门禁含 npm audit、文档/行数检查、QA 测试、actionlint 和 `CodeQL QA`；安全门禁含全历史 Gitleaks、PR-only `Dependency review` 和 `CodeQL (actions)`；`Sandbox Rust` 在 Windows/Linux 上运行 fmt、clippy、build、test、严格 rustdoc 和 example，并在 Ubuntu 运行 cargo-deny 与 Rust CodeQL；`Sandbox architecture` 在两平台解析真实语义依赖图并执行跨架构域矩阵。结果以对应提交的 Actions 为准，本文不记录某次检查通过，也不以旧提交结果为新提交背书。不为“变绿”修改无关文件。
- `sandbox-metadata.yml` 只在 PR 的 `opened`、`reopened`、`synchronize`、`edited` 事件运行；按 PR 号取消旧元数据任务。`PR title` 用固定版本 Node.js 直接执行 CLI，不安装依赖，标题只通过 `PR_TITLE` 环境变量传入且不回显。仅编辑标题不会触发质量、安全或 Rust 工作流的重型检查；工作流存在不等于已设为 required check。
- CI 失败先读失败 job 的日志，区分代码问题与基础设施故障。代码问题修复并补做针对性验证；明确的临时基础设施故障可重跑并记录原因，不靠反复重跑或放宽检查掩盖失败。
- 逐条判断 Sourcery 和人工审查意见：有效问题修复并说明验证；不采纳时回复技术理由、证据或取舍。安全、隔离、生命周期及资源回收问题必须明确处理，不盲从建议，也不无说明地关闭讨论。
- 审查修复默认追加提交，便于查看增量；只有明确需要整理历史时才重写已推送提交，且不得覆盖他人提交。每轮修复完成本地验证后集中推送，再检查新一轮 CI 与审查；旧提交的绿灯不能为新提交背书。

### 4. 合并与收尾

- 合并前确认：PR 不再是 Draft；所声称的检查有该 PR 中的证据；审查问题已修复或有明确的不采纳说明；无未解决的阻断项或冲突。机器人评论不是批准审查，也不是合并授权。
- `main` 当前不要求非作者批准；任务明确授权时，可在最新 CI 通过、审查问题闭环且无冲突后自动 squash merge。仍须使用 PR，不使用管理员绕过或强推。工作流存在不等于 required status check；缺失或被跳过的必需检查不是实际覆盖。
- 不绕过分支保护或必需检查。若必需检查因路径过滤一直 pending，应处理检查规则或交由维护者处理，不把缺失检查视为成功。
- 推荐 squash merge，使目标分支保持“一项任务一个提交”；若仓库已有合并策略，以仓库设置为准。合并说明保留关键背景与验证证据。
- 合并后查看适用的目标分支 CI；出现回归优先回滚或提交最小修复，不把 PR 阶段通过当作最终保证。确认不再使用后清理任务分支，交付时报告 PR、验证结果与已知限制。
