<script lang="ts">
  import { every } from '$lib/timers';
  import { onMount } from 'svelte';
  import { api } from '$lib/api';
  import { getScoped, setScoped } from '$lib/windowing';
  import { INDICATOR_PRESETS, indexKey, optionKey } from '$lib/charts';
  import type { ChartsLayout, InstrumentList, PanelConfig, Position, Ruleset } from '$lib/types';
  import ChartPanel from '$lib/components/ChartPanel.svelte';
  import PositionsDock from '$lib/components/PositionsDock.svelte';

  const LAYOUT_KEY = 'charts.layout';

  const newPanel = (instrument: string | null = null): PanelConfig => ({
    id: crypto.randomUUID(),
    instrument,
    interval: '5m',
    indicators: [INDICATOR_PRESETS.find((p) => p.key === 'ema:21')!],
    volume: true
  });

  let layout = $state<ChartsLayout>({ cols: 1, panels: [] });
  let list = $state<InstrumentList>({ indices: [], options: [] });
  let rulesets = $state<Ruleset[]>([]);
  let ready = $state(false);

  onMount(() => {
    (async () => {
      const saved = await getScoped<ChartsLayout | null>(LAYOUT_KEY, null);
      layout = saved?.panels?.length ? saved : { cols: 1, panels: [newPanel(indexKey('NIFTY 50'))] };
      ready = true;
      await refreshList();
    })();
    const timer = every(refreshList, 30_000);
    return () => clearInterval(timer);
  });

  async function refreshList() {
    list = await api.listChartInstruments().catch(() => list);
    rulesets = await api.listRulesets().catch(() => rulesets);
  }

  let saveTimer: ReturnType<typeof setTimeout> | null = null;
  function save() {
    if (saveTimer) clearTimeout(saveTimer);
    saveTimer = setTimeout(() => setScoped(LAYOUT_KEY, $state.snapshot(layout)), 400);
  }

  function updatePanel(i: number, c: PanelConfig) {
    layout.panels[i] = c;
    save();
  }
  function removePanel(i: number) {
    layout.panels.splice(i, 1);
    save();
  }
  function addPanel() {
    layout.panels.push(newPanel(layout.panels.at(-1)?.instrument ?? indexKey('NIFTY 50')));
    save();
  }
  function setCols(c: 1 | 2 | 3) {
    layout.cols = c;
    save();
  }

  // The panel the positions dock acts on: the one last clicked, else the first.
  let focusedId = $state<string | null>(null);
  const targetId = $derived(
    layout.panels.find((p) => p.id === focusedId)?.id ?? layout.panels[0]?.id ?? null
  );
  const multi = $derived(layout.panels.length > 1);

  const keyOf = (p: Position) => optionKey(p.symbol, p.expiry, p.strike, p.isCall ? 'CE' : 'PE');

  function showPosition(p: Position) {
    const i = layout.panels.findIndex((x) => x.id === targetId);
    if (i >= 0) updatePanel(i, { ...layout.panels[i], instrument: keyOf(p) });
  }
</script>

<div class="flex h-full flex-col gap-3">
  <div class="flex items-center gap-3">
    <h1 class="text-xl font-bold">Charts</h1>
    <div class="join">
      {#each [1, 2, 3] as c}
        <button class="btn join-item btn-xs" class:btn-primary={layout.cols === c} onclick={() => setCols(c as 1 | 2 | 3)}>
          {c} col
        </button>
      {/each}
    </div>
    <button class="btn btn-xs btn-outline" onclick={addPanel}>+ Add chart</button>
    <span class="ml-auto text-xs text-base-content/60">
      Bars are stored for indices and the option strikes streamed on the Option Chain page. Times shown in IST.
    </span>
  </div>

  {#if ready}
    <div
      class="grid min-h-0 flex-1 gap-3 overflow-auto"
      style="grid-template-columns: repeat({layout.cols}, minmax(0, 1fr)); grid-auto-rows: minmax(420px, 1fr);"
    >
      {#each layout.panels as p, i (p.id)}
        <ChartPanel
          config={p}
          {list}
          {rulesets}
          focused={multi && p.id === targetId}
          onfocus={() => (focusedId = p.id)}
          onchange={(c) => updatePanel(i, c)}
          onremove={() => removePanel(i)}
        />
      {/each}
    </div>
  {/if}

  <PositionsDock
    onpick={showPosition}
    isActive={(p) => keyOf(p) === layout.panels.find((x) => x.id === targetId)?.instrument}
    hint={multi ? 'Click a contract to chart it in the highlighted panel' : 'Click a contract to chart it'}
  />
</div>
