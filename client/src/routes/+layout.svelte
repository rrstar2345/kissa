<script lang="ts">
  import '../app.css';
  import { onMount } from 'svelte';
  import { page } from '$app/state';
  import { market } from '$lib/market.svelte';
  import { quotes } from '$lib/quotes.svelte';
  import { theme } from '$lib/theme.svelte';
  import { trading } from '$lib/trading.svelte';
  import { num, signed, trend } from '$lib/format';
  import KillSwitch from '$lib/components/KillSwitch.svelte';

  let { children } = $props();

  const nav = [
    { href: '/', label: 'Dashboard', icon: 'M3 13h8V3H3v10zm0 8h8v-6H3v6zm10 0h8V11h-8v10zm0-18v6h8V3h-8z' },
    { href: '/chain', label: 'Option Chain', icon: 'M3 5h18M3 12h18M3 19h18M9 3v18M15 3v18' },
    { href: '/charts', label: 'Charts', icon: 'M3 3v18h18M7 14l4-4 3 3 5-6' },
    { href: '/signals', label: 'Signals', icon: 'M3 12h4l3-8 4 16 3-8h4' },
    { href: '/orders', label: 'Orders', icon: 'M9 5h10M9 12h10M9 19h10M4 5h.01M4 12h.01M4 19h.01' },
    { href: '/risk', label: 'Risk', icon: 'M12 3l8 3v6c0 5-3.5 8-8 9-4.5-1-8-4-8-9V6l8-3zM12 8v5M12 16h.01' },
    { href: '/alerts', label: 'Alerts', icon: 'M18 8a6 6 0 10-12 0c0 7-3 9-3 9h18s-3-2-3-9zM13.7 21a2 2 0 01-3.4 0' },
    { href: '/accounts', label: 'Accounts', icon: 'M16 7a4 4 0 11-8 0 4 4 0 018 0zM4 21a8 8 0 0116 0' },
    { href: '/settings', label: 'Settings', icon: 'M12 15a3 3 0 100-6 3 3 0 000 6zM19.4 15a1.7 1.7 0 00.3 1.8l.1.1a2 2 0 11-2.8 2.8l-.1-.1a1.7 1.7 0 00-1.8-.3 1.7 1.7 0 00-1 1.5V21a2 2 0 11-4 0v-.1a1.7 1.7 0 00-1.1-1.5 1.7 1.7 0 00-1.8.3l-.1.1a2 2 0 11-2.8-2.8l.1-.1a1.7 1.7 0 00.3-1.8 1.7 1.7 0 00-1.5-1H3a2 2 0 110-4h.1a1.7 1.7 0 001.5-1.1 1.7 1.7 0 00-.3-1.8l-.1-.1a2 2 0 112.8-2.8l.1.1a1.7 1.7 0 001.8.3H9a1.7 1.7 0 001-1.5V3a2 2 0 114 0v.1a1.7 1.7 0 001 1.5 1.7 1.7 0 001.8-.3l.1-.1a2 2 0 112.8 2.8l-.1.1a1.7 1.7 0 00-.3 1.8V9a1.7 1.7 0 001.5 1H21a2 2 0 110 4h-.1a1.7 1.7 0 00-1.5 1z' }
  ];

  const watch = ['NIFTY 50', 'NIFTY BANK', 'INDIA VIX'];

  onMount(() => {
    theme.init();
    market.start();
    trading.start();
    quotes.start();
    return () => {
      market.stop();
      trading.stop();
      quotes.stop();
    };
  });

  const active = (href: string) =>
    href === '/' ? page.url.pathname === '/' : page.url.pathname.startsWith(href);
  const live = $derived(market.status?.streams.some((s) => s.state === 'connected'));
</script>

<div class="flex h-screen bg-base-200 text-base-content">
  <aside class="flex w-52 shrink-0 flex-col border-r border-base-300 bg-base-100">
    <div class="flex items-center gap-2 px-4 py-4">
      <div class="grid size-8 place-items-center rounded-lg bg-primary text-sm font-black text-primary-content">K</div>
      <span class="text-lg font-bold tracking-wide">KISSA</span>
    </div>
    <ul class="menu w-full gap-0.5 px-2">
      {#each nav as item}
        <li>
          <a href={item.href} class:menu-active={active(item.href)}>
            <svg class="size-4" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d={item.icon} /></svg>
            {item.label}
          </a>
        </li>
      {/each}
    </ul>
    <div class="mt-auto p-3">
      <button class="btn btn-ghost btn-sm w-full justify-start gap-2" onclick={() => theme.toggle()}>
        {theme.current === 'kissa-dark' ? '☀ Light theme' : '☾ Dark theme'}
      </button>
    </div>
  </aside>

  <div class="flex min-w-0 flex-1 flex-col">
    <header class="flex items-center gap-5 border-b border-base-300 bg-base-100 px-5 py-2 text-sm">
      {#each watch as name}
        {@const t = market.indices[name]}
        <div class="flex items-baseline gap-2">
          <span class="text-xs font-semibold text-base-content/60">{name}</span>
          {#if t}
            <span class="num font-semibold">{num(t.currentPrice)}</span>
            <span class="num text-xs {trend(t.change)}">{signed(t.change)} ({signed(t.perChange)}%)</span>
          {:else}
            <span class="text-base-content/40">—</span>
          {/if}
        </div>
      {/each}

      <div class="ml-auto flex items-center gap-3 text-xs">
        <KillSwitch />
        {#if market.status}
          <span class="num text-base-content/70">IST {market.status.istTime}</span>
          <span class="badge badge-sm {market.status.withinWindow ? 'badge-success' : 'badge-ghost'}">
            {market.status.withinWindow ? 'Market hours' : 'Closed'}
          </span>
          <span class="flex items-center gap-1.5" title="NSE stream state: {market.status.mode}">
            <span class="size-2 rounded-full {live ? 'bg-success' : 'bg-base-content/30'}"></span>
            {live ? 'Live' : market.status.mode === 'IDLE' ? 'Idle' : 'Connecting'}
          </span>
        {/if}
      </div>
    </header>

    <main class="min-h-0 flex-1 overflow-auto p-5">
      {@render children()}
    </main>
  </div>
</div>
