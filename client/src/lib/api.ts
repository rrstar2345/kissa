import { invoke } from '@tauri-apps/api/core';
import type {
  Account,
  AlertConfig,
  AlertRow,
  AlertSettings,
  BackfillResult,
  BacktestReport,
  BacktestRequest,
  ChartData,
  ContractInfo,
  Dashboard,
  DiaryDay,
  ExitAllResult,
  IndicatorSpec,
  KillResult,
  InstrumentList,
  ExpiryItem,
  IndexTick,
  MarketStatus,
  OrderRow,
  PlaceOrderInput,
  QuoteView,
  Algo,
  AlgoJournalRow,
  AlgoView,
  CopierConfig,
  CopierOverview,
  RiskLimits,
  RiskOverview,
  Ruleset,
  SaveAccountInput,
  ServerSettings,
  Source,
  TestResult,
  TradeRow
} from './types';

export const api = {
  marketStatus: () => invoke<MarketStatus>('market_status'),
  indicesSnapshot: () => invoke<IndexTick[]>('indices_snapshot'),
  listFnoSymbols: () => invoke<string[]>('list_fno_symbols'),
  listExpiries: (source: Source, symbol: string) =>
    invoke<ExpiryItem[]>('list_expiries', { source, symbol }),
  startChain: (source: Source, symbol: string, expiry: ExpiryItem) =>
    invoke<void>('start_chain', { source, symbol, expiry }),
  stopChain: () => invoke<void>('stop_chain'),

  getChart: (
    instrument: string,
    interval: string,
    indicators: IndicatorSpec[],
    rulesets: string[] = [],
    draft: Ruleset | null = null,
    limit = 1500
  ) => invoke<ChartData>('get_chart', { req: { instrument, interval, indicators, rulesets, draft, limit } }),
  listRulesets: () => invoke<Ruleset[]>('list_rulesets'),
  saveRuleset: (ruleset: Ruleset) => invoke<Ruleset>('save_ruleset', { ruleset }),
  deleteRuleset: (id: string) => invoke<void>('delete_ruleset', { id }),
  validateRuleset: (ruleset: Ruleset) => invoke<string[]>('validate_ruleset', { ruleset }),
  listChartInstruments: () => invoke<InstrumentList>('list_chart_instruments'),
  backfillChart: (instrument: string, interval: '1m' | '1d', days: number) =>
    invoke<BackfillResult>('backfill_chart', { instrument, interval, days }),

  runBacktest: (req: BacktestRequest) => invoke<BacktestReport>('run_backtest', { req }),

  getSetting: (key: string) => invoke<string | null>('get_setting', { key }),
  setSetting: (key: string, value: string) => invoke<void>('set_setting', { key, value }),

  listAccounts: () => invoke<Account[]>('list_accounts'),
  saveAccount: (input: SaveAccountInput) => invoke<Account>('save_account', { input }),
  deleteAccount: (id: string) => invoke<void>('delete_account', { id }),
  testAccount: (id: string) => invoke<TestResult>('test_account', { id }),

  placeOrder: (input: PlaceOrderInput) => invoke<OrderRow>('place_order', { input }),
  cancelOrder: (id: string) => invoke<OrderRow>('cancel_order', { id }),
  modifyOrder: (id: string, price: number, triggerPrice?: number) =>
    invoke<OrderRow>('modify_order', { id, price, triggerPrice: triggerPrice ?? null }),
  listOrders: (accountId?: string | null, date?: string | null) =>
    invoke<OrderRow[]>('list_orders', { accountId: accountId ?? null, date: date ?? null }),
  listTrades: (accountId?: string | null, date?: string | null) =>
    invoke<TradeRow[]>('list_trades', { accountId: accountId ?? null, date: date ?? null }),
  exitPosition: (accountId: string, instrument: string, product: string) =>
    invoke<OrderRow>('exit_position', { accountId, instrument, product }),
  exitAll: (accountId?: string | null) => invoke<ExitAllResult>('exit_all', { accountId: accountId ?? null }),
  getRisk: () => invoke<RiskOverview>('get_risk'),
  saveRiskLimits: (accountId: string, limits: RiskLimits) =>
    invoke<RiskLimits>('save_risk_limits', { accountId, limits }),
  setKillSwitch: (on: boolean, flatten = false) => invoke<KillResult>('set_kill_switch', { on, flatten }),
  resumeAccount: (accountId: string) => invoke<void>('resume_account', { accountId }),
  listAlgos: () => invoke<AlgoView[]>('list_algos'),
  saveAlgo: (algo: Algo) => invoke<Algo>('save_algo', { algo }),
  deleteAlgo: (id: string) => invoke<void>('delete_algo', { id }),
  setAlgoArmed: (id: string, armed: boolean) => invoke<AlgoView>('set_algo_armed', { id, armed }),
  validateAlgo: (algo: Algo) => invoke<string[]>('validate_algo', { algo }),
  listAlgoJournal: (algoId: string | null = null, limit = 300) =>
    invoke<AlgoJournalRow[]>('list_algo_journal', { algoId, limit }),
  clearAlgoJournal: (algoId: string | null = null) => invoke<void>('clear_algo_journal', { algoId }),
  getCopier: () => invoke<CopierOverview>('get_copier'),
  saveCopier: (config: CopierConfig) => invoke<CopierConfig>('save_copier', { config }),
  getAlertSettings: () => invoke<AlertSettings>('get_alert_settings'),
  saveAlertSettings: (config: AlertConfig, telegramToken?: string, smtpPassword?: string) =>
    invoke<AlertSettings>('save_alert_settings', {
      config,
      telegramToken: telegramToken || null,
      smtpPassword: smtpPassword || null
    }),
  testAlert: (channel: 'telegram' | 'email') => invoke<TestResult>('test_alert', { channel }),
  listAlertHistory: (limit = 200) => invoke<AlertRow[]>('list_alert_history', { limit }),
  clearAlertHistory: () => invoke<void>('clear_alert_history'),
  getQuote: (symbol: string, expiry: string, strike: number, isCall: boolean) =>
    invoke<QuoteView>('get_quote', { symbol, expiry, strike, isCall }),
  getDashboard: (accountId?: string | null) =>
    invoke<Dashboard>('get_dashboard', { accountId: accountId ?? null }),
  getDiary: (accountId: string | null, from: string, to: string) =>
    invoke<DiaryDay[]>('get_diary', { accountId, from, to }),
  contractInfo: (symbol: string, expiry: string, strike: number, isCall: boolean) =>
    invoke<ContractInfo>('contract_info', { symbol, expiry, strike, isCall }),
  refreshInstruments: () => invoke<number>('refresh_instruments'),
  connectBroker: (accountId: string) => invoke<void>('connect_broker', { accountId }),
  syncOrders: (accountId: string) => invoke<number>('sync_orders', { accountId }),

  getSettingsJson: () => invoke<string>('get_settings_json'),
  saveSettingsJson: (json: string) => invoke<void>('save_settings_json', { json }),
  getServerSettings: () => invoke<ServerSettings>('get_server_settings'),
  saveServerSettings: (url: string, port: number, apiKey?: string) =>
    invoke<ServerSettings>('save_server_settings', { url, port, apiKey: apiKey || null }),
  testServer: () => invoke<TestResult>('test_server')
};

/** Read a JSON setting, returning `fallback` if unset or unparsable. */
export async function getJson<T>(key: string, fallback: T): Promise<T> {
  try {
    const raw = await api.getSetting(key);
    return raw ? (JSON.parse(raw) as T) : fallback;
  } catch {
    return fallback;
  }
}

export const setJson = (key: string, value: unknown) => api.setSetting(key, JSON.stringify(value));
