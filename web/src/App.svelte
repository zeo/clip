<script lang="ts">
  import { onMount, tick } from 'svelte';
  import { convertFileSrc, invoke } from '@tauri-apps/api/core';
  import { listen } from '@tauri-apps/api/event';
  import { getCurrentWebview } from '@tauri-apps/api/webview';
  import { getCurrentWindow } from '@tauri-apps/api/window';
  import { art } from './art.svelte';
  import ClipPage from './Clip.svelte';
  import Clips from './Clips.svelte';
  import Games from './Games.svelte';
  import Home from './Home.svelte';
  import Icon from './Icon.svelte';
  import Preferences from './Preferences.svelte';
  import { choose, chord, games, length, sentence, size, type Clip, type Disk, type Hotkey, type Mic, type Query, type Settings, type Status } from './types';
  import { cards, gone, radios, say, toasts, view, zooms } from './view.svelte';

  type Area = 'home' | 'library' | 'settings';
  type Shelf = Query['show'] | 'games';
  const areas: { id: Area; label: string; path: string }[] = [
    { id: 'home', label: 'Home', path: 'M3 9.5 10 3.5l7 6V17h-4.5v-4.5h-5V17H3z' },
    { id: 'library', label: 'Library', path: 'M3 4h5v5H3zM12 4h5v5h-5zM3 12h5v5H3zM12 12h5v5h-5z' },
    { id: 'settings', label: 'Settings', path: 'M3 5.5h14M3 10h14M3 14.5h14M7 3.5v4M13 8v4M9 12.5v4' }
  ];
  const shelves: [Query['show'], string][] = [
    ['all', 'All clips'],
    ['starred', 'Starred'],
    ['today', 'Today'],
    ['week', 'This week']
  ];

  const win = getCurrentWindow();
  let area = $state<Area>('home');
  let shelf = $state<Shelf>('games');
  let game = $state<string | null>(null);
  let opened = $state<string | null>(null);
  let page = $state('hotkeys');
  let status = $state<Status>({ recording: false, app: '', exe: '', message: 'starting', paused: false, hotkeys: [] });
  let settings = $state<Settings | null>(null);
  let found = $state<Clip[]>([]);
  const clips = $derived(found.filter((clip) => !gone.has(clip.path)));
  let disk = $state<Disk | null>(null);
  let mics = $state<Mic[]>([]);
  let first = $state(false);
  let maximized = $state(false);
  let quick = $state(false);
  let chip = $state<HTMLButtonElement>();
  let panel = $state<HTMLElement>();
  let sheet = $state<HTMLDialogElement>();
  let query = $state({ search: '' });
  const current = $derived(clips.find((clip) => clip.path === opened));
  // the list the clip was opened from, for stepping to the clip before or after it
  let siblings = $state<string[]>([]);
  const step = $derived.by(() => {
    const at = siblings.indexOf(opened ?? '');
    const near = (n: number) => (at < 0 ? undefined : clips.find((clip) => clip.path === siblings[at + n]));
    return { prev: near(-1), next: near(1) };
  });
  function open(clip: Clip, list?: Clip[]) {
    if (list) siblings = list.map((c) => c.path);
    opened = clip.path;
  }
  const listed = $derived(games(clips));
  const show = $derived(game || shelf === 'games' ? 'all' : shelf);
  const shown = $derived(choose(clips, { ...query, sort: view.sort, show }));
  const side = $derived(area === 'home' ? 'home' : 'library');
  const sided = $derived(!current && view.side[side]);
  const covers = $derived(area === 'library' && !game && shelf === 'games');
  const used = $derived(clips.reduce((sum, clip) => sum + clip.bytes, 0));
  const counts = $derived(Object.fromEntries(shelves.map(([id]) => [id, choose(clips, { show: id, sort: 'newest', search: '' }).length])));
  const failed = $derived(!status.recording && !status.paused && !['starting', 'waiting for a game'].includes(status.message));
  const hero = $derived(status.recording ? art(status.app)?.hero : null);
  const crumbs = $derived(
    area === 'home' ? ['Home'] : ['Library', game ?? (shelf === 'games' ? 'Games' : shelves.find(([id]) => id === shelf)?.[1] ?? '')]
  );
  // a game switched off stops being in focus for clip, so the panel keeps offering every one switched off
  const switchable = $derived(settings ? [...new Set([status.exe, ...settings.ignore])].filter(Boolean) : []);
  const cap = $derived(settings?.cap ? settings.cap * 1e9 : null);
  const room = $derived(cap ?? (disk ? used + disk.free : null));
  const does = (h: Hotkey) => (h.action === 'mark' ? 'Mark' : h.seconds ? `Save ${length(h.seconds)}` : 'Save');
  const sheetRows = $derived<[string, [string[], string][]][]>([
    [
      'Anywhere in clip',
      [
        [['1', '2', '3'], 'Home, Library, Settings'],
        [['Ctrl+F'], 'Search'],
        [['Ctrl+=', 'Ctrl+-'], 'Make everything bigger or smaller'],
        [['Ctrl+0'], 'Back to 100%'],
        [['Esc'], 'Back'],
        [['?'], 'This list']
      ]
    ],
    [
      'Clips',
      [
        [['Arrows', 'Home', 'End'], 'Move between clips'],
        [['Enter'], 'Open the clip'],
        [['Space', 'Ctrl+Click'], 'Select a clip'],
        [['Shift+Space', 'Shift+Click'], 'Select every clip up to it'],
        [['Ctrl+A'], 'Select all'],
        [['S'], 'Star or unstar'],
        [['Ctrl+C'], 'Copy the files to paste anywhere'],
        [['Del'], 'Delete, with a few seconds to undo'],
        [['Ctrl+Z'], 'Undo the delete'],
        [['Esc'], 'Clear the selection'],
        [['Ctrl+B'], 'Show or hide the sidebar'],
        [['Ctrl+Wheel'], 'Bigger or smaller cards']
      ]
    ],
    [
      'Playing a clip',
      [
        [['Space', 'K'], 'Play or pause'],
        [['J', 'L'], 'Back or ahead 5 seconds'],
        [['Left', 'Right', ',', '.'], 'Step a frame'],
        [['<', '>'], 'Slower or faster'],
        [['Up', 'Down'], 'Volume'],
        [['I', 'O'], 'Set where the cut starts and ends'],
        [['M'], 'Mark the moment'],
        [['Del'], 'Remove the mark in focus'],
        [['F'], 'Fill the screen'],
        [['Shift+P', 'Shift+N'], 'Previous or next clip'],
        [['Ctrl+C'], 'Copy the file to paste anywhere']
      ]
    ],
    [
      'In games',
      status.hotkeys
        .filter((h) => h.key)
        .map((h): [string[], string] => [[chord(h)], h.action === 'mark' ? 'Mark a moment, saved 15 s later' : `${does(h)} clip`])
    ]
  ]);

  $effect(() => {
    getCurrentWebview().setZoom(view.zoom / 100);
  });

  async function refresh() {
    [settings, found] = await Promise.all([invoke<Settings>('settings'), invoke<Clip[]>('clips')]);
    // a clip in the recycle bin stops being listed, so it no longer needs hiding
    for (const path of gone) if (!found.some((clip) => clip.path === path)) gone.delete(path);
    invoke<Disk>('disk', { folder: settings.folder }).then((d) => (disk = d), () => (disk = null));
  }

  onMount(() => {
    invoke<Status>('status').then((s) => (status = s));
    invoke<boolean>('first_run').then((f) => {
      first = f;
      if (f) go('settings');
    });
    refresh();
    const sized = () => win.isMaximized().then((m) => (maximized = m));
    sized();
    const stops = [
      listen<Status>('status', (e) => (status = e.payload)),
      listen('clips', refresh),
      listen<[string, boolean]>('saved', (e) => say(...e.payload)),
      win.onResized(sized)
    ];
    return () => stops.forEach((stop) => stop.then((f) => f()));
  });

  function go(next: Area) {
    area = next;
    game = null;
    opened = null;
    quick = false;
  }

  function browse(next: Shelf, app: string | null = null) {
    go('library');
    shelf = next;
    game = app;
  }

  function settle(next: string) {
    go('settings');
    page = next;
  }

  function back() {
    if (opened) opened = null;
    else game = null;
  }

  async function change(patch: Partial<Settings>) {
    if (!settings) return;
    await invoke('apply', { settings: { ...settings, ...patch } });
    await refresh();
  }

  function record(exe: string, on: boolean) {
    if (settings) change({ ignore: on ? settings.ignore.filter((e) => e !== exe) : [...settings.ignore, exe] });
  }

  async function openQuick() {
    quick = !quick;
    if (!quick) return;
    await tick();
    panel?.querySelector<HTMLElement>('input, select, button')?.focus();
    mics = await invoke<Mic[]>('mics').catch(() => []);
  }

  function closeQuick() {
    const inside = document.activeElement === document.body || panel?.contains(document.activeElement);
    quick = false;
    if (inside) chip?.focus();
  }

  function trap(e: KeyboardEvent) {
    if (e.key !== 'Tab' || !panel) return;
    const all = [...panel.querySelectorAll<HTMLElement>('input, select, button')];
    const edge = e.shiftKey ? all[0] : all[all.length - 1];
    if (document.activeElement !== edge) return;
    e.preventDefault();
    (e.shiftKey ? all[all.length - 1] : all[0]).focus();
  }

  function zoom(step: number | null) {
    const at = zooms.indexOf(view.zoom);
    view.zoom = step === null ? 100 : zooms[Math.min(zooms.length - 1, Math.max(0, at + step))];
    say(`Size ${view.zoom}%`);
  }

  function keydown(e: KeyboardEvent) {
    if (e.key === 'Escape' && quick) return closeQuick();
    if (e.ctrlKey && !e.altKey && !e.metaKey) {
      const step = ({ '=': 1, '+': 1, '-': -1, '0': null } as Record<string, number | null>)[e.key];
      if (step !== undefined) {
        e.preventDefault();
        return zoom(step);
      }
      if (e.key.toLowerCase() === 'f') {
        e.preventDefault();
        return document.querySelector<HTMLInputElement>('input.search')?.focus();
      }
      if (e.key.toLowerCase() === 'b' && area !== 'settings' && !current) {
        e.preventDefault();
        return (view.side[side] = !view.side[side]);
      }
      if (e.key.toLowerCase() === 'z' && !(e.target instanceof HTMLInputElement)) return toasts.filter((t) => t.undo).pop()?.undo?.();
    }
    if (e.target instanceof HTMLInputElement || e.target instanceof HTMLSelectElement || e.ctrlKey || e.altKey || e.metaKey) return;
    if (e.key === '?') return sheet?.showModal();
    const picked = areas[Number(e.key) - 1];
    if (picked) go(picked.id);
    // the clip page handles its own esc
    if (e.key === 'Escape' && !opened && game && !e.defaultPrevented) back();
  }

  async function applied() {
    first = false;
    await refresh();
  }
</script>

<div class="shell">
  <header class="bar">
    <span class="glyph" data-tauri-drag-region>
      <svg viewBox="0 0 18 18" aria-hidden="true">
        <rect x="1.5" y="1.5" width="15" height="15" rx="4" fill="none" stroke="#e6e6e6" stroke-width="1.2" />
        <circle cx="9" cy="9" r="3.4" fill="#e6e6e6" />
      </svg>
    </span>
    <div class="now-wrap">
      <button
        class="now"
        class:on={status.recording}
        class:err={failed}
        bind:this={chip}
        aria-haspopup="dialog"
        aria-expanded={quick}
        title={sentence(status.message)}
        onclick={openQuick}
      >
        {#if hero}<img src={convertFileSrc(hero)} alt="" draggable="false" />{/if}
        <span class="dot"></span>
        <span class="now-text">
          {#if status.recording}Recording <b>{status.app}</b>{:else if status.paused}Paused{:else}{sentence(status.message)}{/if}
        </span>
        <svg class="caret" viewBox="0 0 10 10" aria-hidden="true"><path d="M2 3.5 5 6.5l3-3" /></svg>
      </button>
      {#if quick && settings}
        <div class="quick" role="dialog" aria-label="Recording" tabindex="-1" bind:this={panel} onkeydown={trap}>
          <label class="quick-row">
            <span class="grow">Recording<small>{status.paused ? 'Paused until switched back on' : 'Follows the game in focus'}</small></span>
            <input type="checkbox" class="switch" checked={!status.paused} onchange={(e) => invoke('pause', { on: !e.currentTarget.checked })} />
          </label>
          {#each switchable as exe (exe)}
            <label class="quick-row">
              <span class="grow">Record {exe === status.exe && status.app ? status.app : exe}<small class="mono">{exe}</small></span>
              <input type="checkbox" class="switch" checked={!settings.ignore.includes(exe)} onchange={(e) => record(exe, e.currentTarget.checked)} />
            </label>
          {/each}
          <button class="quick-row" onclick={() => settle('quality')}>
            <span class="grow">Quality<small class="mono">
                {settings.height >= 4320 ? 'native' : `${settings.height}p`}{settings.fps}, {settings.mbps} Mbps, last {length(settings.buffer)}
              </small></span>
            <span class="go">›</span>
          </button>
          <label class="quick-row">
            <span class="grow">Microphone</span>
            <select
              class="field"
              value={settings.mic ? settings.micDevice || 'default' : 'off'}
              onchange={(e) => {
                const v = e.currentTarget.value;
                change({ mic: v !== 'off', micDevice: v === 'off' ? settings!.micDevice : v === 'default' ? '' : v });
              }}
            >
              <option value="off">Off</option>
              <option value="default">Default device</option>
              {#each mics as mic (mic.id)}<option value={mic.id}>{mic.name}</option>{/each}
            </select>
          </label>
          <button class="quick-row" onclick={() => settle('audio')}>
            <span class="grow">Audio<small>Tracks, volumes and push to talk</small></span>
            <span class="go">›</span>
          </button>
        </div>
      {/if}
    </div>
    <div class="hotkeys" data-tauri-drag-region>
      {#each status.hotkeys.filter((h) => h.key) as hotkey, i (i)}
        <button
          class="hotkey"
          class:bad={hotkey.taken}
          title={hotkey.taken ? 'Another app holds this key, pick another' : 'Change hotkeys'}
          onclick={() => settle('hotkeys')}
        >
          <kbd>{chord(hotkey)}</kbd>{does(hotkey)}
        </button>
      {/each}
    </div>
    <span class="grow" data-tauri-drag-region></span>
    <div class="bar-actions">
      <button class="gel" disabled={!status.recording} onclick={() => invoke('mark_clip')} title="Marks now and saves 15 s later">Mark</button>
      <button class="gel default" disabled={!status.recording} onclick={() => invoke('save_clip')}>Save clip</button>
    </div>
    <div class="controls">
      <button aria-label="Minimize" onclick={() => win.minimize()}>
        <svg viewBox="0 0 10 10" aria-hidden="true"><path d="M0 5.5h10" /></svg>
      </button>
      <button aria-label={maximized ? 'Restore' : 'Maximize'} onclick={() => win.toggleMaximize()}>
        {#if maximized}
          <svg viewBox="0 0 10 10" aria-hidden="true"><path d="M2.5 2.5V.5h7v7h-2M.5 2.5h7v7h-7z" /></svg>
        {:else}
          <svg viewBox="0 0 10 10" aria-hidden="true"><path d="M.5.5h9v9h-9z" /></svg>
        {/if}
      </button>
      <button class="close" aria-label="Close" onclick={() => win.close()}>
        <svg viewBox="0 0 10 10" aria-hidden="true"><path d="M.5.5l9 9M9.5.5l-9 9" /></svg>
      </button>
    </div>
  </header>

  <nav class="rail" aria-label="Areas">
    {#each areas as a, i (a.id)}
      <button class="rail-item" aria-current={area === a.id ? 'page' : undefined} title="{a.label} ({i + 1})" onclick={() => go(a.id)}>
        <svg viewBox="0 0 20 20" aria-hidden="true"><path d={a.path} /></svg>
        <span>{a.label}</span>
      </button>
    {/each}
  </nav>

  {#if area === 'settings'}
    <Preferences {first} {used} {status} count={clips.length} bind:page onapplied={applied} />
  {:else}
    {#if sided}<aside class="side">
      <div class="side-list">
        <button class="side-item" aria-current={area === 'library' && !game && shelf === 'games' ? 'page' : undefined} onclick={() => browse('games')}>
          <span class="grow">Games</span><span class="count">{listed.length}</span>
        </button>
        {#each shelves as [id, label] (id)}
          <button class="side-item" aria-current={area === 'library' && !game && shelf === id ? 'page' : undefined} onclick={() => browse(id)}>
            <span class="grow">{label}</span><span class="count">{counts[id]}</span>
          </button>
        {/each}
        {#if listed.length}<h2 class="side-head">Games</h2>{/if}
        {#each listed as g (g.app)}
          <button class="side-item side-game" aria-current={area === 'library' && game === g.app ? 'page' : undefined} onclick={() => browse('games', g.app)}>
            <Icon app={g.app} icon={g.icon} size={20} />
            <span class="grow">{g.app}</span><span class="count">{g.clips.length}</span>
          </button>
        {/each}
      </div>
      <button class="storage" title="Change storage" onclick={() => settle('clips')}>
        <span class="storage-text">
          <span>Storage</span>
          <span class="mono">{size(used)}{cap ? ` of ${settings?.cap} GB` : disk ? `, ${size(disk.free)} free` : ''}</span>
        </span>
        {#if room}<span class="meter"><span style:width="{Math.min(100, (used / room) * 100)}%"></span></span>{/if}
      </button>
    </aside>{/if}

    <main class="stage" class:wide={!sided}>
      {#if current}
        {#key current.path}
          <ClipPage
            clip={current}
            prev={step.prev}
            next={step.next}
            onopen={open}
            onclose={() => (opened = null)}
            onmoved={async (to) => {
              siblings = siblings.map((path) => (path === opened ? to : path));
              await refresh();
              opened = to;
            }}
          />
        {/key}
      {:else}
        <div class="toolbar">
          <button
            class="gel icon"
            aria-pressed={view.side[side]}
            aria-label="Sidebar"
            title="{view.side[side] ? 'Hide' : 'Show'} the sidebar (Ctrl+B)"
            onclick={() => (view.side[side] = !view.side[side])}
          >
            <svg viewBox="0 0 16 16" aria-hidden="true"><path d="M2.5 3.5h11v9h-11zM6 3.5v9" /></svg>
          </button>
          {#if game}
            <button class="gel back" title="Back (Esc)" onclick={back}>
              <svg viewBox="0 0 10 10" aria-hidden="true"><path d="M6.5 1.5 3 5l3.5 3.5" /></svg><span>Back</span>
            </button>
          {/if}
          <nav class="crumbs" aria-label="Where">
            {#each crumbs as crumb, i (i)}
              {#if i > 0}<span class="sep">›</span>{/if}
              <span class:here={i === crumbs.length - 1}>{crumb}</span>
            {/each}
          </nav>
          <span class="grow"></span>
          {#if !covers}
            {#if !view.list}
              <input
                class="size"
                type="range"
                min="0"
                max={cards.length - 1}
                step="1"
                aria-label="Card size"
                aria-valuetext="{['Smallest', 'Small', 'Large', 'Largest'][cards.indexOf(view.card)]} cards"
                title="Card size (Ctrl+Wheel)"
                value={Math.max(0, cards.indexOf(view.card))}
                oninput={(e) => (view.card = cards[Number(e.currentTarget.value)])}
              />
            {/if}
            <div class="seg" role="radiogroup" aria-label="Show clips as" use:radios>
              <button role="radio" aria-checked={!view.list} aria-label="Grid" title="Grid" onclick={() => (view.list = false)}>
                <svg viewBox="0 0 16 16" aria-hidden="true"><path d="M2.5 2.5h4.5v4.5h-4.5zM9 2.5h4.5v4.5H9zM2.5 9h4.5v4.5h-4.5zM9 9h4.5v4.5H9z" /></svg>
              </button>
              <button role="radio" aria-checked={view.list} aria-label="List" title="List" onclick={() => (view.list = true)}>
                <svg viewBox="0 0 16 16" aria-hidden="true"><path d="M2.5 4h11M2.5 8h11M2.5 12h11" /></svg>
              </button>
            </div>
          {/if}
          <select class="field" aria-label="Sort" bind:value={view.sort}>
            <option value="newest">Newest first</option>
            <option value="oldest">Oldest first</option>
            <option value="longest">Longest first</option>
            <option value="largest">Largest first</option>
            <option value="name">Name</option>
            <option value="game">Game</option>
          </select>
          <input class="field search" type="search" placeholder="Search" aria-label="Search clips and games" bind:value={query.search} />
        </div>
        {#if area === 'home'}
          <Home {clips} {shown} {status} grouped={view.sort === 'newest'} buffer={settings?.buffer ?? 30} onopen={open} />
        {:else if game || shelf === 'games'}
          <Games {clips} {shown} app={game} grouped={view.sort === 'newest'} onpick={(app) => browse('games', app)} onopen={open} />
        {:else}
          <div class="view">
            {#if shown.length === 0}
              <p class="empty">
                {query.search ? 'No clips match.' : shelf === 'starred' ? 'Star a clip and it stays here. The storage limit never deletes it.' : 'No clips here yet.'}
              </p>
            {:else}
              <Clips clips={shown} grouped={view.sort === 'newest'} onopen={open} />
            {/if}
          </div>
        {/if}
      {/if}
    </main>
  {/if}
</div>

<div class="toasts" role="status">
  {#each toasts as toast (toast.id)}
    <p class="toast" class:bad={toast.bad}>
      {toast.text}{#if toast.undo}<button class="gel" onclick={toast.undo}>Undo <kbd>Ctrl+Z</kbd></button>{/if}
    </p>
  {/each}
</div>

<!-- keys stop at the sheet, so the page behind never takes them, and a click on the backdrop closes it -->
<!-- svelte-ignore a11y_no_noninteractive_element_interactions, a11y_click_events_have_key_events -->
<dialog
  class="sheet"
  bind:this={sheet}
  aria-labelledby="sheet-title"
  onkeydown={(e) => e.stopPropagation()}
  onclick={(e) => e.target === sheet && sheet.close()}
>
  <div class="sheet-body">
    <header class="sheet-head">
      <h2 id="sheet-title">Keys</h2>
      <button class="gel" onclick={() => sheet?.close()}>Close <kbd>Esc</kbd></button>
    </header>
    {#each sheetRows as [name, rows] (name)}
      {#if rows.length}
        <section>
          <h3 class="side-head">{name}</h3>
          <dl class="sheet-rows">
            {#each rows as [keys, what], n (n)}
              <div>
                <dt>{#each keys as key, i (key)}{#if i > 0}<span class="mute"> or </span>{/if}<kbd class="key">{key}</kbd>{/each}</dt>
                <dd>{what}</dd>
              </div>
            {/each}
          </dl>
        </section>
      {/if}
    {/each}
  </div>
</dialog>

<svelte:window
  onclick={(e) => {
    if (quick && e.target instanceof Element && !e.target.closest('.now-wrap')) closeQuick();
  }}
  onkeydown={keydown}
/>
