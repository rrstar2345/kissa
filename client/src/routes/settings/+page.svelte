<script lang="ts">
  import { onMount } from 'svelte';
  import { api } from '$lib/api';
  import type { TestResult } from '$lib/types';

  // server connection
  let url = $state('');
  let port = $state(0);
  let apiKey = $state('');
  let keySet = $state(false);
  let savingServer = $state(false);
  let testing = $state(false);
  let serverMsg = $state<TestResult | null>(null);

  // settings.json editor
  let json = $state('');
  let original = $state('');
  let savingJson = $state(false);
  let jsonMsg = $state<TestResult | null>(null);
  let loadError = $state('');

  const dirty = $derived(json !== original);

  async function load() {
    try {
      const s = await api.getServerSettings();
      url = s.url;
      port = s.port;
      keySet = s.apiKeySet;
      json = original = await api.getSettingsJson();
      loadError = '';
    } catch (e) {
      loadError = String(e);
    }
  }
  onMount(load);

  async function saveServer() {
    savingServer = true;
    serverMsg = null;
    try {
      const s = await api.saveServerSettings(url.trim(), Number(port) || 0, apiKey.trim() || undefined);
      keySet = s.apiKeySet;
      apiKey = '';
      json = original = await api.getSettingsJson();
      serverMsg = { ok: true, message: 'Saved. Use “Test connection” to verify the key.' };
    } catch (e) {
      serverMsg = { ok: false, message: String(e) };
    } finally {
      savingServer = false;
    }
  }

  async function testServer() {
    testing = true;
    serverMsg = null;
    serverMsg = await api.testServer().catch((e) => ({ ok: false, message: String(e) }));
    testing = false;
  }

  function format() {
    try {
      json = JSON.stringify(JSON.parse(json), null, 2);
      jsonMsg = null;
    } catch (e) {
      jsonMsg = { ok: false, message: `Not valid JSON: ${e}` };
    }
  }

  async function saveJson() {
    savingJson = true;
    jsonMsg = null;
    try {
      await api.saveSettingsJson(json);
      await load();
      jsonMsg = { ok: true, message: 'Saved. Server changes apply now; restart the app for everything else.' };
    } catch (e) {
      jsonMsg = { ok: false, message: String(e) };
    } finally {
      savingJson = false;
    }
  }
</script>

<div class="flex max-w-3xl flex-col gap-6">
  {#if loadError}<div role="alert" class="alert alert-error alert-soft py-2 text-sm break-words">{loadError}</div>{/if}

  <section class="flex flex-col gap-3 rounded-box border border-base-300 bg-base-100 p-5">
    <div>
      <h2 class="font-semibold">Server connection</h2>
      <p class="text-sm text-base-content/60">
        Orders, including paper orders, are only accepted while this server authenticates your API key.
        The URL and port go in settings.json; the API key is kept in the OS keychain.
      </p>
    </div>
    <div class="grid grid-cols-[1fr_8rem] gap-3">
      <label class="flex flex-col gap-1">
        <span class="text-xs text-base-content/60">Server URL</span>
        <input class="input input-sm w-full" bind:value={url} placeholder="https://proxy.example.com" />
      </label>
      <label class="flex flex-col gap-1">
        <span class="text-xs text-base-content/60">Port</span>
        <input class="input input-sm num w-full" type="number" min="0" max="65535" bind:value={port} placeholder="8443" />
      </label>
    </div>
    <label class="flex flex-col gap-1">
      <span class="text-xs text-base-content/60">API key {keySet ? '(saved; leave blank to keep)' : ''}</span>
      <input class="input input-sm w-full" type="password" autocomplete="off" bind:value={apiKey}
        placeholder={keySet ? '••••••••••••' : 'Paste the API key'} />
    </label>
    {#if serverMsg}
      <div role="status" class="alert {serverMsg.ok ? 'alert-success' : 'alert-error'} alert-soft py-2 text-sm break-words">{serverMsg.message}</div>
    {/if}
    <div class="flex gap-2">
      <button class="btn btn-primary btn-sm" onclick={saveServer} disabled={savingServer}>
        {#if savingServer}<span class="loading loading-spinner loading-xs"></span>{/if} Save
      </button>
      <button class="btn btn-outline btn-sm" onclick={testServer} disabled={testing || !keySet || !url}>
        {#if testing}<span class="loading loading-spinner loading-xs"></span>{/if} Test connection
      </button>
    </div>
  </section>

  <section class="flex flex-col gap-3 rounded-box border border-base-300 bg-base-100 p-5">
    <div>
      <h2 class="font-semibold">settings.json</h2>
      <p class="text-sm text-base-content/60">
        Advanced options (data endpoints, charges, lot sizes, …). The server section applies immediately; other changes need an app restart.
      </p>
    </div>
    <textarea class="textarea h-96 w-full font-mono text-xs" spellcheck="false" bind:value={json}></textarea>
    {#if jsonMsg}
      <div role="status" class="alert {jsonMsg.ok ? 'alert-success' : 'alert-error'} alert-soft py-2 text-sm break-words">{jsonMsg.message}</div>
    {/if}
    <div class="flex gap-2">
      <button class="btn btn-primary btn-sm" onclick={saveJson} disabled={savingJson || !dirty}>
        {#if savingJson}<span class="loading loading-spinner loading-xs"></span>{/if} Save settings.json
      </button>
      <button class="btn btn-ghost btn-sm" onclick={format}>Format</button>
      <button class="btn btn-ghost btn-sm" onclick={() => { json = original; jsonMsg = null; }} disabled={!dirty}>Revert</button>
    </div>
  </section>
</div>
