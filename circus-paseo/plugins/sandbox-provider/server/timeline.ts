import type { ProviderTimelineItem } from "@getpaseo/plugin/server/provider";
import type { CircusTaskEvent } from "./bridge/protocol.js";

const phaseLabels = {
  preparing: "Preparing sandbox…",
  running: "Running in sandbox…",
  exporting: "Exporting requested files…",
  cleaning: "Cleaning up sandbox…",
};

interface Tool {
  name: string;
  text: string;
  phase: "started" | "completed";
  exitCode: number | null;
}

export class TaskTimeline {
  private text = "";
  private readonly tools = new Map<string, Tool>();

  constructor(private readonly taskId: string) {}

  accept(event: CircusTaskEvent): ProviderTimelineItem | null {
    switch (event.kind) {
      case "phase":
        return {
          type: "notification",
          id: `${this.taskId}:phase`,
          level: "info",
          message: phaseLabels[event.phase],
        };
      case "sandbox_created":
        return null;
      case "text_delta":
        this.text += event.text;
        return { type: "assistant_message", id: `${this.taskId}:assistant`, text: this.text };
      case "tool": {
        const prior = this.tools.get(event.id);
        this.tools.set(event.id, {
          name: event.tool,
          text: prior?.text ?? "",
          phase: event.phase,
          exitCode: event.exitCode,
        });
        return this.toolItem(event.id);
      }
      case "tool_output": {
        const tool = this.tools.get(event.id) ?? {
          name: "Sandbox tool",
          text: "",
          phase: "started",
          exitCode: null,
        };
        tool.text += event.text;
        this.tools.set(event.id, tool);
        return this.toolItem(event.id);
      }
    }
  }

  finish(state: "failed" | "canceled"): ProviderTimelineItem[] {
    return [...this.tools.entries()]
      .filter(([, tool]) => tool.phase === "started")
      .map(([id, tool]) =>
        Object.assign(
          {
            type: "tool_call" as const,
            id: `${this.taskId}:tool:${id}`,
            callId: `${this.taskId}:${id}`,
            name: tool.name,
            detail: { type: "plain_text" as const, label: tool.name, text: tool.text },
          },
          terminalToolState(state),
        ),
      );
  }

  private toolItem(id: string): ProviderTimelineItem {
    const tool = this.tools.get(id);
    if (!tool) throw new Error("Missing sandbox tool snapshot");
    const failed = tool.exitCode !== null && tool.exitCode !== 0;
    const status = tool.phase === "started" ? "running" : "completed";
    return {
      type: "tool_call",
      id: `${this.taskId}:tool:${id}`,
      callId: `${this.taskId}:${id}`,
      name: tool.name,
      detail: { type: "plain_text", label: tool.name, text: tool.text },
      ...(failed
        ? { status: "failed", error: { exitCode: tool.exitCode } }
        : { status, error: null }),
    };
  }
}

function terminalToolState(state: "failed" | "canceled") {
  if (state === "failed") return { status: "failed", error: "Sandbox task ended" } as const;
  return { status: "canceled", error: null } as const;
}
