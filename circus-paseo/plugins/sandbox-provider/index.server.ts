import type { PluginServerContext } from "@getpaseo/plugin/server";
import { readArtifact, retryCleanup } from "./shared/artifacts.js";
import { ArtifactStore } from "./server/artifacts.js";
import { SandboxProvider } from "./server/provider.js";
import { listMcpConnections } from "./shared/mcp.js";

export default function contribute(server: PluginServerContext) {
  const artifacts = new ArtifactStore();
  const provider = new SandboxProvider(artifacts);
  server.registerProvider(provider.registration());
  server.handle(readArtifact, ({ id, offset }) => artifacts.read(id, offset));
  server.handle(retryCleanup, ({ taskId }) => provider.retryCleanup(taskId));
  server.handle(listMcpConnections, () => provider.mcpCatalog());
  return () => artifacts.close();
}
