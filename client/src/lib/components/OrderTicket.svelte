<script lang="ts">
  import { every } from '$lib/timers';
  import { api } from '$lib/api';
  import { num } from '$lib/format';
  import { quotes } from '$lib/quotes.svelte';
  import { contractName } from '$lib/trading.svelte';
  import type {
    Account,
    ContractInfo,
    OrderRow,
    OrderType,
    Product,
    QuoteView,
    Side,
    TicketRequest,
    Validity
  } from '$lib/types';

  let {
    request,
    onclose,
    onplaced
  }: {
    request: TicketRequest | null;
    onclose: () => void;
    onplaced?: (o: OrderRow) => void;
  } = $props();

  let accounts = $state<Account[]>([]);
  let accountId = $state('');
  let side = $state<Side>('BUY');
  let product = $state<Product>('INTRADAY');
  let orderType = $state<OrderType>('LIMIT');
  let validity = $state<Validity>('DAY');
  let lots = $state(1);
  let price = $state<number | null>(null);
  let trigger = $state<number | null>(null);
  // Stop-loss / target attached to the entry (paper accounts only).
  let useBracket = $state(false);
  let target = $state<number | null>(null);
  let stopTrigger = $state<number | null>(null);
  let info = $state<ContractInfo | null>(null);
  let infoError = $state('');
  let error = $state('');
  let placed = $state<OrderRow | null>(null);
  let busy = $state(false);
  let refreshing = $state(false);
  let quote = $state<QuoteView | null>(null);

  // The price follows the pushed quote stream tick by tick; `quote` (from the
  // backend) only names the instrument and covers the time before the first tick.
  const px = $derived(quote ? quotes.get(quote.key) : undefined);
  const ltp = $derived(px ? px.ltp : (quote?.ltp ?? null));
  const isLive = $derived(px ? quotes.isFresh(px) : (quote?.live ?? false));
  const ageSecs = $derived(px ? Math.max(0, quotes.nowSecs - px.rx) : (quote?.ageSecs ?? null));

  const account = $derived(accounts.find((a) => a.id === accountId) ?? null);
  const qty = $derived(info?.lotSize ? lots * info.lotSize : null);
  const canBracket = $derived(account?.kind === 'paper');
  const bracketOn = $derived(useBracket && canBracket);
  const needsSecurityId = $derived(account?.kind === 'dhan' && info !== null && !info.securityId);

  // Load tradable accounts once.
  $effect(() => {
    api
      .listAccounts()
      .then((list) => {
        accounts = list.filter((a) => a.kind === 'paper' || a.kind === 'dhan');
        if (!accounts.some((a) => a.id === accountId)) accountId = accounts[0]?.id ?? '';
      })
      .catch((e) => (error = String(e)));
  });

  // Reset the form whenever a new strike is clicked.
  $effect(() => {
    const r = request;
    if (!r) return;
    side = r.side;
    price = r.price;
    trigger = null;
    useBracket = false;
    target = null;
    stopTrigger = null;
    orderType = 'LIMIT';
    lots = 1;
    error = '';
    placed = null;
    loadInfo(r);
  });

  // Look the contract up while the ticket is open. Ticks arrive through the
  // `quotes` store; the slow poll only re-reads the stored price when the
  // market is quiet or closed.
  $effect(() => {
    const r = request;
    quote = null;
    if (!r) return;
    let stop = false;
    const poll = async () => {
      try {
        const q = await api.getQuote(r.symbol, r.expiry, r.strike, r.isCall);
        if (!stop) quote = q;
      } catch {
        /* keep the last value */
      }
    };
    poll();
    const t = every(poll, 5000);
    return () => {
      stop = true;
      clearInterval(t);
    };
  });

  function useLtp() {
    if (ltp) price = Math.max(0.05, Math.round(ltp * 20) / 20);
  }

  async function loadInfo(r: TicketRequest) {
    info = null;
    infoError = '';
    try {
      info = await api.contractInfo(r.symbol, r.expiry, r.strike, r.isCall);
    } catch (e) {
      infoError = String(e);
    }
  }

  async function refreshInstruments() {
    if (!request) return;
    refreshing = true;
    try {
      await api.refreshInstruments();
      await loadInfo(request);
    } catch (e) {
      infoError = String(e);
    } finally {
      refreshing = false;
    }
  }

  async function submit() {
    if (!request) return;
    error = '';
    if (!accountId) return (error = 'Create a Paper or Dhan account first.');
    if (!price || price <= 0) return (error = 'Enter a limit price.');
    if (orderType === 'STOP_LOSS' && (!trigger || trigger <= 0)) return (error = 'Enter a trigger price.');
    if (!Number.isInteger(lots) || lots < 1) return (error = 'Lots must be a whole number ≥ 1.');
    if (bracketOn && !target && !stopTrigger) return (error = 'Enter a target, a stop-loss, or turn the bracket off.');
    busy = true;
    try {
      placed = await api.placeOrder({
        accountId,
        symbol: request.symbol,
        expiry: request.expiry,
        strike: request.strike,
        isCall: request.isCall,
        side,
        product,
        orderType,
        validity,
        lots,
        price,
        triggerPrice: orderType === 'STOP_LOSS' ? (trigger ?? undefined) : undefined,
        bracket: bracketOn ? { target: target || undefined, stopTrigger: stopTrigger || undefined } : undefined
      });
      onplaced?.(placed);
    } catch (e) {
      error = String(e);
    } finally {
      busy = false;
    }
  }
</script>

{#if request}
  <aside
    class="fixed inset-y-0 right-0 z-30 flex w-80 flex-col border-l border-base-300 bg-base-100 shadow-xl"
    aria-label="Order ticket"
  >
    <div class="flex items-center gap-2 border-b border-base-300 px-4 py-3">
      <div class="min-w-0">
        <div class="truncate font-semibold">{contractName(request)}</div>
        <div class="text-xs text-base-content/60">
          {#if info?.lotSize}Lot size {info.lotSize}{#if info.lotSource === 'fallback'} (fallback){/if}{:else}Lot size —{/if}
        </div>
      </div>
      <button class="btn btn-ghost btn-xs ml-auto" onclick={onclose} aria-label="Close">✕</button>
    </div>

    <div class="flex flex-1 flex-col gap-3 overflow-auto p-4 text-sm">
      <div class="join w-full">
        <button
          class="btn btn-sm join-item flex-1 {side === 'BUY' ? 'btn-success' : 'btn-outline'}"
          onclick={() => (side = 'BUY')}>BUY</button>
        <button
          class="btn btn-sm join-item flex-1 {side === 'SELL' ? 'btn-error' : 'btn-outline'}"
          onclick={() => (side = 'SELL')}>SELL</button>
      </div>

      <label class="flex flex-col gap-1">
        <span class="text-xs text-base-content/60">Account</span>
        <select class="select select-sm w-full" bind:value={accountId}>
          {#each accounts as a}<option value={a.id}>{a.name} ({a.kind})</option>{/each}
        </select>
        {#if !accounts.length}
          <a href="/accounts" class="link text-xs">Add a Paper or Dhan account</a>
        {/if}
      </label>

      <div class="grid grid-cols-2 gap-3">
        <label class="flex flex-col gap-1">
          <span class="text-xs text-base-content/60">Product</span>
          <select class="select select-sm" bind:value={product}>
            <option value="INTRADAY">Intraday</option>
            <option value="MARGIN">Margin (carry)</option>
          </select>
        </label>
        <label class="flex flex-col gap-1">
          <span class="text-xs text-base-content/60">Validity</span>
          <select class="select select-sm" bind:value={validity}>
            <option value="DAY">Day</option>
            <option value="IOC">IOC</option>
          </select>
        </label>
        <label class="flex flex-col gap-1">
          <span class="text-xs text-base-content/60">Type</span>
          <select class="select select-sm" bind:value={orderType}>
            <option value="LIMIT">Limit</option>
            <option value="STOP_LOSS">Stop-loss</option>
          </select>
        </label>
        <label class="flex flex-col gap-1">
          <span class="text-xs text-base-content/60">Lots</span>
          <input class="input input-sm num" type="number" min="1" step="1" bind:value={lots} />
        </label>
        <label class="flex flex-col gap-1">
          <span class="text-xs text-base-content/60">Price</span>
          <input class="input input-sm num" type="number" min="0.05" step="0.05" bind:value={price} />
        </label>
        {#if orderType === 'STOP_LOSS'}
          <label class="flex flex-col gap-1">
            <span class="text-xs text-base-content/60">Trigger</span>
            <input class="input input-sm num" type="number" min="0.05" step="0.05" bind:value={trigger} />
          </label>
        {/if}
      </div>

      {#if canBracket}
        <div class="flex flex-col gap-2 rounded-lg border border-base-300 px-3 py-2">
          <label class="flex cursor-pointer items-center justify-between text-xs">
            <span class="font-medium">Stop-loss / target <span class="text-base-content/50">(bracket)</span></span>
            <input type="checkbox" class="toggle toggle-xs" bind:checked={useBracket} />
          </label>
          {#if useBracket}
            <div class="grid grid-cols-2 gap-3">
              <label class="flex flex-col gap-1">
                <span class="text-xs text-base-content/60">Target</span>
                <input class="input input-sm num" type="number" min="0.05" step="0.05" bind:value={target}
                  placeholder={side === 'BUY' ? 'above entry' : 'below entry'} />
              </label>
              <label class="flex flex-col gap-1">
                <span class="text-xs text-base-content/60">Stop-loss</span>
                <input class="input input-sm num" type="number" min="0.05" step="0.05" bind:value={stopTrigger}
                  placeholder={side === 'BUY' ? 'below entry' : 'above entry'} />
              </label>
            </div>
            <p class="text-xs text-base-content/50">
              Placed when the entry fills. When one executes, the other is cancelled.
            </p>
          {/if}
        </div>
      {/if}

      <div class="rounded-lg bg-base-200 px-3 py-2 text-xs">
        <div class="flex items-center justify-between">
          <span class="text-base-content/60">LTP</span>
          <span class="flex items-center gap-2">
            <span class="num text-sm font-semibold">{ltp != null ? num(ltp) : '—'}</span>
            {#if ltp != null}
              <span class="badge badge-xs {isLive ? 'badge-success' : 'badge-ghost'}"
                title={ageSecs != null ? `${ageSecs}s old` : ''}>{isLive ? 'live' : 'last'}</span>
              <button class="btn btn-ghost btn-xs" onclick={useLtp} title="Set the price to the LTP">Use</button>
            {/if}
          </span>
        </div>
        <div class="flex justify-between">
          <span class="text-base-content/60">Quantity</span>
          <span class="num">{qty ?? '—'}</span>
        </div>
        <div class="flex justify-between">
          <span class="text-base-content/60">Order value</span>
          <span class="num">{qty && price ? '₹' + num(qty * price) : '—'}</span>
        </div>
      </div>

      {#if infoError}<div class="alert alert-warning alert-soft py-1 text-xs">{infoError}</div>{/if}
      {#if info?.lotSource === 'fallback' && account?.kind === 'dhan'}
        <div class="text-xs text-warning">Lot size is from settings, not Dhan's instrument list.</div>
      {/if}
      {#if needsSecurityId}
        <div class="alert alert-warning alert-soft flex-col items-start gap-2 py-2 text-xs">
          <span>Dhan security id not found for this contract.</span>
          <button class="btn btn-xs" onclick={refreshInstruments} disabled={refreshing}>
            {#if refreshing}<span class="loading loading-spinner loading-xs"></span>{/if}
            Refresh instruments
          </button>
        </div>
      {/if}

      {#if error}<div role="alert" class="alert alert-error alert-soft py-2 text-xs break-words">{error}</div>{/if}
      {#if placed}
        <div role="status" class="alert alert-success alert-soft py-2 text-xs">
          Order {placed.status.toLowerCase()}{placed.message ? ` — ${placed.message}` : ''}.
          <a href="/orders" class="link">View orders</a>
        </div>
      {/if}
    </div>

    <div class="border-t border-base-300 p-4">
      <button
        class="btn w-full {side === 'BUY' ? 'btn-success' : 'btn-error'}"
        onclick={submit}
        disabled={busy || !accountId}
      >
        {#if busy}<span class="loading loading-spinner loading-sm"></span>{/if}
        {side} {lots} lot{lots === 1 ? '' : 's'}
      </button>
    </div>
  </aside>
{/if}
