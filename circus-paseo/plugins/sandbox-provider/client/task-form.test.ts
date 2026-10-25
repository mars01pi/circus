import { expect, test } from "vitest";
import { openTaskForm } from "./task-form.js";
import {
  DEFAULT_WORKSPACE_MAX_ENTRIES,
  DEFAULT_WORKSPACE_MAX_TOTAL_BYTES,
} from "../shared/task.js";

test("MCP access requires explicit tool selection and clearing removes every grant", async () => {
  const submitted: unknown[] = [];
  const form = openTaskForm({
    create: async (input) => {
      submitted.push(input.options);
      return "task";
    },
  });
  expect(form.getState().mcpGrants).toEqual([]);
  form.set("prompt", "Read the approved issue");
  form.setMcpGrants([{ connectionId: "github", tools: ["issue_read"] }]);
  await form.submit();
  expect(submitted[0]).toMatchObject({
    mcpGrants: [{ connectionId: "github", tools: ["issue_read"] }],
  });
  expect(form.getState().mcpGrants).toEqual([]);
  await form.submit();
  expect(submitted[1]).toMatchObject({ mcpGrants: [] });
});

test("rejects duplicate tool grants before the provider sees a task", () => {
  const form = openTaskForm({ create: async () => "task" });
  form.setMcpGrants([{ connectionId: "github", tools: ["issue_read", "issue_read"] }]);
  expect(form.getState().submission.status).toBe("failed");
  expect(form.getState().mcpGrants).toEqual([]);
});

test("creates a task using reviewed files, output paths and deadline", async () => {
  const submitted: unknown[] = [];
  const form = openTaskForm({
    async create(input) {
      submitted.push(input);
      return "agent-1";
    },
  });
  form.set("prompt", "Fix answer.py");
  form.set("filePath", "answer.py");
  form.set("fileText", "print(41)");
  form.addTextFile();
  form.set("outputPaths", "answer.py\nresult.txt");
  await form.submit();
  expect(submitted).toEqual([
    {
      prompt: "Fix answer.py",
      options: {
        inputFiles: [{ path: "answer.py", text: "print(41)" }],
        outputPaths: ["answer.py", "result.txt"],
        timeoutMs: 300_000,
        mcpGrants: [],
        workspaceMaxTotalBytes: DEFAULT_WORKSPACE_MAX_TOTAL_BYTES,
        workspaceMaxEntries: DEFAULT_WORKSPACE_MAX_ENTRIES,
      },
    },
  ]);
  expect(form.getState().submission).toEqual({ status: "created", agentId: "agent-1" });
});

test("retains input and an actionable failure, and prevents duplicate submissions", async () => {
  let reject!: (error: Error) => void;
  let calls = 0;
  const form = openTaskForm({
    create: () => {
      calls += 1;
      return new Promise((_resolve, failure) => {
        reject = failure;
      });
    },
  });
  form.set("prompt", "Task");
  const first = form.submit();
  await form.submit();
  expect(calls).toBe(1);
  expect(form.getState().submission).toEqual({ status: "submitting" });
  reject(new Error("Configure the sandbox host"));
  await first;
  expect(form.getState().prompt).toBe("Task");
  expect(form.getState().submission).toEqual({
    status: "failed",
    message: "Configure the sandbox host",
  });
});

test("rejects duplicate and escaping file paths without starting a task", async () => {
  const form = openTaskForm({
    async create() {
      throw new Error("must not run");
    },
  });
  form.addFiles([{ path: "a.bin", base64: "AP8=" }]);
  form.addFiles([{ path: "a.bin", text: "different" }]);
  expect(form.getState().files).toEqual([{ path: "a.bin", base64: "AP8=" }]);
  expect(form.getState().submission.status).toBe("failed");
  form.set("prompt", "Task");
  form.set("outputPaths", "../outside");
  await form.submit();
  expect(form.getState().submission.status).toBe("failed");
});

test("restores the editable draft through the panel tab state", () => {
  const drafts: unknown[] = [];
  const initial = {
    prompt: "Keep this prompt",
    outputPaths: "result.txt",
    timeoutSeconds: "90",
    filePath: "input.txt",
    fileText: "42\n",
    files: [{ path: "input.txt", text: "42\n" }],
  };
  const form = openTaskForm({
    initial,
    onDraftChange: (draft) => drafts.push(draft),
    async create() {
      return "agent-1";
    },
  });
  expect(form.getState()).toMatchObject(initial);
  form.set("prompt", "Updated prompt");
  expect(drafts.at(-1)).toMatchObject({
    ...initial,
    prompt: "Updated prompt",
  });
});
