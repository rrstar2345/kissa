<script lang="ts">
  import { onMount, untrack } from 'svelte';
  import { api } from '$lib/api';
  import { todayIst } from '$lib/format';
  import { trading } from '$lib/trading.svelte';
  import type { Account, AuditFilter, AuditRow } from '$lib/types';

  const PAGE = 100;
  const CATEGORIES = [
    { id: 'order', label: 'Orders', cls: 'badge-info' },
    { id: 'risk', label: 'Risk', cls: 'badge-error' },
    { id: 'algo', label: 'Algos', cls: 'badge-secondary' },
    { id: 'copier', label: 'Copier', cls: 'badge-accent' },
    { id: 'account', label: 'Accounts', cls: 'badge-primary' },
    { id: 'settings', label: 'Settings', cls: 'badge-warning' },
    { id: 'system', label: 'System', cls: 'badge-ghost' }
  ];
  const catClass = (id: string) => CATEGORIES.find((c) => c.id === id)?.cls ?? 'badge-ghost';
  const catLabel = (id: string) => CATEGORIES.find((c) => c.id === id)?.label ?? id;

  let rows = $state.raw<AuditRow[]>([]);
  let accounts = $state<Account[]>([]);
  let cats = $state<string[]>([]); // empty = all
  let accountId = $state('');
  let from = $state('');
  let to = $state('');
  let searchInput = $state('');
  let search = $state('');
  let loading = $state(false);
  let loadingMore = $state(false);
  let more = $state(false);
  let error = $state('');
  let open = $state<Record<number, boolean>>({});

  const filter = $derived<AuditFilter>({
    categories: cats,
    accountId: accountId || null,
    from: from || null,
    to: to || null,
    search: search || null
  });
  const filtered = $derived(cats.length > 0 || !!accountId || !!from || !!to || !!search);
  const snap = () => $state.snapshot(filter) as AuditFilter;

  onMount(async () => {
    try {
      accounts = await api.listAccounts();
    } catch (e) {
      error = String(e);
    }
  });

  // Each filter change starts over; a response for an older filter is dropped.
  let seq = 0;
  async function reload() {
    const mine = ++seq;
    loading = true;
    try {
      const r = await api.listAudit(snap(), null, PAGE);
      if (mine !== seq) return;
      rows = r;
      more = r.length === PAGE;
      open = {};
      error = '';
    } catch (e) {
      if (mine === seq) error = String(e);
    } finally {
      if (mine === seq) loading = false;
    }
  }

  async function loadMore() {
    if (!rows.length || loadingMore) return;
    const mine = seq;
    loadingMore = true;
    try {
      const r = await api.listAudit(snap(), rows[rows.length - 1].id, PAGE);
      if (mine !== seq) return;
      rows = [...rows, ...r];
      more = r.length === PAGE;
    } catch (e) {
      error = String(e);
    } finally {
      loadingMore = false;
    }
  }

  // New entries are put on top without losing the rows already loaded.
  async function loadNew() {
    if (!rows.length) return reload();
    const mine = seq;
    const top = rows[0].id;
    try {
      const r = await api.listAudit(snap(), null, PAGE);
      if (mine !== seq) return;
      const fresh = r.filter((x) => x.id > top);
      if (fresh.length >= PAGE) return reload(); // a gap: start over
      if (fresh.length) rows = [...fresh, ...rows];
    } catch (e) {
      error = String(e);
    }
  }

  $effect(() => {
    JSON.stringify(filter);
    untrack(reload);
  });
  $effect(() => {
    const v = searchInput;
    const t = setTimeout(() => (search = v.trim()), 300);
    return () => clearTimeout(t);
  });
  let lastVersion = -1;
  $effect(() => {
    const v = trading.auditVersion;
    untrack(() => {
      if (lastVersion >= 0 && v !== lastVersion) loadNew();
      lastVersion = v;
    });
  });

  function toggleCat(id: string) {
    cats = cats.includes(id) ? cats.filter((c) => c !== id) : [...cats, id];
  }
  function clearFilters() {
    cats = [];
    accountId = '';
    from = '';
    to = '';
    searchInput = '';
    search = '';
  }

  /** `DD-MM HH:MM:SS` in IST from an RFC 3339 timestamp. */
  function stamp(at: string): string {
    const d = new Date(at);
    if (Number.isNaN(d.getTime())) return '—';
    const iso = new Date(d.getTime() + 5.5 * 3600_000).toISOString();
    return `${iso.slice(8, 10)}-${iso.slice(5, 7)} ${iso.slice(11, 19)}`;
  }

  function pretty(detail: string | null): string {
    if (!detail) return '';
    try {
      return JSON.stringify(JSON.parse(detail), null, 2);
    } catch {
      return detail;
    }
  }
  const expandable = (r: AuditRow) => !!r.detail || !!r.subject;
</script>

<div class="flex max-w-6xl flex-col gap-4">
  <div>
    <h1 class="text-lg font-semibold">Journal</h1>
    <p class="text-sm text-base-content/60">
      What happened in the app: orders, fills, risk halts, algos, the trade copier, logins and settings changes.
      Entries are kept for 90 days. Times are IST.
    </p>
  </div>

  <section class="flex flex-col gap-3 rounded-box border border-base-300 bg-base-100 p-4">
    <div class="flex flex-wrap items-center gap-2">
      {#each CATEGORIES as c (c.id)}
        <button
          class="btn btn-xs {cats.includes(c.id) ? 'btn-primary' : 'btn-outline'}"
          aria-pressed={cats.includes(c.id)}
          onclick={() => toggleCat(c.id)}>{c.label}</button>
      {/each}
    </div>
    <div class="flex flex-wrap items-end gap-3">
      <label class="flex flex-col gap-1">
        <span class="text-xs text-base-content/60">Account</span>
        <select class="select select-sm w-48" bind:value={accountId}>
          <option value="">All accounts</option>
          {#each accounts as a (a.id)}<option value={a.id}>{a.name}</option>{/each}
        </select>
      </label>
      <label class="flex flex-col gap-1">
        <span class="text-xs text-base-content/60">From</span>
        <input class="input input-sm w-40" type="date" max={to || todayIst()} bind:value={from} />
      </label>
      <label class="flex flex-col gap-1">
        <span class="text-xs text-base-content/60">To</span>
        <input class="input input-sm w-40" type="date" min={from} max={todayIst()} bind:value={to} />
      </label>
      <label class="flex min-w-48 flex-1 flex-col gap-1">
        <span class="text-xs text-base-content/60">Search</span>
        <input class="input input-sm w-full" type="search" placeholder="Summary, order id, account, details…" bind:value={searchInput} />
      </label>
      <button class="btn btn-ghost btn-sm" onclick={clearFilters} disabled={!filtered && !searchInput}>Clear filters</button>
    </div>
  </section>

  {#if error}
    <div role="alert" class="alert alert-error alert-soft py-2 text-sm break-words">{error}</div>
  {/if}

  <section class="flex flex-col gap-3 rounded-box border border-base-300 bg-base-100 p-4">
    {#if loading && !rows.length}
      <p class="text-sm text-base-content/60"><span class="loading loading-spinner loading-xs"></span> Loading…</p>
    {:else if !rows.length}
      <p class="text-sm text-base-content/60">
        {filtered ? 'No entries match these filters.' : 'Nothing yet. Orders, risk events, algo changes and the like are listed here as they happen.'}
      </p>
    {:else}
      <div class="overflow-x-auto">
        <table class="table table-sm">
          <thead>
            <tr>
              <th class="w-32">Time (IST)</th>
              <th class="w-28">Category</th>
              <th class="w-20">By</th>
              <th class="w-36">Account</th>
              <th>What happened</th>
              <th class="w-20"></th>
            </tr>
          </thead>
          <tbody>
            {#each rows as r (r.id)}
              <tr class="align-top">
                <td class="num text-xs whitespace-nowrap">{stamp(r.at)}</td>
                <td>
                  <span class="badge badge-sm {catClass(r.category)}">{catLabel(r.category)}</span>
                  <div class="mt-0.5 text-xs text-base-content/50">{r.action.replaceAll('_', ' ')}</div>
                </td>
                <td class="text-xs">{r.actor}</td>
                <td class="text-xs break-words">{r.accountName ?? (r.accountId ? r.accountId : '—')}</td>
                <td class="text-sm break-words">{r.summary}</td>
                <td class="text-right">
                  {#if expandable(r)}
                    <button class="btn btn-ghost btn-xs" aria-expanded={!!open[r.id]} onclick={() => (open[r.id] = !open[r.id])}>
                      {open[r.id] ? 'Hide' : 'Details'}
                    </button>
                  {/if}
                </td>
              </tr>
              {#if open[r.id]}
                <tr>
                  <td></td>
                  <td colspan="5" class="pb-3">
                    {#if r.subject}
                      <div class="text-xs text-base-content/60">Ref: <span class="num break-all">{r.subject}</span></div>
                    {/if}
                    {#if r.detail}
                      <pre class="mt-1 max-h-72 overflow-auto rounded-box bg-base-200 p-2 text-xs">{pretty(r.detail)}</pre>
                    {/if}
                  </td>
                </tr>
              {/if}
            {/each}
          </tbody>
        </table>
      </div>
      {#if more}
        <div class="flex justify-center">
          <button class="btn btn-outline btn-sm" onclick={loadMore} disabled={loadingMore}>
            {#if loadingMore}<span class="loading loading-spinner loading-xs"></span>{/if} Load more
          </button>
        </div>
      {:else}
        <p class="text-center text-xs text-base-content/50">{rows.length} entr{rows.length === 1 ? 'y' : 'ies'}</p>
      {/if}
    {/if}
  </section>
</div>
