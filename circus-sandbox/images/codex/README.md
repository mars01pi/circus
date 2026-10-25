# Codex Agent 镜像

这个目录构建供 `Sandbox::create(BackendConfig::docker(endpoint, image, "runc"), SandboxConfig::new(limits, isolation, io))` 使用的本地 Linux 镜像：包含 Codex CLI、Node.js、Python 3、Git 和 ripgrep，并把运行时写入集中到 `/workspace`。Python 3 同时服务于现有沙箱文件传输。

这是手动构建的 Agent 运行环境，不是 OCI 镜像解析/拉取服务。镜像不包含账号、API key 或模型配置；当前沙箱保持禁网。离线启动成功不表示 Agent 已能调用模型或完成编程任务。

## 固定版本与构建

| 组件                                    | 固定方式                                                                                        |
| --------------------------------------- | ----------------------------------------------------------------------------------------------- |
| Node.js 24.18.0 / Debian bookworm-slim  | Dockerfile 固定官方基础镜像的多平台 index digest                                                |
| Codex CLI 0.164.0                       | `package.json` 精确版本、npm 生成的 `package-lock.json` 和 `npm ci`；包含对应平台的原生可选依赖 |
| Python 3、Git、ripgrep、CA certificates | 构建时从 Debian 软件源安装，实际版本写入 `/opt/circus/versions.txt`                             |

基础镜像与 npm 依赖已锁定，Debian 软件源仍会更新，因此不声称重复构建得到逐字节相同的产物。保存构建后的镜像 ID；若将来发布到 registry，应使用发布产物的 digest。升级时显式修改版本/锁文件并重新执行下面的真实验收。

在仓库根目录构建；以下示例使用 Linux 的本地 socket，Docker Desktop 使用其实际本地 endpoint：

```bash
export CIRCUS_DOCKER_ENDPOINT=unix:///var/run/docker.sock
docker --host "$CIRCUS_DOCKER_ENDPOINT" build \
  --platform linux/amd64 \
  --tag circus-agent-codex:0.164.0 \
  circus-sandbox/images/codex
docker --host "$CIRCUS_DOCKER_ENDPOINT" image inspect \
  --format '{{.Id}}' circus-agent-codex:0.164.0
```

构建阶段需要网络下载依赖；Agent 运行阶段沿用沙箱的禁网策略。构建上下文仅允许 Dockerfile、依赖清单、锁文件和两个脚本，不复制项目代码、宿主 HOME 或凭证。不声明 `VOLUME`，也不安装 Docker daemon/socket。

## 运行目录与启动约定

| 用途                   | 容器路径                                                      |
| ---------------------- | ------------------------------------------------------------- |
| 项目代码               | `/workspace/project`                                          |
| HOME                   | `/workspace/.circus/home`                                     |
| CODEX_HOME             | `/workspace/.circus/home/.codex`                              |
| 配置、缓存、数据和状态 | HOME 下的 `.config`、`.cache`、`.local/share`、`.local/state` |
| 临时文件               | `/workspace/.circus/tmp`                                      |

这些目录共享沙箱配置中的 tmpfs 预算，容器销毁后全部丢失。镜像默认用户为 `65534:65534`，匹配下述验收配置；不要求宿主存在同名用户。`circus-agent` 使用 `umask 077` 创建运行目录，最后以 `exec` 原样传递命令参数和标准流。

容器后端会覆盖镜像 ENTRYPOINT 以启动自己的 supervisor；运行时 tmpfs 也会遮住构建时的 `/workspace`。因此调用方必须显式使用 `circus-agent`，不能依赖镜像入口初始化目录。默认工作区路径为 `/workspace`；本镜像暂不适配自定义 workspace 根。

下面的持续 stdio 接入示例使用 [PR #23 引入的交互进程 API](https://github.com/BizerNotNull/circus/pull/23)；下方离线验收通过 `exec` 运行镜像内的校验程序。在 `sandbox.start()` 后先调用 `sandbox.create_dir_all("project")`，再传入以下 `ProcessRequest` 字段启动 Codex stdio 服务：

```rust
ProcessRequest {
    argv: vec![
        "circus-agent".into(),
        "codex".into(),
        "app-server".into(),
        "--listen".into(),
        "stdio://".into(),
    ],
    working_dir: Some("project".into()),
    env: BTreeMap::new(),
    timeout: Duration::from_secs(60),
    output_limit_bytes: 128 * 1024,
}
```

调用方仍需消费输出并遵守 `ProcessSession` 的超时、限额和销毁语义。目录初始化与 CLI 启动不会自动登录、开放网络或提供模型凭证。协议参考 [OpenAI Codex app-server 文档](https://developers.openai.com/codex/app-server/)：stdio 使用逐行 JSON，先发送 `initialize`，收到响应后发送 `initialized`。

## 真实离线验收

构建后，在 `circus-sandbox/` 执行。须显式设置本地镜像与 endpoint；测试不会自动构建或拉取镜像：

```bash
CIRCUS_DOCKER_SANDBOX=1 \
CIRCUS_DOCKER_ENDPOINT=unix:///var/run/docker.sock \
CIRCUS_AGENT_IMAGE=circus-agent-codex:0.164.0 \
  cargo test --locked --test agent_image -- --ignored --nocapture
```

测试经真实 Docker 后端（`Sandbox::create`）创建两个全新的容器，使用只读根、禁网、无宿主 bind、非 root 用户及显式内存/CPU/PID/tmpfs 限额，依次验证：

- Node/Codex 版本、Python 3、Git 初始化和 ripgrep 读取实际项目文件。
- HOME、配置、缓存、临时目录可写且没有组/其他用户权限；新容器没有旧项目文件或 Codex 登录信息。
- 真实 `codex app-server` 的 `initialize` / `initialized` / `account/read`，确认未登录；不发送模型请求或启动 Agent turn。
- 沙箱文件 API 能读取生成文件；销毁后通过 Docker inspect 确认容器不存在，失败路径只回收测试自己创建的容器。

验收预算为 512 MiB 内存、1000 milli-CPU、128 PIDs、128 MiB tmpfs；这是启动测试的显式预算，不是所有 Agent 工作负载的最低保证。测试默认忽略，普通 Cargo 测试通过不能代替该验收。当前验收目标为 `linux/amd64`；其他平台须单独构建并验证。
