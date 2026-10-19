/** Minutes → Nordkraft lifecycle duration (`"15m"`, `"0"` = never). */
export function minutesToDuration(minutes: number | undefined): string | undefined {
  if (minutes === undefined) return undefined;
  if (!Number.isFinite(minutes) || minutes < 0) throw new RangeError(`invalid interval: ${minutes} minutes`);
  if (minutes === 0) return '0';
  const seconds = Math.round(minutes * 60);
  return seconds % 60 === 0 ? `${seconds / 60}m` : `${seconds}s`;
}

/** `"512m"` / `"2g"` / `"1024"` (MiB) → MiB, or `undefined` when unparsable. */
export function sizeToMiB(size: string | undefined): number | undefined {
  if (!size) return undefined;
  const m = /^\s*(\d+(?:\.\d+)?)\s*([kmgt]?)i?b?\s*$/i.exec(size);
  if (!m) return undefined;
  const n = Number(m[1]);
  switch ((m[2] ?? '').toLowerCase()) {
    case 'k':
      return n / 1024;
    case '':
    case 'm':
      return n;
    case 'g':
      return n * 1024;
    case 't':
      return n * 1024 * 1024;
    default:
      return undefined;
  }
}
