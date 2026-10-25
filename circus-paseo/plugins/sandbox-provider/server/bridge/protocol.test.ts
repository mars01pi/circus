import { expect, test } from "vitest";
import {
  DEFAULT_WORKSPACE_MAX_ENTRIES,
  DEFAULT_WORKSPACE_MAX_TOTAL_BYTES,
  MAX_WORKSPACE_ENTRIES,
  MAX_WORKSPACE_TOTAL_BYTES,
} from "../../shared/task.js";
import { ServerFrameSchema, TaskRequestSchema } from "./protocol.js";

const stateDir = process.platform === "win32" ? "C:\\circus\\state" : "/circus/state";
const request = {
  prompt: "Fix it",
  inputFiles: [],
  outputPaths: [],
  timeoutMs: 1000,
};

test("accepts bounded workspace and agent session fields", () => {
  expect(
    TaskRequestSchema.parse({
      ...request,
      workspaceArchive: `${stateDir}/session.snapshot`,
      workspaceMaxTotalBytes: DEFAULT_WORKSPACE_MAX_TOTAL_BYTES,
      workspaceMaxEntries: DEFAULT_WORKSPACE_MAX_ENTRIES,
      agentSession: "thread_A-1",
    }),
  ).toMatchObject({ agentSession: "thread_A-1" });
  expect(
    TaskRequestSchema.parse({
      ...request,
      workspaceArchive: `${stateDir}/session.snapshot`,
    }),
  ).toMatchObject({ workspaceArchive: `${stateDir}/session.snapshot` });
});

test("rejects invalid workspace paths, budgets and session ids", () => {
  for (const workspaceArchive of [
    "relative.snapshot",
    "",
    `${stateDir}/bad\0path`,
    `${stateDir}/line\nbreak`,
  ]) {
    expect(TaskRequestSchema.safeParse({ ...request, workspaceArchive }).success).toBe(false);
  }
  for (const workspaceMaxTotalBytes of [0, MAX_WORKSPACE_TOTAL_BYTES + 1]) {
    expect(
      TaskRequestSchema.safeParse({
        ...request,
        workspaceArchive: `${stateDir}/session.snapshot`,
        workspaceMaxTotalBytes,
      }).success,
    ).toBe(false);
  }
  for (const workspaceMaxEntries of [0, MAX_WORKSPACE_ENTRIES + 1]) {
    expect(
      TaskRequestSchema.safeParse({
        ...request,
        workspaceArchive: `${stateDir}/session.snapshot`,
        workspaceMaxEntries,
      }).success,
    ).toBe(false);
  }
  expect(TaskRequestSchema.safeParse({ ...request, workspaceMaxEntries: 16 }).success).toBe(false);
  for (const agentSession of ["", "with/slash", "with space", "é"]) {
    expect(TaskRequestSchema.safeParse({ ...request, agentSession }).success).toBe(false);
  }
});

test("parses optional agent session on successful results only", () => {
  expect(
    ServerFrameSchema.parse({
      v: 1,
      type: "result",
      requestId: "request-1",
      taskId: "task-1",
      result: {
        status: "succeeded",
        cleanup: "confirmed",
        artifacts: [],
        agentSession: "thread-1",
      },
    }),
  ).toMatchObject({ result: { status: "succeeded", agentSession: "thread-1" } });
  expect(
    ServerFrameSchema.safeParse({
      v: 1,
      type: "result",
      requestId: "request-1",
      taskId: "task-1",
      result: {
        status: "succeeded",
        cleanup: "confirmed",
        artifacts: [],
        agentSession: "../escape",
      },
    }).success,
  ).toBe(false);
});
