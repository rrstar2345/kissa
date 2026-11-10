<script lang="ts">
  import type { Account, Algo, CopierConfig, ExpirySel, Ruleset, SideSel } from '$lib/types';

  interface Props {
    algo: Algo;
    /** A signal from imported JSON that does not exist here yet; created on save. */
    pendingSignal: Ruleset | null;
    signals: Ruleset[];
    accounts: Account[];
    underlyings: string[];
    copier: CopierConfig | null;
    errors: string[];
  }
  let { algo = $bindable(), pendingSignal = $bindable(), signals, accounts, underlyings, copier, errors }: Props =
    $props();

  const EXPIRIES: { v: ExpirySel; label: string }[] = [
    { v: 'current', label: 'Current' },
    { v: 'next', label: 'Next' },
    { v: 'next1', label: 'Next +1' },
    { v: 'next2', label: 'Next +2' },
    { v: 'next3', label: 'Next +3' }
  ];
  const SIDES: { v: SideSel; label: string }[] = [
    { v: 'ce', label: 'CE (calls)' },
    { v: 'pe', label: 'PE (puts)' },
    { v: 'both', label: 'Both (CE + PE)' }
  ];
  const INTERVALS = ['1m', '3m', '5m', '10m', '15m', '30m', '60m'];
  const OFFSETS = Array.from({ length: 21 }, (_, i) => i - 10);
  const offsetLabel = (n: number) => (n === 0 ? 'ATM' : n > 0 ? `ATM + ${n}` : `ATM - ${-n}`);

  const tradable = $derived(accounts.filter((a) => a.kind === 'paper' || a.kind === 'dhan'));
  const unders = $derived(
    underlyings.includes(algo.instrument.underlying) ? underlyings : [algo.instrument.underlying, ...underlyings]
  );
  const hasLive = $derived(algo.accounts.some((id) => accounts.find((a) => a.id === id)?.kind === 'dhan'));
  const nameOf = (id: string) => accounts.find((a) => a.id === id)?.name ?? '(deleted account)';

  /** Mirrors the backend rule (algo::select_accounts) to explain what will happen. */
  const copierNote = $derived.by(() => {
    if (!copier || !copier.masterAccountId) return '';
    const master = copier.masterAccountId;
    const masterIn = algo.accounts.includes(master);
    if (algo.copier) {
      if (!copier.enabled) return 'The trade copier is switched off on the Copier page, so nothing is replicated and every selected account trades by itself.';
      if (!masterIn) return `The copier master (${nameOf(master)}) is not selected, so every selected account trades by itself.`;
      const kids = copier.children.filter((c) => c.enabled && algo.accounts.includes(c.accountId));
      return kids.length
        ? `Orders go to ${nameOf(master)} only; the trade copier replicates them. Ignored here: ${kids.map((c) => nameOf(c.accountId)).join(', ')}.`
        : `Orders on ${nameOf(master)} are replicated to its copier children.`;
    }
    return masterIn ? `Copier off: orders on ${nameOf(master)} are not replicated to its children.` : '';
  });

  function toggleAccount(id: string, on: boolean) {
    algo.accounts = on ? [...algo.accounts.filter((a) => a !== id), id] : algo.accounts.filter((a) => a !== id);
  }

  function toggleLive(e: Event & { currentTarget: HTMLInputElement }) {
    const on = e.currentTarget.checked;
    if (on && !confirm('Allow this algo to place orders on live (Dhan) accounts? Real money will be used once it is armed.')) {
      e.currentTarget.checked = false;
      return;
    }
    algo.allowLive = on;
  }

  function onSignalChange() {
    if (algo.signalId) pendingSignal = null;
  }

  // ------------------------------------------------------------ JSON mode ---

  let mode = $state<'form' | 'json'>('form');
  let jsonText = $state('');
  let jsonError = $state('');
  let jsonNote = $state('');

  const stripId = (r: Ruleset) => ({ name: r.name, vars: r.vars, entry: r.entry, exit: r.exit });

  /** The shareable document: rules, instrument and action. No accounts, ids or live permission. */
  function toPortable(): string {
    const a = $state.snapshot(algo);
    const sig = signals.find((s) => s.id === a.signalId) ?? pendingSignal;
    const doc = {
      kissaAlgo: 1,
      algo: { name: a.name, instrument: a.instrument, interval: a.interval, action: a.action, copier: a.copier },
      signal: sig ? stripId($state.snapshot(sig) as Ruleset) : null
    };
    return JSON.stringify(doc, null, 2);
  }

  function applyJson(): boolean {
    try {
      const doc = JSON.parse(jsonText);
      if (!doc || typeof doc !== 'object' || Array.isArray(doc)) throw new Error('expected a JSON object');
      const src = doc.algo && typeof doc.algo === 'object' ? doc.algo : doc;
      if (typeof src.name === 'string') algo.name = src.name;
      algo.instrument = { ...algo.instrument, ...(src.instrument ?? {}), type: 'option' };
      if (typeof src.interval === 'string') algo.interval = src.interval;
      algo.action = { ...algo.action, ...(src.action ?? {}) };
      if (typeof src.copier === 'boolean') algo.copier = src.copier;

      jsonNote = '';
      const sig = doc.signal;
      if (sig && typeof sig === 'object') {
        const incoming: Ruleset = {
          id: '',
          name: String(sig.name ?? 'Imported signal'),
          vars: sig.vars ?? [],
          entry: sig.entry ?? { op: 'and', items: [] },
          exit: sig.exit ?? { op: 'and', items: [] }
        };
        const existing = signals.find((s) => s.name.toLowerCase() === incoming.name.toLowerCase());
        if (existing) {
          algo.signalId = existing.id;
          pendingSignal = null;
          jsonNote =
            JSON.stringify(stripId(existing)) === JSON.stringify(incoming)
              ? `Using your saved signal “${existing.name}”.`
              : `You already have a signal named “${existing.name}” with different rules; yours is used. Rename the signal in the JSON to import it as a new one.`;
        } else {
          algo.signalId = '';
          pendingSignal = incoming;
          jsonNote = `The signal “${incoming.name}” is new here and will be created when you save.`;
        }
      }
      jsonError = '';
      return true;
    } catch (e) {
      jsonError = `Invalid JSON: ${e instanceof Error ? e.message : e}`;
      return false;
    }
  }

  function setMode(m: 'form' | 'json') {
    if (m === mode) return;
    if (m === 'json') {
      jsonText = toPortable();
      jsonError = '';
      jsonNote = '';
      mode = 'json';
    } else if (applyJson()) mode = 'form';
  }

  async function copyJson() {
    try {
      await navigator.clipboard.writeText(jsonText);
      jsonNote = 'Copied to the clipboard.';
    } catch {
      jsonNote = 'Could not copy: select the text and copy it manually.';
    }
  }
</script>

<div class="space-y-3 pb-4">
  <div class="flex items-center gap-2">
    <input class="input input-sm flex-1 font-semibold" placeholder="Algo name" bind:value={algo.name} />
    <div class="join">
      <button class="btn join-item btn-xs" class:btn-primary={mode === 'form'} onclick={() => setMode('form')}>Form</button>
      <button class="btn join-item btn-xs" class:btn-primary={mode === 'json'} onclick={() => setMode('json')}>JSON</button>
    </div>
  </div>

  {#if mode === 'json'}
    <textarea class="textarea h-[60vh] w-full font-mono text-xs" spellcheck="false" bind:value={jsonText}></textarea>
    <div class="flex flex-wrap items-center gap-2">
      <button class="btn btn-primary btn-xs" onclick={applyJson}>Apply</button>
      <button class="btn btn-outline btn-xs" onclick={copyJson}>Copy</button>
      <span class="text-xs text-base-content/60">
        For sharing: rules, instrument and action with the signal included. Accounts and live permission stay on your
        machine. Switching to Form also applies it.
      </span>
    </div>
    {#if jsonError}<p class="text-xs text-error">{jsonError}</p>{/if}
    {#if jsonNote}<p class="text-xs text-info">{jsonNote}</p>{/if}
  {:else}
    <section class="rounded-box border border-base-300 bg-base-100 p-3">
      <h2 class="mb-2 text-sm font-semibold">Signal</h2>
      <div class="flex flex-wrap items-center gap-2">
        <select class="select select-sm w-64" bind:value={algo.signalId} onchange={onSignalChange}>
          {#if !algo.signalId}
            <option value="">{pendingSignal ? `(new) ${pendingSignal.name}` : 'Choose a signal…'}</option>
          {/if}
          {#each signals as s (s.id)}<option value={s.id}>{s.name}</option>{/each}
        </select>
        <select class="select select-sm w-28" bind:value={algo.interval} title="Bar size the signal is evaluated on">
          {#each INTERVALS as i}<option value={i}>{i} bars</option>{/each}
          {#if !INTERVALS.includes(algo.interval)}<option value={algo.interval}>{algo.interval} bars</option>{/if}
        </select>
      </div>
      <p class="mt-2 text-xs text-base-content/60">
        Evaluated on the underlying's closed bars. Entry opens the position, Exit closes it. Editing the signal on the
        Signals page changes every algo that uses it.
      </p>
      {#if pendingSignal && !algo.signalId}
        <p class="mt-1 text-xs text-info">The signal “{pendingSignal.name}” will be created when you save.</p>
      {/if}
    </section>

    <section class="rounded-box border border-base-300 bg-base-100 p-3">
      <h2 class="mb-2 text-sm font-semibold">Instrument</h2>
      <div class="grid grid-cols-2 gap-2">
        <label class="flex flex-col gap-1 text-xs">
          Type
          <select class="select select-sm" value="option" disabled title="Stocks come later">
            <option value="option">Option</option>
            <option value="stock" disabled>Stock (not supported yet)</option>
          </select>
        </label>
        <label class="flex flex-col gap-1 text-xs">
          Underlying
          <select class="select select-sm" bind:value={algo.instrument.underlying}>
            {#each unders as u}<option value={u}>{u}</option>{/each}
          </select>
        </label>
        <label class="flex flex-col gap-1 text-xs">
          Expiry
          <select class="select select-sm" bind:value={algo.instrument.expiry}>
            {#each EXPIRIES as e}<option value={e.v}>{e.label}</option>{/each}
          </select>
        </label>
        <label class="flex flex-col gap-1 text-xs">
          Strike
          <select class="select select-sm" bind:value={algo.instrument.strikeOffset}>
            {#each OFFSETS as o}<option value={o}>{offsetLabel(o)}</option>{/each}
          </select>
        </label>
        <label class="col-span-2 flex flex-col gap-1 text-xs">
          Side
          <select class="select select-sm" bind:value={algo.instrument.side}>
            {#each SIDES as s}<option value={s.v}>{s.label}</option>{/each}
          </select>
        </label>
      </div>
      <p class="mt-2 text-xs text-base-content/60">
        ATM is the strike nearest to the spot at the entry bar; ATM + 1 is one strike above it. The strike is fixed
        until the position is closed, then the next entry is worked out from the then-current spot.
      </p>
    </section>

    <section class="rounded-box border border-base-300 bg-base-100 p-3">
      <h2 class="mb-2 text-sm font-semibold">Order</h2>
      <div class="grid grid-cols-2 gap-2">
        <label class="flex flex-col gap-1 text-xs">
          Capital per account (₹)
          <input class="input input-sm" type="number" min="0" step="100" bind:value={algo.action.capital} />
        </label>
        <label class="flex flex-col gap-1 text-xs">
          Product
          <select class="select select-sm" bind:value={algo.action.product}>
            <option value="INTRADAY">INTRADAY</option>
            <option value="MARGIN">MARGIN</option>
          </select>
        </label>
        <label class="flex flex-col gap-1 text-xs">
          Buffer (ticks of ₹0.05)
          <input class="input input-sm" type="number" min="0" max="100" step="1" bind:value={algo.action.bufferTicks} />
        </label>
        <span></span>
        <label class="flex flex-col gap-1 text-xs">
          Stop-loss % (below entry)
          <input class="input input-sm" type="number" min="0" max="99" step="0.5" placeholder="none" bind:value={algo.action.stopLossPct} />
        </label>
        <label class="flex flex-col gap-1 text-xs">
          Target % (above entry)
          <input class="input input-sm" type="number" min="0" step="0.5" placeholder="none" bind:value={algo.action.targetPct} />
        </label>
      </div>
      <p class="mt-2 text-xs text-base-content/60">
        BUY LIMIT at last price + buffer. Lots = capital ÷ (price × lot size), rounded down; with Both the capital is
        split between CE and PE. Whichever comes first closes a leg: the signal's Exit, the stop-loss or the target.
        Paper accounts get bracket orders; live accounts are watched by the algo while the app runs.
      </p>
    </section>

    <section class="rounded-box border border-base-300 bg-base-100 p-3">
      <h2 class="mb-2 text-sm font-semibold">Accounts</h2>
      {#if tradable.length === 0}
        <p class="text-xs text-base-content/60">No paper or Dhan accounts yet. Add one on the Accounts page.</p>
      {/if}
      <div class="space-y-1">
        {#each tradable as a (a.id)}
          <label class="flex cursor-pointer items-center gap-2 text-sm">
            <input type="checkbox" class="checkbox checkbox-sm" checked={algo.accounts.includes(a.id)} onchange={(e) => toggleAccount(a.id, e.currentTarget.checked)} />
            <span>{a.name}</span>
            <span class="badge badge-sm badge-outline">{a.kind}</span>
            {#if copier?.masterAccountId === a.id}<span class="badge badge-sm badge-info">copier master</span>{/if}
            {#if copier?.children.some((c) => c.accountId === a.id)}<span class="badge badge-sm badge-ghost">copier child</span>{/if}
          </label>
        {/each}
      </div>
      <label class="mt-3 flex cursor-pointer items-center gap-2 text-sm">
        <input type="checkbox" class="toggle toggle-sm toggle-primary" bind:checked={algo.copier} />
        Trade copier
      </label>
      {#if copierNote}<p class="mt-1 text-xs text-base-content/70">{copierNote}</p>{/if}
      {#if hasLive || algo.allowLive}
        <label class="mt-3 flex cursor-pointer items-center gap-2 text-sm">
          <input type="checkbox" class="toggle toggle-sm toggle-warning" checked={algo.allowLive} onchange={toggleLive} />
          Allow live accounts
        </label>
        <p class="mt-1 text-xs text-warning">A live (Dhan) account can only be traded when this is on. Real money.</p>
      {/if}
    </section>
  {/if}

  {#if errors.length}
    <ul class="space-y-0.5 text-xs text-error">
      {#each errors as e}<li>{e}</li>{/each}
    </ul>
  {/if}
</div>
