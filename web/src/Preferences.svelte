<script lang="ts">
  import { onMount } from 'svelte';
  import { invoke } from '@tauri-apps/api/core';
  import { open } from '@tauri-apps/plugin-dialog';
  import { ramMB, type Settings } from './types';

  let { first, onapplied }: { first: boolean; onapplied: () => void } = $props();
  let draft = $state<Settings | null>(null);
  let saved = $state('');
  let error = $state('');
  let busy = $state(false);
  let playing = $state<string[]>([]);
  const choices = $derived([...new Set([...playing, ...(draft?.apps ?? [])])].sort());
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
    await listPlaying();
  });

  async function listPlaying() {
    try {
      playing = await invoke<string[]>('audio_apps');
    } catch (e) {
      error = String(e);
    }
  }

  function split(exe: string, on: boolean) {
    if (!draft) return;
    draft.apps = on ? [...draft.apps, exe] : draft.apps.filter((app) => app !== exe);
  }

  async function pickFolder() {
    if (!draft) return;
    const picked = await open({ directory: true, defaultPath: draft.folder });
    if (typeof picked === 'string') draft.folder = picked;
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

<div class="view">
  <div class="view-head">
    <h1>settings</h1>
    <p class="view-sub">
      {first ? 'memory is already recording with these. change anything, then apply' : 'applying restarts the buffer'}
    </p>
  </div>

  {#if draft}
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
      <label>
        <span>keep the last</span>
        <select bind:value={draft.buffer}>
          {#each buffers as b (b)}<option value={b}>{b < 60 ? `${b} s` : `${b / 60} min`}</option>{/each}
        </select>
      </label>
    </div>
    <label class="toggle">
      <input type="checkbox" bind:checked={draft.hdr10} />
      <span class="switch"></span>
      <span>keep hdr as hevc hdr10 when the display is in hdr, instead of tonemapping to sdr</span>
    </label>

    <h3 class="rule-head">audio</h3>
    <p class="form-hint">the recorded app, everything else and discord always get a track each. apps switched on here get one too</p>
    <div class="apps">
      {#each choices as exe (exe)}
        <label class="toggle">
          <input type="checkbox" checked={draft.apps.includes(exe)} onchange={(e) => split(exe, e.currentTarget.checked)} />
          <span class="switch"></span>
          <span class="proper mono">{exe}</span>
        </label>
      {:else}
        <p class="form-hint">no app is playing sound right now</p>
      {/each}
    </div>
    <div class="btn-row"><button class="btn ghost" onclick={listPlaying}>look again</button></div>
    <label class="toggle">
      <input type="checkbox" bind:checked={draft.mic} />
      <span class="switch"></span>
      <span>record the default microphone on its own track</span>
    </label>

    <h3 class="rule-head">files</h3>
    <div class="btn-row">
      <span class="folder proper mono">{draft.folder}</span>
      <button class="btn" onclick={pickFolder}>change</button>
    </div>
    <div class="fields">
      <label>
        <span>keep clips under, <span class="proper">GB</span></span>
        <input type="number" min="0" step="1" bind:value={draft.cap} />
      </label>
    </div>
    <p class="form-hint">0 keeps everything. past the limit the oldest unstarred clips are deleted for good after each save, they skip the recycle bin</p>

    <h3 class="rule-head">system</h3>
    <label class="toggle">
      <input type="checkbox" bind:checked={draft.startWithWindows} />
      <span class="switch"></span>
      <span>start with windows</span>
    </label>

    <div class="btn-row apply">
      <button class="btn primary" disabled={(!changed && !first) || busy} onclick={apply}>apply</button>
      {#if error}<span class="form-error">{error}</span>{/if}
    </div>
  {/if}
</div>
