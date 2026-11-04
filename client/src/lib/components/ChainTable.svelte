<script lang="ts">
  import { tick } from 'svelte';
  import { COLUMNS } from '$lib/columns';
  import { num } from '$lib/format';
  import type { ChainSnapshot, Leg } from '$lib/types';

  let {
    snapshot,
    visible,
    window: strikeWindow
  }: { snapshot: ChainSnapshot; visible: string[]; window: number } = $props();

  // Call side reads outer → inner (LTP next to the strike); put side mirrors it.
  const cols = $derived(COLUMNS.filter((c) => visible.includes(c.id)));
  const putCols = $derived([...cols].reverse());

  const rows = $derived.by(() => {
    const all = snapshot.rows;
    if (!strikeWindow || snapshot.atm === null) return all;
    const i = all.findIndex((r) => r.strike === snapshot.atm);
    if (i < 0) return all;
    return all.slice(Math.max(0, i - strikeWindow), i + strikeWindow + 1);
  });

  const maxOi = $derived({
    ce: Math.max(1, ...rows.map((r) => r.ce.oi ?? 0)),
    pe: Math.max(1, ...rows.map((r) => r.pe.oi ?? 0))
  });

  // LTP flash on change
  let prev = new Map<string, number>();
  let flashes = $state<Record<string, 'up' | 'down'>>({});
  $effect(() => {
    const next: Record<string, 'up' | 'down'> = {};
    for (const r of snapshot.rows) {
      for (const side of ['ce', 'pe'] as const) {
        const k = `${r.strike}:${side}`;
        const v = r[side].ltp;
        const p = prev.get(k);
        if (v !== null && p !== undefined && v !== p) next[k] = v > p ? 'up' : 'down';
        if (v !== null) prev.set(k, v);
      }
    }
    if (Object.keys(next).length) {
      flashes = next;
      setTimeout(() => (flashes = {}), 700);
    }
  });

  // Reset flash baseline when the instrument changes.
  let lastKey = '';
  $effect(() => {
    const k = `${snapshot.source}|${snapshot.symbol}|${snapshot.expiry}`;
    if (k !== lastKey) {
      prev = new Map();
      lastKey = k;
      scrollToAtm();
    }
  });

  let wrap: HTMLDivElement | undefined = $state();
  async function scrollToAtm() {
    await tick();
    wrap?.querySelector('tr.atm')?.scrollIntoView({ block: 'center' });
  }

  function cell(c: (typeof COLUMNS)[number], leg: Leg, side: 'ce' | 'pe', strike: number) {
    const v = c.value(leg);
    let cls = 'num';
    let style = '';
    if (c.kind === 'trend') cls += v ? (v > 0 ? ' up' : ' down') : '';
    if (c.kind === 'ltp') {
      cls += ' font-semibold' + (leg.change ? (leg.change > 0 ? ' up' : ' down') : '');
      const f = flashes[`${strike}:${side}`];
      if (f) cls += f === 'up' ? ' flash-up' : ' flash-down';
    }
    if (c.kind === 'oi') {
      cls += ` oi-cell oi-${side}`;
      style = `--bar:${Math.round(((v ?? 0) / maxOi[side]) * 100)}%`;
    }
    return { text: c.fmt(v), cls, style };
  }

  const itm = (side: 'ce' | 'pe', strike: number) =>
    snapshot.atm !== null && (side === 'ce' ? strike < snapshot.atm : strike > snapshot.atm);
</script>

<div class="chain-wrap max-h-[calc(100vh-15rem)]" bind:this={wrap}>
  <table class="chain">
    <thead>
      <tr>
        <th colspan={cols.length} class="text-error!">CALLS</th>
        <th></th>
        <th colspan={cols.length} class="text-success!">PUTS</th>
      </tr>
      <tr>
        {#each cols as c}<th>{c.label}</th>{/each}
        <th class="text-center!">Strike</th>
        {#each putCols as c}<th>{c.label}</th>{/each}
      </tr>
    </thead>
    <tbody>
      {#each rows as r (r.strike)}
        <tr class:atm={r.strike === snapshot.atm}>
          {#each cols as c}
            {@const x = cell(c, r.ce, 'ce', r.strike)}
            <td class="{x.cls} {itm('ce', r.strike) ? 'itm' : ''}" style={x.style}>{x.text}</td>
          {/each}
          <td class="strike num">{num(r.strike, 0)}</td>
          {#each putCols as c}
            {@const x = cell(c, r.pe, 'pe', r.strike)}
            <td class="{x.cls} {itm('pe', r.strike) ? 'itm' : ''}" style={x.style}>{x.text}</td>
          {/each}
        </tr>
      {/each}
    </tbody>
  </table>
</div>
