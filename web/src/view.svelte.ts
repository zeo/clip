import { invoke } from '@tauri-apps/api/core';
import { MediaQuery, SvelteSet } from 'svelte/reactivity';
import type { Query } from './types';

export const zooms = [90, 100, 110, 125, 150];
export const cards = [170, 220, 280, 340];

// how the window looks is this window's business, so it lives here and never in the recorder's settings
const stored = (() => {
  // a hand-edited or cut-off entry just starts the look over
  try {
    return JSON.parse(localStorage.getItem('view') ?? '{}') ?? {};
  } catch {
    return {};
  }
})();
export const view = $state({
  zoom: 100,
  previews: true,
  card: 220,
  list: false,
  sort: 'newest' as Query['sort'],
  volume: 1,
  // bytes to fit exports under, 0 for the clip's own quality and -1 for `custom` MB
  fit: 0,
  custom: 25,
  copyAfter: true,
  ...stored,
  side: { home: false, library: true, ...stored.side }
});

$effect.root(() => {
  $effect(() => localStorage.setItem('view', JSON.stringify(view)));
});

export const reduced = new MediaQuery('prefers-reduced-motion: reduce');

// what the punctuation keys print on this keyboard layout, by virtual key
export const keys = $state<Record<number, string>>({});
invoke<Record<number, string>>('key_names').then((names) => Object.assign(keys, names));

export type Toast = { id: number; text: string; bad: boolean; undo?: () => void };

export const toasts = $state<Toast[]>([]);
let made = 0;

const drop = (id: number) => {
  const at = toasts.findIndex((t) => t.id === id);
  if (at >= 0) toasts.splice(at, 1);
};

export function say(text: string, bad = false, undo?: () => void) {
  const id = ++made;
  toasts.push({ id, text, bad, undo: undo && (() => (drop(id), undo())) });
  // a burst of quick actions keeps the latest few on screen, and every undo stays reachable
  while (toasts.length > 3 && toasts.some((t) => !t.undo)) toasts.splice(toasts.findIndex((t) => !t.undo), 1);
  setTimeout(() => drop(id), bad || undo ? 8000 : 4000);
}

// clips waiting out their undo, hidden until the recycle bin has them or they come back
export const gone = new SvelteSet<string>();

// nothing is deleted before the undo runs out, so quitting first keeps every clip
export function discard(paths: string[], what: string) {
  for (const path of paths) gone.add(path);
  const timer = setTimeout(
    () =>
      invoke('delete', { paths }).catch((e) => {
        for (const path of paths) gone.delete(path);
        say(String(e), true);
      }),
    8000
  );
  say(`Deleted ${what}`, false, () => {
    clearTimeout(timer);
    for (const path of paths) gone.delete(path);
  });
}

// arrow keys move the choice along a radiogroup, and only the chosen radio takes a tab stop
export function radios(group: HTMLElement) {
  const items = () => [...group.querySelectorAll<HTMLElement>('[role="radio"]')];
  const sync = () => {
    const all = items();
    const on = all.find((r) => r.getAttribute('aria-checked') === 'true') ?? all[0];
    for (const r of all) r.tabIndex = r === on ? 0 : -1;
  };
  const keydown = (e: KeyboardEvent) => {
    const step = ({ ArrowRight: 1, ArrowDown: 1, ArrowLeft: -1, ArrowUp: -1 } as Record<string, number>)[e.key];
    const all = items();
    const at = all.indexOf(e.target as HTMLElement);
    if (!step || at < 0) return;
    // the clip page would take the arrows as frame steps
    e.preventDefault();
    e.stopPropagation();
    const next = all[(at + step + all.length) % all.length];
    next.focus();
    next.click();
  };
  const watch = new MutationObserver(sync);
  watch.observe(group, { subtree: true, childList: true, attributeFilter: ['aria-checked'] });
  sync();
  group.addEventListener('keydown', keydown);
  return {
    destroy() {
      watch.disconnect();
      group.removeEventListener('keydown', keydown);
    }
  };
}
