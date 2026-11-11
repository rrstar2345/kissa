<script lang="ts">
  import { api } from '$lib/api';
  import { num, signed, trend } from '$lib/format';
  import { liveAccount, livePosition } from '$lib/live';
  import { every } from '$lib/timers';
  import { trading } from '$lib/trading.svelte';
  import type { Dashboard } from '$lib/types';
  import { onMount } from 'svelte';

  const r2 = (v: number) => Math.round(v * 100) / 100;

  let dash = $state.raw<Dashboard | null>(null);

  async function load() {
    try {
      dash = await api.getDashboard(null);
    } catch {
      /* the header must never break the app */
    }
  }

  // Refetched when orders change; prices and P&L follow the live quote stream.
  // The timer is a safety net only (a missed event, a long sleep).
  $effect(() => {
    trading.version;
    load();
  });
  onMount(() => {
    const t = every(load, 30_000);
    return () => clearInterval(t);
  });

  // Same maths as the Dashboard: realised + unrealised - charges.
  const positions = $derived((dash?.positions ?? []).map(livePosition));
  const accounts = $derived((dash?.accounts ?? []).map((a) => liveAccount(a, positions)));
  const rows = $derived([...accounts].sort((a, b) => Math.abs(b.netToday) - Math.abs(a.netToday)));
  const sum = (f: (a: (typeof accounts)[number]) => number) => r2(accounts.reduce((s, a) => s + f(a), 0));
  const net = $derived(sum((a) => a.netToday));
  const realised = $derived(sum((a) => a.realisedToday));
  const unrealised = $derived(sum((a) => a.unrealised));
  const charges = $derived(sum((a) => a.chargesToday));
</script>

{#if dash && accounts.length}
  <div class="h-5 w-px bg-base-300"></div>

  <div class="dropdown">
    <div
      tabindex="0"
      role="button"
      class="flex cursor-pointer items-baseline gap-2 whitespace-nowrap"
      title="Net P&L today (realised + unrealised − charges). Click for the per-account split."
    >
      <span class="text-xs font-semibold text-base-content/60">P&amp;L</span>
      <span class="num font-semibold {trend(net)}">₹{signed(net)}</span>
      <span class="text-[10px] text-base-content/40">▾</span>
    </div>
    <div
      tabindex="-1"
      class="dropdown-content z-30 mt-2 w-72 rounded-box border border-base-300 bg-base-100 p-3 shadow-xl"
    >
      <div class="mb-2 text-xs font-semibold uppercase tracking-wide text-base-content/50">
        Net today · {dash.tradeDate}
      </div>
      <div class="flex max-h-64 flex-col gap-2 overflow-y-auto">
        {#each rows as a (a.accountId)}
          <div>
            <div class="flex items-center justify-between gap-2 text-sm">
              <span class="flex min-w-0 items-center gap-1.5">
                <span class="truncate">{a.name}</span>
                <span class="badge badge-xs badge-outline">{a.kind}</span>
              </span>
              <span class="num font-semibold {trend(a.netToday)}">{signed(a.netToday)}</span>
            </div>
            <div class="num text-[11px] text-base-content/50">
              R {signed(a.realisedToday)} · U {signed(a.unrealised)} · C {num(a.chargesToday)}
            </div>
          </div>
        {/each}
      </div>
      {#if accounts.length > 1}
        <div class="mt-2 border-t border-base-300 pt-2">
          <div class="flex items-center justify-between text-sm font-semibold">
            <span>Total</span>
            <span class="num {trend(net)}">{signed(net)}</span>
          </div>
          <div class="num text-[11px] text-base-content/50">
            R {signed(realised)} · U {signed(unrealised)} · C {num(charges)}
          </div>
        </div>
      {/if}
    </div>
  </div>

  <a href="/orders?tab=positions" class="btn btn-outline btn-xs" title="Open the Positions tab on the Orders page">
    Positions ({positions.length})
  </a>
{/if}