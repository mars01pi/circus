import { afterEach, expect, test } from "vitest";
import { ArtifactStore } from "./artifacts.js";

const store = new ArtifactStore();
afterEach(() => store.close());

test("exports exact bytes without using guest paths as host paths", async () => {
  const artifacts = await store.save({ "nested/result.bin": new Uint8Array([0, 255, 42]) });
  expect(artifacts.map(({ path, size }) => ({ path, size }))).toEqual([
    { path: "nested/result.bin", size: 3 },
  ]);
  expect(await store.read(artifacts[0]!.id, 0)).toEqual({
    base64: "AP8q",
    size: 3,
    nextOffset: 3,
    complete: true,
  });
  await expect(store.read("../../etc/passwd", 0)).rejects.toThrow(
    "Artifact is no longer available",
  );
  await expect(store.read(artifacts[0]!.id, 4)).rejects.toThrow("Invalid artifact offset");
});
