/**
 * Wire framing for Nordkraft exec/shell sessions.
 *
 * Identical to Kubernetes' `v4.channel.k8s.io`: every WebSocket frame is
 * binary and its first byte names the channel. Mirrors
 * `container-api/src/stream/protocol.rs`.
 */

export const STDIN = 0;
export const STDOUT = 1;
export const STDERR = 2;
export const STATUS = 3;
export const RESIZE = 4;

/** Final frame of every session. `exit_code` is absent only when the process could not be started or was lost. */
export interface StatusFrame {
  exit_code?: number | null;
  error?: string;
}

const encoder = new TextEncoder();

/** Prefix `payload` with its channel byte. */
export function frame(channel: number, payload: Uint8Array | string = new Uint8Array(0)): Uint8Array {
  const bytes = typeof payload === 'string' ? encoder.encode(payload) : payload;
  const out = new Uint8Array(bytes.length + 1);
  out[0] = channel;
  out.set(bytes, 1);
  return out;
}

/** Split a received frame into channel and payload. Empty frames yield `undefined`. */
export function unframe(data: Uint8Array): { channel: number; payload: Uint8Array } | undefined {
  if (data.length === 0) return undefined;
  return { channel: data[0]!, payload: data.subarray(1) };
}

export function parseStatus(payload: Uint8Array): StatusFrame {
  const text = new TextDecoder().decode(payload);
  try {
    const parsed = JSON.parse(text) as StatusFrame;
    return parsed && typeof parsed === 'object' ? parsed : { error: `malformed status frame: ${text}` };
  } catch {
    return { error: `malformed status frame: ${text}` };
  }
}

/** Normalise whatever a WebSocket `message` event hands us into bytes. */
export function toBytes(data: unknown): Uint8Array {
  if (data instanceof Uint8Array) return data;
  if (data instanceof ArrayBuffer) return new Uint8Array(data);
  if (ArrayBuffer.isView(data)) return new Uint8Array(data.buffer, data.byteOffset, data.byteLength);
  if (typeof data === 'string') return encoder.encode(data);
  if (Array.isArray(data)) {
    // `ws` may deliver fragmented binary messages as Buffer[]
    const total = data.reduce((n: number, b: Uint8Array) => n + b.length, 0);
    const out = new Uint8Array(total);
    let off = 0;
    for (const b of data as Uint8Array[]) {
      out.set(b, off);
      off += b.length;
    }
    return out;
  }
  throw new TypeError(`unsupported WebSocket payload: ${Object.prototype.toString.call(data)}`);
}
