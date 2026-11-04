<script lang="ts">
  import { onMount } from 'svelte';
  import { api } from '$lib/api';
  import AccountForm from '$lib/components/AccountForm.svelte';
  import type { Account, AccountKind, Source, TestResult } from '$lib/types';

  let accounts = $state<Account[]>([]);
  let loadError = $state('');
  let editing = $state<{ kind: AccountKind; account: Account | null } | null>(null);
  let results = $state<Record<string, TestResult | 'testing'>>({});
  let source = $state<Source>('nse');
  let fyersId = $state('');

  const byKind = (k: AccountKind) => accounts.filter((a) => a.kind === k);
  const proxies = $derived(byKind('proxy'));

  const sections: { kind: AccountKind; title: string; hint: string }[] = [
    { kind: 'proxy', title: 'Broker-proxy', hint: 'Go server on a static-IP VM that places orders on your behalf.' },
    { kind: 'dhan', title: 'Dhan', hint: 'Live trading through a broker-proxy.' },
    { kind: 'paper', title: 'Paper trading', hint: 'Simulated orders, executed locally.' },
    { kind: 'fyers', title: 'Fyers (market data)', hint: 'Optional alternative data source for option chains.' }
  ];

  onMount(load);

  async function load() {
    try {
      accounts = await api.listAccounts();
      source = (await api.getSetting('data.source')) === 'fyers' ? 'fyers' : 'nse';
      const saved = (await api.getSetting('data.fyers_account')) ?? '';
      fyersId = byKind('fyers').some((a) => a.id === saved) ? saved : (byKind('fyers')[0]?.id ?? '');
    } catch (e) {
      loadError = String(e);
    }
  }

  async function saveSource() {
    await api.setSetting('data.source', source);
    await api.setSetting('data.fyers_account', fyersId);
  }

  async function test(a: Account) {
    results[a.id] = 'testing';
    results[a.id] = await api.testAccount(a.id).catch((e) => ({ ok: false, message: String(e) }));
  }

  async function remove(a: Account) {
    if (!confirm(`Delete “${a.name}” and its saved credentials?`)) return;
    try {
      await api.deleteAccount(a.id);
      await load();
    } catch (e) {
      loadError = String(e);
    }
  }

  async function saved() {
    editing = null;
    await load();
  }

  function detail(a: Account): string {
    const m = a.meta;
    switch (a.kind) {
      case 'proxy':
        return [m.baseUrl, m.staticIp && `IP ${m.staticIp}`].filter(Boolean).join(' · ');
      case 'dhan': {
        const p = accounts.find((x) => x.id === m.proxyId);
        return `Client ${m.clientId} · via ${p?.name ?? 'missing proxy'}`;
      }
      case 'paper':
        return `Capital ₹${Number(m.startingCapital).toLocaleString('en-IN')} · ₹${m.brokeragePerOrder}/order`;
      case 'fyers':
        return `App ${m.appId}`;
    }
  }
</script>

<div class="mx-auto flex max-w-4xl flex-col gap-6">
  <div class="flex items-center gap-3">
    <h1 class="text-2xl font-bold">Accounts</h1>
    <div class="dropdown dropdown-end ml-auto">
      <div tabindex="0" role="button" class="btn btn-primary btn-sm">+ Add account</div>
      <ul tabindex="-1" class="menu dropdown-content z-20 mt-1 w-52 rounded-box border border-base-300 bg-base-100 p-2 shadow-xl">
        {#each sections as s}
          <li><button onclick={() => (editing = { kind: s.kind, account: null })}>{s.title}</button></li>
        {/each}
      </ul>
    </div>
  </div>

  {#if loadError}<div class="alert alert-error alert-soft py-2 text-sm">{loadError}</div>{/if}

  <!-- data source -->
  <div class="card border border-base-300 bg-base-100">
    <div class="card-body gap-3 py-4">
      <h2 class="card-title text-base">Market data source</h2>
      <p class="text-sm text-base-content/60">
        NSE public data is the default. Switch to Fyers to use your own API credentials.
      </p>
      <div class="flex flex-wrap items-center gap-4">
        <label class="flex items-center gap-2 text-sm">
          <input type="radio" class="radio radio-sm radio-primary" value="nse" bind:group={source} onchange={saveSource} /> NSE (default)
        </label>
        <label class="flex items-center gap-2 text-sm">
          <input type="radio" class="radio radio-sm radio-primary" value="fyers" bind:group={source} onchange={saveSource} /> Fyers
        </label>
        {#if byKind('fyers').length}
          <select class="select select-sm w-56" bind:value={fyersId} onchange={saveSource}>
            {#each byKind('fyers') as a}<option value={a.id}>{a.name}</option>{/each}
          </select>
        {:else}
          <span class="text-xs text-base-content/50">Add a Fyers account to enable.</span>
        {/if}
      </div>
    </div>
  </div>

  {#each sections as s}
    <section>
      <div class="mb-2 flex items-baseline gap-2">
        <h2 class="font-semibold">{s.title}</h2>
        <span class="text-xs text-base-content/50">{s.hint}</span>
      </div>
      {#if byKind(s.kind).length === 0}
        <div class="rounded-box border border-dashed border-base-300 px-4 py-3 text-sm text-base-content/50">
          None yet.
        </div>
      {/if}
      <div class="flex flex-col gap-2">
        {#each byKind(s.kind) as a (a.id)}
          <div class="card border border-base-300 bg-base-100">
            <div class="card-body flex-row items-center gap-4 px-4 py-3">
              <div class="min-w-0 flex-1">
                <div class="flex items-center gap-2 font-medium">
                  {a.name}
                  {#if a.kind !== 'paper'}
                    <span class="badge badge-xs {a.secretsSet.length ? 'badge-success' : 'badge-warning'} badge-soft">
                      {a.secretsSet.length ? 'credentials saved' : 'no credentials'}
                    </span>
                  {/if}
                </div>
                <div class="truncate text-xs text-base-content/60">{detail(a)}</div>
                {#if results[a.id] && results[a.id] !== 'testing'}
                  {@const r = results[a.id] as TestResult}
                  <div class="mt-1 text-xs {r.ok ? 'text-success' : 'text-error'}">{r.ok ? '✓' : '✗'} {r.message}</div>
                {/if}
              </div>
              <div class="flex gap-1">
                {#if a.kind !== 'paper'}
                  <button class="btn btn-ghost btn-xs" onclick={() => test(a)} disabled={results[a.id] === 'testing'}>
                    {#if results[a.id] === 'testing'}<span class="loading loading-spinner loading-xs"></span>{/if}
                    Test
                  </button>
                {/if}
                <button class="btn btn-ghost btn-xs" onclick={() => (editing = { kind: a.kind, account: a })}>Edit</button>
                <button class="btn btn-ghost btn-xs text-error" onclick={() => remove(a)}>Delete</button>
              </div>
            </div>
          </div>
        {/each}
      </div>
    </section>
  {/each}
</div>

{#if editing}
  <dialog class="modal modal-open">
    <div class="modal-box">
      {#key editing.account?.id ?? editing.kind}
        <AccountForm kind={editing.kind} account={editing.account} {proxies} onsaved={saved} oncancel={() => (editing = null)} />
      {/key}
    </div>
    <button class="modal-backdrop" aria-label="Close" onclick={() => (editing = null)}></button>
  </dialog>
{/if}
