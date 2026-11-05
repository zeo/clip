<script lang="ts">
  import { convertFileSrc } from '@tauri-apps/api/core';

  let { path }: { path: string } = $props();
  let near = $state(false);
  let seconds = $state<number | null>(null);

  // webview2 caps how many media players a page holds, so only clips near the viewport get one
  function watch(node: HTMLElement) {
    const io = new IntersectionObserver(([entry]) => (near = entry.isIntersecting), { rootMargin: '300px' });
    io.observe(node);
    return { destroy: () => io.disconnect() };
  }
</script>

<div class="thumb" use:watch>
  {#if near}
    <video src={`${convertFileSrc(path)}#t=1`} preload="metadata" muted onloadedmetadata={(e) => (seconds = e.currentTarget.duration)}></video>
  {/if}
  {#if seconds !== null}<span class="duration mono">{Math.round(seconds)} s</span>{/if}
</div>
