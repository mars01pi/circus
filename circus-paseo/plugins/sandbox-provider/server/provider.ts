import { createHash } from "node:crypto";
import { access, constants, mkdir } from "node:fs/promises";
import { isAbsolute, join } from "node:path";
import {
  negotiateProviderCapabilities,
  requireProviderCapabilities,
  type ProviderConnection,
  type ProviderEvent,
  type ProviderInput,
  type ProviderLaunch,
  type ProviderPersistence,
  type ProviderRegistration,
} from "@getpaseo/plugin/server/provider";
import { z } from "zod";
import { CircusStdioClient, type CircusClientConfig } from "./bridge/stdio-client.js";
import { isAbsoluteHostPath } from "./bridge/protocol.js";
import { ArtifactStore } from "./artifacts.js";
import { SandboxProviderError } from "./errors.js";
import { SandboxSession, safeError, type TaskClient } from "./session.js";
import { SANDBOX_HOST_MODEL_ID, SANDBOX_PROVIDER_ID } from "../shared/task.js";
import { readMcpCatalog } from "./mcp-catalog.js";

const capabilities = ["prompt.message", "timeline.plugin", "session.persistence"] as const;
const modes = [
  {
    id: "sandbox",
    label: "Sequential sandbox",
    description:
      "Sequential turns: each prompt recreates the sandbox from completed turns only; running processes and in-flight turns are not preserved",
    isUnattended: true,
  },
];
const hostConfiguredModel = {
  id: SANDBOX_HOST_MODEL_ID,
  label: "Host configured",
  description: "The host runner selects the configured agent and model",
  isDefault: true,
} as const;
const persistenceSchema = z.strictObject({
  version: z.literal(1),
  data: z.strictObject({
    continuationDisabled: z.literal(true).optional(),
    agentSession: z
      .string()
      .regex(/^[a-zA-Z0-9_-]{1,128}$/)
      .optional(),
  }),
});
type PersistenceData = z.infer<typeof persistenceSchema>["data"];

export type ClientFactory = (config: CircusClientConfig) => TaskClient;
export type StateDirPreparer = (path: string) => Promise<void>;

export class SandboxProvider {
  private readonly sessions = new Set<SandboxSession>();
  private configFile: string | null = null;

  constructor(
    private readonly artifacts: ArtifactStore,
    private readonly createClient: ClientFactory = (config) => new CircusStdioClient(config),
    private readonly prepareStateDir: StateDirPreparer = async (path) => {
      await mkdir(path, { recursive: true, mode: 0o700 });
    },
  ) {}

  registration(): ProviderRegistration {
    return {
      id: SANDBOX_PROVIDER_ID,
      label: "Sandbox",
      icon: "icon.svg",
      requiresConfiguration: true,
      description:
        "Sequential Codex turns in disposable sandboxes, each restored from the session's completed workspace and native thread",
      command: ["circus-stdio-bridge"],
      status: async ({ launch }) => {
        try {
          const config = launchConfig(launch);
          await access(config.executable, constants.X_OK);
          await access(config.configFile, constants.R_OK);
          this.configFile = config.configFile;
          return { available: true };
        } catch {
          this.configFile = null;
          return {
            available: false,
            diagnostic:
              "Configure the sandbox provider with an absolute circus-stdio-bridge command, CIRCUS_RUNNER_CONFIG pointing to a readable host configuration file, and CIRCUS_SANDBOX_STATE_DIR pointing to an absolute host state directory.",
          };
        }
      },
      connect: async (request) => {
        if (!request.versions.includes(1))
          throw new SandboxProviderError("version", "Provider protocol version 1 is required");
        const config = launchConfig(request.launch);
        this.configFile = config.configFile;
        return this.connect(
          config,
          negotiateProviderCapabilities(request.capabilities, capabilities),
        );
      },
    };
  }

  mcpCatalog() {
    return readMcpCatalog(this.configFile);
  }

  async retryCleanup(taskId: string) {
    const session = [...this.sessions].find((candidate) => candidate.matchesTask(taskId));
    if (!session)
      throw new SandboxProviderError(
        "task_unavailable",
        "This task is no longer connected. Check sandbox cleanup on the host.",
      );
    return { cleanup: await session.retryCleanup() };
  }

  private connect(config: CircusClientConfig, negotiated: readonly string[]): ProviderConnection {
    const sessions = new Map<string, SandboxSession>();
    const listeners = new Set<(event: ProviderEvent) => void>();
    let closed = false;
    function emit(event: ProviderEvent): void {
      for (const listener of listeners) listener(event);
    }
    const dispatch = async (input: ProviderInput) => {
      if (input.type === "catalog") {
        emit({
          type: "catalog",
          requestId: input.requestId,
          catalog: {
            models: [hostConfiguredModel],
            modes,
            defaultModel: SANDBOX_HOST_MODEL_ID,
            defaultMode: "sandbox",
          },
        });
        return;
      }
      if (input.type === "session.open") {
        if (sessions.has(input.sessionId))
          throw new SandboxProviderError("duplicate", "Sandbox session already exists");
        const restored = readPersistence(input.persistence);
        if (restored.continuationDisabled)
          throw new SandboxProviderError(
            "unsupported",
            "This agent did not return a resumable session. Start a new sandbox session.",
          );
        if (input.config.toolPolicy)
          throw new SandboxProviderError(
            "unsupported",
            "Sandbox tasks do not accept host tool policies",
          );
        if (input.config.model !== SANDBOX_HOST_MODEL_ID)
          throw new SandboxProviderError(
            "unsupported",
            `Sandbox model must be ${SANDBOX_HOST_MODEL_ID}; the host runner selects the real model`,
          );
        const archive = workspaceArchive(config.stateDir, input.sessionId);
        if (!isAbsoluteHostPath(archive))
          throw new SandboxProviderError(
            "configuration",
            "CIRCUS_SANDBOX_STATE_DIR must yield a bounded absolute workspace archive path.",
          );
        await this.prepareStateDir(config.stateDir);
        if (restored.agentSession !== undefined) {
          try {
            await access(archive, constants.R_OK);
          } catch {
            throw new SandboxProviderError(
              "invalid_persistence",
              "The persisted sandbox workspace is missing; open a new sandbox session.",
            );
          }
        }
        const session = new SandboxSession({
          id: input.sessionId,
          config: input.config,
          createClient: () => this.createClient(config),
          archive,
          agentSession: restored.agentSession,
          artifacts: this.artifacts,
          emit,
        });
        sessions.set(input.sessionId, session);
        this.sessions.add(session);
        const persistence: ProviderPersistence = {
          version: 1,
          data: restored.agentSession === undefined ? {} : { agentSession: restored.agentSession },
        };
        emit({
          type: "session.opened",
          requestId: input.requestId,
          sessionId: input.sessionId,
          capabilities: negotiated,
          restoration: "core",
          persistence,
          cwd: input.config.cwd,
        });
        emit({ type: "session.persistence", sessionId: input.sessionId, persistence });
        emit({
          type: "session.config",
          sessionId: input.sessionId,
          config: {
            model: SANDBOX_HOST_MODEL_ID,
            models: [hostConfiguredModel],
            modes,
            mode: "sandbox",
            thinkingOptions: [],
            settings: [],
          },
        });
        emit({ type: "session.ready", requestId: input.requestId, sessionId: input.sessionId });
        return;
      }
      if (!("sessionId" in input))
        throw new SandboxProviderError("unsupported", "Unsupported sandbox operation");
      const session = sessions.get(input.sessionId);
      if (!session)
        throw new SandboxProviderError("missing_session", "Sandbox session is not open");
      switch (input.type) {
        case "session.prompt":
          session.prompt(input.prompt);
          return;
        case "session.interrupt":
          await session.interrupt();
          break;
        case "session.close":
          await session.close();
          sessions.delete(input.sessionId);
          this.sessions.delete(session);
          emit({ type: "session.closed", sessionId: input.sessionId });
          break;
        default:
          throw new SandboxProviderError("unsupported", "Unsupported sandbox operation");
      }
      emit({ type: "request.completed", requestId: input.requestId });
    };
    return {
      version: 1,
      capabilities: negotiated,
      onEvent(listener) {
        listeners.add(listener);
        return () => listeners.delete(listener);
      },
      async send(input) {
        if (closed) throw new SandboxProviderError("closed", "Sandbox provider is closed");
        try {
          requireProviderCapabilities(negotiated, input);
          await dispatch(input);
        } catch (failure) {
          const error = safeError(failure);
          if (input.type === "session.prompt")
            emit({
              type: "session.prompt_result",
              sessionId: input.sessionId,
              clientMessageId: input.prompt.clientMessageId,
              result: { type: "failed", error },
            });
          else if ("requestId" in input)
            emit({ type: "request.failed", requestId: input.requestId, error });
          else throw failure;
        }
      },
      close: async () => {
        if (closed) return;
        const results = await Promise.allSettled(
          [...sessions.values()].map((session) => session.close()),
        );
        const failed = results.find((result) => result.status === "rejected");
        if (failed?.status === "rejected") throw failed.reason;
        for (const session of sessions.values()) this.sessions.delete(session);
        sessions.clear();
        listeners.clear();
        closed = true;
      },
    };
  }
}

/** Deterministic archive path; session ids cannot inject path segments. */
export function workspaceArchive(stateDir: string, sessionId: string): string {
  const digest = createHash("sha256").update(sessionId).digest("hex");
  return join(stateDir, `${digest}.snapshot`);
}

function readPersistence(persistence: ProviderPersistence | undefined): PersistenceData {
  if (!persistence) return {};
  if (persistence.version !== 1)
    throw new SandboxProviderError(
      "unsupported",
      "Sandbox persistence version 1 is required to resume a session.",
    );
  const parsed = persistenceSchema.safeParse(persistence);
  if (!parsed.success)
    throw new SandboxProviderError(
      "invalid_persistence",
      "Sandbox persistence data is invalid; open a new sandbox session.",
    );
  return parsed.data.data;
}

export function launchConfig(launch: ProviderLaunch | undefined): CircusClientConfig {
  const configFile = launch?.env.CIRCUS_RUNNER_CONFIG;
  const stateDir = launch?.env.CIRCUS_SANDBOX_STATE_DIR;
  if (
    !launch ||
    !configFile ||
    !isAbsolute(configFile) ||
    !stateDir ||
    !isAbsoluteHostPath(stateDir) ||
    !isAbsolute(launch.command) ||
    launch.args.length !== 0
  ) {
    throw new SandboxProviderError(
      "configuration",
      "Set an absolute bridge executable, CIRCUS_RUNNER_CONFIG and CIRCUS_SANDBOX_STATE_DIR; command arguments are not supported.",
    );
  }
  const env: Record<string, string> = {};
  // The host config supplies absolute Python/model-key paths. No login, proxy or HOME inheritance.
  for (const key of ["PATH", "SystemRoot", "WINDIR", "TEMP", "TMP", "TMPDIR"]) {
    const value = launch.env[key];
    if (value !== undefined) env[key] = value;
  }
  return { executable: launch.command, configFile, stateDir, env };
}
