<script lang="ts">
  import Icon from './Icon.svelte';
  import Thumb from './Thumb.svelte';
  import { at, day, size, type Game } from './types';

  let { game, onopen }: { game: Game; onopen: () => void } = $props();
  const newest = $derived(game.clips[0]);
</script>

<button class="tile" onclick={onopen}>
  <Thumb clip={newest} badge={false} />
  <span class="tile-foot">
    <Icon app={game.app} icon={game.icon} size={36} />
    <span class="tile-text">
      <span class="tile-name proper">{game.app}</span>
      <span class="tile-meta mono">{game.clips.length} {game.clips.length === 1 ? 'clip' : 'clips'}, {size(game.bytes)}</span>
    </span>
    <span class="tile-when mono">{day(newest.modified)} {at(newest.modified)}</span>
  </span>
</button>
