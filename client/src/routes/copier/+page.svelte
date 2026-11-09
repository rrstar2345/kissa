<script lang="ts">
  import { onMount, untrack } from 'svelte';
  import { api } from '$lib/api';
  import { trading, statusClass, contractName } from '$lib/trading.svelte';
  import { istTime } from '$lib/format';
  import type { CopierAccount, CopierChild, CopierConfig, CopyView, TestResult } from '$lib/types';

  let draft = $state<CopierConfig>({ enabled: false, masterAccountId: '', children: [] });
  let saved = $state('');
  let accounts = $state<CopierAccount[]>([]);
  let links = $state<CopyView[]>([]);
  let loadError = $state('');
  let msg = $state<TestResult | null>(null);
  let busy = $state(false);
  let loaded = false;

  const snap = (c: CopierConfig) => JSON.stringify(c);
  const dirty = $derived(snap(draft) !== saved);

  async function load() {
    try {
      const o = await api.getCopier();
      accounts = o.accounts;
      links = o.links;
      // Keep unsaved edits; refresh them only when nothing is being edited.
      if (!loaded || snap(draft) === saved) {
        draft = structuredClone(o.config);
      }
      loaded = true;
      saved = snap(o.config);
      loadError = '';
    } catch (e) {
      loadError = String(e);
    }
  }
  onMount(load);
  $effect(() => {
    trading.copierVersion;
    trading.version; // child order status changes
    untrack(load);
  });

  const nameOf = (id: string) => accounts.find((a) => a.id === id)?.name ?? '(deleted account)';
  const kindOf = (id: string) => accounts.find((a) => a.id === id)?.kind ?? '';
  const free = $derived(
    accounts.filter((a) => a.id !== draft.masterAccountId && !draft.children.some((c) => c.accountId === a.id))
  );

  function addChild() {
    const next = free[0];
    if (!next) return;
    draft.children.push({ accountId: next.id, enabled: true, multiplier: 1, rounding: 'down', minOneLot: false });
  }
  function removeChild(i: number) {
    draft.children.splice(i, 1);
  }
  function setMaster(id: string) {
    draft.masterAccountId = id;
    draft.children = draft.children.filter((c) => c.accountId !== id);
  }

  /** Lots a child gets from a 1, 2, 3 and 5 lot master order (preview). */
  function scale(c: CopierChild, lots: number): string {
    const raw = Math.round(lots * Number(c.multiplier) * 1e6) / 1e6;
    const r = c.rounding === 'down' ? Math.floor(raw) : c.rounding === 'up' ? Math.ceil(raw) : Math.round(raw);
    if (r >= 1) return String(r);
    return c.minOneLot ? '1' : '–';
  }

  async function save() {
    busy = true;
    msg = null;
    try {
      const cfg = await api.saveCopier({
        ...draft,
        children: draft.children.map((c) => ({ ...c, multiplier: Number(c.multiplier) }))
      });
      draft = structuredClone(cfg);
      saved = snap(cfg);
      msg = { ok: true, message: 'Saved.' };
    } catch (e) {
      msg = { ok: false, message: String(e) };
    } finally {
      busy = false;
    }
  }

  function revert() {
    draft = JSON.parse(saved);
    msg = null;
  }

  const linkStatus = (l: CopyView) => (l.status === 'placed' ? (l.childStatus ?? 'PLACED') : l.status.toUpperCase());
  const linkClass = (l: CopyView) =>
    l.status === 'failed' ? 'badge-error' : l.status === 'skipped' ? 'badge-ghost' : statusClass(l.childStatus ?? '');
</script>

<div class="flex max-w-5xl flex-col gap-5">
  <div>
    <h1 class="text-lg font-semibold">Trade copier</h1>
    <p class="text-sm text-base-content/60">
      Orders placed on the master account are copied to the children as they are placed, scaled by each child's
      factor. Cancels and price changes on the master follow to the child orders that have not filled. Every copy goes
      through the normal order checks (server key, risk limits, broker rules); a child that fails is reported and the
      others carry on. Use child accounts only for copying: a full exit on the master closes the child's whole position
      in that contract.
    </p>
  </div>

  {#if loadError}<div role="alert" class="alert alert-error alert-soft py-2 text-sm break-words">{loadError}</div>{/if}

  <section class="flex flex-col gap-4 rounded-box border border-base-300 bg-base-100 p-5">
    <label class="flex cursor-pointer items-center gap-2 text-sm">
      <input type="checkbox" class="toggle toggle-sm toggle-primary" bind:checked={draft.enabled} />
      Copy master orders to the children
    </label>

    <label class="flex max-w-sm flex-col gap-1">
      <span class="text-xs text-base-content/60">Master account</span>
      <select class="select select-sm w-full" value={draft.masterAccountId} onchange={(e) => setMaster(e.currentTarget.value)}>
        <option value="">Select…</option>
        {#each accounts as a (a.id)}
          <option value={a.id}>{a.name} ({a.kind})</option>
        {/each}
      </select>
    </label>

    <div class="overflow-x-auto">
      <table class="table table-sm">
        <thead>
          <tr>
            <th>Child account</th>
            <th>On</th>
            <th>Factor</th>
            <th>Rounding</th>
            <th>Min 1 lot</th>
            <th class="whitespace-nowrap">Master 1 / 2 / 3 / 5 lots →</th>
            <th></th>
          </tr>
        </thead>
        <tbody>
          {#each draft.children as c, i (c.accountId)}
            <tr class:opacity-50={!c.enabled}>
              <td>
                {nameOf(c.accountId)}
                <span class="badge badge-xs badge-outline ml-1">{kindOf(c.accountId)}</span>
              </td>
              <td><input type="checkbox" class="toggle toggle-xs toggle-primary" bind:checked={c.enabled} /></td>
              <td>
                <input class="input input-xs num w-20" type="number" min="0.01" step="0.1" bind:value={c.multiplier} />
              </td>
              <td>
                <select class="select select-xs" bind:value={c.rounding}>
                  <option value="down">Down</option>
                  <option value="nearest">Nearest</option>
                  <option value="up">Up</option>
                </select>
              </td>
              <td><input type="checkbox" class="checkbox checkbox-xs" bind:checked={c.minOneLot} /></td>
              <td class="num whitespace-nowrap text-base-content/70">
                {scale(c, 1)} / {scale(c, 2)} / {scale(c, 3)} / {scale(c, 5)}
              </td>
              <td><button class="btn btn-ghost btn-xs" onclick={() => removeChild(i)}>Remove</button></td>
            </tr>
          {:else}
            <tr><td colspan="7" class="text-base-content/60">No children yet.</td></tr>
          {/each}
        </tbody>
      </table>
    </div>
    <p class="text-xs text-base-content/60">
      “–” = skipped (rounds to 0 lots). Orders that only reduce the master's position are capped at what the child
      holds, so a copy never opens a position the master did not.
    </p>

    <div>
      <button class="btn btn-outline btn-sm" onclick={addChild} disabled={!free.length}>Add child</button>
    </div>

    {#if msg}
      <div role="status" class="alert {msg.ok ? 'alert-success' : 'alert-error'} alert-soft py-2 text-sm break-words">{msg.message}</div>
    {/if}
    <div class="flex gap-2">
      <button class="btn btn-primary btn-sm" onclick={save} disabled={busy || !dirty}>
        {#if busy}<span class="loading loading-spinner loading-xs"></span>{/if} Save
      </button>
      <button class="btn btn-ghost btn-sm" onclick={revert} disabled={!dirty}>Revert</button>
    </div>
  </section>

  <section class="flex flex-col gap-3 rounded-box border border-base-300 bg-base-100 p-5">
    <h2 class="font-semibold">Recent copies</h2>
    <div class="overflow-x-auto">
      <table class="table table-sm">
        <thead>
          <tr>
            <th>Time</th>
            <th>Master order</th>
            <th>Child</th>
            <th class="text-right">Lots</th>
            <th>Status</th>
            <th>Note</th>
          </tr>
        </thead>
        <tbody>
          {#each links as l (l.id)}
            <tr>
              <td class="num whitespace-nowrap">{istTime(l.createdAt)}</td>
              <td class="whitespace-nowrap">
                {#if l.symbol && l.expiry && l.strike !== null}
                  <span class={l.side === 'BUY' ? 'text-success' : 'text-error'}>{l.side}</span>
                  {contractName({ symbol: l.symbol, expiry: l.expiry, strike: l.strike, isCall: !!l.isCall })}
                  {#if l.price}<span class="num text-base-content/60">@ {l.price.toFixed(2)}</span>{/if}
                {:else}
                  <span class="text-base-content/60">(order removed)</span>
                {/if}
              </td>
              <td class="whitespace-nowrap">{l.childAccountName}</td>
              <td class="num text-right whitespace-nowrap">{l.masterLots} → {l.status === 'skipped' ? '–' : l.childLots}</td>
              <td><span class="badge badge-sm {linkClass(l)}">{linkStatus(l)}</span></td>
              <td class="max-w-xs break-words text-xs text-base-content/70">{l.message}</td>
            </tr>
          {:else}
            <tr><td colspan="6" class="text-base-content/60">Nothing copied yet.</td></tr>
          {/each}
        </tbody>
      </table>
    </div>
  </section>
</div>
