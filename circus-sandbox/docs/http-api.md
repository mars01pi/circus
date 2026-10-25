# HTTP API

`circus-sandbox` 的 `http` feature（默认启用，可用 `--no-default-features` 关闭）在同一进程内提供带认证的 JSON HTTP 传输。它直接调用领域 `Sandbox` API，不复制生命周期或清理状态；HTTP 层只额外持有租户注册表、已捕获快照和服务级限额。

端点与 schema 以 [`openapi.json`](openapi.json) 为准；文档与真实路由表由测试双向校验：文档中的每个 path/method 必须在路由表里存在，路由表中的每一项也必须在文档里出现。

## 认证与租户

每个路由（包括 `GET /healthz`）都要求：

- `Authorization: Bearer <token>`：`HttpConfig::new` 显式传入的非空 token；比较不因首个差异字节提前返回。
- `X-Circus-Tenant: <tenant>`：必须命中 `HttpConfig::new` 的显式 allow-list。

默认拒绝：缺失或错误 token 返回 `401 unauthorized`，且认证先于路由和租户判断，未知路径在无 token 时也不会暴露是否存在。token 正确但租户缺失或不在 allow-list 中返回 `403 forbidden`。错误体只有稳定的 `{"code","message"}`，不会包含后端 SDK 类型、宿主路径或容器实例 ID。

每个服务实例为每个允许租户分别维护 sandbox 注册表和 snapshot 注册表。sandbox id 与 snapshot id 都是随机 UUID；查询按已认证租户过滤，所以用其他租户的有效 id 访问会得到 `404 not_found`，与不存在的 id 不可区分。

## 生命周期健康语义

`GET /healthz` 只表示“该 HTTP 进程还能响应请求”，返回 `{"status":"ok"}`。它不访问 Docker、不检查镜像、不检查任何 sandbox、不代表 readiness 或后端可用性。后端相关状态只出现在 sandbox 响应中；`POST /v1/sandboxes` 的错误码（例如 `503 backend_unavailable`）才反映创建失败。

## 启动

```rust
use circus_sandbox::http::{HttpConfig, HttpServer};

let config = HttpConfig::new(
    "127.0.0.1:8080".parse()?,
    "explicit-token",
    ["tenant-a"],
)?;
let server = HttpServer::bind(config).await?;
let served = server.serve().await?;
assert!(served.report.complete());
```

`HttpServer::bind` 先绑定监听地址；端口 `0` 由操作系统选择，`local_addr()` 返回实际地址。`serve()` 等待 Ctrl+C，`serve_with_shutdown(future)` 接受自定义关闭信号。两者都会在停止接收连接、排空在途请求后逐个销毁仍持有的 sandbox。

示例程序使用显式环境变量，不读取或编造凭据：

```bash
CIRCUS_HTTP_TOKEN=example-token CIRCUS_HTTP_TENANT=local \
  cargo run --locked --example http_api
```

它只启动传输。成功打印监听地址不代表 Docker 可用；创建 sandbox 仍要求本地 Docker Engine 和已存在的 Linux 镜像。

## curl 示例

以下示例假设服务监听 `127.0.0.1:8080`：

```bash
export CIRCUS_BASE=http://127.0.0.1:8080
export CIRCUS_TOKEN=example-token
export CIRCUS_TENANT=tenant-a
export AUTH="Authorization: Bearer $CIRCUS_TOKEN"
export TENANT="X-Circus-Tenant: $CIRCUS_TENANT"
```

### 健康检查

```bash
curl -sS -H "$AUTH" -H "$TENANT" "$CIRCUS_BASE/healthz"
# {"status":"ok"}
```

### 创建 sandbox

请求直接映射 `BackendConfig` 与 `SandboxConfig`。`backend.kind` 只接受 `docker`，`backend.runtime` 只接受 `runc`；其他值返回 `501 unsupported_backend`。镜像必须已存在，HTTP 创建不会拉取。

```bash
curl -sS -X POST -H "$AUTH" -H "$TENANT" \
  -H 'content-type: application/json' \
  "$CIRCUS_BASE/v1/sandboxes" \
  --data '{
    "backend": {
      "kind": "docker",
      "endpoint": "unix:///var/run/docker.sock",
      "image": "python:3.12-slim",
      "runtime": "runc"
    },
    "sandbox": {
      "memory_bytes": 67108864,
      "cpu_millis": 1000,
      "pids": 32,
      "workspace_bytes": 1048576,
      "max_file_bytes": 65536,
      "isolation": {
        "workspace_path": "/workspace",
        "guest_uid": 1000,
        "guest_gid": 1000
      },
      "io": {
        "output_limit_bytes": 65536,
        "command_timeout_ms": 5000,
        "file_timeout_ms": 5000,
        "lifecycle_timeout_ms": 5000
      }
    }
  }'
# 201 {"id":"<uuid>","phase":"created","enforcement":null}
```

`isolation` 的结构固定为强制执行 `no-new-privileges`、丢弃全部 capability、只读根文件系统、禁用网络和 tmpfs 工作区；请求不能关闭这些要求。超时单位为毫秒，取值 `1..=86400000`（24 小时）。

```bash
export SANDBOX_ID=<uuid>
```

### 查询状态

```bash
curl -sS -H "$AUTH" -H "$TENANT" "$CIRCUS_BASE/v1/sandboxes/$SANDBOX_ID"
# {"id":"<uuid>","phase":"created","enforcement":null}
```

`phase` 为 `created`、`running`、`paused`、`destroyed` 或 `cleanup_required`。`enforcement` 是后端 inspect 到的证据，缺失字段为 `null`；不会用请求值替代证据。

### 启动、暂停、恢复

```bash
curl -sS -X POST -H "$AUTH" -H "$TENANT" \
  "$CIRCUS_BASE/v1/sandboxes/$SANDBOX_ID/start"
# 204

curl -sS -X POST -H "$AUTH" -H "$TENANT" \
  "$CIRCUS_BASE/v1/sandboxes/$SANDBOX_ID/pause"
# 204

curl -sS -X POST -H "$AUTH" -H "$TENANT" \
  "$CIRCUS_BASE/v1/sandboxes/$SANDBOX_ID/resume"
# 204
```

阶段不允许时返回 `409 conflict`。`start` 的 staging 总量超过工作区字节上限时返回 `413 limit_exceeded`。

### 执行命令

`argv` 是分开的参数，不经过 shell。stdout 与 stderr 共享请求的 `output_limit_bytes`；该值不能超过创建时配置的 `output_limit_bytes`，否则命令不会开始执行并返回 `413 limit_exceeded`。实际运行中输出超出请求预算时同样返回 `413 limit_exceeded`，并按 crate 语义回收该实例。非零退出码仍是 `200`，输出用 base64 传输，因为 JSON 字符串不承载任意字节。

```bash
curl -sS -X POST -H "$AUTH" -H "$TENANT" \
  -H 'content-type: application/json' \
  "$CIRCUS_BASE/v1/sandboxes/$SANDBOX_ID/exec" \
  --data '{"argv":["python3","-c","print(6*7)"],"timeout_ms":3000,"output_limit_bytes":4096}'
# 200 {"exit_code":0,"stdout_base64":"NDIK","stderr_base64":""}
```

### 读写工作区文件

读取返回 `application/octet-stream` 原始字节；写入使用原始请求体，不额外包装 JSON。

```bash
printf 'hello' > /tmp/hello.txt

curl -sS -X PUT -H "$AUTH" -H "$TENANT" \
  -H 'content-type: application/octet-stream' \
  --data-binary @/tmp/hello.txt \
  "$CIRCUS_BASE/v1/sandboxes/$SANDBOX_ID/files/hello.txt"
# 204

curl -sS -H "$AUTH" -H "$TENANT" \
  "$CIRCUS_BASE/v1/sandboxes/$SANDBOX_ID/files/hello.txt"
# hello
```

路径是工作区相对 slash 路径，可包含多段；空路径、绝对路径、`..`、Windows 盘符前缀和反斜杠返回 `400 invalid_request`。父目录必须已存在：HTTP API 不提供 `mkdir` 端点，需要嵌套路径时可在 `running` 阶段用 `exec` 创建目录，或先恢复包含该目录的快照。服务 body 上限和 sandbox 单文件上限都生效，超限返回 `413 limit_exceeded`。`created` 阶段操作宿主 staging；`running` 阶段通过受控 guest 传输。文件不存在返回 `404 not_found`。

### 快照捕获与恢复

捕获把普通工作区文件与目录复制到进程内快照。它不是进程、内存、容器或 VM checkpoint，也不会持久化；服务重启后丢失。快照 id 按租户隔离。

```bash
curl -sS -X POST -H "$AUTH" -H "$TENANT" \
  -H 'content-type: application/json' \
  "$CIRCUS_BASE/v1/sandboxes/$SANDBOX_ID/snapshots" \
  --data '{"max_total_bytes":1048576,"max_entries":1024,"max_archive_bytes":4194304}'
# 201 {"snapshot_id":"<uuid>","entries":1,"total_bytes":5}
```

```bash
export SNAPSHOT_ID=<uuid>
export TARGET_ID=<another-created-sandbox-uuid>

curl -sS -X POST -H "$AUTH" -H "$TENANT" \
  "$CIRCUS_BASE/v1/sandboxes/$TARGET_ID/snapshots/$SNAPSHOT_ID/restore"
# 204
```

恢复只接受 `created` 阶段的 staging；`running`、`paused`、`destroyed`、`cleanup_required` 返回 `409 conflict`。捕获预算、archive 上限、服务级快照字节/条目上限和每租户保留数量都生效；超限返回 `413 limit_exceeded`。没有删除快照的端点：快照保留到进程退出，或由每租户保留上限拒绝后续捕获。

### 销毁

```bash
curl -sS -X DELETE -H "$AUTH" -H "$TENANT" \
  "$CIRCUS_BASE/v1/sandboxes/$SANDBOX_ID"
# 204
```

`204` 只在清理确认容器不存在且 staging 删除后返回。清理未确认时返回 `500 cleanup_required`，sandbox 句柄仍保留在注册表里，随后再次 `DELETE` 可重试；失败不会被写成成功。

## 错误码

错误体固定为 `{"code": "...", "message": "..."}`，`message` 来自固定的 code/失败类别文本，不包含请求值、后端文本或内部类型；客户端应以 `code` 为准。

- `400 invalid_request`：JSON 损坏、字段未知、配置值非法、路径被拒绝、命令请求非法。
- `401 unauthorized`：缺失或错误 bearer token。
- `403 forbidden`：缺失或不在 allow-list 的租户。
- `404 not_found`：未知路由、sandbox、snapshot 或文件；包含跨租户 id。
- `405 method_not_allowed`：路径存在但方法未注册。
- `409 conflict`：当前 phase 不允许该操作，或已有交互进程。
- `413 limit_exceeded`：body、文件、输出、staging、快照或保留数量超限。
- `408 cancelled`：请求取消。
- `504 timeout`：配置的 deadline 触发。
- `500 cleanup_required`：清理未确认；`DELETE` 可重试。
- `503 backend_unavailable`：本地后端、镜像或 worker 不可用。
- `501 unsupported_backend`：请求的后端类型、runtime 或能力未实现。
- `500 internal_error`：无法完成请求；不含内部类型或后端文本。

## 取消与清理归属

HTTP handler 直接 `await` 领域 future。客户端断开或连接被关闭时，axum 丢弃 handler future，也就丢弃了正在等待的领域 future：

- `exec`、文件读写、`start`/`pause`/`resume`、快照捕获/恢复的 future 被丢弃时，crate 的生命周期语义照常生效：该操作可能已被后端接受，因此请求整体实例清理；清理结果无法回写给已断开的客户端。
- `destroy` 没有调用方 deadline，丢弃 future 不会取消 worker 中的清理。HTTP 层只有在 `destroy` 返回 `Ok` 后才从注册表移除句柄；请求中断时句柄保留，后续 `DELETE` 可重试并观察到结果。
- 清理失败时阶段为 `cleanup_required`，句柄和身份继续由服务持有，直到确认清理或进程退出。
- 服务优雅关闭会先排空在途请求，再对注册表中每个 sandbox 顺序调用 `destroy`。失败记录为 `ShutdownFailure { tenant, sandbox_id, code: "cleanup_required" }`；`ShutdownReport::complete()` 只在全部确认后为 `true`。关闭不是 HTTP 响应，不能用 `200` 表示成功。
- 直接丢弃 `HttpServer` 而不调用 `serve*` 时只会请求清理，不确认结果；需要可观察清理时使用优雅关闭。

已经开始的阻塞文件工作不会被 abort 终止，关闭会等待它排空；这与领域库的文档一致。

## 服务限额

`HttpConfig` 提供显式且必须为正的传输/保留限额：

- `with_max_body_bytes`：JSON 请求体与原始文件写入上限，默认 16 MiB。
- `with_snapshot_limits(max_bytes, max_entries)`：单次捕获声明预算的上限，默认 64 MiB / 65536 条目。
- `with_max_snapshots_per_tenant`：每租户保留快照数，默认 32。

这些都是传输和内存保留限额，不是容器资源限额。容器内存、CPU、PID、工作区和单文件大小仍由创建请求里的 `SandboxConfig` 决定。

## 范围与未实现

- HTTP 传输目前只接受 `docker` + `runc`；库 API 另有 `containerd` + `runc` 与 Kubernetes workload（`kubernetes` feature，含工作区文件快照与交互进程）。没有 gVisor、Kata、Firecracker；Kubernetes 工作区的暂停/恢复仍未实现，交互进程仅通过库 API 提供，未在此 HTTP 传输中暴露。
- 没有持久化、多进程共享注册表、TLS 终止、速率限制、指标或审计日志；也没有每租户 sandbox 数量上限或聚合准入控制。
- 没有空闲回收或 TTL：成功创建后未 `DELETE` 的 sandbox 会保留在进程内注册表，直到显式销毁或服务优雅关闭。
- 没有 OpenAPI 运行时生成器；`docs/openapi.json` 是手工维护的契约，由漂移测试对照真实路由表验证。
- 交互进程/会话 API 未暴露；HTTP 只提供一次性 `exec`。`spawn_process` 仍仅作为库 API 使用。
