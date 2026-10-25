# `circus-sandbox-exec` 本地进程代理

`circus-sandbox-exec` 是独立的 stdio 代理二进制。宿主把已有 Paseo provider 的普通启动命令指向它，它用公开的 `Sandbox` API 在 Docker、containerd 或 Kubernetes 中运行同一份 guest argv，原样转发 stdin/stdout/stderr，并在一次运行结束时把工作区保存为有界归档、销毁沙箱。它不修改 `circus-paseo/`，不是新的 provider，也不是 HTTP/RPC 服务。

```text
circus-sandbox-exec --config /absolute/config.json [--] <guest argv...>
```

配置路径之后的所有参数都是 guest argv，与 provider 追加参数的方式一致，例如 `codex app-server ...` 或 Pi/Claude 的 CLI 参数。空 argv、空可执行文件和非 UTF-8 参数会在访问后端前被拒绝。`--` 只消费一次，后续 `--` 属于 guest。

## 构建与特性

在 `circus-sandbox/` 执行：

```bash
cargo build --locked -p circus-sandbox-exec
cargo build --locked -p circus-sandbox-exec --features containerd
cargo build --locked -p circus-sandbox-exec --features "containerd kubernetes"
```

默认构建只含 Docker 后端。`containerd` 需要 `protoc` 并使用 `circus-sandbox/containerd`；`kubernetes` 使用显式 kubeconfig。未编译的特性会在连接前返回明确的不可用错误，不会静默降级到 Docker。

## 配置

配置文件由可信宿主准备，最多 256 KiB，严格拒绝未知字段，`version` 必须为 `1`。所有路径必须是绝对路径；archive 与 state 路径必须不同，父目录必须已经存在。二进制不提供默认路径、默认镜像、默认环境或宿主凭据。

```json
{
  "version": 1,
  "backend": {
    "kind": "docker",
    "endpoint": "unix:///var/run/docker.sock",
    "image": "python:3.12-slim",
    "runtime": "runc"
  },
  "imagePullPolicy": "local-only",
  "imagePlatform": { "os": "linux", "architecture": "amd64" },
  "imagePrepareTimeoutMs": 120000,
  "limits": {
    "memoryBytes": 536870912,
    "cpuMillis": 1000,
    "maxProcesses": 128,
    "workspaceBytes": 134217728,
    "maxFileBytes": 2097152
  },
  "isolation": {
    "workspacePath": "/workspace",
    "guestUid": 1000,
    "guestGid": 1000
  },
  "io": {
    "outputLimitBytes": 1048576,
    "commandTimeoutMs": 30000,
    "fileTimeoutMs": 60000,
    "lifecycleTimeoutMs": 60000
  },
  "workspace": {
    "archivePath": "/var/lib/circus/sessions/agent.snap",
    "maxTotalBytes": 67108864,
    "maxEntries": 4096
  },
  "statePath": "/var/lib/circus/sessions/agent.state.json",
  "agent": {
    "workingDir": "project",
    "env": { "TERM": "dumb" },
    "timeoutMs": 3600000,
    "outputLimitBytes": 8388608
  }
}
```

- `backend.kind`：`docker`、`containerd` 或 `kubernetes`；其余字段与公开 `BackendConfig` 描述符一致。Kubernetes 还可选 `context` 与 `serviceAccount`。
- `imagePullPolicy`：`local-only`（默认语义，只检查本地）或 `if-missing`（本地缺失时允许一次无认证拉取）。Kubernetes 不由此二进制准备镜像，仍由节点 kubelet 按 `IfNotPresent` 拉取。
- `imagePlatform`：Docker/containerd 必填；显式 `linux` 与 Docker 架构名（如 `amd64`、`arm64`）。不会从宿主推断。Kubernetes 可省略。
- `imagePrepareTimeoutMs`：Docker/containerd 必填的正数；覆盖连接、检查、可选拉取与最终检查。
- `limits`：交给 `ResourceLimits::new`，包括内存、milli-CPU、pids、工作区字节与单文件上限。
- `isolation`：工作区容器路径与 guest uid/gid。代理固定以 `no-new-privileges`、全部 capability 丢弃、只读根与禁网构造 `IsolationConfig`，没有关闭这些项的配置字段。Kubernetes 后端要求 `guestUid` 非零（Pod `runAsNonRoot`）。
- `io`：`IoLimits::with_deadlines` 的输出上限与命令/文件/生命周期期限，均为正整数毫秒。
- `workspace.archivePath`：调用方拥有的工作区归档宿主路径；首次运行缺失表示新工作区。
- `workspace.maxTotalBytes`：`SnapshotBudget` 的文件内容总预算，同时用于推导有界 manifest 传输上限（另加固定与每条目 allowance）。
- `workspace.maxEntries`：`SnapshotBudget` 的文件/目录条目预算。
- `statePath`：恢复状态 JSON 的宿主路径；Unix 上以 `0600` 原子写入。
- `agent.workingDir`：`null` 或 workspace 相对目录；拒绝绝对路径、`..`、反斜杠与盘符。
- `agent.env`：显式 guest 环境覆盖；不复制宿主环境。名称必须是可移植变量名，值不得含 NUL。**不得放入模型或其他宿主凭据**；模型代理由后续独立代理承担。
- `agent.timeoutMs`：guest 进程总期限；包含 I/O 背压与暂停时间。
- `agent.outputLimitBytes`：stdout 与 stderr 各自的字节上限，不是两者合计。

`isolation` 字段没有“关闭隔离”的布尔值：构造时四项加固始终为真。调用方仍需自行限制配置目录、归档目录和状态目录的权限；这些路径是可信输入，不是租户能力凭证。

## 生命周期

1. 校验配置、guest argv 与目标父目录，然后注册 SIGINT/SIGTERM；此时尚未连接后端。
2. 读取 state JSON。存在时先校验后端种类一致，再调用 `Sandbox::recover_orphan`。`StillPresent` 或 `Unknown` 立即失败关闭，绝不创建第二个实例；只有 `ConfirmedAbsent` 才继续。缺失 state 时生成 `RecoveryId` 并在创建前写入 `creating`。
3. Docker/containerd 按 `imagePullPolicy` 准备镜像，创建描述符使用不可变 ID/引用。Kubernetes 不执行准备步骤。
4. 归档存在时用调用方 `maxFileBytes` 与预算加载；无效归档在创建沙箱前失败。
5. 用持久化 `RecoveryId` 创建沙箱；若加载了归档，在 `Created` 阶段恢复，然后 `start`。
6. 读取 state 为 `running`，`spawn_process` 一个进程会话：宿主 stdin 原样写入，EOF 时关闭 guest stdin；guest stdout/stderr 原样写到宿主对应流。stdout 不混入任何日志或 JSON 帧。
7. guest 正常退出：在沙箱仍运行时捕获工作区，原子保存归档，销毁沙箱并写 `closed`，退出码等于 guest 退出码。
8. 捕获失败：仍销毁沙箱，保留旧归档不变，以非零码报告可操作错误。
9. SIGINT/SIGTERM、stdin EOF（关闭 guest stdin 后等待最多 5 秒让 guest 自然退出）或宿主输出失败：尝试捕获并保存，取消并销毁沙箱。未确认清理时以非零码退出并保留 state，供下次运行恢复。
10. `ProcessExit::Terminated` 表示进程会话已经尝试清理。代理不会把一次未确认的结果改写成成功：先按沙箱状态决定能否捕获，再用 `destroy` 重试未确认的清理；已确认时写 `closed`，否则保留 state。

`state` 只有三种保守阶段：`creating`（实例可能存在，绝不可假定不存在）、`running`（当前代理持有活动实例）、`closed`（确认实例不存在且暂存清理完成）。状态文件先写于任何可能的实例创建之前；读取损坏、版本不符、recovery id 非法或后端种类不一致都会失败关闭。

## 退出码与诊断

- guest 正常退出：`0..=255` 原样返回；超出范围或负数以 `1` 报告。
- SIGINT / SIGTERM：`130` / `143`。
- stdin EOF 的关闭意图在归档保存和清理确认后返回 `0`；同样条件下归档保存失败或清理未确认则非零。
- 配置或 CLI 错误：`2`。
- 归档、后端、创建、启动、进程或未确认清理错误：`1`。
- stderr 只输出静态分类；不回显 argv、环境值、配置正文、归档内容或后端 SDK 细节。
- 代理不是 TTY，不转发 guest 进程信号，不提供 pause/resume 或后台存活；宿主断开 stdin 且 guest 不退出时，最迟在 5 秒宽限后取消会话。

## 验证

```bash
cargo test --locked -p circus-sandbox-exec
cargo clippy --locked -p circus-sandbox-exec --all-targets -- -D warnings
```

真实 containerd 验收默认忽略，只操作 `circus-sandbox-{recoveryId}` 容器，并要求显式环境。测试覆盖：首次运行写入工作区并产生归档；第二次运行恢复文件；state 为 `closed` 且容器被删除；SIGKILL 代理后留下孤儿，下一次运行先 `recover_orphan` 再创建。

```bash
CIRCUS_CONTAINERD=1 \
CIRCUS_CONTAINERD_SOCKET=/run/containerd/containerd.sock \
CIRCUS_CONTAINERD_NAMESPACE=circus-proxy-test \
CIRCUS_CONTAINERD_IMAGE=python:3.12-slim \
cargo test --locked -p circus-sandbox-exec --features containerd --test containerd_proxy -- --ignored
```

`CIRCUS_CONTAINERD_ARCH` 可显式覆盖镜像架构；缺省仅接受可映射的 `x86_64` / `aarch64` 宿主。测试使用的 `CIRCUS_CONTAINERD_NAMESPACE` 必须是已存在的自有 namespace；测试只创建和删除 `circus-sandbox-{recoveryId}`，不执行全局 prune。单元测试不连接后端，也不证明隔离。真实运行仍需 Linux containerd、可用的 `runc`、本地预拉取镜像与 `ctr`。

## 边界

- 本 PR 不修改 Paseo，也不提供模型代理、凭证注入、任务协议或 UI。
- 归档只保存普通文件与目录，不是进程/内存检查点；不保存被排除的运行时状态。
- 代理不自动安装镜像、不清理共享镜像、不使用 ambient Docker/Kubernetes 配置。
- 配置、归档与状态路径属于可信宿主；不要把租户可控路径直接当作这些值。同一个 `statePath`/`archivePath` 只能由一个代理进程使用；并发共享会产生竞态，且没有分布式锁。
