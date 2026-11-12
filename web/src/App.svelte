<script lang="ts">
  import { onMount, tick } from 'svelte';
  import { convertFileSrc, invoke } from '@tauri-apps/api/core';
  import { listen } from '@tauri-apps/api/event';
  import { getCurrentWebview } from '@tauri-apps/api/webview';
  import { getCurrentWindow } from '@tauri-apps/api/window';
  import appIcon from '../../icons/icon.png';
  import { art } from './art.svelte';
  import ClipPage from './Clip.svelte';
  import Shot from './Shot.svelte';
  import Clips from './Clips.svelte';
  import Games from './Games.svelte';
  import Home from './Home.svelte';
  import Icon from './Icon.svelte';
  import Preferences from './Preferences.svelte';
  import Tour from './Tour.svelte';
  import { ACTIONS, choose, chord, does, games, GB, length, quality, sentence, size, type Clip, type Disk, type Hotkey, type Mic, type Query, type Settings, type Status } from './types';
  import { cards, gone, radios, say, stars, toasts, view, zooms } from './view.svelte';

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
    ['week', 'Last 7 days'],
    ['images', 'Screenshots and GIFs']
  ];

  const win = getCurrentWindow();
  let area = $state<Area>('home');
  let shelf = $state<Shelf>('games');
  let game = $state<string | null>(null);
  let opened = $state<string | null>(null);
  // a whole buffer saved to be cut down, open until it's kept, cut or thrown away
  let rewound = $state<string | null>(null);
  let page = $state('hotkeys');
  let status = $state<Status>({ recording: false, app: '', exe: '', message: 'starting', paused: false, hotkeys: [], mic_muted: false, taping: 0, on_cpu: false, summary: '' });
  let settings = $state<Settings | null>(null);
  let found = $state<Clip[]>([]);
  // an empty library only says so once the clips have been read
  let loaded = $state(false);
  let refreshes = 0;
  const clips = $derived(
    found
      .filter((clip) => !gone.has(clip.path))
      .map((clip) => (stars.has(clip.path) && stars.get(clip.path) !== clip.starred ? { ...clip, starred: stars.get(clip.path)! } : clip))
  );
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
  // what's in the clips folder, which the storage limit and the meter are about
  const ours = $derived(clips.filter((clip) => !clip.source));
  const used = $derived(ours.reduce((sum, clip) => sum + clip.bytes, 0));
  const counts = $derived(Object.fromEntries(shelves.map(([id]) => [id, choose(clips, { show: id, sort: 'newest', search: '' }).length])));
  const failed = $derived(!status.recording && !status.paused && !['starting', 'waiting for a game'].includes(status.message));
  const hero = $derived(status.recording ? art(status.app)?.hero : null);
  const crumbs = $derived(
    area === 'home' ? ['Home'] : ['Library', game ?? (shelf === 'games' ? 'Games' : shelves.find(([id]) => id === shelf)?.[1] ?? '')]
  );
  // a game switched off stops being in focus for clip, so the panel keeps offering every one switched off
  const switchable = $derived(settings ? [...new Set([status.exe, ...settings.ignore])].filter(Boolean) : []);
  const cap = $derived(settings?.cap ? settings.cap * GB : null);
  const room = $derived(cap ?? (disk ? used + disk.free : null));
  const short = (h: Hotkey) => (h.action === 'save' && h.seconds ? `Save ${length(h.seconds)}` : ACTIONS[h.action].key);
  let now = $state(Date.now());
  $effect(() => {
    if (!status.taping) return;
    now = Date.now();
    const timer = setInterval(() => (now = Date.now()), 1000);
    return () => clearInterval(timer);
  });
  let cost = $state<{ cpu: number | null; memory: number; encoder: number | null } | null>(null);
  $effect(() => {
    if (!quick) return;
    const read = () => invoke<typeof cost>('cost').then((c) => (cost = c), () => (cost = null));
    read();
    const timer = setInterval(read, 2000);
    return () => clearInterval(timer);
  });
  const spent = $derived(
    cost &&
      [cost.cpu !== null && `${cost.cpu.toFixed(1)}% of the CPU`, `${size(cost.memory)} of memory`, cost.encoder !== null && `${Math.round(cost.encoder)}% of the video encoder`]
        .filter(Boolean)
        .join(', ')
        .replace(/, ([^,]*)$/, ' and $1')
  );
  const taped = $derived.by(() => {
    const s = Math.max(0, Math.floor((now - status.taping) / 1000));
    return `${Math.floor(s / 60)}:${String(s % 60).padStart(2, '0')}`;
  });
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
        [['Page Up', 'Page Down'], 'Move a screen of clips'],
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
        [['S'], 'Split the cut at the playhead'],
        [['Del'], 'Cut out the part the handles are on'],
        [['M'], 'Mark the moment'],
        [['Del'], 'Remove the mark in focus'],
        [['F'], 'Fill the screen'],
        [['Shift+P', 'Shift+N'], 'Previous or next clip'],
        [['Ctrl+C'], 'Copy the file to paste anywhere']
      ]
    ],
    [
      'Viewing a screenshot',
      [
        [['Left', 'Right'], 'Previous or next'],
        [['S'], 'Star or unstar'],
        [['Del'], 'Delete, with a few seconds to undo'],
        [['Ctrl+C'], 'Copy the file to paste anywhere'],
        [['Esc'], 'Back']
      ]
    ],
    [
      'In games',
      status.hotkeys
        .filter((h) => h.key)
        .map((h): [string[], string] => [[chord(h)], h.action === 'save' ? `${short(h)} clip` : does(h.action, settings?.buffer ?? 30)])
    ]
  ]);

  $effect(() => {
    getCurrentWebview().setZoom(view.zoom / 100);
  });

  async function refresh() {
    const ticket = ++refreshes;
    try {
      const read = await Promise.all([invoke<Settings>('settings'), invoke<Clip[]>('clips')]);
      // a refresh that started later may have finished first, and its list is the newer one
      if (ticket !== refreshes) return;
      [settings, found] = read;
    } catch (e) {
      if (ticket === refreshes) say(`Can't read the clips: ${e}`, true);
      return;
    } finally {
      loaded = true;
    }
    // a clip in the recycle bin stops being listed, so it no longer needs hiding
    for (const path of gone) if (!found.some((clip) => clip.path === path)) gone.delete(path);
    for (const [path, on] of stars) if ((found.find((clip) => clip.path === path)?.starred ?? on) === on) stars.delete(path);
    invoke<Disk>('disk', { folder: settings.folder }).then((d) => (disk = d), () => (disk = null));
  }

  onMount(() => {
    invoke<Status>('status').then((s) => (status = s));
    invoke<boolean>('first_run').then((f) => (first = f));
    invoke<string[]>('damaged').then((all) => all.forEach((text) => say(text, true)));
    refresh();
    const sized = () => win.isMaximized().then((m) => (maximized = m));
    sized();
    const stops = [
      listen<Status>('status', (e) => (status = e.payload)),
      listen('clips', refresh),
      listen<[string, boolean]>('saved', (e) => say(...e.payload)),
      listen<string>('rewind', (e) => {
        go('library');
        opened = rewound = e.payload;
      }),
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
    if (opened) close();
    else game = null;
  }

  // back on the list, the clip that was open keeps the focus
  async function close() {
    const was = opened;
    opened = null;
    await tick();
    if (was) document.querySelector<HTMLElement>(`.view [data-path="${CSS.escape(was)}"]`)?.focus();
  }

  let changing = Promise.resolve();

  // switches apply one after another, each onto what the one before left, and one that failed to
  // apply goes back to what's in effect
  function change(patch: (now: Settings) => Partial<Settings>, undo: () => void) {
    changing = changing.then(async () => {
      if (!settings) return;
      try {
        await invoke('apply', { settings: { ...settings, ...patch(settings) } });
      } catch (e) {
        say(String(e), true);
        undo();
      }
      await refresh();
    });
  }

  function record(exe: string, box: HTMLInputElement) {
    const on = box.checked;
    change((now) => ({ ignore: on ? now.ignore.filter((e) => e !== exe) : [...now.ignore.filter((e) => e !== exe), exe] }), () => (box.checked = !on));
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
      <img src={appIcon} alt="" draggable="false" />
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
        {#if status.taping}<span class="tape mono">REC {taped}</span>{/if}
        {#if status.on_cpu}<span class="cpu mono" title="No hardware video encoder was found, so the CPU records at 720p and 30 fps">CPU</span>{/if}
        {#if status.mic_muted}
          <svg class="muted" viewBox="0 0 20 20" role="img" aria-label="Mic muted">
            <path d="M10 3a2.5 2.5 0 0 0-2.5 2.5v4a2.5 2.5 0 0 0 5 0v-4A2.5 2.5 0 0 0 10 3zM5.5 9.5a4.5 4.5 0 0 0 9 0M10 14v3M4 4l12 12" />
          </svg>
        {/if}
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
              <input type="checkbox" class="switch" checked={!settings.ignore.includes(exe)} onchange={(e) => record(exe, e.currentTarget)} />
            </label>
          {/each}
          <div class="quick-row">
            <span class="grow">Long recording<small>{status.taping ? `Recording for ${taped}` : 'Records until stopped, as long as you play'}</small></span>
            <button class="gel" class:default={!!status.taping} disabled={!status.taping && !status.recording} onclick={() => invoke('toggle_recording')}>
              {status.taping ? 'Stop' : 'Start'}
            </button>
          </div>
          <div class="quick-row">
            <span class="grow">Rewind<small>Keep just a part of the buffer</small></span>
            <button class="gel" disabled={!status.recording} onclick={() => ((quick = false), invoke('rewind'))}>Rewind</button>
          </div>
          <div class="quick-row">
            <span class="grow">Screenshot<small>Saved as a png next to the clips</small></span>
            <button class="gel" disabled={!status.recording} onclick={() => invoke('take_shot')}>Take</button>
          </div>
          <button class="quick-row" onclick={() => settle('quality')}>
            <span class="grow">Quality<small>
                {status.on_cpu ? 'On the CPU, 720p, 30 fps' : quality(settings)}, last {length(settings.buffer)}
              </small></span>
            <span class="go">›</span>
          </button>
          <label class="quick-row">
            <span class="grow">Microphone</span>
            <select
              class="field"
              value={settings.mic ? settings.micDevice || 'default' : 'off'}
              onchange={(e) => {
                const box = e.currentTarget;
                const v = box.value;
                const was = settings!.mic ? settings!.micDevice || 'default' : 'off';
                change((now) => ({ mic: v !== 'off', micDevice: v === 'off' ? now.micDevice : v === 'default' ? '' : v }), () => (box.value = was));
              }}
            >
              <option value="off">Off</option>
              <option value="default">Default device</option>
              {#each mics as mic (mic.id)}<option value={mic.id}>{mic.name}</option>{/each}
            </select>
          </label>
          {#if settings.mic}
            <label class="quick-row">
              <span class="grow">Mute mic<small>{status.mic_muted ? 'Clips get silence from the mic' : 'Clips hear the mic'}</small></span>
              <input type="checkbox" class="switch" checked={status.mic_muted} onchange={() => invoke('toggle_mic')} />
            </label>
          {/if}
          <button class="quick-row" onclick={() => settle('audio')}>
            <span class="grow">Audio<small>Tracks, volumes and push to talk</small></span>
            <span class="go">›</span>
          </button>
          {#if cost}
            <p class="quick-cost">clip uses {spent}</p>
          {/if}
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
          <kbd>{chord(hotkey)}</kbd>{short(hotkey)}
        </button>
      {/each}
    </div>
    <span class="grow" data-tauri-drag-region></span>
    <div class="bar-actions">
      <button class="gel" disabled={!status.recording} onclick={() => invoke('mark_clip')} title={does('mark', settings?.buffer ?? 30)}>Mark</button>
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
    <Preferences {first} {used} {status} count={ours.filter((clip) => clip.kind === 'clip').length} bind:page onapplied={applied} />
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
          {#if current.kind === 'image'}
            <Shot clip={current} prev={step.prev} next={step.next} onopen={open} onclose={close} />
          {:else}
          <ClipPage
            clip={current}
            rewind={current.path === rewound}
            prev={step.prev}
            next={step.next}
            onopen={open}
            onclose={close}
            onmoved={async (to) => {
              siblings = siblings.map((path) => (path === opened ? to : path));
              await refresh();
              opened = to;
            }}
          />
          {/if}
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
                style:--pct={Math.max(0, cards.indexOf(view.card)) / (cards.length - 1)}
                style:--steps={cards.length - 1}
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
        {#if !loaded}
          <div class="view"></div>
        {:else if area === 'home'}
          <Home clips={clips.filter((clip) => clip.kind === 'clip')} {shown} {status} grouped={view.sort === 'newest'} buffer={settings?.buffer ?? 30} onopen={open} onkeys={() => settle('hotkeys')} />
        {:else if game || shelf === 'games'}
          <Games {clips} {shown} app={game} grouped={view.sort === 'newest'} onpick={(app) => browse('games', app)} onopen={open} />
        {:else}
          <div class="view">
            {#if shown.length === 0}
              <p class="empty">
                {query.search
                  ? 'No clips match.'
                  : shelf === 'starred'
                    ? 'Star a clip and it stays here. The storage limit never deletes it.'
                    : shelf === 'images'
                      ? 'Screenshots from the screenshot key, and frames and GIFs saved from clips, show up here.'
                      : shelf === 'today'
                        ? 'Clips saved since midnight show up here.'
                        : shelf === 'week'
                          ? 'Clips saved in the last 7 days show up here. Older ones are under All clips.'
                          : 'No clips here yet.'}
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

{#if first}<Tour ondone={applied} />{/if}

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
