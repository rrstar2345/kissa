<script lang="ts">
  import { every } from '$lib/timers';
  import { onMount } from 'svelte';
  import { page } from '$app/state';
  import { replaceState } from '$app/navigation';
  import { api } from '$lib/api';
  import ExitAllButton from '$lib/components/ExitAllButton.svelte';
  import ExitButton from '$lib/components/ExitButton.svelte';
  import { istTime, num, signed, todayIst, trend } from '$lib/format';
  import { livePosition } from '$lib/live';
  import { contractName, isOpen, statusClass, trading } from '$lib/trading.svelte';
  import type { Account, OrderRow, Position, TradeRow } from '$lib/types';

  type Tab = 'orders' | 'trades' | 'positions';

  let accounts = $state<Account[]>([]);
  let accountId = $state(''); // '' = all
  let date = $state(todayIst());
  let allDates = $state(false);
  // The tab lives in the URL (`?tab=positions`) so the top bar can deep-link to it.
  const TABS: Tab[] = ['orders', 'trades', 'positions'];
  const tab = $derived.by<Tab>(() => {
    const t = page.url.searchParams.get('tab');
    return TABS.find((x) => x === t) ?? 'orders';
  });
  const setTab = (t: Tab) => replaceState(`?tab=${t}`, page.state);
  let orders = $state.raw<OrderRow[]>([]);
  let trades = $state.raw<TradeRow[]>([]);
  let basePositions = $state.raw<Position[]>([]);
  // Prices and unrealised P&L follow the live quote stream; the lists above
  // are only refetched when orders change.
  const positions = $derived(basePositions.map(livePosition));
  let error = $state('');
  let notice = $state('');
  let busyId = $state('');
  let syncing = $state(false);
  let modify = $state<{ order: OrderRow; price: number; trigger: number } | null>(null);

  const account = $derived(accounts.find((a) => a.id === accountId) ?? null);
  const broker = $derived(account?.kind === 'dhan' ? trading.broker[account.id] : null);
  const nameOf = (id: string) => accounts.find((a) => a.id === id)?.name ?? id;
  const dateArg = $derived(allDates ? null : date);

  const totalCharges = $derived(trades.reduce((s, t) => s + t.charges, 0));
  const totalRealised = $derived(trades.reduce((s, t) => s + t.realised, 0));

  onMount(async () => {
    try {
      accounts = (await api.listAccounts()).filter((a) => a.kind === 'paper' || a.kind === 'dhan');
    } catch (e) {
      error = String(e);
    }
  });

  async function load() {
    try {
      const acc = accountId || null;
      const [o, t, d] = await Promise.all([
        api.listOrders(acc, dateArg),
        api.listTrades(acc, dateArg),
        api.getDashboard(acc)
      ]);
      orders = o;
      trades = t;
      basePositions = d.positions;
      error = '';
    } catch (e) {
      error = String(e);
    }
  }

  // Refetch on filter change and on every live order update. The timer is a
  // safety net only; positions' prices come from the live quote stream.
  $effect(() => {
    accountId;
    dateArg;
    trading.version;
    load();
  });
  $effect(() => {
    const t = every(load, 30_000);
    return () => clearInterval(t);
  });

  // Open the proxy session when a Dhan account is picked.
  $effect(() => {
    const a = account;
    if (a?.kind === 'dhan') api.connectBroker(a.id).catch((e) => (error = String(e)));
  });

  async function cancel(o: OrderRow) {
    busyId = o.id;
    try {
      await api.cancelOrder(o.id);
      await load();
    } catch (e) {
      error = String(e);
    } finally {
      busyId = '';
    }
  }

  async function saveModify() {
    if (!modify) return;
    const { order, price, trigger } = modify;
    busyId = order.id;
    try {
      await api.modifyOrder(order.id, price, order.orderType === 'STOP_LOSS' ? trigger : undefined);
      modify = null;
      await load();
    } catch (e) {
      error = String(e);
    } finally {
      busyId = '';
    }
  }

  async function sync() {
    if (!account) return;
    syncing = true;
    notice = '';
    try {
      const n = await api.syncOrders(account.id);
      notice = `Synced ${n} order${n === 1 ? '' : 's'} from the broker.`;
      await load();
    } catch (e) {
      error = String(e);
    } finally {
      syncing = false;
    }
  }

  const canModify = (o: OrderRow) => isOpen(o.status) && o.filledQty === 0 && o.status !== 'UNKNOWN';
  const posValue = (p: Position) => p.netQty * p.avgPrice;
</script>

<div class="flex flex-col gap-4">
  <div class="flex flex-wrap items-center gap-3">
    <select class="select select-sm w-52" bind:value={accountId}>
      <option value="">All accounts</option>
      {#each accounts as a}<option value={a.id}>{a.name} ({a.kind})</option>{/each}
    </select>
    <input class="input input-sm w-40" type="date" bind:value={date} disabled={allDates} />
    <label class="flex cursor-pointer items-center gap-2 text-sm">
      <input type="checkbox" class="checkbox checkbox-sm" bind:checked={allDates} /> All dates
    </label>

    {#if account?.kind === 'dhan'}
      <span class="badge badge-sm {broker?.state === 'connected' ? 'badge-success' : broker?.state === 'error' ? 'badge-error' : 'badge-ghost'}"
        title={broker?.detail}>
        Dhan: {broker?.state ?? 'connecting'}
      </span>
      <button class="btn btn-sm btn-outline" onclick={sync} disabled={syncing}>
        {#if syncing}<span class="loading loading-spinner loading-xs"></span>{/if} Sync orders
      </button>
    {/if}

    <div class="ml-auto flex items-center gap-2">
      <ExitAllButton {positions} accountId={accountId || null} ondone={load} />
      <a href="/chain" class="btn btn-sm btn-primary">New order from chain</a>
    </div>
  </div>

  {#if error}<div role="alert" class="alert alert-error alert-soft py-2 text-sm break-words">{error}</div>{/if}
  {#if notice}<div role="status" class="alert alert-info alert-soft py-2 text-sm">{notice}</div>{/if}

  <div role="tablist" class="tabs tabs-border">
    <button role="tab" class="tab {tab === 'orders' ? 'tab-active' : ''}" onclick={() => setTab('orders')}>Orders ({orders.length})</button>
    <button role="tab" class="tab {tab === 'trades' ? 'tab-active' : ''}" onclick={() => setTab('trades')}>Trades ({trades.length})</button>
    <button role="tab" class="tab {tab === 'positions' ? 'tab-active' : ''}" onclick={() => setTab('positions')}>Positions ({positions.length})</button>
  </div>

  {#if tab === 'orders'}
    <div class="overflow-x-auto rounded-box border border-base-300 bg-base-100">
      <table class="table table-sm">
        <thead>
          <tr><th>Time</th><th>Account</th><th>Contract</th><th>Side</th><th>Type</th><th class="text-right">Qty</th><th class="text-right">Price</th><th class="text-right">Trigger</th><th class="text-right">Filled</th><th class="text-right">Avg</th><th>Status</th><th></th></tr>
        </thead>
        <tbody>
          {#each orders as o (o.id)}
            <tr>
              <td class="num whitespace-nowrap">{istTime(o.createdAt)}</td>
              <td>{nameOf(o.accountId)}</td>
              <td class="whitespace-nowrap">{contractName(o)} <span class="text-xs text-base-content/50">{o.product === 'INTRADAY' ? 'MIS' : 'NRML'}</span></td>
              <td class={o.side === 'BUY' ? 'up' : 'down'}>{o.side}</td>
              <td class="whitespace-nowrap">
                {o.orderType === 'LIMIT' ? 'LMT' : 'SL'}<span class="text-xs text-base-content/50"> {o.validity}</span>
                {#if o.role === 'ENTRY'}
                  <span class="badge badge-ghost badge-xs" title={o.bracket ? 'Stop-loss / target are placed when this fills' : 'Stop-loss / target attached'}>{o.bracket ? 'bracket · pending' : 'bracket'}</span>
                {:else if o.role === 'TARGET'}
                  <span class="badge badge-success badge-outline badge-xs" title="Bracket target">target</span>
                {:else if o.role === 'STOP'}
                  <span class="badge badge-error badge-outline badge-xs" title="Bracket stop-loss">stop-loss</span>
                {:else if o.role === 'SETTLE'}
                  <span class="badge badge-warning badge-outline badge-xs" title="Closed at expiry">expiry</span>
                {/if}
              </td>
              <td class="num text-right">{o.qty}</td>
              <td class="num text-right">{num(o.price)}</td>
              <td class="num text-right">{o.orderType === 'STOP_LOSS' ? num(o.triggerPrice) : '—'}</td>
              <td class="num text-right">{o.filledQty}</td>
              <td class="num text-right">{o.filledQty ? num(o.avgPrice) : '—'}</td>
              <td>
                <span class="badge badge-sm {statusClass(o.status)}">{o.status}</span>
                {#if o.message && (o.status !== 'TRADED' || o.role === 'SETTLE' || o.role === 'ENTRY')}
                  <div class="max-w-56 truncate text-xs text-base-content/60" title={o.message}>{o.message}</div>
                {/if}
              </td>
              <td class="whitespace-nowrap text-right">
                {#if isOpen(o.status)}
                  {#if canModify(o)}
                    <button class="btn btn-ghost btn-xs" disabled={busyId === o.id}
                      onclick={() => (modify = { order: o, price: o.price, trigger: o.triggerPrice })}>Modify</button>
                  {/if}
                  <button class="btn btn-ghost btn-xs text-error" disabled={busyId === o.id} onclick={() => cancel(o)}>Cancel</button>
                {/if}
              </td>
            </tr>
          {:else}
            <tr><td colspan="12" class="py-8 text-center text-base-content/60">No orders. Click a bid, ask or LTP on the option chain to place one.</td></tr>
          {/each}
        </tbody>
      </table>
    </div>
  {:else if tab === 'trades'}
    <div class="overflow-x-auto rounded-box border border-base-300 bg-base-100">
      <table class="table table-sm">
        <thead>
          <tr><th>Time</th><th>Account</th><th>Contract</th><th>Side</th><th class="text-right">Qty</th><th class="text-right">Price</th><th class="text-right">Realised</th><th class="text-right">Charges</th></tr>
        </thead>
        <tbody>
          {#each trades as t (t.id)}
            <tr>
              <td class="num whitespace-nowrap">{istTime(t.ts)}</td>
              <td>{nameOf(t.accountId)}</td>
              <td class="whitespace-nowrap">{contractName(t)}</td>
              <td class={t.side === 'BUY' ? 'up' : 'down'}>{t.side}</td>
              <td class="num text-right">{t.qty}</td>
              <td class="num text-right">{num(t.price)}</td>
              <td class="num text-right {trend(t.realised)}">{t.realised ? signed(t.realised) : '—'}</td>
              <td class="num text-right">{num(t.charges)}</td>
            </tr>
          {:else}
            <tr><td colspan="8" class="py-8 text-center text-base-content/60">No trades.</td></tr>
          {/each}
        </tbody>
        {#if trades.length}
          <tfoot>
            <tr class="font-semibold">
              <td colspan="6" class="text-right">Total</td>
              <td class="num text-right {trend(totalRealised)}">{signed(totalRealised)}</td>
              <td class="num text-right">{num(totalCharges)}</td>
            </tr>
          </tfoot>
        {/if}
      </table>
    </div>
  {:else}
    <div class="overflow-x-auto rounded-box border border-base-300 bg-base-100">
      <table class="table table-sm">
        <thead>
          <tr><th>Account</th><th>Contract</th><th>Product</th><th class="text-right">Net qty</th><th class="text-right">Avg</th><th class="text-right">LTP</th><th class="text-right">Unrealised</th><th></th></tr>
        </thead>
        <tbody>
          {#each positions as p (p.accountId + p.instrument + p.product)}
            <tr>
              <td>{p.accountName}</td>
              <td class="whitespace-nowrap">{contractName(p)}</td>
              <td>{p.product === 'INTRADAY' ? 'MIS' : 'NRML'}</td>
              <td class="num text-right {p.netQty > 0 ? 'up' : 'down'}">{p.netQty}</td>
              <td class="num text-right">{num(p.avgPrice)}</td>
              <td class="num text-right">
                {num(p.ltp)}
                <span class="badge badge-xs {p.ltpSource === 'live' ? 'badge-success' : 'badge-ghost'}">{p.ltpSource}</span>
              </td>
              <td class="num text-right {trend(p.unrealised)}">{signed(p.unrealised)}</td>
              <td class="text-right"><ExitButton position={p} ondone={load} /></td>
            </tr>
          {:else}
            <tr><td colspan="8" class="py-8 text-center text-base-content/60">No open positions.</td></tr>
          {/each}
        </tbody>
      </table>
    </div>
  {/if}
</div>

{#if modify}
  <dialog class="modal modal-open">
    <div class="modal-box max-w-sm">
      <h3 class="text-lg font-bold">Modify order</h3>
      <p class="text-sm text-base-content/60">{contractName(modify.order)} · {modify.order.side} {modify.order.qty}</p>
      <div class="mt-4 flex flex-col gap-3">
        <label class="flex flex-col gap-1">
          <span class="text-xs text-base-content/60">Price</span>
          <input class="input input-sm num" type="number" step="0.05" min="0.05" bind:value={modify.price} />
        </label>
        {#if modify.order.orderType === 'STOP_LOSS'}
          <label class="flex flex-col gap-1">
            <span class="text-xs text-base-content/60">Trigger</span>
            <input class="input input-sm num" type="number" step="0.05" min="0.05" bind:value={modify.trigger} />
          </label>
        {/if}
      </div>
      <div class="modal-action">
        <button class="btn btn-ghost btn-sm" onclick={() => (modify = null)}>Close</button>
        <button class="btn btn-primary btn-sm" onclick={saveModify} disabled={busyId === modify.order.id}>Save</button>
      </div>
    </div>
    <button class="modal-backdrop" aria-label="Close" onclick={() => (modify = null)}></button>
  </dialog>
{/if}