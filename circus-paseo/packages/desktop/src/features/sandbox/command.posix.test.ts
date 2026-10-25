import { mkdtemp, readFile, rm } from "node:fs/promises";
import { join } from "node:path";
import { tmpdir } from "node:os";
import { expect, test } from "vitest";
import { runCommand } from "./command.js";

test.skipIf(process.platform === "win32")(
  "timeout kills descendants that inherited command pipes",
  async () => {
    const directory = await mkdtemp(join(tmpdir(), "circus-command-"));
    const pidFile = join(directory, "child.pid");
    let descendant: number | undefined;
    try {
      const result = await runCommand({
        executable: process.execPath,
        args: [
          "-e",
          `
        const { spawn } = require('node:child_process');
        const { writeFileSync } = require('node:fs');
        const child = spawn(process.execPath, ['-e', 'setInterval(() => {}, 1000)'], { stdio: 'inherit' });
        writeFileSync(process.argv[1], String(child.pid));
        setInterval(() => {}, 1000);
      `,
          pidFile,
        ],
        timeoutMs: 500,
      });
      descendant = Number(await readFile(pidFile, "utf8"));
      expect(result.ok).toBe(false);
      await expect
        .poll(
          () => {
            try {
              process.kill(descendant!, 0);
              return false;
            } catch {
              return true;
            }
          },
          { timeout: 3000 },
        )
        .toBe(true);
    } finally {
      if (descendant) {
        try {
          process.kill(descendant, "SIGKILL");
        } catch {
          /* Already reaped. */
        }
      }
      await rm(directory, { recursive: true, force: true });
    }
  },
);

test("a missing executable reports a sanitized failure", async () => {
  expect(
    await runCommand({ executable: "/no-such-circus-command", args: ["sensitive-argument"] }),
  ).toEqual({ ok: false, stdout: "" });
});
