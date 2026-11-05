<script lang="ts">
  import { convertFileSrc, invoke } from '@tauri-apps/api/core';
  import Thumb from './Thumb.svelte';
  import { size, type Clip } from './types';

  let { clips, buffer }: { clips: Clip[]; buffer: number } = $props();
  let playing = $state<string | null>(null);
  let error = $state('');
  const current = $derived(clips.find((clip) => clip.path === playing) ?? null);
  const when = new Intl.DateTimeFormat(undefined, { month: 'short', day: 'numeric', hour: '2-digit', minute: '2-digit' });

  async function call(command: 'open' | 'reveal', path: string) {
    error = '';
    try {
      await invoke(command, { path });
    } catch (e) {
      error = String(e);
    }
  }
</script>

<div class="view">
  <div class="view-head">
    <h1>clips</h1>
    <p class="view-sub">{clips.length} saved, newest first</p>
  </div>

  {#if current}
    <section class="player">
      <!-- svelte-ignore a11y_media_has_caption -->
      <video src={convertFileSrc(current.path)} controls autoplay></video>
      <div class="btn-row">
        <span class="player-name proper">{current.name}</span>
        <span class="grow"></span>
        <button class="btn" onclick={() => call('open', current.path)}>open</button>
        <button class="btn" onclick={() => call('reveal', current.path)}>show in folder</button>
        <button class="btn ghost" onclick={() => (playing = null)}>close</button>
      </div>
    </section>
  {/if}
  {#if error}<p class="form-error">{error}</p>{/if}

  {#if clips.length === 0}
    <p class="empty">nothing saved yet. press alt+f9 and the last {buffer} s land here.</p>
  {:else}
    <ul class="grid">
      {#each clips as clip (clip.path)}
        <li>
          <button
            class="clip"
            aria-pressed={clip.path === playing}
            onclick={() => (playing = clip.path)}
            ondblclick={() => call('open', clip.path)}
          >
            <Thumb path={clip.path} />
            <span class="clip-name">{when.format(clip.modified)}</span>
            <span class="clip-meta mono proper">{size(clip.bytes)}</span>
          </button>
        </li>
      {/each}
    </ul>
  {/if}
</div>
