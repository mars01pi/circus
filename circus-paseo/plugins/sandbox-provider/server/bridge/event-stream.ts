import type { CircusTaskEvent } from "./protocol.js";
import { CircusBridgeError, Deferred } from "./framing.js";

interface Entry {
  event: CircusTaskEvent;
  bytes: number;
}

export class EventStream implements AsyncIterable<CircusTaskEvent> {
  private queue: Entry[] = [];
  private bytes = 0;
  private ended = false;
  private error: CircusBridgeError | null = null;
  private wake = new Deferred<void>();
  private claimed = false;

  push(entry: Entry): void {
    if (this.ended) return;
    if (this.queue.length >= 256 || this.bytes + entry.bytes > 8 * 1024 * 1024) {
      throw new CircusBridgeError("backpressure", "Task event consumer exceeded its bounded queue");
    }
    this.queue.push(entry);
    this.bytes += entry.bytes;
    this.wake.resolve();
  }

  end(error: CircusBridgeError | null = null): void {
    this.ended = true;
    this.error = error;
    this.wake.resolve();
  }

  async *[Symbol.asyncIterator](): AsyncGenerator<CircusTaskEvent> {
    if (this.claimed) throw new CircusBridgeError("protocol", "Task events have one consumer");
    this.claimed = true;
    while (true) {
      if (this.error) throw this.error;
      const entry = this.queue.shift();
      if (entry) {
        this.bytes -= entry.bytes;
        yield entry.event;
      } else if (this.ended) {
        return;
      } else {
        this.wake = new Deferred<void>();
        await this.wake.promise;
      }
    }
  }
}
