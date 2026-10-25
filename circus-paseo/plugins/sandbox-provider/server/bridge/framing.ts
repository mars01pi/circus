import { MAX_FRAME_BYTES } from "./protocol.js";

export type BridgeErrorCode =
  | "configuration"
  | "invalid_request"
  | "protocol"
  | "transport"
  | "timeout"
  | "closed"
  | "backpressure";

export class CircusBridgeError extends Error {
  readonly cleanup = "unknown";
  constructor(
    public readonly code: BridgeErrorCode,
    message: string,
  ) {
    super(message);
    this.name = "CircusBridgeError";
  }
}

export class FrameDecoder {
  private buffer = Buffer.alloc(4096);
  private bytes = 0;

  push(chunk: Buffer, accept: (frame: unknown, size: number) => void): void {
    let start = 0;
    while (start < chunk.length) {
      const newline = chunk.indexOf(10, start);
      const end = newline === -1 ? chunk.length : newline;
      const part = chunk.subarray(start, end);
      const needed = this.bytes + part.length;
      if (needed > MAX_FRAME_BYTES) {
        throw new CircusBridgeError("protocol", "Bridge frame exceeds byte limit");
      }
      if (needed > this.buffer.length) {
        const capacity = Math.min(MAX_FRAME_BYTES, Math.max(needed, this.buffer.length * 2));
        const grown = Buffer.allocUnsafe(capacity);
        this.buffer.copy(grown, 0, 0, this.bytes);
        this.buffer = grown;
      }
      part.copy(this.buffer, this.bytes);
      this.bytes = needed;
      if (newline === -1) return;
      const bytes = this.buffer.subarray(0, this.bytes);
      this.bytes = 0;
      let frame: unknown;
      try {
        const text = new TextDecoder("utf-8", { fatal: true }).decode(bytes);
        frame = JSON.parse(text);
      } catch {
        throw new CircusBridgeError("protocol", "Bridge sent invalid UTF-8 or JSON");
      }
      accept(frame, bytes.length);
      start = newline + 1;
    }
  }

  end(): void {
    if (this.bytes !== 0)
      throw new CircusBridgeError("protocol", "Bridge stream ended inside a frame");
  }
}

export class Deferred<T> {
  readonly promise: Promise<T>;
  resolve!: (value: T) => void;
  reject!: (error: CircusBridgeError) => void;
  constructor() {
    this.promise = new Promise<T>((resolve, reject) => {
      this.resolve = resolve;
      this.reject = reject;
    });
    // Internal results can settle before the public handle has reached its caller.
    void this.promise.catch(() => {});
  }
}
