<script lang="ts">
  import { every } from '$lib/timers';
  import { onMount } from 'svelte';
  import { api } from '$lib/api';
  import ExitAllButton from '$lib/components/ExitAllButton.svelte';
  import ExitButton from '$lib/components/ExitButton.svelte';
  import { compact, istDay, istStamp, num, signed, todayIst, trend } from '$lib/format';
  import { livePosition } from '$lib/live';
  import { contractName, trading } from '$lib/trading.svelte';
  import type { DashView, Dashboard, PerfCell, PerfColumn, Performance } from '$lib/types';

  const MONTHS = ['Jan', 'Feb', 'Mar', 'Apr', 'May', 'Jun', 'Jul', 'Aug', 'Sep', 'Oct', 'Nov', 'Dec'];
  const VIEWS: { id: DashView; label: string }[] = [
    { id: 'daily', label: 'Daily' },
    { id: 'monthly', label: 'Monthly' },
    { id: 'yearly', label: 'Yearly' }
  ];
  const GROUP_TITLE: Record<DashView, string> = { daily: 'by hour', monthly: 'by day', yearly: 'by month' };

  const today = todayIst();
  const thisYear = Number(today.slice(0, 4));
  const years = Array.from({ length: 6 }, (_, i) => thisYear - i);

  // filters
  let view = $state<DashView>('monthly');
  let day = $state(today);
  let month = $state(Number(today.slice(5, 7)));
  let year = $state(thisYear);
  let accountId = $state('');

  const anchor = $derived(view === 'daily' ? day : `${year}-${String(month).padStart(2, '0')}`);

  // data
  let dash = $state.raw<Dashboard | null>(null);
  let perf = $state.raw<Performance | null>(null);
  let error = $state('');
  let perfError = $state('');
  let open = $state<Record<string, boolean>>({});
  let seq = 0;

  // The open positions follow the live quote stream, so they need no polling.
  const accounts = $derived(dash?.accounts ?? []);
  const positions = $derived(
    (dash?.positions ?? []).filter((p) => !accountId || p.accountId === accountId).map(livePosition)
  );
  const showAccount = $derived(!accountId && accounts.length > 1);

  async function loadDash() {
    try {
      dash = await api.getDashboard(null);
      error = '';
    } catch (e) {
      error = String(e);
    }
  }

  async function loadPerf() {
    if (!anchor) return;
    const mine = ++seq;
    try {
      const p = await api.getPerformance(view, anchor, accountId || null);
      if (mine !== seq) return; // a newer request is in flight
      perf = p;
      perfError = '';
    } catch (e) {
      if (mine === seq) perfError = String(e);
    }
  }

  // Safety net only (a missed event, a long sleep).
  onMount(() => {
    const t = every(() => {
      loadDash();
      loadPerf();
    }, 30_000);
    return () => clearInterval(t);
  });

  $effect(() => {
    trading.version;
    loadDash();
  });
  $effect(() => {
    view;
    anchor;
    accountId;
    trading.version;
    open = {};
    loadPerf();
  });

  // ------------------------------------------------------------ heat view ---

  /** Cells laid out in rows of `per`, padded with nulls (before and after). */
  function matrix(col: PerfColumn, per: number): (PerfCell | null)[][] {
    const flat: (PerfCell | null)[] = [...Array<null>(col.leadBlank).fill(null), ...col.cells];
    while (flat.length % per) flat.push(null);
    const rows: (PerfCell | null)[][] = [];
    for (let i = 0; i < flat.length; i += per) rows.push(flat.slice(i, i + per));
    return rows;
  }

  const per = $derived(view === 'daily' ? 6 : 7);
  const heatClass = $derived(view === 'daily' ? 'heat-lg' : view === 'monthly' ? 'heat-md' : 'heat-sm');
  const showValues = $derived(view !== 'yearly');

  function tip(c: PerfCell): string {
    if (c.state === 'none') return `${c.date}: no trades`;
    return `${c.date}: net ₹${signed(c.net)} · ${c.trades} trade${c.trades === 1 ? '' : 's'} closed`;
  }

  function pillClass(c: PerfColumn): string {
    if (!c.fills) return 'badge-ghost';
    if (c.net > 0) return 'badge-success';
    if (c.net < 0) return 'badge-error';
    return 'badge-warning';
  }

  const pill = (c: PerfColumn) => (!c.fills ? '—' : (c.net > 0 ? '+' : '') + compact(c.net));
  const stamp = (ts: string) => istStamp(ts, view !== 'daily' || istDay(ts) !== perf?.from);

  const periodText = $derived(
    !perf ? '' : perf.from === perf.to ? perf.from : `${perf.from} to ${perf.to}`
  );
</script>

<div class="flex flex-col gap-6">
  {#if error}<div role="alert" class="alert alert-error alert-soft py-2 text-sm break-words">{error}</div>{/if}

  <!-- Row 1: view, period, accounts -->
  <section class="flex flex-col gap-2">
    <div class="flex flex-wrap items-center gap-3">
      <h1 class="text-xl font-bold">Dashboard</h1>
      <div class="join">
        {#each VIEWS as v (v.id)}
          <button class="btn btn-sm join-item {view === v.id ? 'btn-primary' : ''}" onclick={() => (view = v.id)}>{v.label}</button>
        {/each}
      </div>

      {#if view === 'daily'}
        <input class="input input-sm w-40" type="date" bind:value={day} max={today} aria-label="Date" />
      {:else}
        <select class="select select-sm w-24" bind:value={month} aria-label="Month">
          {#each MONTHS as m, i (m)}<option value={i + 1}>{m}</option>{/each}
        </select>
        <select class="select select-sm w-24" bind:value={year} aria-label="Year">
          {#each years as y (y)}<option value={y}>{y}</option>{/each}
        </select>
      {/if}

      <select class="select select-sm w-44" bind:value={accountId} aria-label="Accounts">
        <option value="">All accounts</option>
        {#each accounts as a (a.accountId)}<option value={a.accountId}>{a.name}</option>{/each}
      </select>
      {#if periodText}<span class="num text-xs text-base-content/60">{periodText}</span>{/if}
    </div>
    <p class="text-xs text-base-content/60">
      Realised P&L and charges by the time of each fill, closed trades only. Open positions are not included (see the top bar for live P&L).
    </p>
  </section>

  {#if dash && !accounts.length}
    <div class="card border border-base-300 bg-base-100">
      <div class="card-body items-center py-10 text-center">
        <p class="font-medium">No trading accounts yet.</p>
        <a href="/accounts" class="btn btn-primary btn-sm">Add a Paper or Dhan account</a>
      </div>
    </div>
  {/if}

  {#if perfError}<div role="alert" class="alert alert-error alert-soft py-2 text-sm break-words">{perfError}</div>{/if}

  <!-- Row 2: cards -->
  <section class="grid grid-cols-2 gap-3 lg:grid-cols-5">
    {#each [
      { label: 'Net P&L', value: perf ? '₹' + signed(perf.summary.net) : '—', cls: trend(perf?.summary.net), hint: 'gross − charges' },
      { label: 'Gross P&L', value: perf ? '₹' + signed(perf.summary.gross) : '—', cls: trend(perf?.summary.gross), hint: 'before charges' },
      { label: 'Trades', value: perf ? String(perf.summary.trades) : '—', cls: '', hint: 'closed, entry + exit' },
      { label: 'Total charges', value: perf ? '₹' + num(perf.summary.charges) : '—', cls: '', hint: 'brokerage, taxes, fees' },
      {
        label: 'ROI',
        value: perf && perf.summary.roi !== null ? signed(perf.summary.roi) + '%' : '—',
        cls: trend(perf?.summary.roi),
        hint: perf ? `on ₹${num(perf.summary.entryPremium, 0)} entry premium` : 'net / entry premium'
      }
    ] as c (c.label)}
      <div class="card border border-base-300 bg-base-100">
        <div class="card-body gap-1 p-4">
          <div class="text-xs text-base-content/60">{c.label}</div>
          <div class="num text-xl font-bold {c.cls}">{c.value}</div>
          <div class="text-xs text-base-content/60">{c.hint}</div>
        </div>
      </div>
    {/each}
  </section>

  <!-- Row 3: calendar heat view -->
  {#if perf}
    <section class="overflow-x-auto rounded-box border border-base-300 bg-base-100 p-3">
      <table class="w-full table-fixed border-separate border-spacing-y-2" style:min-width={view === 'yearly' ? '1380px' : undefined}>
        <thead>
          <tr>
            {#each perf.columns as c (c.key)}
              <th class="text-center text-xs font-semibold {c.current ? 'text-primary' : 'text-base-content/70'}">{c.label}</th>
            {/each}
          </tr>
        </thead>
        <tbody>
          <tr>
            {#each perf.columns as c (c.key)}
              <td class="align-top">
                <table class="heat {heatClass}">
                  <tbody>
                    {#each matrix(c, per) as row, ri (ri)}
                      <tr>
                        {#each row as cell, ci (ci)}
                          {#if cell}
                            <td class="h-{cell.state}" title={tip(cell)}>{showValues ? cell.label : ''}</td>
                          {:else}
                            <td class="h-pad"></td>
                          {/if}
                        {/each}
                      </tr>
                    {/each}
                  </tbody>
                </table>
              </td>
            {/each}
          </tr>
          <tr>
            {#each perf.columns as c (c.key)}
              <td class="text-center">
                <span class="badge badge-sm num {pillClass(c)}" title="{c.trades} trade{c.trades === 1 ? '' : 's'} closed">{pill(c)}</span>
              </td>
            {/each}
          </tr>
        </tbody>
      </table>
    </section>
  {/if}

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

  <!-- Trader's diary, grouped like the selected view -->
  <section class="flex flex-col gap-3">
    <div class="flex items-baseline gap-3">
      <h2 class="font-semibold">Trader's diary</h2>
      <span class="text-xs text-base-content/60">{GROUP_TITLE[view]}</span>
    </div>

    <div class="overflow-x-auto rounded-box border border-base-300 bg-base-100">
      <table class="table table-sm">
        <thead>
          <tr>
            <th></th>
            <th>{view === 'daily' ? 'Hour' : view === 'monthly' ? 'Day' : 'Month'}</th>
            <th class="text-right">Orders</th>
            <th class="text-right">Trades</th>
            <th class="text-right">Gross</th>
            <th class="text-right">Charges</th>
            <th class="text-right">Net</th>
          </tr>
        </thead>
        <tbody>
          {#each perf?.groups ?? [] as g (g.key)}
            <tr class="cursor-pointer hover:bg-base-200" onclick={() => (open[g.key] = !open[g.key])}>
              <td class="w-6">{g.rows.length ? (open[g.key] ? '▾' : '▸') : ''}</td>
              <td class="num">{g.label}</td>
              <td class="num text-right">{g.orders}</td>
              <td class="num text-right">{g.trades}</td>
              <td class="num text-right {trend(g.gross)}">{signed(g.gross)}</td>
              <td class="num text-right">{num(g.charges)}</td>
              <td class="num text-right font-semibold {trend(g.net)}">{signed(g.net)}</td>
            </tr>
            {#if open[g.key] && g.rows.length}
              <tr>
                <td></td>
                <td colspan="6" class="bg-base-200 p-0">
                  <div class="overflow-x-auto">
                    <table class="table table-xs">
                      <thead>
                        <tr>
                          {#if showAccount}<th>Account</th>{/if}
                          <th>Contract</th>
                          <th>Side</th>
                          <th class="text-right">Qty</th>
                          <th class="text-right">Entry</th>
                          <th class="text-right">Exit</th>
                          <th class="text-right">Gross</th>
                          <th class="text-right">Charges</th>
                          <th class="text-right">Net</th>
                        </tr>
                      </thead>
                      <tbody>
                        {#each g.rows as t (t.id)}
                          <tr>
                            {#if showAccount}<td>{t.accountName}</td>{/if}
                            <td class="whitespace-nowrap">{contractName(t)} <span class="text-base-content/50">{t.product === 'INTRADAY' ? 'MIS' : 'NRML'}</span></td>
                            <td class={t.long ? 'up' : 'down'}>{t.long ? 'Long' : 'Short'}</td>
                            <td class="num text-right">{t.qty}</td>
                            <td class="num text-right whitespace-nowrap">{stamp(t.entryTs)} @ {num(t.entryPrice)}</td>
                            <td class="num text-right whitespace-nowrap">
                              {#if t.closed && t.exitTs}{stamp(t.exitTs)} @ {num(t.exitPrice)}{:else}<span class="badge badge-xs badge-warning">open</span>{/if}
                            </td>
                            <td class="num text-right {trend(t.gross)}">{t.gross ? signed(t.gross) : '—'}</td>
                            <td class="num text-right">{num(t.charges)}</td>
                            <td class="num text-right font-semibold {trend(t.net)}">{signed(t.net)}</td>
                          </tr>
                        {/each}
                      </tbody>
                    </table>
                  </div>
                </td>
              </tr>
            {/if}
          {:else}
            <tr><td colspan="7" class="py-8 text-center text-base-content/60">No activity in this period.</td></tr>
          {/each}
        </tbody>
        {#if perf?.groups.length}
          <tfoot>
            <tr class="font-semibold">
              <td colspan="3" class="text-right">Total</td>
              <td class="num text-right">{perf.summary.trades}</td>
              <td class="num text-right {trend(perf.summary.gross)}">{signed(perf.summary.gross)}</td>
              <td class="num text-right">{num(perf.summary.charges)}</td>
              <td class="num text-right {trend(perf.summary.net)}">{signed(perf.summary.net)}</td>
            </tr>
          </tfoot>
        {/if}
      </table>
    </div>
    <p class="text-xs text-base-content/60">
      A trade is entry + exit. Each trade is listed where it closed with its full result; the row totals are by fill time, so entry charges of a trade that closed in a later {view === 'daily' ? 'hour' : view === 'monthly' ? 'day' : 'month'} stay in the earlier row. Open trades are listed with their charges so far.
    </p>
  </section>
</div>
