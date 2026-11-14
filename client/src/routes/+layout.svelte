<script lang="ts">
  import '../app.css';
  import { onMount } from 'svelte';
  import { page } from '$app/state';
  import { market, stateBadge, stateLabel } from '$lib/market.svelte';
  import { quotes } from '$lib/quotes.svelte';
  import { theme } from '$lib/theme.svelte';
  import { trading } from '$lib/trading.svelte';
  import { api } from '$lib/api';
  import { isPopout } from '$lib/windowing';
  import { num, signed, trend } from '$lib/format';
  import KillSwitch from '$lib/components/KillSwitch.svelte';
  import TopBarPnl from '$lib/components/TopBarPnl.svelte';
  import FyersLogin from '$lib/components/FyersLogin.svelte';
  import type { Account } from '$lib/types';

  let { children } = $props();

  const nav = [
    { href: '/', label: 'Dashboard', icon: 'M3 13h8V3H3v10zm0 8h8v-6H3v6zm10 0h8V11h-8v10zm0-18v6h8V3h-8z' },
    { href: '/chain', label: 'Option Chain', icon: 'M3 5h18M3 12h18M3 19h18M9 3v18M15 3v18' },
    { href: '/charts', label: 'Charts', icon: 'M3 3v18h18M7 14l4-4 3 3 5-6' },
    { href: '/signals', label: 'Signals', icon: 'M3 12h4l3-8 4 16 3-8h4' },
    { href: '/algos', label: 'Algos', icon: 'M12 8V4H8M4 12a8 8 0 1016 0 8 8 0 00-16 0zM9 13h.01M15 13h.01M9.5 16.5h5' },
    { href: '/backtest', label: 'Backtest', icon: 'M3 3v18h18M7 16l3-5 3 3 5-8M18 6h-3M18 6v3' },
    { href: '/orders', label: 'Orders', icon: 'M9 5h10M9 12h10M9 19h10M4 5h.01M4 12h.01M4 19h.01' },
    { href: '/copier', label: 'Copier', icon: 'M8 8h12v12H8zM4 16V4h12' },
    { href: '/risk', label: 'Risk', icon: 'M12 3l8 3v6c0 5-3.5 8-8 9-4.5-1-8-4-8-9V6l8-3zM12 8v5M12 16h.01' },
    { href: '/alerts', label: 'Alerts', icon: 'M18 8a6 6 0 10-12 0c0 7-3 9-3 9h18s-3-2-3-9zM13.7 21a2 2 0 01-3.4 0' },
    { href: '/accounts', label: 'Accounts', icon: 'M16 7a4 4 0 11-8 0 4 4 0 018 0zM4 21a8 8 0 0116 0' },
    { href: '/settings', label: 'Settings', icon: 'M12 15a3 3 0 100-6 3 3 0 000 6zM19.4 15a1.7 1.7 0 00.3 1.8l.1.1a2 2 0 11-2.8 2.8l-.1-.1a1.7 1.7 0 00-1.8-.3 1.7 1.7 0 00-1 1.5V21a2 2 0 11-4 0v-.1a1.7 1.7 0 00-1.1-1.5 1.7 1.7 0 00-1.8.3l-.1.1a2 2 0 11-2.8-2.8l.1-.1a1.7 1.7 0 00.3-1.8 1.7 1.7 0 00-1.5-1H3a2 2 0 110-4h.1a1.7 1.7 0 001.5-1.1 1.7 1.7 0 00-.3-1.8l-.1-.1a2 2 0 112.8-2.8l.1.1a1.7 1.7 0 001.8.3H9a1.7 1.7 0 001-1.5V3a2 2 0 114 0v.1a1.7 1.7 0 001 1.5 1.7 1.7 0 001.8-.3l.1-.1a2 2 0 112.8 2.8l-.1.1a1.7 1.7 0 00-.3 1.8V9a1.7 1.7 0 001.5 1H21a2 2 0 110 4h-.1a1.7 1.7 0 00-1.5 1z' }
  ];

  const popError = $state({ msg: '' });
  async function popOut(item: { href: string; label: string }) {
    popError.msg = '';
    try {
      await api.openWindow(item.href, item.label);
    } catch (e) {
      popError.msg = String(e);
    }
  }

  const watch = ['NIFTY 50', 'NIFTY BANK', 'INDIA VIX'];

  // Fyers login from the header, when the saved token has expired (or there is none).
  let loggingIn = $state<Account | null>(null);
  const fyersAuth = $derived(market.status?.fyersAuth ?? null);
  async function startFyersLogin() {
    popError.msg = '';
    try {
      const id = fyersAuth?.accountId;
      const acct = (await api.listAccounts()).find((a) => a.id === id);
      if (!acct) throw new Error('The Fyers data account was not found. Check Accounts.');
      if (!acct.secretsSet.includes('appSecret')) {
        throw new Error('Edit the Fyers account under Accounts and enter the Secret ID first.');
      }
      loggingIn = acct;
    } catch (e) {
      popError.msg = String(e);
    }
  }
  async function fyersLoggedIn() {
    const id = loggingIn?.id;
    loggingIn = null;
    if (!id) return;
    // Test call first; the header indices are seeded again once it passes.
    const r = await api.fyersLoginFinished(id).catch((e) => ({ ok: false, message: String(e) }));
    if (!r.ok) popError.msg = `Fyers login saved, but the test call failed: ${r.message}`;
  }

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
  /** Page name for an extra window's title bar and header. */
  const pageLabel = $derived(nav.find((n) => active(n.href))?.label ?? 'KISSA');
  $effect(() => {
    if (isPopout) api.setWindowTitle(`KISSA - ${pageLabel}`).catch(() => {});
  });
  /** Feed indicator dot: green when live, amber when the session is open but nothing is ticking. */
  const feedDot = $derived(
    market.status?.live
      ? 'bg-success'
      : ['No ticks', 'Connecting', 'Error'].includes(market.status?.feed ?? '')
        ? 'bg-warning'
        : 'bg-base-content/30'
  );
</script>

<div class="flex h-screen bg-base-200 text-base-content">
  {#if !isPopout}
  <aside class="flex w-52 shrink-0 flex-col border-r border-base-300 bg-base-100">
    <div class="flex items-center gap-2 px-4 py-4">
      <div class="grid size-8 place-items-center rounded-lg bg-primary text-sm font-black text-primary-content">K</div>
      <span class="text-lg font-bold tracking-wide">KISSA</span>
    </div>
    <ul class="menu w-full gap-0.5 px-2">
      {#each nav as item}
        <li class="group relative">
          <a href={item.href} class:menu-active={active(item.href)}>
            <svg class="size-4" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d={item.icon} /></svg>
            {item.label}
          </a>
          <button
            class="btn btn-ghost btn-xs absolute right-1 top-1/2 hidden size-6 -translate-y-1/2 p-0 group-hover:flex"
            title="Open {item.label} in a new window"
            aria-label="Open {item.label} in a new window"
            onclick={() => popOut(item)}
          >
            <svg class="size-3.5" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M14 4h6v6M20 4l-9 9M18 14v5a1 1 0 01-1 1H5a1 1 0 01-1-1V7a1 1 0 011-1h5" /></svg>
          </button>
        </li>
      {/each}
    </ul>
    <div class="mt-auto p-3">
      <button class="btn btn-ghost btn-sm w-full justify-start gap-2" onclick={() => theme.toggle()}>
        {theme.current === 'kissa-dark' ? '☀ Light theme' : '☾ Dark theme'}
      </button>
    </div>
  </aside>
  {/if}

  <div class="flex min-w-0 flex-1 flex-col">
    <header class="flex items-center gap-5 border-b border-base-300 bg-base-100 px-5 py-2 text-sm">
      {#if isPopout}
        <span class="text-sm font-bold tracking-wide">{pageLabel}</span>
      {/if}
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

      <TopBarPnl />

      <div class="ml-auto flex items-center gap-3 text-xs">
        <KillSwitch />
        {#if fyersAuth && fyersAuth.state !== 'ok'}
          {#if fyersAuth.state === 'noAccount'}
            <a href="/accounts" class="badge badge-sm badge-warning" title="Fyers is the data source but no Fyers account is selected">
              Fyers: select account
            </a>
          {:else}
            <button
              type="button"
              class="badge badge-sm badge-error cursor-pointer"
              title="Click to log in to Fyers. Index prices come from NSE until you do."
              onclick={startFyersLogin}
            >
              {fyersAuth.state === 'expired' ? 'Fyers login expired' : 'Fyers not logged in'} · Log in
            </button>
          {/if}
        {/if}
        {#if market.status}
          <span class="num text-base-content/70">IST {market.clock}</span>
          <!-- One source only: the selected data source (Accounts or Option Chain page). -->
          {@const ex = market.status.exchange}
          <span
            class="badge badge-sm {stateBadge(ex)}"
            title="{ex.message}{ex.checkedAt ? ` (checked ${ex.checkedAt} IST)` : ''}"
          >
            {market.status.source === 'fyers' ? 'Fyers' : 'NSE'} {stateLabel(ex)}
          </span>
          <span class="flex items-center gap-1.5" title={market.status.feedDetail}>
            <span class="size-2 rounded-full {feedDot}"></span>
            {market.status.feed}
          </span>
        {/if}
      </div>
    </header>

    {#if popError.msg}
      <div class="alert alert-error alert-soft mx-5 mt-3 py-2 text-sm">{popError.msg}</div>
    {/if}
    <main class="min-h-0 flex-1 overflow-auto p-5">
      {@render children()}
    </main>
  </div>
</div>

{#if loggingIn}
  <dialog class="modal modal-open">
    <div class="modal-box">
      {#key loggingIn.id}
        <FyersLogin account={loggingIn} ondone={fyersLoggedIn} oncancel={() => (loggingIn = null)} />
      {/key}
    </div>
    <button class="modal-backdrop" aria-label="Close" onclick={() => (loggingIn = null)}></button>
  </dialog>
{/if}
