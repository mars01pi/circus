# containerd + runc 后端

本文记录 `circus-sandbox` 中 containerd 后端的真实范围、配置方式、已执行命令与未验证语义。它只描述当前检出；工作流存在或单元测试通过不代表隔离已在本机验证。

## 实测环境

| 项目                | 值                                                                                                                          |
| ------------------- | --------------------------------------------------------------------------------------------------------------------------- |
| containerd          | 2.2.1（服务端与客户端）                                                                                                     |
| runc                | 1.3.4                                                                                                                       |
| snapshotter         | `overlayfs`                                                                                                                 |
| containerd socket   | `/run/containerd/containerd.sock`                                                                                           |
| namespace           | `default`（测试可用 `CIRCUS_CONTAINERD_NAMESPACE` 覆盖）                                                                    |
| OCI runtime handler | `io.containerd.runc.v2`                                                                                                     |
| 内核                | WSL2 Linux 6.6（cgroup v2，无 `/dev/kvm`）                                                                                  |
| Rust                | 1.99.1                                                                                                                      |
| protoc              | 3.21.12（`containerd-client` 构建期需要）                                                                                   |
| 测试镜像            | `docker.io/library/python:3.12-slim`（OCI index `sha256:a0f57064cbdcd6285fa45f51237ab0b35ea8ebaed06a1f5973e89e19dfb86704`） |

本机 image 已解包为四个 committed snapshot，最终 chain id 为 `sha256:241d372eec8f05a802bb5ca99fba52dd93480b904cb0ca4f591e4f2e27ffd6da`。chain-id 算法在 `containerd_image.rs` 单元测试中与这些真实 snapshot key 对照。

## 构建特性

containerd 后端是可选 Cargo feature：`--features containerd`。它引入 `containerd-client`，该依赖在构建期调用 `protoc` 生成 gRPC 绑定，因此构建机必须安装 protobuf compiler（实测 3.21.12）。未启用 feature 时 `BackendConfig::Containerd` 仍可构造，但 `Sandbox::create` 返回 `SandboxError::UnsupportedCapability`，不会静默降级到 Docker。

```bash
cargo build --locked --features containerd
CIRCUS_CONTAINERD=1 cargo test --locked --features containerd \
  --test containerd_sandbox --test containerd_snapshot --test containerd_process \
  --test containerd_pool -- --ignored --test-threads=1
```

## 配置

```rust
let backend = BackendConfig::containerd(
    "/run/containerd/containerd.sock",
    "default",
    "python:3.12-slim",
    "runc",
    "overlayfs",
);
```

- socket 必须是以 `/` 开头的绝对 Unix socket 路径；`unix://` URL 形式被类型化拒绝。
- namespace 必须非空，仅允许 ASCII 字母数字、`.`、`_`、`-`。
- sandbox 创建只使用本地 reference，不会 pull。熟悉短名会按 containerd 客户端约定回退查询：`python:3.12-slim` 先查原名，再查 `docker.io/library/python:3.12-slim`。显式镜像准备 API 与此独立，见下节。
- runtime 只接受 `runc`；`runsc`（gVisor）在本机不可用，返回 `BackendConfigError::RuntimeUnavailable`，不会静默替换。
- snapshotter 只接受 `overlayfs`。
- 校验在分配任何 snapshot/container/task 之前完成，错误变体为 `BackendConfigError::{InvalidSocketPath, InvalidNamespace, RuntimeUnavailable, UnsupportedSnapshotter, ImageRequired}`。

## 显式镜像准备

Unix 且启用 `containerd` feature 时，`environment::{ContainerdImageRequest, prepare_containerd_image}` 接受显式 socket、namespace、reference、`ImagePlatform` 和 timeout；库不读取环境变量。默认 `ImagePullPolicy::LocalOnly`，缺失返回 `ContainerdImageError::ImageNotFound`。只有调用方选择 `IfMissing` 且 image record 缺失时才进行一次无认证 daemon transfer；已有记录的平台不匹配、内容缺失或 daemon 失败不触发拉取。建议使用完整 registry/repository 名称。

连接、Images.Get、可选 transfer、manifest/config 校验和别名注册共用一个 deadline。显式选择 Linux os/arch[/variant]，不强制宿主平台；读取实际元数据后返回 selected manifest digest、实际平台和同 namespace 的 digest-qualified image record alias。将 `PreparedContainerdImage::reference()` 传给 `BackendConfig::containerd`，而不是重新使用可变 tag。别名存在时 target 的 digest、media type 和 size 必须相同，否则返回 `AliasConflict`，不覆盖；annotations 不参与内容身份比较。

OS/architecture 始终精确校验。请求 variant 为 `None` 表示不约束 variant，允许返回实际观察到的 variant；显式 variant 必须有 selected index/config 元数据证据且匹配。config variant 缺失或 null 时可使用所选 index descriptor 的 variant；两者同时存在但冲突则拒绝。不从宿主推断 variant，direct manifest 若 config 也没有 variant，则无法验证显式 variant 请求。已查询的记录本身就是目标 immutable alias 且直接指向已验证 manifest 时，不重复调用 Images.Create；其他路径仍可能注册 alias metadata。

direct-manifest alias 不保留原 index 的 variant 证据；config 未声明 variant 而依赖 index 验证显式 variant 时，应重复原始 source request 来复用缓存。返回的 alias 仍可用于 `BackendConfig::containerd`，但不保证该 alias 单独重新准备时能满足显式 variant 校验。

Local-only 也可能创建别名 metadata，但不访问 registry。不加载 Docker config、环境凭据或 registry 认证，不重试、不删除共享镜像、不创建容器或解包 snapshot。超时/取消停止客户端等待，不保证 daemon transfer 停止；失败后共享 content、image record 或 alias 仍可能存在。返回值不持有缓存租约，不能防止外部删除，也不保证非宿主平台可运行。sandbox 创建仍不自动拉取，Docker API/default 不变。

示例要求 `CIRCUS_CONTAINERD=1`，且所有连接、镜像、平台、timeout 和 policy 均显式设置；`CIRCUS_CONTAINERD_VARIANT` 可选。`CIRCUS_CONTAINERD_OS` 必须为 `linux`，ARCH 使用 `amd64`/`arm64` 等规范名称，TIMEOUT_SECS 为非零整数，PULL_POLICY 只接受 `local-only` 或 `if-missing`。示例输出真实 alias/digest/platform，再以原始 source request 进行一次 local-only 查询并比较结果，演示源缓存复用：

```bash
# 在 Linux、circus-sandbox 目录执行；daemon 中应已有该 image record/content。
CIRCUS_CONTAINERD=1 \
CIRCUS_CONTAINERD_SOCKET=/run/containerd/containerd.sock \
CIRCUS_CONTAINERD_NAMESPACE=default \
CIRCUS_CONTAINERD_IMAGE=docker.io/library/python:3.12-slim \
CIRCUS_CONTAINERD_OS=linux CIRCUS_CONTAINERD_ARCH=amd64 \
CIRCUS_CONTAINERD_TIMEOUT_SECS=120 CIRCUS_CONTAINERD_PULL_POLICY=local-only \
  cargo run --locked --features containerd --example containerd_image_environment
```

验收缺失拉取路径时，调用方须选一个目标 namespace 中初始不存在的公开镜像（不得删除共享镜像制造缺失），并将上述 IMAGE 替换为它、PULL_POLICY 改为 `if-missing`；需要 daemon 支持 transfer service 及访问目标 registry 的网络。不把命令列在此处当作已运行证据。

协议依据：[containerd Images API](https://github.com/containerd/containerd/blob/v2.2.1/api/services/images/v1/images.proto)、[Transfer API](https://github.com/containerd/containerd/blob/v2.2.1/api/services/transfer/v1/transfer.proto)、[OCI descriptor](https://github.com/opencontainers/image-spec/blob/v1.1.1/descriptor.md) 与 [OCI platform](https://github.com/opencontainers/image-spec/blob/v1.1.1/image-index.md)。

## 创建与运行时语义

`prepare` 只调用 `Version` 与 `Images.Get`，验证 daemon 可达且镜像记录存在；不存在时映射为既有 `SandboxError::BackendImageUnavailable`。它不创建 snapshot。

`create_owned`：

1. 解析镜像 manifest/config（OCI index 按 host 架构选择 `linux/<arch>`），从同一份已加载的 image config 读取 `rootfs.diff_ids` 与 `config.Env`，按 containerd 的 chain-id 规则计算每一层的 committed snapshot key；非法环境元数据失败关闭，不回退为空环境。若目标 committed snapshot 不存在，使用本地 content + diff service 解包（`Snapshots.Prepare` → `Diff.Apply` → `Snapshots.Commit`）；不访问 registry。进程内用一把异步互斥锁串行化解包，避免并发 create 重复 apply。解包期间持有一个 namespace 级 lease（`containerd-lease` gRPC header），active snapshot 使用随机 `extract-<uuid>` key，commit 时以 chain id 命名；完成后在 image config content 上写入 `containerd.io/gc.ref.snapshot.<snapshotter>` 引用，使 committed chain 在 image 存在期间不被 GC 回收，最后 best-effort 删除 lease。
2. 以容器名（`circus-sandbox-<uuid>`）为 snapshot key，从最终 chain snapshot prepare 一个 active snapshot，并写入 owner label。
3. 以真实 OCI runtime-spec JSON 调用 `Containers.Create`：runtime handler `io.containerd.runc.v2`、`snapshotter=overlayfs`、`snapshot_key=<id>`。
4. 用 `Snapshots.Prepare` 返回的 mounts 调用 `Tasks.Create`，**不启动** task；`Sandbox::start` 才调用 `Tasks.Start`。

镜像 `config.Env` 是初始 OCI process 的基础环境，并持久在 containerd 保存的容器 spec 中。后续 `ProcessSession` 与一次性 `exec` 继承该存储的基础环境，不重新解析可变 image tag；交互请求的显式 `ProcessRequest.env` 按变量名覆盖镜像值（包括空值和含 `=` 的值）。固定 `PATH` / `LANG` 仅在对应变量缺失时补齐，不覆盖镜像或请求值；不会复制宿主环境。镜像的 `USER`、`WORKDIR`、`ENTRYPOINT` / `CMD` 不接管执行：guest uid/gid、workspace 工作目录和初始化 argv 仍由 lifecycle 拥有，交互进程的 argv 与相对工作目录仍来自已校验的请求。

OCI spec 的安全设置：

- `root.readonly = true`。
- `process.capabilities` 的 bounding/effective/inheritable/permitted/ambient 全为空；`noNewPrivileges = true`。
- 新建 pid/ipc/uts/mount/network/cgroup namespace；network 项没有 `path`，不是继承宿主或加入已有 namespace。适配器不调用 CNI、不安装 OCI hooks，因此新 namespace 只有 loopback，没有外部接口或路由。
- workspace 为 tmpfs：`rw,nosuid,nodev,uid=<guest_uid>,gid=<guest_gid>,mode=0755,size=<workspace_bytes>`，不是 host bind。
- 额外挂载 `/proc`、`/dev`、`/dev/pts`、`/dev/shm`、`/sys`、`/sys/fs/cgroup`；`maskedPaths`/`readonlyPaths` 与 Docker 默认集合一致。
- 设备 cgroup：默认拒绝，再显式允许标准伪设备（null/zero/full/random/urandom/tty/console/ptmx/pts、loop、tun、fuse）。缺少该 allow list 时 cgroup v2 设备控制器会拒绝 runc 重新打开 `/dev/null`。
- cgroup 资源：memory limit 与 swap 都等于 `memory_bytes`；CPU quota = `cpu_millis * 100`、period = 100000；pids limit = `pids`。
- 未应用 seccomp profile，也未使用 user namespace；见“未验证/未实现”。

exec 与交互进程通过 runc shim 的 FIFO：

- 每个 exec 创建独立临时目录和三只 FIFO，`Tasks.Exec` 传入路径，然后并发打开 reader 与 `Tasks.Start`。
- stdin writer 是自定义 `FifoWriter`：`shutdown` 时真正关闭 fd，guest 才能看到 EOF（`tokio::fs::File` 只 flush 不关闭）。
- 一次性 exec 的 stdout/stderr 使用与 Docker 适配器相同的 `OutputCollector`，强制 per-stream 与 combined 预算；超限时 kill 该 exec、返回类型化 Limit 错误，由 lifecycle 销毁整个实例。
- 交互进程返回有序 `ProcessChunk` 流、可写 stdin 与 exit future；exit 只在输出流结束后调用 `Tasks.Wait`，并 best-effort `DeleteProcess`。

## 所有权与清理

- 容器与 snapshot 都带 `circus.sandbox.owner=<nonce>` 与 `circus.sandbox.name=<id>` label。
- `recover_owned` 先 `Containers.Get` 与 `Snapshots.Stat`。任一资源存在但 owner label 不匹配时返回 `Recovered::Unknown` 并拒绝删除。只有 owner 确认后才 `Tasks.Kill(all=true, SIGKILL)` → `Tasks.Delete` → `Containers.Delete` → `Snapshots.Remove`，最后重新 Get/Stat 确认两者都不存在；未确认时返回 `StillPresent`/`Unknown`，lifecycle 保留 staging 与身份供重试。
- create 中途失败时，已存在的资源由同一恢复路径删除；传输层不确定的失败标记 `create_pending`，避免把可能迟到的实例当作已清理。
- lifecycle 仍是唯一状态机；后端不修改 phase，也不保存 receipt。

## 未验证 / 未实现

- containerd 的 API 不暴露 cgroup 控制器生效值。`SandboxStatus::enforcement` 只报告从**存储的 OCI spec** 读回的结构事实（runtime handler、readonly root、capability 全空、no-new-privileges、network namespace、0 个 bind mount、workspace tmpfs 选项）。`network_mode` 仅对唯一、无 `path`、无 hooks 的新网络 namespace 报告 `"none"`；加入已有路径为 `"private"`，没有 network 项为 `"host"`，重复、损坏或带 hooks 的离线候选不报告该值。它不监控宿主特权进程在创建后的修改。`memory_bytes`、`nano_cpus`、`pids_limit` 保持 `None`，不能当作内核已强制。
- 没有 seccomp 过滤、user namespace、AppArmor/SELinux profile、TTY、进程组信号或 detached recovery。
- 离线网络不等于可配置的网络策略；没有 bandwidth 限制或连接审计。运行器仍要求 `network_mode = "none"`，不会接受任意 `"private"` 网络作为禁网证明。
- image 解包路径只在 committed snapshot 缺失时执行。真实 daemon 测试现在使用全新 namespace，实际执行 `Prepare`/`Diff.Apply`/`Commit`，并验证 guest 可启动、可 exec、清理后无 snapshot 残留。
- 外部进程并发解包同一 chain 时，各进程使用独立的随机 extraction key；Commit 以 chain id 命名，先完成者胜出，后完成者的 `AlreadyExists` 被当作成功并只删除自己的 extraction snapshot。没有跨进程锁。
- 未验证 Kubernetes/CRI、Kata、Firecracker、gVisor（`runsc` 在本机不可用）、VM/内存检查点与 registry 认证。显式 image transfer 的新行为需单独提供真实 daemon 验证，不沿用上面的旧运行记录。
- Windows 主机不编译 containerd 适配器；在非 Unix 主机上选择 `BackendConfig::Containerd` 会在连接前返回 typed backend error。

## 测试

`containerd_network_is_offline` 属于已有 `containerd_sandbox` 套件和 CI：宿主先连接本机监听端点作为正对照，guest 验证只有 loopback、没有 IPv4 路由、IPv6 路由仅指向 loopback，且无法连接同一宿主端点；最后确认容器和自有 snapshot 已移除。不依赖外部站点或公网连通性。WSL2/containerd 2.2.1/runc 环境已实际执行该用例并通过。

```bash
CIRCUS_CONTAINERD=1 cargo test --locked --features containerd \
  --test containerd_sandbox containerd_network_is_offline \
  -- --ignored --exact --nocapture --test-threads=1
```

`containerd_process_inherits_image_environment` 属于默认忽略的 `containerd_process` 套件：同一真实 sandbox 中，`ProcessSession` 与一次性 `exec` 都必须继承 `PYTHON_VERSION`，并与实际 `sys.version` 的第一个 token 精确相等；交互请求覆盖镜像变量、新增含 `=` 的值，且宿主已设为 `1` 的 `CIRCUS_CONTAINERD` 不得出现在 guest 中。用例检查 stdout、stderr、非零退出码，并在传播操作错误或断言输出前销毁容器、确认自有 snapshot 消失，不修改宿主环境。

该环境回归的 fixture 默认使用本地已存在的官方 `python:3.12-slim`，依赖官方镜像的 `ENV PYTHON_VERSION`。可通过 `CIRCUS_CONTAINERD_IMAGE` 指向自备镜像，但它必须提供 Python 3，并在 image config 中声明与该解释器实际版本一致的 `PYTHON_VERSION`；不得预置测试控制变量 `CIRCUS_CONTAINERD` 或 `CIRCUS_CONTAINERD_ENV_REGRESSION`。仅有可执行 Python、但没有相应 image ENV 的镜像不满足此回归的 fixture 契约。以下命令是运行入口，不代表本次修改已执行或通过：

```bash
CIRCUS_CONTAINERD=1 CIRCUS_CONTAINERD_IMAGE=python:3.12-slim \
  cargo test --locked --features containerd --test containerd_process \
  containerd_process_inherits_image_environment \
  -- --ignored --exact --test-threads=1
```

新增镜像准备测试与下方历史运行记录分开验收。确定性 `containerd_image_policy` 使用 Unix + `containerd` feature 的具体本地 gRPC peer，不需要真实 daemon；它验证协议/策略边界，不是真实 registry 或隔离证据：

```bash
cargo test --locked --features containerd --test containerd_image_policy
```

`containerd_image_environment` 默认忽略，显式连接真实 daemon，创建测试自有的唯一 namespace（不使用共享的 `CIRCUS_CONTAINERD_NAMESPACE`），覆盖实际 pull、不可变 alias/local-only 缓存查询，以及用产物启动 sandbox、exec、destroy 和自有资源清理。它需要 daemon transfer service、registry 网络及可运行且包含 Python 3 的镜像：

```bash
CIRCUS_CONTAINERD=1 \
CIRCUS_CONTAINERD_SOCKET=/run/containerd/containerd.sock \
CIRCUS_CONTAINERD_IMAGE=docker.io/library/python:3.12-slim \
  cargo test --locked --features containerd --test containerd_image_environment \
  -- --ignored --test-threads=1
```

上述两条新测试命令尚未在本次修改中执行，不声明通过；真实套件自有资源清理不改变生产 API 不删除共享缓存的契约。

四组集成测试默认忽略，只有显式设置 `CIRCUS_CONTAINERD=1` 才连接真实 daemon：

```bash
CIRCUS_CONTAINERD=1 \
CIRCUS_CONTAINERD_SOCKET=/run/containerd/containerd.sock \
CIRCUS_CONTAINERD_IMAGE=python:3.12-slim \
cargo test --locked --features containerd --test containerd_sandbox --test containerd_snapshot --test containerd_process --test containerd_pool -- --ignored
```

- `containerd_sandbox`：生命周期、exec argv/退出码/共享输出预算、文件写入/读取/删除、pause/resume、缺失镜像类型化错误且新 namespace 无残留、并发创建、只读 status。
- `containerd_snapshot`：运行中 guest tmpfs 的 capture，并 restore 到新容器；capture 后源容器仍可写。
- `containerd_process`：交互进程 stdout/stdin/stderr/退出码、镜像 `PYTHON_VERSION` 继承与精确版本对照、显式环境覆盖及宿主环境不复制（同时覆盖一次性 exec 的继承）、取消导致整容器清理、输出超限导致整容器清理。
- `containerd_pool`：`SandboxPool`（后端中立，Docker 与 containerd 通用）在真实 containerd 上预热容量 2、`status()` 计数、租约写入私有标记后 release 并经 `ctr` 确认容器与 snapshot 均不存在且下一租约看不到标记、容量 1 的 `AcquireTimeout` 后重新获取、短 TTL 撤销等待睡眠 Python 子进程的 exec 并补充全新容器、持有租约时 `shutdown()` 销毁全部槽位且幂等、坏镜像准备失败留下类型化错误且无容器。租约单次使用，release、TTL 与 shutdown 都真实销毁；`ResourceLimits` 仍是每容器限制，不是宿主聚合准入。
- 每个 teardown 都用 `ctr -n <ns> containers list -q` 与 `ctr -n <ns> snapshots --snapshotter overlayfs list` 确认容器和 snapshot 不存在。

本地实际执行（2026-10-25；WSL2 root 运行 Rust 与 containerd，Windows Node 24.18.0 在仓库根运行 QA）：

```text
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo clippy --locked --all-targets --features containerd -- -D warnings
cargo test --locked
RUSTDOCFLAGS="-D warnings" cargo doc --locked --no-deps
CIRCUS_CONTAINERD=1 CIRCUS_CONTAINERD_SOCKET=/run/containerd/containerd.sock \
CIRCUS_CONTAINERD_NAMESPACE=default CIRCUS_CONTAINERD_IMAGE=python:3.12-slim \
  cargo test --locked --features containerd --test containerd_pool -- --ignored
npm --prefix circus-sandbox/qa run check
npm --prefix circus-sandbox/qa test
node circus-sandbox/qa/architecture.mjs
```

`containerd_pool` 的实际结果是 `6 passed; 0 failed`（约 13.5 秒）。运行前后 `ctr -n default containers list -q` 均为空；`ctr -n default snapshots --snapshotter overlayfs list` 在运行后只保留镜像的 5 个 committed 层（与运行前计数相同），没有残留 active snapshot。该套件自身用进程内互斥串行化，因此未额外传 `--test-threads=1`。
