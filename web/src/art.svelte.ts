import { invoke } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';

export type Art = { app: string; cover: string | null; hero: string | null; logo: string | null };

const found = $state<Record<string, Art>>({});
const asked = new Set<string>();
listen<Art>('art', (e) => (found[e.payload.app] = e.payload));

// each app is asked for once per window, and missing art arrives later as an event
export function art(app: string): Art | undefined {
  if (!asked.has(app)) {
    asked.add(app);
    invoke<Art>('game_art', { app }).then((got) => (found[app] ??= got));
  }
  return found[app];
}
