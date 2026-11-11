<script lang="ts">
  import { fileSrc, invoke } from './host';
  import Icon from './Icon.svelte';
  import { at, day, size, title, type Clip } from './types';
  import { discard, say } from './view.svelte';

  let { clip, prev, next, onopen, onclose }: { clip: Clip; prev?: Clip; next?: Clip; onopen: (clip: Clip) => void; onclose: () => void } = $props();
  let picture = $state<{ width: number; height: number } | null>(null);
  const gif = $derived(clip.path.toLowerCase().endsWith('.gif'));

  function run(command: string, args: Record<string, unknown>, done?: string) {
    invoke(command, args).then(
      () => done && say(done),
      (e) => say(String(e), true)
    );
  }

  const copy = () => run('copy_files', { paths: [clip.path] }, `Copied ${title(clip)}, paste into a chat or folder`);
  const star = () => run('star', { paths: [clip.path], on: !clip.starred });

  function remove() {
    discard([clip.path], title(clip));
    onclose();
  }

  function keys(e: KeyboardEvent) {
    const target = e.target instanceof Element ? e.target : null;
    if (target instanceof HTMLInputElement || target instanceof HTMLSelectElement || target?.closest('dialog')) return;
    if (e.ctrlKey && !e.altKey && !e.metaKey && e.key.toLowerCase() === 'c') {
      e.preventDefault();
      copy();
      return;
    }
    if (e.ctrlKey || e.altKey || e.metaKey) return;
    const keys: Record<string, () => void> = {
      ArrowLeft: () => prev && onopen(prev),
      ArrowRight: () => next && onopen(next),
      s: star,
      Delete: () => !clip.source && remove(),
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

<div class="toolbar">
  <button class="gel back" title="Back (Esc)" onclick={onclose}>
    <svg viewBox="0 0 10 10" aria-hidden="true"><path d="M6.5 1.5 3 5l3.5 3.5" /></svg><span>Back</span>
  </button>
  <div class="gel-row steps">
    <button class="gel icon" disabled={!prev} aria-label="Previous image" title="Previous image (Left)" onclick={() => prev && onopen(prev)}>
      <svg viewBox="0 0 16 16" aria-hidden="true"><path d="M10 3.5 5.5 8l4.5 4.5" /></svg>
    </button>
    <button class="gel icon" disabled={!next} aria-label="Next image" title="Next image (Right)" onclick={() => next && onopen(next)}>
      <svg viewBox="0 0 16 16" aria-hidden="true"><path d="M6 3.5 10.5 8 6 12.5" /></svg>
    </button>
  </div>
  <h1 class="toolbar-title">{title(clip)}</h1>
  <span class="grow"></span>
  <button class="gel" aria-pressed={clip.starred} title="Star (S)" onclick={star}>{clip.starred ? 'Starred' : 'Star'}</button>
  <button
    class="gel danger"
    disabled={!!clip.source}
    title={clip.source ? `It's in ${clip.source}'s folder, clip only reads it` : 'Delete (Del)'}
    onclick={remove}>Delete</button
  >
</div>

<div class="theater">
  <div class="screen">
    <img
      src={fileSrc(clip.path)}
      alt={title(clip)}
      draggable="false"
      onload={(e) => {
        const img = e.currentTarget as HTMLImageElement;
        picture = { width: img.naturalWidth, height: img.naturalHeight };
      }}
    />
  </div>
  <aside class="inspector">
    <section class="panel">
      <div class="panel-app">
        <Icon app={clip.app} icon={clip.icon} size={22} />
        <span>{clip.app}</span>
      </div>
      <dl class="facts">
        <div><dt>Saved</dt><dd>{day(clip.modified)} {at(clip.modified)}</dd></div>
        <div><dt>Kind</dt><dd>{gif ? 'GIF' : 'PNG'}</dd></div>
        <div><dt>Size</dt><dd>{size(clip.bytes)}</dd></div>
        <div><dt>Pixels</dt><dd>{picture ? `${picture.width} × ${picture.height}` : '...'}</dd></div>
        {#if clip.source}<div><dt>Folder</dt><dd>{clip.source}</dd></div>{/if}
      </dl>
      <div class="gel-row">
        <button class="gel" onclick={() => run('open', { path: clip.path })}>Open</button>
        <button class="gel" onclick={() => run('reveal', { path: clip.path })}>Show in folder</button>
        <button class="gel" title="Copy the file to paste into a chat or folder (Ctrl+C)" onclick={copy}>Copy file</button>
      </div>
    </section>
  </aside>
</div>
