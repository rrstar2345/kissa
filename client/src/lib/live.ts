import { quotes } from './quotes.svelte';
import type { AccountSummary, Position } from './types';

const r2 = (v: number) => Math.round(v * 100) / 100;

/**
 * A position with the latest pushed price applied. Call it inside `$derived`
 * so the result follows that instrument's price. Without a pushed price (none
 * arrived yet) the backend's snapshot is kept as is.
 */
export function livePosition(p: Position): Position {
  const q = quotes.get(p.instrument);
  if (!q) return p;
  return {
    ...p,
    ltp: q.ltp,
    ltpSource: quotes.isFresh(q) ? 'live' : 'last',
    unrealised: r2((q.ltp - p.avgPrice) * p.netQty)
  };
}

/** Account totals recomputed from live positions (realised and charges come from the ledger). */
export function liveAccount(a: AccountSummary, positions: Position[]): AccountSummary {
  const unrealised = r2(
    positions.filter((p) => p.accountId === a.accountId).reduce((s, p) => s + (p.unrealised ?? 0), 0)
  );
  return { ...a, unrealised, netToday: r2(a.realisedToday + unrealised - a.chargesToday) };
}
