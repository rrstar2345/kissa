<script lang="ts">
  import { onMount, untrack } from 'svelte';
  import { api } from '$lib/api';
  import { trading } from '$lib/trading.svelte';
  import type { AlertConfig, AlertRow, AlertSettings, TestResult } from '$lib/types';

  let settings = $state<AlertSettings | null>(null);
  let cfg = $state<AlertConfig | null>(null);
  // List fields are edited as text and split on save.
  let chatText = $state('');
  let toText = $state('');
  let tokenInput = $state('');
  let passInput = $state('');
  let saved = $state('');
  let saving = $state(false);
  let msg = $state<TestResult | null>(null);
  let loadError = $state('');
  let testing = $state<Record<string, boolean>>({});
  let testMsg = $state<Record<string, TestResult | null>>({});

  let rows = $state<AlertRow[]>([]);
  let hideQuiet = $state(false);

  const split = (t: string) =>
    t
      .split(/[\n,;]+/)
      .map((s) => s.trim())
      .filter(Boolean);
  const snap = () => JSON.stringify([cfg, chatText, toText, tokenInput, passInput]);
  const dirty = $derived(cfg !== null && snap() !== saved);

  function apply(s: AlertSettings) {
    settings = s;
    cfg = structuredClone($state.snapshot(s.config));
    chatText = s.config.telegram.chatIds.join('\n');
    toText = s.config.email.to.join(', ');
    tokenInput = '';
    passInput = '';
    saved = snap();
  }

  async function load() {
    try {
      apply(await api.getAlertSettings());
      loadError = '';
    } catch (e) {
      loadError = String(e);
    }
  }

  async function loadHistory() {
    try {
      rows = await api.listAlertHistory(200);
    } catch (e) {
      loadError = String(e);
    }
  }

  onMount(() => {
    load();
    loadHistory();
  });
  $effect(() => {
    trading.alertVersion;
    untrack(loadHistory);
  });

  const isOn = (kind: string) => cfg?.events[kind] !== false;

  async function save() {
    if (!cfg) return;
    saving = true;
    msg = null;
    try {
      const c: AlertConfig = $state.snapshot(cfg);
      c.telegram.chatIds = split(chatText);
      c.email.to = split(toText);
      c.email.port = Math.max(0, Math.floor(Number(c.email.port) || 0));
      c.dedupeSecs = Math.max(0, Math.floor(Number(c.dedupeSecs) || 0));
      c.tokenWarnHours = Math.max(0, Math.floor(Number(c.tokenWarnHours) || 0));
      c.fyersTokenHours = Math.max(0, Math.floor(Number(c.fyersTokenHours) || 0));
      c.dhanTokenHours = Math.max(0, Math.floor(Number(c.dhanTokenHours) || 0));
      apply(await api.saveAlertSettings(c, tokenInput.trim() || undefined, passInput.trim() || undefined));
      msg = { ok: true, message: 'Saved.' };
    } catch (e) {
      msg = { ok: false, message: String(e) };
    } finally {
      saving = false;
    }
  }

  async function test(channel: 'telegram' | 'email') {
    testing[channel] = true;
    testMsg[channel] = null;
    testMsg[channel] = await api.testAlert(channel).catch((e) => ({ ok: false, message: String(e) }));
    testing[channel] = false;
  }

  async function clearHistory() {
    try {
      await api.clearAlertHistory();
      await loadHistory();
    } catch (e) {
      loadError = String(e);
    }
  }

  const kindLabel = (kind: string) =>
    kind === 'test' ? 'Test' : (settings?.events.find((e) => e.kind === kind)?.label ?? kind);

  function statusClass(s: string): string {
    switch (s) {
      case 'sent':
        return 'badge-success';
      case 'partial':
        return 'badge-warning';
      case 'failed':
        return 'badge-error';
      case 'sending':
        return 'badge-info';
      default:
        return 'badge-ghost';
    }
  }

  /** `DD-MM HH:MM:SS` in IST from an RFC 3339 timestamp. */
  function stamp(at: string): string {
    const d = new Date(at);
    if (Number.isNaN(d.getTime())) return '—';
    const iso = new Date(d.getTime() + 5.5 * 3600_000).toISOString();
    return `${iso.slice(8, 10)}-${iso.slice(5, 7)} ${iso.slice(11, 19)}`;
  }

  const shown = $derived(hideQuiet ? rows.filter((r) => r.status !== 'suppressed' && r.status !== 'skipped') : rows);
</script>

<div class="flex max-w-4xl flex-col gap-5">
  <div>
    <h1 class="text-lg font-semibold">Alerts</h1>
    <p class="text-sm text-base-content/60">
      Get a Telegram message and/or an email when something happens. The bot token and SMTP password are kept in the
      OS keychain. Alerts are sent only while the app is running.
    </p>
  </div>

  {#if loadError}<div role="alert" class="alert alert-error alert-soft py-2 text-sm break-words">{loadError}</div>{/if}

  {#if cfg && settings}
    <section class="flex flex-col gap-3 rounded-box border border-base-300 bg-base-100 p-5">
      <div class="flex items-center gap-3">
        <h2 class="font-semibold">Telegram</h2>
        <label class="ml-auto flex cursor-pointer items-center gap-2 text-sm">
          <input type="checkbox" class="toggle toggle-sm toggle-primary" bind:checked={cfg.telegram.enabled} />
          Enabled
        </label>
      </div>
      <p class="text-xs text-base-content/60">
        Create a bot with @BotFather and paste its token. Send your bot a message, then open
        <span class="num">api.telegram.org/bot&lt;token&gt;/getUpdates</span> in a browser to read your chat ID.
      </p>
      <label class="flex flex-col gap-1">
        <span class="text-xs text-base-content/60">Bot token {settings.telegramTokenSet ? '(saved; leave blank to keep)' : ''}</span>
        <input class="input input-sm w-full" type="password" autocomplete="off" bind:value={tokenInput}
          placeholder={settings.telegramTokenSet ? '••••••••••••' : '123456:ABC-DEF…'} />
      </label>
      <label class="flex flex-col gap-1">
        <span class="text-xs text-base-content/60">Chat IDs (one per line, or comma-separated; groups are negative numbers)</span>
        <textarea class="textarea textarea-sm num w-full" rows="2" bind:value={chatText} placeholder="123456789"></textarea>
      </label>
      {#if testMsg.telegram}
        <div role="status" class="alert {testMsg.telegram.ok ? 'alert-success' : 'alert-error'} alert-soft py-2 text-sm break-words">{testMsg.telegram.message}</div>
      {/if}
      <div class="flex items-center gap-2">
        <button class="btn btn-outline btn-sm" onclick={() => test('telegram')} disabled={testing.telegram || dirty || !settings.telegramTokenSet}>
          {#if testing.telegram}<span class="loading loading-spinner loading-xs"></span>{/if} Send test
        </button>
        {#if dirty}<span class="text-xs text-base-content/60">Save first: the test uses the saved settings.</span>{/if}
      </div>
    </section>

    <section class="flex flex-col gap-3 rounded-box border border-base-300 bg-base-100 p-5">
      <div class="flex items-center gap-3">
        <h2 class="font-semibold">Email (SMTP)</h2>
        <label class="ml-auto flex cursor-pointer items-center gap-2 text-sm">
          <input type="checkbox" class="toggle toggle-sm toggle-primary" bind:checked={cfg.email.enabled} />
          Enabled
        </label>
      </div>
      <div class="grid grid-cols-1 gap-3 sm:grid-cols-[1fr_7rem_10rem]">
        <label class="flex flex-col gap-1">
          <span class="text-xs text-base-content/60">SMTP host</span>
          <input class="input input-sm w-full" bind:value={cfg.email.host} placeholder="smtp.gmail.com" />
        </label>
        <label class="flex flex-col gap-1">
          <span class="text-xs text-base-content/60">Port</span>
          <input class="input input-sm num w-full" type="number" min="1" max="65535" bind:value={cfg.email.port} />
        </label>
        <label class="flex flex-col gap-1">
          <span class="text-xs text-base-content/60">Security</span>
          <select class="select select-sm w-full" bind:value={cfg.email.security}>
            <option value="starttls">STARTTLS (587)</option>
            <option value="tls">TLS (465)</option>
            <option value="none">None (localhost only)</option>
          </select>
        </label>
      </div>
      <div class="grid grid-cols-1 gap-3 sm:grid-cols-2">
        <label class="flex flex-col gap-1">
          <span class="text-xs text-base-content/60">Username (blank = no login)</span>
          <input class="input input-sm w-full" autocomplete="off" bind:value={cfg.email.username} />
        </label>
        <label class="flex flex-col gap-1">
          <span class="text-xs text-base-content/60">Password {settings.smtpPasswordSet ? '(saved; leave blank to keep)' : ''}</span>
          <input class="input input-sm w-full" type="password" autocomplete="off" bind:value={passInput}
            placeholder={settings.smtpPasswordSet ? '••••••••••••' : ''} />
        </label>
        <label class="flex flex-col gap-1">
          <span class="text-xs text-base-content/60">From</span>
          <input class="input input-sm w-full" bind:value={cfg.email.from} placeholder="KISSA <you@example.com>" />
        </label>
        <label class="flex flex-col gap-1">
          <span class="text-xs text-base-content/60">To (comma-separated)</span>
          <input class="input input-sm w-full" bind:value={toText} placeholder="you@example.com" />
        </label>
      </div>
      {#if testMsg.email}
        <div role="status" class="alert {testMsg.email.ok ? 'alert-success' : 'alert-error'} alert-soft py-2 text-sm break-words">{testMsg.email.message}</div>
      {/if}
      <div class="flex items-center gap-2">
        <button class="btn btn-outline btn-sm" onclick={() => test('email')} disabled={testing.email || dirty || !cfg.email.host}>
          {#if testing.email}<span class="loading loading-spinner loading-xs"></span>{/if} Send test
        </button>
        {#if dirty}<span class="text-xs text-base-content/60">Save first: the test uses the saved settings.</span>{/if}
      </div>
    </section>

    <section class="flex flex-col gap-4 rounded-box border border-base-300 bg-base-100 p-5">
      <h2 class="font-semibold">What to send</h2>
      <ul class="flex flex-col gap-3">
        {#each settings.events as e (e.kind)}
          <li>
            <label class="flex cursor-pointer items-start gap-3">
              <input type="checkbox" class="toggle toggle-sm toggle-primary mt-0.5" checked={isOn(e.kind)}
                onchange={(ev) => { if (cfg) cfg.events[e.kind] = ev.currentTarget.checked; }} />
              <span class="flex flex-col">
                <span class="text-sm font-medium">{e.label}</span>
                <span class="text-xs text-base-content/60">{e.description}</span>
              </span>
            </label>
          </li>
        {/each}
      </ul>

      <div class="grid grid-cols-1 gap-3 sm:grid-cols-3">
        <label class="flex flex-col gap-1">
          <span class="text-xs text-base-content/60">De-duplicate repeats within (seconds, 0 = off)</span>
          <input class="input input-sm num w-full" type="number" min="0" max="3600" step="10" bind:value={cfg.dedupeSecs} />
        </label>
        <label class="flex flex-col gap-1">
          <span class="text-xs text-base-content/60">Daily summary time (IST, weekdays)</span>
          <input class="input input-sm num w-full" bind:value={cfg.summaryTime} placeholder="15:45" />
        </label>
        <label class="flex flex-col gap-1">
          <span class="text-xs text-base-content/60">Token reminder lead (hours)</span>
          <input class="input input-sm num w-full" type="number" min="0" max="72" bind:value={cfg.tokenWarnHours} />
        </label>
        <label class="flex flex-col gap-1">
          <span class="text-xs text-base-content/60">Fyers token lasts (hours, 0 = no reminder)</span>
          <input class="input input-sm num w-full" type="number" min="0" step="1" bind:value={cfg.fyersTokenHours} />
        </label>
        <label class="flex flex-col gap-1">
          <span class="text-xs text-base-content/60">Dhan token lasts (hours, 0 = no reminder)</span>
          <input class="input input-sm num w-full" type="number" min="0" step="1" bind:value={cfg.dhanTokenHours} />
        </label>
      </div>
      <p class="text-xs text-base-content/60">
        Token expiry is estimated: the time you last saved the token on the Accounts page plus the lifetime above.
      </p>

      {#if msg}
        <div role="status" class="alert {msg.ok ? 'alert-success' : 'alert-error'} alert-soft py-2 text-sm break-words">{msg.message}</div>
      {/if}
      <div class="flex gap-2">
        <button class="btn btn-primary btn-sm" onclick={save} disabled={saving || !dirty}>
          {#if saving}<span class="loading loading-spinner loading-xs"></span>{/if} Save
        </button>
        <button class="btn btn-ghost btn-sm" onclick={() => settings && apply(settings)} disabled={!dirty}>Revert</button>
      </div>
    </section>
  {/if}

  <section class="flex flex-col gap-3 rounded-box border border-base-300 bg-base-100 p-5">
    <div class="flex flex-wrap items-center gap-3">
      <h2 class="font-semibold">History</h2>
      <label class="ml-auto flex cursor-pointer items-center gap-2 text-xs text-base-content/60">
        <input type="checkbox" class="checkbox checkbox-xs" bind:checked={hideQuiet} />
        Hide suppressed and skipped
      </label>
      <button class="btn btn-ghost btn-xs" onclick={clearHistory} disabled={!rows.length}>Clear</button>
    </div>
    {#if !shown.length}
      <p class="text-sm text-base-content/60">Nothing yet. Alerts you send, and ones that failed, are listed here.</p>
    {:else}
      <div class="overflow-x-auto">
        <table class="table table-sm">
          <thead>
            <tr><th class="w-28">Time (IST)</th><th class="w-32">Event</th><th>Alert</th><th class="w-24">Status</th></tr>
          </thead>
          <tbody>
            {#each shown as r (r.id)}
              <tr class="align-top">
                <td class="num whitespace-nowrap text-xs">{stamp(r.at)}</td>
                <td class="text-xs">{kindLabel(r.kind)}</td>
                <td>
                  <div class="text-sm break-words">{r.title}</div>
                  {#if r.body}<div class="text-xs whitespace-pre-line break-words text-base-content/60">{r.body}</div>{/if}
                  {#if r.detail && r.status !== 'sent'}
                    <div class="mt-1 text-xs whitespace-pre-line break-words {r.status === 'failed' || r.status === 'partial' ? 'text-error' : 'text-base-content/50'}">{r.detail}</div>
                  {/if}
                </td>
                <td>
                  <span class="badge badge-sm {statusClass(r.status)}">{r.status}</span>
                  {#if r.attempts > 1}<div class="text-xs text-base-content/50">{r.attempts} tries</div>{/if}
                </td>
              </tr>
            {/each}
          </tbody>
        </table>
      </div>
    {/if}
  </section>
</div>
