export function isTrustedSetupFrame(input: {
  frameUrl: string;
  isMainFrame: boolean;
  isOwnedWindow: boolean;
  packaged: boolean;
  developmentUrl: string;
}): boolean {
  if (!input.isMainFrame || !input.isOwnedWindow) return false;
  try {
    const url = new URL(input.frameUrl);
    if (input.packaged) return url.protocol === "paseo:" && url.host === "app";
    const development = new URL(input.developmentUrl);
    return url.origin === development.origin;
  } catch {
    return false;
  }
}
