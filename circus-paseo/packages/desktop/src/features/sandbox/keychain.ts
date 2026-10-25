import type { RunCommand } from "./command.js";

export interface KeychainReference {
  service: string;
  account: string;
}

function quote(value: string): string {
  if (/[\r\n]/.test(value) || value.includes("\0"))
    throw new Error("Credentials cannot contain line breaks.");
  return `"${value.replaceAll("\\", "\\\\").replaceAll('"', '\\"')}"`;
}

export function createKeychain(run: RunCommand) {
  return {
    async put(reference: KeychainReference, value: string): Promise<boolean> {
      // security's interactive parser receives the password through stdin, never process argv.
      const command = [
        "add-generic-password",
        "-s",
        quote(reference.service),
        "-a",
        quote(reference.account),
        "-w",
        quote(value),
      ].join(" ");
      const written = await run({
        executable: "/usr/bin/security",
        args: ["-i"],
        input: `${command}\n`,
      });
      if (!written.ok) return false;
      // Interactive security can exit successfully after a failed subcommand. Read back exactly.
      return (await this.get(reference)) === value;
    },
    async get(reference: KeychainReference): Promise<string | null> {
      const result = await run({
        executable: "/usr/bin/security",
        args: ["find-generic-password", "-s", reference.service, "-a", reference.account, "-w"],
      });
      return result.ok ? result.stdout.replace(/[\r\n]+$/, "") : null;
    },
    async remove(reference: KeychainReference): Promise<void> {
      await run({
        executable: "/usr/bin/security",
        args: ["delete-generic-password", "-s", reference.service, "-a", reference.account],
      });
    },
  };
}
