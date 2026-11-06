<script lang="ts">
  import { onMount } from 'svelte';
  import { invoke } from '@tauri-apps/api/core';
  import { open } from '@tauri-apps/plugin-dialog';
  import { ramMB, size, type Settings } from './types';

  let { first, used, count, onapplied }: { first: boolean; used: number; count: number; onapplied: () => void } = $props();
  const sections = ['Recording', 'Video', 'Audio', 'Storage', 'System'] as const;
  let section = $state<(typeof sections)[number]>('Recording');
  let draft = $state<Settings | null>(null);
  let saved = $state('');
  let error = $state('');
  let busy = $state(false);
  let playing = $state<string[]>([]);
  let windowed = $state<string[]>([]);
  const choices = (open: string[], on: string[] = []) => [...new Set([...open, ...on])].sort();
  const changed = $derived(draft !== null && JSON.stringify(draft) !== saved);

  const presets = [
    { name: 'Light', height: 1080, fps: 60, mbps: 20 },
    { name: 'Balanced', height: 1440, fps: 60, mbps: 35 },
    { name: 'High', height: 4320, fps: 60, mbps: 50 },
    { name: 'Max', height: 4320, fps: 120, mbps: 80 }
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
  <div class="toolbar">
    <h1 class="toolbar-title">Settings</h1>
    <span class="grow"></span>
    <div class="seg" role="radiogroup" aria-label="Section">
      {#each sections as name (name)}
        <button role="radio" aria-checked={section === name} onclick={() => (section = name)}>{name}</button>
      {/each}
    </div>
    <span class="grow"></span>
  </div>

  {#if draft}
    <div class="settings-body">
      {#if section === 'Recording'}
        <section class="group">
          <h2 class="group-head">Buffer</h2>
          <div class="rows">
            <label class="row">
              <span class="row-label">Keep the last<small>About {ramMB(draft)} MB of memory at the current video settings</small></span>
              <select class="field" bind:value={draft.buffer}>
                {#each buffers as b (b)}<option value={b}>{b < 60 ? `${b} s` : `${b / 60} min`}</option>{/each}
              </select>
            </label>
          </div>
        </section>
        <section class="group">
          <h2 class="group-head">Apps in a window</h2>
          <p class="form-hint">
            A game is recorded once it covers a whole monitor, fullscreen or borderless. Apps switched on here are recorded in a window
            too. Nothing else is, not even the desktop.
          </p>
          <div class="rows">
            {#each choices(windowed, draft.record) as exe (exe)}
              <label class="row">
                <span class="row-label mono">{exe}</span>
                <input type="checkbox" checked={draft.record.includes(exe)} onchange={(e) => pick('record', exe, e.currentTarget.checked)} />
              </label>
            {:else}
              <div class="row"><span class="row-label dim">No app has a window open right now</span></div>
            {/each}
            <div class="row"><span class="row-label"></span><button class="gel" onclick={look}>Look again</button></div>
          </div>
        </section>
      {:else if section === 'Video'}
        <section class="group">
          <h2 class="group-head">Quality</h2>
          <p class="form-hint">Higher settings hold more in memory and cost more to encode.</p>
          <div class="rows">
            <div class="row">
              <span class="row-label">Preset</span>
              <div class="seg" role="radiogroup" aria-label="Preset">
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
            </div>
            <label class="row">
              <span class="row-label">Resolution</span>
              <select class="field" bind:value={draft.height}>
                {#each heights as h (h)}<option value={h}>{h >= 4320 ? 'Native' : `${h}p`}</option>{/each}
              </select>
            </label>
            <label class="row">
              <span class="row-label">Frame rate</span>
              <select class="field" bind:value={draft.fps}>
                {#each rates as r (r)}<option value={r}>{r} fps</option>{/each}
              </select>
            </label>
            <label class="row">
              <span class="row-label">Bitrate<small>Mbps</small></span>
              <input class="field" type="number" min="1" max="200" step="1" bind:value={draft.mbps} />
            </label>
            <label class="row">
              <span class="row-label">Keep HDR<small>HEVC HDR10 when the display is in HDR, instead of tonemapping to SDR</small></span>
              <input type="checkbox" bind:checked={draft.hdr10} />
            </label>
          </div>
        </section>
        <section class="group">
          <h2 class="group-head">Result</h2>
          <div class="rows">
            <div class="row">
              <span class="row-label">Video</span>
              <span class="mono">
                {draft.height >= 4320 ? 'native' : `${draft.height}p`}{draft.fps}, {draft.mbps} Mbps, {draft.hdr10 ? 'HEVC HDR10 on HDR' : 'H.264 SDR'}
              </span>
            </div>
            <div class="row"><span class="row-label">In memory</span><span class="mono">about {ramMB(draft)} MB for {draft.buffer} s</span></div>
          </div>
        </section>
      {:else if section === 'Audio'}
        <section class="group">
          <h2 class="group-head">Tracks</h2>
          <p class="form-hint">The recorded app, everything else and Discord always get a track each. Apps switched on here get one too.</p>
          <div class="rows">
            {#each choices(playing, draft.apps) as exe (exe)}
              <label class="row">
                <span class="row-label mono">{exe}</span>
                <input type="checkbox" checked={draft.apps.includes(exe)} onchange={(e) => pick('apps', exe, e.currentTarget.checked)} />
              </label>
            {:else}
              <div class="row"><span class="row-label dim">No app is playing sound right now</span></div>
            {/each}
            <div class="row"><span class="row-label"></span><button class="gel" onclick={look}>Look again</button></div>
          </div>
        </section>
        <section class="group">
          <h2 class="group-head">Microphone</h2>
          <div class="rows">
            <label class="row">
              <span class="row-label">Record the default microphone on its own track</span>
              <input type="checkbox" bind:checked={draft.mic} />
            </label>
          </div>
        </section>
      {:else if section === 'Storage'}
        <section class="group">
          <h2 class="group-head">Clips</h2>
          <div class="rows">
            <div class="row">
              <span class="row-label">Folder<small class="row-path mono">{draft.folder}</small></span>
              <button class="gel" onclick={pickFolder}>Change</button>
            </div>
            <div class="row">
              <span class="row-label">In use<small>{count} {count === 1 ? 'clip' : 'clips'}</small></span>
              {#if draft.cap > 0}<span class="meter"><span style:width="{Math.min(100, used / (draft.cap * 1e7))}%"></span></span>{/if}
              <span class="mono">{size(used)}{draft.cap > 0 ? ` of ${draft.cap} GB` : ''}</span>
            </div>
          </div>
        </section>
        <section class="group">
          <h2 class="group-head">Limit</h2>
          <p class="form-hint">
            0 keeps everything. Past the limit the oldest unstarred clips are deleted for good after each save, and they skip the
            Recycle Bin.
          </p>
          <div class="rows">
            <label class="row">
              <span class="row-label">Keep clips under<small>GB</small></span>
              <input class="field" type="number" min="0" step="1" bind:value={draft.cap} />
            </label>
          </div>
        </section>
      {:else}
        <section class="group">
          <h2 class="group-head">Startup</h2>
          <p class="form-hint">memory lives in the tray. Closing this window keeps it recording.</p>
          <div class="rows">
            <label class="row">
              <span class="row-label">Start with Windows</span>
              <input type="checkbox" bind:checked={draft.startWithWindows} />
            </label>
          </div>
        </section>
        <section class="group">
          <h2 class="group-head">Game art</h2>
          <p class="form-hint">
            Covers of Steam games come from Steam. For other games, paste an API key from the preferences of your SteamGridDB profile.
          </p>
          <div class="rows">
            <label class="row">
              <span class="row-label">SteamGridDB key</span>
              <input class="field" type="password" spellcheck="false" autocomplete="off" placeholder="None" bind:value={draft.steamgriddb} />
            </label>
          </div>
        </section>
      {/if}
    </div>

    {#if changed || first || error}
      <div class="applybar">
        <span class="grow">
          {#if error}<span class="form-error">{error}</span>
          {:else if first}memory is already recording with these. Change anything, then apply.
          {:else}Unsaved changes. Applying restarts the buffer.{/if}
        </span>
        {#if changed}<button class="gel" disabled={busy} onclick={revert}>Revert</button>{/if}
        <button class="gel default" disabled={(!changed && !first) || busy} onclick={apply}>Apply</button>
      </div>
    {/if}
  {/if}
</div>
