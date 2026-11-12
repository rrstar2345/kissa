const dash = '—';

export function num(v: number | null | undefined, dp = 2): string {
  if (v === null || v === undefined || Number.isNaN(v)) return dash;
  return v.toLocaleString('en-IN', { minimumFractionDigits: dp, maximumFractionDigits: dp });
}

export function signed(v: number | null | undefined, dp = 2): string {
  if (v === null || v === undefined || Number.isNaN(v)) return dash;
  return (v > 0 ? '+' : '') + num(v, dp);
}

/** Compact Indian-style: 1.2L, 3.4Cr. */
export function compact(v: number | null | undefined): string {
  if (v === null || v === undefined || Number.isNaN(v)) return dash;
  const a = Math.abs(v);
  const s = v < 0 ? '-' : '';
  if (a >= 1e7) return `${s}${(a / 1e7).toFixed(2)}Cr`;
  if (a >= 1e5) return `${s}${(a / 1e5).toFixed(2)}L`;
  if (a >= 1e3) return `${s}${(a / 1e3).toFixed(1)}K`;
  return `${s}${a.toFixed(0)}`;
}

export function trend(v: number | null | undefined): string {
  if (!v) return '';
  return v > 0 ? 'up' : 'down';
}

/** Today's IST date as YYYY-MM-DD (independent of the machine's time zone). */
export function todayIst(): string {
  return new Date(Date.now() + 5.5 * 3600_000).toISOString().slice(0, 10);
}

/** HH:MM:SS (IST) from an RFC 3339 UTC timestamp. */
export function istTime(ts: string): string {
  const d = new Date(ts);
  if (Number.isNaN(d.getTime())) return dash;
  return new Date(d.getTime() + 5.5 * 3600_000).toISOString().slice(11, 19);
}

/** IST calendar date (YYYY-MM-DD) of an RFC 3339 UTC timestamp. */
export function istDay(ts: string): string {
  const d = new Date(ts);
  if (Number.isNaN(d.getTime())) return dash;
  return new Date(d.getTime() + 5.5 * 3600_000).toISOString().slice(0, 10);
}

/** `DD Mon HH:MM` (IST) when `withDate`, else `HH:MM:SS`. */
export function istStamp(ts: string, withDate: boolean): string {
  if (!withDate) return istTime(ts);
  const d = new Date(ts);
  if (Number.isNaN(d.getTime())) return dash;
  const ist = new Date(d.getTime() + 5.5 * 3600_000);
  const mon = ist.toLocaleDateString('en-GB', { month: 'short', timeZone: 'UTC' });
  return `${ist.toISOString().slice(8, 10)} ${mon} ${ist.toISOString().slice(11, 16)}`;
}
