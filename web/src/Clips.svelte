<script lang="ts">
  import { invoke } from '@tauri-apps/api/core';
  import { SvelteSet } from 'svelte/reactivity';
  import Thumb from './Thumb.svelte';
  import { at, day, size, title, type Clip } from './types';
  import { reduced, say, view } from './view.svelte';

  let {
    clips,
    onopen,
    showApp = true,
    grouped = true
  }: { clips: Clip[]; onopen: (clip: Clip) => void; showApp?: boolean; grouped?: boolean } = $props();
  let hovered = $state<string | null>(null);
  const selected = new SvelteSet<string>();
  // where a shift click's range starts
  let anchor: string | null = null;
  const picked = $derived(clips.filter((clip) => selected.has(clip.path)));
  const allStarred = $derived(picked.length > 0 && picked.every((clip) => clip.starred));

  // clips come newest first and each session's clips sit together in that order, unless sorted otherwise
  const sessions = $derived.by(() => {
    if (!grouped) return clips.length > 0 ? [clips] : [];
    const groups = new Map<number, Clip[]>();
    for (const clip of clips) {
      const group = groups.get(clip.session);
      if (group) group.push(clip);
      else groups.set(clip.session, [clip]);
    }
    return [...groups.values()];
  });

  async function act(command: string, args: Record<string, unknown>) {
    try {
      await invoke(command, args);
    } catch (e) {
      say(String(e), true);
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

<div class="sessions">
  {#each sessions as session (session[0].path)}
    {@const newest = session[0]}
    {@const oldest = session[session.length - 1]}
    <section class="session">
      {#if grouped}
        <h3 class="session-head">
          {#if showApp}<span>{newest.app}</span>{/if}
          <span class:session-when={showApp}>{day(oldest.modified)} {at(oldest.modified)}{newest !== oldest ? ` to ${at(newest.modified)}` : ''}</span>
          <span class="mono">{session.length} {session.length === 1 ? 'clip' : 'clips'}</span>
        </h3>
      {/if}
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
              <Thumb {clip} live={view.previews && !reduced.current && hovered === clip.path} />
              <span class="clip-name">{title(clip)}</span>
              <span class="clip-meta mono">{size(clip.bytes)}</span>
            </button>
            <button class="pick" aria-pressed={selected.has(clip.path)} aria-label="Select {title(clip)}" onclick={(e) => pick(clip, e)}>
              <svg viewBox="0 0 16 16" aria-hidden="true"><path d="M3.5 8.5l3 3 6-7" /></svg>
            </button>
            <button
              class="star"
              aria-pressed={clip.starred}
              aria-label="Star {title(clip)}"
              onclick={() => act('star', { paths: [clip.path], on: !clip.starred })}
            >
              <svg viewBox="0 0 16 16" aria-hidden="true"><path d="M8 1.6l1.9 4 4.3.5-3.2 3 .9 4.3L8 11.2l-3.9 2.2.9-4.3-3.2-3 4.3-.5z" /></svg>
            </button>
          </li>
        {/each}
      </ul>
    </section>
  {/each}
  {#if picked.length > 0}
    <div class="selection" role="toolbar" aria-label="Selected clips">
      <span class="mono">{picked.length} selected, {size(picked.reduce((total, clip) => total + clip.bytes, 0))}</span>
      <span class="grow"></span>
      <button class="gel" onclick={() => act('star', { paths: picked.map((clip) => clip.path), on: !allStarred })}>
        {allStarred ? 'Unstar' : 'Star'}
      </button>
      <button class="gel danger" onclick={remove}>Delete</button>
      <button class="gel" onclick={() => selected.clear()}>Clear <kbd>Esc</kbd></button>
    </div>
  {/if}
</div>

<svelte:window
  onkeydowncapture={(e) => {
    if (e.key !== 'Escape' || selected.size === 0 || (e.target instanceof Element && e.target.closest('dialog'))) return;
    // the page behind would take esc as back too
    e.preventDefault();
    selected.clear();
  }}
/>
