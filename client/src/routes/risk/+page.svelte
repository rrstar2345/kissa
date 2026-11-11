<script lang="ts">
  import { every } from '$lib/timers';
  import { onMount, untrack } from 'svelte';
  import { api } from '$lib/api';
  import { trading } from '$lib/trading.svelte';
  import { num, signed, trend } from '$lib/format';
  import type { AccountRisk, RiskLimits, TestResult } from '$lib/types';

  type Card = AccountRisk & { draft: RiskLimits; saved: string; msg: TestResult | null; busy: boolean };

  let cards = $state<Card[]>([]);
  let killOn = $state(false);
  let loadError = $state('');

  const snap = (l: RiskLimits) => JSON.stringify(l);

  async function load() {
    try {
      const o = await api.getRisk();
      killOn = !!o.killSwitch;
      // Keep unsaved edits of cards that are being edited; refresh the live numbers.
      cards = o.accounts.map((a) => {
        const old = cards.find((c) => c.accountId === a.accountId);
        const editing = old && snap(old.draft) !== old.saved;
        return {
          ...a,
          draft: editing ? old!.draft : { ...a.limits },
          saved: snap(a.limits),
          msg: old?.msg ?? null,
          busy: false
        };
      });
      loadError = '';
    } catch (e) {
      loadError = String(e);
    }
  }
  onMount(load);
  $effect(() => {
    trading.riskVersion;
    trading.version;
    untrack(load); // load reads `cards`; do not subscribe to it
  });
  // P&L moves with prices; refresh the live numbers every few seconds.
  onMount(() => {
    const t = every(load, 5000);
    return () => clearInterval(t);
  });

  async function save(c: Card) {
    c.busy = true;
    c.msg = null;
    try {
      const d = c.draft;
      const saved = await api.saveRiskLimits(c.accountId, {
        ...d,
        maxLotsPerOrder: Math.max(0, Math.floor(Number(d.maxLotsPerOrder) || 0)),
        maxOpenPositions: Math.max(0, Math.floor(Number(d.maxOpenPositions) || 0)),
        maxDailyLoss: Math.max(0, Number(d.maxDailyLoss) || 0)
      });
      c.draft = { ...saved };
      c.saved = snap(saved);
      c.msg = { ok: true, message: 'Saved.' };
    } catch (e) {
      c.msg = { ok: false, message: String(e) };
    } finally {
      c.busy = false;
    }
  }

  async function resume(c: Card) {
    c.busy = true;
    try {
      await api.resumeAccount(c.accountId);
      await load();
    } catch (e) {
      c.msg = { ok: false, message: String(e) };
    } finally {
      c.busy = false;
    }
  }

  const dirty = (c: Card) => snap(c.draft) !== c.saved;
  const lossLeft = (c: Card) =>
    c.limits.enabled && c.limits.maxDailyLoss > 0 ? Math.max(0, c.limits.maxDailyLoss + c.netToday) : null;
</script>

<div class="flex max-w-4xl flex-col gap-5">
  <div>
    <h1 class="text-lg font-semibold">Risk controls</h1>
    <p class="text-sm text-base-content/60">
      Limits are checked on every order (manual, copier and algo). Orders that only reduce a position — Exit,
      stop-loss, target, square-off — are never blocked. A value of 0 turns that limit off.
    </p>
  </div>

  {#if killOn}
    <div role="alert" class="alert alert-error alert-soft py-2 text-sm">
      The kill switch is on: no new positions on any account. Release it from the header.
    </div>
  {/if}
  {#if loadError}<div role="alert" class="alert alert-error alert-soft py-2 text-sm break-words">{loadError}</div>{/if}
  {#if !cards.length && !loadError}
    <p class="text-sm text-base-content/60">No accounts yet. Add one on the Accounts page.</p>
  {/if}

  {#each cards as c (c.accountId)}
    <section class="flex flex-col gap-4 rounded-box border border-base-300 bg-base-100 p-5">
      <div class="flex flex-wrap items-center gap-3">
        <h2 class="font-semibold">{c.name}</h2>
        <span class="badge badge-sm badge-outline">{c.kind}</span>
        {#if c.halt}
          <span class="badge badge-sm badge-error">Halted</span>
        {:else if c.draft.enabled}
          <span class="badge badge-sm badge-success">Limits on</span>
        {:else}
          <span class="badge badge-sm badge-ghost">Limits off</span>
        {/if}
        <div class="ml-auto flex items-baseline gap-4 text-sm">
          <span class="text-xs text-base-content/60">Today</span>
          <span class="num font-semibold {trend(c.netToday)}">{signed(c.netToday)}</span>
          {#if lossLeft(c) !== null}
            <span class="text-xs text-base-content/60">headroom <span class="num">{num(lossLeft(c), 0)}</span></span>
          {/if}
        </div>
      </div>

      {#if c.halt}
        <div role="alert" class="alert alert-warning alert-soft flex items-center justify-between py-2 text-sm">
          <span class="break-words">Halted: {c.halt.reason}. New positions are blocked until tomorrow.</span>
          {#if c.halt.kind === 'daily_loss'}
            <button class="btn btn-warning btn-xs" onclick={() => resume(c)} disabled={c.busy}>Resume today</button>
          {/if}
        </div>
      {/if}

      <label class="flex cursor-pointer items-center gap-2 text-sm">
        <input type="checkbox" class="toggle toggle-sm toggle-primary" bind:checked={c.draft.enabled} />
        Enable risk limits for this account
      </label>

      <div class="grid grid-cols-1 gap-3 sm:grid-cols-3" class:opacity-50={!c.draft.enabled}>
        <label class="flex flex-col gap-1">
          <span class="text-xs text-base-content/60">Max lots per order</span>
          <input class="input input-sm num w-full" type="number" min="0" step="1" bind:value={c.draft.maxLotsPerOrder} />
        </label>
        <label class="flex flex-col gap-1">
          <span class="text-xs text-base-content/60">Max open positions</span>
          <input class="input input-sm num w-full" type="number" min="0" step="1" bind:value={c.draft.maxOpenPositions} />
        </label>
        <label class="flex flex-col gap-1">
          <span class="text-xs text-base-content/60">Max daily loss (₹)</span>
          <input class="input input-sm num w-full" type="number" min="0" step="100" bind:value={c.draft.maxDailyLoss} />
        </label>
      </div>

      <label class="flex cursor-pointer items-center gap-2 text-sm" class:opacity-50={!c.draft.enabled}>
        <input type="checkbox" class="checkbox checkbox-sm" bind:checked={c.draft.squareOffOnBreach} />
        When the daily loss is reached, also cancel orders and exit all positions
      </label>

      <div class="flex flex-wrap items-center gap-3" class:opacity-50={!c.draft.enabled}>
        <label class="flex cursor-pointer items-center gap-2 text-sm">
          <input type="checkbox" class="checkbox checkbox-sm" bind:checked={c.draft.windowEnabled} />
          Only open positions between
        </label>
        <input class="input input-sm num w-24" bind:value={c.draft.windowStart} placeholder="09:20" disabled={!c.draft.windowEnabled} />
        <span class="text-sm">and</span>
        <input class="input input-sm num w-24" bind:value={c.draft.windowEnd} placeholder="15:15" disabled={!c.draft.windowEnabled} />
        <span class="text-xs text-base-content/60">IST, 24-hour</span>
      </div>

      {#if c.msg}
        <div role="status" class="alert {c.msg.ok ? 'alert-success' : 'alert-error'} alert-soft py-2 text-sm break-words">{c.msg.message}</div>
      {/if}
      <div class="flex gap-2">
        <button class="btn btn-primary btn-sm" onclick={() => save(c)} disabled={c.busy || !dirty(c)}>
          {#if c.busy}<span class="loading loading-spinner loading-xs"></span>{/if} Save
        </button>
        <button class="btn btn-ghost btn-sm" onclick={() => { c.draft = { ...c.limits }; c.msg = null; }} disabled={!dirty(c)}>Revert</button>
      </div>
    </section>
  {/each}
</div>
