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
    this.timer = setInterval(() => this.refresh(), 3000);
  }

  async refresh() {
    this.status = await api.marketStatus().catch(() => this.status);
  }

  stop() {
    if (this.timer) clearInterval(this.timer);
    this.unlisten?.();
  }
}

export const market = new MarketStore();
