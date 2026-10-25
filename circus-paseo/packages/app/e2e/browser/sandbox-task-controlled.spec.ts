import {
  chmodSync,
  existsSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  rmSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { expect, test, type Page } from "../support/fixtures";
import { gotoAppShell } from "../support/helpers/app";
import { openCommandCenter } from "../support/helpers/command-center";
import { seedWorkspace } from "../support/helpers/seed-client";
import { getServerId } from "../support/helpers/server-id";
import { switchWorkspaceViaSidebar } from "../support/helpers/workspace-ui";

const fixtureDirectory = mkdtempSync(join(tmpdir(), "paseo-sandbox-stdio-fixture-"));
const bridge = join(fixtureDirectory, "controlled-bridge");
const runnerConfig = join(fixtureDirectory, "runner.json");
const marker = `${runnerConfig}.seen`;
const demoDirectory = process.env.CIRCUS_README_DEMO_DIR;

if (demoDirectory) mkdirSync(demoDirectory, { recursive: true });
writeFileSync(
  runnerConfig,
  `${JSON.stringify(
    {
      mcpConnections: [
        {
          id: "github-demo",
          tools: [
            {
              name: "issue_read",
              fixedArguments: { owner: "demo-team", repo: "demo-project" },
            },
          ],
        },
      ],
    },
    null,
    2,
  )}\n`,
  "utf8",
);
writeFileSync(
  bridge,
  `#!/usr/bin/env node
const fs = require("node:fs");
const readline = require("node:readline");
const write = (frame) => process.stdout.write(JSON.stringify(frame) + "\\n");
const wait = (milliseconds) => new Promise((resolve) => setTimeout(resolve, milliseconds));
const demo = ${Boolean(demoDirectory)};
write({ v: 1, type: "ready", maxFrameBytes: 12 * 1024 * 1024 });
readline.createInterface({ input: process.stdin }).on("line", async (line) => {
  const frame = JSON.parse(line);
  if (frame.type !== "start") return;
  const configIndex = process.argv.indexOf("--config");
  const configPath = process.argv[configIndex + 1];
  fs.writeFileSync(configPath + ".seen", JSON.stringify(frame.request));
  const waitForDemoCheckpoint = async (name) => {
    if (!demo) return;
    const signal = configPath + ".demo-" + name;
    while (!fs.existsSync(signal)) await wait(25);
  };
  let seq = 0;
  const event = (payload) => {
    write({ v: 1, type: "event", requestId: frame.requestId, taskId: "fixture-task", seq: ++seq, event: payload });
  };
  write({ v: 1, type: "accepted", requestId: frame.requestId, taskId: "fixture-task" });
  event({ kind: "phase", phase: "preparing" });
  await waitForDemoCheckpoint("preparing");
  event({ kind: "phase", phase: "running" });
  event({
    kind: "text_delta",
    text: "Approved issue_read returned demo-team/demo-project#42. ",
  });
  event({ kind: "text_delta", text: "Two Python assertions passed." });
  await waitForDemoCheckpoint("running");
  event({ kind: "phase", phase: "exporting" });
  await waitForDemoCheckpoint("exporting");
  event({ kind: "phase", phase: "cleaning" });
  await waitForDemoCheckpoint("cleaning");
  write({
    v: 1,
    type: "result",
    requestId: frame.requestId,
    taskId: "fixture-task",
    result: {
      status: "succeeded",
      cleanup: "confirmed",
      artifacts: [
        { path: "answer.py", data: Buffer.from("def add(a, b):\\n    return a + b\\n").toString("base64") },
        { path: "result.txt", data: "NDIK" }
      ]
    }
  });
});
`,
  "utf8",
);
chmodSync(bridge, 0o755);

test.use({
  e2eDaemonConfig: {
    version: 1,
    agents: {
      providers: {
        sandbox: {
          enabled: true,
          command: [bridge],
          env: {
            CIRCUS_RUNNER_CONFIG: runnerConfig,
            CIRCUS_SANDBOX_STATE_DIR: join(fixtureDirectory, "state"),
          },
        },
      },
    },
  },
});

test.afterAll(() => {
  rmSync(fixtureDirectory, { recursive: true, force: true });
});

async function captureDemoFrame(page: Page, filename: string) {
  if (!demoDirectory) return;
  await page.screenshot({
    animations: "disabled",
    path: join(demoDirectory, filename),
  });
}

function releaseDemoCheckpoint(name: string) {
  if (demoDirectory) writeFileSync(`${runnerConfig}.demo-${name}`, "continue\n", "utf8");
}

async function openSandboxTask(page: Page, workspaceId: string, beforeOpen?: () => Promise<void>) {
  await gotoAppShell(page);
  await switchWorkspaceViaSidebar({ page, serverId: getServerId(), workspaceId });
  const command = await openCommandCenter(page);
  await command.getByTestId("command-center-input").fill("New sandbox task");
  await beforeOpen?.();
  await command.getByRole("button", { name: "New sandbox task", exact: true }).click();
  await expect(page.getByText(/Sandbox sessions continue completed turns only/)).toBeVisible();
}

test("creates through agents.create with a controlled stdio provider fixture", async ({
  page,
}, info) => {
  const workspace = await seedWorkspace({
    repoPrefix: "circus-demo-",
    title: "main",
    repo: {
      originUrl: "https://github.com/demo-team/circus-demo.git",
      paseoConfig: { worktree: { setup: "true" } },
    },
  });
  try {
    await workspace.client.renameProject(workspace.projectId, "Circus Demo");
    await page.setViewportSize({ width: 1280, height: 720 });
    await openSandboxTask(page, workspace.workspaceId, () =>
      captureDemoFrame(page, "01-open-task.png"),
    );

    const prompt = page.getByLabel("Sandbox prompt");
    await prompt.fill("修复 answer.py 中的 add 函数，运行断言，并将结果 42 写入 result.txt。");
    await captureDemoFrame(page, "02-describe-task.png");

    const chooser = page.waitForEvent("filechooser");
    await page.getByRole("button", { name: "Choose files", exact: true }).click();
    const inputContents = "def add(a, b): return a - b\n";
    await (
      await chooser
    ).setFiles([
      { name: "answer.py", mimeType: "text/x-python", buffer: Buffer.from(inputContents) },
    ]);
    await expect(
      page.getByText(`answer.py · ${Buffer.byteLength(inputContents)} bytes`),
    ).toBeVisible();
    const outputPaths = page.getByLabel("Output file paths");
    await outputPaths.fill("answer.py\nresult.txt");
    if (demoDirectory) {
      await outputPaths.evaluate((element) => element.scrollIntoView({ block: "center" }));
    } else {
      await outputPaths.scrollIntoViewIfNeeded();
    }
    await captureDemoFrame(page, "03-select-files.png");

    const toolPermission = page.getByRole("switch", { name: "github-demo / issue_read" });
    const refreshTools = page.getByRole("button", { name: "Refresh", exact: true }).last();
    await refreshTools.scrollIntoViewIfNeeded();
    await expect
      .poll(
        async () => {
          if (await toolPermission.isVisible()) return true;
          if (await refreshTools.isEnabled()) await refreshTools.click();
          return toolPermission.isVisible();
        },
        { timeout: 15_000 },
      )
      .toBe(true);
    await toolPermission.scrollIntoViewIfNeeded();
    await expect(toolPermission).not.toBeChecked();
    await captureDemoFrame(page, "04-permission-default.png");
    await toolPermission.click();
    await expect(toolPermission).toBeChecked();
    const allowedSummary = page.getByText("1 service(s) allowed for this task", { exact: true });
    await expect(allowedSummary).toBeVisible();
    const startTask = page.getByRole("button", { name: "Start task", exact: true });
    await startTask.scrollIntoViewIfNeeded();
    await captureDemoFrame(page, "05-grant-tool.png");

    await startTask.click();
    await expect(page.getByText("Sandbox task created.", { exact: true })).toBeVisible();
    await expect.poll(() => existsSync(marker)).toBe(true);
    const openTask = page.getByRole("button", { name: "Open task", exact: true });
    await openTask.scrollIntoViewIfNeeded();
    await captureDemoFrame(page, "06-start-task.png");
    expect(JSON.parse(readFileSync(marker, "utf8"))).toMatchObject({
      prompt: "修复 answer.py 中的 add 函数，运行断言，并将结果 42 写入 result.txt。",
      inputFiles: [
        {
          path: "answer.py",
          data: Buffer.from(inputContents).toString("base64"),
        },
      ],
      outputPaths: ["answer.py", "result.txt"],
      mcpGrants: [{ connectionId: "github-demo", tools: ["issue_read"] }],
    });
    await openTask.click();
    await expect(page.getByText("Preparing sandbox…", { exact: true })).toBeVisible();
    await captureDemoFrame(page, "07-prepare.png");
    releaseDemoCheckpoint("preparing");
    await expect(page.getByText("Running in sandbox…", { exact: true })).toBeVisible();
    await expect(
      page.getByText(/Approved issue_read returned demo-team\/demo-project#42\./),
    ).toBeVisible();
    await expect(page.getByText(/Two Python assertions passed\./)).toBeVisible();
    await captureDemoFrame(page, "08-run.png");
    releaseDemoCheckpoint("running");
    await expect(page.getByText("Exporting requested files…", { exact: true })).toBeVisible();
    await captureDemoFrame(page, "09-export.png");
    releaseDemoCheckpoint("exporting");
    await expect(page.getByText("Cleaning up sandbox…", { exact: true })).toBeVisible();
    await captureDemoFrame(page, "10-cleanup.png");
    releaseDemoCheckpoint("cleaning");
    await expect(page.getByText("Sandbox cleanup: confirmed", { exact: true })).toBeVisible();
    await expect(page.getByText("answer.py · 32 bytes", { exact: true })).toBeVisible();
    await expect(page.getByText("result.txt · 3 bytes", { exact: true })).toBeVisible();
    await page.getByRole("button", { name: "Preview", exact: true }).last().click();
    await expect(page.getByText("42\n", { exact: true })).toBeVisible();
    const downloadEvent = page.waitForEvent("download");
    await page.getByRole("button", { name: "Download", exact: true }).last().click();
    const download = await downloadEvent;
    expect(download.suggestedFilename()).toBe("result.txt");
    const downloadPath = await download.path();
    if (!downloadPath) throw new Error("Expected Playwright to retain the downloaded artifact");
    expect(readFileSync(downloadPath, "utf8")).toBe("42\n");
    await expect(page.getByText("Download started.", { exact: true })).toBeVisible();
    await captureDemoFrame(page, "11-result.png");
    await page.screenshot({ path: info.outputPath("sandbox-result-card-preview.png") });
  } finally {
    await workspace.cleanup();
  }
});
