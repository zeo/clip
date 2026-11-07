import { MediaQuery } from 'svelte/reactivity';

export const zooms = [90, 100, 110, 125, 150];

// how the window looks is this window's business, so it lives here and never in the recorder's settings
export const view = $state({ zoom: 100, previews: true, ...JSON.parse(localStorage.getItem('view') ?? '{}') });

$effect.root(() => {
  $effect(() => localStorage.setItem('view', JSON.stringify(view)));
});

export const reduced = new MediaQuery('prefers-reduced-motion: reduce');

export type Toast = { id: number; text: string; bad: boolean };

export const toasts = $state<Toast[]>([]);
let made = 0;

export function say(text: string, bad = false) {
  const id = ++made;
  toasts.push({ id, text, bad });
  setTimeout(() => toasts.splice(toasts.findIndex((t) => t.id === id), 1), bad ? 8000 : 4000);
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
