import type { IndicatorSpec } from './types';

export const INTERVALS = ['1m', '3m', '5m', '15m', '30m', '60m', '1d'];

/** Display offset so the time axis shows IST (lightweight-charts renders UTC). */
export const IST_SHIFT = 19800;

export const INDICATOR_PRESETS: IndicatorSpec[] = [
  { key: 'ema:9', kind: 'ema', period: 9 },
  { key: 'ema:21', kind: 'ema', period: 21 },
  { key: 'ema:50', kind: 'ema', period: 50 },
  { key: 'sma:20', kind: 'sma', period: 20 },
  { key: 'sma:50', kind: 'sma', period: 50 },
  { key: 'sma:200', kind: 'sma', period: 200 },
  { key: 'bb:20', kind: 'bb', period: 20, mult: 2 },
  { key: 'vwap', kind: 'vwap' },
  { key: 'supertrend:10:3', kind: 'supertrend', period: 10, mult: 3 },
  { key: 'rsi:14', kind: 'rsi', period: 14 },
  { key: 'macd:12:26:9', kind: 'macd', fast: 12, slow: 26, signal: 9 }
];

export const presetLabel = (p: IndicatorSpec) =>
  ({
    ema: `EMA ${p.period}`,
    sma: `SMA ${p.period}`,
    bb: `Bollinger ${p.period}`,
    vwap: 'VWAP',
    supertrend: `Supertrend ${p.period},${p.mult}`,
    rsi: `RSI ${p.period}`,
    macd: `MACD ${p.fast},${p.slow},${p.signal}`
  })[p.kind];

export type Parsed =
  | { type: 'index'; name: string }
  | { type: 'option'; symbol: string; expiry: string; strike: string; leg: 'CE' | 'PE' };

export function parseInstrument(key: string | null): Parsed | null {
  if (!key) return null;
  if (key.startsWith('IDX:')) return { type: 'index', name: key.slice(4) };
  const p = key.split(':');
  if (p[0] === 'OPT' && p.length === 5)
    return { type: 'option', symbol: p[1], expiry: p[2], strike: p[3], leg: p[4] as 'CE' | 'PE' };
  return null;
}

export const indexKey = (name: string) => `IDX:${name}`;
export const optionKey = (symbol: string, expiry: string, strike: string | number, leg: 'CE' | 'PE') =>
  `OPT:${symbol}:${expiry}:${strike}:${leg}`;

export function instrumentLabel(key: string | null): string {
  const p = parseInstrument(key);
  if (!p) return 'Select instrument';
  return p.type === 'index' ? p.name : `${p.symbol} ${p.expiry} ${p.strike} ${p.leg}`;
}

/** Resolve a DaisyUI CSS variable (oklch) to an rgb() string lightweight-charts can parse. */
export function resolveColor(el: HTMLElement, cssVar: string, fallback: string): string {
  try {
    const probe = document.createElement('span');
    probe.style.color = `var(${cssVar})`;
    probe.style.display = 'none';
    el.appendChild(probe);
    const computed = getComputedStyle(probe).color;
    probe.remove();
    const ctx = document.createElement('canvas').getContext('2d');
    if (!ctx) return fallback;
    ctx.fillStyle = fallback;
    ctx.fillStyle = computed;
    ctx.fillRect(0, 0, 1, 1);
    const [r, g, b] = ctx.getImageData(0, 0, 1, 1).data;
    return `rgb(${r},${g},${b})`;
  } catch {
    return fallback;
  }
}
