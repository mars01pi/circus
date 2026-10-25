import { expect, test } from "vitest";
import {
  DEFAULT_WORKSPACE_MAX_ENTRIES,
  DEFAULT_WORKSPACE_MAX_TOTAL_BYTES,
  MAX_WORKSPACE_ENTRIES,
  MAX_WORKSPACE_TOTAL_BYTES,
  taskOptionsSchema,
} from "./task.js";

test("accepts only explicit project-relative files and bounded task settings", () => {
  expect(
    taskOptionsSchema.parse({
      inputFiles: [{ path: "src/answer.py", text: "answer = 42\n" }],
      outputPaths: ["result.txt"],
      timeoutMs: 60_000,
    }),
  ).toEqual({
    inputFiles: [{ path: "src/answer.py", text: "answer = 42\n" }],
    outputPaths: ["result.txt"],
    timeoutMs: 60_000,
    mcpGrants: [],
    workspaceMaxTotalBytes: DEFAULT_WORKSPACE_MAX_TOTAL_BYTES,
    workspaceMaxEntries: DEFAULT_WORKSPACE_MAX_ENTRIES,
  });
  for (const workspaceMaxTotalBytes of [0, MAX_WORKSPACE_TOTAL_BYTES + 1]) {
    expect(taskOptionsSchema.safeParse({ workspaceMaxTotalBytes }).success).toBe(false);
  }
  for (const workspaceMaxEntries of [0, MAX_WORKSPACE_ENTRIES + 1]) {
    expect(taskOptionsSchema.safeParse({ workspaceMaxEntries }).success).toBe(false);
  }
  for (const path of [
    "/etc/passwd",
    "../secret",
    "a/../b",
    "a//b",
    "a\\b",
    "C:secret",
    "a\n.txt",
    ".",
    "",
  ]) {
    expect(taskOptionsSchema.safeParse({ outputPaths: [path] }).success).toBe(false);
  }
  expect(
    taskOptionsSchema.safeParse({
      inputFiles: [
        { path: "a", text: "x" },
        { path: "a", text: "y" },
      ],
    }).success,
  ).toBe(false);
  expect(taskOptionsSchema.safeParse({ outputPaths: ["a", "a"] }).success).toBe(false);
  expect(taskOptionsSchema.safeParse({ timeoutMs: 3_600_001 }).success).toBe(false);
  expect(taskOptionsSchema.safeParse({ cwd: "/home/user" }).success).toBe(false);
});

test("MCP grants select existing host identities without carrying authority", () => {
  const grant = { connectionId: "github", tools: ["issue_read"] };
  expect(taskOptionsSchema.parse({ mcpGrants: [grant] }).mcpGrants).toEqual([grant]);
  for (const mcpGrants of [
    [grant, grant],
    [{ ...grant, tools: [] }],
    [{ ...grant, tools: ["issue_read", "issue_read"] }],
    [{ ...grant, url: "https://attacker.example" }],
    [{ ...grant, headers: { Authorization: "secret" } }],
    [{ ...grant, fixedArguments: { repo: "other" } }],
  ])
    expect(taskOptionsSchema.safeParse({ mcpGrants }).success).toBe(false);
});
