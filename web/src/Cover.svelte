<script lang="ts">
  import { fileSrc } from './host';
  import { art } from './art.svelte';
  import Icon from './Icon.svelte';
  import Thumb from './Thumb.svelte';
  import { day, size, type Game } from './types';

  let { game, onopen }: { game: Game; onopen: () => void } = $props();
  const newest = $derived(game.clips[0]);
  const cover = $derived(art(game.app)?.cover);
</script>

<button class="cover" onclick={onopen}>
  <span class="cover-art">
    {#if cover}
      <img src={fileSrc(cover)} alt="" draggable="false" />
    {:else}
      <Thumb clip={newest} badge={false} />
      <span class="cover-plain"><Icon app={game.app} icon={game.icon} size={40} /></span>
    {/if}
  </span>
  <span class="cover-name">{game.app}</span>
  <span class="cover-meta mono">
    {game.clips.length} {game.clips.length === 1 ? 'clip' : 'clips'}, {size(game.bytes)}, {day(newest.modified)}
  </span>
</button>
