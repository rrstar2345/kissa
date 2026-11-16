<script lang="ts">
  import { onMount } from 'svelte';
  import { api } from '$lib/api';
  import { every } from '$lib/timers';
  import type { BackupResult, FyersUsage, TestResult } from '$lib/types';

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
  // Saved sections that are only read at startup (they apply after a restart)
  let restartPending = $state<string[]>([]);

  const dirty = $derived(json !== original);

  // Fyers call budget (counts since local midnight IST; saved across restarts)
  let usage = $state<FyersUsage | null>(null);
  async function loadUsage() {
    usage = await api.fyersUsage().catch(() => usage);
  }
  onMount(() => {
    loadUsage();
    const t = every(loadUsage, 10_000);
    return () => clearInterval(t);
  });
  const pct = (used: number, budget: number) => (budget > 0 ? Math.min(100, Math.round((used / budget) * 100)) : 0);

  // database backup
  let backingUp = $state(false);
  let backupMsg = $state<TestResult | null>(null);
  let backupInfo = $state<BackupResult | null>(null);
  const mb = (b: number) => (b / 1048576).toFixed(1) + ' MB';

  async function backup() {
    backingUp = true;
    backupMsg = null;
    backupInfo = null;
    try {
      const r = await api.backupDatabases();
      if (r) {
        backupInfo = r;
        backupMsg = { ok: true, message: `Backup saved to ${r.folder}` };
      }
    } catch (e) {
      backupMsg = { ok: false, message: String(e) };
    } finally {
      backingUp = false;
    }
  }

  async function load() {
    try {
      const s = await api.getServerSettings();
      url = s.url;
      port = s.port;
      keySet = s.apiKeySet;
      json = original = await api.getSettingsJson();
      restartPending = await api.getRestartPending().catch(() => restartPending);
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
      jsonMsg = {
        ok: true,
        message: restartPending.length
          ? 'Saved and applied. Some sections need an app restart (see below).'
          : 'Saved and applied.'
      };
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

  {#if usage}
    <section class="flex flex-col gap-3 rounded-box border border-base-300 bg-base-100 p-5">
      <div>
        <h2 class="font-semibold">Fyers API usage</h2>
        <p class="text-sm text-base-content/60">
          Every Fyers call is counted and throttled to {Math.round(usage.headroom * 100)}% of the {usage.plan} plan limits
          (<code>fyers.plan</code> and <code>fyers.limits</code> in settings.json). Exceeding the per-minute limit more than
          3 times in a day blocks the account for the rest of the day.
        </p>
      </div>
      {#each usage.categories as c (c.category)}
        <div class="flex flex-col gap-1.5">
          <div class="flex items-baseline justify-between text-sm">
            <span class="font-medium">{c.label}</span>
            {#if c.paused}<span class="badge badge-sm badge-warning">{c.paused}</span>{/if}
          </div>
          <div class="grid grid-cols-[5rem_1fr_9rem] items-center gap-x-3 gap-y-1 text-xs">
            <span class="text-base-content/60">Today</span>
            <progress class="progress {pct(c.usedDay, c.budgetDay) >= 85 ? 'progress-warning' : 'progress-primary'} w-full" value={c.usedDay} max={c.budgetDay}></progress>
            <span class="num text-right">{c.usedDay.toLocaleString()} / {c.budgetDay.toLocaleString()}</span>
            <span class="text-base-content/60">Last minute</span>
            <progress class="progress progress-primary w-full" value={c.usedMin} max={c.budgetMin}></progress>
            <span class="num text-right">{c.usedMin} / {c.budgetMin}</span>
            <span class="text-base-content/60">Last second</span>
            <progress class="progress progress-primary w-full" value={c.usedSec} max={c.budgetSec}></progress>
            <span class="num text-right">{c.usedSec} / {c.budgetSec}</span>
          </div>
          <p class="text-xs text-base-content/50">
            Plan limit {c.planDay.toLocaleString()} a day.{c.breachesToday > 0 ? ` Rate-limit errors today: ${c.breachesToday}.` : ''}
          </p>
        </div>
      {/each}
      {#each usage.hints as h}
        <div role="status" class="alert alert-warning alert-soft py-2 text-sm break-words">{h}</div>
      {/each}
    </section>
  {/if}

  <section class="flex flex-col gap-3 rounded-box border border-base-300 bg-base-100 p-5">
    <div>
      <h2 class="font-semibold">Backup</h2>
      <p class="text-sm text-base-content/60">
        Saves orders, trades, accounts, settings and chart bars into a new folder you choose. Passwords, API keys and
        tokens are kept in the OS keychain and are <strong>not</strong> included; you will need to enter them again on a new machine.
      </p>
    </div>
    {#if backupMsg}
      <div role="status" class="alert {backupMsg.ok ? 'alert-success' : 'alert-error'} alert-soft py-2 text-sm break-words">
        {backupMsg.message}
        {#if backupInfo}({backupInfo.files.map((f) => `${f.name} ${mb(f.bytes)}`).join(', ')}){/if}
      </div>
    {/if}
    <div>
      <button class="btn btn-primary btn-sm" onclick={backup} disabled={backingUp}>
        {#if backingUp}<span class="loading loading-spinner loading-xs"></span>{/if} Back up databases…
      </button>
    </div>
  </section>

  <section class="flex flex-col gap-3 rounded-box border border-base-300 bg-base-100 p-5">
    <div>
      <h2 class="font-semibold">settings.json</h2>
      <p class="text-sm text-base-content/60">
        Advanced options (data endpoints, charges, lot sizes, …). Server, trading (charges, paper broker, exit buffer, lot sizes, brokerage, instrument list) and risk_free_rate apply immediately; an open option chain picks up risk_free_rate when it is reopened. System, market_runtime, fyers, charts and trading.request_timeout_secs need an app restart.
      </p>
    </div>
    {#if restartPending.length}
      <div role="status" class="alert alert-warning alert-soft py-2 text-sm break-words">
        Saved but not applied until you restart the app: {restartPending.join(', ')}.
      </div>
    {/if}
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
