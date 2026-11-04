<script lang="ts">
  let { title, phase, items }: { title: string; phase: string; items: string[] } = $props();
</script>

<div class="mx-auto mt-10 max-w-xl">
  <div class="card border border-base-300 bg-base-100">
    <div class="card-body gap-3">
      <div class="flex items-center gap-3">
        <h1 class="card-title text-2xl">{title}</h1>
        <span class="badge badge-outline badge-primary">{phase}</span>
      </div>
      <p class="text-base-content/70">Planned for this view:</p>
      <ul class="list-inside list-disc space-y-1 text-sm text-base-content/80">
        {#each items as it}<li>{it}</li>{/each}
      </ul>
    </div>
  </div>
</div>
