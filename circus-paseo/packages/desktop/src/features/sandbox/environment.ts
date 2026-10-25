import { access, constants } from "node:fs/promises";
import { dirname, join } from "node:path";
import type { SandboxSetupSnapshot } from "@getpaseo/protocol/sandbox-setup";
import type { RunCommand } from "./command.js";

export interface SandboxEnvironment {
  source: string | null;
  bridge: string | null;
  python: string | null;
  docker: string | null;
  endpoint: string | null;
  image: string | null;
  modelCaFile: string | null;
  trustedTls: boolean;
  checks: SandboxSetupSnapshot["checks"];
}

// Fully qualify the built-in tag: some local Engines fail short-name inspection even when
// their image listing contains it. Save still pins the inspected immutable image ID.
export const CODEX_IMAGE = "docker.io/library/circus-agent-codex:0.164.0";

export async function exists(path: string, mode = constants.R_OK): Promise<boolean> {
  try {
    await access(path, mode);
    return true;
  } catch {
    return false;
  }
}

async function firstExecutable(candidates: string[]): Promise<string | null> {
  for (const candidate of candidates) {
    if (await exists(candidate, constants.X_OK)) return candidate;
  }
  return null;
}

async function findSource(candidates: string[]): Promise<string | null> {
  for (let candidate of candidates) {
    for (let depth = 0; depth < 6; depth += 1) {
      if (await exists(join(candidate, "circus-sandbox/images/codex/Dockerfile"))) return candidate;
      const parent = dirname(candidate);
      if (parent === candidate) break;
      candidate = parent;
    }
  }
  return null;
}

interface InspectEnvironmentInput {
  platform: string;
  userHome: string;
  sourceCandidates: string[];
  run: RunCommand;
  configured?: {
    python: string;
    dockerEndpoint: string;
    agentImage: string;
    model?: { caFile?: string };
  };
  configuredBridge?: string;
  binaryDirectories: string[];
}

export async function inspectEnvironment(
  input: InspectEnvironmentInput,
): Promise<SandboxEnvironment> {
  const source = await findSource(input.sourceCandidates);
  const bridgeCandidates = input.binaryDirectories.map((directory) =>
    join(directory, "circus-stdio-bridge"),
  );
  if (source)
    bridgeCandidates.unshift(
      join(source, "circus-sandbox/target/debug/circus-stdio-bridge"),
      join(source, "circus-sandbox/target/release/circus-stdio-bridge"),
    );
  if (input.configuredBridge) bridgeCandidates.unshift(input.configuredBridge);
  const bridge = await firstExecutable(bridgeCandidates);
  const pythonCandidates = input.binaryDirectories.map((directory) => join(directory, "python3"));
  if (input.configured) pythonCandidates.unshift(input.configured.python);
  let python: string | null = null;
  for (const candidate of pythonCandidates) {
    const checked = await input.run({
      executable: candidate,
      args: ["-I", "-c", "import sys; raise SystemExit(sys.version_info < (3, 11))"],
    });
    if (checked.ok) {
      python = candidate;
      break;
    }
  }
  const tls = await inspectPythonTrust({
    python,
    platform: input.platform,
    run: input.run,
    caFile: input.configured?.model?.caFile,
  });
  const docker = await firstExecutable(
    input.binaryDirectories.map((directory) => join(directory, "docker")),
  );
  const endpoints = [
    "unix:///var/run/docker.sock",
    `unix://${input.userHome}/.docker/run/docker.sock`,
    `unix://${input.userHome}/.colima/default/docker.sock`,
  ];
  if (input.configured) endpoints.unshift(input.configured.dockerEndpoint);
  const imageRef = input.configured?.agentImage ?? CODEX_IMAGE;
  const { endpoint, image } = await inspectDocker({ docker, endpoints, imageRef, run: input.run });
  const supported = input.platform === "darwin";
  const checks: SandboxSetupSnapshot["checks"] = [
    {
      id: "platform",
      label: "Credential storage",
      ready: supported,
      detail: supported
        ? "macOS Keychain keeps credentials on this computer."
        : "This setup currently requires macOS Keychain. Use the documented host configuration on other systems.",
    },
    {
      id: "source",
      label: "Circus runtime files",
      ready: source !== null,
      detail: source
        ? "Circus source folder detected."
        : "Select the Circus source folder to prepare the runtime.",
    },
    {
      id: "bridge",
      label: "Task runner",
      ready: bridge !== null,
      detail: bridge
        ? "Task runner found."
        : "Prepare the runtime. Rust 1.99.1 must be installed for this source build.",
    },
    {
      id: "python",
      label: "Model connector",
      ready: python !== null,
      detail: python
        ? "Python 3.11 or later found."
        : "Install Python 3.11 or later, then check again.",
    },
    {
      id: "tls",
      label: "Secure model connection",
      ready: tls.ready,
      detail: tls.ready
        ? "Trusted certificates loaded. Model connections require HTTPS verification."
        : "The model connector could not load trusted certificates. Repair Python's certificate store or the configured certificate bundle before saving.",
    },
    {
      id: "docker",
      label: "Sandbox engine",
      ready: endpoint !== null,
      detail: endpoint
        ? "Local Linux Docker Engine is responding."
        : "Install and open Docker Desktop, or start your local Linux Docker Engine, then check again.",
    },
    {
      id: "image",
      label: "Codex environment",
      ready: image !== null,
      detail: image
        ? "Pinned Codex 0.164.0 image found."
        : "Prepare the runtime to build the Codex environment. The first build needs internet access.",
    },
  ];
  return {
    source,
    bridge,
    python,
    docker,
    endpoint,
    image,
    modelCaFile: tls.caFile,
    trustedTls: tls.ready,
    checks,
  };
}

interface InspectDockerInput {
  docker: string | null;
  endpoints: string[];
  imageRef: string;
  run: RunCommand;
}
async function inspectDocker({ docker, endpoints, imageRef, run }: InspectDockerInput) {
  let endpoint: string | null = null;
  let image: string | null = null;
  if (!docker) return { endpoint, image };
  for (const candidate of endpoints) {
    if (!candidate.startsWith("unix:///")) continue;
    const inspected = await run({
      executable: docker,
      args: ["--host", candidate, "info", "--format", "{{.OSType}}"],
      timeoutMs: 3000,
    });
    if (inspected.ok && inspected.stdout.trim() === "linux") {
      endpoint = candidate;
      break;
    }
  }
  if (!endpoint) return { endpoint, image };
  const inspected = await run({
    executable: docker,
    args: ["--host", endpoint, "image", "inspect", "--format", "{{.Id}}", imageRef],
  });
  if (inspected.ok && /^sha256:[a-f0-9]{64}$/.test(inspected.stdout.trim()))
    image = inspected.stdout.trim();
  return { endpoint, image };
}

interface PythonTrustInput {
  python: string | null;
  platform: string;
  run: RunCommand;
  caFile?: string;
}

export async function inspectPythonTrust(input: PythonTrustInput) {
  const { python, caFile: configuredCa } = input;
  if (!python) return { ready: false, caFile: configuredCa ?? null };
  const verify = async (caFile: string | null) =>
    (
      await input.run({
        executable: python,
        args: [
          "-I",
          "-c",
          "import ssl,sys; context=ssl.create_default_context(cafile=sys.argv[1] or None); raise SystemExit(not context.get_ca_certs())",
          caFile ?? "",
        ],
      })
    ).ok;
  if (configuredCa) return { ready: await verify(configuredCa), caFile: configuredCa };
  if (await verify(null)) return { ready: true, caFile: null };
  const systemCa = "/etc/ssl/cert.pem";
  if (input.platform === "darwin" && (await verify(systemCa)))
    return { ready: true, caFile: systemCa };
  return { ready: false, caFile: null };
}
