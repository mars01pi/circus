# Claude Code Agent 镜像

此镜像固定 Claude Code **2.1.310**，提供 Python 3、Git、ripgrep 与可写的临时 HOME。任务由 [Agent runner](../../docs/claude-agent.md) 启动；构建镜像不会自动创建或运行 Agent 任务。

从仓库根目录构建，并将本机镜像 ID 传给运行器：

```bash
docker --host "$CIRCUS_DOCKER_ENDPOINT" build \
  --platform linux/amd64 \
  --tag circus-agent-claude-code:2.1.310 \
  circus-sandbox/images/claude-code
docker --host "$CIRCUS_DOCKER_ENDPOINT" image inspect \
  --format '{{.Id}}' circus-agent-claude-code:2.1.310
```

Node 基础镜像固定 digest，npm 包与平台二进制由 `package-lock.json` 固定并校验完整性。安装先禁用所有 npm lifecycle scripts，再显式执行该固定版本的 `install.cjs`，将平台二进制放到 CLI 入口。构建需联网；Debian 包由构建时仓库解析，不声称逐字节可复现。

镜像以 `65534:65534` 运行。HOME、配置目录、临时目录均位于 `/workspace/.circus`；运行器为每次任务创建新的 tmpfs、只读根文件系统、禁网策略与资源限制。仅运行此 Dockerfile 本身不构成这些运行时约束。

Claude Code 是 Anthropic 提供的独立软件，使用其随包附带的许可与服务条款；不因被装入此镜像而变成 Circus 的 Apache-2.0 代码。镜像中不写入 Anthropic API key、登录文件、宿主目录或 Docker socket。
