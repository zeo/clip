<script lang="ts">
  import { invoke } from '@tauri-apps/api/core';
  import Clips from './Clips.svelte';
  import Icon from './Icon.svelte';
  import Thumb from './Thumb.svelte';
  import Tile from './Tile.svelte';
  import { at, clock, day, games, size, title, type Clip, type Status } from './types';

  let {
    clips,
    status,
    buffer,
    onopen,
    ongame,
    onall
  }: {
    clips: Clip[];
    status: Status;
    buffer: number;
    onopen: (clip: Clip) => void;
    ongame: (app: string) => void;
    onall: () => void;
  } = $props();
  let live = $state(false);
  const latest = $derived(clips[0]);
  const all = $derived(games(clips));
  // the newest three sessions, whichever apps they're from
  const recent = $derived.by(() => {
    const kept = new Set<number>();
    for (const clip of clips) if (kept.size < 3) kept.add(clip.session);
    return clips.filter((clip) => kept.has(clip.session));
  });
</script>

<div class="view">
  {#if latest}
    <section class="hero">
      <button
        class="hero-screen"
        aria-label="open {title(latest)}"
        onclick={() => onopen(latest)}
        onpointerenter={() => (live = true)}
        onpointerleave={() => (live = false)}
      >
        <Thumb clip={latest} {live} />
      </button>
      <div class="hero-side">
        <p class="eyebrow">latest clip</p>
        <button class="hero-app" onclick={() => ongame(latest.app)}>
          <Icon app={latest.app} icon={latest.icon} size={24} />
          <span class="proper">{latest.app}</span>
        </button>
        <h1 class="hero-title proper">{title(latest)}</h1>
        <dl class="facts">
          <div><dt>saved</dt><dd>{day(latest.modified)} {at(latest.modified)}</dd></div>
          <div><dt>length</dt><dd>{clock(latest.seconds)}</dd></div>
          <div><dt>size</dt><dd class="proper">{size(latest.bytes)}</dd></div>
          <div><dt>marks</dt><dd>{latest.marks.length || 'none'}</dd></div>
        </dl>
        <div class="btn-row">
          <button class="btn primary" onclick={() => onopen(latest)}>open</button>
          <button class="btn" aria-pressed={latest.starred} onclick={() => invoke('star', { paths: [latest.path], on: !latest.starred })}>
            {latest.starred ? 'starred' : 'star'}
          </button>
          <button class="btn ghost" onclick={() => invoke('reveal', { path: latest.path })}>show in folder</button>
        </div>
      </div>
    </section>

    <h3 class="rule-head">
      <span>games</span>
      {#if all.length > 4}<button class="link" onclick={onall}>all {all.length}</button>{/if}
    </h3>
    <div class="tiles">
      {#each all.slice(0, 4) as game (game.app)}
        <Tile {game} onopen={() => ongame(game.app)} />
      {/each}
    </div>

    <h3 class="rule-head">recent</h3>
    <Clips clips={recent} {onopen} />
  {:else}
    <section class="welcome">
      <p class="eyebrow">nothing saved yet</p>
      <h1>play something, then press <kbd>alt+f9</kbd></h1>
      <p class="welcome-text">
        the last {buffer} s of the game in focus land here. <kbd>alt+f10</kbd> marks a moment and saves it 15 s later, so it
        sits in the middle.
      </p>
      <p class="welcome-status" class:live={status.recording}>
        <span class="pill-dot"></span>
        {#if status.recording}recording <span class="proper">{status.app}</span>{:else}{status.message}{/if}
      </p>
    </section>
  {/if}
</div>
