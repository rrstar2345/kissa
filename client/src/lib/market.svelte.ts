import { listen, type UnlistenFn } from '@tauri-apps/api/event';
import { api } from './api';
import type { IndexTick, MarketStatus } from './types';

class MarketStore {
  status = $state<MarketStatus | null>(null);
  indices = $state<Record<string, IndexTick>>({});
  private timer: ReturnType<typeof setInterval> | null = null;
  private unlisten: UnlistenFn | null = null;

  async start() {
    const seed = await api.indicesSnapshot().catch(() => []);
    for (const t of seed) this.indices[t.indexName] = t;
    this.unlisten = await listen<IndexTick>('index-tick', (e) => {
      this.indices[e.payload.indexName] = e.payload;
    });
    await this.refresh();
    // Status is a small read (clock, mode, stream states): once a second keeps
    // the header clock ticking and stream changes visible almost at once.
    this.timer = setInterval(() => this.refresh(), 1000);
  }

  private busy = false;

  async refresh() {
    if (this.busy) return; // never queue calls behind a slow one
    this.busy = true;
    try {
      this.status = await api.marketStatus().catch(() => this.status);
    } finally {
      this.busy = false;
    }
  }

  stop() {
    if (this.timer) clearInterval(this.timer);
    this.unlisten?.();
  }
}

export const market = new MarketStore();
