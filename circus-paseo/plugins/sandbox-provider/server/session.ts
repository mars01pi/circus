import { randomUUID } from "node:crypto";
import { copyFile, rename, rm } from "node:fs/promises";
import type {
  ProviderEvent,
  ProviderPrompt,
  ProviderSessionConfig,
  ProviderTimelineItem,
} from "@getpaseo/plugin/server/provider";
import { taskOptionsSchema, SANDBOX_SESSION_NOTICE } from "../shared/task.js";
import type { TaskResultView } from "../shared/artifacts.js";
import { ArtifactStore, ArtifactError } from "./artifacts.js";
import {
  CircusBridgeError,
  type CircusStdioClient,
  type CircusTask,
  type CleanupStatus,
} from "./bridge/stdio-client.js";
import { isAbsoluteHostPath } from "./bridge/protocol.js";
import { SandboxProviderError } from "./errors.js";
import { TaskTimeline } from "./timeline.js";

export type TaskClient = Pick<CircusStdioClient, "start" | "close">;

interface SessionOptions {
  id: string;
  config: ProviderSessionConfig;
  /** One bridge process per prompt; a client never runs a second task. */
  createClient(): TaskClient;
  /** Stable caller-owned archive derived from the session id and state dir. */
  archive: string;
  /** Native Codex thread restored from persistence, if any. */
  agentSession?: string;
  artifacts: ArtifactStore;
  emit(event: ProviderEvent): void;
}

type RunState =
  | { status: "ready" }
  | {
      status: "active" | "finished";
      client: TaskClient;
      task: Promise<CircusTask>;
      done: Promise<void>;
      cleanup: CleanupStatus;
      candidate: string;
    };

export class SandboxSession {
  private state: RunState = { status: "ready" };
  private result: TaskResultView | null = null;
  private agentSession: string | undefined;
  private continuationDisabled = false;
  private readonly options;

  constructor(private readonly context: SessionOptions) {
    if (!isAbsoluteHostPath(context.archive))
      throw new SandboxProviderError(
        "configuration",
        "Sandbox workspace archive must be an absolute host path",
      );
    this.options = taskOptionsSchema.parse(context.config.providerOptions ?? {});
    this.agentSession = context.agentSession;
  }

  prompt(prompt: ProviderPrompt): void {
    const { id, emit } = this.context;
    const rejection = this.validatePrompt(prompt);
    if (rejection) {
      emit({
        type: "session.prompt_result",
        sessionId: id,
        clientMessageId: prompt.clientMessageId,
        result: { type: "failed", error: { code: "session_busy", message: rejection } },
      });
      return;
    }
    if (prompt.input.type !== "message") return;
    const text = prompt.input.content
      .map((part) => (part.type === "text" ? part.text : ""))
      .join("\n");
    const turnId = randomUUID();
    this.item({
      type: "user_message",
      id: `${turnId}:user`,
      clientMessageId: prompt.clientMessageId,
      text,
    });
    emit({
      type: "session.prompt_result",
      sessionId: id,
      clientMessageId: prompt.clientMessageId,
      result: { type: "turn", turnId },
    });
    emit({ type: "session.turn", sessionId: id, turnId, state: "started" });
    const inputFiles = Object.fromEntries(
      this.options.inputFiles.map((file) => {
        const bytes = "text" in file ? Buffer.from(file.text) : Buffer.from(file.base64, "base64");
        return [file.path, bytes];
      }),
    );
    let client: TaskClient;
    let pending: Promise<CircusTask>;
    try {
      client = this.context.createClient();
      this.item({
        type: "notification",
        id: `${turnId}:boundary`,
        level: "info",
        message: SANDBOX_SESSION_NOTICE,
      });
      // Only connection/tool IDs cross this boundary. Host MCP configurations,
      // credentials, system prompts, env and cwd never enter the task request.
      const candidate = `${this.context.archive}.${turnId}.pending`;
      pending = Promise.resolve().then(async () => {
        try {
          await copyFile(this.context.archive, candidate);
        } catch (error) {
          if ((error as NodeJS.ErrnoException).code !== "ENOENT" || this.agentSession !== undefined)
            throw error;
        }
        state.cleanup = "unknown";
        return client.start({
          prompt: text,
          inputFiles,
          outputPaths: this.options.outputPaths,
          timeoutMs: this.options.timeoutMs,
          mcpGrants: this.options.mcpGrants,
          workspaceArchive: candidate,
          workspaceMaxTotalBytes: this.options.workspaceMaxTotalBytes,
          workspaceMaxEntries: this.options.workspaceMaxEntries,
          ...(this.agentSession === undefined ? {} : { agentSession: this.agentSession }),
        });
      });
    } catch (failure) {
      this.promptFailure(turnId, failure);
      return;
    }
    const state: Exclude<RunState, { status: "ready" }> = {
      status: "active",
      client,
      task: pending,
      done: Promise.resolve(),
      cleanup: "confirmed",
      candidate: `${this.context.archive}.${turnId}.pending`,
    };
    this.state = state;
    state.done = this.consume(pending, client, turnId);
  }

  private promptFailure(turnId: string, failure: unknown): void {
    const error = safeError(failure);
    this.result = {
      taskId: turnId,
      status: "failed",
      cleanup: "unknown",
      artifacts: [],
      message: error.message,
    };
    this.publishResult();
    this.context.emit({
      type: "session.turn",
      sessionId: this.context.id,
      turnId,
      state: "failed",
      error,
    });
  }

  private validatePrompt(prompt: ProviderPrompt): string | null {
    if (this.continuationDisabled)
      return "This agent did not return a resumable session. Start a new sandbox session.";
    if (this.state.status === "active")
      return "Wait for the current sandbox turn to finish before sending another prompt.";
    if (this.state.status === "finished")
      return "Sandbox cleanup is not confirmed. Retry cleanup before the next prompt.";
    if (prompt.delivery === "steer") return "Sandbox tasks do not support steering.";
    if (prompt.input.type !== "message" || prompt.outputSchema)
      return "Sandbox tasks accept a text prompt and explicitly configured input files.";
    if (prompt.input.content.some((part) => part.type !== "text" || "mimeType" in part))
      return "Add files in the Sandbox task panel. Host attachments and image prompts are not imported automatically.";
    return null;
  }

  private async consume(
    pending: Promise<CircusTask>,
    client: TaskClient,
    turnId: string,
  ): Promise<void> {
    const timeline = new TaskTimeline(turnId);
    let terminal: "completed" | "failed" | "canceled" = "failed";
    let error: { message: string; code: string } | undefined;
    let taskId = turnId;
    try {
      const task = await pending;
      taskId = task.id;
      const drain = async () => {
        for await (const event of task.events) {
          const item = timeline.accept(event);
          if (item) this.item(item);
        }
      };
      const [outcome] = await Promise.all([task.result, drain()]);
      if (this.state.status === "active") this.state.cleanup = outcome.cleanup;
      if (outcome.status === "succeeded") {
        const message = await this.commitCheckpoint(outcome.agentSession);
        const artifacts = await this.context.artifacts.save(outcome.artifacts);
        this.result = {
          taskId,
          status: "succeeded",
          cleanup: "confirmed",
          artifacts,
          message,
        };
        terminal = "completed";
      } else {
        const message =
          outcome.status === "cancelled"
            ? "Task stopped. The next prompt continues from the previous completed turn."
            : `${outcome.error.message} (${outcome.error.code})`;
        this.result = {
          taskId,
          status: outcome.status,
          cleanup: outcome.cleanup,
          artifacts: [],
          message,
        };
        error = outcome.error;
        terminal = outcome.status === "cancelled" ? "canceled" : "failed";
      }
      if (outcome.cleanup === "confirmed") {
        await this.finishConfirmed(this.state, client);
      } else if (this.state.status !== "ready") {
        // A retryable cleanup handle keeps its bridge alive; an untracked
        // unknown cleanup cannot be resumed safely and remains blocked.
        this.state.status = "finished";
        if (outcome.cleanup !== "pending") {
          const closeError = await this.release(client);
          if (closeError) this.appendMessage(closeError);
        }
      }
    } catch (failure) {
      terminal = "failed";
      error = safeError(failure);
      const current = this.state;
      const cleanup = current.status === "ready" ? "unknown" : current.cleanup;
      this.result = { taskId, status: "failed", cleanup, artifacts: [], message: error.message };
      if (cleanup === "confirmed") await this.finishConfirmed(current, client);
      else {
        if (cleanup !== "pending") {
          const closeError = await this.release(client);
          if (closeError) this.appendMessage(closeError);
        }
        if (current.status !== "ready") this.state = { ...current, status: "finished" };
      }
    }
    if (terminal !== "completed") for (const item of timeline.finish(terminal)) this.item(item);
    this.publishResult();
    this.context.emit({
      type: "session.turn",
      sessionId: this.context.id,
      turnId,
      state: terminal,
      error,
    });
  }

  async interrupt(): Promise<void> {
    const state = this.state;
    if (state.status === "ready") return;
    if (state.status === "active") {
      const task = await state.task;
      await task.cancel();
      await state.done;
    }
    if (this.state.status === "finished")
      throw new SandboxProviderError(
        "cleanup_unconfirmed",
        "Sandbox stop is not confirmed. Retry cleanup from the result card.",
      );
  }

  async retryCleanup(): Promise<CleanupStatus> {
    const state = this.state;
    if (state.status !== "finished")
      throw new SandboxProviderError(
        "not_finished",
        "Wait for the task result before retrying cleanup.",
      );
    const task = await state.task;
    state.cleanup = await task.retryCleanup();
    if (this.result) this.result = { ...this.result, cleanup: state.cleanup };
    this.publishResult();
    if (state.cleanup === "confirmed") await this.finishConfirmed(state, state.client);
    return state.cleanup;
  }

  matchesTask(id: string): boolean {
    return this.result?.taskId === id;
  }

  async close(): Promise<void> {
    let failure: unknown;
    if (this.state.status === "active") {
      try {
        await this.interrupt();
      } catch (error) {
        failure = error;
      }
    }
    const current = this.state;
    if (current.status !== "ready") {
      try {
        await current.client.close();
      } catch (error) {
        failure ??= error;
      }
    }
    if (failure) throw failure;
  }

  private async finishConfirmed(state: RunState, client: TaskClient): Promise<void> {
    const closeError = await this.release(client);
    if (closeError) this.appendMessage(closeError);
    if (state.status !== "ready") await this.discardCandidate(state.candidate);
    if (this.state === state) this.state = { status: "ready" };
  }

  private async discardCandidate(candidate: string): Promise<void> {
    try {
      await rm(candidate, { force: true });
    } catch {
      this.appendMessage("The temporary workspace checkpoint could not be removed on the host.");
    }
  }

  private async release(client: TaskClient): Promise<string | null> {
    try {
      await client.close();
      return null;
    } catch (failure) {
      return safeError(failure).message;
    }
  }

  private async commitCheckpoint(agentSession: string | undefined): Promise<string> {
    if (agentSession !== undefined) {
      const current = this.state;
      if (current.status === "ready") throw new Error("No active checkpoint");
      await rename(current.candidate, this.context.archive);
      this.agentSession = agentSession;
    } else {
      this.continuationDisabled = true;
    }
    this.publishPersistence();
    return this.continuationDisabled
      ? "Task complete. Sandbox cleanup confirmed. This agent has no resumable session; start a new sandbox session for another task."
      : "Task complete. Sandbox cleanup confirmed. The next prompt resumes this session's completed turn.";
  }

  private publishPersistence(): void {
    let data: { continuationDisabled?: true; agentSession?: string } = {};
    if (this.continuationDisabled) data = { continuationDisabled: true };
    else if (this.agentSession !== undefined) data = { agentSession: this.agentSession };
    this.context.emit({
      type: "session.persistence",
      sessionId: this.context.id,
      persistence: {
        version: 1,
        data,
      },
    });
  }

  private publishResult(): void {
    if (!this.result) return;
    this.item({
      type: "plugin",
      id: `${this.result.taskId}:result`,
      pluginId: "sandbox-provider",
      kind: "sandbox-result",
      version: 1,
      data: this.result,
    });
  }

  private appendMessage(message: string): void {
    if (this.result) this.result = { ...this.result, message: `${this.result.message} ${message}` };
  }

  private item(item: ProviderTimelineItem): void {
    this.context.emit({ type: "timeline.item", sessionId: this.context.id, item });
  }
}

export function safeError(error: unknown): { message: string; code: string } {
  if (error instanceof CircusBridgeError || error instanceof SandboxProviderError)
    return { message: error.message, code: error.code };
  if (error instanceof ArtifactError) return { message: error.message, code: "artifact_storage" };
  return {
    code: "provider",
    message: "Sandbox task failed. Check the host's sandbox configuration and try again.",
  };
}
