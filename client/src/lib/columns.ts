import { compact, num, signed } from './format';
import type { Leg } from './types';

export interface ColumnDef {
  id: string;
  label: string;
  /** Ordered outer → inner (nearest the strike last) for the CALL side. */
  value: (l: Leg) => number | null;
  fmt: (v: number | null) => string;
  /** 'trend' colours by sign; 'oi' draws a bar; 'ltp' colours by day change. */
  kind?: 'trend' | 'oi' | 'ltp';
  group: 'Market' | 'Greeks';
}

export const COLUMNS: ColumnDef[] = [
  { id: 'oi', label: 'OI', value: (l) => l.oi, fmt: compact, kind: 'oi', group: 'Market' },
  { id: 'oiChange', label: 'OI Chg', value: (l) => l.oiChange, fmt: (v) => (v === null ? '—' : (v > 0 ? '+' : '') + compact(v)), kind: 'trend', group: 'Market' },
  { id: 'volume', label: 'Volume', value: (l) => l.volume, fmt: compact, group: 'Market' },
  { id: 'iv', label: 'IV', value: (l) => l.iv, fmt: (v) => num(v, 1), group: 'Greeks' },
  { id: 'delta', label: 'Delta', value: (l) => l.delta, fmt: (v) => num(v, 2), group: 'Greeks' },
  { id: 'gamma', label: 'Gamma', value: (l) => l.gamma, fmt: (v) => num(v, 4), group: 'Greeks' },
  { id: 'theta', label: 'Theta', value: (l) => l.theta, fmt: (v) => num(v, 2), group: 'Greeks' },
  { id: 'vega', label: 'Vega', value: (l) => l.vega, fmt: (v) => num(v, 2), group: 'Greeks' },
  { id: 'bid', label: 'Bid', value: (l) => l.bid, fmt: (v) => num(v, 2), group: 'Market' },
  { id: 'ask', label: 'Ask', value: (l) => l.ask, fmt: (v) => num(v, 2), group: 'Market' },
  { id: 'changePct', label: 'Chg %', value: (l) => l.changePct, fmt: (v) => signed(v, 1), kind: 'trend', group: 'Market' },
  { id: 'change', label: 'Chg', value: (l) => l.change, fmt: (v) => signed(v, 2), kind: 'trend', group: 'Market' },
  { id: 'ltp', label: 'LTP', value: (l) => l.ltp, fmt: (v) => num(v, 2), kind: 'ltp', group: 'Market' }
];

export const DEFAULT_VISIBLE = ['oi', 'oiChange', 'volume', 'iv', 'delta', 'change', 'ltp'];

export const STRIKE_WINDOWS = [5, 8, 10, 15, 20, 0] as const; // 0 = all
