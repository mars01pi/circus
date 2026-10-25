import type { PluginClientContext } from "@getpaseo/plugin/client";
import { SandboxResult } from "./client/result.js";
import { TaskPanel } from "./client/task-panel.js";
import { resultSchema } from "./shared/artifacts.js";

export default function contribute(client: PluginClientContext) {
  client.addWorkspacePanel({
    id: "sandbox-task",
    title: "Sandbox task",
    icon: "Box",
    Component: TaskPanel,
  });
  client.addCommandCenterItem({
    id: "new-sandbox-task",
    title: "New sandbox task",
    icon: "Box",
    context: "workspace",
    onSelect({ openPanel }) {
      openPanel("sandbox-task");
    },
  });
  client.addTimelineRenderer({
    kind: "sandbox-result",
    version: 1,
    schema: resultSchema,
    Component: SandboxResult,
  });
  return () => {};
}
