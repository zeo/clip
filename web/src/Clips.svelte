<script lang="ts">
  import { invoke } from '@tauri-apps/api/core';
  import { SvelteSet } from 'svelte/reactivity';
  import Thumb from './Thumb.svelte';
  import { at, day, size, title, type Clip } from './types';

  let {
    clips,
    heading,
    buffer,
    onopen
  }: { clips: Clip[]; heading: string; buffer: number; onopen: (clip: Clip) => void } = $props();
  let hovered = $state<string | null>(null);
  let error = $state('');
  const selected = new SvelteSet<string>();
  // where a shift click's range starts
  let anchor: string | null = null;
  const picked = $derived(clips.filter((clip) => selected.has(clip.path)));
  const allStarred = $derived(picked.length > 0 && picked.every((clip) => clip.starred));

  // clips come newest first and each session's clips sit together in that order
  const sessions = $derived.by(() => {
    const groups = new Map<number, Clip[]>();
    for (const clip of clips) {
      const group = groups.get(clip.session);
      if (group) group.push(clip);
      else groups.set(clip.session, [clip]);
    }
    return [...groups.values()];
  });

  async function act(command: string, args: Record<string, unknown>) {
    error = '';
    try {
      await invoke(command, args);
    } catch (e) {
      error = String(e);
    }
  }

  // clips are in the order they're drawn, so a range runs across sessions as it looks
  function pick(clip: Clip, e: MouseEvent) {
    const from = anchor === null ? -1 : clips.findIndex((c) => c.path === anchor);
    const to = clips.indexOf(clip);
    if (e.shiftKey && from >= 0) {
      for (const c of clips.slice(Math.min(from, to), Math.max(from, to) + 1)) selected.add(c.path);
    } else if (selected.has(clip.path)) {
      selected.delete(clip.path);
    } else {
      selected.add(clip.path);
    }
    anchor = clip.path;
  }

  async function remove() {
    const paths = picked.map((clip) => clip.path);
    await act('delete', { paths });
    for (const path of paths) selected.delete(path);
  }
</script>

<div class="view">
  <div class="view-head">
    <h1 class:proper={heading !== 'clips' && heading !== 'starred'}>{heading}</h1>
    <p class="view-sub">{clips.length} saved, {sessions.length} {sessions.length === 1 ? 'session' : 'sessions'}</p>
  </div>
  {#if error}<p class="form-error">{error}</p>{/if}

  {#if clips.length === 0}
    <p class="empty">
      {heading === 'starred' ? 'no starred clips yet' : `nothing saved yet. press alt+f9 in any app and its last ${buffer} s land here.`}
    </p>
  {/if}
  {#each sessions as session (session[0].path)}
    {@const newest = session[0]}
    {@const oldest = session[session.length - 1]}
    <section class="session">
      <h3 class="rule-head session-head">
        <span class="proper session-app">{newest.app}</span>
        <span>{day(oldest.modified)} {at(oldest.modified)}{newest !== oldest ? ` to ${at(newest.modified)}` : ''}</span>
        <span class="session-count">{session.length} {session.length === 1 ? 'clip' : 'clips'}</span>
      </h3>
      <ul class="grid" class:picking={picked.length > 0}>
        {#each session as clip (clip.path)}
          <li
            class="clip"
            class:picked={selected.has(clip.path)}
            onpointerenter={() => (hovered = clip.path)}
            onpointerleave={() => hovered === clip.path && (hovered = null)}
          >
            <button
              class="clip-open"
              onclick={(e) => (picked.length > 0 || e.shiftKey || e.ctrlKey ? pick(clip, e) : onopen(clip))}
            >
              <Thumb {clip} live={hovered === clip.path} />
              <span class="clip-name proper">{title(clip)}</span>
              <span class="clip-meta mono proper">{size(clip.bytes)}</span>
            </button>
            <button class="pick" aria-pressed={selected.has(clip.path)} aria-label="select" onclick={(e) => pick(clip, e)}>
              <svg viewBox="0 0 16 16"><path d="M3.5 8.5l3 3 6-7" /></svg>
            </button>
            <button
              class="star"
              aria-pressed={clip.starred}
              aria-label="star"
              onclick={() => act('star', { paths: [clip.path], on: !clip.starred })}
            >
              <svg viewBox="0 0 16 16"><path d="M8 1.6l1.9 4 4.3.5-3.2 3 .9 4.3L8 11.2l-3.9 2.2.9-4.3-3.2-3 4.3-.5z" /></svg>
            </button>
          </li>
        {/each}
      </ul>
    </section>
  {/each}
  {#if picked.length > 0}
    <div class="selection" role="toolbar" aria-label="selected clips">
      <span class="mono">{picked.length} selected, {size(picked.reduce((total, clip) => total + clip.bytes, 0))}</span>
      <span class="grow"></span>
      <button class="btn" onclick={() => act('star', { paths: picked.map((clip) => clip.path), on: !allStarred })}>
        {allStarred ? 'unstar' : 'star'}
      </button>
      <button class="btn danger" onclick={remove}>delete</button>
      <button class="btn ghost" onclick={() => selected.clear()}>clear <kbd>esc</kbd></button>
    </div>
  {/if}
</div>

<svelte:window onkeydown={(e) => e.key === 'Escape' && selected.clear()} />
