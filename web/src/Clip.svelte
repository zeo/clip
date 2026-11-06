<script lang="ts">
  import { onMount, untrack } from 'svelte';
  import { convertFileSrc, invoke } from '@tauri-apps/api/core';
  import { at, clock, day, size, title, type Clip, type Track } from './types';

  let { clip, onclose, onmoved }: { clip: Clip; onclose: () => void; onmoved: (path: string) => void } = $props();

  type Lane = { name: string; buffer: AudioBuffer; peaks: string; muted: boolean; solo: boolean };
  const PEAKS = 800;
  // past this the tracks are restarted where the video is
  const DRIFT = 0.04;

  let video = $state<HTMLVideoElement>();
  let bar = $state<HTMLElement>();
  // the page is keyed by path, so the probe's length only seeds these
  const probed = untrack(() => clip.seconds);
  let duration = $state(probed);
  let now = $state(0);
  let paused = $state(true);
  let start = $state(0);
  let end = $state(probed);
  let lanes = $state<Lane[]>([]);
  let loading = $state(true);
  let mix = $state(true);
  let busy = $state('');
  let note = $state('');
  let error = $state('');
  let naming = $state<string | null>(null);
  const fps = $derived(clip.fps || 60);
  const soloed = $derived(lanes.some((lane) => lane.solo));
  const audible = $derived(lanes.flatMap((lane, i) => ((soloed ? lane.solo : !lane.muted) ? [i] : [])));
  const pct = (t: number) => `${(100 * t) / (duration || 1)}%`;

  const context = new AudioContext();
  let playing: AudioBufferSourceNode[] = [];
  let anchor = 0;
  let frame = 0;

  onMount(() => {
    load();
    return () => {
      cancelAnimationFrame(frame);
      silence();
      context.close();
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
    const from = video.currentTime;
    anchor = context.currentTime - from;
    for (const i of audible) {
      const buffer = lanes[i].buffer;
      if (from >= buffer.duration) continue;
      const source = context.createBufferSource();
      source.buffer = buffer;
      source.connect(context.destination);
      source.start(0, from);
      playing.push(source);
    }
  }

  function follow() {
    if (!video) return;
    now = video.currentTime;
    if (playing.length > 0 && Math.abs(context.currentTime - anchor - now) > DRIFT) sound();
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

  const snap = (t: number) => Math.min(duration, Math.max(0, Math.round(t * fps) / fps));

  function seek(t: number) {
    if (!video) return;
    // a hair past the frame's start so the webview shows that frame, not the one before
    video.currentTime = snap(t) + 0.25 / fps;
    now = snap(t);
  }

  function toggle() {
    if (!video) return;
    if (video.paused) video.play();
    else video.pause();
  }

  function setLane(i: number, change: Partial<Pick<Lane, 'muted' | 'solo'>>) {
    Object.assign(lanes[i], change);
    sound();
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
      if (what === 'in') start = Math.min(t, end - 1 / fps);
      else if (what === 'out') end = Math.max(t, start + 1 / fps);
      else seek(t);
    };
    move(event);
    target.onpointermove = move;
    target.onpointerup = () => (target.onpointermove = null);
  }

  async function act<T>(label: string, work: () => Promise<T>): Promise<T | undefined> {
    busy = label;
    error = '';
    note = '';
    try {
      return await work();
    } catch (e) {
      error = String(e);
    } finally {
      busy = '';
    }
  }

  async function exportCut(discord: boolean) {
    const out = await act(discord ? 'fitting' : 'saving', () =>
      invoke<string>('export', { path: clip.path, start, end, tracks: audible, mix: mix && audible.length > 1, discord })
    );
    if (out) note = `saved ${out.split(/[\\/]/).pop()}`;
  }

  async function rename() {
    if (naming === null) return;
    const name = naming.trim();
    naming = null;
    if (!name || name === clip.name) return;
    const to = await act('renaming', () => invoke<string>('rename', { path: clip.path, name }));
    if (to) onmoved(to);
  }

  async function remove() {
    video?.pause();
    if (await act('deleting', () => invoke('delete', { paths: [clip.path] }).then(() => true))) onclose();
  }

  function keys(e: KeyboardEvent) {
    if (e.target instanceof HTMLInputElement) return;
    const keys: Record<string, () => void> = {
      ' ': toggle,
      ArrowLeft: () => (video?.pause(), seek(now - 1 / fps)),
      ArrowRight: () => (video?.pause(), seek(now + 1 / fps)),
      i: () => (start = Math.min(snap(now), end - 1 / fps)),
      o: () => (end = Math.max(snap(now), start + 1 / fps)),
      Escape: onclose
    };
    const run = keys[e.key.length === 1 ? e.key.toLowerCase() : e.key];
    if (!run) return;
    e.preventDefault();
    e.stopPropagation();
    run();
  }
</script>

<svelte:window onkeydown={keys} />

<div class="view cut">
  <div class="view-head">
    {#if naming !== null}
      <!-- svelte-ignore a11y_autofocus -->
      <input
        class="rename proper"
        bind:value={naming}
        autofocus
        onkeydown={(e) => {
          if (e.key === 'Enter') rename();
          if (e.key === 'Escape') naming = null;
        }}
        onblur={rename}
      />
    {:else}
      <h1 class="proper">{title(clip)}</h1>
    {/if}
    <p class="view-sub">
      <span class="proper">{clip.app}</span>, {day(clip.modified)} {at(clip.modified)}, {size(clip.bytes)}{clip.fps ? `, ${Math.round(clip.fps)} fps` : ''}
    </p>
  </div>

  <div class="screen">
    <!-- svelte-ignore a11y_media_has_caption -->
    <video
      bind:this={video}
      src={convertFileSrc(clip.path)}
      muted
      preload="auto"
      onclick={toggle}
      onplay={played}
      onpause={stopped}
      onended={stopped}
      onseeked={sound}
      onloadedmetadata={(e) => {
        duration = e.currentTarget.duration;
        if (!clip.seconds || end > duration) end = duration;
      }}
    ></video>
  </div>

  <div class="transport">
    <button class="btn" onclick={toggle} aria-label={paused ? 'play' : 'pause'}>
      <svg class="glyph" viewBox="0 0 12 12">
        {#if paused}<path d="M3 1.5v9l7.5-4.5z" />{:else}<path d="M2.5 1.5h2.5v9H2.5zM7 1.5h2.5v9H7z" />{/if}
      </svg>
    </button>
    <span class="mono readout"><span class="hi">{clock(now, true)}</span> / {clock(duration, true)}</span>
    <span class="sep">|</span>
    <span class="mono readout">in {clock(start, true)} out {clock(end, true)}, <span class="hi">{(end - start).toFixed(2)} s</span></span>
    <span class="grow"></span>
    <div class="segmented" role="radiogroup" aria-label="audio in the copy">
      <button role="radio" aria-checked={mix} onclick={() => (mix = true)}>mix to one track</button>
      <button role="radio" aria-checked={!mix} onclick={() => (mix = false)}>keep tracks</button>
    </div>
    <button class="btn" disabled={busy !== ''} onclick={() => exportCut(true)} title="under 20 MB, one audio track">
      {busy === 'fitting' ? 'fitting' : 'copy for discord'}
    </button>
    <button class="btn primary" disabled={busy !== ''} onclick={() => exportCut(false)}>
      {busy === 'saving' ? 'saving' : 'save trimmed copy'}
    </button>
  </div>

  <div class="tracks">
    <span class="lane-label mono">time</span>
    <div class="timeline" bind:this={bar} role="presentation" onpointerdown={(e) => drag(e, 'head')}>
      <div class="kept" style:left={pct(start)} style:right={`calc(100% - ${pct(end)})`}></div>
      {#each clip.marks as at (at)}
        <button
          class="mark"
          style:left={pct(at)}
          aria-label="mark at {clock(at, true)}"
          title="alt+f10 at {clock(at, true)}"
          onpointerdown={(e) => e.stopPropagation()}
          onclick={() => (video?.pause(), seek(at))}
        ></button>
      {/each}
      <div class="handle" style:left={pct(start)} role="presentation" onpointerdown={(e) => drag(e, 'in')}></div>
      <div class="handle out" style:left={pct(end)} role="presentation" onpointerdown={(e) => drag(e, 'out')}></div>
      <div class="playhead" style:left={pct(now)}></div>
    </div>
    {#each lanes as lane, i (i)}
      <div class="lane-label" class:quiet={!audible.includes(i)}>
        <span class="proper lane-name">{lane.name}</span>
        <button class="lane-key" aria-pressed={lane.muted} onclick={() => setLane(i, { muted: !lane.muted })}>m</button>
        <button class="lane-key" aria-pressed={lane.solo} onclick={() => setLane(i, { solo: !lane.solo })}>s</button>
      </div>
      <div class="wave" class:quiet={!audible.includes(i)}>
        <svg viewBox="0 0 {PEAKS} 100" preserveAspectRatio="none"><path d={lane.peaks} /></svg>
        <div class="dim" style:right={`calc(100% - ${pct(start)})`}></div>
        <div class="dim" style:left={pct(end)}></div>
        <div class="playhead" style:left={pct(now)}></div>
      </div>
    {:else}
      <span></span>
      <p class="form-hint">{loading ? 'reading the audio tracks' : 'no audio tracks'}</p>
    {/each}
  </div>

  <div class="btn-row actions">
    <button class="btn ghost" onclick={onclose}>back <kbd>esc</kbd></button>
    <span class="grow"></span>
    {#if note}<span class="form-hint proper">{note}</span>{/if}
    {#if error}<span class="form-error">{error}</span>{/if}
    <button class="btn" onclick={() => invoke('open', { path: clip.path })}>open</button>
    <button class="btn" onclick={() => invoke('reveal', { path: clip.path })}>show in folder</button>
    <button class="btn" aria-pressed={clip.starred} onclick={() => invoke('star', { paths: [clip.path], on: !clip.starred })}>
      {clip.starred ? 'starred' : 'star'}
    </button>
    <button class="btn" onclick={() => (naming = clip.name)}>rename</button>
    <button class="btn ghost danger" disabled={busy !== ''} onclick={remove}>delete</button>
  </div>
  <p class="form-hint keys">space plays, arrows step one frame, i and o set the cut, drag the handles to trim, the green ticks are alt+f10 marks</p>
</div>
