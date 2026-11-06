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
  import { choose, games, sentence, type Clip, type Query, type Settings, type Status } from './types';

  type Page = 'home' | 'games' | 'starred' | 'settings';
  const pages: { id: Page; label: string }[] = [
    { id: 'home', label: 'Home' },
    { id: 'games', label: 'Games' },
    { id: 'starred', label: 'Starred' },
    { id: 'settings', label: 'Settings' }
  ];
  const shows: [Query['show'], string][] = [
    ['all', 'All'],
    ['starred', 'Starred'],
    ['today', 'Today'],
    ['week', 'This week']
  ];

  const win = getCurrentWindow();
  let page = $state<Page>('home');
  let game = $state<string | null>(null);
  let opened = $state<string | null>(null);
  let status = $state<Status>({ recording: false, app: '', message: 'starting' });
  let settings = $state<Settings | null>(null);
  let clips = $state<Clip[]>([]);
  let first = $state(false);
  let maximized = $state(false);
  let query = $state<Query>({ show: 'all', sort: 'newest', search: '' });
  const current = $derived(clips.find((clip) => clip.path === opened));
  const starred = $derived(clips.filter((clip) => clip.starred));
  const shown = $derived(choose(page === 'starred' ? starred : clips, query));
  const used = $derived(clips.reduce((sum, clip) => sum + clip.bytes, 0));
  const counts = $derived<Record<Page, number | null>>({
    home: null,
    games: games(clips).length,
    starred: starred.length,
    settings: null
  });
  const crumbs = $derived([pages.find((p) => p.id === page)?.label ?? '', ...(page === 'games' && game ? [game] : [])]);
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
    const sized = () => win.isMaximized().then((m) => (maximized = m));
    sized();
    const stops = [listen<Status>('status', (e) => (status = e.payload)), listen('clips', refresh), win.onResized(sized)];
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
  <header class="bar">
    <span class="glyph" data-tauri-drag-region>
      <svg viewBox="0 0 18 18" aria-hidden="true">
        <rect x="1.5" y="1.5" width="15" height="15" rx="4" fill="none" stroke="#e6e6e6" stroke-width="1.2" />
        <circle cx="9" cy="9" r="3.4" fill="#e6e6e6" />
      </svg>
    </span>
    {#each pages as p, i (p.id)}
      <button class="tab" aria-current={page === p.id ? 'page' : undefined} title="{p.label} ({i + 1})" onclick={() => go(p.id)}>
        {p.label}{#if counts[p.id]}<span class="count">{counts[p.id]}</span>{/if}
      </button>
    {/each}
    <div class="live" class:on={status.recording} class:err={failed} title={sentence(status.message)} data-tauri-drag-region>
      <span class="dot"></span>
      {#if status.recording}
        <span>Recording <span class="live-app">{status.app}</span>, keeping the last <span class="mono">{settings?.buffer ?? 30} s</span></span>
      {:else}
        <span>{sentence(status.message)}</span>
      {/if}
    </div>
    <div class="bar-actions">
      <button class="gel" disabled={!status.recording} onclick={() => invoke('mark_clip')} title="Marks now and saves 15 s later">
        Mark <kbd>Alt+F10</kbd>
      </button>
      <button class="gel default" disabled={!status.recording} onclick={() => invoke('save_clip')}>
        Save clip <kbd>Alt+F9</kbd>
      </button>
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

  {#if !current && page !== 'settings'}
    <div class="toolbar">
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
      <div class="seg" role="radiogroup" aria-label="Show">
        {#each shows as [id, label] (id)}
          <button role="radio" aria-checked={query.show === id} onclick={() => (query.show = id)}>{label}</button>
        {/each}
      </div>
      <select class="field" aria-label="Sort" bind:value={query.sort}>
        <option value="newest">Newest first</option>
        <option value="longest">Longest first</option>
        <option value="largest">Largest first</option>
      </select>
      <input class="field search" type="search" placeholder="Search" aria-label="Search clips and games" bind:value={query.search} />
    </div>
  {/if}

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
      <Home {clips} {shown} {status} grouped={query.sort === 'newest'} buffer={settings?.buffer ?? 30} onopen={(clip) => (opened = clip.path)} />
    {:else if page === 'games'}
      <Games {clips} {shown} app={game} grouped={query.sort === 'newest'} onpick={showGame} onopen={(clip) => (opened = clip.path)} />
    {:else if page === 'starred'}
      <div class="view">
        <h1 class="shelf">
          Starred
          <span class="shelf-note">{starred.length} {starred.length === 1 ? 'clip' : 'clips'}, kept past the storage limit</span>
        </h1>
        {#if starred.length === 0}
          <p class="empty">Star a clip and it stays here. The storage limit never deletes it.</p>
        {:else}
          <Clips clips={shown} grouped={query.sort === 'newest'} onopen={(clip) => (opened = clip.path)} />
        {/if}
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
