<script lang="ts">
  import { onMount, untrack } from 'svelte';
  import { convertFileSrc, invoke } from '@tauri-apps/api/core';
  import { getCurrentWindow } from '@tauri-apps/api/window';
  import Icon from './Icon.svelte';
  import { at, clock, day, size, title, type Clip, type Track } from './types';
  import { discard, radios, say, view } from './view.svelte';

  let {
    clip,
    prev,
    next,
    onopen,
    onclose,
    onmoved
  }: {
    clip: Clip;
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

  let video = $state<HTMLVideoElement>();
  let bar = $state<HTMLElement>();
  let play = $state<HTMLButtonElement>();
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
  let error = $state('');
  let naming = $state<string | null>(null);
  let rate = $state(1);
  let loop = $state(false);
  let full = $state(false);
  const fps = $derived(clip.fps || 60);
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
    if (loop && now >= end) seek(start);
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
      seek(start);
      video?.play();
    }
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
    if (!video.paused) return video.pause();
    if (loop && (now < start || now >= end - 1 / fps)) seek(start);
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
    try {
      return await work();
    } catch (e) {
      error = String(e);
    } finally {
      busy = '';
    }
  }

  const named = (path: string) => path.split(/[\\/]/).pop();

  async function exportCut(discord: boolean) {
    const out = await act(discord ? 'fitting' : 'saving', () =>
      invoke<string>('export', { path: clip.path, start, end, tracks: audible, mix: mix && audible.length > 1, discord })
    );
    if (out) say(`Saved ${named(out)}`);
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
      i: () => (start = Math.min(snap(now), end - 1 / fps)),
      o: () => (end = Math.max(snap(now), start + 1 / fps)),
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
  {:else}
    <h1 class="toolbar-title renamable" title="Double click to rename" ondblclick={() => (naming = clip.name)}>{title(clip)}</h1>
  {/if}
  <span class="grow"></span>
  <button class="gel" aria-pressed={clip.starred} onclick={() => invoke('star', { paths: [clip.path], on: !clip.starred })}>
    {clip.starred ? 'Starred' : 'Star'}
  </button>
  <button class="gel" onclick={() => (naming = clip.name)}>Rename</button>
  <button class="gel danger" disabled={busy !== ''} onclick={remove}>Delete</button>
</div>

<div class="theater" class:full>
  <div class="screen">
    <!-- svelte-ignore a11y_media_has_caption -->
    <video
      bind:this={video}
      src={convertFileSrc(clip.path)}
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
        if (!clip.seconds || end > duration) end = duration;
      }}
    ></video>
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
        <input type="range" min="0" max="1" step="0.05" aria-label="Volume" aria-valuetext="{Math.round(view.volume * 100)}%" bind:value={view.volume} />
      </label>
      <span class="grow"></span>
      <span class="form-hint">K plays, J and L skip, I and O cut, M marks, ? lists every key</span>
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
        <div class="kept" style:left={pct(start)} style:right={`calc(100% - ${pct(end)})`}></div>
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
          <div class="shade" style:right={`calc(100% - ${pct(start)})`}></div>
          <div class="shade" style:left={pct(end)}></div>
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
      <h3 class="panel-head">Cut</h3>
      <dl class="facts cut-facts">
        <div><dt>In</dt><dd>{clock(start, true)}</dd></div>
        <div><dt>Out</dt><dd>{clock(end, true)}</dd></div>
        <div><dt>Keeps</dt><dd>{(end - start).toFixed(2)} s</dd></div>
      </dl>
      <div class="seg" role="radiogroup" aria-label="Audio in the copy" use:radios>
        <button role="radio" aria-checked={mix} onclick={() => (mix = true)}>Mix to one</button>
        <button role="radio" aria-checked={!mix} onclick={() => (mix = false)}>Keep tracks</button>
      </div>
      <p class="form-hint">{audible.length} of {lanes.length} tracks go in, the muted ones are left out</p>
      <button class="gel default wide" disabled={busy !== ''} onclick={() => exportCut(false)}>
        {busy === 'saving' ? 'Saving' : 'Save trimmed copy'}
      </button>
      <button class="gel wide" disabled={busy !== ''} onclick={() => exportCut(true)} title="Under 20 MB, one audio track">
        {busy === 'fitting' ? 'Fitting' : 'Copy for Discord'}
      </button>
      {#if error}<p class="form-error" role="alert">{error}</p>{/if}
    </section>
  </aside>
</div>
