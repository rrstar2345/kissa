import { listen, type UnlistenFn } from '@tauri-apps/api/event';
import type { BrokerStatus, OrderRow } from './types';

/** Live trading events shared by the Orders page, Dashboard and order ticket. */
class TradingStore {
  /** Bumped on every `order-update`; pages `$effect` on it to refetch. */
  version = $state(0);
  lastOrder = $state<OrderRow | null>(null);
  broker = $state<Record<string, BrokerStatus>>({});
  /** Bumped on every `risk-update` (limits saved, halt raised or lifted, kill switch). */
  riskVersion = $state(0);
  /** Bumped on every `alert-update` (alert queued, delivered or failed; history cleared). */
  alertVersion = $state(0);
  private unlisten: UnlistenFn[] = [];
  private started = false;

  async start() {
    if (this.started) return;
    this.started = true;
    this.unlisten.push(
      await listen<OrderRow>('order-update', (e) => {
        this.lastOrder = e.payload;
        this.version++;
      }),
      await listen<BrokerStatus>('broker-status', (e) => {
        this.broker[e.payload.accountId] = e.payload;
      }),
      await listen('risk-update', () => {
        this.riskVersion++;
      }),
      await listen('alert-update', () => {
        this.alertVersion++;
      })
    );
  }

  stop() {
    this.unlisten.forEach((u) => u());
    this.unlisten = [];
    this.started = false;
  }
}

export const trading = new TradingStore();

export const OPEN_STATUSES = ['TRANSIT', 'PENDING', 'TRIGGERED', 'PART_TRADED', 'UNKNOWN'];
export const isOpen = (status: string) => OPEN_STATUSES.includes(status);

export function statusClass(status: string): string {
  switch (status) {
    case 'TRADED':
      return 'badge-success';
    case 'PART_TRADED':
    case 'TRIGGERED':
      return 'badge-info';
    case 'REJECTED':
      return 'badge-error';
    case 'CANCELLED':
    case 'EXPIRED':
      return 'badge-ghost';
    case 'UNKNOWN':
      return 'badge-warning';
    default:
      return 'badge-outline';
  }
}

/** Display name for an option contract, e.g. `NIFTY 24 Nov 24000 CE`. */
export function contractName(o: { symbol: string; expiry: string; strike: number; isCall: boolean }): string {
  const d = new Date(o.expiry + 'T00:00:00');
  const ex = d.toLocaleDateString('en-GB', { day: '2-digit', month: 'short' });
  return `${o.symbol} ${ex} ${o.strike} ${o.isCall ? 'CE' : 'PE'}`;
}
