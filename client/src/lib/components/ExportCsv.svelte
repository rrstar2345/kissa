<script lang="ts">
  import { api } from '$lib/api';
  import { todayIst } from '$lib/format';
  import type { Account, ExportDataset } from '$lib/types';

  // The page's current filters seed the dialog each time it opens.
  let {
    dataset = 'trades',
    accountId = '',
    from = '',
    to = ''
  }: { dataset?: ExportDataset; accountId?: string; from?: string; to?: string } = $props();

  const DATASETS: { id: ExportDataset; label: string; hint: string }[] = [
    { id: 'orders', label: 'Orders', hint: 'Every order placed, with its status and fills.' },
    { id: 'trades', label: 'Trades', hint: 'Every fill with its realised P&L and charges.' },
    { id: 'diary', label: "Trader's diary", hint: 'One row per round trip, dated on the day it closed.' }
  ];

  type Preset = { label: string; from: string; to: string };
  const pad = (n: number) => String(n).padStart(2, '0');

  function presets(today: string): Preset[] {
    const y = Number(today.slice(0, 4));
    const m = Number(today.slice(5, 7));
    const prev = new Date(Date.UTC(y, m - 1, 0)); // last day of the previous month
    const py = prev.getUTCFullYear();
    const pm = prev.getUTCMonth() + 1;
    return [
      { label: 'Today', from: today, to: today },
      { label: 'This month', from: `${y}-${pad(m)}-01`, to: today },
      { label: 'Last month', from: `${py}-${pad(pm)}-01`, to: `${py}-${pad(pm)}-${pad(prev.getUTCDate())}` },
      { label: 'This year', from: `${y}-01-01`, to: today }
    ];
  }

  let show = $state(false);
  let ds = $state<ExportDataset>('trades');
  let acc = $state('');
  let start = $state('');
  let end = $state('');
  let quick = $state<Preset[]>([]);
  let accounts = $state<Account[]>([]);
  let busy = $state(false);
  let error = $state('');
  let notice = $state('');

  const invalid = $derived(!start || !end || start > end);

  async function openDialog() {
    const today = todayIst();
    ds = dataset;
    acc = accountId;
    start = from || `${today.slice(0, 7)}-01`;
    end = to || today;
    quick = presets(today);
    error = '';
    notice = '';
    show = true;
    if (!accounts.length) {
      try {
        accounts = (await api.listAccounts()).filter((a) => a.kind === 'paper' || a.kind === 'dhan');
      } catch (e) {
        error = String(e);
      }
    }
  }

  async function save() {
    if (invalid || busy) return;
    busy = true;
    error = '';
    notice = '';
    try {
      const r = await api.exportCsv({ dataset: ds, from: start, to: end, accountId: acc || null });
      if (r.rows === 0) notice = 'Nothing to export for that range.';
      else if (r.path) notice = `Saved ${r.rows} row${r.rows === 1 ? '' : 's'} to ${r.path}`;
      // Cancelled in the save dialog: stay here, say nothing.
    } catch (e) {
      error = String(e);
    } finally {
      busy = false;
    }
  }
</script>

<button class="btn btn-sm btn-outline" onclick={openDialog}>Export CSV</button>

{#if show}
  <dialog class="modal modal-open">
    <div class="modal-box max-w-md">
      <h3 class="text-lg font-bold">Export CSV</h3>
      <div class="mt-4 flex flex-col gap-4">
        <fieldset class="flex flex-col gap-2">
          <legend class="mb-1 text-xs text-base-content/60">What to export</legend>
          {#each DATASETS as d (d.id)}
            <label class="flex cursor-pointer items-start gap-2">
              <input type="radio" class="radio radio-sm mt-0.5" name="export-dataset" value={d.id} bind:group={ds} />
              <span class="text-sm">
                {d.label}
                <span class="block text-xs text-base-content/60">{d.hint}</span>
              </span>
            </label>
          {/each}
        </fieldset>

        <div class="flex flex-col gap-2">
          <div class="flex items-end gap-2">
            <label class="flex flex-1 flex-col gap-1">
              <span class="text-xs text-base-content/60">From</span>
              <input class="input input-sm w-full" type="date" bind:value={start} max={end || undefined} />
            </label>
            <label class="flex flex-1 flex-col gap-1">
              <span class="text-xs text-base-content/60">To</span>
              <input class="input input-sm w-full" type="date" bind:value={end} min={start || undefined} />
            </label>
          </div>
          <div class="flex flex-wrap gap-1">
            {#each quick as p (p.label)}
              <button
                class="btn btn-ghost btn-xs"
                onclick={() => {
                  start = p.from;
                  end = p.to;
                }}>{p.label}</button
              >
            {/each}
          </div>
          <span class="text-xs text-base-content/60">IST trade dates, both days included.</span>
        </div>

        <label class="flex flex-col gap-1">
          <span class="text-xs text-base-content/60">Account</span>
          <select class="select select-sm w-full" bind:value={acc}>
            <option value="">All accounts</option>
            {#each accounts as a (a.id)}<option value={a.id}>{a.name} ({a.kind})</option>{/each}
          </select>
        </label>

        {#if error}<div role="alert" class="alert alert-error alert-soft py-2 text-sm break-words">{error}</div>{/if}
        {#if notice}<div role="status" class="alert alert-success alert-soft py-2 text-sm break-all">{notice}</div>{/if}
      </div>
      <div class="modal-action">
        <button class="btn btn-ghost btn-sm" onclick={() => (show = false)}>Close</button>
        <button class="btn btn-primary btn-sm" onclick={save} disabled={invalid || busy}>
          {#if busy}<span class="loading loading-spinner loading-xs"></span>{/if} Save as…
        </button>
      </div>
    </div>
    <button class="modal-backdrop" aria-label="Close" onclick={() => (show = false)}></button>
  </dialog>
{/if}
