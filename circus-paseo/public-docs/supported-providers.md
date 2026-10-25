---
title: Supported providers
description: Every coding agent Paseo can launch, natively supported providers and the ACP catalog.
nav: Supported providers
order: 21
category: Providers
---

# Supported providers

For the concept and how Paseo manages providers, see [Providers](/docs/providers). To add or configure your own, see [Custom providers](/docs/custom-providers).

## Native support

Work out of the box once the underlying CLI is installed and authenticated.

- [Claude Code](https://docs.anthropic.com/en/docs/claude-code). Anthropic's coding agent with MCP support, streaming, and deep reasoning.
- [Codex](/docs/codex). OpenAI's workspace agent with sandbox controls and optional network access.
- [OpenCode](https://opencode.ai/). Open-source coding assistant with multi-provider model support.
- [Pi](https://pi.dev). Minimal terminal-based coding agent with multi-provider LLM support.
- [Antigravity](#antigravity). Uses your installed `agy` CLI through a built-in provider plugin.
- [Muse Code](/docs/muse-code). Meta's terminal coding agent, bundled as a provider plugin.

### Antigravity

Install the Antigravity CLI (`agy`) on your daemon host, then run `agy` once to sign in. Pick
**Antigravity** in Paseo. The provider ships with Paseo; no separate plugin install is required.

**Full access** is the only permission mode. Antigravity cannot ask for permission when another
app drives it, so Paseo starts `agy` with `--dangerously-skip-permissions`. Every tool call,
including shell commands, runs without asking. Each session starts with a warning explaining
this, including when you reopen a chat. Antigravity's own policy can still deny
an action.

After a daemon restart, Antigravity remembers the conversation, but Paseo's transcript starts
fresh because `agy` does not replay its history.

This integration uses your installed CLI and sign-in. It is not endorsed by Google.

Paseo MCP tools and steering are unavailable. Your own `agy` MCP servers still work. Attached
images are passed as file references for the agent to read.

If `agy` is outside your daemon's PATH, override its command in `~/.paseo/config.json`. Use `env`
for provider-specific environment variables:

```json
{
  "agents": {
    "providers": {
      "antigravity": {
        "command": ["/absolute/path/to/agy"],
        "env": { "EXAMPLE_VARIABLE": "value" }
      }
    }
  }
}
```

See [Custom providers](/docs/custom-providers) for command and environment overrides.

## Sandbox (Codex)

Circus's **Sandbox** provider is opt-in. The host configuration selects Codex or Claude Code and
its matching pinned image and model protocol. On macOS, open the local desktop host settings and
use **Sandbox setup** to detect prerequisites, prepare Codex, test the model connection and save its
API key in Keychain. This source-checkout workflow requires Docker Desktop and build prerequisites;
it is not a bundled installer. Browser and remote clients must configure the desktop host first.

For manual setup or Claude Code, install the Circus Rust bridge on the daemon host and configure
its Docker endpoint, matching agent image, resource limits and model channel. A credential can be
an explicit private key file or macOS Keychain reference. Personal Agent logins, home directories,
proxy settings, ambient MCP servers, system prompts and agent environments are not imported.

Add this entry to the daemon configuration, replacing all three absolute paths:

```json
{
  "agents": {
    "providers": {
      "sandbox": {
        "enabled": true,
        "command": ["/absolute/path/to/circus-stdio-bridge"],
        "env": {
          "CIRCUS_RUNNER_CONFIG": "/absolute/path/to/runner.json",
          "CIRCUS_SANDBOX_STATE_DIR": "/absolute/path/to/state"
        }
      }
    }
  }
}
```

Choose **Sandbox** for a prompt-only task. To supply files, open the workspace's
**Sandbox task** panel. Choose files on web/desktop, or paste text with an explicit relative path;
review the list, enter each file to export and set a deadline. Browser selection imports the chosen
bytes, including current unsaved changes only if you explicitly paste them. It does not scan the
project directory. Duplicate names are rejected; nested paths can be supplied through the pasted
file control or the SDK's `options.inputFiles`.

The **Tool permissions** section lists host-configured MCP connections, tools and fixed resources.
Select only the tools this task needs; an empty selection grants none. Connection credentials stay
on the host, and task requests cannot change endpoints or fixed arguments. The list describes host
policy, not a live service connection check. The desktop GitHub preset authenticates and discovers
`issue_read` before saving a connection, fixing `method=get`, owner and repository. Runtime access
still depends on that service and account. This release supports HTTPS Streamable HTTP tools;
OAuth login, arbitrary local stdio MCP programs and full MCP feature compatibility remain outside
this implementation.

The normal task timeline shows text, tool output and progress through export and cleanup.
**Stop** waits for the running task's cancellation result. Successful completion requires confirmed
sandbox cleanup. A result with pending cleanup offers **Retry cleanup**. Unknown cleanup after a
bridge failure requires checking the runner's owned resources on the host.

Exports remain separate from the project. Preview and download them from the result card on
web/desktop; native clients can preview and copy UTF-8 text. Binary download currently requires
web/desktop. Files are held in a bounded, in-memory daemon cache (64 MiB, 256 files) and expire when
the daemon restarts. Download them before restarting. No automatic patch application, project
overwrite, Git commit or workspace cleanup is performed.

Sandbox sessions are sequential. Each prompt runs in a fresh sandbox restored from the session's
plaintext workspace archive in `CIRCUS_SANDBOX_STATE_DIR`. Each turn captures into a separate candidate
archive; only a successful turn with a resumable thread replaces the committed archive. Failed or
cancelled turns leave the previous checkpoint unchanged. Codex resumes the native thread returned by the previous completed turn,
so follow-up prompts retain completed files and conversation. Running processes, in-flight turns
and unsandboxed host state are not preserved; a turn that is still running cannot be recovered
after a restart. Archive paths are derived from the session ID, are never accepted from prompts or
attachments, and are not encrypted, so they stay inside the trusted host state directory. Persistence
contains a versioned Codex thread handle, or a marker that continuation is unavailable; reopening a
resumable session after a daemon restart uses the same session ID and state directory to find the archive.

Steering, host tools, image prompts and automatic patch application are unavailable. Only one turn
may run at a time. **Stop** cancels the active turn and waits for its cleanup; a pending or unknown
cleanup blocks the next prompt until **Retry cleanup** confirms it. An agent that returns no resumable
handle, including Claude Code, remains single-turn: follow-up prompts and reopening are explicitly rejected.
This implementation uses Docker on Linux only.
The host configuration selects the model. Provider options `workspaceMaxTotalBytes` (default 64 MiB,
maximum 512 MiB) and `workspaceMaxEntries` (default 65,536, maximum 1,000,000) bound each archive
load and capture.

## ACP catalog

Pick any of these from the in-app provider catalog. Each entry is a one-click install of the underlying CLI.

- [Agoragentic](https://agoragentic.com), agent marketplace with 174+ AI capabilities.
- [Amp](https://github.com/tao12345666333/amp-acp), frontier coding agent.
- [Auggie CLI](https://www.augmentcode.com/), Augment Code's agent backed by their context engine.
- [Autohand Code](https://www.autohand.ai/cli/), Autohand AI's coding agent.
- [Cline](https://cline.bot/cli), autonomous coding agent.
- [Codebuddy Code](https://www.codebuddy.cn/cli/), Tencent Cloud's coding tool.
- [CodeWhale](https://codewhale.net/), terminal coding agent for DeepSeek V4 and open models.
- [Cortex Code](https://docs.snowflake.com/en/user-guide/cortex-code/cortex-code-cli), Snowflake's coding agent.
- [Corust Agent](https://github.com/Corust-ai/corust-agent-release/releases), Rust-focused coding partner.
- [crow-cli](https://crow-ai.dev/), minimal ACP-native coding agent.
- [Cursor](https://cursor.com/docs/cli/overview), Cursor's coding agent.
- [DeepAgents](https://docs.langchain.com/oss/javascript/deepagents/overview), LangChain-powered coding agent.
- [DimCode](https://dimcode.dev/docs/acp.html), multi-model coding agent.
- [Dirac](https://dirac.run), open-source agent with hash-anchored parallel edits.
- [Factory Droid](https://factory.ai/product/cli), Factory AI's coding agent.
- [fast-agent](https://fast-agent.ai/acp/), multi-provider coding agent.
- [Gajae Code](https://gajae-code.com), subscription-based coding agent with plan-before-mutation workflows.
- [Gemini CLI](https://geminicli.com), Google's official Gemini CLI.
- [GitHub Copilot](https://github.com/features/copilot/cli/), GitHub's AI pair programmer via ACP.
- [GLM Agent](https://github.com/stefandevo/glm-acp-agent), Zhipu AI's GLM coding agent.
- [goose](https://block.github.io/goose/), Block's local open-source AI agent.
- [Grok](https://docs.x.ai/build/overview), xAI's Grok Build agentic coding CLI.
- [Hermes Agent](https://hermes-agent.nousresearch.com/docs/user-guide/features/acp), Nous Research's self-improving agent.
- [Hoplite](#hoplite), cloud coding agent through its local ACP bridge.
- [Junie](https://junie.jetbrains.com/docs/junie-cli-acp.html), JetBrains' coding agent.
- [Kilo Code](https://kilo.ai/docs/code-with-ai/platforms/cli), open-source coding agent.
- [Kimi Code CLI](https://github.com/MoonshotAI/kimi-code), Moonshot AI's coding assistant.
- [Minion Code](https://github.com/femto/minion-code), Minion-framework coding agent.
- [Mistral Vibe](https://github.com/mistralai/mistral-vibe), Mistral's open-source CLI assistant.
- [Nova](https://www.compassap.ai/portfolio/nova.html), Compass AI's software engineer.
- [Poolside](https://docs.poolside.ai/cli/pool), Poolside's coding agent.
- [Qoder](https://qoder.com), agentic coding assistant.
- [Qwen Code](https://qwenlm.github.io/qwen-code-docs/en/users/overview), Alibaba's Qwen coding assistant.
- [siGit Code](https://github.com/getsigit/sigit), local-first coding agent with optional on-device LLM.
- [Stakpak](https://stakpak.dev/), Rust-based DevOps agent.
- [TRAE CLI](https://docs.trae.cn/cli_get-started-with-trae-cli), ByteDance's official TRAE coding agent.
- [VT Code](https://github.com/vinhnx/VTCode/blob/main/docs/guides/zed-acp.md), open-source multi-provider coding agent.

The in-app catalog is the canonical, version-pinned source. Anything not listed here can still be added manually, see [Custom providers](/docs/custom-providers).

### Hoplite

Install the [Hoplite CLI](https://hoplite.sh/docs/cli) on your daemon host and run
`hoplite login`. In **Settings → Hosts → your host → Providers**, find **Hoplite**
in the catalog and choose **Add**. Paseo launches `hoplite acp` through the generic ACP adapter.

Hoplite runs the agent in a cloud workspace. Open a repository linked to a Hoplite project,
or set `agents.providers.hoplite.env.HOPLITE_PROJECT_ID` to use one project everywhere.
Without a linked repository or pinned project, discovery reports `No Hoplite project`.
Your local files and Paseo's loopback MCP tools are not available in that cloud workspace.

See [Hoplite's ACP guide](https://hoplite.sh/docs/cli/acp) for authentication and project setup.
Use a [command override](/docs/custom-providers) if `hoplite` is outside your daemon's PATH;
keep `acp` in the arguments.
