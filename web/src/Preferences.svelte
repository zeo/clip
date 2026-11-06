<script lang="ts">
  import { onMount } from 'svelte';
  import { invoke } from '@tauri-apps/api/core';
  import { open } from '@tauri-apps/plugin-dialog';
  import { ramMB, size, type Settings } from './types';

  let { first, used, count, onapplied }: { first: boolean; used: number; count: number; onapplied: () => void } = $props();
  const sections = ['recording', 'video', 'audio', 'storage', 'system'] as const;
  let section = $state<(typeof sections)[number]>('recording');
  let draft = $state<Settings | null>(null);
  let saved = $state('');
  let error = $state('');
  let busy = $state(false);
  let playing = $state<string[]>([]);
  let windowed = $state<string[]>([]);
  const choices = (open: string[], on: string[] = []) => [...new Set([...open, ...on])].sort();
  const changed = $derived(draft !== null && JSON.stringify(draft) !== saved);

  const presets = [
    { name: 'light', height: 1080, fps: 60, mbps: 20 },
    { name: 'balanced', height: 1440, fps: 60, mbps: 35 },
    { name: 'high', height: 4320, fps: 60, mbps: 50 },
    { name: 'max', height: 4320, fps: 120, mbps: 80 }
  ];
  const buffers = [15, 30, 60, 120, 300];
  const heights = [720, 1080, 1440, 4320];
  const rates = [30, 60, 120, 144, 240];

  onMount(async () => {
    const s = await invoke<Settings>('settings');
    saved = JSON.stringify(s);
    draft = s;
    await look();
  });

  async function look() {
    try {
      [playing, windowed] = await Promise.all([invoke<string[]>('audio_apps'), invoke<string[]>('windowed_apps')]);
    } catch (e) {
      error = String(e);
    }
  }

  function pick(list: 'apps' | 'record', exe: string, on: boolean) {
    if (!draft) return;
    draft[list] = on ? [...draft[list], exe] : draft[list].filter((app) => app !== exe);
  }

  async function pickFolder() {
    if (!draft) return;
    const picked = await open({ directory: true, defaultPath: draft.folder });
    if (typeof picked === 'string') draft.folder = picked;
  }

  function revert() {
    draft = JSON.parse(saved);
    error = '';
  }

  async function apply() {
    if (!draft) return;
    draft.cap = Math.max(0, Math.floor(draft.cap || 0));
    busy = true;
    error = '';
    try {
      await invoke('apply', { settings: draft });
      saved = JSON.stringify(draft);
      onapplied();
    } catch (e) {
      error = String(e);
    } finally {
      busy = false;
    }
  }
</script>

<div class="settings">
  <nav class="settings-nav" aria-label="settings">
    <h1>settings</h1>
    {#each sections as name (name)}
      <button class="settings-item" aria-current={section === name ? 'page' : undefined} onclick={() => (section = name)}>{name}</button>
    {/each}
  </nav>

  {#if draft}
    <div class="settings-body">
      {#if section === 'recording'}
        <div class="view-head">
          <h2>recording</h2>
          <p class="view-sub">what gets recorded, and how much of it stays in memory</p>
        </div>
        <div class="fields">
          <label>
            <span>keep the last</span>
            <select bind:value={draft.buffer}>
              {#each buffers as b (b)}<option value={b}>{b < 60 ? `${b} s` : `${b / 60} min`}</option>{/each}
            </select>
          </label>
        </div>
        <h3 class="rule-head">apps in a window</h3>
        <p class="form-hint">
          a game is recorded once it covers a whole monitor, fullscreen or borderless. apps switched on here are recorded in a window too. nothing else is, not even the desktop
        </p>
        <div class="apps">
          {#each choices(windowed, draft.record) as exe (exe)}
            <label class="toggle">
              <input type="checkbox" checked={draft.record.includes(exe)} onchange={(e) => pick('record', exe, e.currentTarget.checked)} />
              <span class="switch"></span>
              <span class="proper mono">{exe}</span>
            </label>
          {:else}
            <p class="form-hint">no app has a window open right now</p>
          {/each}
        </div>
        <div class="btn-row"><button class="btn ghost" onclick={look}>look again</button></div>
      {:else if section === 'video'}
        <div class="view-head">
          <h2>video</h2>
          <p class="view-sub">higher settings hold more in memory and cost more to encode</p>
        </div>
        <dl class="spec">
          <div><dt>keeps</dt><dd>{draft.buffer} s</dd></div>
          <div><dt>video</dt><dd>{draft.height >= 4320 ? 'native' : `${draft.height}p`}{draft.fps}, {draft.mbps} <span class="proper">Mbps</span></dd></div>
          <div><dt>ram</dt><dd>about {ramMB(draft)} <span class="proper">MB</span></dd></div>
          <div><dt>format</dt><dd>{draft.hdr10 ? 'hevc hdr10 on hdr' : 'h.264 sdr'}</dd></div>
        </dl>
        <h3 class="rule-head">quality</h3>
        <div class="segmented" role="radiogroup" aria-label="preset">
          {#each presets as p (p.name)}
            <button
              role="radio"
              aria-checked={draft.height === p.height && draft.fps === p.fps && draft.mbps === p.mbps}
              onclick={() => draft && Object.assign(draft, { height: p.height, fps: p.fps, mbps: p.mbps })}
            >
              {p.name}
            </button>
          {/each}
        </div>
        <div class="fields">
          <label>
            <span>resolution</span>
            <select bind:value={draft.height}>
              {#each heights as h (h)}<option value={h}>{h >= 4320 ? 'native' : `${h}p`}</option>{/each}
            </select>
          </label>
          <label>
            <span>frame rate</span>
            <select bind:value={draft.fps}>
              {#each rates as r (r)}<option value={r}>{r} fps</option>{/each}
            </select>
          </label>
          <label>
            <span>bitrate, <span class="proper">Mbps</span></span>
            <input type="number" min="1" max="200" step="1" bind:value={draft.mbps} />
          </label>
        </div>
        <label class="toggle">
          <input type="checkbox" bind:checked={draft.hdr10} />
          <span class="switch"></span>
          <span>keep hdr as hevc hdr10 when the display is in hdr, instead of tonemapping to sdr</span>
        </label>
      {:else if section === 'audio'}
        <div class="view-head">
          <h2>audio</h2>
          <p class="view-sub">the recorded app, everything else and discord always get a track each. apps switched on here get one too</p>
        </div>
        <div class="apps">
          {#each choices(playing, draft.apps) as exe (exe)}
            <label class="toggle">
              <input type="checkbox" checked={draft.apps.includes(exe)} onchange={(e) => pick('apps', exe, e.currentTarget.checked)} />
              <span class="switch"></span>
              <span class="proper mono">{exe}</span>
            </label>
          {:else}
            <p class="form-hint">no app is playing sound right now</p>
          {/each}
        </div>
        <div class="btn-row"><button class="btn ghost" onclick={look}>look again</button></div>
        <h3 class="rule-head">microphone</h3>
        <label class="toggle">
          <input type="checkbox" bind:checked={draft.mic} />
          <span class="switch"></span>
          <span>record the default microphone on its own track</span>
        </label>
      {:else if section === 'storage'}
        <div class="view-head">
          <h2>storage</h2>
          <p class="view-sub">
            {count} {count === 1 ? 'clip' : 'clips'}, <span class="proper">{size(used)}</span>
            {#if draft.cap > 0}of {draft.cap} <span class="proper">GB</span>{/if}
          </p>
        </div>
        {#if draft.cap > 0}<div class="meter wide"><span style:width="{Math.min(100, used / (draft.cap * 1e7))}%"></span></div>{/if}
        <h3 class="rule-head">folder</h3>
        <div class="btn-row">
          <span class="folder proper mono">{draft.folder}</span>
          <button class="btn" onclick={pickFolder}>change</button>
        </div>
        <h3 class="rule-head">limit</h3>
        <div class="fields">
          <label>
            <span>keep clips under, <span class="proper">GB</span></span>
            <input type="number" min="0" step="1" bind:value={draft.cap} />
          </label>
        </div>
        <p class="form-hint">0 keeps everything. past the limit the oldest unstarred clips are deleted for good after each save, they skip the recycle bin</p>
      {:else}
        <div class="view-head">
          <h2>system</h2>
          <p class="view-sub">memory lives in the tray, closing this window keeps it recording</p>
        </div>
        <label class="toggle">
          <input type="checkbox" bind:checked={draft.startWithWindows} />
          <span class="switch"></span>
          <span>start with windows</span>
        </label>
      {/if}
    </div>

    {#if changed || first || error}
      <div class="applybar">
        <span class="applybar-text">
          {#if error}<span class="form-error">{error}</span>
          {:else if first}memory is already recording with these. change anything, then apply
          {:else}unsaved changes, applying restarts the buffer{/if}
        </span>
        {#if changed}<button class="btn ghost" disabled={busy} onclick={revert}>revert</button>{/if}
        <button class="btn primary" disabled={(!changed && !first) || busy} onclick={apply}>apply</button>
      </div>
    {/if}
  {/if}
</div>
