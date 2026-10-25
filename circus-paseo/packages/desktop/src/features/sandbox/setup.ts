import { createHash, randomUUID } from "node:crypto";
import { mkdir, open, rename, rm, writeFile } from "node:fs/promises";
import { constants } from "node:fs";
import { join, isAbsolute } from "node:path";
import { z } from "zod";
import { readPersistedConfig, editPersistedConfig } from "@getpaseo/server/configuration";
import { ProviderOverrideSchema } from "@getpaseo/protocol/provider-config";
import {
  sandboxGithubInputSchema,
  sandboxSetupInputSchema,
  type SandboxSetupInput,
  type SandboxSetupResult,
  type SandboxSetupSnapshot,
} from "@getpaseo/protocol/sandbox-setup";
import { createKeychain, type KeychainReference } from "./keychain.js";
import {
  CODEX_IMAGE,
  exists,
  inspectEnvironment,
  inspectPythonTrust,
  type SandboxEnvironment,
} from "./environment.js";
import type { RunCommand } from "./command.js";
import { probeGithub } from "./mcp.js";
import { acceptedResponse } from "./model.js";

export class SandboxSetupError extends Error {}

const referenceSchema = z.strictObject({ service: z.string(), account: z.string() });
const connectionSchema = z
  .object({
    id: z.string(),
    url: z.string(),
    tools: z.array(
      z.object({
        name: z.string(),
        allowedArguments: z.array(z.string()),
        fixedArguments: z.record(z.string(), z.string()),
      }),
    ),
    headersKeychain: referenceSchema.optional(),
    headersFile: z.string().optional(),
    caFile: z.string().optional(),
  })
  .passthrough();
const resourcesSchema = z
  .object({
    memoryBytes: z.number(),
    cpuMillis: z.number(),
    maxProcesses: z.number(),
    workspaceBytes: z.number(),
    maxFileBytes: z.number(),
  })
  .passthrough();
const runnerSchema = z
  .object({
    version: z.literal(1),
    dockerEndpoint: z.string(),
    agentImage: z.string(),
    python: z.string(),
    resources: resourcesSchema,
    model: z
      .object({
        baseUrl: z.string(),
        model: z.string(),
        apiKeyFile: z.string().optional(),
        apiKeyKeychain: referenceSchema.optional(),
        caFile: z.string().optional(),
      })
      .passthrough(),
  })
  .passthrough();
type RunnerConfig = z.infer<typeof runnerSchema>;
const presets = {
  compact: {
    memoryBytes: 536870912,
    cpuMillis: 1000,
    maxProcesses: 128,
    workspaceBytes: 134217728,
    maxFileBytes: 2097152,
  },
  balanced: {
    memoryBytes: 1073741824,
    cpuMillis: 2000,
    maxProcesses: 128,
    workspaceBytes: 268435456,
    maxFileBytes: 2097152,
  },
};

export interface SandboxSetupDependencies {
  home: string;
  userHome: string;
  platform: string;
  sourceCandidates: string[];
  binaryDirectories: string[];
  run: RunCommand;
  reload(): Promise<void>;
  request: typeof fetch;
}

function validateHttpsUrl(value: string): string {
  let url: URL;
  try {
    url = new URL(value);
  } catch {
    throw new SandboxSetupError("Enter a valid HTTPS model service URL.");
  }
  if (url.protocol !== "https:" || url.username || url.password || url.search || url.hash)
    throw new SandboxSetupError(
      "Use an HTTPS URL without credentials, query parameters or fragments.",
    );
  return url.toString();
}

function validateBaseUrl(value: string): string {
  return validateHttpsUrl(value).replace(/\/+$/, "");
}

function presetFor(resources: RunnerConfig["resources"]): SandboxSetupInput["preset"] {
  for (const name of ["compact", "balanced"] as const) {
    if (Object.entries(presets[name]).every(([key, value]) => resources[key] === value))
      return name;
  }
  return "custom";
}

function requireRuntime(environment: SandboxEnvironment) {
  const { bridge, endpoint, image, python } = environment;
  if (!bridge || !endpoint || !image || !python || !environment.trustedTls)
    throw new SandboxSetupError("Finish the environment checks before saving.");
  return { bridge, endpoint, image, python };
}

function selectResources(preset: SandboxSetupInput["preset"], config: RunnerConfig | null) {
  if (config?.agent && config.agent !== "codex")
    throw new SandboxSetupError(
      "This setup currently manages Codex only. Your existing agent configuration has not been changed.",
    );
  if (preset !== "custom") return presets[preset];
  if (!config) throw new SandboxSetupError("Choose a resource preset for the new environment.");
  return config.resources;
}

function credentialStorage(
  config: RunnerConfig | null,
): SandboxSetupSnapshot["settings"]["credentialStorage"] {
  if (config?.model.apiKeyKeychain) return "keychain";
  if (config?.model.apiKeyFile) return "file";
  return "none";
}

async function readBounded(path: string, limit = 65536): Promise<string> {
  const file = await open(path, constants.O_RDONLY | constants.O_NOFOLLOW | constants.O_NONBLOCK);
  try {
    if (!(await file.stat()).isFile())
      throw new SandboxSetupError("The saved configuration or credential must be a regular file.");
    const buffer = Buffer.alloc(limit + 1);
    let total = 0;
    while (total < buffer.length) {
      const { bytesRead } = await file.read(buffer, total, buffer.length - total, total);
      if (bytesRead === 0) break;
      total += bytesRead;
    }
    if (total > limit)
      throw new SandboxSetupError("The saved configuration or credential exceeds its size limit.");
    return buffer.subarray(0, total).toString("utf8");
  } finally {
    await file.close();
  }
}

async function restoreConfig(path: string, previous: string | null): Promise<void> {
  if (previous === null) await rm(path, { force: true });
  else await atomicWrite(path, previous);
}

async function atomicWrite(path: string, contents: string): Promise<void> {
  const temporary = `${path}.${randomUUID()}.tmp`;
  try {
    await writeFile(temporary, contents, { mode: 0o600, flag: "wx" });
    await rename(temporary, path);
  } finally {
    await rm(temporary, { force: true });
  }
}

export function createSandboxSetup(deps: SandboxSetupDependencies) {
  const directory = join(deps.home, "circus");
  const managedConfig = join(directory, "runner.json");
  const sourceFile = join(directory, "source.json");
  const keychain = createKeychain(deps.run);
  let saving = false;

  function requirePlatform(): void {
    if (deps.platform !== "darwin")
      throw new SandboxSetupError("Desktop setup currently requires macOS Keychain.");
  }

  async function current() {
    const persisted = readPersistedConfig(deps.home, { defaultsIfMissing: true });
    const candidate = persisted.agents?.providers?.sandbox;
    const provider = candidate ? ProviderOverrideSchema.parse(candidate) : undefined;
    const path = provider?.env?.CIRCUS_RUNNER_CONFIG;
    if (!path) return { config: null, provider, path: null, serialized: null };
    if (!isAbsolute(path))
      throw new SandboxSetupError(
        "The existing sandbox configuration path is invalid. It has not been changed.",
      );
    let config: RunnerConfig;
    let serialized: string;
    try {
      serialized = await readBounded(path);
      config = runnerSchema.parse(JSON.parse(serialized));
    } catch {
      throw new SandboxSetupError(
        "The existing sandbox configuration could not be read. It has not been changed.",
      );
    }
    return { config, provider, path, serialized };
  }

  async function inspect() {
    const stored = await current();
    const candidates = [...deps.sourceCandidates];
    if (await exists(sourceFile)) {
      const selected = z.string().parse(JSON.parse(await readBounded(sourceFile, 8192)));
      candidates.unshift(selected);
    }
    const environment = await inspectEnvironment({
      platform: deps.platform,
      userHome: deps.userHome,
      sourceCandidates: candidates,
      run: deps.run,
      binaryDirectories: deps.binaryDirectories,
      configured: stored.config ?? undefined,
      configuredBridge: stored.provider?.command?.[0],
    });
    return { ...stored, environment };
  }

  async function credential(
    input: SandboxSetupInput,
    config: RunnerConfig | null,
  ): Promise<string> {
    if (input.apiKey) {
      if (/[\r\n]/.test(input.apiKey) || input.apiKey.includes("\0"))
        throw new SandboxSetupError("The API key must be a single line.");
      return input.apiKey;
    }
    if (!config || validateBaseUrl(config.model.baseUrl) !== validateBaseUrl(input.baseUrl))
      throw new SandboxSetupError(
        "Enter an API key for this model service. Stored credentials are never sent to a different service URL.",
      );
    if (config.model.apiKeyKeychain) {
      const value = await keychain.get(config.model.apiKeyKeychain);
      if (value) return value;
    } else if (config.model.apiKeyFile) {
      const value = await readBounded(config.model.apiKeyFile, 4098);
      if (value.length <= 4098) return value.replace(/[\r\n]+$/, "");
    }
    throw new SandboxSetupError("The saved credential is unavailable. Enter the API key again.");
  }

  return {
    async snapshot(): Promise<SandboxSetupSnapshot> {
      const { config, environment } = await inspect();
      const supported = deps.platform === "darwin";
      const ready = environment.checks
        .filter((check) => check.id !== "source")
        .every((check) => check.ready);
      const storage = credentialStorage(config);
      return {
        supported,
        ready,
        checks: environment.checks,
        settings: {
          baseUrl: config?.model.baseUrl ?? "",
          model: config?.model.model ?? "",
          preset: config ? presetFor(config.resources) : "compact",
          hasCredential: storage !== "none",
          credentialStorage: storage,
        },
      };
    },
    async selectSource(path: string): Promise<void> {
      requirePlatform();
      if (
        !isAbsolute(path) ||
        !(await exists(join(path, "circus-sandbox/images/codex/Dockerfile")))
      )
        throw new SandboxSetupError(
          "Select the Circus repository folder containing circus-sandbox.",
        );
      await mkdir(directory, { recursive: true, mode: 0o700 });
      await atomicWrite(sourceFile, JSON.stringify(path));
    },
    async prepare(): Promise<SandboxSetupResult> {
      requirePlatform();
      const { environment } = await inspect();
      const { source, docker, endpoint } = environment;
      if (!source) return { ok: false, message: "Select the Circus source folder first." };
      if (!docker || !endpoint)
        return {
          ok: false,
          message: "Start Docker Desktop or your local Linux Docker Engine, then check again.",
        };
      if (!environment.bridge) {
        const built = await deps.run({
          executable: join(deps.userHome, ".cargo/bin/cargo"),
          args: ["build", "--locked", "-p", "circus-stdio-bridge"],
          cwd: join(source, "circus-sandbox"),
          timeoutMs: 600_000,
        });
        if (!built.ok)
          return {
            ok: false,
            message:
              "The task runner build failed. Install the repository's Rust 1.99.1 toolchain and retry, or build circus-stdio-bridge using the documented developer setup.",
          };
      }
      if (!environment.image) {
        const built = await deps.run({
          executable: docker,
          args: [
            "--host",
            endpoint,
            "build",
            "--platform",
            "linux/amd64",
            "--tag",
            CODEX_IMAGE,
            join(source, "circus-sandbox/images/codex"),
          ],
          timeoutMs: 600_000,
        });
        if (!built.ok)
          return {
            ok: false,
            message:
              "The Codex image build failed. Check Docker's internet access and available disk space, then retry.",
          };
      }
      return {
        ok: true,
        message: "Runtime preparation finished. Check the environment before saving.",
      };
    },
    async testConnection(raw: unknown): Promise<SandboxSetupResult> {
      requirePlatform();
      const input = sandboxSetupInputSchema.parse(raw);
      const baseUrl = validateBaseUrl(input.baseUrl);
      const { config } = await current();
      const apiKey = await credential(input, config);
      try {
        const response = await deps.request(`${baseUrl}/responses`, {
          method: "POST",
          redirect: "error",
          signal: AbortSignal.timeout(20_000),
          headers: { "Content-Type": "application/json", Authorization: `Bearer ${apiKey}` },
          body: JSON.stringify({
            model: input.model,
            input: "Reply with OK.",
            max_output_tokens: 32,
            stream: false,
          }),
        });
        if (response.ok) {
          if (await acceptedResponse(response))
            return {
              ok: true,
              message:
                "The model service accepted a small Responses API request. This checks the connection, not a complete sandbox task.",
            };
          return {
            ok: false,
            message:
              "The service did not return a supported Responses API result. Check the URL and model. Raw provider output is not shown.",
          };
        }
        await response.body?.cancel();
        if (response.status === 401 || response.status === 403)
          return {
            ok: false,
            message:
              "The service rejected this credential or model. Check the key and model access.",
          };
        if (response.status === 404)
          return {
            ok: false,
            message: "Responses API was not found. Check the service URL, including its /v1 path.",
          };
        return {
          ok: false,
          message: `The model service returned HTTP ${response.status}. Check model availability and retry.`,
        };
      } catch {
        return {
          ok: false,
          message:
            "Could not reach the HTTPS model service. Check the address, trusted certificate, and internet connection. Redirects are not followed.",
        };
      }
    },
    async saveGithub(raw: unknown): Promise<SandboxSetupResult> {
      requirePlatform();
      if (saving) throw new SandboxSetupError("A configuration update is already in progress.");
      saving = true;
      let added: KeychainReference | null = null;
      let saved = false;
      try {
        const parsed = sandboxGithubInputSchema.parse(raw);
        const input = { ...parsed, url: validateHttpsUrl(parsed.url) };
        const { config, path, serialized } = await current();
        if (!config || !path)
          throw new SandboxSetupError("Save the model and sandbox setup before adding tools.");
        const connections = z
          .array(connectionSchema)
          .max(7)
          .parse(config.mcpConnections ?? []);
        if (connections.some((connection) => connection.id === input.id))
          throw new SandboxSetupError(
            "A connection with this name already exists. Choose a new name; existing permissions have not been changed.",
          );
        // MCP has a separate trust boundary; the model endpoint may use its own private CA.
        const tls = await inspectPythonTrust({
          python: config.python,
          platform: deps.platform,
          run: deps.run,
        });
        if (!tls.ready)
          throw new SandboxSetupError(
            "The MCP connector cannot load trusted certificates. Repair Python's certificate store before connecting a service.",
          );
        const checked = await probeGithub(input, deps.request);
        if (!checked.ok) return checked;
        added = {
          service: `app.circus.mcp.${createHash("sha256").update(deps.home).digest("hex").slice(0, 16)}`,
          account: randomUUID(),
        };
        if (
          !(await keychain.put(added, JSON.stringify({ Authorization: `Bearer ${input.token}` })))
        )
          throw new SandboxSetupError(
            "Could not store the MCP credential in macOS Keychain. Unlock your login keychain and retry.",
          );
        const connection = {
          id: input.id,
          url: input.url,
          headersKeychain: added,
          ...(tls.caFile ? { caFile: tls.caFile } : {}),
          tools: [
            {
              name: "issue_read",
              allowedArguments: ["issue_number"],
              fixedArguments: { method: "get", owner: input.owner, repo: input.repo },
            },
          ],
        };
        if ((await readBounded(path)) !== serialized)
          throw new SandboxSetupError(
            "Sandbox configuration changed during the connection check. Retry with the latest configuration.",
          );
        await atomicWrite(
          path,
          `${JSON.stringify({ ...config, mcpConnections: [...connections, connection] }, null, 2)}\n`,
        );
        saved = true;
        try {
          await deps.reload();
        } catch {
          return {
            ok: true,
            message:
              "GitHub policy saved with its credential in Keychain. Reload host configuration, then explicitly allow issue_read in a new sandbox task.",
          };
        }
        return {
          ok: true,
          message: `GitHub connection saved for ${input.owner}/${input.repo}. In a new sandbox task, explicitly allow ${input.id} / issue_read. No task is authorized automatically.`,
        };
      } finally {
        try {
          if (!saved && added) await keychain.remove(added);
        } finally {
          saving = false;
        }
      }
    },
    async save(raw: unknown): Promise<SandboxSetupResult> {
      requirePlatform();
      if (saving) throw new SandboxSetupError("A configuration update is already in progress.");
      saving = true;
      let added: KeychainReference | null = null;
      let previous: string | null = null;
      let wroteConfig = false;
      let applied = false;
      try {
        const input = sandboxSetupInputSchema.parse(raw);
        const baseUrl = validateBaseUrl(input.baseUrl);
        const { config, provider, environment, path, serialized } = await inspect();
        const { bridge, endpoint, image, python } = requireRuntime(environment);
        const apiKey = await credential(input, config);
        const resources = selectResources(input.preset, config);
        added = {
          service: `app.circus.model.${createHash("sha256").update(deps.home).digest("hex").slice(0, 16)}`,
          account: randomUUID(),
        };
        if (!(await keychain.put(added, apiKey)))
          throw new SandboxSetupError(
            "Could not save the credential in macOS Keychain. Unlock your login keychain and retry.",
          );
        const model = { ...config?.model, baseUrl, model: input.model, apiKeyKeychain: added };
        delete model.apiKeyFile;
        if (environment.modelCaFile) model.caFile = environment.modelCaFile;
        const next: RunnerConfig = {
          ...config,
          version: 1,
          dockerEndpoint: endpoint,
          agentImage: image,
          python,
          resources,
          model,
        };
        await mkdir(directory, { recursive: true, mode: 0o700 });
        if (path && (await readBounded(path)) !== serialized)
          throw new SandboxSetupError(
            "Sandbox configuration changed while saving. Check again before retrying.",
          );
        if (await exists(managedConfig)) previous = await readBounded(managedConfig);
        await atomicWrite(managedConfig, `${JSON.stringify(next, null, 2)}\n`);
        wroteConfig = true;
        // Existing provider options and all other daemon settings remain owned by the daemon config API.
        const latest = readPersistedConfig(deps.home, { defaultsIfMissing: true });
        const latestCandidate = latest.agents?.providers?.sandbox;
        const latestProvider = latestCandidate
          ? ProviderOverrideSchema.parse(latestCandidate)
          : undefined;
        if (JSON.stringify(latestProvider) !== JSON.stringify(provider))
          throw new SandboxSetupError(
            "Sandbox settings changed while saving. Check again before retrying.",
          );
        editPersistedConfig(deps.home, "agents.providers", {
          value: {
            ...latest.agents?.providers,
            sandbox: {
              ...provider,
              enabled: true,
              command: [bridge],
              env: {
                ...provider?.env,
                CIRCUS_RUNNER_CONFIG: managedConfig,
                CIRCUS_SANDBOX_STATE_DIR: join(directory, "sandbox-state"),
              },
            },
          },
        });
        applied = true;
        try {
          await deps.reload();
        } catch {
          return {
            ok: true,
            message:
              "Saved locally with the credential in Keychain. The running host did not reload; use Reload configuration before starting a sandbox task. No daemon was restarted.",
          };
        }
        return {
          ok: true,
          message:
            "Sandbox setup saved and host configuration reloaded. Open a workspace and choose New sandbox task.",
        };
      } finally {
        try {
          try {
            if (!applied && wroteConfig) await restoreConfig(managedConfig, previous);
          } finally {
            if (!applied && added) await keychain.remove(added);
          }
        } finally {
          saving = false;
        }
      }
    },
  };
}
