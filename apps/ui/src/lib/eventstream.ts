// A small reader of the AWS event stream (`application/vnd.amazon.eventstream`),
// which Bedrock's ConverseStream answers in: binary messages, each a prelude,
// typed headers and a payload. The gateway checks the CRCs; this reads the
// messages as the playground's stream events.

import type { SseEvent } from "@/lib/sse";

/** A message as a stream event: its event type, or `exception`, and its payload. */
function event(headers: Map<string, string>, payload: Uint8Array): SseEvent {
  const exception = headers.get(":message-type") === "exception";
  const kind = exception ? "exception" : (headers.get(":event-type") ?? "message");
  return { event: kind, data: new TextDecoder().decode(payload) };
}

/** The string headers of a message's header block. */
function headersOf(block: Uint8Array): Map<string, string> {
  const view = new DataView(block.buffer, block.byteOffset, block.byteLength);
  const headers = new Map<string, string>();
  const fixed: Record<number, number> = { 0: 0, 1: 0, 2: 1, 3: 2, 4: 4, 5: 8, 8: 8, 9: 16 };
  let at = 0;
  while (at < block.length) {
    const nameLength = view.getUint8(at);
    const name = new TextDecoder().decode(block.subarray(at + 1, at + 1 + nameLength));
    at += 1 + nameLength;
    const type = view.getUint8(at);
    at += 1;
    if (type === 6 || type === 7) {
      const length = view.getUint16(at);
      if (type === 7) headers.set(name, new TextDecoder().decode(block.subarray(at + 2, at + 2 + length)));
      at += 2 + length;
    } else {
      at += fixed[type] ?? block.length;
    }
  }
  return headers;
}

/** Incremental parser: feed it chunks in any split, get whole messages. */
export class EventStreamParser {
  private buffer = new Uint8Array(0);

  push(chunk: Uint8Array): SseEvent[] {
    const joined = new Uint8Array(this.buffer.length + chunk.length);
    joined.set(this.buffer);
    joined.set(chunk, this.buffer.length);
    const out: SseEvent[] = [];
    let start = 0;
    while (joined.length - start >= 12) {
      const view = new DataView(joined.buffer, joined.byteOffset + start);
      const total = view.getUint32(0);
      const headers = view.getUint32(4);
      if (total < 16 || joined.length - start < total) break;
      const message = joined.subarray(start, start + total);
      out.push(event(headersOf(message.subarray(12, 12 + headers)), message.subarray(12 + headers, total - 4)));
      start += total;
    }
    this.buffer = joined.slice(start);
    return out;
  }
}

/** Read an event stream body to the end, calling onEvent for each message. */
export async function readEventStream(
  body: ReadableStream<Uint8Array>,
  onEvent: (event: SseEvent) => void,
): Promise<void> {
  const reader = body.getReader();
  const parser = new EventStreamParser();
  try {
    for (;;) {
      const { value, done } = await reader.read();
      if (done) break;
      for (const event of parser.push(value)) onEvent(event);
    }
  } finally {
    reader.releaseLock();
  }
}
