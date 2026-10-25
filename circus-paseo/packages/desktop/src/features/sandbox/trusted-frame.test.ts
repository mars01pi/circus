import { expect, test } from "vitest";
import { isTrustedSetupFrame } from "./trusted-frame.js";

const owned = {
  isMainFrame: true,
  isOwnedWindow: true,
  packaged: true,
  developmentUrl: "http://localhost:8081",
};

test("only the owned local application main frame can manage sandbox credentials", () => {
  expect(isTrustedSetupFrame({ ...owned, frameUrl: "paseo://app/settings" })).toBe(true);
  for (const frameUrl of [
    "https://example.com",
    "paseo://untrusted/settings",
    "file:///tmp/page.html",
    "http://localhost:8081",
    "not a URL",
  ]) {
    expect(isTrustedSetupFrame({ ...owned, frameUrl })).toBe(false);
  }
  expect(
    isTrustedSetupFrame({ ...owned, frameUrl: "paseo://app/settings", isMainFrame: false }),
  ).toBe(false);
  expect(
    isTrustedSetupFrame({ ...owned, frameUrl: "paseo://app/settings", isOwnedWindow: false }),
  ).toBe(false);
});

test("development accepts only the exact configured origin, never remote pages or other ports", () => {
  const development = { ...owned, packaged: false };
  expect(isTrustedSetupFrame({ ...development, frameUrl: "http://localhost:8081/settings" })).toBe(
    true,
  );
  for (const frameUrl of [
    "http://localhost:8082",
    "http://localhost.evil:8081",
    "https://localhost:8081",
    "http://127.0.0.1:8081",
  ]) {
    expect(isTrustedSetupFrame({ ...development, frameUrl })).toBe(false);
  }
});
