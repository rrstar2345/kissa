<script lang="ts">
  import { onMount } from 'svelte';
  import { api } from '$lib/api';
  import { trading } from '$lib/trading.svelte';
  import type { HaltInfo, KillResult } from '$lib/types';

  let kill = $state<HaltInfo | null>(null);
  let open = $state(false);
  let flatten = $state(false);
  let busy = $state(false);
  let error = $state('');
  let result = $state<KillResult | null>(null);

  async function load() {
    try {
      kill = (await api.getRisk()).killSwitch;
    } catch {
      /* the header must never break the app */
    }
  }
  onMount(load);
  $effect(() => {
    trading.riskVersion;
    load();
  });

  function show() {
    error = '';
    result = null;
    flatten = false;
    open = true;
  }

  async function engage() {
    busy = true;
    error = '';
    try {
      result = await api.setKillSwitch(true, flatten);
      await load();
    } catch (e) {
      error = String(e);
    } finally {
      busy = false;
    }
  }

  async function release() {
    busy = true;
    try {
      await api.setKillSwitch(false);
      await load();
    } catch (e) {
      error = String(e);
    } finally {
      busy = false;
    }
  }
</script>

{#if kill}
  <button class="btn btn-error btn-xs gap-1" onclick={release} disabled={busy} title="Kill switch is on: new positions are blocked. Click to release.">
    ■ HALTED · Release
  </button>
{:else}
  <button class="btn btn-outline btn-error btn-xs" onclick={show} title="Stop all new positions on every account">Kill switch</button>
{/if}

{#if open}
  <dialog class="modal modal-open">
    <div class="modal-box max-w-md">
      <h3 class="text-lg font-bold">Kill switch</h3>
      {#if !result}
        <p class="mt-2 text-sm">
          Blocks every order that would open or add to a position, on all accounts, and cancels resting entry
          orders. Orders that reduce a position (Exit, stop-loss, target) still work. It stays on until you release it.
        </p>
        <label class="mt-3 flex cursor-pointer items-start gap-2 text-sm">
          <input type="checkbox" class="checkbox checkbox-sm checkbox-error mt-0.5" bind:checked={flatten} />
          <span>Also exit every open position now</span>
        </label>
      {:else}
        <p class="mt-2 text-sm">
          Kill switch is on. Cancelled {result.cancelled} order{result.cancelled === 1 ? '' : 's'}{flatten
            ? `; placed ${result.placed} exit order${result.placed === 1 ? '' : 's'}`
            : ''}.
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
          <button class="btn btn-error btn-sm" onclick={engage} disabled={busy}>
            {#if busy}<span class="loading loading-spinner loading-xs"></span>{/if} Turn on
          </button>
        {/if}
      </div>
    </div>
    <button class="modal-backdrop" aria-label="Close" onclick={() => (open = false)}></button>
  </dialog>
{/if}
