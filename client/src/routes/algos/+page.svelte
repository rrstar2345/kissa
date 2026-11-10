<script lang="ts">
  import { onMount, untrack } from 'svelte';
  import { api, getJson, setJson } from '$lib/api';
  import { trading, contractName } from '$lib/trading.svelte';
  import { istTime, num } from '$lib/format';
  import AlgoEditor from '$lib/components/AlgoEditor.svelte';
  import type { Account, Algo, AlgoJournalRow, AlgoView, CopierConfig, Ruleset } from '$lib/types';

  const SELECTED_KEY = 'algos.selected';

  const clone = <T,>(x: T): T => JSON.parse(JSON.stringify(x));
  const blank = (): Algo => ({
    id: '',
    name: 'New algo',
    signalId: '',
    instrument: { type: 'option', underlying: 'NIFTY', expiry: 'current', strikeOffset: 0, side: 'ce' },
    interval: '5m',
    action: { product: 'INTRADAY', capital: 15000, bufferTicks: 2, stopLossPct: null, targetPct: null },
    accounts: [],
    copier: false,
    allowLive: false
  });

  /** Empty number inputs give null / undefined: send what the backend expects. */
  function clean(a: Algo): Algo {
    const c = clone(a);
    const opt = (v: unknown) => (v === null || v === undefined || v === '' ? null : Number(v));
    c.action.capital = Number(c.action.capital) || 0;
    c.action.bufferTicks = Math.max(0, Math.floor(Number(c.action.bufferTicks) || 0));
    c.action.stopLossPct = opt(c.action.stopLossPct);
    c.action.targetPct = opt(c.action.targetPct);
    c.instrument.strikeOffset = Math.trunc(Number(c.instrument.strikeOffset) || 0);
    return c;
  }

  let views = $state<AlgoView[]>([]);
  let signals = $state<Ruleset[]>([]);
  let accounts = $state<Account[]>([]);
  let underlyings = $state<string[]>([]);
  let copier = $state<CopierConfig | null>(null);
  let journal = $state<AlgoJournalRow[]>([]);
  let allJournal = $state(false);

  let current = $state<Algo>(blank());
  let pendingSignal = $state<Ruleset | null>(null);
  let savedJson = $state('');
  let errors = $state<string[]>([]);
  let msg = $state('');
  let busy = $state(false);
  let ready = $state(false);

  const dirty = $derived(JSON.stringify(current) !== savedJson || pendingSignal !== null);
  const view = $derived(views.find((v) => v.algo.id === current.id) ?? null);
  /** A signal that comes with imported JSON is created on save, so "choose a signal" is not an error then. */
  const shownErrors = $derived(
    errors.filter((e) => !(pendingSignal && !current.signalId && e.startsWith('Choose the signal')))
  );
  const nameOf = (id: string) => accounts.find((a) => a.id === id)?.name ?? '(deleted account)';

  async function refresh() {
    try {
      views = await api.listAlgos();
      journal = await api.listAlgoJournal(allJournal ? null : current.id || '-', 300);
    } catch (e) {
      msg = String(e);
    }
  }

  async function loadRefs() {
    signals = await api.listRulesets().catch(() => signals);
    accounts = await api.listAccounts().catch(() => accounts);
    copier = await api.getCopier().then((o) => o.config).catch(() => copier);
  }

  onMount(() => {
    (async () => {
      await loadRefs();
      underlyings = await api.listFnoSymbols().catch(() => []);
      await refresh();
      const sel = await getJson<string>(SELECTED_KEY, '');
      load(views.find((v) => v.algo.id === sel) ?? views[0] ?? null);
      ready = true;
    })();
  });

  // Live: armed/disarmed, entries, exits and journal lines; order status changes close legs.
  $effect(() => {
    trading.algoVersion;
    trading.version;
    trading.copierVersion;
    untrack(() => {
      if (!ready) return;
      refresh();
      loadRefs();
    });
  });

  function load(v: AlgoView | null) {
    if (v) {
      current = clone(v.algo);
      savedJson = JSON.stringify(current);
      setJson(SELECTED_KEY, v.algo.id);
    } else {
      current = blank();
      savedJson = '';
    }
    pendingSignal = null;
    msg = '';
    refresh();
  }

  const discardOk = () => !dirty || confirm('Discard unsaved changes to this algo?');

  function pick(e: Event & { currentTarget: HTMLSelectElement }) {
    const id = e.currentTarget.value;
    if (!discardOk()) {
      e.currentTarget.value = current.id;
      return;
    }
    load(views.find((v) => v.algo.id === id) ?? null);
  }

  function create() {
    if (!discardOk()) return;
    load(null);
  }

  function duplicate() {
    const c = clone(current);
    current = { ...c, id: '', name: `${c.name} copy`, allowLive: false };
    savedJson = '';
    msg = '';
    refresh();
  }

  // Validate the form fields as you type (the database checks happen on save).
  $effect(() => {
    const snap = JSON.stringify(current);
    const t = setTimeout(async () => {
      errors = await api.validateAlgo(clean(JSON.parse(snap))).catch((x) => [String(x)]);
    }, 400);
    return () => clearTimeout(t);
  });

  async function save() {
    busy = true;
    msg = '';
    try {
      const a = clean(current);
      if (pendingSignal && !a.signalId) {
        const s = await api.saveRuleset(clone(pendingSignal));
        signals = await api.listRulesets();
        a.signalId = s.id;
      }
      const saved = await api.saveAlgo(a);
      pendingSignal = null;
      await refresh();
      load(views.find((v) => v.algo.id === saved.id) ?? null);
      msg = 'Saved';
    } catch (e) {
      msg = String(e);
    } finally {
      busy = false;
    }
  }

  async function remove() {
    if (!current.id || !confirm(`Delete algo “${current.name}”?`)) return;
    busy = true;
    try {
      await api.deleteAlgo(current.id);
      await refresh();
      load(views[0] ?? null);
    } catch (e) {
      msg = String(e);
    } finally {
      busy = false;
    }
  }

  async function toggleArm() {
    const v = view;
    if (!v) return;
    const arm = !v.armed;
    const live = v.algo.accounts.some((id) => accounts.find((a) => a.id === id)?.kind === 'dhan');
    if (arm && live && !confirm('This algo will place orders on LIVE accounts with real money. Arm it?')) return;
    busy = true;
    msg = '';
    try {
      await api.setAlgoArmed(v.algo.id, arm);
      await refresh();
    } catch (e) {
      msg = String(e);
    } finally {
      busy = false;
    }
  }

  async function clearJournal() {
    if (!confirm(allJournal ? 'Clear the journal of all algos?' : `Clear the journal of “${current.name}”?`)) return;
    await api.clearAlgoJournal(allJournal ? null : current.id).catch((e) => (msg = String(e)));
    await refresh();
  }

  const barTime = (ts: number) =>
    ts
      ? new Date(ts * 1000).toLocaleTimeString('en-GB', { timeZone: 'Asia/Kolkata', hour: '2-digit', minute: '2-digit' })
      : '–';
  const levelClass = (l: string) => (l === 'error' ? 'text-error' : l === 'warn' ? 'text-warning' : '');
</script>

<div class="flex h-full flex-col gap-3">
  <div class="flex flex-wrap items-center gap-2">
    <h1 class="text-xl font-bold">Algos</h1>
    <select class="select select-sm w-56" value={current.id} onchange={pick}>
      {#if !current.id}<option value="">(unsaved) {current.name}</option>{/if}
      {#each views as v (v.algo.id)}
        <option value={v.algo.id}>{v.armed ? '● ' : ''}{v.algo.name}</option>
      {/each}
    </select>
    <button class="btn btn-outline btn-sm" onclick={create}>New</button>
    <button class="btn btn-outline btn-sm" onclick={duplicate}>Duplicate</button>
    <button class="btn btn-outline btn-error btn-sm" disabled={!current.id || busy} onclick={remove}>Delete</button>
    <button class="btn btn-primary btn-sm" disabled={busy || !dirty || shownErrors.length > 0} onclick={save}>Save</button>
    {#if view}
      <button
        class="btn btn-sm {view.armed ? 'btn-warning' : 'btn-success'}"
        disabled={busy || dirty}
        title={dirty ? 'Save your changes first' : ''}
        onclick={toggleArm}
      >
        {view.armed ? 'Disarm' : 'Arm'}
      </button>
      {#if view.armed}
        <span class="badge badge-success badge-sm">Armed</span>
      {:else}
        <span class="badge badge-ghost badge-sm">Disarmed</span>
      {/if}
    {/if}
    {#if dirty}<span class="badge badge-warning badge-sm">Unsaved</span>{/if}
    {#if msg}<span class="text-xs {msg === 'Saved' ? 'text-success' : 'text-error'}">{msg}</span>{/if}
    <span class="ml-auto text-xs text-base-content/60">
      Runs only while the app is open. Paper accounts by default; the kill switch or a loss halt disarms an algo.
    </span>
  </div>

  {#if ready}
    <div class="grid min-h-0 flex-1 gap-3" style="grid-template-columns: minmax(380px, 520px) minmax(0, 1fr);">
      <div class="min-h-0 overflow-auto pr-1">
        <AlgoEditor
          bind:algo={current}
          bind:pendingSignal
          {signals}
          {accounts}
          {underlyings}
          {copier}
          errors={shownErrors}
        />
      </div>

      <div class="flex min-h-0 flex-col gap-3 overflow-auto">
        <section class="rounded-box border border-base-300 bg-base-100 p-3">
          <h2 class="mb-2 text-sm font-semibold">Status</h2>
          {#if !view}
            <p class="text-xs text-base-content/60">Save the algo to arm it.</p>
          {:else}
            <div class="flex flex-wrap items-center gap-2 text-sm">
              <span class="badge {view.phase === 'in_position' ? 'badge-info' : 'badge-outline'}">
                {view.phase === 'in_position' ? 'In position' : view.armed ? 'Flat, scanning for an entry' : 'Flat'}
              </span>
              <span class="text-xs text-base-content/60">
                Signal: {view.signalName ?? '(missing)'} · last bar looked at: {barTime(view.lastBarTs)} IST
              </span>
            </div>
            {#if view.legs.length}
              <div class="mt-2 overflow-x-auto">
                <table class="table table-xs">
                  <thead>
                    <tr><th>Contract</th><th>Account</th><th class="text-right">Lots</th><th class="text-right">Entry</th><th>Stop-loss / target</th></tr>
                  </thead>
                  <tbody>
                    {#each view.legs as l (l.orderId)}
                      <tr>
                        <td>{contractName(l)}</td>
                        <td>{nameOf(l.accountId)}</td>
                        <td class="text-right">{l.lots}</td>
                        <td class="text-right">{num(l.entryPrice)}</td>
                        <td class="text-xs">
                          {#if l.bracket}
                            bracket orders at the broker
                          {:else if l.stop || l.target}
                            {l.stop ? `SL ${num(l.stop)}` : ''} {l.target ? `T ${num(l.target)}` : ''} (watched by the algo)
                          {:else}
                            none
                          {/if}
                        </td>
                      </tr>
                    {/each}
                  </tbody>
                </table>
              </div>
            {/if}
          {/if}
        </section>

        <section class="min-h-0 rounded-box border border-base-300 bg-base-100 p-3">
          <div class="mb-2 flex items-center gap-2">
            <h2 class="text-sm font-semibold">Journal</h2>
            <label class="ml-2 flex cursor-pointer items-center gap-1 text-xs">
              <input type="checkbox" class="checkbox checkbox-xs" bind:checked={allJournal} onchange={refresh} />
              All algos
            </label>
            <button class="btn btn-ghost btn-xs ml-auto" onclick={clearJournal}>Clear</button>
          </div>
          {#if journal.length === 0}
            <p class="text-xs text-base-content/60">Every decision (signals, entries, exits, skips, errors) appears here.</p>
          {:else}
            <div class="overflow-x-auto">
              <table class="table table-xs">
                <tbody>
                  {#each journal as j (j.id)}
                    <tr class={levelClass(j.level)}>
                      <td class="whitespace-nowrap">{istTime(j.ts)}</td>
                      {#if allJournal}<td class="whitespace-nowrap">{j.algoName}</td>{/if}
                      <td><span class="badge badge-xs badge-outline">{j.kind}</span></td>
                      <td class="whitespace-normal">{j.message}</td>
                    </tr>
                  {/each}
                </tbody>
              </table>
            </div>
          {/if}
        </section>
      </div>
    </div>
  {/if}
</div>
