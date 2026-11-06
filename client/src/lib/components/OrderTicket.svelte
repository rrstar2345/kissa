<script lang="ts">
  import { api } from '$lib/api';
  import { num } from '$lib/format';
  import { contractName } from '$lib/trading.svelte';
  import type {
    Account,
    ContractInfo,
    OrderRow,
    OrderType,
    Product,
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
  let info = $state<ContractInfo | null>(null);
  let infoError = $state('');
  let error = $state('');
  let placed = $state<OrderRow | null>(null);
  let busy = $state(false);
  let refreshing = $state(false);

  const account = $derived(accounts.find((a) => a.id === accountId) ?? null);
  const qty = $derived(info?.lotSize ? lots * info.lotSize : null);
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
    orderType = 'LIMIT';
    lots = 1;
    error = '';
    placed = null;
    loadInfo(r);
  });

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
        triggerPrice: orderType === 'STOP_LOSS' ? (trigger ?? undefined) : undefined
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

      <div class="rounded-lg bg-base-200 px-3 py-2 text-xs">
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
