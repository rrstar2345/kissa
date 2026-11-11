import { listen, type UnlistenFn } from '@tauri-apps/api/event';
import { api } from './api';
import type { IndexTick, MarketStatus, SourceStatus } from './types';

/** Short label for a source's market state, e.g. for a header badge. */
export function stateLabel(s: SourceStatus | undefined): string {
  switch (s?.state) {
    case 'open':
      return 'Open';
    case 'preOpen':
      return 'Pre-open';
    case 'closed':
      return 'Closed';
    default:
      return s?.reason === 'error' ? 'Status n/a' : 'Checking…';
  }
}

export function stateBadge(s: SourceStatus | undefined): string {
  switch (s?.state) {
    case 'open':
      return 'badge-success';
    case 'preOpen':
      return 'badge-warning';
    case 'closed':
      return 'badge-ghost';
    default:
      return 'badge-outline';
  }
}

const IST_OFFSET_MS = (5 * 60 + 30) * 60 * 1000;

function fmtIst(ms: number): string {
  const d = new Date(ms + IST_OFFSET_MS);
  const p = (n: number) => String(n).padStart(2, '0');
  return `${p(d.getUTCHours())}:${p(d.getUTCMinutes())}:${p(d.getUTCSeconds())}`;
}

class MarketStore {
  status = $state<MarketStatus | null>(null);
  indices = $state<Record<string, IndexTick>>({});
  /** IST wall clock (HH:MM:SS), kept running locally from the backend's clock. */
  clock = $state('');
  private offsetMs = 0;
  private timer: ReturnType<typeof setInterval> | null = null;
  private unlisten: UnlistenFn[] = [];

  private apply(s: MarketStatus) {
    this.status = s;
    this.offsetMs = s.nowMs - Date.now();
    this.tickClock();
  }

  private tickClock() {
    this.clock = fmtIst(Date.now() + this.offsetMs);
  }

  async start() {
    // Listen first, then read the current state, so no push is missed in between.
    this.unlisten.push(
      await listen<IndexTick>('index-tick', (e) => {
        this.indices[e.payload.indexName] = e.payload;
      }),
      await listen<MarketStatus>('market-status', (e) => this.apply(e.payload))
    );
    const seed = await api.indicesSnapshot().catch(() => []);
    for (const t of seed) this.indices[t.indexName] = this.indices[t.indexName] ?? t;
    const s = await api.marketStatus().catch(() => null);
    if (s) this.apply(s);
    // Status changes are pushed by the backend (`market-status`); this timer
    // only advances the displayed clock, with no backend call.
    this.timer = setInterval(() => {
      if (!document.hidden) this.tickClock();
    }, 1000);
  }

  stop() {
    if (this.timer) clearInterval(this.timer);
    this.timer = null;
    this.unlisten.forEach((u) => u());
    this.unlisten = [];
  }
}

export const market = new MarketStore();
