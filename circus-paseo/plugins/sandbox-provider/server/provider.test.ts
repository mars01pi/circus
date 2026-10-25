import { mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { expect, test, vi } from "vitest";
import type {
  ProviderConnection,
  ProviderEvent,
  ProviderInput,
  ProviderPersistence,
  ProviderSessionConfig,
} from "@getpaseo/plugin/server/provider";
import { ArtifactStore } from "./artifacts.js";
import { SandboxProvider, launchConfig, workspaceArchive } from "./provider.js";
import type { TaskClient } from "./session.js";
import type { CircusTaskRequest, CircusTaskResult } from "./bridge/stdio-client.js";
import { SANDBOX_HOST_MODEL_ID } from "../shared/task.js";

const hostRoot = process.platform === "win32" ? "C:\\circus" : "/circus";

function sessionConfig(): ProviderSessionConfig {
  return {
    cwd: hostRoot,
    env: {},
    mcpServers: {},
    settings: {},
    persist: true,
    model: SANDBOX_HOST_MODEL_ID,
  };
}

function harness() {
  const requests: CircusTaskRequest[] = [];
  let cancelled = 0;
  const turns: Array<{
    complete(result: CircusTaskResult): void;
    closed: Promise<void>;
  }> = [];
  const createClient = (): TaskClient => {
    const finished = Promise.withResolvers<void>();
    const result = Promise.withResolvers<CircusTaskResult>();
    return {
      async start(request) {
        await writeFile(request.workspaceArchive!, `candidate-${requests.length + 1}`);
        requests.push(request);
        turns.push({ complete: result.resolve, closed: finished.promise });
        return {
          id: `task-${turns.length}`,
          result: result.promise,
          events: {
            async *[Symbol.asyncIterator]() {
              await result.promise;
              for (const _ of [] as never[]) yield _;
            },
          },
          async cancel() {
            cancelled += 1;
          },
          async retryCleanup() {
            return "confirmed" as const;
          },
        };
      },
      async close() {
        finished.resolve();
      },
    };
  };
  const events: ProviderEvent[] = [];
  const terminalTurns: Promise<void>[] = [];
  const terminalById = new Map<string, () => void>();
  return {
    provider: new SandboxProvider(new ArtifactStore(), createClient, async () => undefined),
    requests,
    turns,
    events,
    cancelled: () => cancelled,
    observe(connection: ProviderConnection) {
      connection.onEvent((event) => {
        events.push(event);
        if (event.type !== "session.turn") return;
        if (event.state === "started") {
          const terminal = Promise.withResolvers<void>();
          terminalTurns.push(terminal.promise);
          terminalById.set(event.turnId, () => terminal.resolve());
        } else {
          terminalById.get(event.turnId)?.();
        }
      });
    },
    async waitStart(count: number) {
      await vi.waitFor(() => expect(requests).toHaveLength(count));
    },
    waitTurn(index: number) {
      return terminalTurns[index - 1]!;
    },
  };
}

function launch(stateDir: string) {
  return {
    command: resolve("bridge"),
    args: [],
    env: {
      CIRCUS_RUNNER_CONFIG: resolve("runner.json"),
      CIRCUS_SANDBOX_STATE_DIR: stateDir,
      PATH: hostRoot,
    },
  };
}

const capabilities = ["prompt.message", "timeline.plugin", "session.persistence"];

async function open(
  connection: ProviderConnection,
  sessionId: string,
  requestId: string,
  persistence?: ProviderPersistence,
) {
  await connection.send({
    type: "session.open",
    requestId,
    sessionId,
    config: sessionConfig(),
    history: "skip",
    ...(persistence === undefined ? {} : { persistence }),
  } as ProviderInput);
}

function prompt(connection: ProviderConnection, sessionId: string, clientMessageId: string) {
  return connection.send({
    type: "session.prompt",
    sessionId,
    prompt: {
      clientMessageId,
      delivery: "auto",
      input: { type: "message", content: [{ type: "text", text: "Fix it" }] },
    },
  });
}

test("advertises a sequential sandbox mode and negotiates persistence", async () => {
  const stateDir = await mkdtemp(join(tmpdir(), "circus-provider-"));
  try {
    const f = harness();
    const connection = await f.provider.registration().connect({
      launch: launch(stateDir),
      versions: [1],
      capabilities,
    });
    f.observe(connection);
    expect(connection.capabilities).toContain("session.persistence");
    await connection.send({ type: "catalog", requestId: "catalog-1" });
    expect(f.events.find((event) => event.type === "catalog")).toMatchObject({
      catalog: {
        defaultMode: "sandbox",
        defaultModel: SANDBOX_HOST_MODEL_ID,
        modes: [
          {
            id: "sandbox",
            label: "Sequential sandbox",
            isUnattended: true,
          },
        ],
      },
    });
    await open(connection, "session-a", "open-1");
    expect(f.events.find((event) => event.type === "session.opened")).toMatchObject({
      sessionId: "session-a",
      restoration: "core",
      persistence: { version: 1, data: {} },
    });
    expect(f.events.find((event) => event.type === "session.persistence")).toMatchObject({
      sessionId: "session-a",
      persistence: { version: 1, data: {} },
    });
    await connection.close();
  } finally {
    await rm(stateDir, { recursive: true, force: true });
  }
});

test("sequential prompts reuse the derived archive and continue the returned thread", async () => {
  const stateDir = await mkdtemp(join(tmpdir(), "circus-provider-"));
  try {
    const f = harness();
    const connection = await f.provider.registration().connect({
      launch: launch(stateDir),
      versions: [1],
      capabilities,
    });
    f.observe(connection);
    await open(connection, "session-a", "open-1");
    await prompt(connection, "session-a", "message-1");
    await f.waitStart(1);
    const archive = workspaceArchive(stateDir, "session-a");
    expect(dirname(archive)).toBe(stateDir);
    expect(f.requests[0]?.workspaceArchive).not.toBe(archive);
    expect(f.requests[0]?.agentSession).toBeUndefined();
    f.turns[0]!.complete({
      status: "succeeded",
      cleanup: "confirmed",
      artifacts: {},
      agentSession: "thread-a",
    });
    await f.waitTurn(1);
    await f.turns[0]!.closed;
    expect(f.events.findLast((event) => event.type === "session.persistence")).toMatchObject({
      persistence: { version: 1, data: { agentSession: "thread-a" } },
    });

    await prompt(connection, "session-a", "message-2");
    await f.waitStart(2);
    expect(f.requests[1]).toMatchObject({
      agentSession: "thread-a",
    });
    f.turns[1]!.complete({
      status: "succeeded",
      cleanup: "confirmed",
      artifacts: {},
      agentSession: "thread-b",
    });
    await f.waitTurn(2);
    await f.turns[1]!.closed;
    expect(f.events.findLast((event) => event.type === "session.persistence")).toMatchObject({
      persistence: { version: 1, data: { agentSession: "thread-b" } },
    });
    await connection.close();
  } finally {
    await rm(stateDir, { recursive: true, force: true });
  }
});

test("restores persistence and rejects unsupported or malformed handles", async () => {
  const stateDir = await mkdtemp(join(tmpdir(), "circus-provider-"));
  try {
    const f = harness();
    const connection = await f.provider.registration().connect({
      launch: launch(stateDir),
      versions: [1],
      capabilities,
    });
    f.observe(connection);
    await open(connection, "bad-version", "open-1", { version: 2, data: {} });
    expect(f.events.findLast((event) => event.type === "request.failed")).toMatchObject({
      error: { code: "unsupported" },
    });
    await open(connection, "bad-data", "open-2", {
      version: 1,
      data: { agentSession: "../escape" },
    });
    expect(f.events.findLast((event) => event.type === "request.failed")).toMatchObject({
      error: { code: "invalid_persistence" },
    });

    await writeFile(workspaceArchive(stateDir, "../../escape"), Buffer.alloc(0));
    await open(connection, "../../escape", "open-3", {
      version: 1,
      data: { agentSession: "thread-restored" },
    });
    expect(f.events.find((event) => event.type === "session.persistence")).toMatchObject({
      persistence: { version: 1, data: { agentSession: "thread-restored" } },
    });
    await prompt(connection, "../../escape", "message-1");
    await f.waitStart(1);
    const archive = workspaceArchive(stateDir, "../../escape");
    expect(dirname(archive)).toBe(stateDir);
    expect(f.requests[0]).toMatchObject({
      agentSession: "thread-restored",
    });
    f.turns[0]!.complete({
      status: "succeeded",
      cleanup: "confirmed",
      artifacts: {},
    });
    await f.waitTurn(1);
    await connection.close();
  } finally {
    await rm(stateDir, { recursive: true, force: true });
  }
});

test("pending cleanup blocks the next prompt until retry", async () => {
  const stateDir = await mkdtemp(join(tmpdir(), "circus-provider-"));
  try {
    const f = harness();
    const connection = await f.provider.registration().connect({
      launch: launch(stateDir),
      versions: [1],
      capabilities,
    });
    f.observe(connection);
    await open(connection, "session-a", "open-1");
    await prompt(connection, "session-a", "message-1");
    await f.waitStart(1);
    f.turns[0]!.complete({
      status: "cancelled",
      cleanup: "pending",
      error: { code: "cleanup", message: "Cleanup requires retry" },
    });
    await f.waitTurn(1);
    await prompt(connection, "session-a", "message-2");
    expect(f.requests).toHaveLength(1);
    expect(f.events.at(-1)).toMatchObject({
      type: "session.prompt_result",
      result: { type: "failed", error: { code: "session_busy" } },
    });
    expect(await f.provider.retryCleanup("task-1")).toEqual({ cleanup: "confirmed" });
    await prompt(connection, "session-a", "message-3");
    await f.waitStart(2);
    expect(f.requests).toHaveLength(2);
    f.turns[1]!.complete({ status: "succeeded", cleanup: "confirmed", artifacts: {} });
    await f.waitTurn(2);
    await connection.close();
  } finally {
    await rm(stateDir, { recursive: true, force: true });
  }
});

test("a persisted non-resumable agent cannot silently reopen a fresh conversation", async () => {
  const stateDir = await mkdtemp(join(tmpdir(), "circus-provider-"));
  try {
    const f = harness();
    const connection = await f.provider.registration().connect({
      launch: launch(stateDir),
      versions: [1],
      capabilities,
    });
    f.observe(connection);
    await open(connection, "one-shot", "open-1", {
      version: 1,
      data: { continuationDisabled: true },
    });
    expect(f.events.findLast((event) => event.type === "request.failed")).toMatchObject({
      error: { code: "unsupported" },
    });
    expect(f.events.some((event) => event.type === "session.opened")).toBe(false);
    await connection.close();
  } finally {
    await rm(stateDir, { recursive: true, force: true });
  }
});

test("requires an absolute state directory", () => {
  expect(() =>
    launchConfig({
      command: resolve("bridge"),
      args: [],
      env: {
        CIRCUS_RUNNER_CONFIG: resolve("runner.json"),
        CIRCUS_SANDBOX_STATE_DIR: "relative/state",
      },
    }),
  ).toThrow("CIRCUS_SANDBOX_STATE_DIR");
});
