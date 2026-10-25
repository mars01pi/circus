# Agent 任务运行器

`agent-runner/` 提供独立的 `circus-agent-runner` library crate，把 Codex 单任务示例整理成可复用调用入口。调用方提交任务、消费事件、等待产物；运行器负责准备独立沙箱、驱动一个 Codex turn、取回文件和确认清理。它通过 `circus-sandbox` 的公开 API 工作，Codex 协议没有进入沙箱核心。调用方可以用调用方拥有的工作区归档和原生线程 ID 串起多轮任务，但每个任务仍创建新沙箱。Paseo 通过 [stdio bridge](stdio-bridge.md) 和 [sandbox provider](../../circus-paseo/public-docs/supported-providers.md#sandbox-codex) 接入；bridge v1 的每个进程仍只执行一个任务，但请求可以携带明文工作区归档与 Codex 线程 ID，provider 每次提示启动新进程来串起已完成轮次。HTTP 服务、进行中 turn 的恢复和自动重试仍未实现。

## 调用契约

`AgentRunner::new(RunnerConfig)` 接收显式后端、资源限制、`ModelConfig` 和宿主 Python 路径；库不会自动读取环境变量或个人 Codex 登录文件。同一个运行器可以创建并发任务，每个任务使用新沙箱和独立临时 token。

| 入口                        | 语义                                                                           |
| --------------------------- | ------------------------------------------------------------------------------ |
| `runner.start(TaskRequest)` | 校验后在当前 Tokio runtime 启动任务，返回 `AgentTask`；不等待执行完成。        |
| `task.id()`                 | 任务的独立标识。                                                               |
| `task.next_event().await`   | 按序读取阶段、沙箱标识、文本增量、工具开始/完成和工具输出；关闭时返回 `None`。 |
| `task.cancel()`             | 幂等请求取消；仍需等待结果确认终止与回收。                                     |
| `task.wait().await`         | 返回可重复读取的 `Arc<TaskOutcome>`；取消等待 future 不会取消任务。            |

典型消费方式是在循环中读取 `next_event`，结束后调用 `wait`。也可以同时等待结果和消费事件；不要只等结果而永久不读事件。任务句柄 Drop 会请求取消，后台 worker 继续负责清理；宿主进程或 Tokio runtime 退出后的恢复不在当前能力范围内。

成功结果包含 `task_id`、`artifacts: BTreeMap<String, Vec<u8>>` 和 `agent_session: Option<String>`；只在任务完成、全部指定文件取回、工作区归档（如启用）保存并确认沙箱销毁后返回。`agent_session` 是下一次任务应传回的 Codex 线程 ID；没有报告时为 `None`。库不会把模型文件自动写入宿主路径。失败返回 `TaskFailure`：`error` 为可匹配错误，`cleanup` 在沙箱回收未确认时保留所有权，调用方应保存句柄并执行 `cleanup.retry().await`。清理错误同时保留此前的任务错误。`WorkerGone` 表示 worker 意外消失，不可把它的空 cleanup 字段当作已清理证据。

事件队列最多 256 项，单条宿主协议帧最多 64 KiB，累计事件文本及开销最多 8 MiB。消费过慢会以 `EventBackpressure` 结束任务并清理；最终结果通过独立通道返回，不会被满队列阻塞。`Cleaning` 事件为尽力发送，清理结论以 `wait` 为准。文本和工具输出来自沙箱，应作为不可信文本展示，不能当宿主命令执行。

## 输入、产物与期限

`TaskRequest` 包含 `prompt`、`input_files`、`output_paths`、`timeout`、`workspace` 和 `agent_session`。输入字节写入沙箱的 `/workspace/project/`；产物路径相对这个项目目录，按相同名称返回。

- 提示词最多 64 KiB；输入最多 64 个文件、总计 8 MiB；输出最多 32 个指定文件、总计 8 MiB。单文件另受 `ResourceLimits.max_file_bytes` 约束。
- 文件路径拒绝绝对路径、空组件、`.`、`..`、反斜杠、冒号和控制字符；后端文件 API 继续约束链接和非普通文件。重复输出、缺失产物、超限产物均不返回成功。
- 期限必须大于零且不超过一小时，包含准备、运行和导出。创建请求必须先收敛才能保留正确的清理所有权；销毁也必须等到确认，因此实际返回时间可能超过任务期限。
- 取消、超时、模型错误、协议错误、事件拥塞和产物失败均进入统一清理路径。没有重试模型请求、自动恢复或跨任务排队策略。

## 多轮任务：持久工作区与原生线程

多轮由调用方串行发起，不是常驻会话：

1. 第一个任务设置 `TaskRequest.workspace = Some(TaskWorkspace { archive, budget })`，`archive` 可以尚不存在。
2. 成功后保存返回的 `TaskOutput.agent_session`，并复用同一归档路径创建下一个 `TaskRequest`。
3. 下一个任务把该值传入 `TaskRequest.agent_session`。Codex 控制器收到 `CIRCUS_RESUME_THREAD` 后调用 app-server 的 `thread/resume`，不再新建线程；控制器通过 `agent_session` stderr 事件报告实际线程 ID，运行器校验后写入 `TaskOutput`。
4. 每个任务仍 `Sandbox::create` 新实例：先按 `budget` 加载归档并恢复到 `Created` 沙箱，再写入 bridge 和输入文件（输入覆盖归档中的同名文件），启动、执行、在销毁前捕获并原子保存归档。

边界：

- 归档是调用方拥有的明文 `CodeSnapshot` 文件，包含 `/workspace` 下的项目文件、Codex 会话文件和 `.circus` 状态；调用方负责存储、保留、加密和清理。运行器不记录宿主归档路径，也不把它传入沙箱。同一归档路径被并发任务使用或与外部写入竞争时没有锁；串行复用是调用方的责任。
- `TaskWorkspace.budget` 同时约束加载和捕获（内容字节与条目数）；单文件上限来自 `RunnerConfig.resources.max_file_bytes()`。归档父目录必须已存在；保存不创建目录，失败按 `TaskError::Workspace` 报告。
- 捕获发生在任务结束（成功、失败、取消或超时）且沙箱仍为 `Running` 时，始终早于 `destroy`。加载/恢复/捕获/保存失败是 `TaskError::Workspace { phase, task, .. }`，其中 `task` 保留先前的执行错误；它不会取消销毁，销毁失败仍按 `TaskError::Cleanup` 保留所有权。
- 这不是 turn 进行中的恢复：宿主进程崩溃、容器被杀或会话文件损坏都无法续接已开始的 turn；新任务从归档的最后一次完整保存继续。
- Claude Code 仍以 `--no-session-persistence` 运行；传入 `agent_session` 会在分配沙箱前返回 `Invalid`，宿主入口也拒绝 `CIRCUS_RESUME_THREAD`，不会假装续接。
- 归档内的 Codex 配置由 guest 以 `O_NOFOLLOW` + 截断重写；不会复用旧端口或旧配置。线程可恢复的前提是线程状态位于被捕获的 `/workspace` 内。

## 模型通道

容器使用 `network=none`、只读根、非 root、无宿主 bind 和显式资源限额。容器内 Python 转发器只监听自己的 `127.0.0.1`，将 Codex 的模型请求封装到已有进程标准流；宿主 Python 控制器验证请求后访问显式配置的 HTTPS 模型服务，并将 Responses SSE 流送回。Docker socket 只由 Rust 管理端使用。

```text
调用方 → AgentRunner → Sandbox / ProcessSession → 容器内 Codex app-server
                          │                              │
                     宿主控制器 ← 标准流隧道 ← 容器内模型转发器
                          │
                     HTTPS /responses → 模型服务
```

- 真实 API key 仅提供给宿主控制器，不写入镜像、工作区或沙箱进程环境；`ModelConfig` 的 Debug 隐去它。每次任务使用独立临时 token，权限覆盖整个任务沙箱，不区分 Codex 与其工具进程。
- 宿主只接受 `POST /v1/responses`，模型必须匹配配置，要求流式响应，强制 `store=false` 并拒绝后台任务。不跟随重定向，不继承 HTTP 代理，不接受容器指定上游 URL 或转发 Authorization。
- 每个任务最多 16 次请求、每次正文 2 MiB、响应 8 MiB、累计响应 16 MiB，一次只允许一个在途请求；连接与读取空闲超时 30 秒。每条进程输出流最多 32 MiB。它们是传输预算，不是 token 计费上限。
- 模型错误保留安全类别：TLS、连接/读取超时、HTTP 状态、传输、协议和预算；不把原始上游错误正文或请求头放入事件和日志。需要额外 CA 时调用 `ModelConfig::with_ca_file`，仍验证 HTTPS 证书。
- 使用 Codex `externalSandbox` 策略，文件、进程、网络与资源边界由外层 Docker 执行。遇到额外审批或用户输入请求返回 `UnsupportedInteraction`。
- Codex 使用固定参数 `codex app-server --listen stdio://` 启动，不经过 shell。动态回环端口写入沙箱 TOML；恢复归档后该文件由 guest 以 `O_NOFOLLOW` 截断重写，不复用旧端口，也不跟随链接。配置只引用任务 token 的环境变量名，随沙箱销毁。

模型通道不提供任意联网、Git 拉取、软件包下载、内容过滤或费用计量。宿主控制器由 Tokio 启动、终止和等待；沙箱销毁仍归既有 lifecycle 管理。

## 运行真实模型任务

前提：已构建 [Codex Agent 镜像](../images/codex/README.md)、显式可用的本地 Linux Docker Engine、宿主 Python 3.11+（含可信 CA）和支持 Codex 工具调用的 Responses SSE 服务。仅兼容 Chat Completions 的网关不适用。`CIRCUS_HOST_PYTHON` 可指定宿主 Python，默认 `python3`；`CIRCUS_MODEL_CA_FILE` 可指定 PEM CA 文件。

先用 `docker --host "$CIRCUS_DOCKER_ENDPOINT" image inspect circus-agent-codex:0.164.0 --format '{{.Id}}'` 获取本地镜像的不可变 ID。以下 `CIRCUS_AGENT_IMAGE` 使用该 `sha256:` ID，与 stdio 配置一致；验收不依赖可变 tag 解析。

在 `circus-sandbox/` 运行以下 Bash 示例，替换镜像 ID、地址和模型名。密钥经终端输入，示例不读取个人登录文件。

```bash
export CIRCUS_DOCKER_ENDPOINT=unix:///var/run/docker.sock
export CIRCUS_AGENT_IMAGE=sha256:REPLACE_WITH_VERIFIED_IMAGE_ID
export CIRCUS_MODEL_BASE_URL=https://your-approved-gateway.example/v1
export CIRCUS_MODEL=your-approved-model
read -r -s -p 'Model API key: ' CIRCUS_MODEL_API_KEY
export CIRCUS_MODEL_API_KEY
printf '\n'
export CIRCUS_AGENT_PROMPT='Fix the existing answer.py add(a, b). Run Python assertions for add(20, 22) == 42 and add(-2, 2) == 0. Write 42 followed by a newline to result.txt after the tests pass.'
cargo run --locked -p circus-agent-runner --example codex_agent
unset CIRCUS_MODEL_API_KEY
```

示例先传入错误的 `answer.py`，导出修复后的 `answer.py` 和 `result.txt`，打印阶段、工具状态、文件大小和结果内容。库调用方自行选择输入文件和产物列表；旧示例的 `CIRCUS_AGENT_RESULT` 已移除。示例不打印模型事件正文，产物内容本身仍是不可信任务数据。

## 验证入口

输入与事件边界、宿主策略及本地 HTTPS 测试不请求模型、不需要 Docker。HTTPS 测试需要 OpenSSL CLI，只生成临时证书，不修改系统信任设置：

```bash
cargo test --locked -p circus-agent-runner --lib
python3 -B -m unittest discover -s agent-runner/tests -p 'test_codex_*.py' -v
```

真实 Docker / Codex 测试用脚本生成 Responses 事件作为对端，不进行模型推理，不证明实际网关兼容：

```bash
CIRCUS_DOCKER_SANDBOX=1 \
CIRCUS_DOCKER_ENDPOINT=unix:///var/run/docker.sock \
CIRCUS_AGENT_IMAGE=sha256:REPLACE_WITH_VERIFIED_IMAGE_ID \
  cargo test --locked -p circus-agent-runner tests::docker -- --ignored --nocapture --test-threads=1
```

这些测试覆盖并发任务输入隔离、修改已有代码、真实工具执行、事件和产物；检查容器没有启用的非 loopback 接口及对外路由、宿主 key 未进入任何工具环境值，并按每个任务自己的实例标识确认回收。另覆盖 HTTP 拒绝、产物缺失、主动取消、超时和句柄 Drop，以及同一归档的两次任务（第一次写入文件，第二次读取并续接同一 Codex 线程）和工作区归档失败仍完成回收。控制器对端仅由私有测试入口注入，公开运行器没有假模型开关。实际网关用上方 example 单独验收；其他平台、长任务和高并发需另外验证。

协议依据：[Codex app-server](https://developers.openai.com/codex/app-server/) 的 `initialize`、`thread/start`、`thread/resume`、`turn/start`、`turn/completed`，以及 [配置参考](https://developers.openai.com/codex/config-reference/) 的自定义 Responses provider。镜像固定 Codex CLI 0.164.0；升级 CLI 后需重跑实际验收。
