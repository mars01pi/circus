import { expect, test } from "vitest";
import { TaskTimeline } from "./timeline.js";

test("streams text and tool snapshots without interpreting guest paths or commands", () => {
  const timeline = new TaskTimeline("task-1");
  expect(timeline.accept({ kind: "text_delta", text: "Hello " })).toEqual({
    type: "assistant_message",
    id: "task-1:assistant",
    text: "Hello ",
  });
  expect(timeline.accept({ kind: "text_delta", text: "world" })).toEqual({
    type: "assistant_message",
    id: "task-1:assistant",
    text: "Hello world",
  });
  timeline.accept({
    kind: "tool",
    id: "t1",
    tool: "commandExecution",
    phase: "started",
    exitCode: null,
  });
  timeline.accept({ kind: "tool_output", id: "t1", text: "untrusted /etc/passwd\n" });
  expect(
    timeline.accept({
      kind: "tool",
      id: "t1",
      tool: "commandExecution",
      phase: "completed",
      exitCode: 1,
    }),
  ).toEqual({
    type: "tool_call",
    id: "task-1:tool:t1",
    callId: "task-1:t1",
    name: "commandExecution",
    status: "failed",
    error: { exitCode: 1 },
    detail: { type: "plain_text", label: "commandExecution", text: "untrusted /etc/passwd\n" },
  });
  expect(timeline.accept({ kind: "phase", phase: "cleaning" })).toEqual({
    type: "notification",
    id: "task-1:phase",
    level: "info",
    message: "Cleaning up sandbox…",
  });
});
