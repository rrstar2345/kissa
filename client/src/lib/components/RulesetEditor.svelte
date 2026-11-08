<script lang="ts">
  import type { CmpOp, RuleNode, RuleVar, Ruleset, VarKind } from '$lib/types';

  let { ruleset = $bindable(), errors = [] }: { ruleset: Ruleset; errors?: string[] } = $props();

  type Grp = { op: 'and' | 'or'; items: RuleNode[] };

  const SOURCES: [string, string][] = [
    ['open', 'Open'],
    ['high', 'High'],
    ['low', 'Low'],
    ['close', 'Close'],
    ['avg', 'Avg (OHLC/4)'],
    ['hl2', 'HL/2'],
    ['hlc3', 'HLC/3']
  ];
  const KINDS: [VarKind, string][] = [
    ['rsi', 'RSI'],
    ['ema', 'EMA'],
    ['sma', 'SMA'],
    ['macd', 'MACD'],
    ['bb', 'Bollinger'],
    ['atr', 'ATR'],
    ['vwap', 'VWAP'],
    ['supertrend', 'Supertrend'],
    ['formula', 'Formula (A op B)']
  ];
  const OPS: [CmpOp, string][] = [
    ['crosses_above', 'crosses above'],
    ['crosses_below', 'crosses below'],
    ['>', '>'],
    ['>=', '>='],
    ['<', '<'],
    ['<=', '<='],
    ['=', '='],
    ['<>', '<>']
  ];
  const PARAM_KEYS = ['length', 'src', 'fast', 'slow', 'signal', 'mult', 'output', 'left', 'op', 'right'] as const;

  function defaults(kind: VarKind): Partial<RuleVar> {
    switch (kind) {
      case 'rsi':
        return { length: 14, src: 'close' };
      case 'ema':
        return { length: 9, src: 'close' };
      case 'sma':
        return { length: 20, src: 'close' };
      case 'macd':
        return { fast: 12, slow: 26, signal: 9, output: 'line', src: 'close' };
      case 'bb':
        return { length: 20, mult: 2, output: 'mid', src: 'close' };
      case 'atr':
        return { length: 14 };
      case 'supertrend':
        return { length: 10, mult: 3, output: 'line' };
      case 'formula':
        return { left: 'close', op: '-', right: '0' };
      default:
        return {};
    }
  }

  let mode = $state<'form' | 'json'>('form');
  let jsonText = $state('');
  let jsonError = $state('');
  let addKind = $state<VarKind>('rsi');

  function addVar() {
    const base = addKind === 'formula' ? 'f' : addKind;
    let n = 1;
    while (ruleset.vars.some((v) => v.id === `${base}${n}`)) n++;
    ruleset.vars.push({ id: `${base}${n}`, kind: addKind, plot: true, ...defaults(addKind) });
  }

  function setKind(v: RuleVar, kind: VarKind) {
    for (const k of PARAM_KEYS) delete v[k];
    v.kind = kind;
    Object.assign(v, defaults(kind));
  }

  function moveVar(i: number, d: number) {
    const j = i + d;
    if (j < 0 || j >= ruleset.vars.length) return;
    const [x] = ruleset.vars.splice(i, 1);
    ruleset.vars.splice(j, 0, x);
  }

  /** Rename a variable and update everything that referred to it. */
  function rename(v: RuleVar, next: string) {
    const old = v.id;
    if (next === old) return;
    v.id = next;
    for (const x of ruleset.vars) {
      if (x.src === old) x.src = next;
      if (x.left === old) x.left = next;
      if (x.right === old) x.right = next;
    }
    const walk = (n: RuleNode) => {
      if (n.type === 'cond') {
        if (n.left === old) n.left = next;
        if (n.right === old) n.right = next;
      } else n.items.forEach(walk);
    };
    ruleset.entry.items.forEach(walk);
    ruleset.exit.items.forEach(walk);
  }

  function addCond(g: Grp) {
    g.items.push({ type: 'cond', left: 'close', op: '>', right: '0' });
  }
  function addGroup(g: Grp) {
    g.items.push({ type: 'group', op: g.op === 'and' ? 'or' : 'and', items: [] });
  }

  function normalize(raw: any): Ruleset {
    const grp = (g: any): Grp => ({ op: g?.op === 'or' ? 'or' : 'and', items: Array.isArray(g?.items) ? g.items : [] });
    return {
      id: ruleset.id,
      name: String(raw?.name ?? ''),
      vars: (Array.isArray(raw?.vars) ? raw.vars : []).map((v: any) => ({ ...v, plot: !!v.plot })),
      entry: grp(raw?.entry),
      exit: grp(raw?.exit)
    };
  }

  function applyJson(): boolean {
    try {
      ruleset = normalize(JSON.parse(jsonText));
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
      jsonText = JSON.stringify($state.snapshot(ruleset), null, 2);
      jsonError = '';
      mode = 'json';
    } else if (applyJson()) mode = 'form';
  }
</script>

{#snippet operand(o: any, key: string, cands: RuleVar[], allowNum: boolean)}
  {@const val = String(o[key] ?? '')}
  {@const isNum = allowNum && (val === '' || (val.trim() !== '' && Number.isFinite(Number(val))))}
  <div class="flex min-w-0 flex-1 gap-1">
    <select
      class="select select-xs min-w-0 flex-1"
      value={isNum ? '#num' : val}
      onchange={(e) => {
        const v = e.currentTarget.value;
        o[key] = v === '#num' ? '0' : v;
      }}
    >
      <optgroup label="Price">
        {#each SOURCES as [k, l]}<option value={k}>{l}</option>{/each}
      </optgroup>
      {#if cands.length}
        <optgroup label="Variables">
          {#each cands as c}<option value={c.id}>{c.id}</option>{/each}
        </optgroup>
      {/if}
      {#if allowNum}<option value="#num">Number…</option>{/if}
    </select>
    {#if isNum}
      <input type="number" step="any" class="input input-xs w-20" value={val} oninput={(e) => (o[key] = e.currentTarget.value)} />
    {/if}
  </div>
{/snippet}

{#snippet param(o: any, key: string, label: string)}
  <label class="flex items-center gap-1 text-xs">
    {label}
    <input
      type="number"
      step="any"
      class="input input-xs w-16"
      value={o[key] ?? ''}
      oninput={(e) => {
        const n = e.currentTarget.valueAsNumber;
        o[key] = Number.isFinite(n) ? n : undefined;
      }}
    />
  </label>
{/snippet}

{#snippet outputPick(v: RuleVar, options: string[])}
  <label class="flex items-center gap-1 text-xs">
    Output
    <select class="select select-xs w-24" value={v.output ?? options[0]} onchange={(e) => (v.output = e.currentTarget.value)}>
      {#each options as o}<option value={o}>{o}</option>{/each}
    </select>
  </label>
{/snippet}

{#snippet group(g: Grp, depth: number)}
  <div class="space-y-1.5">
    <select class="select select-xs w-36" value={g.op} onchange={(e) => (g.op = e.currentTarget.value as 'and' | 'or')}>
      <option value="and">All of (AND)</option>
      <option value="or">Any of (OR)</option>
    </select>
    {#each g.items as node, i}
      <div class="flex items-start gap-1.5">
        {#if node.type === 'cond'}
          <div class="flex min-w-0 flex-1 flex-wrap items-center gap-1">
            {@render operand(node, 'left', ruleset.vars, true)}
            <select class="select select-xs w-32 shrink-0" value={node.op} onchange={(e) => (node.op = e.currentTarget.value as CmpOp)}>
              {#each OPS as [k, l]}<option value={k}>{l}</option>{/each}
            </select>
            {@render operand(node, 'right', ruleset.vars, true)}
          </div>
        {:else}
          <div class="min-w-0 flex-1 border-l-2 border-primary/40 pl-2">
            {@render group(node, depth + 1)}
          </div>
        {/if}
        <button class="btn btn-ghost btn-xs" title="Remove" onclick={() => g.items.splice(i, 1)}>✕</button>
      </div>
    {/each}
    <div class="flex gap-1">
      <button class="btn btn-outline btn-xs" onclick={() => addCond(g)}>+ Condition</button>
      {#if depth < 3}
        <button class="btn btn-ghost btn-xs" onclick={() => addGroup(g)}>+ Group</button>
      {/if}
    </div>
  </div>
{/snippet}

<div class="space-y-3 pb-4">
  <div class="flex items-center gap-2">
    <input class="input input-sm flex-1 font-semibold" placeholder="Ruleset name" bind:value={ruleset.name} />
    <div class="join">
      <button class="btn join-item btn-xs" class:btn-primary={mode === 'form'} onclick={() => setMode('form')}>Form</button>
      <button class="btn join-item btn-xs" class:btn-primary={mode === 'json'} onclick={() => setMode('json')}>JSON</button>
    </div>
  </div>

  {#if mode === 'json'}
    <textarea class="textarea h-[60vh] w-full font-mono text-xs" spellcheck="false" bind:value={jsonText}></textarea>
    <div class="flex items-center gap-2">
      <button class="btn btn-primary btn-xs" onclick={applyJson}>Apply</button>
      <span class="text-xs text-base-content/60">Switching to Form also applies it.</span>
    </div>
    {#if jsonError}<p class="text-xs text-error">{jsonError}</p>{/if}
  {:else}
    <section class="rounded-box border border-base-300 bg-base-100 p-3">
      <h2 class="mb-2 text-sm font-semibold">Variables</h2>
      <p class="mb-2 text-xs text-base-content/60">
        A variable can use a price or any variable above it as its source (e.g. EMA of an RSI).
      </p>
      <div class="space-y-2">
        {#each ruleset.vars as v, i (i)}
          {@const earlier = ruleset.vars.slice(0, i)}
          <div class="space-y-1.5 rounded-field border border-base-300 p-2">
            <div class="flex items-center gap-1.5">
              <input
                class="input input-xs w-24 font-mono"
                value={v.id}
                onchange={(e) => rename(v, e.currentTarget.value.trim())}
              />
              <select class="select select-xs w-40" value={v.kind} onchange={(e) => setKind(v, e.currentTarget.value as VarKind)}>
                {#each KINDS as [k, l]}<option value={k}>{l}</option>{/each}
              </select>
              <label class="ml-auto flex items-center gap-1 text-xs" title="Draw on the chart">
                <input type="checkbox" class="checkbox checkbox-xs" bind:checked={v.plot} /> plot
              </label>
              <button class="btn btn-ghost btn-xs" title="Move up" disabled={i === 0} onclick={() => moveVar(i, -1)}>↑</button>
              <button class="btn btn-ghost btn-xs" title="Move down" disabled={i === ruleset.vars.length - 1} onclick={() => moveVar(i, 1)}>↓</button>
              <button class="btn btn-ghost btn-xs" title="Delete" onclick={() => ruleset.vars.splice(i, 1)}>✕</button>
            </div>
            <div class="flex flex-wrap items-center gap-2">
              {#if v.kind === 'sma' || v.kind === 'ema' || v.kind === 'rsi'}
                {@render param(v, 'length', 'Length')}
                <span class="text-xs">Source</span>
                {@render operand(v, 'src', earlier, false)}
              {:else if v.kind === 'macd'}
                {@render param(v, 'fast', 'Fast')}
                {@render param(v, 'slow', 'Slow')}
                {@render param(v, 'signal', 'Signal')}
                {@render outputPick(v, ['line', 'signal', 'hist'])}
                <span class="text-xs">Source</span>
                {@render operand(v, 'src', earlier, false)}
              {:else if v.kind === 'bb'}
                {@render param(v, 'length', 'Length')}
                {@render param(v, 'mult', 'Mult')}
                {@render outputPick(v, ['mid', 'upper', 'lower'])}
                <span class="text-xs">Source</span>
                {@render operand(v, 'src', earlier, false)}
              {:else if v.kind === 'atr'}
                {@render param(v, 'length', 'Length')}
              {:else if v.kind === 'supertrend'}
                {@render param(v, 'length', 'Length')}
                {@render param(v, 'mult', 'Mult')}
                {@render outputPick(v, ['line', 'dir'])}
              {:else if v.kind === 'vwap'}
                <span class="text-xs text-base-content/60">Session VWAP (resets each day)</span>
              {:else if v.kind === 'formula'}
                {@render operand(v, 'left', earlier, true)}
                <select class="select select-xs w-16" value={v.op ?? '-'} onchange={(e) => (v.op = e.currentTarget.value)}>
                  <option value="+">+</option>
                  <option value="-">−</option>
                  <option value="*">×</option>
                  <option value="/">÷</option>
                </select>
                {@render operand(v, 'right', earlier, true)}
              {/if}
            </div>
          </div>
        {/each}
      </div>
      <div class="mt-2 flex items-center gap-1.5">
        <select class="select select-xs w-40" bind:value={addKind}>
          {#each KINDS as [k, l]}<option value={k}>{l}</option>{/each}
        </select>
        <button class="btn btn-outline btn-xs" onclick={addVar}>+ Add variable</button>
      </div>
    </section>

    <section class="rounded-box border border-base-300 bg-base-100 p-3">
      <h2 class="mb-2 text-sm font-semibold">Entry: BUY when</h2>
      {@render group(ruleset.entry, 0)}
    </section>

    <section class="rounded-box border border-base-300 bg-base-100 p-3">
      <h2 class="mb-1 text-sm font-semibold">Exit: SELL when</h2>
      <p class="mb-2 text-xs text-base-content/60">
        Entry fires only while flat and Exit only after an Entry. With no Exit conditions, Entry fires on every bar it is true.
      </p>
      {@render group(ruleset.exit, 0)}
    </section>
  {/if}

  <div class="rounded-box border border-base-300 bg-base-100 p-3 text-xs">
    {#if errors.length}
      <ul class="space-y-0.5 text-error">
        {#each errors as e}<li>{e}</li>{/each}
      </ul>
    {:else}
      <span class="text-success">Ruleset is valid.</span>
    {/if}
  </div>
</div>
