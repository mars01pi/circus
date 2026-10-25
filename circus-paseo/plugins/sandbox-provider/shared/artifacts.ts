import { defineRpc } from "@getpaseo/plugin";
import { z } from "zod";

export const artifactSchema = z.object({
  id: z.string(),
  path: z.string(),
  size: z.number().int().nonnegative(),
});
export const resultSchema = z.object({
  taskId: z.string(),
  status: z.enum(["succeeded", "failed", "cancelled"]),
  cleanup: z.enum(["confirmed", "pending", "unknown"]),
  message: z.string(),
  artifacts: z.array(artifactSchema),
});
export type TaskResultView = z.infer<typeof resultSchema>;

export const readArtifact = defineRpc({
  name: "artifact.read",
  input: z.object({ id: z.string().uuid(), offset: z.number().int().nonnegative() }),
  output: z.object({
    base64: z.string(),
    size: z.number().int().nonnegative(),
    nextOffset: z.number().int().nonnegative(),
    complete: z.boolean(),
  }),
});

export const retryCleanup = defineRpc({
  name: "task.cleanup.retry",
  input: z.object({ taskId: z.string() }),
  output: z.object({ cleanup: z.enum(["confirmed", "pending", "unknown"]) }),
});
