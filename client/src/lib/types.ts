export type Source = 'nse' | 'fyers';
export type AccountKind = 'paper' | 'dhan' | 'proxy' | 'fyers';

export interface Leg {
  ltp: number | null;
  change: number | null;
  changePct: number | null;
  oi: number | null;
  oiChange: number | null;
  volume: number | null;
  bid: number | null;
  ask: number | null;
  iv: number | null;
  delta: number | null;
  gamma: number | null;
  theta: number | null;
  vega: number | null;
}

export interface StrikeRow {
  strike: number;
  ce: Leg;
  pe: Leg;
}

export interface ChainSnapshot {
  source: Source;
  symbol: string;
  expiry: string;
  underlyingName: string | null;
  underlying: number | null;
  underlyingChange: number | null;
  underlyingChangePct: number | null;
  vix: number | null;
  atm: number | null;
  callOi: number;
  putOi: number;
  pcr: number | null;
  greeksSource: 'computed' | 'broker';
  updatedAt: string;
  rows: StrikeRow[];
}

export interface ExpiryItem {
  value: string;
  label: string;
  date: string;
}

export interface IndexTick {
  time: string;
  indexName: string;
  currentPrice: number;
  change: number;
  perChange: number;
  previousClose: number;
  open: number;
  high: number;
  low: number;
  mktStatus: string;
}

export interface StreamStat {
  name: string;
  state: 'connecting' | 'connected' | 'reconnecting' | 'idle';
  ticksReceived: number;
  lastTickAt: string | null;
  lastError: string | null;
  reconnectCount: number;
}

export interface MarketStatus {
  istTime: string;
  withinWindow: boolean;
  mode: string;
  streams: StreamStat[];
}

export interface Account {
  id: string;
  kind: AccountKind;
  name: string;
  meta: Record<string, any>;
  secretsSet: string[];
  createdAt: string;
}

export interface SaveAccountInput {
  id?: string;
  kind: AccountKind;
  name: string;
  meta: Record<string, any>;
  secrets: Record<string, string>;
}

export interface TestResult {
  ok: boolean;
  message: string;
}

export interface ChainStatus {
  state: 'started' | 'error';
  message: string;
}
