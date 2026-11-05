<script lang="ts">
  import type { InstrumentList } from '$lib/types';
  import { indexKey, optionKey, parseInstrument } from '$lib/charts';

  let {
    value,
    list,
    onpick
  }: { value: string | null; list: InstrumentList; onpick: (key: string) => void } = $props();

  const DEFAULT_INDICES = ['NIFTY 50', 'NIFTY BANK', 'INDIA VIX'];
  const DEFAULT_SYMBOLS = ['NIFTY', 'BANKNIFTY', 'FINNIFTY', 'MIDCPNIFTY'];

  const parsed = $derived(parseInstrument(value));
  let mode = $state<'index' | 'option'>('index');
  let indexName = $state('NIFTY 50');
  let symbol = $state('NIFTY');
  let expiry = $state('');
  let strike = $state('');
  let leg = $state<'CE' | 'PE'>('CE');

  // Follow the panel's current instrument.
  $effect(() => {
    const p = parsed;
    if (!p) return;
    mode = p.type;
    if (p.type === 'index') indexName = p.name;
    else {
      symbol = p.symbol;
      expiry = p.expiry;
      strike = p.strike;
      leg = p.leg;
    }
  });

  const indices = $derived([...new Set([...DEFAULT_INDICES, ...list.indices])]);
  const symbols = $derived([...new Set([...DEFAULT_SYMBOLS, ...list.options.map((o) => o.symbol)])]);
  const expiries = $derived(list.options.filter((o) => o.symbol === symbol).map((o) => o.expiry));
  const strikes = $derived(
    list.options.find((o) => o.symbol === symbol && o.expiry === expiry)?.strikes ?? []
  );
  const optionReady = $derived(!!expiry && Number.isFinite(Number(strike)) && strike !== '');

  const uid = $props.id();
</script>

<div class="flex flex-wrap items-center gap-1">
  <div class="join">
    <button class="btn join-item btn-xs" class:btn-primary={mode === 'index'} onclick={() => (mode = 'index')}>Index</button>
    <button class="btn join-item btn-xs" class:btn-primary={mode === 'option'} onclick={() => (mode = 'option')}>Option</button>
  </div>

  {#if mode === 'index'}
    <select class="select select-xs w-40" bind:value={indexName} onchange={() => onpick(indexKey(indexName))}>
      {#each indices as n}<option value={n}>{n}</option>{/each}
    </select>
  {:else}
    <select class="select select-xs w-28" bind:value={symbol}>
      {#each symbols as s}<option value={s}>{s}</option>{/each}
    </select>
    <input class="input input-xs w-28" placeholder="YYYY-MM-DD" list="{uid}-exp" bind:value={expiry} />
    <datalist id="{uid}-exp">{#each expiries as e}<option value={e}></option>{/each}</datalist>
    <input class="input input-xs w-20" type="number" placeholder="Strike" list="{uid}-str" bind:value={strike} />
    <datalist id="{uid}-str">{#each strikes as s}<option value={s}></option>{/each}</datalist>
    <div class="join">
      <button class="btn join-item btn-xs" class:btn-success={leg === 'CE'} onclick={() => (leg = 'CE')}>CE</button>
      <button class="btn join-item btn-xs" class:btn-error={leg === 'PE'} onclick={() => (leg = 'PE')}>PE</button>
    </div>
    <button class="btn btn-xs btn-primary" disabled={!optionReady} onclick={() => onpick(optionKey(symbol, expiry, strike, leg))}>Go</button>
  {/if}
</div>
