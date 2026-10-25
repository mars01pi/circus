import { expect, test, type Page } from "../support/fixtures";
import { gotoAppShell } from "../support/helpers/app";
import { openCommandCenter } from "../support/helpers/command-center";
import { seedWorkspace } from "../support/helpers/seed-client";
import { getServerId } from "../support/helpers/server-id";
import { switchWorkspaceViaSidebar } from "../support/helpers/workspace-ui";

async function openSandboxTask(page: Page, workspaceId: string) {
  await gotoAppShell(page);
  await switchWorkspaceViaSidebar({
    page,
    serverId: getServerId(),
    workspaceId,
  });
  const command = await openCommandCenter(page);
  await command.getByTestId("command-center-input").fill("New sandbox task");
  await command.getByRole("button", { name: "New sandbox task", exact: true }).click();
  await expect(page.getByText(/Sandbox sessions continue completed turns only/)).toBeVisible();
}

test("sandbox form distinguishes configuration failures and preserves the draft across layouts", async ({
  page,
}, info) => {
  const workspace = await seedWorkspace({ repoPrefix: "sandbox-form-" });
  try {
    await page.setViewportSize({ width: 1200, height: 900 });
    await openSandboxTask(page, workspace.workspaceId);
    await page.getByRole("button", { name: "Start task", exact: true }).click();
    await expect(
      page.getByText("Enter a prompt of at most 64 KiB.", { exact: true }),
    ).toBeVisible();
    await page.getByLabel("Sandbox prompt").fill("Read input.txt and write result.txt");
    const chooser = page.waitForEvent("filechooser");
    await page.getByRole("button", { name: "Choose files", exact: true }).click();
    await (
      await chooser
    ).setFiles([{ name: "input.txt", mimeType: "text/plain", buffer: Buffer.from("42\n") }]);
    await expect(page.getByText("input.txt · 3 bytes", { exact: true })).toBeVisible();
    await page.getByLabel("Output file paths").fill("../escape");
    await page.getByRole("button", { name: "Start task", exact: true }).click();
    await expect(page.getByText(/project-relative/)).toBeVisible();
    await page.getByLabel("Output file paths").fill("result.txt");
    await page.getByLabel("Deadline (seconds)").fill("90");

    await page.setViewportSize({ width: 390, height: 844 });
    await expect(page.getByLabel("Sandbox prompt")).toHaveValue(
      "Read input.txt and write result.txt",
    );
    await expect(page.getByLabel("Output file paths")).toHaveValue("result.txt");
    await expect(page.getByLabel("Deadline (seconds)")).toHaveValue("90");
    await expect(page.getByText("input.txt · 3 bytes", { exact: true })).toBeVisible();
    await page.setViewportSize({ width: 1200, height: 900 });

    await page.getByRole("button", { name: "Start task", exact: true }).click();
    const configurationError = page.locator('[role="alert"]').filter({ hasText: /sandbox/i });
    await expect(configurationError).toHaveText("Provider sandbox is not configured");
    await expect(configurationError).not.toContainText("Expected config.provider");
    await expect(page.getByLabel("Sandbox prompt")).toHaveValue(
      "Read input.txt and write result.txt",
    );
    await expect(page.getByRole("button", { name: "Start task", exact: true })).toBeEnabled();
    await page.screenshot({ path: info.outputPath("sandbox-task-wide.png") });
    await page.setViewportSize({ width: 390, height: 844 });
    await expect(page.getByLabel("Sandbox prompt")).toHaveValue(
      "Read input.txt and write result.txt",
    );
    await expect(page.getByText("input.txt · 3 bytes", { exact: true })).toBeVisible();
    await page.screenshot({ path: info.outputPath("sandbox-task-compact.png") });
  } finally {
    await workspace.cleanup();
  }
});
