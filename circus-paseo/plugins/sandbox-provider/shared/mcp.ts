import { defineRpc } from "@getpaseo/plugin";
import { z } from "zod";

const connectionId = z.string().regex(/^[A-Za-z0-9_-]{1,64}$/);
const toolName = z.string().regex(/^[A-Za-z0-9_-]{1,64}$/);
const uniqueTools = z
  .array(toolName)
  .min(1)
  .max(32)
  .refine((tools) => new Set(tools).size === tools.length);

export const mcpGrantSchema = z.strictObject({ connectionId, tools: uniqueTools });
export type McpGrant = z.infer<typeof mcpGrantSchema>;
export const mcpGrantsSchema = z
  .array(mcpGrantSchema)
  .max(8)
  .refine((grants) => {
    const ids = grants.map((grant) => grant.connectionId);
    return new Set(ids).size === ids.length;
  });

export const mcpConnectionViewSchema = z.strictObject({
  id: connectionId,
  tools: z
    .array(
      z.strictObject({
        name: toolName,
        fixedArguments: z.record(
          toolName,
          z
            .string()
            .max(1024)
            .refine((value) => !/\p{Cc}/u.test(value)),
        ),
      }),
    )
    .min(1)
    .max(32),
});
export type McpConnectionView = z.infer<typeof mcpConnectionViewSchema>;

export const listMcpConnections = defineRpc({
  name: "mcp.connections.list",
  input: z.strictObject({}),
  output: z.discriminatedUnion("status", [
    z.strictObject({
      status: z.literal("ready"),
      connections: z.array(mcpConnectionViewSchema).max(8),
    }),
    z.strictObject({ status: z.literal("unavailable"), message: z.string() }),
  ]),
});
