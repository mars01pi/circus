import { homedir } from "node:os";
import { app, BrowserWindow, dialog, type IpcMainInvokeEvent } from "electron";
import { resolvePaseoHome } from "@getpaseo/server/daemon-control";
import type { DesktopCommandHandler } from "../../settings/desktop-settings-commands.js";
import { runExternalCliTextCommand } from "../../daemon/cli/external.js";
import { createSandboxSetup, SandboxSetupError } from "./setup.js";
import { runCommand } from "./command.js";
import { isTrustedSetupFrame } from "./trusted-frame.js";

export function requireTrustedSetupSender(event: IpcMainInvokeEvent): void {
  const trusted = isTrustedSetupFrame({
    frameUrl: event.senderFrame?.url ?? "",
    isMainFrame: event.senderFrame === event.sender.mainFrame,
    isOwnedWindow: BrowserWindow.fromWebContents(event.sender) !== null,
    packaged: app.isPackaged,
    developmentUrl: process.env.EXPO_DEV_URL ?? "http://localhost:8081",
  });
  if (!trusted) throw new Error("Sandbox settings are available only in the local desktop app.");
}

async function action(operation: () => Promise<unknown>) {
  try {
    return await operation();
  } catch (error) {
    const message =
      error instanceof SandboxSetupError
        ? error.message
        : "Could not complete setup. Check the entered values and local configuration; no raw service diagnostics are displayed.";
    return { ok: false, message };
  }
}

export function createSandboxSetupCommandHandlers(): Record<string, DesktopCommandHandler> {
  const home = resolvePaseoHome(process.env);
  const setup = createSandboxSetup({
    home,
    userHome: homedir(),
    platform: process.platform,
    sourceCandidates: [process.cwd(), __dirname],
    run: runCommand,
    binaryDirectories: [
      "/opt/homebrew/bin",
      "/usr/local/bin",
      "/usr/bin",
      "/Applications/Docker.app/Contents/Resources/bin",
    ],
    request: fetch,
    async reload() {
      await runExternalCliTextCommand(["daemon", "reload", "--home", home]);
    },
  });
  return {
    sandbox_setup_inspect: () => setup.snapshot(),
    sandbox_setup_prepare: () => action(() => setup.prepare()),
    sandbox_setup_test: (args) => action(() => setup.testConnection(args)),
    sandbox_setup_save: (args) => action(() => setup.save(args)),
    sandbox_setup_mcp_save: (args) => action(() => setup.saveGithub(args)),
    sandbox_setup_select_source: async () => {
      const selection = await dialog.showOpenDialog({
        title: "Select Circus source folder",
        properties: ["openDirectory"],
      });
      if (!selection.canceled && selection.filePaths[0])
        await setup.selectSource(selection.filePaths[0]);
      return setup.snapshot();
    },
  };
}
