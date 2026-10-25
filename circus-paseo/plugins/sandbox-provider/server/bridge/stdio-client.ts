import { randomUUID } from "node:crypto";
import { isAbsolute } from "node:path";
import type { ChildProcessWithoutNullStreams } from "node:child_process";
import { spawnProcess, terminateProcess } from "@getpaseo/plugin/server";
import { z } from "zod";
import type { McpGrant } from "../../shared/mcp.js";
import { EventStream } from "./event-stream.js";
import { CircusBridgeError, Deferred, FrameDecoder } from "./framing.js";
import {
  MAX_EVENT_BYTES,
  MAX_FILE_BYTES,
  MAX_FRAME_BYTES,
  ServerFrameSchema,
  TaskRequestSchema,
  isAbsoluteHostPath,
  type CircusTaskEvent,
  type CleanupStatus,
  type ClientFrame,
  type ServerFrame,
  type WireResult,
} from "./protocol.js";

export { CircusBridgeError } from "./framing.js";
export type { CircusTaskEvent, CleanupStatus } from "./protocol.js";

export interface CircusClientConfig {
  executable: string;
  configFile: string;
  env: Record<string, string>;
  /** Host state directory for workspace archives; never sent to the bridge. */
  stateDir: string;
  shutdownTimeoutMs?: number;
}
export interface CircusTaskRequest {
  prompt: string;
  inputFiles: Record<string, Uint8Array>;
  outputPaths: string[];
  timeoutMs: number;
  mcpGrants?: McpGrant[];
  workspaceArchive?: string;
  workspaceMaxTotalBytes?: number;
  workspaceMaxEntries?: number;
  agentSession?: string;
}
export type CircusTaskResult =
  | Exclude<WireResult, { status: "succeeded" }>
  | {
      status: "succeeded";
      cleanup: "confirmed";
      artifacts: Record<string, Uint8Array>;
      agentSession?: string;
    };
export interface CircusTask {
  readonly id: string;
  readonly events: AsyncIterable<CircusTaskEvent>;
  readonly result: Promise<CircusTaskResult>;
  cancel(): Promise<void>;
  retryCleanup(): Promise<CleanupStatus>;
}

const configSchema = z.strictObject({
  executable: z
    .string()
    .refine(isAbsolute)
    .refine((path) => !/\.(cmd|bat)$/i.test(path)),
  configFile: z.string().refine(isAbsolute),
  env: z.record(z.string(), z.string()),
  stateDir: z.string().refine(isAbsoluteHostPath),
  shutdownTimeoutMs: z.number().int().min(1).max(300_000).default(120_000),
});

export class CircusStdioClient {
  private readonly config: z.infer<typeof configSchema>;
  private child: ChildProcessWithoutNullStreams | null = null;
  private readonly ready = new Deferred<void>();
  private readonly accepted = new Deferred<CircusTask>();
  private readonly outcome = new Deferred<CircusTaskResult>();
  private readonly exited = new Deferred<void>();
  private readonly events = new EventStream();
  private readonly decoder = new FrameDecoder();
  private readonly requestId = randomUUID();
  private taskId: string | null = null;
  private state: "idle" | "starting" | "running" | "finished" | "closed" = "idle";
  private receivedReady = false;
  private seq = 0;
  private outputPaths: string[] = [];
  private failure: CircusBridgeError | null = null;
  private cleanup: CleanupStatus = "unknown";
  private cancellation: Deferred<void> | null = null;
  private cleanupRetry: Deferred<CleanupStatus> | null = null;
  private closing: Promise<void> | null = null;
  private deadline: ReturnType<typeof setTimeout> | null = null;

  constructor(config: CircusClientConfig) {
    const parsed = configSchema.safeParse(config);
    if (!parsed.success)
      throw new CircusBridgeError(
        "configuration",
        "Explicit absolute binary/config paths and environment are required",
      );
    this.config = parsed.data;
  }

  async start(request: CircusTaskRequest): Promise<CircusTask> {
    if (this.state !== "idle")
      throw new CircusBridgeError("closed", "Each bridge client accepts one task");
    const entries = Object.entries(request.inputFiles);
    const inputBytes = entries.reduce((sum, [, bytes]) => sum + bytes.byteLength, 0);
    if (entries.length > 64 || inputBytes > MAX_FILE_BYTES)
      throw new CircusBridgeError("invalid_request", "Task input exceeds byte or file limit");
    const inputFiles = entries.map(([path, bytes]) => ({
      path,
      data: Buffer.from(bytes).toString("base64"),
    }));
    const wireRequest = { ...request, inputFiles };
    // COMPAT(sandboxMcpGrants): added in v0.13.0-beta.2, remove after 2027-04-25
    // once the minimum supported bridge accepts the optional v1 mcpGrants field.
    if (wireRequest.mcpGrants?.length === 0) delete wireRequest.mcpGrants;
    const parsed = TaskRequestSchema.safeParse(wireRequest);
    if (!parsed.success)
      throw new CircusBridgeError("invalid_request", "Task request violates path or byte limits");
    const startFrame: ClientFrame = {
      v: 1,
      type: "start",
      requestId: this.requestId,
      request: parsed.data,
    };
    if (Buffer.byteLength(JSON.stringify(startFrame)) > MAX_FRAME_BYTES)
      throw new CircusBridgeError("invalid_request", "Encoded task exceeds frame limit");
    this.state = "starting";
    this.outputPaths = parsed.data.outputPaths;
    this.child = spawnProcess(this.config.executable, ["--config", this.config.configFile], {
      env: this.config.env,
      stdio: "pipe",
      shell: false,
      windowsHide: true,
    });
    this.child.on("error", () =>
      this.fail(new CircusBridgeError("transport", "Cannot start Rust bridge")),
    );
    this.child.stdin.on("error", () =>
      this.fail(new CircusBridgeError("transport", "Bridge input stream failed")),
    );
    this.child.stdout.on("error", () =>
      this.fail(new CircusBridgeError("transport", "Bridge output stream failed")),
    );
    this.child.stdout.on("data", (chunk: Buffer) => this.receive(chunk));
    this.child.stdout.on("end", () => {
      if (this.state === "running" || this.state === "starting")
        this.fail(
          new CircusBridgeError("transport", "Bridge output ended before the terminal result"),
        );
    });
    // Diagnostics are drained without retaining or forwarding potentially sensitive text.
    this.child.stderr.resume();
    this.child.on("close", (code) => this.onExit(code));
    this.deadline = setTimeout(
      () => this.fail(new CircusBridgeError("timeout", "Bridge readiness deadline expired")),
      10_000,
    );
    await this.ready.promise;
    await this.send(startFrame);
    this.clearDeadline();
    this.deadline = setTimeout(
      () => this.fail(new CircusBridgeError("timeout", "Bridge task deadline expired")),
      request.timeoutMs + this.config.shutdownTimeoutMs,
    );
    return this.accepted.promise;
  }

  close(): Promise<void> {
    if (this.closing) return this.closing;
    this.closing = this.shutdown();
    return this.closing;
  }

  private async shutdown(): Promise<void> {
    this.clearDeadline();
    if (!this.child) {
      this.state = "closed";
      return;
    }
    this.child.stdin.end();
    const timer = setTimeout(() => {
      const error = new CircusBridgeError(
        "timeout",
        "Bridge shutdown timed out; cleanup is unconfirmed",
      );
      this.fail(error);
      if (this.child) void terminateProcess(this.child).catch(() => {});
      this.exited.reject(error);
    }, this.config.shutdownTimeoutMs);
    try {
      await this.exited.promise;
      if (this.failure) throw this.failure;
      if (this.cleanup !== "confirmed" && this.taskId)
        throw new CircusBridgeError("transport", "Bridge closed with unconfirmed cleanup");
    } finally {
      clearTimeout(timer);
      this.state = "closed";
    }
  }

  private receive(chunk: Buffer): void {
    if (this.failure) return;
    try {
      this.decoder.push(chunk, (value, bytes) => {
        const parsed = ServerFrameSchema.safeParse(value);
        if (!parsed.success)
          throw new CircusBridgeError("protocol", "Invalid bridge response schema");
        this.acceptFrame(parsed.data, bytes);
      });
    } catch (error) {
      if (!(error instanceof CircusBridgeError)) throw error;
      this.fail(error);
    }
  }

  private acceptFrame(frame: ServerFrame, bytes: number): void {
    if (frame.type === "error")
      throw new CircusBridgeError("protocol", `Bridge rejected protocol: ${frame.code}`);
    if (frame.type === "ready") {
      if (this.receivedReady) throw new CircusBridgeError("protocol", "Duplicate readiness frame");
      this.receivedReady = true;
      this.ready.resolve();
      return;
    }
    if (!this.receivedReady || frame.requestId !== this.requestId)
      throw new CircusBridgeError("protocol", "Response belongs to another request");
    if (frame.type === "accepted") {
      if (this.state !== "starting")
        throw new CircusBridgeError("protocol", "Unexpected task acceptance");
      this.taskId = frame.taskId;
      this.state = "running";
      this.accepted.resolve({
        id: frame.taskId,
        events: this.events,
        result: this.outcome.promise,
        cancel: () => this.cancel(),
        retryCleanup: () => this.retryCleanup(),
      });
      return;
    }
    if (frame.taskId !== this.taskId)
      throw new CircusBridgeError("protocol", "Response belongs to another task");
    switch (frame.type) {
      case "event":
        if (this.state !== "running" || frame.seq !== this.seq + 1 || bytes > MAX_EVENT_BYTES)
          throw new CircusBridgeError("protocol", "Invalid task event ordering or size");
        this.seq = frame.seq;
        this.events.push({ event: frame.event, bytes });
        break;
      case "result":
        this.acceptResult(frame.result);
        break;
      case "cancel_ack":
        if (!this.cancellation)
          throw new CircusBridgeError("protocol", "Unexpected cancellation acknowledgement");
        this.cancellation.resolve();
        break;
      case "cleanup_result":
        if (!this.cleanupRetry)
          throw new CircusBridgeError("protocol", "Unexpected cleanup result");
        this.cleanup = frame.cleanup;
        this.cleanupRetry.resolve(frame.cleanup);
        this.cleanupRetry = null;
        break;
    }
  }

  private acceptResult(result: WireResult): void {
    if (this.state !== "running")
      throw new CircusBridgeError("protocol", "Unexpected final result");
    let decoded: CircusTaskResult;
    if (result.status === "succeeded") {
      const returnedPaths = new Set(result.artifacts.map((file) => file.path));
      const missingPath = this.outputPaths.some((path) => !returnedPaths.has(path));
      if (returnedPaths.size !== this.outputPaths.length || missingPath) {
        throw new CircusBridgeError("protocol", "Bridge returned an unexpected artifact set");
      }
      const artifacts = Object.fromEntries(
        result.artifacts.map((file) => [file.path, Buffer.from(file.data, "base64")]),
      );
      decoded = { ...result, artifacts };
    } else decoded = result;
    this.state = "finished";
    this.clearDeadline();
    this.cleanup = result.cleanup;
    this.events.end();
    this.outcome.resolve(decoded);
  }

  private async cancel(): Promise<void> {
    if (this.failure) throw this.failure;
    if (this.state === "finished") return;
    if (!this.taskId || this.state !== "running")
      throw new CircusBridgeError("closed", "Task is not running");
    if (this.cancellation) return this.cancellation.promise;
    this.cancellation = new Deferred<void>();
    await this.send({ v: 1, type: "cancel", requestId: this.requestId, taskId: this.taskId });
    return this.controlDeadline(this.cancellation.promise);
  }

  private async retryCleanup(): Promise<CleanupStatus> {
    if (this.failure) throw this.failure;
    if (!this.taskId || this.state !== "finished")
      throw new CircusBridgeError("closed", "Cleanup retry requires a completed task");
    if (this.cleanup !== "pending") return this.cleanup;
    if (this.cleanupRetry) return this.cleanupRetry.promise;
    this.cleanupRetry = new Deferred<CleanupStatus>();
    const pending = this.cleanupRetry;
    await this.send({
      v: 1,
      type: "cleanup_retry",
      requestId: this.requestId,
      taskId: this.taskId,
    });
    return this.controlDeadline(pending.promise);
  }

  private async controlDeadline<T>(operation: Promise<T>): Promise<T> {
    const timer = setTimeout(
      () => this.fail(new CircusBridgeError("timeout", "Bridge control operation timed out")),
      this.config.shutdownTimeoutMs,
    );
    try {
      return await operation;
    } finally {
      clearTimeout(timer);
    }
  }

  private send(frame: ClientFrame): Promise<void> {
    const child = this.child;
    if (!child || child.stdin.destroyed)
      return Promise.reject(new CircusBridgeError("closed", "Bridge input is closed"));
    return new Promise((resolve, reject) => {
      child.stdin.write(`${JSON.stringify(frame)}\n`, (error) => {
        if (error) reject(new CircusBridgeError("transport", "Bridge write failed"));
        else resolve();
      });
    });
  }

  private onExit(code: number | null): void {
    try {
      this.decoder.end();
    } catch (error) {
      if (!(error instanceof CircusBridgeError)) throw error;
      this.fail(error);
    }
    if (code !== 0 || this.state === "running" || this.state === "starting")
      this.fail(
        new CircusBridgeError("transport", "Bridge exited without a clean terminal result"),
      );
    this.clearDeadline();
    this.exited.resolve();
  }

  private fail(error: CircusBridgeError): void {
    if (this.failure) return;
    this.failure = error;
    this.clearDeadline();
    this.ready.reject(error);
    this.accepted.reject(error);
    this.outcome.reject(error);
    this.events.end(error);
    this.cancellation?.reject(error);
    this.cleanupRetry?.reject(error);
    // EOF lets the Rust owner cancel and await cleanup even after a bad response.
    void this.closeAfterFailure();
  }

  private async closeAfterFailure(): Promise<void> {
    try {
      await this.close();
    } catch (error) {
      // Public result/close promises retain the failure; this only owns teardown.
      if (!(error instanceof CircusBridgeError)) throw error;
    }
  }

  private clearDeadline(): void {
    if (this.deadline) clearTimeout(this.deadline);
    this.deadline = null;
  }
}
