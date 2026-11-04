<script lang="ts">
  import ComingSoon from '$lib/components/ComingSoon.svelte';
</script>

<ComingSoon
  title="Orders"
  phase="Phase 3"
  items={[
    'Place LIMIT / STOP_LOSS orders via the broker-proxy (Dhan) or paper account',
    'Live order updates over the proxy WebSocket',
    'Order log stored locally (the proxy keeps no state)'
  ]}
/>
