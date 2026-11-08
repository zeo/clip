<script lang="ts">
  import Clips from './Clips.svelte';
  import { keyFor, length, sentence, sessionCount, size, type Clip, type Status } from './types';

  let {
    clips,
    shown,
    status,
    grouped,
    buffer,
    onopen,
    onkeys
  }: {
    clips: Clip[];
    shown: Clip[];
    status: Status;
    grouped: boolean;
    buffer: number;
    onopen: (clip: Clip, list: Clip[]) => void;
    onkeys: () => void;
  } = $props();
  const since = Date.now() - 7 * 864e5;
  const recent = $derived(clips.filter((clip) => clip.modified >= since));
  const feed = $derived(shown.filter((clip) => clip.modified >= since));
</script>

<div class="view">
  {#if clips.length > 0}
    <h1 class="shelf">
      Last 7 days
      <span class="shelf-note">
        {recent.length} {recent.length === 1 ? 'clip' : 'clips'} in {sessionCount(recent)}
        {sessionCount(recent) === 1 ? 'session' : 'sessions'}, <span class="mono">{size(recent.reduce((sum, clip) => sum + clip.bytes, 0))}</span>
      </span>
    </h1>
    {#if recent.length === 0}
      <p class="empty">Nothing saved in the last 7 days. Older clips are in the Library.</p>
    {:else if feed.length === 0}
      <p class="empty">No clips match.</p>
    {:else}
      <Clips clips={feed} {grouped} {onopen} />
    {/if}
  {:else}
    <section class="welcome">
      <h1>Play something, then press <kbd>{keyFor(status, 'save') ?? 'Save clip'}</kbd></h1>
      <p>
        The last {length(buffer)} of the game in focus land here.
        {#if keyFor(status, 'mark')}<kbd>{keyFor(status, 'mark')}</kbd> marks a moment and saves it 15 s later, so the moment sits
          in the middle of the clip.{/if}
      </p>
      <p>Coming from Medal, the NVIDIA App, SteelSeries Moments or AMD? <button class="link" onclick={onkeys}>Use the keys you know</button></p>
      <p class="live" class:on={status.recording}>
        <span class="dot"></span>
        <span>{#if status.recording}Recording {status.app}{:else}{sentence(status.message)}{/if}</span>
      </p>
    </section>
  {/if}
</div>
