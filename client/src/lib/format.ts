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
