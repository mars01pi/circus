# 在沙箱中运行 Claude Code

Claude Code 适配器复用 Agent runner 的输入文件、事件、产物、取消和清理契约。与 Codex 的 Responses 通道不同，它使用 **Anthropic Messages HTTPS API**；模型名称与密钥由可信宿主配置提供，不读取个人 Claude Code 登录状态。

## 执行与凭据边界

```text
宿主 AgentRunner / stdio bridge
  ├─ 持有真实模型 key、HTTPS 目标、模型名、CA 配置
  ├─ 只转发指定模型的有界 Messages SSE 请求
  └─ Docker + runc 一次性沙箱（禁网、non-root、只读根、无宿主 bind）
       ├─ 127.0.0.1 模型入口，只接受本任务临时 token
       ├─ claude --bare -p --output-format stream-json
       └─ /workspace/project：显式输入与请求导出的产物
```

真实上游 key 只由宿主写入 `X-Api-Key`。沙箱内的 `ANTHROPIC_API_KEY` 是本任务临时 token，不是服务凭据。宿主拒绝更改模型、目的地址、任意认证头、重定向及超预算请求，固定使用 `/messages?beta=true`；只允许 `anthropic-version`、`anthropic-beta` 两类版本元数据。TLS 证书验证始终开启。

固定 CLI 以 `--bare` 启动，不自动加载项目/用户的 hooks、插件、登录与 MCP 配置；默认仅启用 Bash、Read、Write、Edit、Glob、Grep，在容器边界内执行。未授权的交互与权限提示使任务失败。任务取消和超时沿用 lifecycle 的容器销毁责任；成功只有在取回产物并确认清理后发布。

密钥留在宿主不等于阻止获准读取的数据发送给模型。模型服务目标、输入文件与 MCP 权限仍应由用户明确授权；此实现不承诺容器内核零漏洞或宿主被攻陷后的凭据安全。

## 运行示例

前提：本地 Linux Docker Engine + runc、Rust 1.99.1、宿主 Python 3.11+，以及[已构建的固定镜像](../images/claude-code/README.md)。模型服务须支持 Anthropic Messages 流与工具调用，不能只支持 OpenAI Chat Completions 或 Responses。

在 `circus-sandbox/` 下，以 Bash 运行：

```bash
export CIRCUS_DOCKER_ENDPOINT=unix:///var/run/docker.sock
export CIRCUS_AGENT_IMAGE=sha256:REPLACE_WITH_VERIFIED_IMAGE_ID
export CIRCUS_MODEL_BASE_URL=https://api.anthropic.com/v1
export CIRCUS_MODEL=REPLACE_WITH_AUTHORIZED_MODEL_ID
read -r -s -p 'Model API key: ' CIRCUS_MODEL_API_KEY
export CIRCUS_MODEL_API_KEY
printf '\n'
export CIRCUS_AGENT_PROMPT='Fix answer.py add(a, b). Run assertions for add(20, 22) == 42 and add(-2, 2) == 0. Write 42 followed by a newline to result.txt.'
cargo run --locked -p circus-agent-runner --example claude_agent
unset CIRCUS_MODEL_API_KEY
```

`baseUrl` 包含 `/v1`，运行器在其后添加 `/messages?beta=true`。使用 Docker Desktop 时显式设置它的 Linux Engine socket；不把 socket 挂进沙箱。私有 CA 可通过 `CIRCUS_MODEL_CA_FILE` 指定，解释器可通过 `CIRCUS_HOST_PYTHON` 指定。

示例自动放入有缺陷的 `answer.py`，取回 `answer.py` 与 `result.txt`，只打印产物字节数和 `result.txt` 内容，不自动覆盖宿主文件。Rust 调用方使用：

```rust,ignore
let runner = AgentRunner::new(config)?.with_agent(AgentKind::ClaudeCode);
let task = runner.start(request)?;
```

[stdio bridge](stdio-bridge.md) 的宿主配置增加 `"agent": "claude-code"`，并选择 Claude 镜像及 Messages 模型配置。省略 `agent` 时保持 Codex 行为；不接受任意 executable 或 shell 命令作为 Agent 类型。

## 验证范围

策略测试覆盖固定目的地址、模型与认证、重放/头部注入拒绝、工具/文本事件归一化、失败与交互不冒充成功：

```bash
python3 -B -m unittest discover -s agent-runner/tests -p 'test_claude_bridge.py' -v
```

真实 Docker + 固定 Claude CLI + 本地 HTTPS 脚本对端验收：

```bash
cargo build --locked -p circus-agent-runner --example claude_agent
export CIRCUS_DOCKER_SANDBOX=1
export CIRCUS_CLAUDE_IMAGE=sha256:REPLACE_WITH_VERIFIED_IMAGE_ID
export CIRCUS_CLAUDE_EXAMPLE="$PWD/target/debug/examples/claude_agent"
python3 -B -m unittest discover -s agent-runner/tests -p 'test_claude_docker.py' -v
cargo test --locked -p circus-agent-runner claude_cancel_and_timeout -- --ignored
```

脚本对端要求真实 CLI 执行 Bash 工具：修改文件、运行 Python assertions，检查容器没有上游 key、Docker socket 与外部网络，再导出产物并独立 `docker inspect` 确认容器不存在。这证明真实 CLI、HTTPS Messages 传输、工具执行与清理链路，**不证明真实模型推理**；使用真实模型需另外提供有权限的服务配置。

当前仅单任务，不实现 Claude 登录、Bedrock/Vertex/Foundry 认证、多轮恢复、订阅 OAuth 复用或跨进程持久化。Claude 以 `--no-session-persistence` 运行；传入 `TaskRequest.agent_session` 会在分配沙箱前返回 `Invalid`，宿主入口也会拒绝 `CIRCUS_RESUME_THREAD`，不会静默开始一个无关的新 turn。MCP 的协议与授权范围以集成的 MCP 代理说明及实际测试为准。

集成 MCP 代理后，`test_claude_mcp_docker.py` 使用同一宿主工具代理验证真实 Claude CLI 的工具发现、授权调用、文件产物与清理。设置 `CIRCUS_CLAUDE_MCP_DOCKER=1`、`CIRCUS_BRIDGE_BINARY`（构建后的 stdio bridge 绝对路径）、`CIRCUS_CLAUDE_IMAGE` 与 `CIRCUS_DOCKER_ENDPOINT` 后显式运行。它使用本地 HTTPS MCP 服务和脚本化 Messages 对端，不声称真实 SaaS、OAuth 或真实模型已验证；代理会验证并丢弃 CLI 的追踪元数据，不向上游传递额外权限。

## 协议依据

- [Claude Code headless](https://code.claude.com/docs/en/headless)：bare、JSON stream、非交互权限。
- [CLI reference](https://code.claude.com/docs/en/cli-reference)：固定启动参数。
- [Gateway protocol](https://code.claude.com/docs/en/llm-gateway-protocol)：Messages 路径、SSE、版本头及可选 token counting。
- [Environment variables](https://code.claude.com/docs/en/env-vars)：模型入口、API key 与非必要网络关闭。

实现以锁定的 CLI 2.1.310 与实际验收为准；在线文档可能随新版本变化。
