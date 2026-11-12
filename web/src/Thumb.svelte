<script lang="ts" module>
  import { fileSrc, invoke } from './host';

  const LONG_SIDE = 640;
  // stills are made one at a time, so the library holds at most one media player of its own
  let queue: Promise<unknown> = Promise.resolve();

  function still(path: string, image: boolean): Promise<string> {
    const next = queue.then(() => (image ? picture(path) : grab(path)));
    queue = next.catch(() => {});
    return next;
  }

  // a file the player never answers for would hold up every still after it
  const PATIENCE = 15_000;

  function once(video: HTMLVideoElement, event: string): Promise<void> {
    return new Promise((done, fail) => {
      const late = setTimeout(() => fail(new Error(`${video.src} gave no ${event} in ${PATIENCE / 1000} s`)), PATIENCE);
      video.addEventListener(event, () => (clearTimeout(late), done()), { once: true });
      video.addEventListener('error', () => (clearTimeout(late), fail(new Error(`can't read ${video.src}`))), { once: true });
    });
  }

  async function picture(path: string): Promise<string> {
    const img = new Image();
    img.crossOrigin = 'anonymous';
    img.src = fileSrc(path);
    await img.decode();
    return keep(path, img, img.naturalWidth, img.naturalHeight);
  }

  async function keep(path: string, from: CanvasImageSource, width: number, height: number): Promise<string> {
    const scale = Math.min(1, LONG_SIDE / Math.max(width, height));
    const canvas = document.createElement('canvas');
    canvas.width = Math.max(1, Math.round(width * scale));
    canvas.height = Math.max(1, Math.round(height * scale));
    canvas.getContext('2d')?.drawImage(from, 0, 0, canvas.width, canvas.height);
    const blob = await new Promise<Blob | null>((done) => canvas.toBlob(done, 'image/jpeg', 0.82));
    if (!blob) throw new Error('no still');
    const jpeg = Array.from(new Uint8Array(await blob.arrayBuffer()));
    return await invoke<string>('save_thumb', { path, jpeg });
  }

  async function grab(path: string): Promise<string> {
    const video = document.createElement('video');
    // the asset protocol answers with this window's origin, so the canvas stays readable
    video.crossOrigin = 'anonymous';
    video.muted = true;
    try {
      const loaded = once(video, 'loadeddata');
      video.src = fileSrc(path);
      await loaded;
      const seeked = once(video, 'seeked');
      video.currentTime = Math.min(1, video.duration / 2);
      await seeked;
      return await keep(path, video, video.videoWidth, video.videoHeight);
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
        still(clip.path, clip.kind === 'image').then(
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
  {#if shown}<img src={fileSrc(shown)} alt="" draggable="false" />{:else if failed}<span class="thumb-none">No preview</span>{/if}
  {#if live && clip.kind === 'clip'}
    <!-- svelte-ignore a11y_media_has_caption -->
    <video src={fileSrc(clip.path)} autoplay muted loop></video>
  {/if}
  {#if badge && clip.seconds > 0}<span class="duration mono">{clock(clip.seconds)}</span>{:else if badge && clip.path.toLowerCase().endsWith('.gif')}<span class="duration mono">GIF</span>{/if}
</div>
