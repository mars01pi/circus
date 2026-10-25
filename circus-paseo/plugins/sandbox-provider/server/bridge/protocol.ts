import { z } from "zod";
import { mcpGrantsSchema } from "../../shared/mcp.js";
import { MAX_WORKSPACE_ENTRIES, MAX_WORKSPACE_TOTAL_BYTES } from "../../shared/task.js";

export const PROTOCOL_VERSION = 1;
export const MAX_FRAME_BYTES = 12 * 1024 * 1024;
export const MAX_FILE_BYTES = 8 * 1024 * 1024;
export const MAX_EVENT_BYTES = 512 * 1024;

const id = z.string().regex(/^[a-zA-Z0-9-]{1,64}$/);
const sessionId = z.string().regex(/^[a-zA-Z0-9_-]{1,128}$/);
const path = z.string().refine(isProjectPath);
const hostPath = z.string().refine(isAbsoluteHostPath);
const base64 = z
  .string()
  .max(Math.ceil(MAX_FILE_BYTES / 3) * 4)
  .refine((value) => {
    return Buffer.from(value, "base64").toString("base64") === value;
  });
const file = z.strictObject({ path, data: base64 });

export function isProjectPath(value: string): boolean {
  const forbidden = /[\\:\p{Cc}]/u.test(value);
  const invalidComponent = value.split("/").some((part) => !part || part === "." || part === "..");
  return (
    Buffer.byteLength(value) <= 1024 &&
    Buffer.from(value).toString() === value &&
    !forbidden &&
    !invalidComponent
  );
}

/** Trusted host path from configuration or the provider's derived state path. */
export function isAbsoluteHostPath(value: string): boolean {
  const absolute =
    process.platform === "win32"
      ? /^(?:[A-Za-z]:[\\/]|\\\\[^\\/])/.test(value)
      : value.startsWith("/");
  return (
    value.length > 0 &&
    Buffer.byteLength(value) <= 4096 &&
    Buffer.from(value).toString() === value &&
    !/\p{Cc}/u.test(value) &&
    absolute
  );
}

function files(max: number) {
  return z
    .array(file)
    .max(max)
    .refine((values) => {
      const paths = new Set(values.map((value) => value.path));
      const bytes = values.reduce((sum, value) => sum + Buffer.byteLength(value.data, "base64"), 0);
      return paths.size === values.length && bytes <= MAX_FILE_BYTES;
    });
}

export const TaskRequestSchema = z
  .strictObject({
    prompt: z.string().refine((value) => {
      const bytes = Buffer.byteLength(value);
      return (
        bytes > 0 &&
        bytes <= 65536 &&
        Buffer.from(value).toString() === value &&
        !value.includes("\0")
      );
    }),
    inputFiles: files(64),
    outputPaths: z
      .array(path)
      .max(32)
      .refine((values) => new Set(values).size === values.length),
    timeoutMs: z.number().int().min(1).max(3_600_000),
    mcpGrants: mcpGrantsSchema.optional(),
    workspaceArchive: hostPath.optional(),
    workspaceMaxTotalBytes: z.number().int().min(1).max(MAX_WORKSPACE_TOTAL_BYTES).optional(),
    workspaceMaxEntries: z.number().int().min(1).max(MAX_WORKSPACE_ENTRIES).optional(),
    agentSession: sessionId.optional(),
  })
  .superRefine((request, context) => {
    if (
      request.workspaceArchive === undefined &&
      (request.workspaceMaxTotalBytes !== undefined || request.workspaceMaxEntries !== undefined)
    ) {
      context.addIssue({
        code: "custom",
        path: ["workspaceArchive"],
        message: "Workspace budgets require a workspace archive",
      });
    }
  });

export const TaskEventSchema = z.discriminatedUnion("kind", [
  z.strictObject({
    kind: z.literal("phase"),
    phase: z.enum(["preparing", "running", "exporting", "cleaning"]),
  }),
  z.strictObject({ kind: z.literal("sandbox_created"), instanceId: id }),
  z.strictObject({ kind: z.literal("text_delta"), text: z.string().max(65536) }),
  z.strictObject({
    kind: z.literal("tool"),
    id: z.string().max(65536),
    tool: z.string().max(65536),
    phase: z.enum(["started", "completed"]),
    exitCode: z.number().int().safe().nullable(),
  }),
  z.strictObject({
    kind: z.literal("tool_output"),
    id: z.string().max(65536),
    text: z.string().max(65536),
  }),
]);
export type CircusTaskEvent = z.infer<typeof TaskEventSchema>;

export const CleanupStatusSchema = z.enum(["confirmed", "pending", "unknown"]);
export type CleanupStatus = z.infer<typeof CleanupStatusSchema>;
const failure = z.strictObject({
  code: z.enum([
    "invalid_request",
    "cancelled",
    "timed_out",
    "backpressure",
    "sandbox",
    "workspace",
    "host_io",
    "model_tls",
    "model_timeout",
    "model_rejected",
    "model_transport",
    "model_protocol",
    "model_budget",
    "agent_failed",
    "unsupported_interaction",
    "protocol",
    "process",
    "artifact_limit",
    "cleanup",
    "worker_gone",
  ]),
  message: z.string().max(256),
});
const result = z.discriminatedUnion("status", [
  z.strictObject({
    status: z.literal("succeeded"),
    cleanup: z.literal("confirmed"),
    artifacts: files(32),
    agentSession: sessionId.optional(),
  }),
  z.strictObject({ status: z.literal("failed"), cleanup: CleanupStatusSchema, error: failure }),
  z.strictObject({ status: z.literal("cancelled"), cleanup: CleanupStatusSchema, error: failure }),
]);

const correlation = { v: z.literal(PROTOCOL_VERSION), requestId: id, taskId: id };
export const ClientFrameSchema = z.discriminatedUnion("type", [
  z.strictObject({
    v: z.literal(PROTOCOL_VERSION),
    type: z.literal("start"),
    requestId: id,
    request: TaskRequestSchema,
  }),
  z.strictObject({ ...correlation, type: z.literal("cancel") }),
  z.strictObject({ ...correlation, type: z.literal("cleanup_retry") }),
]);
export const ServerFrameSchema = z.discriminatedUnion("type", [
  z.strictObject({
    v: z.literal(PROTOCOL_VERSION),
    type: z.literal("ready"),
    maxFrameBytes: z.literal(MAX_FRAME_BYTES),
  }),
  z.strictObject({ ...correlation, type: z.literal("accepted") }),
  z.strictObject({
    ...correlation,
    type: z.literal("event"),
    seq: z.number().int().min(1).safe(),
    event: TaskEventSchema,
  }),
  z.strictObject({ ...correlation, type: z.literal("result"), result }),
  z.strictObject({ ...correlation, type: z.literal("cancel_ack") }),
  z.strictObject({
    ...correlation,
    type: z.literal("cleanup_result"),
    cleanup: CleanupStatusSchema,
  }),
  z.strictObject({
    v: z.literal(PROTOCOL_VERSION),
    type: z.literal("error"),
    code: z.enum([
      "invalid_frame",
      "invalid_request",
      "wrong_task",
      "invalid_state",
      "transport",
      "configuration",
    ]),
    message: z.string().max(256),
  }),
]);
export type ClientFrame = z.infer<typeof ClientFrameSchema>;
export type ServerFrame = z.infer<typeof ServerFrameSchema>;
export type WireResult = z.infer<typeof result>;
