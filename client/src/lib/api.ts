import { invoke } from '@tauri-apps/api/core';
import type {
  Account,
  BackfillResult,
  ChartData,
  ContractInfo,
  Dashboard,
  DiaryDay,
  IndicatorSpec,
  InstrumentList,
  ExpiryItem,
  IndexTick,
  MarketStatus,
  OrderRow,
  PlaceOrderInput,
  SaveAccountInput,
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

  getChart: (instrument: string, interval: string, indicators: IndicatorSpec[], limit = 1500) =>
    invoke<ChartData>('get_chart', { req: { instrument, interval, indicators, limit } }),
  listChartInstruments: () => invoke<InstrumentList>('list_chart_instruments'),
  backfillChart: (instrument: string, interval: '1m' | '1d', days: number) =>
    invoke<BackfillResult>('backfill_chart', { instrument, interval, days }),

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
  getDashboard: (accountId?: string | null) =>
    invoke<Dashboard>('get_dashboard', { accountId: accountId ?? null }),
  getDiary: (accountId: string | null, from: string, to: string) =>
    invoke<DiaryDay[]>('get_diary', { accountId, from, to }),
  contractInfo: (symbol: string, expiry: string, strike: number, isCall: boolean) =>
    invoke<ContractInfo>('contract_info', { symbol, expiry, strike, isCall }),
  refreshInstruments: () => invoke<number>('refresh_instruments'),
  connectBroker: (accountId: string) => invoke<void>('connect_broker', { accountId }),
  syncOrders: (accountId: string) => invoke<number>('sync_orders', { accountId })
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
