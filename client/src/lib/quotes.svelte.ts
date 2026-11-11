import { listen, type UnlistenFn } from '@tauri-apps/api/event';
import { SvelteMap } from 'svelte/reactivity';

/** A price older than this (seconds) is shown as "last", not "live". */
const FRESH_SECS = 120;

export interface Px {
  ltp: number;
  /** Local receive time, epoch seconds. */
  rx: number;
}

interface QuoteTick {
  k: string; // instrument key
  p: number; // last traded price
  t: number; // epoch seconds on the NSE-synced clock
}

/**
 * Live prices pushed by the backend (`quotes` event, pushed when prices change, coalesced over 50 ms).
 * Components read `prices.get(key)` inside `$derived` / `$effect`; only those
 * that read a key re-run when that key's price changes, so a tick repaints
 * just the cells showing it.
 */
class QuoteStore {
  prices = new SvelteMap<string, Px>();
  /** Epoch seconds, advanced every 5 s so live / last badges age without ticks. */
  nowSecs = $state(Math.floor(Date.now() / 1000));
  private unlisten: UnlistenFn | null = null;
  private timer: ReturnType<typeof setInterval> | null = null;
  private started = false;

  async start() {
    if (this.started) return;
    this.started = true;
    // Freshness is judged in tens of seconds, so a slow clock is enough; skipped while hidden.
    this.timer = setInterval(() => {
      if (!document.hidden) this.nowSecs = Math.floor(Date.now() / 1000);
    }, 5000);
    this.unlisten = await listen<QuoteTick[]>('quotes', (e) => {
      const rx = Math.floor(Date.now() / 1000);
      for (const q of e.payload) this.prices.set(q.k, { ltp: q.p, rx });
    });
  }

  stop() {
    if (this.timer) clearInterval(this.timer);
    this.unlisten?.();
    this.timer = null;
    this.unlisten = null;
    this.started = false;
  }

  /** Last pushed price of an instrument, if any arrived since the app started. */
  get(key: string): Px | undefined {
    return this.prices.get(key);
  }

  isFresh(px: Px): boolean {
    return this.nowSecs - px.rx <= FRESH_SECS;
  }
}

export const quotes = new QuoteStore();
