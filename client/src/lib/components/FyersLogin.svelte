<script lang="ts">
  import { onDestroy } from 'svelte';
  import { openUrl } from '@tauri-apps/plugin-opener';
  import { api } from '$lib/api';
  import type { Account } from '$lib/types';

  let { account, ondone, oncancel }: { account: Account; ondone: () => void; oncancel: () => void } = $props();

  // http://127.0.0.1:<port> redirect: the app can catch the redirect itself.
  const local = $derived(/^http:\/\/127\.0\.0\.1:\d+/i.test(String(account.meta.redirectUri ?? '')));
  let waiting = $state(false); // listening for the redirect
  let superseded = false; // the pasted code took over; ignore the listener's "cancelled" result
  let opened = $state(false);
  let pasted = $state('');
  let busy = $state(false);
  let error = $state('');

  onDestroy(() => {
    if (waiting) api.fyersLoginCancel(account.id).catch(() => {});
  });

  /** Step 1: open the Fyers login; in local mode also listen for the redirect. */
  async function startLogin() {
    error = '';
    superseded = false;
    if (!local) {
      try {
        await openUrl(await api.fyersLoginUrl(account.id));
        opened = true;
      } catch (e) {
        error = String(e);
      }
      return;
    }
    waiting = true;
    opened = true;
    try {
      await api.fyersLoginLocal(account.id);
      waiting = false;
      ondone();
    } catch (e) {
      waiting = false;
      if (!superseded) error = String(e);
    }
  }

  /** Use the pasted auth code / URL and stop listening. */
  async function usePasted() {
    busy = true;
    error = '';
    try {
      if (waiting) {
        superseded = true;
        await api.fyersLoginCancel(account.id);
      }
      await api.fyersLogin(account.id, pasted);
      ondone();
    } catch (e) {
      error = String(e);
    } finally {
      busy = false;
    }
  }

  function cancel() {
    if (waiting) api.fyersLoginCancel(account.id).catch(() => {});
    oncancel();
  }
</script>

<div class="flex flex-col gap-4">
  <h3 class="text-lg font-bold">Log in to Fyers · {account.name}</h3>

  <div class="flex flex-col gap-1 text-sm">
    <span class="font-medium">1. Log in at Fyers</span>
    <span class="text-xs text-base-content/60">
      {#if local}
        Opens your browser and listens on <span class="font-mono">{account.meta.redirectUri}</span> for the redirect.
      {:else}
        Opens your browser. After logging in, copy the URL you land on.
      {/if}
      Tokens expire daily, so repeat this each trading day.
    </span>
    <div class="flex items-center gap-2">
      <button type="button" class="btn btn-sm btn-outline mt-1" onclick={startLogin} disabled={waiting}>
        {#if waiting}<span class="loading loading-spinner loading-xs"></span> Waiting for redirect…{:else}{opened ? 'Open Fyers login again' : 'Open Fyers login'}{/if}
      </button>
    </div>
  </div>

  <div class="flex flex-col gap-1 text-sm">
    <span class="font-medium">{local ? '2. Or paste the auth code' : '2. Paste the URL you land on'}</span>
    <span class="text-xs text-base-content/60">
      {#if local}If the browser blocks the redirect, copy the auth_code (or the full address) from the address bar and use it here. This stops the listener.
      {:else}Copy the full address from the browser's address bar (the auth_code alone also works).{/if}
    </span>
    <div class="flex items-start gap-2">
      <textarea
        class="textarea textarea-sm h-20 min-w-0 flex-1 font-mono text-xs"
        bind:value={pasted}
        placeholder="auth_code, or https://…?s=ok&code=200&auth_code=…&state=…"
      ></textarea>
      <button type="button" class="btn btn-primary btn-sm" onclick={usePasted} disabled={busy || !pasted.trim()}>
        {#if busy}<span class="loading loading-spinner loading-xs"></span>{/if}
        Use this code
      </button>
    </div>
  </div>

  {#if error}<div class="alert alert-error alert-soft py-2 text-sm">{error}</div>{/if}

  <div class="modal-action mt-0">
    <button type="button" class="btn btn-ghost btn-sm" onclick={cancel}>Cancel</button>
  </div>
</div>
