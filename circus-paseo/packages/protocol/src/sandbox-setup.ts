import { z } from "zod";

// Local Electron IPC only. These operations are never daemon or plugin RPCs.
export const sandboxPresetSchema = z.enum(["compact", "balanced", "custom"]);
export const sandboxSetupInputSchema = z.strictObject({
  baseUrl: z.string().max(2048),
  model: z
    .string()
    .min(1)
    .max(128)
    .regex(/^[A-Za-z0-9][A-Za-z0-9._:/-]*$/),
  apiKey: z.string().max(4096).optional(),
  preset: sandboxPresetSchema,
});

export const sandboxCheckSchema = z.strictObject({
  id: z.enum(["platform", "source", "bridge", "python", "tls", "docker", "image"]),
  label: z.string(),
  ready: z.boolean(),
  detail: z.string(),
});

export const sandboxSetupSnapshotSchema = z.strictObject({
  supported: z.boolean(),
  ready: z.boolean(),
  checks: z.array(sandboxCheckSchema),
  settings: z.strictObject({
    baseUrl: z.string(),
    model: z.string(),
    preset: sandboxPresetSchema,
    hasCredential: z.boolean(),
    credentialStorage: z.enum(["keychain", "file", "none"]),
  }),
});

export const sandboxSetupResultSchema = z.strictObject({
  ok: z.boolean(),
  message: z.string(),
});

export type SandboxSetupInput = z.infer<typeof sandboxSetupInputSchema>;
export type SandboxSetupSnapshot = z.infer<typeof sandboxSetupSnapshotSchema>;
export type SandboxSetupResult = z.infer<typeof sandboxSetupResultSchema>;
export type SandboxResourcePreset = z.infer<typeof sandboxPresetSchema>;

export const sandboxGithubInputSchema = z.strictObject({
  id: z.string().regex(/^[A-Za-z0-9_-]{1,64}$/),
  url: z.string().max(2048),
  token: z
    .string()
    .min(1)
    .max(4096)
    .refine((value) => !/[\r\n]/.test(value) && !value.includes("\0")),
  owner: z.string().regex(/^[A-Za-z0-9][A-Za-z0-9-]{0,38}$/),
  repo: z.string().regex(/^[A-Za-z0-9_.-]{1,100}$/),
});
export type SandboxGithubInput = z.infer<typeof sandboxGithubInputSchema>;
