import { mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { afterEach, expect, test, vi } from "vitest";
import type { ProviderEvent, ProviderSessionConfig } from "@getpaseo/plugin/server/provider";
import { SandboxSession, type TaskClient } from "./session.js";
import { ArtifactStore } from "./artifacts.js";
import type {
  CircusTaskEvent,
  CircusTaskResult,
  CleanupStatus,
  CircusTaskRequest,
} from "./bridge/stdio-client.js";
import { launchConfig } from "./provider.js";

const hostRoot = process.platform === "win32" ? "C:\\private" : "/private";
const tempDirs: string[] = [];

afterEach(async () => {
  await Promise.all(tempDirs.splice(0).map((dir) => rm(dir, { recursive: true, force: true })));
});

const config: ProviderSessionConfig = {
  cwd: "/private/host/project",
  env: { SECRET: "private" },
  systemPrompt: "host-only-system-prompt",
  mcpServers: { host: { type: "http", url: "http://localhost:6767" } },
  settings: {},
  persist: false,
  providerOptions: {
    inputFiles: [{ path: "answer.py", text: "print(42)" }],
    outputPaths: ["result.txt"],
    timeoutMs: 60_000,
  },
};

interface Turn {
  turnId: string;
  complete(result: CircusTaskResult): void;
  finished: Promise<void>;
  resolveFinished(): void;
  setCleanup(cleanup: CleanupStatus): void;
}

async function fixture(options: {
  events?: CircusTaskEvent[];
  sessionConfig?: ProviderSessionConfig;
  agentSession?: string;
  archive?: string;
  onClose?(count: number): Promise<void>;
}) {
  const dir = await mkdtemp(join(tmpdir(), "circus-session-"));
  tempDirs.push(dir);
  const archive = options.archive ?? join(dir, "session.snapshot");
  if (options.agentSession) await writeFile(archive, "restored archive");
  const observed: ProviderEvent[] = [];
  const requests: CircusTaskRequest[] = [];
  const restoredInputs: Array<string | undefined> = [];
  const startedTurnIds: string[] = [];
  const turns: Turn[] = [];
  const turnsById = new Map<string, Turn>();
  let cancelled = 0;
  let closed = 0;

  const createClient = (): TaskClient => ({
    async start(request) {
      restoredInputs.push(
        await readFile(request.workspaceArchive!, "utf8").catch((error) => {
          if ((error as NodeJS.ErrnoException).code === "ENOENT") return undefined;
          throw error;
        }),
      );
      await writeFile(request.workspaceArchive!, `candidate-${requests.length + 1}`);
      requests.push(request);
      const turnId = startedTurnIds[turns.length];
      if (!turnId) throw new Error("turn start was not announced");
      const result = Promise.withResolvers<CircusTaskResult>();
      const finished = Promise.withResolvers<void>();
      let cleanup: CleanupStatus = "confirmed";
      const turn: Turn = {
        turnId,
        complete: result.resolve,
        finished: finished.promise,
        resolveFinished: () => finished.resolve(),
        setCleanup(value) {
          cleanup = value;
        },
      };
      turns.push(turn);
      turnsById.set(turnId, turn);
      return {
        id: `task-${turns.length}`,
        result: result.promise,
        events: {
          async *[Symbol.asyncIterator]() {
            for (const event of options.events ?? []) yield event;
            await result.promise;
          },
        },
        async cancel() {
          cancelled += 1;
        },
        async retryCleanup() {
          return cleanup;
        },
      };
    },
    async close() {
      closed += 1;
      await options.onClose?.(closed);
    },
  });

  const session = new SandboxSession({
    id: "session-1",
    config: options.sessionConfig ?? config,
    createClient,
    archive,
    agentSession: options.agentSession,
    artifacts: new ArtifactStore(),
    emit(event) {
      observed.push(event);
      if (event.type !== "session.turn") return;
      if (event.state === "started") {
        startedTurnIds.push(event.turnId);
      } else {
        turnsById.get(event.turnId)?.resolveFinished();
      }
    },
  });
  async function prompt(text = "Fix the project") {
    const before = requests.length;
    const promptResult = observed.length;
    session.prompt({
      clientMessageId: `message-${observed.length}`,
      delivery: "auto",
      input: { type: "message", content: [{ type: "text", text }] },
    });
    await vi.waitFor(() => {
      const rejected = observed
        .slice(promptResult)
        .some((event) => event.type === "session.prompt_result" && event.result.type === "failed");
      const failedTurn = observed
        .slice(promptResult)
        .some((event) => event.type === "session.turn" && event.state === "failed");
      expect(requests.length > before || rejected || failedTurn).toBe(true);
    });
  }
  return {
    session,
    observed,
    requests,
    turns,
    prompt,
    archive,
    restoredInputs,
    cancelled: () => cancelled,
    closed: () => closed,
    persistence: () =>
      observed.flatMap((event) =>
        event.type === "session.persistence" ? [event.persistence] : [],
      ),
  };
}

test("runs sequential prompts with one fresh client, archive and returned agent session", async () => {
  const f = await fixture({});
  await f.prompt();
  expect(f.restoredInputs[0]).toBeUndefined();
  f.turns[0]!.complete({
    status: "succeeded",
    cleanup: "confirmed",
    artifacts: { "result.txt": Buffer.from("42\n") },
    agentSession: "thread-1",
  });
  await f.turns[0]!.finished;
  expect(await readFile(f.archive, "utf8")).toBe("candidate-1");
  expect(f.closed()).toBe(1);
  expect(
    f.observed.filter((event) => event.type === "session.turn").map((event) => event.state),
  ).toEqual(["started", "completed"]);

  await f.prompt("Continue");
  expect(f.restoredInputs[1]).toBe("candidate-1");
  expect(f.requests[1]?.agentSession).toBe("thread-1");
  f.turns[1]!.complete({
    status: "succeeded",
    cleanup: "confirmed",
    artifacts: {},
    agentSession: "thread-2",
  });
  await f.turns[1]!.finished;
  expect(await readFile(f.archive, "utf8")).toBe("candidate-2");
  expect(f.closed()).toBe(2);
  expect(f.turns).toHaveLength(2);
  expect(f.persistence()).toEqual([
    { version: 1, data: { agentSession: "thread-1" } },
    { version: 1, data: { agentSession: "thread-2" } },
  ]);
});

test("restores a persisted agent session and exposes it on the first request", async () => {
  const f = await fixture({ agentSession: "thread-restored" });
  await f.prompt();
  expect(f.requests[0]?.agentSession).toBe("thread-restored");
  f.turns[0]!.complete({
    status: "succeeded",
    cleanup: "confirmed",
    artifacts: {},
    agentSession: "thread-restored",
  });
  await f.turns[0]!.finished;
  expect(f.requests).toHaveLength(1);
  expect(f.persistence()).toEqual([{ version: 1, data: { agentSession: "thread-restored" } }]);
});

test("provider options override archive budgets", async () => {
  const f = await fixture({
    sessionConfig: {
      ...config,
      providerOptions: {
        ...config.providerOptions,
        workspaceMaxTotalBytes: 4096,
        workspaceMaxEntries: 32,
      },
    },
  });
  await f.prompt();
  expect(f.requests[0]).toMatchObject({
    workspaceMaxTotalBytes: 4096,
    workspaceMaxEntries: 32,
  });
  f.turns[0]!.complete({ status: "succeeded", cleanup: "confirmed", artifacts: {} });
  await f.turns[0]!.finished;
});

test("Stop cancels the active turn, waits for cleanup, then accepts the next prompt", async () => {
  const f = await fixture({});
  await f.prompt();
  const stopped = f.session.interrupt();
  f.turns[0]!.complete({
    status: "cancelled",
    cleanup: "confirmed",
    error: { code: "cancelled", message: "Agent task cancelled" },
  });
  await stopped;
  expect(f.cancelled()).toBe(1);
  expect(f.closed()).toBe(1);
  expect(
    f.observed.findLast((event) => event.type === "timeline.item" && event.item.type === "plugin"),
  ).toMatchObject({
    item: {
      data: {
        status: "cancelled",
        cleanup: "confirmed",
        message: "Task stopped. The next prompt continues from the previous completed turn.",
      },
    },
  });
  await f.prompt("Try again");
  expect(f.requests).toHaveLength(2);
  expect(f.requests[1]?.agentSession).toBeUndefined();
});

test.each(["failed", "cancelled"] as const)(
  "%s turns cannot replace the completed checkpoint",
  async (status) => {
    const f = await fixture({ agentSession: "thread-completed" });
    await f.prompt();
    f.turns[0]!.complete({
      status,
      cleanup: "confirmed",
      error: { code: status === "cancelled" ? "cancelled" : "agent_failed", message: status },
    });
    await f.turns[0]!.finished;
    expect(await readFile(f.archive, "utf8")).toBe("restored archive");
    expect(f.persistence()).toEqual([]);
    await f.prompt("Continue the completed turn");
    expect(f.restoredInputs[1]).toBe("restored archive");
    expect(f.requests[1]?.agentSession).toBe("thread-completed");
    f.turns[1]!.complete({
      status: "succeeded",
      cleanup: "confirmed",
      artifacts: {},
      agentSession: "thread-next",
    });
    await f.turns[1]!.finished;
    expect(await readFile(f.archive, "utf8")).toBe("candidate-2");
  },
);

test("an agent without a resumable thread remains one-shot", async () => {
  const f = await fixture({});
  await f.prompt();
  f.turns[0]!.complete({ status: "succeeded", cleanup: "confirmed", artifacts: {} });
  await f.turns[0]!.finished;
  expect(f.persistence()).toEqual([{ version: 1, data: { continuationDisabled: true } }]);
  await expect(readFile(f.archive)).rejects.toMatchObject({ code: "ENOENT" });
  await f.prompt("Must not silently start a fresh conversation");
  expect(f.requests).toHaveLength(1);
  expect(f.observed.at(-1)).toMatchObject({
    type: "session.prompt_result",
    result: { type: "failed" },
  });
  await f.session.close();
  expect(f.closed()).toBe(1);
});

test("pending cleanup keeps the result and blocks prompts until retry confirms", async () => {
  const f = await fixture({});
  await f.prompt();
  f.turns[0]!.complete({
    status: "cancelled",
    cleanup: "pending",
    error: { code: "cleanup", message: "Cleanup requires retry" },
  });
  await f.turns[0]!.finished;
  expect(f.closed()).toBe(0);
  await f.prompt("Should not start");
  expect(f.requests).toHaveLength(1);
  expect(f.observed.at(-1)).toMatchObject({
    type: "session.prompt_result",
    result: { type: "failed", error: { code: "session_busy" } },
  });
  await expect(f.session.interrupt()).rejects.toThrow("Sandbox stop is not confirmed");
  f.turns[0]!.setCleanup("confirmed");
  expect(await f.session.retryCleanup()).toBe("confirmed");
  expect(f.closed()).toBe(1);
  await f.prompt("After cleanup");
  expect(f.requests).toHaveLength(2);
});

test("a stale concurrent cleanup retry cannot erase the next active turn", async () => {
  const releaseStale = Promise.withResolvers<void>();
  const f = await fixture({
    onClose: async (count) => {
      if (count === 2) await releaseStale.promise;
    },
  });
  await f.prompt();
  f.turns[0]!.complete({
    status: "cancelled",
    cleanup: "pending",
    error: { code: "cleanup", message: "requires retry" },
  });
  await f.turns[0]!.finished;
  f.turns[0]!.setCleanup("confirmed");
  const first = f.session.retryCleanup();
  const stale = f.session.retryCleanup();
  await first;
  await f.prompt("Next turn");
  releaseStale.resolve();
  await stale;
  await f.prompt("Must remain blocked while next turn runs");
  expect(f.requests).toHaveLength(2);
  expect(f.observed.at(-1)).toMatchObject({
    type: "session.prompt_result",
    result: { type: "failed" },
  });
  f.turns[1]!.complete({
    status: "succeeded",
    cleanup: "confirmed",
    artifacts: {},
    agentSession: "next-thread",
  });
  await f.turns[1]!.finished;
  expect(await readFile(f.archive, "utf8")).toBe("candidate-2");
});

test("unknown cleanup blocks prompts as well", async () => {
  const f = await fixture({});
  await f.prompt();
  f.turns[0]!.complete({
    status: "failed",
    cleanup: "unknown",
    error: { code: "cleanup", message: "Cleanup state is unknown" },
  });
  await f.turns[0]!.finished;
  expect(f.closed()).toBe(1);
  await f.prompt("Blocked");
  expect(f.requests).toHaveLength(1);
  expect(f.observed.at(-1)).toMatchObject({
    type: "session.prompt_result",
    result: { type: "failed", error: { code: "session_busy" } },
  });
});

test("keeps model errors visible and rejects host attachments", async () => {
  const f = await fixture({});
  f.session.prompt({
    clientMessageId: "upload",
    delivery: "auto",
    input: {
      type: "message",
      content: [
        {
          type: "uploaded_file",
          id: "f",
          path: "/private/secret",
          fileName: "secret",
          size: 2,
          mimeType: "text/plain",
        },
      ],
    },
  });
  expect(f.requests).toHaveLength(0);
  expect(f.observed.at(-1)).toMatchObject({
    type: "session.prompt_result",
    result: { type: "failed" },
  });
  await f.prompt();
  f.turns[0]!.complete({
    status: "failed",
    cleanup: "confirmed",
    error: { code: "model_rejected", message: "Model request rejected" },
  });
  await f.turns[0]!.finished;
  expect(
    f.observed.findLast((event) => event.type === "timeline.item" && event.item.type === "plugin"),
  ).toMatchObject({
    item: {
      data: {
        status: "failed",
        cleanup: "confirmed",
        message: "Model request rejected (model_rejected)",
      },
    },
  });
  expect(f.observed.at(-1)).toMatchObject({
    type: "session.turn",
    state: "failed",
    error: { code: "model_rejected", message: "Model request rejected" },
  });
});

test("rejects a relative archive path", () => {
  expect(
    () =>
      new SandboxSession({
        id: "session-1",
        config,
        createClient: () => {
          throw new Error("unused");
        },
        archive: "relative.snapshot",
        artifacts: new ArtifactStore(),
        emit: () => undefined,
      }),
  ).toThrow("absolute host path");
});

test("bridge launch requires the state directory and strips host credentials", () => {
  expect(
    launchConfig({
      command: "/opt/circus-stdio-bridge",
      args: [],
      env: {
        CIRCUS_RUNNER_CONFIG: `${hostRoot}/runner.json`,
        CIRCUS_SANDBOX_STATE_DIR: `${hostRoot}/state`,
        HOME: "/private/home",
        OPENAI_API_KEY: "secret",
        HTTPS_PROXY: "http://proxy",
        PATH: "/bin",
      },
    }),
  ).toEqual({
    executable: "/opt/circus-stdio-bridge",
    configFile: `${hostRoot}/runner.json`,
    stateDir: `${hostRoot}/state`,
    env: { PATH: "/bin" },
  });
  expect(() =>
    launchConfig({
      command: "/opt/circus-stdio-bridge",
      args: [],
      env: { CIRCUS_RUNNER_CONFIG: `${hostRoot}/runner.json` },
    }),
  ).toThrow("CIRCUS_SANDBOX_STATE_DIR");
});
