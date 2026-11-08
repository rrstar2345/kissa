<script lang="ts">
  import { onMount } from 'svelte';
  import { api } from '$lib/api';
  import ExitAllButton from '$lib/components/ExitAllButton.svelte';
  import ExitButton from '$lib/components/ExitButton.svelte';
  import { istTime, num, signed, todayIst, trend } from '$lib/format';
  import { liveAccount, livePosition } from '$lib/live';
  import { contractName, trading } from '$lib/trading.svelte';
  import type { Dashboard, DiaryDay } from '$lib/types';

  let dash = $state.raw<Dashboard | null>(null);
  let error = $state('');

  // The dashboard is fetched when orders change; prices and P&L follow the
  // live quote stream, so they move with the market without any polling.
  const positions = $derived((dash?.positions ?? []).map(livePosition));
  const accounts = $derived((dash?.accounts ?? []).map((a) => liveAccount(a, positions)));

  // diary
  const daysAgo = (n: number) => new Date(Date.now() + 5.5 * 3600_000 - n * 86400_000).toISOString().slice(0, 10);
  let from = $state(daysAgo(30));
  let to = $state(todayIst());
  let diaryAccount = $state('');
  let diary = $state<DiaryDay[]>([]);
  let open = $state<Record<string, boolean>>({});
  let diaryError = $state('');

  const totals = $derived({
    realised: diary.reduce((s, d) => s + d.realised, 0),
    charges: diary.reduce((s, d) => s + d.charges, 0),
    net: diary.reduce((s, d) => s + d.net, 0)
  });

  async function loadDash() {
    try {
      dash = await api.getDashboard(null);
      error = '';
    } catch (e) {
      error = String(e);
    }
  }

  async function loadDiary() {
    try {
      diary = await api.getDiary(diaryAccount || null, from, to);
      diaryError = '';
    } catch (e) {
      diaryError = String(e);
    }
  }

  // Safety net only (a missed event, a long sleep): live prices do not need it.
  onMount(() => {
    const t = setInterval(loadDash, 30_000);
    return () => clearInterval(t);
  });

  $effect(() => {
    trading.version;
    loadDash();
  });
  $effect(() => {
    from;
    to;
    diaryAccount;
    trading.version;
    loadDiary();
  });
</script>

<div class="flex flex-col gap-6">
  {#if error}<div role="alert" class="alert alert-error alert-soft py-2 text-sm break-words">{error}</div>{/if}

  <section class="flex flex-col gap-3">
    <div class="flex items-baseline gap-3">
      <h1 class="text-xl font-bold">Today</h1>
      {#if dash}<span class="num text-xs text-base-content/60">{dash.tradeDate}</span>{/if}
    </div>

    {#if dash && !accounts.length}
      <div class="card border border-base-300 bg-base-100">
        <div class="card-body items-center py-10 text-center">
          <p class="font-medium">No trading accounts yet.</p>
          <a href="/accounts" class="btn btn-primary btn-sm">Add a Paper or Dhan account</a>
        </div>
      </div>
    {/if}

    <div class="grid gap-3 sm:grid-cols-2 xl:grid-cols-3">
      {#each accounts as a (a.accountId)}
        <div class="card border border-base-300 bg-base-100">
          <div class="card-body gap-2 p-4">
            <div class="flex items-center gap-2">
              <h2 class="font-semibold">{a.name}</h2>
              <span class="badge badge-sm badge-outline">{a.kind}</span>
            </div>
            <div class="num text-2xl font-bold {trend(a.netToday)}">₹{signed(a.netToday)}</div>
            <div class="text-xs text-base-content/60">Net today (realised + unrealised − charges)</div>
            <div class="mt-1 grid grid-cols-3 gap-2 text-xs">
              <div><div class="text-base-content/60">Realised</div><div class="num {trend(a.realisedToday)}">{signed(a.realisedToday)}</div></div>
              <div><div class="text-base-content/60">Unrealised</div><div class="num {trend(a.unrealised)}">{signed(a.unrealised)}</div></div>
              <div><div class="text-base-content/60">Charges</div><div class="num">{num(a.chargesToday)}</div></div>
            </div>
            <div class="flex flex-wrap gap-x-4 text-xs text-base-content/60">
              <span>{a.openPositions} open</span>
              <span>{a.tradesToday} trades</span>
              <span>{a.ordersToday} orders</span>
              {#if a.startingCapital}<span>Capital ₹{num(a.startingCapital, 0)}</span>{/if}
            </div>
          </div>
        </div>
      {/each}
    </div>
  </section>

  {#if positions.length}
    <section class="flex flex-col gap-2">
      <div class="flex items-center justify-between">
        <h2 class="font-semibold">Open positions</h2>
        <ExitAllButton {positions} ondone={loadDash} />
      </div>
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
                  <span class="badge badge-xs {p.ltpSource === 'live' ? 'badge-success' : 'badge-ghost'}" title={p.ltpSource === 'last' ? 'Last stored bar; no live price' : ''}>{p.ltpSource}</span>
                </td>
                <td class="num text-right font-semibold {trend(p.unrealised)}">{signed(p.unrealised)}</td>
                <td class="text-right"><ExitButton position={p} ondone={loadDash} /></td>
              </tr>
            {/each}
          </tbody>
        </table>
      </div>
    </section>
  {/if}

  <section class="flex flex-col gap-3">
    <div class="flex flex-wrap items-center gap-3">
      <h2 class="font-semibold">Trader's diary</h2>
      <select class="select select-sm w-44" bind:value={diaryAccount}>
        <option value="">All accounts</option>
        {#each accounts as a}<option value={a.accountId}>{a.name}</option>{/each}
      </select>
      <input class="input input-sm w-40" type="date" bind:value={from} max={to} />
      <span class="text-sm text-base-content/60">to</span>
      <input class="input input-sm w-40" type="date" bind:value={to} min={from} />
    </div>

    {#if diaryError}<div role="alert" class="alert alert-error alert-soft py-2 text-sm break-words">{diaryError}</div>{/if}

    <div class="overflow-x-auto rounded-box border border-base-300 bg-base-100">
      <table class="table table-sm">
        <thead>
          <tr><th></th><th>Date</th><th class="text-right">Orders</th><th class="text-right">Trades</th><th class="text-right">Realised</th><th class="text-right">Charges</th><th class="text-right">Net</th></tr>
        </thead>
        <tbody>
          {#each diary as d (d.date)}
            <tr class="cursor-pointer hover:bg-base-200" onclick={() => (open[d.date] = !open[d.date])}>
              <td class="w-6">{open[d.date] ? '▾' : '▸'}</td>
              <td class="num">{d.date}</td>
              <td class="num text-right">{d.orders}</td>
              <td class="num text-right">{d.trades}</td>
              <td class="num text-right {trend(d.realised)}">{signed(d.realised)}</td>
              <td class="num text-right">{num(d.charges)}</td>
              <td class="num text-right font-semibold {trend(d.net)}">{signed(d.net)}</td>
            </tr>
            {#if open[d.date]}
              <tr>
                <td></td>
                <td colspan="6" class="bg-base-200 p-0">
                  <table class="table table-xs">
                    <tbody>
                      {#each d.rows as t (t.id)}
                        <tr>
                          <td class="num">{istTime(t.ts)}</td>
                          <td>{contractName(t)}</td>
                          <td class={t.side === 'BUY' ? 'up' : 'down'}>{t.side}</td>
                          <td class="num text-right">{t.qty} @ {num(t.price)}</td>
                          <td class="num text-right {trend(t.realised)}">{t.realised ? signed(t.realised) : '—'}</td>
                          <td class="num text-right">{num(t.charges)}</td>
                        </tr>
                      {/each}
                    </tbody>
                  </table>
                </td>
              </tr>
            {/if}
          {:else}
            <tr><td colspan="7" class="py-8 text-center text-base-content/60">No activity in this range.</td></tr>
          {/each}
        </tbody>
        {#if diary.length}
          <tfoot>
            <tr class="font-semibold">
              <td colspan="4" class="text-right">Total</td>
              <td class="num text-right {trend(totals.realised)}">{signed(totals.realised)}</td>
              <td class="num text-right">{num(totals.charges)}</td>
              <td class="num text-right {trend(totals.net)}">{signed(totals.net)}</td>
            </tr>
          </tfoot>
        {/if}
      </table>
    </div>
  </section>
</div>
