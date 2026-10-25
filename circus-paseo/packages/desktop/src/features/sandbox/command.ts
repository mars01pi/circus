import { spawn } from "node:child_process";
import { once } from "node:events";

export interface CommandInput {
  executable: string;
  args: string[];
  input?: string;
  cwd?: string;
  timeoutMs?: number;
}

export interface CommandResult {
  ok: boolean;
  stdout: string;
}

export type RunCommand = (input: CommandInput) => Promise<CommandResult>;

/** Never retain stderr or embed command arguments in errors: either may contain credentials. */
export const runCommand: RunCommand = async (input) => {
  const child = spawn(input.executable, input.args, {
    cwd: input.cwd,
    shell: false,
    detached: process.platform !== "win32",
    stdio: "pipe",
    env: { PATH: process.env.PATH, HOME: process.env.HOME, TMPDIR: process.env.TMPDIR },
  });
  let stdout = "";
  let outputBytes = 0;
  let failed = false;
  function stop(): void {
    failed = true;
    if (process.platform === "win32" || child.pid === undefined) {
      child.kill("SIGKILL");
      return;
    }
    try {
      process.kill(-child.pid, "SIGKILL");
    } catch (error) {
      if (!(error instanceof Error && "code" in error && error.code === "ESRCH"))
        child.kill("SIGKILL");
    }
  }
  const timer = setTimeout(stop, input.timeoutMs ?? 10_000);
  child.stdout.on("data", (bytes: Buffer) => {
    outputBytes += bytes.length;
    if (outputBytes > 64 * 1024) stop();
    else stdout += bytes.toString("utf8");
  });
  child.stderr.resume();
  child.stdin.on("error", () => {});
  // Subscribe before writing so an early spawn failure is always observed.
  const closed = once(child, "close");
  child.stdin.end(input.input);
  try {
    const [code] = await closed;
    return { ok: code === 0 && !failed, stdout };
  } catch {
    return { ok: false, stdout: "" };
  } finally {
    clearTimeout(timer);
  }
};
