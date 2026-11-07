<script lang="ts">
  import { api } from '$lib/api';
  import { num } from '$lib/format';
  import { contractName } from '$lib/trading.svelte';
  import type { Position } from '$lib/types';

  let { position, ondone }: { position: Position; ondone?: () => void } = $props();

  let open = $state(false);
  let busy = $state(false);
  let error = $state('');

  const side = $derived(position.netQty > 0 ? 'SELL' : 'BUY');
  const qty = $derived(Math.abs(position.netQty));
  const lots = $derived(position.lotSize > 0 ? qty / position.lotSize : 0);

  const blocked = $derived(
    position.exitPrice === null
      ? 'No LTP available yet'
      : !Number.isInteger(lots) || lots < 1
        ? 'Quantity is not a whole number of lots'
        : ''
  );

  async function confirm() {
    if (blocked) return;
    busy = true;
    error = '';
    try {
      await api.exitPosition(position.accountId, position.instrument, position.product);
      open = false;
      ondone?.();
    } catch (e) {
      error = String(e);
    } finally {
      busy = false;
    }
  }
</script>

<button class="btn btn-outline btn-error btn-xs" disabled={!!blocked} title={blocked || 'Close this position'}
  onclick={() => { error = ''; open = true; }}>Exit</button>

{#if open}
  <dialog class="modal modal-open">
    <div class="modal-box max-w-sm">
      <h3 class="text-lg font-bold">Exit position</h3>
      <p class="text-sm text-base-content/60">{position.accountName} · {contractName(position)}</p>
      <div class="mt-4 grid grid-cols-2 gap-y-1 text-sm">
        <span class="text-base-content/60">Order</span>
        <span class="font-semibold {side === 'BUY' ? 'up' : 'down'}">{side} LIMIT</span>
        <span class="text-base-content/60">Quantity</span>
        <span class="num">{qty} ({lots} lot{lots === 1 ? '' : 's'})</span>
        <span class="text-base-content/60">Limit price</span>
        <span class="num">{num(position.exitPrice)} <span class="text-xs text-base-content/50">(LTP {num(position.ltp)} + buffer)</span></span>
        <span class="text-base-content/60">Product</span>
        <span>{position.product === 'INTRADAY' ? 'MIS' : 'NRML'}</span>
      </div>
      <p class="mt-3 text-xs text-base-content/60">Open orders on this contract are cancelled first.</p>
      {#if error}<div role="alert" class="alert alert-error alert-soft mt-3 py-2 text-sm break-words">{error}</div>{/if}
      <div class="modal-action">
        <button class="btn btn-ghost btn-sm" onclick={() => (open = false)} disabled={busy}>Close</button>
        <button class="btn btn-error btn-sm" onclick={confirm} disabled={busy}>
          {#if busy}<span class="loading loading-spinner loading-xs"></span>{/if} Place {side} order
        </button>
      </div>
    </div>
    <button class="modal-backdrop" aria-label="Close" onclick={() => (open = false)}></button>
  </dialog>
{/if}
