<script lang="ts">
  import { onMount } from 'svelte';
  import { api } from '$lib/api';
  import { getScoped, setScoped } from '$lib/windowing';
  import { num, signed, trend } from '$lib/format';
  import { livePosition } from '$lib/live';
  import { every } from '$lib/timers';
  import { contractName, trading } from '$lib/trading.svelte';
  import type { Position } from '$lib/types';
  import ExitAllButton from './ExitAllButton.svelte';
  import ExitButton from './ExitButton.svelte';

  let {
    onpick,
    isActive = () => false,
    hint = ''
  }: {
    /** Called when a contract name is clicked; the page decides what that does. */
    onpick: (p: Position) => void;
    /** Rows for which this returns true are highlighted (what the page currently shows). */
    isActive?: (p: Position) => boolean;
    /** Short line shown in the header when expanded, e.g. what a click does here. */
    hint?: string;
  } = $props();

  const OPEN_KEY = 'positions.dock.open';

  let open = $state(true);
  let base = $state.raw<Position[]>([]);
  let loaded = $state(false);

  // Prices and unrealised P&L follow the live quote stream; the list itself is
  // only refetched when orders change.
  const positions = $derived(base.map(livePosition));
  const unrealised = $derived(positions.reduce((s, p) => s + (p.unrealised ?? 0), 0));

  async function load() {
    try {
      base = (await api.getDashboard(null)).positions;
      loaded = true;
    } catch {
      /* keep the last list; the dock must never break the page */
    }
  }

  $effect(() => {
    trading.version;
    load();
  });

  onMount(() => {
    getScoped<boolean>(OPEN_KEY, true).then((v) => (open = v));
    const t = every(load, 30_000);
    return () => clearInterval(t);
  });

  function toggle() {
    open = !open;
    setScoped(OPEN_KEY, open).catch(() => {});
  }
</script>

<section class="shrink-0 rounded-box border border-base-300 bg-base-100 shadow-sm">
  <div class="flex items-center gap-3 px-3 py-1.5">
    <button class="flex items-center gap-2 text-sm font-semibold" aria-expanded={open} onclick={toggle}>
      <span class="text-[10px] text-base-content/50">{open ? '▾' : '▸'}</span>
      Open positions ({positions.length})
    </button>
    {#if positions.length}
      <span class="num text-sm font-semibold {trend(unrealised)}" title="Unrealised P&L of all open positions">
        ₹{signed(unrealised)}
      </span>
    {/if}
    <span class="ml-auto flex items-center gap-3">
      {#if hint && open && positions.length}<span class="text-xs text-base-content/50">{hint}</span>{/if}
      <ExitAllButton {positions} ondone={load} />
    </span>
  </div>

  {#if open}
    <div class="max-h-48 overflow-auto border-t border-base-300">
      {#if !positions.length}
        <p class="px-3 py-3 text-sm text-base-content/50">{loaded ? 'No open positions.' : 'Loading…'}</p>
      {:else}
        <table class="table table-xs table-pin-rows">
          <thead>
            <tr>
              <th>Account</th><th>Contract</th><th>Product</th>
              <th class="text-right">Net qty</th><th class="text-right">Avg</th>
              <th class="text-right">LTP</th><th class="text-right">Unrealised</th><th></th>
            </tr>
          </thead>
          <tbody>
            {#each positions as p (p.accountId + p.instrument + p.product)}
              <tr class={isActive(p) ? 'bg-primary/10' : ''}>
                <td class="whitespace-nowrap">{p.accountName}</td>
                <td class="whitespace-nowrap">
                  <button class="link link-hover font-medium" title="Show this contract" onclick={() => onpick(p)}>
                    {contractName(p)}
                  </button>
                </td>
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
            {/each}
          </tbody>
        </table>
      {/if}
    </div>
  {/if}
</section>
