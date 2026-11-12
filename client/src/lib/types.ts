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

export type MarketState = 'open' | 'preOpen' | 'closed' | 'unknown';

/** What one data source's exchange-status API says. */
export interface SourceStatus {
  state: MarketState;
  reason: 'starting' | 'weekend' | 'outsideHours' | 'reported' | 'error' | 'notUsed';
  message: string;
  /** IST wall clock of the last successful check (HH:MM:SS). */
  checkedAt: string | null;
}

export interface MarketStatus {
  istTime: string;
  /** UTC epoch ms of the snapshot (backend, NSE-synced clock); the header clock runs from it. */
  nowMs: number;
  /** Clock only (weekday 08:30-16:00 IST); it does not know holidays. Use nse / fyers. */
  withinWindow: boolean;
  mode: string;
  nse: SourceStatus;
  fyers: SourceStatus;
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

// ---------------------------------------------------------------- copier ---

export type Rounding = 'down' | 'nearest' | 'up';

export interface CopierChild {
  accountId: string;
  enabled: boolean;
  /** Child lots = master lots x multiplier, then rounded. */
  multiplier: number;
  rounding: Rounding;
  /** A result below 1 lot becomes 1 lot instead of being skipped. */
  minOneLot: boolean;
}

export interface CopierConfig {
  enabled: boolean;
  masterAccountId: string;
  children: CopierChild[];
}

export interface CopierAccount {
  id: string;
  name: string;
  kind: string;
}

export interface CopyView {
  id: string;
  masterOrderId: string;
  childAccountId: string;
  childAccountName: string;
  childOrderId: string | null;
  masterLots: number;
  childLots: number;
  /** placed | skipped | failed */
  status: 'placed' | 'skipped' | 'failed';
  message: string;
  createdAt: string;
  tradeDate: string;
  symbol: string | null;
  expiry: string | null;
  strike: number | null;
  isCall: boolean | null;
  side: Side | null;
  product: Product | null;
  price: number | null;
  /** Current status of the child order, when one exists. */
  childStatus: string | null;
  childFilledQty: number | null;
  childQty: number | null;
}

export interface CopierOverview {
  config: CopierConfig;
  accounts: CopierAccount[];
  links: CopyView[];
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

export type DashView = 'daily' | 'monthly' | 'yearly';

export interface PerfSummary {
  gross: number;
  charges: number;
  net: number;
  /** Closed round trips (entry + exit = one trade). */
  trades: number;
  fills: number;
  entryPremium: number;
  /** Net / entry premium, percent; null without any entry. */
  roi: number | null;
}

export interface PerfCell {
  label: string;
  date: string;
  net: number;
  fills: number;
  trades: number;
  state: 'profit' | 'loss' | 'flat' | 'none';
}

export interface PerfColumn {
  key: string;
  label: string;
  net: number;
  trades: number;
  fills: number;
  /** Empty cells before the first one in a Monday-first calendar. */
  leadBlank: number;
  current: boolean;
  cells: PerfCell[];
}

export interface PerfTrip {
  id: string;
  accountId: string;
  accountName: string;
  symbol: string;
  expiry: string;
  strike: number;
  isCall: boolean;
  product: Product;
  long: boolean;
  qty: number;
  entryPrice: number;
  exitPrice: number | null;
  entryTs: string;
  exitTs: string | null;
  gross: number;
  charges: number;
  net: number;
  closed: boolean;
}

export interface PerfGroup {
  key: string;
  label: string;
  orders: number;
  trades: number;
  fills: number;
  gross: number;
  charges: number;
  net: number;
  rows: PerfTrip[];
}

export interface Performance {
  view: DashView;
  from: string;
  to: string;
  summary: PerfSummary;
  columns: PerfColumn[];
  groups: PerfGroup[];
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

// ----------------------------------------------------------------- algos ---

export type ExpirySel = 'current' | 'next' | 'next1' | 'next2' | 'next3';
export type SideSel = 'ce' | 'pe' | 'both';

export interface AlgoInstrument {
  type: 'option';
  underlying: string;
  expiry: ExpirySel;
  /** Strikes above (+) or below (-) the at-the-money strike. */
  strikeOffset: number;
  side: SideSel;
}

export interface AlgoAction {
  product: Product;
  /** Rupees per account; decides the lots. */
  capital: number;
  bufferTicks: number;
  stopLossPct: number | null;
  targetPct: number | null;
}

export interface Algo {
  id: string;
  name: string;
  signalId: string;
  instrument: AlgoInstrument;
  /** Minutes only: 1m, 5m, 15m... */
  interval: string;
  action: AlgoAction;
  accounts: string[];
  /** Trade-copier switch. */
  copier: boolean;
  /** Per-algo permission to trade live (Dhan) accounts. */
  allowLive: boolean;
}

export interface AlgoLeg {
  accountId: string;
  instrument: string;
  symbol: string;
  expiry: string;
  strike: number;
  isCall: boolean;
  product: Product;
  lots: number;
  orderId: string;
  entryPrice: number;
  /** Premium committed by this leg (counts against the capital pool). */
  cost?: number;
  stop: number | null;
  target: number | null;
  bracket: boolean;
  propagate: boolean;
}

export interface AlgoView {
  algo: Algo;
  signalName: string | null;
  armed: boolean;
  phase: 'flat' | 'in_position';
  legs: AlgoLeg[];
  lastBarTs: number;
  updatedAt: string;
}

export interface AlgoJournalRow {
  id: number;
  ts: string;
  algoId: string;
  algoName: string;
  level: 'info' | 'warn' | 'error';
  kind: string;
  message: string;
}

// -------------------------------------------------------------- backtest ---

export interface BacktestRequest {
  rulesetId: string;
  instrument: string;
  interval: string;
  from: string | null;
  to: string | null;
  lots: number;
  capitalPerTrade: number;
  lotSize: number;
  startingCapital: number;
  stopLossPct: number | null;
  targetPct: number | null;
  slippageTicks: number;
  charges: boolean;
  brokeragePerOrder: number | null;
  carryOvernight: boolean;
}

export interface BacktestTrade {
  entryTs: number;
  entryPrice: number;
  exitTs: number;
  exitPrice: number;
  lots: number;
  qty: number;
  reason: 'signal' | 'stop_loss' | 'target' | 'eod' | 'end';
  gross: number;
  charges: number;
  net: number;
  bars: number;
  returnPct: number;
}

export interface BacktestSummary {
  trades: number;
  wins: number;
  losses: number;
  winRate: number;
  grossPnl: number;
  charges: number;
  netPnl: number;
  avgWin: number;
  avgLoss: number;
  profitFactor: number | null;
  expectancy: number;
  best: number;
  worst: number;
  maxDrawdown: number;
  maxDrawdownPct: number | null;
  maxConsecutiveLosses: number;
  avgBars: number;
  returnPct: number | null;
  skipped: number;
}

export interface BacktestReport {
  ruleset: string;
  instrument: string;
  interval: string;
  lotSize: number;
  bars: number;
  sessions: number;
  firstTs: number;
  lastTs: number;
  summary: BacktestSummary;
  trades: BacktestTrade[];
  equity: { ts: number; equity: number }[];
  warnings: string[];
}
