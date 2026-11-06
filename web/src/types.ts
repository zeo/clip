export type Settings = {
  buffer: number;
  fps: number;
  height: number;
  mbps: number;
  hdr10: boolean;
  mic: boolean;
  apps: string[];
  record: string[];
  folder: string;
  startWithWindows: boolean;
  cap: number;
};

export type Status = { recording: boolean; app: string; message: string };

export type Clip = {
  path: string;
  app: string;
  name: string;
  bytes: number;
  modified: number;
  seconds: number;
  fps: number;
  starred: boolean;
  session: number;
  thumb: string | null;
};

export type Track = { name: string; path: string };

// the app and everything else at 192 kbps each, plus every app split out and the mic when it's on
export function ramMB(s: Settings): number {
  const tracks = 2 + s.apps.length + (s.mic ? 1 : 0);
  return Math.round((s.buffer * (s.mbps * 1e6 + tracks * 192e3)) / 8 / 1e6);
}

export function size(bytes: number): string {
  return bytes >= 1e9 ? `${(bytes / 1e9).toFixed(2)} GB` : `${(bytes / 1e6).toFixed(1)} MB`;
}

export function clock(seconds: number, hundredths = false): string {
  const m = Math.floor(seconds / 60);
  const s = seconds - m * 60;
  return `${m}:${hundredths ? s.toFixed(2).padStart(5, '0') : String(Math.floor(s)).padStart(2, '0')}`;
}

const hour = new Intl.DateTimeFormat(undefined, { hour: '2-digit', minute: '2-digit' });
const date = new Intl.DateTimeFormat(undefined, { month: 'short', day: 'numeric' });

export function at(ms: number): string {
  return hour.format(ms);
}

export function day(ms: number): string {
  const start = new Date();
  start.setHours(0, 0, 0, 0);
  if (ms >= start.getTime()) return 'today';
  if (ms >= start.getTime() - 864e5) return 'yesterday';
  return date.format(ms);
}

// clips keep the time they were saved as their name until renamed
export function title(clip: Clip): string {
  return /^\d{4}-\d\d-\d\d \d\d-\d\d-\d\d$/.test(clip.name) ? at(clip.modified) : clip.name;
}
