<script lang="ts">
  import { onMount } from 'svelte';
  import { invoke } from '@tauri-apps/api/core';
  import { listen } from '@tauri-apps/api/event';
  import { getCurrentWindow } from '@tauri-apps/api/window';
  import Clips from './Clips.svelte';
  import Preferences from './Preferences.svelte';
  import { ramMB, type Clip, type Settings, type Status } from './types';

  const win = getCurrentWindow();
  let view = $state<'clips' | 'settings'>('clips');
  let status = $state<Status>({ recording: false, app: '', message: 'starting' });
  let settings = $state<Settings | null>(null);
  let clips = $state<Clip[]>([]);
  let first = $state(false);
  let app = $state<string | null>(null);
  // clips arrive newest first, so apps come out in order of their latest clip
  const apps = $derived.by(() => {
    const counts = new Map<string, number>();
    for (const clip of clips) counts.set(clip.app, (counts.get(clip.app) ?? 0) + 1);
    return [...counts];
  });
  const shown = $derived(app === null ? clips : clips.filter((clip) => clip.app === app));

  async function refresh() {
    [settings, clips] = await Promise.all([invoke<Settings>('settings'), invoke<Clip[]>('clips')]);
  }

  onMount(() => {
    invoke<Status>('status').then((s) => (status = s));
    invoke<boolean>('first_run').then((f) => {
      first = f;
      if (f) view = 'settings';
    });
    refresh();
    const stops = [listen<Status>('status', (e) => (status = e.payload)), listen('clips', refresh)];
    return () => stops.forEach((stop) => stop.then((f) => f()));
  });

  async function applied() {
    first = false;
    await refresh();
  }
</script>

<div class="shell">
  <header class="titlebar" data-tauri-drag-region>
    <div class="titlebar-drag" data-tauri-drag-region>
      <span class="titlebar-mark" data-tauri-drag-region>memory</span>
      <span class="titlebar-path" data-tauri-drag-region><span class="path-sep">/</span><span>{view}</span></span>
    </div>
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

  <aside class="sidebar">
    <p class="sidebar-label"><span class="sidebar-label-mark"></span>replay buffer</p>
    <section class="module">
      <div class="module-head">
        <span class="module-index">A</span>
        <h2>buffer</h2>
        <span class="module-meta" class:live={status.recording}>{status.recording ? 'live' : 'off'}</span>
      </div>
      {#if status.recording}<p class="buffer-app proper">{status.app}</p>{/if}
      <p class="buffer-note" class:err={!status.recording && status.message !== 'starting'}>{status.message}</p>
      <button class="btn primary wide" disabled={!status.recording} onclick={() => invoke('save_clip')}>
        save clip <kbd>alt+f9</kbd>
      </button>
    </section>
    <section class="module">
      <div class="module-head"><span class="module-index">B</span><h2>view</h2></div>
      <div class="tasks" role="radiogroup" aria-label="view">
        <button class="nav-item" role="radio" aria-checked={view === 'clips'} onclick={() => (view = 'clips')}>
          <span class="nav-item-key"><span class="nav-item-bracket">[</span>1<span class="nav-item-bracket">]</span></span>
          <span>clips</span>
          <span class="nav-item-count">{clips.length}</span>
        </button>
        <button class="nav-item" role="radio" aria-checked={view === 'settings'} onclick={() => (view = 'settings')}>
          <span class="nav-item-key"><span class="nav-item-bracket">[</span>2<span class="nav-item-bracket">]</span></span>
          <span>settings</span>
          <span></span>
        </button>
      </div>
    </section>
    {#if view === 'clips' && apps.length > 0}
      <section class="module">
        <div class="module-head"><span class="module-index">C</span><h2>apps</h2></div>
        <div class="tasks" role="radiogroup" aria-label="app">
          <button class="nav-item app" role="radio" aria-checked={app === null} onclick={() => (app = null)}>
            <span>all</span>
            <span class="nav-item-count">{clips.length}</span>
          </button>
          {#each apps as [name, count] (name)}
            <button class="nav-item app" role="radio" aria-checked={app === name} onclick={() => (app = name)}>
              <span class="proper app-name">{name}</span>
              <span class="nav-item-count">{count}</span>
            </button>
          {/each}
        </div>
      </section>
    {/if}
  </aside>

  <main class="stage">
    {#if view === 'clips'}
      <Clips clips={shown} {app} buffer={settings?.buffer ?? 30} />
    {:else}
      <Preferences {first} onapplied={applied} />
    {/if}
  </main>

  <footer class="statusbar">
    {#if settings}
      <span class="proper">{settings.folder}</span>
      <span class="grow"></span>
      <span>{settings.height >= 4320 ? 'native' : `${settings.height}p`}{settings.fps} · {settings.mbps} <span class="proper">Mbps</span></span>
      <span class="sep">|</span>
      <span>ram about {ramMB(settings)} <span class="proper">MB</span></span>
    {/if}
  </footer>
</div>

<svelte:window
  onkeydown={(e) => {
    if (e.target instanceof HTMLInputElement || e.target instanceof HTMLSelectElement) return;
    if (e.key === '1') view = 'clips';
    if (e.key === '2') view = 'settings';
  }}
/>
