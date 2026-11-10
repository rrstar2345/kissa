<script lang="ts">
  import { onMount } from 'svelte';
  import { api, getJson, setJson } from '$lib/api';
  import { indexKey } from '$lib/charts';
  import { num, signed, trend } from '$lib/format';
  import type { BacktestReport, BacktestRequest, InstrumentList, Ruleset } from '$lib/types';
  import InstrumentPicker from '$lib/components/InstrumentPicker.svelte';

  const KEY = 'backtest.form';
  const INTERVALS = ['1m', '3m', '5m', '15m', '30m', '60m', '1d'];
  const REASONS: Record<string, string> = {
    signal: 'Signal',
    stop_loss: 'Stop-loss',
    target: 'Target',
    eod: 'Day end',
    end: 'Data end'
  };

  interface Form {
    rulesetId: string;
    instrument: string;
    interval: string;
    from: string;
    to: string;
    sizing: 'lots' | 'capital';
    lots: number;
    capitalPerTrade: number;
    lotSize: number;
    startingCapital: number;
    stopLossPct: number | null;
    targetPct: number | null;
    slippageTicks: number;
    charges: boolean;
    brokerage: number | null;
    carryOvernight: boolean;
  }

  let form = $state<Form>({
    rulesetId: '',
    instrument: indexKey('NIFTY 50'),
    interval: '5m',
    from: '',
    to: '',
    sizing: 'lots',
    lots: 1,
    capitalPerTrade: 50000,
    lotSize: 0,
    startingCapital: 100000,
    stopLossPct: null,
    targetPct: null,
    slippageTicks: 0,
    charges: true,
    brokerage: null,
    carryOvernight: false
  });
  let rulesets = $state<Ruleset[]>([]);
  let list = $state<InstrumentList>({ indices: [], options: [] });
  let report = $state<BacktestReport | null>(null);
  let busy = $state(false);
  let error = $state('');
  let ready = $state(false);

  onMount(() => {
    (async () => {
      const saved = await getJson<Partial<Form> | null>(KEY, null);
      if (saved) form = { ...form, ...saved };
      rulesets = await api.listRulesets().catch(() => []);
      if (!rulesets.some((r) => r.id === form.rulesetId)) form.rulesetId = rulesets[0]?.id ?? '';
      list = await api.listChartInstruments().catch(() => list);
      ready = true;
    })();
  });

  const isDaily = $derived(form.interval === '1d');
  const opt = (v: number | null | undefined) => (v && v > 0 ? v : null);

  async function run() {
    if (!form.rulesetId) {
      error = 'Pick a signal first (create one on the Signals page).';
      return;
    }
    busy = true;
    error = '';
    const req: BacktestRequest = {
      rulesetId: form.rulesetId,
      instrument: form.instrument,
      interval: form.interval,
      from: form.from || null,
      to: form.to || null,
      lots: form.sizing === 'lots' ? Math.max(0, Math.floor(form.lots || 0)) : 0,
      capitalPerTrade: form.sizing === 'capital' ? form.capitalPerTrade || 0 : 0,
      lotSize: Math.max(0, Math.floor(form.lotSize || 0)),
      startingCapital: form.startingCapital || 0,
      stopLossPct: opt(form.stopLossPct),
      targetPct: opt(form.targetPct),
      slippageTicks: Math.max(0, Math.floor(form.slippageTicks || 0)),
      charges: form.charges,
      brokeragePerOrder: form.brokerage === null || form.brokerage === undefined ? null : Math.max(0, form.brokerage),
      carryOvernight: form.carryOvernight
    };
    try {
      report = await api.runBacktest(req);
      setJson(KEY, $state.snapshot(form));
    } catch (e) {
      error = String(e);
      report = null;
    } finally {
      busy = false;
    }
  }

  const ist = (ts: number) => new Date((ts + 19800) * 1000).toISOString().slice(0, 16).replace('T', ' ');
  const istDay = (ts: number) => ist(ts).slice(0, 10);
  const px = (v: number) => num(v, 2);
  const pct = (v: number | null | undefined) => (v === null || v === undefined ? '—' : `${num(v)}%`);

  // Equity curve (closed trades), as an SVG polyline.
  const W = 640;
  const H = 170;
  const curve = $derived.by(() => {
    const eq = report?.equity ?? [];
    if (eq.length === 0) return null;
    const ys = eq.map((p) => p.equity);
    const base = ys[0] - report!.trades[0].net; // equity before the first trade
    const all = [base, ...ys];
    const lo = Math.min(...all);
    const hi = Math.max(...all);
    const span = hi - lo || 1;
    const x = (i: number) => (all.length === 1 ? 0 : (i / (all.length - 1)) * W);
    const y = (v: number) => H - 6 - ((v - lo) / span) * (H - 12);
    return {
      points: all.map((v, i) => `${x(i).toFixed(1)},${y(v).toFixed(1)}`).join(' '),
      baseY: y(base),
      lo,
      hi
    };
  });
</script>

<div class="flex h-full flex-col gap-3">
  <div class="flex flex-wrap items-baseline gap-2">
    <h1 class="text-xl font-bold">Backtest</h1>
    <span class="text-xs text-base-content/60">
      Runs a saved signal over the bars stored for one instrument. Long only; orders fill at the next bar's open.
    </span>
  </div>

  {#if ready}
    <div class="grid min-h-0 flex-1 gap-3" style="grid-template-columns: minmax(340px, 400px) minmax(0, 1fr);">
      <!-- Form -->
      <div class="min-h-0 overflow-auto pr-1">
        <div class="card bg-base-100 shadow-sm">
          <div class="card-body gap-3 p-4">
            <label class="form-control">
              <span class="label-text mb-1 text-xs">Signal</span>
              <select class="select select-sm w-full" bind:value={form.rulesetId}>
                {#if rulesets.length === 0}<option value="">No saved signals</option>{/if}
                {#each rulesets as r (r.id)}<option value={r.id}>{r.name}</option>{/each}
              </select>
            </label>

            <div>
              <span class="label-text mb-1 block text-xs">Instrument (stored bars)</span>
              <InstrumentPicker value={form.instrument} {list} onpick={(k) => (form.instrument = k)} />
              <div class="num mt-1 truncate text-xs text-base-content/60">{form.instrument}</div>
            </div>

            <div class="grid grid-cols-3 gap-2">
              <label class="form-control">
                <span class="label-text mb-1 text-xs">Interval</span>
                <select class="select select-sm w-full" bind:value={form.interval}>
                  {#each INTERVALS as i}<option value={i}>{i}</option>{/each}
                </select>
              </label>
              <label class="form-control">
                <span class="label-text mb-1 text-xs">From (IST)</span>
                <input class="input input-sm w-full" type="date" bind:value={form.from} />
              </label>
              <label class="form-control">
                <span class="label-text mb-1 text-xs">To (IST)</span>
                <input class="input input-sm w-full" type="date" bind:value={form.to} />
              </label>
            </div>
            <p class="-mt-1 text-xs text-base-content/50">Empty dates = all stored data. Bars before “From” only warm up the indicators.</p>

            <div class="divider my-0"></div>

            <div class="join">
              <button class="btn join-item btn-xs" class:btn-primary={form.sizing === 'lots'} onclick={() => (form.sizing = 'lots')}>Fixed lots</button>
              <button class="btn join-item btn-xs" class:btn-primary={form.sizing === 'capital'} onclick={() => (form.sizing = 'capital')}>Capital per trade</button>
            </div>
            <div class="grid grid-cols-2 gap-2">
              {#if form.sizing === 'lots'}
                <label class="form-control">
                  <span class="label-text mb-1 text-xs">Lots per trade</span>
                  <input class="input input-sm w-full" type="number" min="1" step="1" bind:value={form.lots} />
                </label>
              {:else}
                <label class="form-control">
                  <span class="label-text mb-1 text-xs">Capital per trade (₹)</span>
                  <input class="input input-sm w-full" type="number" min="0" step="1000" bind:value={form.capitalPerTrade} />
                </label>
              {/if}
              <label class="form-control">
                <span class="label-text mb-1 text-xs">Lot size (0 = auto)</span>
                <input class="input input-sm w-full" type="number" min="0" step="1" bind:value={form.lotSize} />
              </label>
              <label class="form-control">
                <span class="label-text mb-1 text-xs">Stop-loss %</span>
                <input class="input input-sm w-full" type="number" min="0" step="0.5" placeholder="off" bind:value={form.stopLossPct} />
              </label>
              <label class="form-control">
                <span class="label-text mb-1 text-xs">Target %</span>
                <input class="input input-sm w-full" type="number" min="0" step="0.5" placeholder="off" bind:value={form.targetPct} />
              </label>
              <label class="form-control">
                <span class="label-text mb-1 text-xs">Slippage (ticks of 0.05)</span>
                <input class="input input-sm w-full" type="number" min="0" max="100" step="1" bind:value={form.slippageTicks} />
              </label>
              <label class="form-control">
                <span class="label-text mb-1 text-xs">Starting capital (₹)</span>
                <input class="input input-sm w-full" type="number" min="0" step="10000" bind:value={form.startingCapital} />
              </label>
            </div>

            <label class="flex cursor-pointer items-center gap-2 text-sm">
              <input type="checkbox" class="toggle toggle-sm" bind:checked={form.charges} />
              Deduct estimated charges
            </label>
            {#if form.charges}
              <label class="form-control">
                <span class="label-text mb-1 text-xs">Brokerage per order (₹, empty = Dhan default from settings)</span>
                <input class="input input-sm w-full" type="number" min="0" step="1" placeholder="default" bind:value={form.brokerage} />
              </label>
            {/if}
            <label class="flex cursor-pointer items-center gap-2 text-sm" class:opacity-50={isDaily}>
              <input type="checkbox" class="toggle toggle-sm" disabled={isDaily} checked={isDaily || form.carryOvernight} onchange={(e) => (form.carryOvernight = e.currentTarget.checked)} />
              Hold positions overnight {isDaily ? '(always on for daily bars)' : '(off = closed at the day’s last bar)'}
            </label>

            <button class="btn btn-primary btn-sm" disabled={busy || !form.rulesetId} onclick={run}>
              {#if busy}<span class="loading loading-spinner loading-xs"></span>{/if}
              Run backtest
            </button>
            {#if error}<div class="alert alert-error py-2 text-sm">{error}</div>{/if}
          </div>
        </div>
      </div>

      <!-- Results -->
      <div class="min-h-0 overflow-auto">
        {#if !report}
          <div class="card bg-base-100 shadow-sm">
            <div class="card-body text-sm text-base-content/60">
              Pick a signal and an instrument that has stored bars (record them live or backfill on the Charts page), then run.
              Stop-loss and target are checked on every bar; a bar that touches both counts as a stop-out. Charges use the rates in
              settings.json and are estimates.
            </div>
          </div>
        {:else}
          {@const s = report.summary}
          <div class="flex flex-col gap-3">
            <div class="text-xs text-base-content/60">
              {report.ruleset} · {report.instrument} · {report.interval} · {report.bars} bars, {report.sessions} session(s),
              {istDay(report.firstTs)} → {istDay(report.lastTs)} · lot size {report.lotSize}
            </div>

            {#each report.warnings as w}
              <div class="alert alert-warning py-2 text-sm">{w}</div>
            {/each}

            <div class="grid grid-cols-2 gap-2 md:grid-cols-4">
              {#each [
                { k: 'Net P&L', v: signed(s.netPnl), cls: trend(s.netPnl) },
                { k: 'Return', v: s.returnPct === null ? '—' : `${signed(s.returnPct)}%`, cls: trend(s.returnPct) },
                { k: 'Trades', v: String(s.trades), cls: '' },
                { k: 'Win rate', v: s.trades ? `${num(s.winRate)}%` : '—', cls: '' },
                { k: 'Gross P&L', v: signed(s.grossPnl), cls: trend(s.grossPnl) },
                { k: 'Charges', v: num(s.charges), cls: '' },
                { k: 'Profit factor', v: s.profitFactor === null ? (s.wins ? '∞' : '—') : num(s.profitFactor), cls: '' },
                { k: 'Expectancy / trade', v: signed(s.expectancy), cls: trend(s.expectancy) },
                { k: 'Avg win', v: signed(s.avgWin), cls: 'up' },
                { k: 'Avg loss', v: signed(s.avgLoss), cls: 'down' },
                { k: 'Best / worst', v: `${signed(s.best)} / ${signed(s.worst)}`, cls: '' },
                { k: 'Max drawdown', v: `${num(s.maxDrawdown)}${s.maxDrawdownPct === null ? '' : ` (${pct(s.maxDrawdownPct)})`}`, cls: s.maxDrawdown > 0 ? 'down' : '' },
                { k: 'Max losing streak', v: String(s.maxConsecutiveLosses), cls: '' },
                { k: 'Avg bars held', v: s.trades ? num(s.avgBars, 1) : '—', cls: '' },
                { k: 'Skipped (no capital)', v: String(s.skipped), cls: '' }
              ] as m}
                <div class="rounded-box bg-base-100 px-3 py-2 shadow-sm">
                  <div class="text-xs text-base-content/60">{m.k}</div>
                  <div class="num text-sm font-semibold {m.cls}">{m.v}</div>
                </div>
              {/each}
            </div>

            {#if curve}
              <div class="card bg-base-100 shadow-sm">
                <div class="card-body gap-1 p-3">
                  <div class="flex items-baseline justify-between text-xs text-base-content/60">
                    <span>Equity after each closed trade{report.summary.returnPct === null ? ' (P&L from 0)' : ''}</span>
                    <span class="num">{num(curve.lo)} – {num(curve.hi)}</span>
                  </div>
                  <svg viewBox="0 0 {W} {H}" class="h-40 w-full" preserveAspectRatio="none" role="img" aria-label="Equity curve">
                    <line x1="0" x2={W} y1={curve.baseY} y2={curve.baseY} stroke="currentColor" stroke-opacity="0.25" stroke-dasharray="4 4" />
                    <polyline points={curve.points} fill="none" stroke="currentColor" stroke-width="2" vector-effect="non-scaling-stroke" class={s.netPnl >= 0 ? 'text-success' : 'text-error'} />
                  </svg>
                </div>
              </div>
            {/if}

            <div class="card bg-base-100 shadow-sm">
              <div class="max-h-[420px] overflow-auto">
                <table class="table table-xs">
                  <thead class="sticky top-0 bg-base-100">
                    <tr>
                      <th>#</th><th>Entry (IST)</th><th class="text-right">Price</th><th>Exit (IST)</th><th class="text-right">Price</th>
                      <th class="text-right">Lots</th><th class="text-right">Gross</th><th class="text-right">Charges</th>
                      <th class="text-right">Net</th><th class="text-right">Ret %</th><th class="text-right">Bars</th><th>Exit by</th>
                    </tr>
                  </thead>
                  <tbody>
                    {#each report.trades as t, i}
                      <tr>
                        <td>{i + 1}</td>
                        <td class="num">{ist(t.entryTs)}</td>
                        <td class="num text-right">{px(t.entryPrice)}</td>
                        <td class="num">{ist(t.exitTs)}</td>
                        <td class="num text-right">{px(t.exitPrice)}</td>
                        <td class="num text-right">{t.lots}</td>
                        <td class="num text-right {trend(t.gross)}">{signed(t.gross)}</td>
                        <td class="num text-right">{num(t.charges)}</td>
                        <td class="num text-right font-semibold {trend(t.net)}">{signed(t.net)}</td>
                        <td class="num text-right {trend(t.returnPct)}">{signed(t.returnPct)}</td>
                        <td class="num text-right">{t.bars}</td>
                        <td>{REASONS[t.reason] ?? t.reason}</td>
                      </tr>
                    {:else}
                      <tr><td colspan="12" class="py-6 text-center text-base-content/50">The signal produced no trades in this range.</td></tr>
                    {/each}
                  </tbody>
                </table>
              </div>
            </div>
          </div>
        {/if}
      </div>
    </div>
  {/if}
</div>
