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
  /** Problems with a selected/draft ruleset (it was skipped). */
  warnings?: string[];
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
  /** Ids of saved signal rulesets applied to this chart. */
  rulesets?: string[];
  volume: boolean;
}

export interface ChartsLayout {
  cols: 1 | 2 | 3;
  panels: PanelConfig[];
}

// --------------------------------------------------------------- signals ---

export type VarKind = 'sma' | 'ema' | 'rsi' | 'macd' | 'bb' | 'atr' | 'vwap' | 'supertrend' | 'formula';
export type CmpOp = 'crosses_above' | 'crosses_below' | '>' | '>=' | '<' | '<=' | '=' | '<>';

export interface RuleVar {
  id: string;
  kind: VarKind;
  length?: number;
  /** Price source or the id of an earlier variable. */
  src?: string;
  fast?: number;
  slow?: number;
  signal?: number;
  mult?: number;
  output?: string;
  left?: string;
  op?: string;
  right?: string;
  plot: boolean;
}

export interface RuleCond {
  type: 'cond';
  left: string;
  op: CmpOp;
  right: string;
}

export interface RuleGroup {
  type: 'group';
  op: 'and' | 'or';
  items: RuleNode[];
}

export type RuleNode = RuleCond | RuleGroup;

export interface Ruleset {
  id: string;
  name: string;
  vars: RuleVar[];
  entry: { op: 'and' | 'or'; items: RuleNode[] };
  exit: { op: 'and' | 'or'; items: RuleNode[] };
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
  /** Bracket group: the entry order's id, shared by its target and stop-loss legs. */
  groupId: string;
  /** '' plain order | ENTRY (bracket attached) | TARGET | STOP | SETTLE (expiry settlement). */
  role: '' | 'ENTRY' | 'TARGET' | 'STOP' | 'SETTLE';
  /** On an ENTRY: legs waiting for the fill (JSON). Empty once they are placed. */
  bracket: string;
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
  /** Stop-loss and / or target placed once this order fills (paper accounts only). */
  bracket?: BracketInput;
}

export interface BracketInput {
  target?: number;
  stopTrigger?: number;
  /** Limit price of the stop-loss; the backend derives it from the trigger when left out. */
  stopPrice?: number;
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
  /** Limit price an Exit order would use now (last price plus buffer). */
  exitPrice: number | null;
}

export interface QuoteView {
  /** Instrument key; follow it in the live `quotes` store. */
  key: string;
  ltp: number | null;
  ageSecs: number | null;
  live: boolean;
}

export interface ExitAllResult {
  placed: number;
  cancelled: number;
  skipped: string[];
  orders: OrderRow[];
}

// ------------------------------------------------------------------ risk ---

export interface RiskLimits {
  enabled: boolean;
  /** 0 = no limit */
  maxLotsPerOrder: number;
  /** 0 = no limit */
  maxOpenPositions: number;
  /** Rupees; 0 = no limit */
  maxDailyLoss: number;
  squareOffOnBreach: boolean;
  windowEnabled: boolean;
  /** HH:MM, IST */
  windowStart: string;
  windowEnd: string;
}

export interface HaltInfo {
  kind: 'kill' | 'daily_loss';
  reason: string;
  since: string;
}

export interface AccountRisk {
  accountId: string;
  name: string;
  kind: string;
  limits: RiskLimits;
  halt: HaltInfo | null;
  netToday: number;
  openPositions: number;
}

export interface RiskOverview {
  killSwitch: HaltInfo | null;
  accounts: AccountRisk[];
}

export interface KillResult {
  cancelled: number;
  placed: number;
  skipped: string[];
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

export interface ServerSettings {
  url: string;
  port: number;
  apiKeySet: boolean;
}

// ---------------------------------------------------------------- alerts ---

export interface TelegramConfig {
  enabled: boolean;
  /** Numeric chat ids or @channelname */
  chatIds: string[];
}

export interface EmailConfig {
  enabled: boolean;
  host: string;
  port: number;
  security: 'starttls' | 'tls' | 'none';
  /** Blank = no authentication */
  username: string;
  from: string;
  to: string[];
}

export interface AlertConfig {
  telegram: TelegramConfig;
  email: EmailConfig;
  /** event kind -> on/off; a missing kind is on */
  events: Record<string, boolean>;
  dedupeSecs: number;
  /** HH:MM, IST */
  summaryTime: string;
  tokenWarnHours: number;
  fyersTokenHours: number;
  dhanTokenHours: number;
}

export interface AlertEventInfo {
  kind: string;
  label: string;
  description: string;
}

export interface AlertSettings {
  config: AlertConfig;
  events: AlertEventInfo[];
  telegramTokenSet: boolean;
  smtpPasswordSet: boolean;
}

export interface AlertRow {
  id: number;
  at: string;
  kind: string;
  title: string;
  body: string;
  status: 'sending' | 'sent' | 'partial' | 'failed' | 'suppressed' | 'skipped';
  detail: string;
  attempts: number;
}
