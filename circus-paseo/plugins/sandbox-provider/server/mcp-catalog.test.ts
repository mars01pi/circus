import { mkdtemp, rm, writeFile, symlink } from "node:fs/promises";
import { join } from "node:path";
import { tmpdir } from "node:os";
import { afterEach, expect, test } from "vitest";
import { readMcpCatalog } from "./mcp-catalog.js";

const directories: string[] = [];
afterEach(async () => {
  await Promise.all(
    directories.splice(0).map((path) => rm(path, { recursive: true, force: true })),
  );
});

async function file(contents: string): Promise<string> {
  const directory = await mkdtemp(join(tmpdir(), "circus-mcp-catalog-"));
  directories.push(directory);
  const path = join(directory, "runner.json");
  await writeFile(path, contents);
  return path;
}

test("shows only host-approved tools and resources, without resolving credentials", async () => {
  const path = await file(
    JSON.stringify({
      model: { apiKeyFile: "/private/model-secret" },
      mcpConnections: [
        {
          id: "github",
          url: "https://private-mcp.example/mcp",
          headersKeychain: { service: "secret-service", account: "secret-account" },
          tools: [
            {
              name: "issue_read",
              allowedArguments: ["issue_number"],
              fixedArguments: { owner: "demo", repo: "approved", method: "get" },
            },
          ],
        },
      ],
    }),
  );
  expect(await readMcpCatalog(path)).toEqual({
    status: "ready",
    connections: [
      {
        id: "github",
        tools: [
          {
            name: "issue_read",
            fixedArguments: { owner: "demo", repo: "approved", method: "get" },
          },
        ],
      },
    ],
  });
  expect(await readMcpCatalog(await file("{}"))).toEqual({ status: "ready", connections: [] });
});

test("reports configuration failures without returning private contents or paths", async () => {
  const invalid = await file("private-secret-invalid-json");
  const oversized = await file("private-secret".repeat(10_000));
  for (const path of [invalid, oversized, join(tmpdir(), "missing-circus-config")]) {
    expect(await readMcpCatalog(path)).toEqual({
      status: "unavailable",
      message: "Cannot read sandbox MCP permissions. Check the host configuration.",
    });
  }
  expect(await readMcpCatalog(null)).toEqual({
    status: "unavailable",
    message: "Configure the sandbox provider on its desktop host, then refresh its status.",
  });
});

test.skipIf(process.platform === "win32")("refuses symlink configuration files", async () => {
  const target = await file("{}");
  const link = `${target}.link`;
  await symlink(target, link);
  expect((await readMcpCatalog(link)).status).toBe("unavailable");
});
