<script lang="ts">
  import Clips from './Clips.svelte';
  import Icon from './Icon.svelte';
  import Tile from './Tile.svelte';
  import { at, day, games, sessionCount, size, type Clip } from './types';

  let {
    clips,
    app,
    onpick,
    onopen
  }: { clips: Clip[]; app: string | null; onpick: (app: string) => void; onopen: (clip: Clip) => void } = $props();
  const all = $derived(games(clips));
  const game = $derived(all.find((game) => game.app === app));
</script>

<div class="view">
  {#if game}
    {@const newest = game.clips[0]}
    <header class="game-head">
      <Icon app={game.app} icon={game.icon} size={56} />
      <div class="game-title">
        <h1 class="proper">{game.app}</h1>
        <p class="mono">
          {game.clips.length} {game.clips.length === 1 ? 'clip' : 'clips'} in {sessionCount(game.clips)}
          {sessionCount(game.clips) === 1 ? 'session' : 'sessions'}, <span class="proper">{size(game.bytes)}</span>, last
          {day(newest.modified)} {at(newest.modified)}
        </p>
      </div>
    </header>
    <Clips clips={game.clips} {onopen} showApp={false} />
  {:else}
    <div class="view-head">
      <h1>games</h1>
      <p class="view-sub">{all.length} {all.length === 1 ? 'game' : 'games'}, {clips.length} clips</p>
    </div>
    {#if all.length === 0}<p class="empty">a game shows up here once a clip of it is saved</p>{/if}
    <div class="tiles">
      {#each all as game (game.app)}
        <Tile {game} onopen={() => onpick(game.app)} />
      {/each}
    </div>
  {/if}
</div>
