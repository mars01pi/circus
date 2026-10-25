import { constants } from "node:fs";
import { open } from "node:fs/promises";
import { z } from "zod";
import { mcpConnectionViewSchema, type McpConnectionView } from "../shared/mcp.js";

// Parse only the public projection. Secret sources and endpoints never leave
// the host, and this reader never resolves a credential reference.
const catalogConfig = z.object({
  mcpConnections: z
    .array(
      z.object({
        id: z.string(),
        tools: z.array(
          z.object({
            name: z.string(),
            fixedArguments: z.record(z.string(), z.string()).default({}),
          }),
        ),
      }),
    )
    .max(8)
    .default([]),
});
const publicConnections = z.array(mcpConnectionViewSchema).max(8);
type CatalogResult =
  | { status: "ready"; connections: McpConnectionView[] }
  | { status: "unavailable"; message: string };

export async function readMcpCatalog(configFile: string | null): Promise<CatalogResult> {
  if (configFile === null)
    return {
      status: "unavailable",
      message: "Configure the sandbox provider on its desktop host, then refresh its status.",
    };
  const limit = 64 * 1024;
  try {
    const flags = constants.O_RDONLY | (constants.O_NOFOLLOW || 0) | (constants.O_NONBLOCK || 0);
    const file = await open(configFile, flags);
    try {
      const metadata = await file.stat();
      if (!metadata.isFile() || metadata.size > limit) throw new Error("configuration");
      const buffer = Buffer.alloc(limit + 1);
      let length = 0;
      while (length < buffer.length) {
        const { bytesRead } = await file.read(buffer, length, buffer.length - length, null);
        if (bytesRead === 0) break;
        length += bytesRead;
      }
      if (length > limit) throw new Error("configuration");
      const config = catalogConfig.parse(JSON.parse(buffer.subarray(0, length).toString("utf8")));
      const connections = publicConnections.parse(config.mcpConnections);
      return { status: "ready", connections };
    } finally {
      await file.close();
    }
  } catch {
    // All input is private host configuration; do not return parser or OS errors.
    return {
      status: "unavailable",
      message: "Cannot read sandbox MCP permissions. Check the host configuration.",
    };
  }
}
