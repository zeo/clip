<script lang="ts">
  import { invoke } from '@tauri-apps/api/core';
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

  async function star(clip: Clip) {
    error = '';
    try {
      await invoke('star', { path: clip.path, on: !clip.starred });
    } catch (e) {
      error = String(e);
    }
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
      <ul class="grid">
        {#each session as clip (clip.path)}
          <li
            class="clip"
            onpointerenter={() => (hovered = clip.path)}
            onpointerleave={() => hovered === clip.path && (hovered = null)}
          >
            <button class="clip-open" onclick={() => onopen(clip)}>
              <Thumb {clip} live={hovered === clip.path} />
              <span class="clip-name proper">{title(clip)}</span>
              <span class="clip-meta mono proper">{size(clip.bytes)}</span>
            </button>
            <button class="star" aria-pressed={clip.starred} aria-label="star" onclick={() => star(clip)}>
              <svg viewBox="0 0 16 16"><path d="M8 1.6l1.9 4 4.3.5-3.2 3 .9 4.3L8 11.2l-3.9 2.2.9-4.3-3.2-3 4.3-.5z" /></svg>
            </button>
          </li>
        {/each}
      </ul>
    </section>
  {/each}
</div>
