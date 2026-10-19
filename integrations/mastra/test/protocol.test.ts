import { describe, expect, it } from 'vitest';
import { minutesToDuration, sizeToMiB } from '../src/duration.js';
import { STATUS, STDOUT, frame, parseStatus, toBytes, unframe } from '../src/protocol.js';

describe('protocol', () => {
  it('frames and unframes', () => {
    const f = frame(STDOUT, 'hello');
    expect(f[0]).toBe(STDOUT);
    expect(unframe(f)).toEqual({ channel: STDOUT, payload: new TextEncoder().encode('hello') });
    expect(unframe(new Uint8Array(0))).toBeUndefined();
    expect(frame(0)).toEqual(new Uint8Array([0]));
  });

  it('parses status frames and tolerates garbage', () => {
    expect(parseStatus(unframe(frame(STATUS, '{"exit_code":7}'))!.payload)).toEqual({ exit_code: 7 });
    expect(parseStatus(new TextEncoder().encode('nope'))).toMatchObject({ error: expect.stringContaining('malformed') });
  });

  it('normalises WebSocket payload shapes', () => {
    const ab = new Uint8Array([1, 2, 3]).buffer;
    expect(Array.from(toBytes(ab))).toEqual([1, 2, 3]);
    expect(Array.from(toBytes(Buffer.from([4])))).toEqual([4]);
    expect(Array.from(toBytes([Buffer.from([1]), Buffer.from([2])]))).toEqual([1, 2]);
    expect(Array.from(toBytes('a'))).toEqual([97]);
  });
});

describe('durations and sizes', () => {
  it('maps minutes to the lifecycle vocabulary', () => {
    expect(minutesToDuration(undefined)).toBeUndefined();
    expect(minutesToDuration(0)).toBe('0');
    expect(minutesToDuration(15)).toBe('15m');
    expect(minutesToDuration(0.5)).toBe('30s');
    expect(() => minutesToDuration(-1)).toThrow(RangeError);
  });

  it('parses memory sizes', () => {
    expect(sizeToMiB('512m')).toBe(512);
    expect(sizeToMiB('2g')).toBe(2048);
    expect(sizeToMiB('1024')).toBe(1024);
    expect(sizeToMiB('1GiB')).toBe(1024);
    expect(sizeToMiB('lots')).toBeUndefined();
  });
});
