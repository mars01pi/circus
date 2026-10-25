import { describe, expect, it } from "vitest";
import { FrameDecoder } from "./framing.js";
import { MAX_FRAME_BYTES, TaskRequestSchema } from "./protocol.js";

function discardFrame(): void {}

describe("stdio framing", () => {
  it("decodes fragmented UTF-8 and several complete frames", () => {
    const frames: unknown[] = [];
    const decoder = new FrameDecoder();
    const input = Buffer.from('{"text":"你好"}\n{"v":1}\n');
    for (const byte of input) decoder.push(Buffer.from([byte]), (frame) => frames.push(frame));
    decoder.end();
    expect(frames).toEqual([{ text: "你好" }, { v: 1 }]);
  });

  it("rejects overflow before a newline and rejects a partial final frame", () => {
    const decoder = new FrameDecoder();
    expect(() => decoder.push(Buffer.alloc(MAX_FRAME_BYTES + 1), discardFrame)).toThrow(
      "byte limit",
    );
    const partial = new FrameDecoder();
    partial.push(Buffer.from('{"v":'), () => {});
    expect(() => partial.end()).toThrow("inside a frame");
  });

  it.each([Buffer.from([0xff, 10]), Buffer.from("not json\n"), Buffer.from("\n")])(
    "rejects malformed input",
    (input) => {
      expect(() => new FrameDecoder().push(input, discardFrame)).toThrow("UTF-8 or JSON");
    },
  );
});

describe("task request boundary", () => {
  const request = { prompt: "fix", inputFiles: [], outputPaths: ["out.txt"], timeoutMs: 1000 };
  it.each(["../secret", "/root", "C:secret", "a\\b", "a//b", "a/./b", "", "a\0b"])(
    "rejects host or ambiguous path %s",
    (path) => {
      expect(TaskRequestSchema.safeParse({ ...request, outputPaths: [path] }).success).toBe(false);
    },
  );
  it("rejects noncanonical base64 and duplicate paths", () => {
    expect(
      TaskRequestSchema.safeParse({ ...request, inputFiles: [{ path: "a", data: "YWJj!" }] })
        .success,
    ).toBe(false);
    expect(TaskRequestSchema.safeParse({ ...request, outputPaths: ["a", "a"] }).success).toBe(
      false,
    );
  });
  it("rejects ambient execution authority", () => {
    expect(
      TaskRequestSchema.safeParse({ ...request, command: "sh", cwd: "/", apiKey: "key" }).success,
    ).toBe(false);
  });
});
