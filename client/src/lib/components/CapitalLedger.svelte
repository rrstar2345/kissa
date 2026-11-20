<script lang="ts">
  import { onMount } from 'svelte';
  import { api } from '$lib/api';
  import { todayIst } from '$lib/format';
  import type { Account, CapitalEvent, CapitalKind } from '$lib/types';

  let { account, onclose }: { account: Account; onclose: () => void } = $props();

  const KINDS: { id: CapitalKind; label: string }[] = [
    { id: 'opening', label: 'Opening balance' },
    { id: 'deposit', label: 'Deposit' },
    { id: 'withdrawal', label: 'Withdrawal' }
  ];

  let events = $state<CapitalEvent[]>([]);
  let error = $state('');
  let busy = $state(false);
  let kind = $state<CapitalKind>('opening');
  let date = $state(todayIst());
  let amount = $state('');
  let note = $state('');

  const inr = (n: number) => '₹' + n.toLocaleString('en-IN', { maximumFractionDigits: 2 });
  const label = (k: CapitalKind) => KINDS.find((x) => x.id === k)?.label ?? k;

  onMount(load);

  async function load() {
    try {
      events = await api.listCapitalEvents(account.id);
    } catch (e) {
      error = String(e);
    }
  }

  async function add() {
    error = '';
    busy = true;
    try {
      await api.addCapitalEvent({ accountId: account.id, date, kind, amount: Number(amount), note });
      amount = '';
      note = '';
      await load();
    } catch (e) {
      error = String(e);
    } finally {
      busy = false;
    }
  }

  async function remove(ev: CapitalEvent) {
    if (!confirm(`Remove this ${label(ev.kind).toLowerCase()} of ${inr(ev.amount)}?`)) return;
    try {
      await api.deleteCapitalEvent(ev.id);
      await load();
    } catch (e) {
      error = String(e);
    }
  }
</script>

<div class="flex flex-col gap-3">
  <h3 class="text-lg font-bold">Capital · {account.name}</h3>
  <p class="text-xs text-base-content/60">
    The Dashboard ROI is net P&amp;L divided by the balance at the start of the period. Enter the balance you had at the
    start of a day (it already includes that morning's deposits), then record later deposits and withdrawals. The
    latest opening balance on or before a period's start is the base; its trading P&amp;L is added by KISSA.
  </p>

  <div class="grid grid-cols-2 gap-2">
    <select class="select select-sm" bind:value={kind}>
      {#each KINDS as k}<option value={k.id}>{k.label}</option>{/each}
    </select>
    <input type="date" class="input input-sm" bind:value={date} max={todayIst()} />
    <input type="number" min="0" step="any" class="input input-sm" placeholder="Amount (₹)" bind:value={amount} />
    <input class="input input-sm" placeholder="Note (optional)" maxlength="200" bind:value={note} />
  </div>
  <button class="btn btn-primary btn-sm self-start" onclick={add} disabled={busy || !amount}>
    {#if busy}<span class="loading loading-spinner loading-xs"></span>{/if}Add
  </button>

  {#if error}<div class="alert alert-error alert-soft py-2 text-sm break-words">{error}</div>{/if}

  {#if events.length === 0}
    <div class="rounded-box border border-dashed border-base-300 px-4 py-3 text-sm text-base-content/50">
      No entries yet: the ROI stays blank for this account until an opening balance is added.
    </div>
  {:else}
    <div class="max-h-64 overflow-y-auto">
      <table class="table table-xs">
        <thead><tr><th>Date</th><th>Type</th><th class="text-right">Amount</th><th>Note</th><th></th></tr></thead>
        <tbody>
          {#each events as ev (ev.id)}
            <tr>
              <td class="num">{ev.date}</td>
              <td>{label(ev.kind)}</td>
              <td class="num text-right {ev.kind === 'withdrawal' ? 'text-error' : ''}">
                {ev.kind === 'withdrawal' ? '−' : ''}{inr(ev.amount)}
              </td>
              <td class="max-w-40 truncate text-base-content/60">{ev.note}</td>
              <td><button class="btn btn-ghost btn-xs text-error" onclick={() => remove(ev)}>Remove</button></td>
            </tr>
          {/each}
        </tbody>
      </table>
    </div>
  {/if}

  <div class="modal-action mt-0"><button class="btn btn-sm" onclick={onclose}>Close</button></div>
</div>
