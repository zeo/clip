<script lang="ts" module>
  import { convertFileSrc, invoke } from '@tauri-apps/api/core';

  const LONG_SIDE = 640;
  // stills are made one at a time, so the library holds at most one media player of its own
  let queue: Promise<unknown> = Promise.resolve();

  function still(path: string): Promise<string> {
    const next = queue.then(() => grab(path));
    queue = next.catch(() => {});
    return next;
  }

  function once(video: HTMLVideoElement, event: string): Promise<void> {
    return new Promise((done, fail) => {
      video.addEventListener(event, () => done(), { once: true });
      video.addEventListener('error', () => fail(new Error(`can't read ${video.src}`)), { once: true });
    });
  }

  async function grab(path: string): Promise<string> {
    const video = document.createElement('video');
    // the asset protocol answers with this window's origin, so the canvas stays readable
    video.crossOrigin = 'anonymous';
    video.muted = true;
    try {
      const loaded = once(video, 'loadeddata');
      video.src = convertFileSrc(path);
      await loaded;
      const seeked = once(video, 'seeked');
      video.currentTime = Math.min(1, video.duration / 2);
      await seeked;
      const scale = LONG_SIDE / Math.max(video.videoWidth, video.videoHeight);
      const canvas = document.createElement('canvas');
      canvas.width = Math.round(video.videoWidth * scale);
      canvas.height = Math.round(video.videoHeight * scale);
      canvas.getContext('2d')?.drawImage(video, 0, 0, canvas.width, canvas.height);
      const blob = await new Promise<Blob | null>((done) => canvas.toBlob(done, 'image/jpeg', 0.82));
      if (!blob) throw new Error('no still');
      const jpeg = Array.from(new Uint8Array(await blob.arrayBuffer()));
      return await invoke<string>('save_thumb', { path, jpeg });
    } finally {
      video.removeAttribute('src');
      video.load();
    }
  }
</script>

<script lang="ts">
  import { clock, type Clip } from './types';

  let { clip, live = false, badge = true }: { clip: Clip; live?: boolean; badge?: boolean } = $props();
  let made = $state<string | null>(null);
  let failed = $state(false);
  const shown = $derived(clip.thumb ?? made);

  function watch(node: HTMLElement) {
    if (clip.thumb) return;
    const io = new IntersectionObserver(
      ([entry]) => {
        if (!entry.isIntersecting) return;
        io.disconnect();
        still(clip.path).then(
          (thumb) => (made = thumb),
          () => (failed = true)
        );
      },
      { rootMargin: '200px' }
    );
    io.observe(node);
    return { destroy: () => io.disconnect() };
  }
</script>

<div class="thumb" use:watch>
  {#if shown}<img src={convertFileSrc(shown)} alt="" draggable="false" />{:else if failed}<span class="thumb-none">no preview</span>{/if}
  {#if live}
    <!-- svelte-ignore a11y_media_has_caption -->
    <video src={convertFileSrc(clip.path)} autoplay muted loop></video>
  {/if}
  {#if badge && clip.seconds > 0}<span class="duration mono">{clock(clip.seconds)}</span>{/if}
</div>
