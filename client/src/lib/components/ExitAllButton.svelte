<script lang="ts">
  import { api } from '$lib/api';
  import type { ExitAllResult, Position } from '$lib/types';

  let {
    positions,
    accountId = null,
    ondone
  }: { positions: Position[]; accountId?: string | null; ondone?: () => void } = $props();

  let open = $state(false);
  let busy = $state(false);
  let error = $state('');
  let result = $state<ExitAllResult | null>(null);

  const accountCount = $derived(new Set(positions.map((p) => p.accountId)).size);

  function show() {
    error = '';
    result = null;
    open = true;
  }

  async function confirm() {
    busy = true;
    error = '';
    try {
      result = await api.exitAll(accountId);
      ondone?.();
    } catch (e) {
      error = String(e);
    } finally {
      busy = false;
    }
  }
</script>

<button class="btn btn-error btn-sm" disabled={!positions.length} onclick={show}>Exit all</button>

{#if open}
  <dialog class="modal modal-open">
    <div class="modal-box max-w-md">
      <h3 class="text-lg font-bold">Exit all positions</h3>
      {#if !result}
        <p class="mt-2 text-sm">
          Close {positions.length} open position{positions.length === 1 ? '' : 's'}
          across {accountCount} account{accountCount === 1 ? '' : 's'}. Open orders on those contracts are
          cancelled first, then a marketable LIMIT order is placed for each position.
        </p>
      {:else}
        <p class="mt-2 text-sm">
          Placed {result.placed} exit order{result.placed === 1 ? '' : 's'}; cancelled {result.cancelled} open order{result.cancelled === 1 ? '' : 's'}.
        </p>
        {#if result.skipped.length}
          <div role="alert" class="alert alert-warning alert-soft mt-3 flex-col items-start py-2 text-xs break-words">
            <span class="font-semibold">Not exited:</span>
            {#each result.skipped as s}<span>{s}</span>{/each}
          </div>
        {/if}
      {/if}
      {#if error}<div role="alert" class="alert alert-error alert-soft mt-3 py-2 text-sm break-words">{error}</div>{/if}
      <div class="modal-action">
        <button class="btn btn-ghost btn-sm" onclick={() => (open = false)} disabled={busy}>Close</button>
        {#if !result}
          <button class="btn btn-error btn-sm" onclick={confirm} disabled={busy}>
            {#if busy}<span class="loading loading-spinner loading-xs"></span>{/if} Exit all
          </button>
        {/if}
      </div>
    </div>
    <button class="modal-backdrop" aria-label="Close" onclick={() => (open = false)}></button>
  </dialog>
{/if}
