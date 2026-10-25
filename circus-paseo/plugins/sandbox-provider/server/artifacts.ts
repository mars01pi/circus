import { randomUUID } from "node:crypto";
import type { z } from "zod";
import type { artifactSchema } from "../shared/artifacts.js";

// Results live for the daemon lifetime. A bounded store avoids writing guest names to host paths.
export class ArtifactStore {
  private readonly files = new Map<string, Buffer>();
  private bytes = 0;

  async save(files: Record<string, Uint8Array>): Promise<z.infer<typeof artifactSchema>[]> {
    const entries = Object.entries(files);
    const bytes = entries.reduce((sum, [, value]) => sum + value.byteLength, 0);
    if (this.bytes + bytes > 64 * 1024 * 1024 || this.files.size + entries.length > 256) {
      throw new ArtifactError(
        "Artifact storage is full. Download earlier results and restart the sandbox plugin before running another task.",
      );
    }
    const stored = entries.map(([path, value]) => {
      const id = randomUUID();
      this.files.set(id, Buffer.from(value));
      return { id, path, size: value.byteLength };
    });
    this.bytes += bytes;
    return stored;
  }

  async read(id: string, offset: number) {
    const file = this.files.get(id);
    if (!file)
      throw new ArtifactError(
        "Artifact is no longer available. Results expire when the daemon restarts.",
      );
    if (!Number.isSafeInteger(offset) || offset < 0 || offset > file.length)
      throw new ArtifactError("Invalid artifact offset");
    const end = Math.min(offset + 48 * 1024, file.length);
    return {
      base64: file.subarray(offset, end).toString("base64"),
      size: file.length,
      nextOffset: end,
      complete: end === file.length,
    };
  }

  close(): void {
    this.files.clear();
    this.bytes = 0;
  }
}

export class ArtifactError extends Error {
  constructor(message: string) {
    super(message);
    this.name = "ArtifactError";
  }
}
