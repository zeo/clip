<script lang="ts">
  import { invoke } from '@tauri-apps/api/core';
  import { SvelteSet } from 'svelte/reactivity';
  import Thumb from './Thumb.svelte';
  import { at, clock, day, size, title, type Clip, type Query } from './types';
  import { cards, discard, reduced, say, view } from './view.svelte';

  let {
    clips: listed,
    onopen,
    showApp = true,
    grouped = true
  }: { clips: Clip[]; onopen: (clip: Clip, list: Clip[]) => void; showApp?: boolean; grouped?: boolean } = $props();
  let hovered = $state<string | null>(null);
  let box = $state<HTMLElement>();
  const uid = $props.id();
  // the one clip in the tab order, the first until another takes focus
  let focused = $state<string | null>(null);
  const selected = new SvelteSet<string>();
  // where a shift click's range starts
  let anchor: string | null = null;
  const columns: [string, Query['sort'] | null][] = [
    ['Name', 'name'],
    ['Game', 'game'],
    ['Saved', 'newest'],
    ['Length', 'longest'],
    ['Size', 'largest'],
    ['Marks', null]
  ];

  // clips come newest first and each session's clips sit together in that order, unless sorted otherwise
  const sessions = $derived.by(() => {
    if (!grouped) return listed.length > 0 ? [listed] : [];
    const groups = new Map<number, Clip[]>();
    for (const clip of listed) {
      const group = groups.get(clip.session);
      if (group) group.push(clip);
      else groups.set(clip.session, [clip]);
    }
    return [...groups.values()];
  });
  // the order they're drawn in, which every key, range and next clip follows
  const clips = $derived(sessions.flat());
  const picked = $derived(clips.filter((clip) => selected.has(clip.path)));
  const allStarred = $derived(picked.length > 0 && picked.every((clip) => clip.starred));
  const stop = $derived(clips.some((clip) => clip.path === focused) ? focused : (clips[0]?.path ?? null));

  const when = (clip: Clip) => `${day(clip.modified)} ${at(clip.modified)}`;

  async function act(command: string, args: Record<string, unknown>) {
    try {
      await invoke(command, args);
    } catch (e) {
      say(String(e), true);
    }
  }

  function pick(clip: Clip, range: boolean) {
    const from = anchor === null ? -1 : clips.findIndex((c) => c.path === anchor);
    const to = clips.indexOf(clip);
    if (range && from >= 0) {
      for (const c of clips.slice(Math.min(from, to), Math.max(from, to) + 1)) selected.add(c.path);
    } else if (selected.has(clip.path)) {
      selected.delete(clip.path);
    } else {
      selected.add(clip.path);
    }
    anchor = clip.path;
  }

  function copy(files: Clip[]) {
    invoke('copy_files', { paths: files.map((clip) => clip.path) }).then(
      () => say(`Copied ${files.length === 1 ? title(files[0]) : `${files.length} clips`}, paste into a chat or folder`),
      (e) => say(String(e), true)
    );
  }

  function remove(chosen: Clip[]) {
    const gone = chosen.filter((clip) => !clip.source);
    if (gone.length < chosen.length) say(`Clips in other apps' folders stay, clip only reads them`, true);
    if (gone.length === 0) return;
    const next = clips.find((clip, i) => !gone.includes(clip) && i > clips.indexOf(gone[gone.length - 1])) ?? clips.find((clip) => !gone.includes(clip));
    discard(
      gone.map((clip) => clip.path),
      gone.length === 1 ? title(gone[0]) : `${gone.length} clips`
    );
    for (const clip of gone) selected.delete(clip.path);
    if (next && box?.contains(document.activeElement)) move(next.path);
  }

  function move(path: string) {
    focused = path;
    box?.querySelector<HTMLElement>(`[data-path="${CSS.escape(path)}"]`)?.focus();
  }

  // up and down go to the nearest card in the row above or below, as drawn, so they cross sessions too
  function vertical(from: HTMLElement, dir: 1 | -1, far = 0): HTMLElement {
    const here = from.getBoundingClientRect();
    const top = (el: HTMLElement) => Math.round(el.getBoundingClientRect().top);
    const past = [...box!.querySelectorAll<HTMLElement>('[role="option"]')].filter((el) => dir * (top(el) - here.top) > 1);
    const rows = [...new Set(past.map(top))].sort((a, b) => dir * (a - b));
    const row = rows.find((t) => Math.abs(t - here.top) >= far) ?? rows[rows.length - 1];
    const x = (el: HTMLElement) => Math.abs(el.getBoundingClientRect().left - here.left);
    return past.filter((el) => top(el) === row).reduce((best, el) => (x(el) < x(best) ? el : best), from);
  }

  function keys(e: KeyboardEvent) {
    const el = (e.target as HTMLElement).closest<HTMLElement>('[role="option"]');
    const clip = el && clips.find((c) => c.path === el.dataset.path);
    if (!el || !clip) return;
    const i = clips.indexOf(clip);
    const page = (box?.closest('.view')?.clientHeight ?? 600) * 0.8;
    const mod = e.ctrlKey || e.metaKey;
    const to =
      e.altKey || mod
        ? undefined
        : ({
            ArrowLeft: () => clips[i - 1]?.path,
            ArrowRight: () => clips[i + 1]?.path,
            ArrowUp: () => vertical(el, -1).dataset.path,
            ArrowDown: () => vertical(el, 1).dataset.path,
            PageUp: () => vertical(el, -1, page).dataset.path,
            PageDown: () => vertical(el, 1, page).dataset.path,
            Home: () => clips[0].path,
            End: () => clips[clips.length - 1].path
          } as Record<string, () => string | undefined>)[e.key];
    if (to) {
      e.preventDefault();
      const path = to();
      if (!path) return;
      move(path);
      if (!e.shiftKey) return;
      anchor ??= clip.path;
      pick(clips.find((c) => c.path === path)!, true);
      return;
    }
    if (mod && e.key.toLowerCase() === 'a') {
      e.preventDefault();
      for (const c of clips) selected.add(c.path);
    } else if (mod && e.key.toLowerCase() === 'c' && !e.altKey) {
      e.preventDefault();
      copy(picked.length > 0 ? picked : [clip]);
    } else if (e.key === 'Enter' && !mod) {
      onopen(clip, clips);
    } else if (e.key === ' ' && !mod) {
      e.preventDefault();
      pick(clip, e.shiftKey);
    } else if (e.key.toLowerCase() === 's' && !mod && !e.altKey) {
      act('star', { paths: [clip.path], on: !clip.starred });
    } else if (e.key === 'Delete' && !mod) {
      remove(picked.length > 0 ? picked : [clip]);
    }
  }

  function wheel(e: WheelEvent) {
    if (!e.ctrlKey || view.list) return;
    e.preventDefault();
    const at = cards.indexOf(view.card);
    view.card = cards[Math.min(cards.length - 1, Math.max(0, (at < 0 ? 1 : at) + (e.deltaY < 0 ? 1 : -1)))];
  }

  function sortBy(sort: Query['sort']) {
    view.sort = sort === 'newest' && view.sort === 'newest' ? 'oldest' : sort;
  }
</script>

{#if view.list}
  <div class="list-head" class:bare={!showApp} role="toolbar" aria-label="Sort by">
    <span></span>
    <span></span>
    {#each columns as [label, sort] (label)}
      {#if label !== 'Game' || showApp}
        {#if sort}
          {@const on = view.sort === sort || (sort === 'newest' && view.sort === 'oldest')}
          <button class:on aria-pressed={on} onclick={() => sortBy(sort)}>
            {label}{#if on}<span aria-hidden="true">{view.sort === 'oldest' ? ' ▴' : ' ▾'}</span>{/if}
          </button>
        {:else}
          <span>{label}</span>
        {/if}
      {/if}
    {/each}
    <span></span>
  </div>
{/if}

<!-- the pick and star spots are for the pointer, space and s do the same from the keys -->
<!-- the cards take the keys, the box only hands them on -->
<div
  class="sessions"
  class:listing={view.list}
  class:tight={view.card === cards[0]}
  class:bare={!showApp}
  style:--card="{view.card}px"
  role="listbox"
  aria-multiselectable="true"
  aria-label="Clips"
  tabindex="-1"
  bind:this={box}
  onkeydown={keys}
  onwheel={wheel}
>
  {#each sessions as session, n (session[0].path)}
    {@const newest = session[0]}
    {@const oldest = session[session.length - 1]}
    {@const span = `${day(oldest.modified)} ${at(oldest.modified)}${newest !== oldest ? ` to ${at(newest.modified)}` : ''}`}
    <section class="session" role={grouped ? 'group' : 'presentation'} aria-labelledby={grouped ? `${uid}-${n}` : undefined}>
      {#if grouped}
        <div class="session-head" id="{uid}-{n}">
          {#if showApp}<span>{newest.app}</span>{/if}
          <span class:session-when={showApp}>{span}</span>
          <span class="mono">{session.length} {session[0].kind === 'image' ? 'image' : 'clip'}{session.length === 1 ? '' : 's'}</span>
        </div>
      {/if}
      <ul class="grid" class:picking={picked.length > 0} role="presentation">
        {#each session as clip (clip.path)}
          <!-- svelte-ignore a11y_click_events_have_key_events -->
          <li
            class="clip"
            class:picked={selected.has(clip.path)}
            role="option"
            aria-selected={selected.has(clip.path)}
            tabindex={clip.path === stop ? 0 : -1}
            data-path={clip.path}
            onfocus={() => (focused = clip.path)}
            onpointerenter={() => (hovered = clip.path)}
            onpointerleave={() => hovered === clip.path && (hovered = null)}
            onclick={(e) => (picked.length > 0 || e.shiftKey || e.ctrlKey ? pick(clip, e.shiftKey) : onopen(clip, clips))}
          >
            <!-- the title comes first, so it starts the name a screen reader gives the card, and css draws the still above it -->
            <span class="clip-name">{title(clip)}</span>
            {#if view.list}
              {#if showApp}<span class="clip-cell">{clip.app}{#if clip.source}<span class="clip-source">{clip.source}</span>{/if}</span>{/if}
              <span class="clip-cell">{when(clip)}</span>
              <span class="clip-cell mono">{clip.kind === 'clip' ? clock(clip.seconds) : ''}</span>
              <span class="clip-cell mono">{size(clip.bytes)}</span>
              <span class="clip-cell mono">{#if clip.marks.length}{clip.marks.length}<span class="spoken"> marks</span>{/if}</span>
            {:else}
              <span class="clip-meta mono">
                {#if clip.marks.length}<span class="clip-marks"><span aria-hidden="true">◆</span>{clip.marks.length}<span class="spoken"> marks</span></span>{/if}
                {#if clip.source}<span class="clip-source" title="In {clip.source}'s folder, clip only reads it">{clip.source}</span>{/if}
                <span class="clip-size">{size(clip.bytes)}</span>
              </span>
              {#if !grouped}<span class="clip-sub">{showApp ? `${clip.app}, ` : ''}{when(clip)}</span>{/if}
            {/if}
            <Thumb {clip} badge={!view.list} live={!view.list && view.previews && !reduced.current && hovered === clip.path} />
            {#if clip.starred}<span class="spoken">, starred</span>{/if}
            <!-- svelte-ignore a11y_click_events_have_key_events, a11y_no_static_element_interactions -->
            <span
              class="pick"
              aria-hidden="true"
              onclick={(e) => {
                e.stopPropagation();
                pick(clip, e.shiftKey);
              }}
            >
              <svg viewBox="0 0 16 16"><path d="M3.5 8.5l3 3 6-7" /></svg>
            </span>
            <!-- svelte-ignore a11y_click_events_have_key_events, a11y_no_static_element_interactions -->
            <span
              class="star"
              class:on={clip.starred}
              aria-hidden="true"
              onclick={(e) => {
                e.stopPropagation();
                act('star', { paths: [clip.path], on: !clip.starred });
              }}
            >
              <svg viewBox="0 0 16 16"><path d="M8 1.6l1.9 4 4.3.5-3.2 3 .9 4.3L8 11.2l-3.9 2.2.9-4.3-3.2-3 4.3-.5z" /></svg>
            </span>
          </li>
        {/each}
      </ul>
    </section>
  {/each}
</div>
{#if picked.length > 0}
  <div class="selection" role="toolbar" aria-label="Selected clips">
    <span class="mono">{picked.length} selected, {size(picked.reduce((total, clip) => total + clip.bytes, 0))}</span>
    <span class="grow"></span>
    {#if picked.length < clips.length}<button class="gel" onclick={() => clips.forEach((clip) => selected.add(clip.path))}>Select all <kbd>Ctrl+A</kbd></button>{/if}
    <button class="gel" onclick={() => act('star', { paths: picked.map((clip) => clip.path), on: !allStarred })}>
      {allStarred ? 'Unstar' : 'Star'}
    </button>
    <button class="gel" onclick={() => copy(picked)}>Copy <kbd>Ctrl+C</kbd></button>
    <button class="gel danger" onclick={() => remove(picked)}>Delete <kbd>Del</kbd></button>
    <button class="gel" onclick={() => selected.clear()}>Clear <kbd>Esc</kbd></button>
  </div>
{/if}

<svelte:window
  onkeydowncapture={(e) => {
    if (e.key !== 'Escape' || selected.size === 0 || (e.target instanceof Element && e.target.closest('dialog'))) return;
    // the page behind would take esc as back too
    e.preventDefault();
    selected.clear();
  }}
/>
