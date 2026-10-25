# 宿主 MCP 凭据代理

Circus 的单任务 Codex / Claude Code 运行器可通过宿主访问显式配置的 MCP 工具。沙箱内只配置
固定的 stdio 适配器和本次任务的临时通道标识；MCP 服务地址、认证 header、密码和
资源授权策略保留在宿主。任务仍运行于禁网容器，不获得宿主 Docker socket。

## 当前范围

- 上游支持 HTTPS Streamable HTTP，协议版本 `2025-03-26`，响应支持 JSON 与有界
  SSE。TLS 校验默认开启，可指定宿主 CA 文件；不跟随重定向，不使用环境代理。
- 提供 `initialize`、`notifications/initialized`、`ping`、`tools/list`、
  `tools/call` 子集。其他方法、服务端主动请求、未知请求字段均拒绝。
- 只暴露本次任务获准的工具。工具参数由宿主限制：允许的动态参数之外全部拒绝；
  固定资源参数由宿主注入，沙箱不得覆盖。
- API key、Basic 认证值等可以保存在宿主私有 header 文件，或 macOS Keychain。
  Keychain 的读取与模型凭据共用宿主凭据模块；非 macOS 不静默降级。
- 本地 stdio MCP 服务、OAuth 登录流程、资源读取/订阅、提示词模板与 sampling
  尚未接入。支持该传输不代表所有第三方 MCP 产品已经验收。

## 信任边界

```text
Agent → 沙箱内固定 MCP stdio 适配器 → 容器回环 /mcp
      → Docker attach 标准流 → 宿主任务权限代理
      → 注入宿主认证 header → 已配置的 HTTPS MCP 服务
```

连接配置是宿主操作员的可信配置。任务调用方只能引用连接 ID 与其中已批准的工具；
不得提交新的地址、可执行程序、header、密钥路径或固定资源范围。将连接授予哪个用户
仍由调用方负责；运行器不能替代桌面/daemon 的用户认证。

工具的 `annotations` 不作为授权依据。`fixedArguments` 只约束具体的字符串参数，
不是通用 SQL 检查器、文件路径沙箱或 URL 过滤器。操作员必须确认工具语义，例如将
仓库的 `owner`、`repo` 固定；数据库服务还应使用真实只读账户。将任意 SQL 或任意 URL
参数列入 `allowedArguments`，并不会自动使它变成安全的只读工具。

MCP 服务本身是受信任的数据处理方，并会接收认证信息。工具成功结果中的已知认证
值会被替换为 `[REDACTED]`，失败详情不转发；这不保证抵御主动编码或改写凭据的恶意
MCP 服务。不要将密钥写入工具描述、任务提示词、文件内容或可返回的数据。

## 配置连接

在 [stdio bridge 宿主配置](stdio-bridge.md)中加入 `mcpConnections`。以下为**测试
服务形状**，`read_issue` 并非对 GitHub 实际工具名称的承诺：

```json
{
  "mcpConnections": [
    {
      "id": "project-issues",
      "url": "https://approved-mcp.example/mcp",
      "headersFile": "/absolute/private/mcp-headers.json",
      "tools": [
        {
          "name": "read_issue",
          "allowedArguments": ["number"],
          "fixedArguments": {
            "repo": "approved/project"
          }
        }
      ]
    }
  ]
}
```

`headersFile` 是仅宿主读取的 JSON 字符串映射，例如包含 `Authorization`。
创建文件时限制读取权限；真实值不应写入代码库或聊天记录。也可以使用
`"headersKeychain": {"service": "Circus MCP", "account": "project-issues"}`，其 Keychain
条目内容为相同的 JSON 字符串映射。两种来源不能同时指定；无需认证的服务可全部省略。
可选 `caFile` 必须为宿主绝对路径。

连接 ID、工具名、参数名采用有界 ASCII 标识符；最多 8 个连接、每连接 32 个批准工具。
认证 header 不能覆盖 Host、内容 framing 或 MCP session/protocol header。
模型认证和 MCP 认证彼此独立，不从个人 Agent 配置自动导入。

## 每次任务显式授权

在 `start.request` 中加入以下字段，其他输入、产物路径、超时字段保持原协议：

```json
{
  "mcpGrants": [
    {
      "connectionId": "project-issues",
      "tools": ["read_issue"]
    }
  ]
}
```

省略 `mcpGrants` 等于不允许任何 MCP。不存在的连接/工具和重复授权会在分配沙箱前
失败。沙箱里的工具名增加连接序号前缀，例如 `c0__read_issue`，避免不同服务同名冲突；
任务授权始终使用原始工具名。

Rust 调用方先用 `McpConnection::new` 和 `McpToolPolicy` 建立宿主配置，再调用
`AgentRunner::with_mcp_connections`。执行时使用
`start_with_mcp(TaskRequest, Vec<McpGrant>)`；原 `start(TaskRequest)` 继续默认无 MCP。

## 预算与结束语义

每任务最多 64 次 MCP 通道请求，单个请求/响应最多 1 MiB，输出累计最多 4 MiB。
每次上游请求总时限 15 秒，单个连接最多 96 次上游请求；工具目录最多 4 页、512 个
条目，只向 Agent 返回其中已授权的工具。未知字段、重复 JSON key、伪造/重放通道 ID、
错误任务 token、错误 JSON-RPC 关联 ID、未批准的工具/参数都会拒绝。

任务正常结束时撤销通道并关闭活动连接；取消、超时和输入断开时，运行器终止宿主控制
进程并继续负责容器清理。旧临时通道不能调用新的任务。已到达外部服务的操作可能已经
发生，取消不是事务回滚，也不保证撤销外部服务已接受的写操作。该切片没有远端 session
持久化或服务端事务管理；清理结果仍以任务 outcome 为准。

## 验证

本地真实 TLS 服务验证认证注入、JSON/SSE、作用域、越权拒绝、错误脱敏、TLS 失败、
重定向拒绝与撤销。它是协议测试服务，不是外部 SaaS 或真实模型验收：

```bash
PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover \
  -s circus-sandbox/agent-runner/tests -p 'test_mcp.py' -v
```

另有显式启用的 Docker 用例，使用真正的 Codex CLI、stdio bridge、TLS MCP 和
**脚本化 Responses 服务**，验证 MCP 调用、密钥未进入沙箱、产物和最终容器回收。用例还让容器内进程持有临时
通道 token 直接请求越权工具、覆盖固定仓库和读取 MCP 资源，验证宿主拒绝且请求没有
到达上游：

```bash
# 在 circus-sandbox/ 下构建，然后回仓库根目录运行测试。
cargo build --locked -p circus-stdio-bridge
```

```bash
export CIRCUS_MCP_DOCKER=1
export CIRCUS_DOCKER_ENDPOINT=unix:///var/run/docker.sock
export CIRCUS_AGENT_IMAGE=sha256:REPLACE_WITH_VERIFIED_IMAGE_ID
export CIRCUS_BRIDGE_BINARY="$PWD/circus-sandbox/target/debug/circus-stdio-bridge"
PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover \
  -s circus-sandbox/agent-runner/tests -p 'test_mcp_docker.py' -v
```

镜像构建、真实模型接入和环境前提见 [Codex 任务说明](codex-agent.md)。测试凭据均为
临时公开 sentinel，不接触真实服务账户。

Claude Code 使用同一个 broker；其真实 CLI + Docker + 脚本 Messages 服务验收命令见
[Claude 任务说明](claude-agent.md)。这两条测试都不能替代真实模型与第三方 MCP 服务验收。
