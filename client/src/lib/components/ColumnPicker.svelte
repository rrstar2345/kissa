<script lang="ts">
  import { COLUMNS, DEFAULT_VISIBLE } from '$lib/columns';

  let { visible = $bindable(), onchange }: { visible: string[]; onchange: () => void } = $props();

  function toggle(id: string) {
    visible = visible.includes(id) ? visible.filter((x) => x !== id) : [...visible, id];
    onchange();
  }
  function reset() {
    visible = [...DEFAULT_VISIBLE];
    onchange();
  }
  const groups = ['Market', 'Greeks'] as const;
</script>

<div class="dropdown dropdown-end">
  <div tabindex="0" role="button" class="btn btn-sm btn-outline">Columns ({visible.length})</div>
  <div tabindex="-1" class="dropdown-content z-30 mt-1 w-56 rounded-box border border-base-300 bg-base-100 p-3 shadow-xl">
    {#each groups as g}
      <div class="mb-1 text-xs font-semibold uppercase tracking-wide text-base-content/50">{g}</div>
      <div class="mb-2 grid grid-cols-1 gap-1">
        {#each COLUMNS.filter((c) => c.group === g) as c}
          <label class="flex cursor-pointer items-center gap-2 text-sm">
            <input type="checkbox" class="checkbox checkbox-xs checkbox-primary" checked={visible.includes(c.id)} onchange={() => toggle(c.id)} />
            {c.label}
          </label>
        {/each}
      </div>
    {/each}
    <button class="btn btn-ghost btn-xs mt-1 w-full" onclick={reset}>Reset to default</button>
  </div>
</div>
