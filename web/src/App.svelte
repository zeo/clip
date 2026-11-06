<script lang="ts">
  import { onMount } from 'svelte';
  import { invoke } from '@tauri-apps/api/core';
  import { listen } from '@tauri-apps/api/event';
  import { getCurrentWindow } from '@tauri-apps/api/window';
  import ClipPage from './Clip.svelte';
  import Clips from './Clips.svelte';
  import Games from './Games.svelte';
  import Home from './Home.svelte';
  import Preferences from './Preferences.svelte';
  import { games, size, title, type Clip, type Settings, type Status } from './types';

  type Page = 'home' | 'games' | 'starred' | 'settings';
  const pages: { id: Page; icon: string }[] = [
    { id: 'home', icon: 'M3.5 9.2 10 3.5l6.5 5.7V16.5h-4.25v-4.5h-4.5v4.5H3.5z' },
    { id: 'games', icon: 'M3.5 3.5h5v5h-5zM11.5 3.5h5v5h-5zM3.5 11.5h5v5h-5zM11.5 11.5h5v5h-5z' },
    { id: 'starred', icon: 'M10 2.8l2.2 4.6 5 .6-3.7 3.5 1 5L10 14l-4.5 2.5 1-5L2.8 8l5-.6z' },
    { id: 'settings', icon: 'M3 6h7.5M14.5 6H17M3 14h2.5M9.5 14H17M12.5 4v4M7.5 12v4' }
  ];

  const win = getCurrentWindow();
  let page = $state<Page>('home');
  let game = $state<string | null>(null);
  let opened = $state<string | null>(null);
  let status = $state<Status>({ recording: false, app: '', message: 'starting' });
  let settings = $state<Settings | null>(null);
  let clips = $state<Clip[]>([]);
  let first = $state(false);
  const current = $derived(clips.find((clip) => clip.path === opened));
  const starred = $derived(clips.filter((clip) => clip.starred));
  const used = $derived(clips.reduce((sum, clip) => sum + clip.bytes, 0));
  const counts = $derived<Record<Page, number | null>>({
    home: null,
    games: games(clips).length,
    starred: starred.length,
    settings: null
  });
  const crumbs = $derived([page, ...(page === 'games' && game ? [game] : []), ...(current ? [title(current)] : [])]);
  const failed = $derived(!status.recording && !['starting', 'waiting for a game'].includes(status.message));

  async function refresh() {
    [settings, clips] = await Promise.all([invoke<Settings>('settings'), invoke<Clip[]>('clips')]);
  }

  onMount(() => {
    invoke<Status>('status').then((s) => (status = s));
    invoke<boolean>('first_run').then((f) => {
      first = f;
      if (f) page = 'settings';
    });
    refresh();
    const stops = [listen<Status>('status', (e) => (status = e.payload)), listen('clips', refresh)];
    return () => stops.forEach((stop) => stop.then((f) => f()));
  });

  function go(next: Page) {
    page = next;
    game = null;
    opened = null;
  }

  function back() {
    if (opened) opened = null;
    else game = null;
  }

  function showGame(app: string) {
    page = 'games';
    game = app;
    opened = null;
  }

  async function applied() {
    first = false;
    await refresh();
  }
</script>

<div class="shell">
  <div class="brand" data-tauri-drag-region><span class="brand-mark" data-tauri-drag-region>memory</span></div>

  <header class="topbar" data-tauri-drag-region>
    {#if opened || game}
      <button class="back" aria-label="back" title="back (esc)" onclick={back}>
        <svg viewBox="0 0 12 12"><path d="M7.5 2 3.5 6l4 4" /></svg>
      </button>
    {/if}
    <nav class="crumbs" aria-label="where" data-tauri-drag-region>
      {#each crumbs as crumb, i (i)}
        {#if i > 0}<span class="path-sep" data-tauri-drag-region>/</span>{/if}
        <span class:proper={i > 0} class:here={i === crumbs.length - 1} data-tauri-drag-region>{crumb}</span>
      {/each}
    </nav>
    <span class="grow" data-tauri-drag-region></span>
    <div class="pill" class:live={status.recording} class:err={failed} title={status.message}>
      <span class="pill-dot"></span>
      {#if status.recording}
        <span class="pill-state">recording</span>
        <span class="pill-app proper">{status.app}</span>
        <span class="pill-meta mono">last {settings?.buffer ?? 30} s</span>
      {:else}
        <span class="pill-state">{status.message}</span>
      {/if}
    </div>
    <button class="btn" disabled={!status.recording} onclick={() => invoke('mark_clip')} title="marks now, saves 15 s later">
      mark <kbd>alt+f10</kbd>
    </button>
    <button class="btn primary" disabled={!status.recording} onclick={() => invoke('save_clip')}>
      save clip <kbd>alt+f9</kbd>
    </button>
    <div class="window-controls">
      <button aria-label="minimize" onclick={() => win.minimize()}>
        <svg viewBox="0 0 10 10"><path d="M0 5.5h10" /></svg>
      </button>
      <button aria-label="maximize" onclick={() => win.toggleMaximize()}>
        <svg viewBox="0 0 10 10"><rect x="0.5" y="0.5" width="9" height="9" /></svg>
      </button>
      <button class="close" aria-label="close" onclick={() => win.close()}>
        <svg viewBox="0 0 10 10"><path d="M0 0l10 10M10 0L0 10" /></svg>
      </button>
    </div>
  </header>

  <nav class="rail" aria-label="pages">
    {#each pages as p, i (p.id)}
      <button class="rail-item" aria-current={page === p.id ? 'page' : undefined} title="{p.id} ({i + 1})" onclick={() => go(p.id)}>
        <svg viewBox="0 0 20 20"><path d={p.icon} /></svg>
        <span>{p.id}</span>
        {#if counts[p.id]}<span class="rail-count mono">{counts[p.id]}</span>{/if}
      </button>
    {/each}
    <span class="grow"></span>
    {#if settings}
      <div class="rail-foot mono" title="{settings.folder}, {clips.length} clips">
        <span class="proper">{size(used)}</span>
        {#if settings.cap > 0}<span class="rail-cap">of {settings.cap} <span class="proper">GB</span></span>{/if}
        {#if settings.cap > 0}<span class="meter"><span style:width="{Math.min(100, used / (settings.cap * 1e7))}%"></span></span>{/if}
      </div>
    {/if}
  </nav>

  <main class="stage">
    {#if current}
      {#key current.path}
        <ClipPage
          clip={current}
          onclose={() => (opened = null)}
          onmoved={async (to) => {
            await refresh();
            opened = to;
          }}
        />
      {/key}
    {:else if page === 'home'}
      <Home {clips} {status} buffer={settings?.buffer ?? 30} onopen={(clip) => (opened = clip.path)} ongame={showGame} onall={() => go('games')} />
    {:else if page === 'games'}
      <Games {clips} app={game} onpick={showGame} onopen={(clip) => (opened = clip.path)} />
    {:else if page === 'starred'}
      <div class="view">
        <div class="view-head">
          <h1>starred</h1>
          <p class="view-sub">{starred.length} {starred.length === 1 ? 'clip' : 'clips'}, kept past the storage limit</p>
        </div>
        {#if starred.length === 0}<p class="empty">star a clip and it stays here, and the storage limit never deletes it</p>{/if}
        <Clips clips={starred} onopen={(clip) => (opened = clip.path)} />
      </div>
    {:else}
      <Preferences {first} {used} count={clips.length} onapplied={applied} />
    {/if}
  </main>
</div>

<svelte:window
  onkeydown={(e) => {
    if (e.target instanceof HTMLInputElement || e.target instanceof HTMLSelectElement || e.ctrlKey || e.altKey || e.metaKey) return;
    const picked = pages[Number(e.key) - 1];
    if (picked) go(picked.id);
    // the clip page handles its own esc
    if (e.key === 'Escape' && !opened && game && !e.defaultPrevented) back();
  }}
/>
