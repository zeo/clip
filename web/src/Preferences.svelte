<script lang="ts">
  import { onMount, tick } from 'svelte';
  import { invoke } from '@tauri-apps/api/core';
  import { getVersion } from '@tauri-apps/api/app';
  import { open } from '@tauri-apps/plugin-dialog';
  import Bind from './Bind.svelte';
  import { ACTIONS, PRESETS, chord, length, ramMB, size, type Corner, type Disk, type Hotkey, type Mic, type Settings, type Status } from './types';
  import { radios, reduced, say, view, zooms } from './view.svelte';

  let {
    first,
    used,
    count,
    status,
    page = $bindable(),
    onapplied
  }: { first: boolean; used: number; count: number; status: Status; page: string; onapplied: () => void } = $props();

  const groups: { name: string; pages: { id: string; title: string; blurb: string; path: string }[] }[] = [
    {
      name: 'Recording',
      pages: [
        { id: 'hotkeys', title: 'Hotkeys', blurb: 'Keys that save clips, mark, record and take screenshots from inside a game', path: 'M2.5 6h15v9h-15zM5.5 9h1M8.5 9h1M11.5 9h1M14.5 9h0M6.5 12h7' },
        { id: 'recorder', title: 'Recorder', blurb: 'What clip records and how much it keeps', path: 'M10 3.5a6.5 6.5 0 1 0 0 13 6.5 6.5 0 0 0 0-13zM10 8a2 2 0 1 0 0 4 2 2 0 0 0 0-4z' },
        { id: 'quality', title: 'Quality', blurb: 'Resolution, frame rate and bitrate of the video', path: 'M3 4.5h14v10H3zM7 14.5v2M13 14.5v2M5.5 16.5h9' },
        { id: 'audio', title: 'Audio', blurb: 'A track for each source, its volume and the microphone', path: 'M3.5 8h3l4-3.5v11l-4-3.5h-3zM13.5 7.5c1.2 1.4 1.2 3.6 0 5M15.5 5.5c2.4 2.6 2.4 6.4 0 9' }
      ]
    },
    {
      name: 'Storage',
      pages: [{ id: 'clips', title: 'Clips', blurb: 'Where clips go and how long they stay', path: 'M3 5h5l1.5 1.5H17V15H3z' }]
    },
    {
      name: 'App',
      pages: [
        { id: 'appearance', title: 'Appearance', blurb: 'How big everything is, and clips playing on hover', path: 'M2.5 15 6.5 5l4 10M4 11.5h5M12 15l2.75-6.5L17.5 15M13 12.8h3.5' },
        { id: 'sounds', title: 'Alerts', blurb: 'The sound and the note on screen that say a clip was saved', path: 'M6 13V9a4 4 0 0 1 8 0v4l1.5 1.5h-11zM8.5 16.5h3' },
        { id: 'notifications', title: 'Notifications', blurb: 'A Windows notification for each saved clip', path: 'M3.5 4h13v9h-6l-3.5 3v-3H3.5z' },
        { id: 'startup', title: 'Startup and window', blurb: 'When clip opens and what closing the window does', path: 'M10 3v6M6.2 5.8a6 6 0 1 0 7.6 0' },
        { id: 'art', title: 'Game art', blurb: 'Covers and banners for the library', path: 'M3 4.5h14v11H3zM3 13l4-4 3 3 2-2 5 5' }
      ]
    }
  ];
  const pages = groups.flatMap((g) => g.pages);
  // changing any of these starts a new buffer, the rest apply as they are
  const restarting = ['buffer', 'fps', 'height', 'mbps', 'hdr10', 'ignore', 'mic', 'apps', 'volumes', 'micDevice', 'pushToTalk'] as const;
  const presets = [
    { name: 'Light', height: 1080, fps: 60, mbps: 20 },
    { name: 'Balanced', height: 1440, fps: 60, mbps: 35 },
    { name: 'High', height: 4320, fps: 60, mbps: 50 },
    { name: 'Max', height: 4320, fps: 120, mbps: 80 }
  ];
  const buffers = [15, 30, 60, 120, 300];
  const corners: [Corner, string][] = [
    ['top-left', 'Top left'],
    ['top-right', 'Top right'],
    ['bottom-left', 'Bottom left'],
    ['bottom-right', 'Bottom right']
  ];
  let from = $state('');
  const heights = [720, 1080, 1440, 4320];
  const rates = [30, 60, 120, 144, 240];

  let draft = $state<Settings | null>(null);
  let saved = $state('');
  let error = $state('');
  let busy = $state(false);
  let playing = $state<string[]>([]);
  let windowed = $state<string[]>([]);
  let mics = $state<Mic[]>([]);
  let disk = $state<Disk | null>(null);
  let level = $state<number | null>(null);
  let sent = $state('');
  let version = $state('');
  let search = $state('');
  let found = $state<Record<string, number>>({});
  let body = $state<HTMLElement>();
  const choices = (open: string[], on: string[] = []) => [...new Set([...open, ...on])].sort();
  const before = $derived<Settings | null>(saved ? JSON.parse(saved) : null);
  const changed = $derived(draft !== null && JSON.stringify(draft) !== saved);
  const restarts = $derived(!!draft && !!before && restarting.some((k) => JSON.stringify(draft![k]) !== JSON.stringify(before[k])));
  const searching = $derived(search.trim() !== '');
  const shownPages = $derived(searching ? pages.filter((p) => found[p.id]) : pages.filter((p) => p.id === page));
  const lanes = $derived(
    draft
      ? [
          { name: 'game', label: 'The game', hint: 'The app being recorded' },
          { name: 'others', label: 'Everything else', hint: 'Every other app, unless it has its own track' },
          { name: 'voice', label: 'Discord', hint: 'Voice chat, while Discord is running' }
        ]
      : []
  );

  onMount(async () => {
    const s = await invoke<Settings>('settings');
    saved = JSON.stringify(s);
    draft = s;
    getVersion().then((v) => (version = v));
    await look();
  });

  $effect(() => {
    if (draft?.folder) invoke<Disk>('disk', { folder: draft.folder }).then((d) => (disk = d), () => (disk = null));
  });

  // rows are matched on what they say, so every page is searched without a list kept beside the markup
  $effect(() => {
    const words = search.trim().toLowerCase();
    if (!body || !draft) return;
    tick().then(() => {
      const next: Record<string, number> = {};
      for (const section of body!.querySelectorAll<HTMLElement>('[data-page]')) {
        let hits = 0;
        for (const group of section.querySelectorAll<HTMLElement>('.group')) {
          let inGroup = 0;
          for (const row of group.querySelectorAll<HTMLElement>('.row')) {
            const hit = !words || (row.querySelector('.row-label')?.textContent ?? '').toLowerCase().includes(words);
            row.hidden = !hit;
            inGroup += +hit;
          }
          group.hidden = inGroup === 0;
          hits += inGroup;
        }
        section.hidden = !!words && hits === 0;
        next[section.dataset.page!] = hits;
      }
      if (words) found = next;
    });
  });

  // while searching every page is drawn once so its rows can be counted
  const drawn = (id: string) => searching || page === id;

  async function look() {
    try {
      [playing, windowed, mics] = await Promise.all([invoke<string[]>('audio_apps'), invoke<string[]>('windowed_apps'), invoke<Mic[]>('mics')]);
    } catch (e) {
      error = String(e);
    }
  }

  function pick(list: 'apps' | 'record' | 'ignore', exe: string, on: boolean) {
    if (!draft) return;
    draft[list] = on ? [...draft[list], exe] : draft[list].filter((app) => app !== exe);
  }

  function volume(name: string, percent: number) {
    if (draft) draft.volumes = { ...draft.volumes, [name]: percent };
  }

  async function test() {
    if (level !== null) return (level = null);
    level = 0;
    const until = Date.now() + 10_000;
    while (level !== null && Date.now() < until) {
      try {
        level = await invoke<number>('mic_level', { device: draft?.micDevice ?? '' });
      } catch (e) {
        error = String(e);
        break;
      }
    }
    level = null;
  }

  async function notify() {
    sent = '';
    try {
      await invoke('test_notification');
      sent = 'Sent';
    } catch (e) {
      sent = String(e).replace(/^./, (c) => c.toUpperCase());
    }
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
    draft.keepDays = Math.max(0, Math.floor(draft.keepDays || 0));
    busy = true;
    error = '';
    try {
      await invoke('apply', { settings: draft });
      say(restarts ? 'Settings applied, a new buffer started' : 'Settings applied');
      saved = JSON.stringify(draft);
      onapplied();
    } catch (e) {
      error = String(e);
    } finally {
      busy = false;
    }
  }

  const taken = (hotkey: Hotkey) => status.hotkeys.some((h) => h.taken && h.key === hotkey.key && h.mods === hotkey.mods);
</script>

{#snippet head(id: string)}
  {@const p = pages.find((p) => p.id === id)!}
  <header class="page-head" class:small={searching}>
    <svg viewBox="0 0 20 20" aria-hidden="true"><path d={p.path} /></svg>
    <h1>{p.title}</h1>
    {#if !searching}<p>{p.blurb}</p>{/if}
  </header>
{/snippet}

{#snippet slider(name: string, label: string)}
  {@const value = draft?.volumes[name] ?? 100}
  <input type="range" min="0" max="200" step="5" aria-label="{label} volume" {value} oninput={(e) => volume(name, Number(e.currentTarget.value))} />
  <span class="mono readout">{value}%</span>
{/snippet}

<aside class="side">
  <input class="field search side-search" type="search" placeholder="Search settings" aria-label="Search settings" bind:value={search} />
  <div class="side-list">
    {#each groups as group (group.name)}
      {@const listed = searching ? group.pages.filter((p) => found[p.id]) : group.pages}
      {#if listed.length}
        <h2 class="side-head">{group.name}</h2>
        {#each listed as p (p.id)}
          <button
            class="side-item"
            aria-current={!searching && page === p.id ? 'page' : undefined}
            onclick={() => {
              page = p.id;
              search = '';
            }}
          >
            <svg viewBox="0 0 20 20" aria-hidden="true"><path d={p.path} /></svg>
            <span class="grow">{p.title}</span>
            {#if searching}<span class="count">{found[p.id]}</span>{/if}
          </button>
        {/each}
      {/if}
    {/each}
    {#if searching && shownPages.length === 0}<p class="side-note">Nothing matches</p>{/if}
  </div>
  {#if version}<p class="side-note mono">clip {version}</p>{/if}
</aside>

<main class="stage settings">
  {#if draft}
    <div class="settings-body" bind:this={body}>
      {#if drawn('hotkeys')}
        <section data-page="hotkeys">
          {@render head('hotkeys')}
          <section class="group">
            <p class="form-hint">Click a key box, then press the keys. Backspace clears one. A save key can keep less than the whole buffer.</p>
            <div class="rows">
              <div class="row">
                <span class="row-label">Coming from another app<small>Uses the keys it ships with. Close it first, or it keeps the keys.</small></span>
                <select class="field" aria-label="App to copy keys from" bind:value={from}>
                  <option value="">Pick an app</option>
                  {#each PRESETS as preset (preset.name)}
                    <option value={preset.name}>{preset.name}: {preset.hotkeys.map(chord).join(', ')}</option>
                  {/each}
                </select>
                <button
                  class="gel"
                  disabled={!from}
                  onclick={() => {
                    draft!.hotkeys = structuredClone(PRESETS.find((p) => p.name === from)!.hotkeys);
                    say(`Keys from ${from}, apply to keep them`);
                  }}>Use keys</button
                >
              </div>
              {#each draft.hotkeys as hotkey, i (i)}
                <div class="row">
                  <span class="row-label">
                    {ACTIONS[hotkey.action].key} hotkey
                    <small class:bad={taken(hotkey)}>{taken(hotkey) ? 'Another app holds this key' : ACTIONS[hotkey.action].does}</small>
                  </span>
                  <Bind bind:mods={hotkey.mods} bind:key={hotkey.key} label="Keys for hotkey {i + 1}" />
                  <select class="field" aria-label="Action" bind:value={hotkey.action}>
                    {#each Object.entries(ACTIONS) as [value, action] (value)}<option {value}>{action.name}</option>{/each}
                  </select>
                  <select class="field" aria-label="Length" bind:value={hotkey.seconds} disabled={hotkey.action !== 'save'}>
                    <option value={0}>Whole buffer</option>
                    {#each buffers.filter((b) => b < draft!.buffer) as b (b)}<option value={b}>Last {length(b)}</option>{/each}
                  </select>
                  <button class="gel" aria-label="Remove hotkey {i + 1}" onclick={() => draft!.hotkeys.splice(i, 1)}>Remove</button>
                </div>
              {/each}
              <div class="row">
                <span class="row-label">Add hotkey<small>A key for any of the actions above</small></span>
                <button class="gel" onclick={() => draft!.hotkeys.push({ action: 'save', mods: 1, key: 0, seconds: 0 })}>Add</button>
              </div>
            </div>
          </section>
        </section>
      {/if}

      {#if drawn('recorder')}
        <section data-page="recorder">
          {@render head('recorder')}
          <section class="group">
            <h2 class="group-head">Buffer</h2>
            <div class="rows">
              <label class="row">
                <span class="row-label">Keep the last<small>About {ramMB(draft)} MB of memory at the current video settings</small></span>
                <select class="field" bind:value={draft.buffer}>
                  {#each buffers as b (b)}<option value={b}>{length(b)}</option>{/each}
                </select>
              </label>
            </div>
          </section>
          <section class="group">
            <h2 class="group-head">Apps in a window</h2>
            <p class="form-hint">A game is recorded once it covers a whole monitor. Apps switched on here are recorded in a window too.</p>
            <div class="rows">
              {#each choices(windowed, draft.record) as exe (exe)}
                <label class="row">
                  <span class="row-label mono">Record {exe} in a window</span>
                  <input type="checkbox" checked={draft.record.includes(exe)} onchange={(e) => pick('record', exe, e.currentTarget.checked)} />
                </label>
              {:else}
                <div class="row"><span class="row-label dim">No app has a window open right now</span></div>
              {/each}
              <div class="row"><span class="row-label">Look again<small>Lists the apps open right now</small></span><button class="gel" onclick={look}>Look again</button></div>
            </div>
          </section>
          <section class="group">
            <h2 class="group-head">Games not recorded</h2>
            <p class="form-hint">Switch a game off from the recording chip at the top while it's in focus.</p>
            <div class="rows">
              {#each choices(before?.ignore ?? [], draft.ignore) as exe (exe)}
                <label class="row">
                  <span class="row-label mono">Record {exe}</span>
                  <input type="checkbox" checked={!draft.ignore.includes(exe)} onchange={(e) => pick('ignore', exe, !e.currentTarget.checked)} />
                </label>
              {:else}
                <div class="row"><span class="row-label dim">Every game is recorded</span></div>
              {/each}
            </div>
          </section>
        </section>
      {/if}

      {#if drawn('quality')}
        <section data-page="quality">
          {@render head('quality')}
          <section class="group">
            <p class="form-hint">Higher settings hold more in memory and cost more to encode.</p>
            <div class="rows">
              <div class="row">
                <span class="row-label">Preset</span>
                <div class="seg" role="radiogroup" aria-label="Preset" use:radios>
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
              <div class="row">
                <span class="row-label">Result<small>About {ramMB(draft)} MB in memory for {length(draft.buffer)}</small></span>
                <span class="mono">
                  {draft.height >= 4320 ? 'native' : `${draft.height}p`}{draft.fps}, {draft.mbps} Mbps, {draft.hdr10 ? 'HEVC HDR10 on HDR' : 'H.264 SDR'}
                </span>
              </div>
            </div>
          </section>
        </section>
      {/if}

      {#if drawn('audio')}
        <section data-page="audio">
          {@render head('audio')}
          <section class="group">
            <h2 class="group-head">Tracks</h2>
            <p class="form-hint">Each source gets its own track. Volumes go up to 200%, louder clips past 100% where it's quiet.</p>
            <div class="rows">
              {#each lanes as lane (lane.name)}
                <div class="row">
                  <span class="row-label">{lane.label} volume<small>{lane.hint}</small></span>
                  {@render slider(lane.name, lane.label)}
                </div>
              {/each}
              {#each choices(playing, draft.apps) as exe (exe)}
                {@const name = exe.toLowerCase().replace(/\.exe$/, '')}
                <div class="row">
                  <label class="row-label check">
                    <input type="checkbox" checked={draft.apps.includes(exe)} onchange={(e) => pick('apps', exe, e.currentTarget.checked)} />
                    <span class="mono">{exe} on its own track</span>
                  </label>
                  {#if draft.apps.includes(exe)}{@render slider(name, exe)}{/if}
                </div>
              {:else}
                <div class="row"><span class="row-label dim">No other app is playing sound right now</span></div>
              {/each}
              <div class="row"><span class="row-label">Look again<small>Lists the apps playing sound right now</small></span><button class="gel" onclick={look}>Look again</button></div>
            </div>
          </section>
          <section class="group">
            <h2 class="group-head">Microphone</h2>
            <div class="rows">
              <label class="row">
                <span class="row-label">Record the microphone<small>On a track of its own</small></span>
                <input type="checkbox" bind:checked={draft.mic} />
              </label>
              <label class="row">
                <span class="row-label">Microphone device</span>
                <select class="field" bind:value={draft.micDevice}>
                  <option value="">Default device</option>
                  {#each mics as mic (mic.id)}<option value={mic.id}>{mic.name}</option>{/each}
                </select>
              </label>
              <div class="row">
                <span class="row-label">Microphone volume<small>Applied to its track</small></span>
                {@render slider('mic', 'Microphone')}
              </div>
              <div class="row">
                <span class="row-label">Test microphone level<small>{mics.length ? 'Speak and watch the bar for up to 10 s' : 'No microphone is plugged in'}</small></span>
                <span class="meter level"><span style:width="{Math.min(100, (level ?? 0) * 100)}%"></span></span>
                <button class="gel" aria-pressed={level !== null} disabled={!mics.length} onclick={test}>{level !== null ? 'Stop' : 'Test'}</button>
              </div>
              <div class="row">
                <span class="row-label">Push to talk<small>The microphone stays silent until this key is held, None keeps it open</small></span>
                <Bind plain bind:key={draft.pushToTalk} label="Push to talk key" />
              </div>
            </div>
          </section>
        </section>
      {/if}

      {#if drawn('clips')}
        <section data-page="clips">
          {@render head('clips')}
          <section class="group">
            <h2 class="group-head">Folder</h2>
            <div class="rows">
              <div class="row">
                <span class="row-label">Clips folder<small class="row-path mono">{draft.folder}</small></span>
                <button class="gel" onclick={pickFolder}>Change</button>
              </div>
              <div class="row">
                <span class="row-label">Space used<small>{count} {count === 1 ? 'clip' : 'clips'}{disk ? `, ${size(disk.free)} free of ${size(disk.total)} on this drive` : ''}</small></span>
                {#if disk}<span class="meter"><span style:width="{Math.min(100, ((disk.total - disk.free) / disk.total) * 100)}%"></span><span class="ours" style:width="{Math.min(100, (used / disk.total) * 100)}%"></span></span>{/if}
                <span class="mono">{size(used)}</span>
              </div>
            </div>
          </section>
          <section class="group">
            <h2 class="group-head">Cleanup</h2>
            <p class="form-hint">0 turns a limit off. Unstarred clips past a limit are deleted for good after each save, and they skip the Recycle Bin.</p>
            <div class="rows">
              <label class="row">
                <span class="row-label">Keep clips under<small>GB, the oldest go first</small></span>
                <input class="field" type="number" min="0" step="1" bind:value={draft.cap} />
              </label>
              <label class="row">
                <span class="row-label">Keep clips for<small>Days</small></span>
                <input class="field" type="number" min="0" step="1" bind:value={draft.keepDays} />
              </label>
            </div>
          </section>
        </section>
      {/if}

      {#if drawn('appearance')}
        <section data-page="appearance">
          {@render head('appearance')}
          <section class="group">
            <p class="form-hint">These change this window straight away, with nothing to apply.</p>
            <div class="rows">
              <label class="row">
                <span class="row-label">Size<small>Ctrl+= and Ctrl+- change it from anywhere in clip, Ctrl+0 goes back to 100%</small></span>
                <select class="field" bind:value={view.zoom}>
                  {#each zooms as z (z)}<option value={z}>{z}%</option>{/each}
                </select>
              </label>
              <label class="row">
                <span class="row-label"
                  >Play clips on hover<small>{reduced.current ? 'Off while Windows has animation effects turned off' : 'A card plays its clip, muted, while the pointer is on it'}</small></span
                >
                <input type="checkbox" bind:checked={view.previews} disabled={reduced.current} />
              </label>
            </div>
          </section>
        </section>
      {/if}

      {#if drawn('sounds')}
        <section data-page="sounds">
          {@render head('sounds')}
          <section class="group">
            <div class="rows">
              <label class="row">
                <span class="row-label">Play a sound when a clip is saved<small>A failed save always beeps</small></span>
                <input type="checkbox" bind:checked={draft.sound} />
              </label>
              <div class="row">
                <span class="row-label">Sound volume</span>
                <input type="range" min="0" max="150" step="5" aria-label="Sound volume" bind:value={draft.soundVolume} disabled={!draft.sound} />
                <span class="mono readout">{draft.soundVolume}%</span>
                <button class="gel" onclick={() => invoke('preview_sound', { percent: draft!.soundVolume })}>Play</button>
              </div>
            </div>
          </section>
          <section class="group">
            <h2 class="group-head">Note on screen</h2>
            <p class="form-hint">A small note over the game for a few seconds. It never takes focus and stays out of recordings and screen shares.</p>
            <div class="rows">
              <label class="row">
                <span class="row-label">Show a note on saves and key presses<small>Clip saved, screenshot, mic muted, recording started</small></span>
                <input type="checkbox" bind:checked={draft.notice} />
              </label>
              <label class="row">
                <span class="row-label">Corner</span>
                <select class="field" bind:value={draft.noticeCorner} disabled={!draft.notice}>
                  {#each corners as [value, name] (value)}<option {value}>{name}</option>{/each}
                </select>
              </label>
            </div>
          </section>
        </section>
      {/if}

      {#if drawn('notifications')}
        <section data-page="notifications">
          {@render head('notifications')}
          <section class="group">
            <div class="rows">
              <label class="row">
                <span class="row-label">Show a notification for each clip<small>With the game, length and size, and a button that opens it</small></span>
                <input type="checkbox" bind:checked={draft.notify} />
              </label>
              <div class="row">
                <span class="row-label">Send a test notification<small class:bad={sent && sent !== 'Sent'}>{sent || 'Windows shows it in the corner and keeps it in the notification center'}</small></span>
                <button class="gel" onclick={notify}>Send</button>
              </div>
            </div>
          </section>
        </section>
      {/if}

      {#if drawn('startup')}
        <section data-page="startup">
          {@render head('startup')}
          <section class="group">
            <div class="rows">
              <label class="row">
                <span class="row-label">Start with Windows<small>clip starts in the tray and records from sign in</small></span>
                <input type="checkbox" bind:checked={draft.startWithWindows} />
              </label>
              <label class="row">
                <span class="row-label">Open the window on start<small>Otherwise clip starts in the tray</small></span>
                <input type="checkbox" bind:checked={draft.openOnStart} />
              </label>
              <div class="row">
                <span class="row-label">Closing the window</span>
                <div class="choices" role="radiogroup" aria-label="Closing the window" use:radios>
                  {#each [[true, 'Hide to tray', 'Keeps recording'], [false, 'Quit', 'Stops recording']] as const as [tray, label, hint] (label)}
                    <button class="choice" role="radio" aria-checked={draft.closeToTray === tray} onclick={() => (draft!.closeToTray = tray)}>
                      <b>{label}</b><small>{hint}</small>
                    </button>
                  {/each}
                </div>
              </div>
            </div>
          </section>
        </section>
      {/if}

      {#if drawn('art')}
        <section data-page="art">
          {@render head('art')}
          <section class="group">
            <p class="form-hint">Steam games get art from Steam. For other games, paste an API key from the preferences of your SteamGridDB profile.</p>
            <div class="rows">
              <label class="row">
                <span class="row-label">SteamGridDB key</span>
                <input class="field" type="password" spellcheck="false" autocomplete="off" placeholder="None" bind:value={draft.steamgriddb} />
              </label>
            </div>
          </section>
        </section>
      {/if}
    </div>

    {#if changed || first || error}
      <div class="applybar">
        <span class="grow">
          {#if error}<span class="form-error" role="alert">{error}</span>
          {:else if first}clip is already recording with these. Change anything, then apply.
          {:else if restarts}Unsaved changes. Applying starts a new buffer, so what it holds now is dropped.
          {:else}Unsaved changes. They apply without touching the buffer.{/if}
        </span>
        {#if changed}<button class="gel" disabled={busy} onclick={revert}>Revert</button>{/if}
        <button class="gel default" disabled={(!changed && !first) || busy} onclick={apply}>Apply</button>
      </div>
    {/if}
  {/if}
</main>
