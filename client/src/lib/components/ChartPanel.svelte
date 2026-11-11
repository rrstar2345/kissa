<script lang="ts">
  import { onMount } from 'svelte';
  import { listen, type UnlistenFn } from '@tauri-apps/api/event';
  import {
    createChart,
    createSeriesMarkers,
    CandlestickSeries,
    HistogramSeries,
    LineSeries,
    type IChartApi,
    type ISeriesApi,
    type ISeriesMarkersPluginApi,
    type SeriesMarker,
    type Time,
    type UTCTimestamp
  } from 'lightweight-charts';
  import { api } from '$lib/api';
  import { theme } from '$lib/theme.svelte';
  import {
    INDICATOR_PRESETS,
    INTERVALS,
    IST_SHIFT,
    bucketStart,
    instrumentLabel,
    parseInstrument,
    presetLabel,
    resolveColor
  } from '$lib/charts';
  import type { BarEvent, ChartData, IndicatorLine, InstrumentList, PanelConfig, Ruleset } from '$lib/types';
  import InstrumentPicker from './InstrumentPicker.svelte';

  let {
    config,
    list,
    rulesets = [],
    draft = null,
    focused = false,
    onfocus,
    onchange,
    onremove
  }: {
    config: PanelConfig;
    list: InstrumentList;
    /** Saved rulesets offered in the Signals dropdown. */
    rulesets?: Ruleset[];
    /** Unsaved ruleset previewed on top (Signals editor). */
    draft?: Ruleset | null;
    /** Highlights the panel (the Charts page's target for the positions dock). */
    focused?: boolean;
    /** Called when the person clicks or tabs into the panel. */
    onfocus?: () => void;
    onchange: (c: PanelConfig) => void;
    onremove?: () => void;
  } = $props();

  let host: HTMLDivElement;
  let chart: IChartApi | null = null;
  let candles: ISeriesApi<'Candlestick'> | null = null;
  let volume: ISeriesApi<'Histogram'> | null = null;
  let markers: ISeriesMarkersPluginApi<Time> | null = null;
  const lineSeries = new Map<string, { s: ISeriesApi<'Line' | 'Histogram'>; pane: string }>();
  let colors = { up: '#22c55e', down: '#ef4444', text: '#9ca3af', grid: '#374151', palette: [] as string[] };

  let status = $state('');
  let barCount = $state(0);
  let busy = $state(false);
  let backfillDays = $state(30);
  let lastKey = '';
  let timer: ReturnType<typeof setTimeout> | null = null;
  let seq = 0;

  const t = (ts: number) => (ts + IST_SHIFT) as UTCTimestamp;
  const parsed = $derived(parseInstrument(config.instrument));
  const isDaily = $derived(config.interval === '1d');

  function readColors() {
    colors = {
      up: resolveColor(host, '--color-success', '#22c55e'),
      down: resolveColor(host, '--color-error', '#ef4444'),
      text: resolveColor(host, '--color-base-content', '#9ca3af'),
      grid: resolveColor(host, '--color-base-300', '#374151'),
      palette: [
        resolveColor(host, '--color-primary', '#60a5fa'),
        resolveColor(host, '--color-warning', '#fbbf24'),
        resolveColor(host, '--color-secondary', '#c084fc'),
        resolveColor(host, '--color-accent', '#22d3ee'),
        resolveColor(host, '--color-info', '#38bdf8')
      ]
    };
  }

  function applyTheme() {
    if (!chart || !candles) return;
    readColors();
    chart.applyOptions({
      layout: { background: { color: 'transparent' }, textColor: colors.text },
      grid: { vertLines: { color: colors.grid }, horzLines: { color: colors.grid } },
      rightPriceScale: { borderColor: colors.grid },
      timeScale: { borderColor: colors.grid }
    });
    candles.applyOptions({
      upColor: colors.up,
      downColor: colors.down,
      wickUpColor: colors.up,
      wickDownColor: colors.down,
      borderVisible: false
    });
  }

  onMount(() => {
    chart = createChart(host, {
      autoSize: true,
      crosshair: { mode: 0 },
      timeScale: { timeVisible: true, secondsVisible: false }
    });
    candles = chart.addSeries(CandlestickSeries, {});
    markers = createSeriesMarkers(candles, []);
    applyTheme();
    schedule(0);

    let unlisten: UnlistenFn | null = null;
    listen<BarEvent[]>('bar-update', (e) => {
      if (!config.instrument || !e.payload.some((b) => b.instrument === config.instrument)) return;
      // The candle moves with every update (4 a second); volume and indicators
      // need the backend, so they refresh on a coalesced 2 s reload.
      applyLive(e.payload);
      schedule(2000);
    }).then((u) => (unlisten = u));

    return () => {
      unlisten?.();
      if (timer) clearTimeout(timer);
      chart?.remove();
      chart = null;
    };
  });

  // Theme switch: re-resolve colours and redraw indicator colours.
  $effect(() => {
    theme.current;
    if (chart) {
      applyTheme();
      // Indicator colours are fixed at creation; recreate them for the new theme.
      for (const e of lineSeries.values()) chart.removeSeries(e.s);
      lineSeries.clear();
      schedule(0);
    }
  });

  // Reload whenever the panel's instrument / interval / indicators change.
  $effect(() => {
    config.instrument;
    config.interval;
    JSON.stringify(config.indicators);
    JSON.stringify(config.rulesets ?? []);
    JSON.stringify(draft);
    config.volume;
    if (chart) schedule(0);
  });

  /** The candle currently on the right edge, kept so live updates can extend it. */
  let lastCandle: { time: UTCTimestamp; open: number; high: number; low: number; close: number } | null = null;

  /** Fold live 1m bar updates into the last candle (any interval) without a reload. */
  function applyLive(events: BarEvent[]) {
    if (!candles || !config.instrument) return;
    // Only once the panel shows this instrument / interval (not mid-switch).
    if (lastKey !== `${config.instrument}|${config.interval}`) return;
    for (const b of events) {
      if (b.instrument !== config.instrument) continue;
      const time = t(bucketStart(b.ts, config.interval));
      if (lastCandle && time < lastCandle.time) continue; // older than the visible edge
      const next =
        lastCandle && time === lastCandle.time
          ? { ...lastCandle, high: Math.max(lastCandle.high, b.high), low: Math.min(lastCandle.low, b.low), close: b.close }
          : { time, open: b.open, high: b.high, low: b.low, close: b.close };
      try {
        candles.update(next);
        lastCandle = next;
      } catch {
        /* series moved on (reload in flight): the next reload redraws it */
      }
    }
  }

  function schedule(ms: number) {
    if (timer) return; // coalesce bursts
    timer = setTimeout(() => {
      timer = null;
      load();
    }, ms);
  }

  async function load() {
    if (!chart || !candles) return;
    if (!config.instrument) {
      candles.setData([]);
      status = 'Pick an instrument';
      return;
    }
    const key = `${config.instrument}|${config.interval}`;
    const mine = ++seq;
    let data: ChartData;
    try {
      data = await api.getChart(config.instrument, config.interval, config.indicators, config.rulesets ?? [], draft);
    } catch (e) {
      status = String(e);
      return;
    }
    if (mine !== seq || !chart || !candles) return; // superseded
    status = data.warnings?.length ? data.warnings.join(' | ') : '';
    barCount = data.bars.length;
    candles.setData(
      data.bars.map((b) => ({ time: t(b.ts), open: b.open, high: b.high, low: b.low, close: b.close }))
    );
    const edge = data.bars[data.bars.length - 1];
    lastCandle = edge ? { time: t(edge.ts), open: edge.open, high: edge.high, low: edge.low, close: edge.close } : null;
    syncVolume(data);
    syncLines(data.lines);
    syncMarkers(data);
    if (key !== lastKey) {
      chart.timeScale().fitContent();
      lastKey = key;
    }
    if (!data.bars.length) status = 'No bars stored yet. Data is recorded 09:15–15:30 IST, or use Backfill.';
  }

  function syncVolume(data: ChartData) {
    if (!chart) return;
    const has = config.volume && data.bars.some((b) => b.volume > 0);
    if (!has) {
      if (volume) {
        chart.removeSeries(volume);
        volume = null;
      }
      return;
    }
    if (!volume) {
      volume = chart.addSeries(HistogramSeries, { priceFormat: { type: 'volume' }, priceScaleId: 'vol' });
      volume.priceScale().applyOptions({ scaleMargins: { top: 0.82, bottom: 0 } });
    }
    volume.setData(
      data.bars.map((b) => ({
        time: t(b.ts),
        value: b.volume,
        color: (b.close >= b.open ? colors.up : colors.down) + '66'
      }))
    );
  }

  function syncLines(lines: IndicatorLine[]) {
    if (!chart) return;
    const want = new Set(lines.map((l) => l.id));
    for (const [id, e] of lineSeries) {
      if (!want.has(id)) {
        chart.removeSeries(e.s);
        lineSeries.delete(id);
      }
    }
    // Sub-panes in order of first appearance: 1, 2, ...
    const panes: string[] = [];
    for (const l of lines) if (l.pane !== 'price' && !panes.includes(l.pane)) panes.push(l.pane);

    lines.forEach((l, i) => {
      let entry = lineSeries.get(l.id);
      if (!entry) {
        const paneIndex = l.pane === 'price' ? 0 : panes.indexOf(l.pane) + 1;
        const color =
          l.tone === 'up' ? colors.up : l.tone === 'down' ? colors.down : colors.palette[i % colors.palette.length];
        const s =
          l.style === 'histogram'
            ? chart!.addSeries(HistogramSeries, { color, priceLineVisible: false, lastValueVisible: false }, paneIndex)
            : chart!.addSeries(
                LineSeries,
                { color, lineWidth: 2, priceLineVisible: false, lastValueVisible: false, title: l.label },
                paneIndex
              );
        entry = { s, pane: l.pane };
        lineSeries.set(l.id, entry);
      }
      entry.s.setData(
        l.points.map((p) => (p.value === null ? { time: t(p.time) } : { time: t(p.time), value: p.value }))
      );
    });

    // Drop empty panes and give sub-panes a modest share of the height.
    try {
      const ps = chart.panes();
      for (let i = ps.length - 1; i >= 1; i--) if (ps[i].getSeries().length === 0) chart.removePane(i);
      const now = chart.panes();
      if (now.length > 1) {
        now[0].setStretchFactor(3);
        for (let i = 1; i < now.length; i++) now[i].setStretchFactor(1);
      }
    } catch {
      /* pane API unavailable: ignore */
    }
  }

  function syncMarkers(data: ChartData) {
    if (!markers) return;
    const ms: SeriesMarker<Time>[] = data.signals.slice(-200).map((s) => ({
      time: t(s.time),
      position: s.side === 'buy' ? 'belowBar' : 'aboveBar',
      color: s.side === 'buy' ? colors.up : colors.down,
      shape: s.side === 'buy' ? 'arrowUp' : 'arrowDown',
      text: s.source
    }));
    markers.setMarkers(ms);
  }

  const update = (patch: Partial<PanelConfig>) => onchange({ ...config, ...patch });

  const active = (key: string) => config.indicators.some((i) => i.key === key);
  function toggle(key: string) {
    const preset = INDICATOR_PRESETS.find((p) => p.key === key)!;
    update({
      indicators: active(key) ? config.indicators.filter((i) => i.key !== key) : [...config.indicators, preset]
    });
  }

  const ruleOn = (id: string) => (config.rulesets ?? []).includes(id);
  function toggleRuleset(id: string) {
    const cur = config.rulesets ?? [];
    update({ rulesets: cur.includes(id) ? cur.filter((x) => x !== id) : [...cur, id] });
  }

  async function backfill() {
    if (!config.instrument) return;
    busy = true;
    status = 'Fetching history from Fyers…';
    try {
      const r = await api.backfillChart(config.instrument, isDaily ? '1d' : '1m', backfillDays);
      status = `Backfill ${r.symbol}: ${r.inserted} new of ${r.fetched} bars`;
      schedule(0);
    } catch (e) {
      status = String(e);
    } finally {
      busy = false;
    }
  }
</script>

<!-- svelte-ignore a11y_no_static_element_interactions -->
<div
  class="flex min-h-0 flex-col rounded-box border bg-base-100 {focused ? 'border-primary' : 'border-base-300'}"
  onpointerdowncapture={() => onfocus?.()}
  onfocusincapture={() => onfocus?.()}
>
  <div class="flex flex-wrap items-center gap-2 border-b border-base-300 px-2 py-1.5">
    <InstrumentPicker value={config.instrument} {list} onpick={(k) => update({ instrument: k })} />
    <select class="select select-xs w-20" value={config.interval} onchange={(e) => update({ interval: e.currentTarget.value })}>
      {#each INTERVALS as i}<option value={i}>{i}</option>{/each}
    </select>

    <div class="dropdown">
      <div tabindex="0" role="button" class="btn btn-xs">Indicators ({config.indicators.length})</div>
      <ul class="dropdown-content menu z-20 w-52 rounded-box border border-base-300 bg-base-100 p-2 shadow">
        {#each INDICATOR_PRESETS as p}
          <li>
            <label class="flex cursor-pointer gap-2">
              <input type="checkbox" class="checkbox checkbox-xs" checked={active(p.key)} onchange={() => toggle(p.key)} />
              {presetLabel(p)}
            </label>
          </li>
        {/each}
        <li>
          <label class="flex cursor-pointer gap-2">
            <input type="checkbox" class="checkbox checkbox-xs" checked={config.volume} onchange={() => update({ volume: !config.volume })} />
            Volume
          </label>
        </li>
      </ul>
    </div>

    <div class="dropdown">
      <div tabindex="0" role="button" class="btn btn-xs">Signals ({(config.rulesets ?? []).filter((id) => rulesets.some((r) => r.id === id)).length})</div>
      <ul class="dropdown-content menu z-20 w-56 rounded-box border border-base-300 bg-base-100 p-2 shadow">
        {#each rulesets as r (r.id)}
          <li>
            <label class="flex cursor-pointer gap-2">
              <input type="checkbox" class="checkbox checkbox-xs" checked={ruleOn(r.id)} onchange={() => toggleRuleset(r.id)} />
              <span class="truncate">{r.name}</span>
            </label>
          </li>
        {:else}
          <li class="px-2 py-1 text-xs text-base-content/60">No rulesets yet.</li>
        {/each}
        <li><a href="/signals" class="text-xs">Edit rulesets…</a></li>
      </ul>
    </div>

    <div class="dropdown">
      <div tabindex="0" role="button" class="btn btn-xs">Backfill</div>
      <div class="dropdown-content z-20 w-56 space-y-2 rounded-box border border-base-300 bg-base-100 p-3 shadow">
        <p class="text-xs text-base-content/70">
          Fill gaps from Fyers ({isDaily ? 'daily' : '1-minute'} bars). Needs a Fyers data account.
        </p>
        <label class="flex items-center gap-2 text-xs">
          Days
          <input type="number" min="1" max="365" class="input input-xs w-20" bind:value={backfillDays} />
        </label>
        <button class="btn btn-xs btn-primary w-full" disabled={busy || !config.instrument} onclick={backfill}>
          {busy ? 'Fetching…' : 'Fetch history'}
        </button>
      </div>
    </div>

    <span class="ml-auto truncate text-xs text-base-content/60">{instrumentLabel(config.instrument)}{barCount ? ` · ${barCount} bars` : ''}</span>
    {#if onremove}
      <button class="btn btn-ghost btn-xs" title="Remove panel" onclick={onremove}>✕</button>
    {/if}
  </div>
  <div class="relative min-h-0 flex-1">
    <div bind:this={host} class="absolute inset-0"></div>
    {#if status}
      <div class="pointer-events-none absolute inset-x-0 top-2 flex justify-center">
        <span class="badge badge-neutral max-w-[90%] truncate">{status}</span>
      </div>
    {/if}
  </div>
</div>
