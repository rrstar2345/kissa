import { invoke } from '@tauri-apps/api/core';
import type {
  Account,
  BackfillResult,
  ChartData,
  IndicatorSpec,
  InstrumentList,
  ExpiryItem,
  IndexTick,
  MarketStatus,
  SaveAccountInput,
  Source,
  TestResult
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
  testAccount: (id: string) => invoke<TestResult>('test_account', { id })
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
