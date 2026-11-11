<script lang="ts">
  import { every } from '$lib/timers';
  import { onMount } from 'svelte';
  import { api, getJson, setJson } from '$lib/api';
  import { indexKey } from '$lib/charts';
  import type { InstrumentList, PanelConfig, Ruleset } from '$lib/types';
  import ChartPanel from '$lib/components/ChartPanel.svelte';
  import RulesetEditor from '$lib/components/RulesetEditor.svelte';

  const PANEL_KEY = 'signals.panel';
  const SELECTED_KEY = 'signals.selected';

  const clone = <T,>(x: T): T => JSON.parse(JSON.stringify(x));
  const blank = (): Ruleset => ({
    id: '',
    name: 'New ruleset',
    vars: [],
    entry: { op: 'and', items: [] },
    exit: { op: 'and', items: [] }
  });

  let rulesets = $state<Ruleset[]>([]);
  let current = $state<Ruleset>(blank());
  let savedJson = $state('');
  let errors = $state<string[]>([]);
  let draft = $state<Ruleset | null>(null);
  let msg = $state('');
  let busy = $state(false);
  let list = $state<InstrumentList>({ indices: [], options: [] });
  let panel = $state<PanelConfig>({
    id: 'signals',
    instrument: indexKey('NIFTY 50'),
    interval: '5m',
    indicators: [],
    rulesets: [],
    volume: true
  });
  let ready = $state(false);

  const dirty = $derived(JSON.stringify(current) !== savedJson);

  onMount(() => {
    (async () => {
      const saved = await getJson<PanelConfig | null>(PANEL_KEY, null);
      if (saved) panel = { ...saved, id: 'signals', rulesets: [] };
      rulesets = await api.listRulesets().catch(() => []);
      const sel = await getJson<string>(SELECTED_KEY, '');
      load(rulesets.find((r) => r.id === sel) ?? rulesets[0] ?? null);
      ready = true;
      list = await api.listChartInstruments().catch(() => list);
    })();
    const timer = every(async () => (list = await api.listChartInstruments().catch(() => list)), 30_000);
    return () => clearInterval(timer);
  });

  function load(r: Ruleset | null) {
    if (r) {
      current = clone(r);
      savedJson = JSON.stringify(current);
      setJson(SELECTED_KEY, r.id);
    } else {
      current = blank();
      savedJson = '';
    }
    msg = '';
  }

  const discardOk = () => !dirty || confirm('Discard unsaved changes to this ruleset?');

  function pick(e: Event & { currentTarget: HTMLSelectElement }) {
    const id = e.currentTarget.value;
    if (!discardOk()) {
      e.currentTarget.value = current.id;
      return;
    }
    load(rulesets.find((r) => r.id === id) ?? null);
  }

  function create() {
    if (!discardOk()) return;
    load(null);
  }

  function duplicate() {
    const c = clone(current);
    current = { ...c, id: '', name: `${c.name} copy` };
    savedJson = '';
    msg = '';
  }

  async function save() {
    busy = true;
    msg = '';
    try {
      const s = await api.saveRuleset(clone(current));
      rulesets = await api.listRulesets();
      load(rulesets.find((r) => r.id === s.id) ?? s);
      msg = 'Saved';
    } catch (e) {
      msg = String(e);
    } finally {
      busy = false;
    }
  }

  async function remove() {
    if (!current.id || !confirm(`Delete ruleset “${current.name}”?`)) return;
    busy = true;
    try {
      await api.deleteRuleset(current.id);
      rulesets = await api.listRulesets();
      load(rulesets[0] ?? null);
    } catch (e) {
      msg = String(e);
    } finally {
      busy = false;
    }
  }

  // Validate (debounced) and preview the draft on the chart. The chart keeps
  // the last valid draft while the form is mid-edit.
  $effect(() => {
    const snap = JSON.stringify(current);
    const t = setTimeout(async () => {
      const e = await api.validateRuleset(JSON.parse(snap)).catch((x) => [String(x)]);
      errors = e;
      if (!e.length) draft = JSON.parse(snap);
    }, 500);
    return () => clearTimeout(t);
  });

  let saveTimer: ReturnType<typeof setTimeout> | null = null;
  function onPanel(c: PanelConfig) {
    panel = { ...c, rulesets: [] };
    if (saveTimer) clearTimeout(saveTimer);
    saveTimer = setTimeout(() => setJson(PANEL_KEY, $state.snapshot(panel)), 400);
  }
</script>

<div class="flex h-full flex-col gap-3">
  <div class="flex flex-wrap items-center gap-2">
    <h1 class="text-xl font-bold">Signals</h1>
    <select class="select select-sm w-56" value={current.id} onchange={pick}>
      {#if !current.id}<option value="">(unsaved) {current.name}</option>{/if}
      {#each rulesets as r (r.id)}<option value={r.id}>{r.name}</option>{/each}
    </select>
    <button class="btn btn-outline btn-sm" onclick={create}>New</button>
    <button class="btn btn-outline btn-sm" onclick={duplicate}>Duplicate</button>
    <button class="btn btn-outline btn-error btn-sm" disabled={!current.id || busy} onclick={remove}>Delete</button>
    <button class="btn btn-primary btn-sm" disabled={busy || !dirty || errors.length > 0} onclick={save}>Save</button>
    {#if dirty}<span class="badge badge-warning badge-sm">Unsaved</span>{/if}
    {#if msg}<span class="text-xs {msg === 'Saved' ? 'text-success' : 'text-error'}">{msg}</span>{/if}
    <span class="ml-auto text-xs text-base-content/60">
      Entry = BUY marker, Exit = SELL marker; closed candles only. Apply saved rulesets on any chart via its Signals menu.
    </span>
  </div>

  {#if ready}
    <div class="grid min-h-0 flex-1 gap-3" style="grid-template-columns: minmax(380px, 520px) minmax(0, 1fr);">
      <div class="min-h-0 overflow-auto pr-1">
        <RulesetEditor bind:ruleset={current} {errors} />
      </div>
      <div class="grid min-h-0" style="grid-template-rows: minmax(420px, 1fr);">
        <ChartPanel config={panel} {list} {rulesets} {draft} onchange={onPanel} />
      </div>
    </div>
  {/if}
</div>
