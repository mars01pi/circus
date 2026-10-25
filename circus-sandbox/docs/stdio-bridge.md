# stdio 桥接

`circus-stdio-bridge` 是 `circus-agent-runner` 的同机子进程入口。Node 客户端位于
`circus-paseo/plugins/sandbox-provider/server/bridge/stdio-client.ts`，只依赖 Paseo 插件 SDK、
Zod 与 Node 内置模块。它不启动 HTTP 服务，不注册 provider，也不增加另一套沙箱生命周期。
Provider 与 UI 由 [Paseo sandbox provider](../../circus-paseo/public-docs/supported-providers.md#sandbox-codex) 提供；运行器能力和限制见 [Agent 运行器](codex-agent.md)。

每个客户端进程只提交一个任务。Paseo provider 通过调用方拥有的明文工作区归档和 Codex
原生线程 ID 串起顺序多轮：每次提示启动新的 bridge 进程，任务开始时恢复归档，结束时在沙箱
销毁前保存归档，并在成功结果中返回 `agentSession`。这只延续已完成轮次，不保留运行中的
进程、进行中的 turn 或 bridge 进程内状态；也没有跨连接按 taskId 查询或取消的入口。
Claude Code 仍为单轮并拒绝 `agentSession`。

## 宿主配置

在 `circus-sandbox/` 构建：

```bash
cargo build --locked -p circus-stdio-bridge
```

启动参数固定为 `circus-stdio-bridge --config /absolute/config.json`。配置只由可信宿主准备，
不能从任务 prompt、文件内容、providerOptions 或远端任务消息中生成。

顶层 `agent` 可选 `codex`（默认）或 `claude-code`。镜像须匹配所选 CLI，模型服务须分别支持
Responses 或 Anthropic Messages；不能把 Codex 网关配置直接当作 Claude 已可用。
Claude 的镜像与运行契约见 [Claude Code 适配器](claude-agent.md)。

```json
{
  "version": 1,
  "dockerEndpoint": "unix:///var/run/docker.sock",
  "agentImage": "sha256:REPLACE_WITH_VERIFIED_IMAGE_DIGEST",
  "python": "/absolute/path/to/python3",
  "resources": {
    "memoryBytes": 536870912,
    "cpuMillis": 1000,
    "maxProcesses": 128,
    "workspaceBytes": 134217728,
    "maxFileBytes": 2097152
  },
  "model": {
    "baseUrl": "https://approved-gateway.example/v1",
    "model": "approved-model",
    "apiKeyFile": "/absolute/private/model-key",
    "caFile": "/absolute/path/to/trusted-ca.pem"
  }
}
```

`caFile` 可省略，省略时使用宿主 Python 的证书信任。配置最多 64 KiB；key 文件最多 4098 字节，
去掉末尾换行后交给运行器校验。配置、key、CA 和 Python 使用绝对路径；binary 由 Node 显式指定
绝对路径且拒绝 `.cmd` / `.bat`。Node 只传调用方提供的环境，不自动继承 `process.env`。
配置与 key 必须是普通文件，拒绝末端符号链接；Unix 使用非阻塞打开，拒绝 FIFO 等特殊节点。
key 文件应只允许运行 daemon 的宿主账户读取。Node 不读取 key，Rust 不把真实 key 放入协议、
沙箱环境或工作区。stdout 仅输出协议；stderr 只输出静态诊断，不输出配置、上游正文或错误链。

macOS 可用 `"apiKeyKeychain": { "service": "circus", "account": "model" }` 替代
`apiKeyFile`，两者必须且只能选一个。桌面设置将真实密钥保存在本机 Keychain，JSON 只记录引用。
Rust 通过固定 `/usr/bin/security` 读取，输出有容量限制，10 秒内未完成则终止并等待子进程；
查询失败不输出平台诊断，也不回退到环境变量或明文文件。其他平台拒绝 Keychain 引用，仍可由
可信宿主显式配置凭证文件。本机特权进程与已经控制 daemon 账户的攻击者不在此凭证隔离边界内。

可选 `mcpConnections` 配置宿主 HTTPS 工具服务、凭证引用与参数策略；任务仅以
`mcpGrants` 引用连接 ID 和工具名。省略授权不会自动开放任何工具，完整范围见
[MCP 凭据代理](mcp-broker.md)。

## TypeScript 调用契约

从 `stdio-client.ts` 导入 `CircusStdioClient` 及 `CircusTaskRequest`、`CircusTask`、
`CircusTaskEvent`、`CircusTaskResult`、`CleanupStatus`、`CircusBridgeError`。

```typescript
const client = new CircusStdioClient({ executable, configFile, stateDir, env });
try {
  const task = await client.start({
    prompt: "Fix answer.py and export result.txt",
    inputFiles: { "answer.py": new TextEncoder().encode(source) },
    outputPaths: ["answer.py", "result.txt"],
    timeoutMs: 180000,
  });
  for await (const event of task.events) renderUntrustedEvent(event);
  const result = await task.result;
  if (result.cleanup === "pending") await task.retryCleanup();
  consumeResult(result);
} finally {
  await client.close();
}
```

`start` 在收到 Rust 的 `accepted` 后返回。`events` 是单消费者异步流；应与任务一起持续消费。
`cancel()` 等待取消请求确认，终止和清理结论仍须等待 `result`。成功结果包含原始产物字节，
桥接不自动写回宿主。输入与产物路径只是沙箱项目相对名称，不能赋予宿主文件访问权限。

顺序多轮时，Paseo 为每次提示创建新的 `CircusStdioClient`，先把已提交归档复制为独立的候选
`workspaceArchive`，再传入预算和上次成功结果的 `agentSession`。运行器在任务结束且沙箱销毁前
保存候选归档（失败/取消也可能保存）；只有任务成功并返回可恢复线程后，Paseo 才原子替换已提交
归档并发布持久化句柄。失败/取消不替换上次成功检查点。没有恢复句柄的 Agent 只允许单轮，
后续提示和重新打开均明确拒绝。不要从提示词、附件或远端消息生成归档路径。

`stateDir` 是调用方拥有的宿主状态目录，用于派生工作区归档路径；它只参与组装请求，
不会传给 Rust 进程。桥接也不读取该目录中的其他文件。任务失败/取消在 `result` 中返回
`failed` / `cancelled` 和安全错误类别；协议、断流、进程退出、消费拥塞等传输失败以
`CircusBridgeError` 拒绝 Promise。传输错误的 `cleanup` 为 `unknown`。
`close()` 发送 EOF 并等待进程收尾，默认 120 秒，允许显式设置 1..300000 毫秒。
超时才强制结束进程并拒绝，不能把强杀当作已清理。调用者需要处理 `close()` 的错误。

## Wire v1

双向使用 UTF-8 JSON Lines，一帧一个 JSON 对象加 LF。所有帧带 `v: 1`，拒绝未知字段、
未知版本、空帧、无效 UTF-8、超限帧、EOF 中的半帧和非法消息状态。
TypeScript 的严格 Zod schema 位于 `bridge/protocol.ts`，Rust 模型位于 `stdio-bridge/src/protocol.rs`。

| 方向        | type             | 字段与语义                                                                                                                                                          |
| ----------- | ---------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Rust → Node | `ready`          | `maxFrameBytes: 12582912`，配置完成，尚未创建任务                                                                                                                   |
| Node → Rust | `start`          | `requestId`、`request: { prompt, inputFiles, outputPaths, timeoutMs, mcpGrants?, workspaceArchive?, workspaceMaxTotalBytes?, workspaceMaxEntries?, agentSession? }` |
| Rust → Node | `accepted`       | `requestId`、Rust 分配的 `taskId`                                                                                                                                   |
| Rust → Node | `event`          | `requestId`、`taskId`、从 1 连续递增的 `seq`、`event`                                                                                                               |
| Node → Rust | `cancel`         | `requestId`、`taskId`，只作用于本连接任务                                                                                                                           |
| Rust → Node | `cancel_ack`     | 相同关联字段；仅确认请求，未确认回收                                                                                                                                |
| Rust → Node | `result`         | 相同关联字段和 `result`，一次且在全部事件之后                                                                                                                       |
| Node → Rust | `cleanup_retry`  | 相同关联字段，仅任务结束后可调用                                                                                                                                    |
| Rust → Node | `cleanup_result` | 相同关联字段、`cleanup`                                                                                                                                             |
| Rust → Node | `error`          | `code`、静态 `message`，连接失败并退出非零                                                                                                                          |

Wire 文件使用 `{ "path": "relative/name", "data": "canonical padded base64" }` 数组，
拒绝重复路径。请求没有命令、宿主 cwd、模型端点、环境或凭证字段。归档路径和
`agentSession` 只能由可信调用方提供，Rust 在分配沙箱前校验。
`requestId` / `taskId` 只允许 1..64 个 ASCII 字母、数字或连字符。

事件 `kind` 为 `phase`、`sandbox_created`、`text_delta`、`tool` 或 `tool_output`。
`tool.exitCode` 是整数或 null。事件和产物都是不可信内容；展示或下载不等于执行权限。

`result.status` 为 `succeeded`、`failed` 或 `cancelled`。成功时 `cleanup` 必须是 `confirmed`，
并且 `artifacts` 与请求的输出集合完全一致；请求携带 `workspaceArchive` 或 `agentSession`
时，成功结果还包含可选的 `agentSession`，供下一次任务续接；旧单轮客户端不携带这两个字段，
结果线格式保持不变。失败/取消包含 `error: { code, message }`。
`cleanup` 可为 `confirmed`、`pending` 或 `unknown`。`pending` 保留运行器清理句柄，客户端可以
显式重试。EOF 时也尝试一次重试；仍未确认则退出非零。`WorkerGone` 不被当作清理成功。
重试不会把原任务失败改成成功。

## 上限与退出

- 每帧最多 12 MiB，不含 LF；事件帧最多 512 KiB，覆盖 JSON 转义开销。
- prompt 最多 64 KiB；输入最多 64 个文件、解码后合计 8 MiB；产物最多 32 个，合计 8 MiB。
  每文件另受宿主资源配置限制。项目路径最多 1024 个 UTF-8 字节，拒绝路径越界和不可移植形式。
- `workspaceArchive` 必须是宿主绝对路径，最多 4096 字节且不含控制字符。预算
  `workspaceMaxTotalBytes` 为 1..512 MiB，`workspaceMaxEntries` 为 1..1000000，只能在提供
  归档时出现；省略时使用 64 MiB / 65536。`agentSession` 只允许 1..128 个 ASCII 字母、数字、
  `-` 或 `_`。
- 任务期限 1..3600000 毫秒。Node 就绪等待 10 秒；任务响应预算为任务期限加收尾预算。
- Rust 输入队列与输出队列各最多一帧，stdout 单次写入等待最多 5 秒。Node 事件队列最多
  256 项且最多 8 MiB；消费超限会关闭连接并请求取消，最终结果不能冒充成功。
- EOF、非法帧、错误任务关联、SIGINT、SIGTERM、stdout 失败均触发本连接任务取消，随后等待
  运行器的权威结果。清理只调用运行器接口；stdio 层不直接操作容器或另建生命周期。
- 断线前未交付结果时，即使执行恰好完成，也返回取消而非成功。进程崩溃、SIGKILL、宿主重启后
  没有持久化回收服务；`unknown` 需要宿主运维核查，不能用任务事件推断清理完成。

## 验证边界

```bash
cargo test --locked -p circus-stdio-bridge
cargo clippy --locked -p circus-stdio-bridge --all-targets -- -D warnings
```

Rust 默认测试包含真实 stdio 二进制进程及不可用后端路径。Node 的
`framing.test.ts`、`protocol.test.ts` 和 `stdio-client.posix.test.ts` 验证有界帧、子进程断流、任务关联、取消、
清理重试、工作区路径/预算/会话字段边界和二进制文件往返；后者使用脚本协议对端，不是沙箱端到端验收。

真实 Docker 的取消与 EOF 用例在收到任务自己的实例标识后触发收尾，并独立 inspect 确认删除。
它不访问模型，必须显式提供后端、已验证镜像和宿主 Python：

```bash
CIRCUS_DOCKER_SANDBOX=1 \
CIRCUS_DOCKER_ENDPOINT=unix:///var/run/docker.sock \
CIRCUS_AGENT_IMAGE=your-verified-image \
CIRCUS_HOST_PYTHON=/absolute/path/to/python3 \
  cargo test --locked -p circus-stdio-bridge --test transport \
  docker_cancellation_and_eof_confirm_owned_container_removal -- --ignored --nocapture
```
