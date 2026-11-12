<script lang="ts">
  import { onMount, onDestroy } from 'svelte';
  import { listen, type UnlistenFn } from '@tauri-apps/api/event';
  import { api } from '$lib/api';
  import { getScoped, setScoped } from '$lib/windowing';
  import { DEFAULT_VISIBLE, STRIKE_WINDOWS } from '$lib/columns';
  import { compact, num, signed, trend } from '$lib/format';
  import { market } from '$lib/market.svelte';
  import ChainTable from '$lib/components/ChainTable.svelte';
  import ColumnPicker from '$lib/components/ColumnPicker.svelte';
  import OrderTicket from '$lib/components/OrderTicket.svelte';
  import PositionsDock from '$lib/components/PositionsDock.svelte';
  import type { ChainSnapshot, ChainStatus, ExpiryItem, Position, Source, TicketRequest } from '$lib/types';

  let source = $state<Source>('nse');
  let symbols = $state<string[]>([]);
  let symbol = $state('NIFTY');
  let expiries = $state<ExpiryItem[]>([]);
  let expiryValue = $state('');
  // Replaced wholesale on every push and never edited in place, so skip the
  // deep reactive proxy (a full chain is hundreds of nested objects).
  let snapshot = $state.raw<ChainSnapshot | null>(null);
  let error = $state('');
  let busy = $state(false);
  let visible = $state<string[]>([...DEFAULT_VISIBLE]);
  let strikeWindow = $state(10);
  let ticket = $state<TicketRequest | null>(null);

  const expiry = $derived(expiries.find((e) => e.value === expiryValue) ?? null);
  let unlisten: UnlistenFn[] = [];
  let token = 0;

  onMount(async () => {
    unlisten.push(
      await listen<ChainSnapshot>('chain-snapshot', (e) => {
        const s = e.payload;
        if (s.source === source && s.symbol === symbol && s.expiry === expiry?.label) {
          snapshot = s;
          error = '';
        }
      }),
      await listen<ChainStatus>('chain-status', (e) => {
        if (e.payload.state === 'error') error = e.payload.message;
      })
    );
    source = (await api.getSetting('data.source').catch(() => null)) === 'fyers' ? 'fyers' : 'nse';
    visible = await getScoped('chain.columns', [...DEFAULT_VISIBLE]);
    strikeWindow = await getScoped('chain.window', 10);
    symbol = await getScoped('chain.symbol', 'NIFTY');
    symbols = await api.listFnoSymbols().catch(() => ['NIFTY', 'BANKNIFTY']);
    if (!symbols.includes(symbol)) symbol = symbols[0] ?? 'NIFTY';
    await loadExpiries();
  });

  onDestroy(() => {
    unlisten.forEach((u) => u());
    api.stopChain().catch(() => {});
  });

  /** `preferDate` (ISO) picks that expiry instead of the nearest one, when the source lists it. */
  async function loadExpiries(preferDate?: string) {
    const mine = ++token;
    busy = true;
    error = '';
    snapshot = null;
    expiries = [];
    try {
      const list = await api.listExpiries(source, symbol);
      if (mine !== token) return;
      expiries = list;
      const wanted = preferDate ? list.find((e) => e.date === preferDate) : undefined;
      expiryValue = (wanted ?? list[0])?.value ?? '';
      if (!list.length) throw new Error('No expiries returned for this symbol.');
      await start();
      if (preferDate && !wanted && mine === token) {
        error = `${source === 'fyers' ? 'Fyers' : 'NSE'} does not list the ${preferDate} expiry for ${symbol}; showing the nearest one.`;
      }
    } catch (e) {
      if (mine === token) error = String(e);
      await api.stopChain().catch(() => {});
    } finally {
      if (mine === token) busy = false;
    }
  }

  async function start() {
    if (!expiry) return;
    snapshot = null;
    error = '';
    try {
      await api.startChain(source, symbol, expiry);
    } catch (e) {
      error = String(e);
    }
  }

  async function pickSource(s: Source) {
    if (s === source) return;
    source = s;
    await api.setSetting('data.source', s).catch(() => {});
    await loadExpiries();
  }

  async function pickSymbol() {
    await setScoped('chain.symbol', symbol).catch(() => {});
    await loadExpiries();
  }

  /** Positions dock: switch the chain to the position's underlying and expiry. */
  async function showPosition(p: Position) {
    if (busy) return;
    if (p.symbol !== symbol) {
      if (!symbols.includes(p.symbol)) {
        error = `${p.symbol} is not in the F&O symbol list.`;
        return;
      }
      symbol = p.symbol;
      await setScoped('chain.symbol', symbol).catch(() => {});
      await loadExpiries(p.expiry);
    } else if (expiry?.date !== p.expiry) {
      const e = expiries.find((x) => x.date === p.expiry);
      if (!e) {
        error = `${source === 'fyers' ? 'Fyers' : 'NSE'} does not list the ${p.expiry} expiry for ${symbol}.`;
        return;
      }
      expiryValue = e.value;
      await start();
    }
  }

  const saveColumns = () => setScoped('chain.columns', visible).catch(() => {});
  const saveWindow = () => setScoped('chain.window', strikeWindow).catch(() => {});

  const optionStream = $derived(
    market.status?.streams.find((s) => s.name.startsWith('options:'))
  );

  // What the selected source's exchange status says (holidays included).
  const sourceStatus = $derived(source === 'fyers' ? market.status?.fyers : market.status?.nse);
  const sourceName = $derived(source === 'fyers' ? 'Fyers' : 'NSE');
  const marketClosed = $derived(sourceStatus?.state === 'closed');
  const marketPreOpen = $derived(sourceStatus?.state === 'preOpen');
</script>

<div class="flex min-h-full flex-col gap-4">
  <!-- controls -->
  <div class="flex flex-wrap items-center gap-3">
    <div class="join">
      <button class="btn btn-sm join-item {source === 'nse' ? 'btn-primary' : 'btn-outline'}" onclick={() => pickSource('nse')}>NSE</button>
      <button class="btn btn-sm join-item {source === 'fyers' ? 'btn-primary' : 'btn-outline'}" onclick={() => pickSource('fyers')}>Fyers</button>
    </div>

    <select class="select select-sm w-40" bind:value={symbol} onchange={pickSymbol} disabled={busy}>
      {#each symbols as s}<option value={s}>{s}</option>{/each}
    </select>

    <select class="select select-sm w-40" bind:value={expiryValue} onchange={start} disabled={busy || !expiries.length}>
      {#each expiries as e}<option value={e.value}>{e.label}</option>{/each}
    </select>

    <select class="select select-sm w-32" bind:value={strikeWindow} onchange={saveWindow}>
      {#each STRIKE_WINDOWS as w}<option value={w}>{w ? `ATM ± ${w}` : 'All strikes'}</option>{/each}
    </select>

    {#if busy}<span class="loading loading-spinner loading-sm text-primary"></span>{/if}

    <div class="ml-auto"><ColumnPicker bind:visible onchange={saveColumns} /></div>
  </div>

  {#if error}
    <div role="alert" class="alert alert-error alert-soft py-2 text-sm">
      <span>{error}</span>
      {#if source === 'fyers'}<a href="/accounts" class="link">Open Accounts</a>{/if}
    </div>
  {/if}

  {#if marketClosed && snapshot}
    <div role="status" class="alert alert-soft py-2 text-sm">
      <span>
        {sourceName} reports the market closed (weekend, holiday or after the close).
        {source === 'fyers'
          ? "Showing the last fetched data; polling resumes when it opens."
          : 'The table shows what was last received; ticks resume when it opens.'}
      </span>
    </div>
  {:else if marketPreOpen}
    <div role="status" class="alert alert-soft py-2 text-sm">
      <span>{sourceName} is in pre-open. Option prices start at 9:15 IST.</span>
    </div>
  {/if}

  <!-- summary strip -->
  {#if snapshot}
    <div class="stats w-full border border-base-300 bg-base-100 text-sm">
      <div class="stat px-4 py-2">
        <div class="stat-title text-xs">{snapshot.underlyingName ?? snapshot.symbol}</div>
        <div class="stat-value num text-xl">{num(snapshot.underlying)}</div>
        <div class="stat-desc num {trend(snapshot.underlyingChange)}">
          {signed(snapshot.underlyingChange)} ({signed(snapshot.underlyingChangePct)}%)
        </div>
      </div>
      <div class="stat px-4 py-2">
        <div class="stat-title text-xs">ATM</div>
        <div class="stat-value num text-xl">{num(snapshot.atm, 0)}</div>
        <div class="stat-desc">India VIX {num(snapshot.vix)}</div>
      </div>
      <div class="stat px-4 py-2">
        <div class="stat-title text-xs">PCR (OI)</div>
        <div class="stat-value num text-xl">{num(snapshot.pcr)}</div>
        <div class="stat-desc num">Calls {compact(snapshot.callOi)} · Puts {compact(snapshot.putOi)}</div>
      </div>
      <div class="stat px-4 py-2">
        <div class="stat-title text-xs">Source</div>
        <div class="stat-value text-xl uppercase">{snapshot.source}</div>
        <div class="stat-desc">Greeks: {snapshot.greeksSource}</div>
      </div>
    </div>

    <ChainTable
      {snapshot}
      {visible}
      window={strikeWindow}
      onpick={(r) => expiry && (ticket = { ...r, symbol, expiry: expiry.date })}
    />

    {#if snapshot.source === 'nse'}
      <p class="text-xs text-base-content/50">
        Click a bid, ask or LTP to open an order ticket. OI Chg is measured from the first OI seen this session unless NSE supplies it. IV and Greeks are computed
        from LTP (Black-76, r = 7%).
      </p>
    {/if}
  {:else if !busy && !error}
    <div class="card border border-base-300 bg-base-100">
      <div class="card-body items-center py-16 text-center">
        <span class="loading loading-dots loading-md text-primary"></span>
        <p class="font-medium">Waiting for option ticks…</p>
        <p class="max-w-md text-sm text-base-content/60">
          {#if source === 'nse' && marketClosed}
            NSE reports the market closed (weekend, holiday or outside 9:15–15:30 IST), so there is nothing to stream.
            The app checks the exchange status and starts streaming when it opens.
            Switch to Fyers to view the last session's data now.
          {:else if source === 'nse' && marketPreOpen}
            NSE is in pre-open. Option ticks start at 9:15 IST.
          {:else if source === 'nse'}
            Stream: {optionStream?.state ?? 'starting'}{optionStream?.lastError ? ` — ${optionStream.lastError}` : ''}. The table appears once the index price arrives, so ATM is correct.
          {:else}
            Fetching from Fyers…
          {/if}
        </p>
      </div>
    </div>
  {/if}

  <!-- Stays at the bottom of the viewport while the chain scrolls. -->
  <div class="sticky bottom-0 z-10 mt-auto">
    <PositionsDock
      onpick={showPosition}
      isActive={(p) => p.symbol === symbol && p.expiry === expiry?.date}
      hint="Click a contract to open its underlying and expiry"
    />
  </div>
</div>

<OrderTicket request={ticket} onclose={() => (ticket = null)} />
