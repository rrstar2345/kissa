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

// ---------------------------------------------------------------- charts ---

export interface Bar {
  ts: number; // epoch seconds (UTC), bucket start
  open: number;
  high: number;
  low: number;
  close: number;
  volume: number;
  oi: number | null;
}

export type IndicatorKind = 'sma' | 'ema' | 'rsi' | 'macd' | 'bb' | 'vwap' | 'supertrend';

export interface IndicatorSpec {
  /** Stable preset key used by the UI, e.g. `ema:21` (ignored by the backend). */
  key: string;
  kind: IndicatorKind;
  period?: number;
  fast?: number;
  slow?: number;
  signal?: number;
  mult?: number;
}

export interface IndicatorLine {
  id: string;
  label: string;
  /** `price` or a sub-pane id (`rsi`, `macd`) */
  pane: string;
  style: 'line' | 'histogram';
  tone: 'up' | 'down' | null;
  points: { time: number; value: number | null }[];
}

export interface ChartSignal {
  time: number;
  side: 'buy' | 'sell';
  price: number;
  source: string;
}

export interface ChartData {
  bars: Bar[];
  lines: IndicatorLine[];
  signals: ChartSignal[];
}

export interface OptionExpiryInfo {
  symbol: string;
  expiry: string; // YYYY-MM-DD
  strikes: number[];
}

export interface InstrumentList {
  indices: string[];
  options: OptionExpiryInfo[];
}

export interface BackfillResult {
  symbol: string;
  fetched: number;
  inserted: number;
}

export interface BarEvent extends Bar {
  instrument: string;
}

export interface PanelConfig {
  id: string;
  instrument: string | null;
  interval: string;
  indicators: IndicatorSpec[];
  volume: boolean;
}

export interface ChartsLayout {
  cols: 1 | 2 | 3;
  panels: PanelConfig[];
}

// --------------------------------------------------------------- trading ---

export type Side = 'BUY' | 'SELL';
export type Product = 'INTRADAY' | 'MARGIN';
export type OrderType = 'LIMIT' | 'STOP_LOSS';
export type Validity = 'DAY' | 'IOC';

export interface OrderRow {
  id: string;
  accountId: string;
  accountKind: 'paper' | 'dhan';
  correlationId: string;
  brokerOrderId: string | null;
  instrument: string;
  symbol: string;
  expiry: string; // YYYY-MM-DD
  strike: number;
  isCall: boolean;
  securityId: string | null;
  lotSize: number;
  side: Side;
  product: Product;
  orderType: OrderType;
  validity: Validity;
  qty: number;
  price: number;
  triggerPrice: number;
  status: string;
  filledQty: number;
  avgPrice: number;
  message: string;
  createdAt: string;
  updatedAt: string;
  tradeDate: string;
}

export interface TradeRow {
  id: string;
  accountId: string;
  orderId: string;
  instrument: string;
  symbol: string;
  expiry: string;
  strike: number;
  isCall: boolean;
  lotSize: number;
  side: Side;
  product: Product;
  qty: number;
  price: number;
  charges: number;
  chargeDetail: string;
  realised: number;
  ts: string;
  tradeDate: string;
  extId: string | null;
}

export interface PlaceOrderInput {
  accountId: string;
  symbol: string;
  expiry: string; // YYYY-MM-DD
  strike: number;
  isCall: boolean;
  side: Side;
  product: Product;
  orderType: OrderType;
  validity: Validity;
  lots: number;
  price: number;
  triggerPrice?: number;
}

export interface ContractInfo {
  lotSize: number | null;
  securityId: string | null;
  tradingSymbol: string | null;
  lotSource: 'dhan' | 'fallback' | 'none';
}

export interface Position {
  accountId: string;
  accountName: string;
  accountKind: string;
  instrument: string;
  symbol: string;
  expiry: string;
  strike: number;
  isCall: boolean;
  product: Product;
  lotSize: number;
  netQty: number;
  avgPrice: number;
  ltp: number | null;
  ltpSource: 'live' | 'last' | 'none';
  unrealised: number | null;
}

export interface AccountSummary {
  accountId: string;
  name: string;
  kind: string;
  startingCapital: number | null;
  realisedToday: number;
  chargesToday: number;
  unrealised: number;
  netToday: number;
  openPositions: number;
  tradesToday: number;
  ordersToday: number;
}

export interface Dashboard {
  asOf: string;
  tradeDate: string;
  accounts: AccountSummary[];
  positions: Position[];
}

export interface DiaryDay {
  date: string;
  orders: number;
  trades: number;
  realised: number;
  charges: number;
  net: number;
  rows: TradeRow[];
}

export interface BrokerStatus {
  accountId: string;
  state: 'connecting' | 'connected' | 'disconnected' | 'error' | 'feed_down';
  detail: string;
}

/** What the chain hands to the order ticket when a price is clicked. */
export interface TicketRequest {
  symbol: string;
  expiry: string; // YYYY-MM-DD
  strike: number;
  isCall: boolean;
  side: Side;
  price: number | null;
}
