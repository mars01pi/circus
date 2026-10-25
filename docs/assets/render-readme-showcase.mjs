#!/usr/bin/env node

// Run from the repository root after installing the circus-paseo dependencies.

import { createRequire } from "node:module";
import path from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const assetsDirectory = path.dirname(fileURLToPath(import.meta.url));
const repositoryRoot = path.resolve(assetsDirectory, "..", "..");
const requireFromPaseo = createRequire(
  pathToFileURL(path.join(repositoryRoot, "circus-paseo", "package.json")),
);
const { chromium } = requireFromPaseo("playwright");

const exports = [
  ["#desktop-overview", "circus-desktop-overview.png"],
  ["#security-flow", "circus-security-flow.png"],
];

const browser = await chromium.launch({ headless: true });
try {
  const page = await browser.newPage({
    colorScheme: "light",
    deviceScaleFactor: 1,
    viewport: { width: 1920, height: 1200 },
  });
  await page.goto(
    pathToFileURL(path.join(assetsDirectory, "readme-showcase.html")).href,
  );
  await page.waitForFunction(() =>
    [...document.images].every((image) => image.complete),
  );

  for (const [selector, filename] of exports) {
    await page.locator(selector).screenshot({
      animations: "disabled",
      path: path.join(assetsDirectory, filename),
    });
  }
} finally {
  await browser.close();
}
