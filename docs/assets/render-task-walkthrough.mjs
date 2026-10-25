#!/usr/bin/env node

// Run from the repository root after installing the circus-paseo dependencies.

import { spawnSync } from "node:child_process";
import { createRequire } from "node:module";
import { mkdtempSync, rmSync, statSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const assetsDirectory = path.dirname(fileURLToPath(import.meta.url));
const repositoryRoot = path.resolve(assetsDirectory, "..", "..");
const paseoRoot = path.join(repositoryRoot, "circus-paseo");
const framesDirectory = mkdtempSync(
  path.join(tmpdir(), "circus-task-walkthrough-"),
);
const requireFromPaseo = createRequire(
  pathToFileURL(path.join(paseoRoot, "package.json")),
);
const sharp = requireFromPaseo("sharp");

const frames = [
  {
    filename: "01-open-task.png",
    delay: 1400,
    title: "打开 Sandbox task",
    detail: "从 Paseo 工作区进入安全任务表单。",
  },
  {
    filename: "02-describe-task.png",
    delay: 1500,
    title: "描述任务与期限",
    detail: "任务说明和 300 秒期限随请求一起提交。",
  },
  {
    filename: "03-select-files.png",
    delay: 1700,
    title: "选择输入与导出范围",
    detail: "只导入 answer.py，只返回 answer.py 与 result.txt。",
  },
  {
    filename: "04-permission-default.png",
    delay: 1400,
    title: "工具默认不授权",
    detail: "真实凭据和固定仓库参数仍留在宿主。",
  },
  {
    filename: "05-grant-tool.png",
    delay: 1900,
    title: "只为本次任务授权 issue_read",
    detail: "已授权 1 个服务；仓库固定为 demo-team/demo-project。",
  },
  {
    filename: "06-start-task.png",
    delay: 1500,
    title: "提交并打开任务进度",
    detail: "Paseo 创建任务，并进入受控 provider 回放。",
  },
  {
    filename: "07-prepare.png",
    delay: 1500,
    title: "Paseo 展示准备阶段",
    detail: "此帧验证阶段事件呈现；真实 Rust 和 Docker 证据见验收记录。",
  },
  {
    filename: "08-run.png",
    delay: 2100,
    title: "Paseo 展示运行阶段",
    detail: "受控 fixture 返回批准工具与测试结果，不代表真实模型执行。",
  },
  {
    filename: "09-export.png",
    delay: 1400,
    title: "只导出指定产物",
    detail: "界面展示导出阶段；结果不会自动覆盖宿主项目。",
  },
  {
    filename: "10-cleanup.png",
    delay: 1600,
    title: "任务结束后显示清理",
    detail: "cleanup 是结果契约的一部分，不能用进程退出代替。",
  },
  {
    filename: "11-result.png",
    delay: 2600,
    title: "预览、下载并确认回收",
    detail: "两个产物可取回，界面显示 Sandbox cleanup: confirmed。",
  },
];
const frameWidth = 960;
const captionHeight = 104;

function escapeXml(value) {
  return value
    .replaceAll("&", "&amp;")
    .replaceAll("<", "&lt;")
    .replaceAll(">", "&gt;")
    .replaceAll('"', "&quot;")
    .replaceAll("'", "&apos;");
}

function captionSvg(frame, index) {
  const progressWidth = Math.round(((index + 1) / frames.length) * frameWidth);
  const step = `${String(index + 1).padStart(2, "0")} / ${String(frames.length).padStart(2, "0")}`;
  return Buffer.from(`
    <svg xmlns="http://www.w3.org/2000/svg" width="${frameWidth}" height="${captionHeight}">
      <rect width="${frameWidth}" height="${captionHeight}" fill="#0f172a"/>
      <text x="24" y="34" fill="#5eead4" font-family="ui-monospace, SFMono-Regular, Menlo, monospace" font-size="18" font-weight="700">${step}</text>
      <text x="124" y="33" fill="#f8fafc" font-family="Inter, PingFang SC, Noto Sans CJK SC, sans-serif" font-size="20" font-weight="700">${escapeXml(frame.title)}</text>
      <text x="124" y="68" fill="#cbd5e1" font-family="Inter, PingFang SC, Noto Sans CJK SC, sans-serif" font-size="14">${escapeXml(frame.detail)}</text>
      <text x="936" y="33" text-anchor="end" fill="#94a3b8" font-family="ui-monospace, SFMono-Regular, Menlo, monospace" font-size="11">LIVE PASEO UI · CONTROLLED PROVIDER FIXTURE</text>
      <rect x="0" y="100" width="${progressWidth}" height="4" fill="#2dd4bf"/>
    </svg>
  `);
}

async function renderFrame(frame, index) {
  const ui = await sharp(path.join(framesDirectory, frame.filename))
    .resize({ width: frameWidth })
    .png()
    .toBuffer();
  const { height } = await sharp(ui).metadata();
  if (!height) throw new Error(`Missing frame height for ${frame.filename}`);
  return sharp({
    create: {
      width: frameWidth,
      height: height + captionHeight,
      channels: 4,
      background: "#0f172a",
    },
  })
    .composite([
      { input: ui, left: 0, top: 0 },
      { input: captionSvg(frame, index), left: 0, top: height },
    ])
    .png()
    .toBuffer();
}

try {
  const npm = process.platform === "win32" ? "npm.cmd" : "npm";
  const run = spawnSync(
    npm,
    [
      "run",
      "test:e2e",
      "--workspace=@getpaseo/app",
      "--",
      "e2e/browser/sandbox-task-controlled.spec.ts",
    ],
    {
      cwd: paseoRoot,
      env: {
        ...process.env,
        CIRCUS_README_DEMO_DIR: framesDirectory,
        E2E_WORKERS: "1",
      },
      stdio: "inherit",
    },
  );
  if (run.error) throw run.error;
  if (run.status !== 0)
    throw new Error(`Paseo walkthrough failed with status ${run.status}`);

  const animationFrames = await Promise.all(frames.map(renderFrame));
  const delays = frames.map(({ delay }) => delay);

  await sharp(animationFrames, { join: { animated: true } })
    .webp({ quality: 82, effort: 6, loop: 0, delay: delays })
    .toFile(path.join(assetsDirectory, "circus-task-walkthrough.webp"));

  await sharp(animationFrames, { join: { animated: true } })
    .gif({
      colours: 128,
      dither: 0.35,
      effort: 8,
      interFrameMaxError: 8,
      interPaletteMaxError: 8,
      loop: 0,
      delay: delays,
    })
    .toFile(path.join(assetsDirectory, "circus-task-walkthrough.gif"));

  const posterFrames = await Promise.all(
    [0, 2, 4, 10].map((index) =>
      sharp(animationFrames[index]).resize({ width: 480 }).png().toBuffer(),
    ),
  );
  await sharp(posterFrames, {
    join: { across: 2, shim: 8, background: "#0f172a" },
  }).toFile(path.join(assetsDirectory, "circus-task-walkthrough-poster.png"));

  for (const filename of [
    "circus-task-walkthrough.webp",
    "circus-task-walkthrough.gif",
    "circus-task-walkthrough-poster.png",
  ]) {
    const bytes = statSync(path.join(assetsDirectory, filename)).size;
    console.log(`${filename}: ${(bytes / 1024 / 1024).toFixed(2)} MiB`);
  }
} finally {
  rmSync(framesDirectory, { recursive: true, force: true });
}
