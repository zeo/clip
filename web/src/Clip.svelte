<script lang="ts">
  import { onMount, untrack } from 'svelte';
  import { convertFileSrc, invoke } from '@tauri-apps/api/core';
  import { listen } from '@tauri-apps/api/event';
  import { getCurrentWindow } from '@tauri-apps/api/window';
  import { CANCELLED, LONGEST, gif } from './gif';
  import Icon from './Icon.svelte';
  import { at, clock, day, size, title, type Clip, type Track } from './types';
  import { discard, radios, say, view } from './view.svelte';

  let {
    clip,
    rewind = false,
    prev,
    next,
    onopen,
    onclose,
    onmoved
  }: {
    clip: Clip;
    rewind?: boolean;
    prev?: Clip;
    next?: Clip;
    onopen: (clip: Clip) => void;
    onclose: () => void;
    onmoved: (path: string) => void;
  } = $props();

  type Lane = { name: string; buffer: AudioBuffer; peaks: string; muted: boolean; solo: boolean };
  const PEAKS = 800;
  // past this the tracks are restarted where the video is
  const DRIFT = 0.04;
  const RATES = [0.25, 0.5, 1, 1.5, 2];
  // the upload limits people meet most, in the bytes the sites count
  const SIZES = [
    { bytes: 0, label: 'Original quality' },
    { bytes: 20e6, label: '20 MB, Discord' },
    { bytes: 50e6, label: '50 MB, Nitro Basic or boost level 2' },
    { bytes: 100e6, label: '100 MB, boost level 3' },
    { bytes: 1000e6, label: '1 GB, Nitro' },
    { bytes: -1, label: 'Custom' }
  ];

  let video = $state<HTMLVideoElement>();
  let bar = $state<HTMLElement>();
  let play = $state<HTMLButtonElement>();
  // the page is keyed by path, so the probe's length only seeds these
  const probed = untrack(() => clip.seconds);
  let duration = $state(probed);
  let now = $state(0);
  let paused = $state(true);
  // a rewound buffer opens on its last 30 s, the usual clip length
  let start = $state(untrack(() => rewind) ? Math.max(0, probed - 30) : 0);
  let end = $state(probed);
  // the other kept parts, start and end are the part the handles move
  let others = $state<[number, number][]>([]);
  let lanes = $state<Lane[]>([]);
  let loading = $state(true);
  let mix = $state(true);
  let busy = $state('');
  let error = $state('');
  let naming = $state<string | null>(null);
  let rate = $state(1);
  let loop = $state(false);
  let full = $state(false);
  let portrait = $state(false);
  let middle = $state(0.5);
  let replace = $state(false);
  let animated = $state(false);
  let halted = false;
  let progress = $state<number | null>(null);
  // bumped when the file under the player changes, so the webview fetches it again
  let version = $state(0);
  let shown = $state({ width: 0, height: 0 });
  let picture = $state({ width: 16, height: 9 });
  const fit = $derived(view.fit === -1 ? Math.round(Math.max(1, view.custom) * 1e6) : view.fit);
  // the share of the picture's width a 9:16 window takes, and where it sits on screen
  const narrow = $derived(Math.min(1, (picture.height * 9) / 16 / picture.width));
  const crop = $derived.by(() => {
    const scale = Math.min(shown.width / picture.width, shown.height / picture.height) || 0;
    const width = picture.width * scale;
    const x = (shown.width - width) / 2;
    const left = x + (middle - narrow / 2) * width;
    return { x, span: width, left, top: (shown.height - picture.height * scale) / 2, width: narrow * width, height: picture.height * scale };
  });
  const fps = $derived(clip.fps || 60);
  const source = $derived(convertFileSrc(clip.path) + (version ? `?v=${version}` : ''));
  const parts = $derived([...others, [start, end] as [number, number]].sort((a, b) => a[0] - b[0]));
  const kept = $derived(parts.reduce((sum, [a, b]) => sum + b - a, 0));
  const gaps = $derived([0, ...parts.flat(), duration].flatMap((t, i, edges) => (i % 2 === 0 && edges[i + 1] > t ? [[t, edges[i + 1]]] : [])));
  // the part moving can't run into its neighbours
  const lo = $derived(Math.max(0, ...others.filter(([, b]) => b <= start).map(([, b]) => b)));
  const hi = $derived(Math.min(duration, ...others.filter(([a]) => a >= end).map(([a]) => a)));
  const long = $derived(animated && end - start > LONGEST);
  const soloed = $derived(lanes.some((lane) => lane.solo));
  const audible = $derived(lanes.flatMap((lane, i) => ((soloed ? lane.solo : !lane.muted) ? [i] : [])));
  const pct = (t: number) => `${(100 * t) / (duration || 1)}%`;

  const context = new AudioContext();
  const master = context.createGain();
  master.connect(context.destination);
  let playing: AudioBufferSourceNode[] = [];
  // where the video was and what the audio clock read when the tracks last started
  let from = 0;
  let started = 0;
  let frame = 0;

  $effect(() => {
    master.gain.value = view.volume;
  });

  $effect(() => {
    if (!video) return;
    video.defaultPlaybackRate = video.playbackRate = rate;
    untrack(sound);
  });

  onMount(() => {
    load();
    return () => {
      cancelAnimationFrame(frame);
      silence();
      context.close();
      // stepping to the next clip keeps the screen filled, anything else gives the window back
      if (full) setTimeout(() => document.querySelector('.theater.full') || getCurrentWindow().setFullscreen(false));
    };
  });

  async function load() {
    try {
      const tracks = await invoke<Track[]>('track_audio', { path: clip.path });
      lanes = await Promise.all(
        tracks.map(async (track) => {
          const data = await (await fetch(convertFileSrc(track.path))).arrayBuffer();
          const buffer = await context.decodeAudioData(data);
          return { name: track.name, buffer, peaks: peaks(buffer), muted: false, solo: false };
        })
      );
    } catch (e) {
      error = String(e);
    } finally {
      loading = false;
    }
  }

  function peaks(buffer: AudioBuffer): string {
    const channels = Array.from({ length: buffer.numberOfChannels }, (_, c) => buffer.getChannelData(c));
    const per = Math.max(1, Math.floor(buffer.length / PEAKS));
    let path = '';
    for (let i = 0; i < PEAKS; i++) {
      let peak = 0;
      for (const data of channels) {
        for (let n = i * per, last = Math.min(n + per, data.length); n < last; n++) peak = Math.max(peak, Math.abs(data[n]));
      }
      const h = Math.max(0.5, Math.sqrt(peak) * 48);
      path += `M${i} ${50 - h}V${50 + h}`;
    }
    return path;
  }

  function silence() {
    for (const source of playing) {
      source.stop();
      source.disconnect();
    }
    playing = [];
  }

  function sound() {
    silence();
    if (!video || video.paused) return;
    from = video.currentTime;
    started = context.currentTime;
    for (const i of audible) {
      const buffer = lanes[i].buffer;
      if (from >= buffer.duration) continue;
      const source = context.createBufferSource();
      source.buffer = buffer;
      source.playbackRate.value = rate;
      source.connect(master);
      source.start(0, from);
      playing.push(source);
    }
  }

  function follow() {
    if (!video) return;
    now = video.currentTime;
    const [a, b] = ahead(now);
    if (loop && (now < a || now >= b)) seek(a);
    else if (playing.length > 0 && Math.abs(from + (context.currentTime - started) * rate - now) > DRIFT) sound();
    if (!video.paused) frame = requestAnimationFrame(follow);
  }

  function played() {
    paused = false;
    context.resume().then(sound);
    frame = requestAnimationFrame(follow);
  }

  function stopped() {
    paused = true;
    silence();
    cancelAnimationFrame(frame);
    if (video) now = video.currentTime;
  }

  function ended() {
    stopped();
    if (loop) {
      seek(parts[0][0]);
      video?.play();
    }
  }

  const snap = (t: number) => Math.min(duration, Math.max(0, Math.round(t * fps) / fps));

  function seek(t: number) {
    if (!video) return;
    // a hair past the frame's start so the webview shows that frame, not the one before
    video.currentTime = snap(t) + 0.25 / fps;
    now = snap(t);
    choose(now);
  }

  // the kept part playing at t or the next one, round to the first after the last
  const ahead = (t: number) => parts.find(([, b]) => t < b - 1 / fps) ?? parts[0];

  // the part under t is the one the handles and I and O move
  function choose(t: number) {
    const i = others.findIndex(([a, b]) => t >= a && t < b);
    if (i >= 0) [[start, end]] = others.splice(i, 1, [start, end]);
  }

  function split() {
    const t = snap(now);
    if (t - start < 1 / fps || end - t < 1 / fps) return say('Put the playhead inside a kept part to split it', true);
    others.push([start, t]);
    start = t;
  }

  function cutOut() {
    const rest = [...others].sort((a, b) => a[0] - b[0]);
    if (!rest.length) return;
    const i = rest.findIndex(([a]) => a > start);
    [[start, end]] = rest.splice(i < 0 ? rest.length - 1 : i, 1);
    others = rest;
    seek(start);
  }

  function toggle() {
    if (!video) return;
    if (!video.paused) return video.pause();
    const [a, b] = ahead(now);
    if (loop && (now < a || now >= b - 1 / fps)) seek(a);
    video.play();
  }

  const step = (frames: number) => (video?.pause(), seek(now + frames / fps));
  const skip = (seconds: number) => seek(now + seconds);
  const speed = (by: number) => (rate = RATES[Math.min(RATES.length - 1, Math.max(0, RATES.indexOf(rate) + by))]);
  const loud = (by: number) => (view.volume = Math.round(Math.min(1, Math.max(0, view.volume + by)) * 100) / 100);

  async function fill(on = !full) {
    full = on;
    await getCurrentWindow()
      .setFullscreen(on)
      .catch((e) => say(String(e), true));
  }

  function close() {
    if (full) fill(false);
    onclose();
  }

  function setLane(i: number, change: Partial<Pick<Lane, 'muted' | 'solo'>>) {
    Object.assign(lanes[i], change);
    sound();
  }

  function setMarks(marks: number[]) {
    invoke('set_marks', { path: clip.path, at: marks }).catch((e) => say(String(e), true));
  }

  function mark() {
    const t = snap(now);
    if (clip.marks.some((m) => Math.abs(m - t) < 0.05)) return;
    setMarks([...clip.marks, t]);
    say(`Marked ${clock(t, true)}`);
  }

  function unmark(t: number) {
    setMarks(clip.marks.filter((m) => m !== t));
    say(`Removed the mark at ${clock(t, true)}`);
    play?.focus();
  }

  function timeAt(event: PointerEvent): number {
    const box = bar?.getBoundingClientRect();
    if (!box) return 0;
    return snap((Math.min(Math.max(event.clientX - box.left, 0), box.width) / box.width) * duration);
  }

  function drag(event: PointerEvent, what: 'in' | 'out' | 'head') {
    event.stopPropagation();
    const target = event.currentTarget as HTMLElement;
    target.setPointerCapture(event.pointerId);
    const move = (e: PointerEvent) => {
      const t = timeAt(e);
      if (what === 'in') start = Math.max(lo, Math.min(t, end - 1 / fps));
      else if (what === 'out') end = Math.min(hi, Math.max(t, start + 1 / fps));
      else seek(t);
    };
    move(event);
    target.onpointermove = move;
    target.onpointerup = () => (target.onpointermove = null);
  }

  async function act<T>(label: string, work: () => Promise<T>): Promise<T | undefined> {
    busy = label;
    error = '';
    try {
      return await work();
    } catch (e) {
      error = String(e);
    } finally {
      busy = '';
    }
  }

  const named = (path: string) => path.split(/[\\/]/).pop();

  async function exportCut() {
    const path = clip.path;
    const before = $state.snapshot(clip.marks);
    const replacing = replace && !animated;
    progress = 0;
    halted = false;
    const heard = animated ? () => {} : await listen<number>('export-progress', (e) => (progress = e.payload));
    const out = await act('export', async () => {
      if (!animated) {
        const args = { path, ranges: parts, tracks: audible, mix: mix && audible.length > 1, fit: fit || null, portrait: portrait ? middle : null, replace };
        return invoke<string>('export', args);
      }
      const cut = { start, end, middle: portrait ? middle : null, fit };
      const bytes = await gif(source, cut, (pct) => (progress = pct), () => halted);
      return invoke<string>('save_frame', bytes, { headers: { clip: encodeURIComponent(path), at: String(start) } });
    }).finally(() => (heard(), (progress = null)));
    if (error === CANCELLED) {
      error = '';
      say('Export cancelled');
    }
    if (!out) return;
    if (rewind && !animated) {
      say(`Saved ${named(out)}`);
      discard([path], 'the rest of the buffer');
      return onmoved(out);
    }
    const copied =
      fit > 0 &&
      view.copyAfter &&
      (await invoke('copy_files', { paths: [out] }).then(
        () => true,
        (e) => (say(String(e), true), false)
      ));
    const pasted = copied ? ', copied to paste' : '';
    if (!replacing) return say(`Saved ${named(out)}${pasted}`);
    [start, end] = [0, kept];
    others = [];
    version++;
    // the original waits beside the clip until the undo runs out
    const timer = setTimeout(() => invoke('drop_original', { path }).catch((e) => say(String(e), true)), 8000);
    say(`Replaced ${title(clip)} with the cut${pasted}`, false, async () => {
      clearTimeout(timer);
      try {
        await invoke('restore_original', { path });
        await invoke('set_marks', { path, at: before });
        version++;
        say(`Put back ${title(clip)}`);
      } catch (e) {
        say(String(e), true);
      }
    });
  }

  const place = (at: number) => Math.min(1 - narrow / 2, Math.max(narrow / 2, at));

  function slide(event: PointerEvent) {
    const target = event.currentTarget as HTMLElement;
    target.setPointerCapture(event.pointerId);
    const screen = target.parentElement!.getBoundingClientRect();
    const grab = event.clientX - (screen.left + crop.left + crop.width / 2);
    target.onpointermove = (e) => {
      const width = crop.width / narrow;
      middle = place((e.clientX - grab - screen.left - (shown.width - width) / 2) / width);
    };
    target.onpointerup = () => (target.onpointermove = null);
  }

  async function still() {
    if (!video?.videoWidth) return;
    video.pause();
    const canvas = document.createElement('canvas');
    canvas.width = video.videoWidth;
    canvas.height = video.videoHeight;
    canvas.getContext('2d')?.drawImage(video, 0, 0);
    const out = await act('frame', async () => {
      const png = await new Promise<Blob | null>((done) => canvas.toBlob(done, 'image/png'));
      if (!png) throw new Error("couldn't draw the frame");
      const headers = { clip: encodeURIComponent(clip.path), at: String(now) };
      return invoke<string>('save_frame', new Uint8Array(await png.arrayBuffer()), { headers });
    });
    if (out) say(`Saved ${named(out)}`);
  }

  function copy() {
    invoke('copy_files', { paths: [clip.path] }).then(
      () => say(`Copied ${title(clip)}, paste it into a chat or folder`),
      (e) => say(String(e), true)
    );
  }

  async function rename() {
    if (naming === null) return;
    const name = naming.trim();
    naming = null;
    if (!name || name === clip.name) return;
    const to = await act('renaming', () => invoke<string>('rename', { path: clip.path, name }));
    if (to) onmoved(to);
  }

  async function keepAll() {
    const to = await act('renaming', () => invoke<string>('rename', { path: clip.path, name: clip.name.replace(/ rewind$/, '') }));
    if (to) onmoved(to);
  }

  function remove() {
    video?.pause();
    discard([clip.path], title(clip));
    close();
  }

  function keys(e: KeyboardEvent) {
    const target = e.target instanceof Element ? e.target : null;
    if (target instanceof HTMLInputElement || target instanceof HTMLSelectElement || target?.closest('dialog')) return;
    if (e.ctrlKey || e.altKey || e.metaKey) {
      if (e.ctrlKey && !e.altKey && e.key.toLowerCase() === 'c' && !getSelection()?.toString()) {
        e.preventDefault();
        copy();
      }
      return;
    }
    // a focused button already answers space and enter
    if (target instanceof HTMLButtonElement && (e.key === ' ' || e.key === 'Enter')) return;
    const keys: Record<string, () => void> = {
      ' ': toggle,
      k: toggle,
      j: () => skip(-5),
      l: () => skip(5),
      ArrowLeft: () => step(-1),
      ArrowRight: () => step(1),
      ',': () => step(-1),
      '.': () => step(1),
      '<': () => speed(-1),
      '>': () => speed(1),
      ArrowUp: () => loud(0.05),
      ArrowDown: () => loud(-0.05),
      i: () => (start = Math.max(lo, Math.min(snap(now), end - 1 / fps))),
      o: () => (end = Math.min(hi, Math.max(snap(now), start + 1 / fps))),
      s: () => !animated && split(),
      Delete: cutOut,
      m: mark,
      f: () => fill(),
      'Shift+p': () => prev && onopen(prev),
      'Shift+n': () => next && onopen(next),
      Escape: () => (full ? fill(false) : close())
    };
    const key = e.key.length === 1 ? e.key.toLowerCase() : e.key;
    const run = (e.shiftKey && keys[`Shift+${key}`]) || keys[key];
    if (!run) return;
    e.preventDefault();
    e.stopPropagation();
    run();
  }
</script>

<svelte:window onkeydown={keys} />

<div class="toolbar">
  <button class="gel back" title="Back (Esc)" onclick={close}>
    <svg viewBox="0 0 10 10" aria-hidden="true"><path d="M6.5 1.5 3 5l3.5 3.5" /></svg><span>Back</span>
  </button>
  <div class="gel-row steps">
    <button class="gel icon" disabled={!prev} aria-label="Previous clip" title="Previous clip (Shift+P)" onclick={() => prev && onopen(prev)}>
      <svg viewBox="0 0 16 16" aria-hidden="true"><path d="M10 3.5 5.5 8l4.5 4.5" /></svg>
    </button>
    <button class="gel icon" disabled={!next} aria-label="Next clip" title="Next clip (Shift+N)" onclick={() => next && onopen(next)}>
      <svg viewBox="0 0 16 16" aria-hidden="true"><path d="M6 3.5 10.5 8 6 12.5" /></svg>
    </button>
  </div>
  {#if naming !== null}
    <!-- svelte-ignore a11y_autofocus -->
    <input
      class="field rename"
      aria-label="Clip name"
      bind:value={naming}
      autofocus
      onkeydown={(e) => {
        if (e.key === 'Enter') rename();
        if (e.key === 'Escape') naming = null;
      }}
      onblur={rename}
    />
  {:else if rewind}
    <h1 class="toolbar-title">Rewind, the last {clock(probed)}</h1>
  {:else}
    <h1 class="toolbar-title" class:renamable={!clip.source} title={clip.source ? undefined : 'Double click to rename'} ondblclick={() => !clip.source && (naming = clip.name)}>
      {title(clip)}
    </h1>
  {/if}
  <span class="grow"></span>
  {#if rewind}
    <button class="gel" disabled={busy !== ''} title="Keep all of it as a clip" onclick={keepAll}>Keep all</button>
    <button class="gel danger" disabled={busy !== ''} onclick={remove}>Discard</button>
  {:else}
  <button class="gel" aria-pressed={clip.starred} onclick={() => invoke('star', { paths: [clip.path], on: !clip.starred })}>
    {clip.starred ? 'Starred' : 'Star'}
  </button>
  {#if clip.source}
    <span class="form-hint" title="Exports and frames go to the clips folder">In {clip.source}'s folder, clip only reads it</span>
  {:else}
    <button class="gel" onclick={() => (naming = clip.name)}>Rename</button>
    <button class="gel danger" disabled={busy !== ''} onclick={remove}>Delete</button>
  {/if}
  {/if}
</div>

<div class="theater" class:full>
  <div class="screen" bind:clientWidth={shown.width} bind:clientHeight={shown.height}>
    <!-- svelte-ignore a11y_media_has_caption -->
    <video
      bind:this={video}
      src={source}
      crossorigin="anonymous"
      muted
      preload="auto"
      onclick={toggle}
      ondblclick={() => fill()}
      onplay={played}
      onpause={stopped}
      onended={ended}
      onseeked={sound}
      onloadedmetadata={(e) => {
        duration = e.currentTarget.duration;
        picture = { width: e.currentTarget.videoWidth || 16, height: e.currentTarget.videoHeight || 9 };
        if (!clip.seconds || end > duration) end = duration;
      }}
    ></video>
    {#if portrait}
      <div class="shroud" style:left="{crop.x}px" style:top="{crop.top}px" style:width="{crop.left - crop.x}px" style:height="{crop.height}px"></div>
      <div class="shroud" style:left="{crop.left + crop.width}px" style:top="{crop.top}px" style:width="{crop.x + crop.span - crop.left - crop.width}px" style:height="{crop.height}px"></div>
      <div
        class="crop"
        role="slider"
        tabindex="0"
        aria-label="9:16 window"
        aria-valuemin={0}
        aria-valuemax={100}
        aria-valuenow={Math.round(middle * 100)}
        aria-valuetext="{Math.round(middle * 100)}% across"
        title="Drag to choose what the 9:16 copy shows"
        style:left="{crop.left}px"
        style:top="{crop.top}px"
        style:width="{crop.width}px"
        style:height="{crop.height}px"
        onpointerdown={slide}
        onkeydown={(e) => {
          const to = ({ ArrowLeft: middle - 0.02, ArrowRight: middle + 0.02, Home: 0, End: 1 } as Record<string, number>)[e.key];
          if (to === undefined) return;
          e.preventDefault();
          e.stopPropagation();
          middle = place(to);
        }}
      ></div>
    {/if}
  </div>

  <div class="dock">
    <div class="transport">
      <button class="gel" bind:this={play} onclick={toggle} aria-label={paused ? 'Play' : 'Pause'} title="{paused ? 'Play' : 'Pause'} (Space or K)">
        <svg viewBox="0 0 12 12" aria-hidden="true">
          {#if paused}<path d="M3 1.5v9l7.5-4.5z" />{:else}<path d="M2.5 1.5h2.5v9H2.5zM7 1.5h2.5v9H7z" />{/if}
        </svg>
      </button>
      <span class="mono time"><b>{clock(now, true)}</b> / {clock(duration, true)}</span>
      <select class="field rate" aria-label="Speed" title="Speed (&lt; and &gt;)" bind:value={rate}>
        {#each RATES as r (r)}<option value={r}>{r}x</option>{/each}
      </select>
      <button class="gel" aria-pressed={loop} title="Repeat the cut" onclick={() => (loop = !loop)}>Loop</button>
      <label class="volume" title="Volume (Up and Down)">
        <svg viewBox="0 0 16 16" aria-hidden="true">
          <path d="M2.5 6h2.5l3.5-3v10l-3.5-3H2.5z" />
          {#if view.volume > 0}<path d="M11 5.5a3.5 3.5 0 0 1 0 5" />{/if}
          {#if view.volume > 0.5}<path d="M12.8 3.6a6 6 0 0 1 0 8.8" />{/if}
        </svg>
        <input type="range" min="0" max="1" step="0.05" aria-label="Volume" aria-valuetext="{Math.round(view.volume * 100)}%" bind:value={view.volume} style:--pct={view.volume} />
      </label>
      <span class="grow"></span>
      <span class="form-hint">K plays, J and L skip, I and O cut, S splits, M marks, ? lists every key</span>
      <button class="gel" disabled={busy !== ''} title="Save this frame as a png beside the clip" onclick={still}>Save frame</button>
      <button class="gel icon" aria-pressed={full} aria-label="Fill the screen" title="{full ? 'Leave' : 'Fill'} the screen (F)" onclick={() => fill()}>
        <svg viewBox="0 0 16 16" aria-hidden="true">
          {#if full}<path d="M6 2.5V6H2.5M10 2.5V6h3.5M6 13.5V10H2.5M10 13.5V10h3.5" />{:else}<path d="M2.5 6V2.5H6M13.5 6V2.5H10M2.5 10v3.5H6M13.5 10v3.5H10" />{/if}
        </svg>
      </button>
    </div>
    <div class="tracks">
      <span class="lane-label mono">Time</span>
      <div class="timeline" bind:this={bar} role="presentation" onpointerdown={(e) => drag(e, 'head')}>
        {#each parts as [a, b] (a)}
          <div class="kept" class:other={a !== start} style:left={pct(a)} style:right={`calc(100% - ${pct(b)})`}></div>
        {/each}
        {#each clip.marks as at (at)}
          <button
            class="mark"
            style:left={pct(at)}
            aria-label="Mark at {clock(at, true)}"
            title="Marked at {clock(at, true)}, Del removes it"
            onpointerdown={(e) => e.stopPropagation()}
            onclick={() => (video?.pause(), seek(at))}
            onkeydown={(e) => {
              if (e.key !== 'Delete' && e.key !== 'Backspace') return;
              e.preventDefault();
              e.stopPropagation();
              unmark(at);
            }}
          ></button>
        {/each}
        <div class="handle" style:left={pct(start)} role="presentation" onpointerdown={(e) => drag(e, 'in')}></div>
        <div class="handle out" style:left={pct(end)} role="presentation" onpointerdown={(e) => drag(e, 'out')}></div>
        <div class="playhead" style:left={pct(now)}></div>
      </div>
      {#each lanes as lane, i (i)}
        <div class="lane-label" class:quiet={!audible.includes(i)}>
          <span class="lane-name">{lane.name}</span>
          <button class="lane-key" aria-pressed={lane.muted} aria-label="Mute {lane.name}" title="Mute" onclick={() => setLane(i, { muted: !lane.muted })}>M</button>
          <button class="lane-key" aria-pressed={lane.solo} aria-label="Solo {lane.name}" title="Solo" onclick={() => setLane(i, { solo: !lane.solo })}>S</button>
        </div>
        <div class="wave" class:quiet={!audible.includes(i)}>
          <svg aria-hidden="true" viewBox="0 0 {PEAKS} 100" preserveAspectRatio="none"><path d={lane.peaks} /></svg>
          {#each gaps as [a, b] (a)}
            <div class="shade" style:left={pct(a)} style:right={`calc(100% - ${pct(b)})`}></div>
          {/each}
          <div class="playhead" style:left={pct(now)}></div>
        </div>
      {:else}
        <span></span>
        <p class="form-hint">{loading ? 'Reading the audio tracks' : 'No audio tracks'}</p>
      {/each}
    </div>
  </div>

  <aside class="inspector">
    <section class="panel">
      <div class="panel-app">
        <Icon app={clip.app} icon={clip.icon} size={22} />
        <span>{clip.app}</span>
      </div>
      <dl class="facts">
        <div><dt>Saved</dt><dd>{day(clip.modified)} {at(clip.modified)}</dd></div>
        <div><dt>Length</dt><dd>{clock(duration, true)}</dd></div>
        <div><dt>Size</dt><dd>{size(clip.bytes)}</dd></div>
        <div><dt>Frame rate</dt><dd>{clip.fps ? `${Math.round(clip.fps)} fps` : 'unknown'}</dd></div>
        <div><dt>Tracks</dt><dd>{loading ? '...' : lanes.length}</dd></div>
      </dl>
      <div class="gel-row">
        <button class="gel" onclick={() => invoke('open', { path: clip.path })}>Open</button>
        <button class="gel" onclick={() => invoke('reveal', { path: clip.path })}>Show in folder</button>
        <button class="gel" title="Copy the file to paste into a chat or folder (Ctrl+C)" onclick={copy}>Copy file</button>
      </div>
    </section>

    <section class="panel">
      <h3 class="panel-head">Marks</h3>
      {#if clip.marks.length}
        <ul class="marks">
          {#each clip.marks as at (at)}
            <li>
              <button class="mark-time mono" title="Go to the mark" onclick={() => (video?.pause(), seek(at))}>{clock(at, true)}</button>
              <button class="lane-key" aria-label="Remove the mark at {clock(at, true)}" title="Remove" onclick={() => unmark(at)}>
                <svg viewBox="0 0 10 10" aria-hidden="true"><path d="M2.5 2.5l5 5M7.5 2.5l-5 5" /></svg>
              </button>
            </li>
          {/each}
        </ul>
      {:else}
        <p class="form-hint">M marks the moment under the playhead.</p>
      {/if}
      <button class="gel wide" onclick={mark}>Mark {clock(snap(now), true)}</button>
    </section>

    <section class="panel">
      <h3 class="panel-head">Export</h3>
      <dl class="facts cut-facts">
        <div><dt>In</dt><dd>{clock(start, true)}</dd></div>
        <div><dt>Out</dt><dd>{clock(end, true)}</dd></div>
        <div><dt>Keeps</dt><dd>{kept.toFixed(2)} s{parts.length > 1 ? ` in ${parts.length} parts` : ''}</dd></div>
      </dl>
      <div class="gel-row">
        <button class="gel" disabled={busy !== '' || animated} title="Split the part at the playhead, to cut out what's between (S)" onclick={split}>Split</button>
        <button class="gel" disabled={busy !== '' || parts.length < 2} title="Cut out the part the handles are on (Del)" onclick={cutOut}>Cut out this part</button>
      </div>
      <div class="option">
        <span id="format-label">Format</span>
        <div class="seg" role="radiogroup" aria-labelledby="format-label" use:radios>
          <button role="radio" aria-checked={!animated} disabled={busy !== ''} onclick={() => (animated = false)}>MP4</button>
          <button role="radio" aria-checked={animated} disabled={busy !== '' || parts.length > 1} title={parts.length > 1 ? 'GIFs keep one part' : undefined} onclick={() => (animated = true)}>GIF</button>
        </div>
      </div>
      <label class="option">
        <span>Size</span>
        <select class="field" bind:value={view.fit} disabled={busy !== ''}>
          {#each SIZES as s (s.bytes)}<option value={s.bytes}>{s.label}</option>{/each}
        </select>
      </label>
      {#if view.fit === -1}
        <label class="option">
          <span>Under</span>
          <input class="field mb" type="number" min="1" max="10000" step="1" bind:value={view.custom} disabled={busy !== ''} />
          <span class="form-hint">MB</span>
        </label>
      {/if}
      <div class="option">
        <span id="frame-label">Frame</span>
        <div class="seg" role="radiogroup" aria-labelledby="frame-label" use:radios>
          <button role="radio" aria-checked={!portrait} disabled={busy !== ''} onclick={() => (portrait = false)}>Original</button>
          <button role="radio" aria-checked={portrait} disabled={busy !== ''} onclick={() => (portrait = true)}>9:16</button>
        </div>
      </div>
      {#if !animated}
        <div class="option">
          <span id="audio-label">Audio</span>
          <div class="seg" role="radiogroup" aria-labelledby="audio-label" use:radios>
            <button role="radio" aria-checked={mix || fit > 0} disabled={busy !== ''} onclick={() => (mix = true)}>Mix to one</button>
            <button role="radio" aria-checked={!mix && !fit} disabled={busy !== '' || fit > 0} onclick={() => (mix = false)}>Keep tracks</button>
          </div>
        </div>
        {#if !rewind}
        <div class="option">
          <span id="save-label">Save as</span>
          <div class="seg" role="radiogroup" aria-labelledby="save-label" use:radios>
            <button role="radio" aria-checked={!replace} disabled={busy !== ''} onclick={() => (replace = false)}>New clip</button>
            <button role="radio" aria-checked={replace} disabled={busy !== '' || !!clip.source} onclick={() => (replace = true)}>Replace this one</button>
          </div>
        </div>
        {/if}
      {/if}
      {#if fit > 0}
        <label class="option">
          <input type="checkbox" bind:checked={view.copyAfter} disabled={busy !== ''} />
          <span>Copy the file when it's done</span>
        </label>
      {/if}
      <p class="form-hint">
        {#if animated}
          GIFs have no sound and play up to {LONGEST} s{fit > 0 ? ', at 10 frames a second to fit the size' : ''}.
        {:else}
          {audible.length} of {lanes.length} tracks go in{fit > 0 && audible.length > 1 ? ', mixed to one since most sites play only the first' : ''}.
        {/if}
        {#if portrait}Drag the frame on the picture to choose what shows.{/if}
        {#if replace && !animated}The original goes to the recycle bin, with an undo.{/if}
        {#if clip.hdr && !animated}HDR clips export as HEVC, which some sites and players can't show.{/if}
      </p>
      {#if long}<p class="form-error">The cut is {(end - start).toFixed(1)} s, trim it to {LONGEST} s or less for a GIF.</p>{/if}
      {#if progress !== null}
        <div class="exporting">
          <span class="meter" role="progressbar" aria-label="Export" aria-valuemin={0} aria-valuemax={100} aria-valuenow={progress}>
            <span style:width="{progress}%"></span>
          </span>
          <button class="gel" onclick={() => (animated ? (halted = true) : invoke('cancel_export'))}>Cancel</button>
        </div>
      {/if}
      <button class="gel default wide" disabled={busy !== '' || long} onclick={exportCut}>
        {busy === 'export' ? `Exporting ${progress ?? 0}%` : animated ? 'Export GIF' : replace ? 'Replace with the cut' : rewind ? 'Save this part' : 'Export'}
      </button>
      {#if error}<p class="form-error" role="alert">{error}</p>{/if}
    </section>
  </aside>
</div>
