<script lang="ts">
  import { fileSrc } from './host';
  import { art } from './art.svelte';
  import Clips from './Clips.svelte';
  import Cover from './Cover.svelte';
  import Icon from './Icon.svelte';
  import Thumb from './Thumb.svelte';
  import { at, day, games, sessionCount, size, type Clip } from './types';

  let {
    clips,
    shown,
    app,
    grouped,
    onpick,
    onopen
  }: {
    clips: Clip[];
    shown: Clip[];
    app: string | null;
    grouped: boolean;
    onpick: (app: string) => void;
    onopen: (clip: Clip, list: Clip[]) => void;
  } = $props();
  const all = $derived(games(clips));
  const listed = $derived(games(shown));
  const game = $derived(all.find((game) => game.app === app));
  const found = $derived(game && art(game.app));
</script>

<div class="view">
  {#if game}
    {@const newest = game.clips[0]}
    {@const sessions = sessionCount(game.clips)}
    {@const list = shown.filter((clip) => clip.app === game.app)}
    <header class="banner">
      {#if found?.hero}<img src={fileSrc(found.hero)} alt="" draggable="false" />{:else}<Thumb clip={newest} badge={false} />{/if}
      {#if found?.logo}<img class="banner-logo" src={fileSrc(found.logo)} alt={game.app} draggable="false" />{/if}
      <div class="banner-foot">
        {#if !found?.logo}
          <Icon app={game.app} icon={game.icon} size={52} />
          <h1 class="banner-name">{game.app}</h1>
        {/if}
        <p class="banner-facts mono">
          {game.clips.length} {game.clips.length === 1 ? 'clip' : 'clips'} in {sessions} {sessions === 1 ? 'session' : 'sessions'},
          {size(game.bytes)}, newest {day(newest.modified)} {at(newest.modified)}
        </p>
      </div>
    </header>
    {#if list.length === 0}
      <p class="empty">No clips match.</p>
    {:else}
      <Clips clips={list} {onopen} {grouped} showApp={false} />
    {/if}
  {:else}
    <h1 class="shelf">
      Games
      <span class="shelf-note">{all.length} {all.length === 1 ? 'game' : 'games'}, {clips.length} {clips.length === 1 ? 'clip' : 'clips'}</span>
    </h1>
    {#if all.length === 0}
      <p class="empty">A game shows up here once a clip of it is saved.</p>
    {:else if listed.length === 0}
      <p class="empty">No games match.</p>
    {/if}
    <div class="covers">
      {#each listed as game (game.app)}
        <Cover {game} onopen={() => onpick(game.app)} />
      {/each}
    </div>
  {/if}
</div>
