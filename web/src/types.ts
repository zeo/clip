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
  steamgriddb: string;
  hotkeys: Hotkey[];
  volumes: Record<string, number>;
  micDevice: string;
  pushToTalk: number;
  sound: boolean;
  soundVolume: number;
  notify: boolean;
  keepDays: number;
  openOnStart: boolean;
  closeToTray: boolean;
  ignore: string[];
  notice: boolean;
  noticeCorner: Corner;
};

export type Corner = 'top-left' | 'top-right' | 'bottom-left' | 'bottom-right';

// mods are MOD_ALT 1, MOD_CONTROL 2, MOD_SHIFT 4 and MOD_WIN 8, key is a virtual key code with 0 for none
export type Hotkey = { action: Action; mods: number; key: number; seconds: number };
export type Action = 'save' | 'mark' | 'shot' | 'mic' | 'record';

export const ACTIONS: Record<Action, { name: string; does: string; key: string }> = {
  save: { name: 'Save a clip', does: 'Saves what the buffer holds', key: 'Save' },
  mark: { name: 'Mark a moment', does: 'Marks now and saves 15 s later', key: 'Mark' },
  shot: { name: 'Take a screenshot', does: 'Saves the next frame as a png', key: 'Screenshot' },
  mic: { name: 'Mute the mic', does: 'Mutes or unmutes the mic in clips', key: 'Mic' },
  record: { name: 'Start or stop recording', does: 'Records until pressed again', key: 'Record' }
};

const ALT = 1;
const CTRL = 2;
const SHIFT = 4;
const F = (n: number) => 0x6f + n;
const hot = (action: Action, mods: number, key: number): Hotkey => ({ action, mods, key, seconds: 0 });

// each app's keys as it ships them, so people coming over keep their muscle memory
export const PRESETS: { name: string; hotkeys: Hotkey[] }[] = [
  { name: 'clip', hotkeys: [hot('save', ALT, F(9)), hot('mark', ALT, F(10))] },
  { name: 'Medal', hotkeys: [hot('save', 0, F(8)), hot('record', ALT, F(7))] },
  { name: 'NVIDIA App', hotkeys: [hot('save', ALT, F(10)), hot('record', ALT, F(9)), hot('shot', ALT, F(1))] },
  { name: 'SteelSeries Moments', hotkeys: [hot('save', ALT, 0x53), hot('mic', ALT, 0x4d)] },
  {
    name: 'AMD Software',
    hotkeys: [hot('save', CTRL | SHIFT, 0x53), hot('record', CTRL | SHIFT, 0x45), hot('shot', CTRL | SHIFT, 0x49)]
  }
];


export type Status = {
  recording: boolean;
  app: string;
  exe: string;
  message: string;
  paused: boolean;
  hotkeys: (Hotkey & { taken: boolean })[];
  mic_muted: boolean;
  // ms since 1970 a long recording started, 0 when none runs
  taping: number;
};

const named: Record<number, string> = {
  0x08: 'Backspace', 0x09: 'Tab', 0x0d: 'Enter', 0x13: 'Pause', 0x14: 'Caps Lock', 0x1b: 'Esc', 0x20: 'Space',
  0x21: 'Page Up', 0x22: 'Page Down', 0x23: 'End', 0x24: 'Home', 0x25: 'Left', 0x26: 'Up', 0x27: 'Right', 0x28: 'Down',
  0x2c: 'Print Screen', 0x2d: 'Insert', 0x2e: 'Delete', 0x6a: 'Num *', 0x6b: 'Num +', 0x6d: 'Num -', 0x6e: 'Num .',
  0x6f: 'Num /', 0x90: 'Num Lock', 0x91: 'Scroll Lock', 0xba: ';', 0xbb: '=', 0xbc: ',', 0xbd: '-', 0xbe: '.', 0xbf: '/',
  0xc0: '`', 0xdb: '[', 0xdc: '\\', 0xdd: ']', 0xde: "'"
};

export function keyName(vk: number): string {
  if (vk >= 0x70 && vk <= 0x87) return `F${vk - 0x6f}`;
  if ((vk >= 0x30 && vk <= 0x39) || (vk >= 0x41 && vk <= 0x5a)) return String.fromCharCode(vk);
  if (vk >= 0x60 && vk <= 0x69) return `Num ${vk - 0x60}`;
  return named[vk] ?? `Key ${vk}`;
}

export function chord(hotkey: Pick<Hotkey, 'mods' | 'key'>): string {
  if (!hotkey.key) return 'None';
  const mods = ([[2, 'Ctrl'], [4, 'Shift'], [1, 'Alt'], [8, 'Win']] as const).filter(([bit]) => hotkey.mods & bit);
  return [...mods.map(([, name]) => name), keyName(hotkey.key)].join('+');
}

// the first working key of an action, for labels that name it
export function keyFor(status: Status, action: Hotkey['action']): string | null {
  const hotkey = status.hotkeys.find((h) => h.action === action && h.key && !h.taken);
  return hotkey ? chord(hotkey) : null;
}

export type Disk = { free: number; total: number };

export type Mic = { id: string; name: string };

export function length(seconds: number): string {
  return seconds < 60 ? `${seconds} s` : `${seconds / 60} min`;
}

export type Clip = {
  path: string;
  app: string;
  name: string;
  bytes: number;
  modified: number;
  seconds: number;
  fps: number;
  hdr: boolean;
  starred: boolean;
  marks: number[];
  session: number;
  thumb: string | null;
  icon: string | null;
};

export type Game = { app: string; icon: string | null; clips: Clip[]; bytes: number };

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
  if (ms >= start.getTime()) return 'Today';
  if (ms >= start.getTime() - 864e5) return 'Yesterday';
  return date.format(ms);
}

// clips keep the time they were saved as their name until renamed
export function title(clip: Clip): string {
  return /^\d{4}-\d\d-\d\d \d\d-\d\d-\d\d$/.test(clip.name) ? at(clip.modified) : clip.name;
}

// clips arrive newest first, so games come out in order of their latest clip and keep their clips in that order
export function games(clips: Clip[]): Game[] {
  const found = new Map<string, Game>();
  for (const clip of clips) {
    let game = found.get(clip.app);
    if (!game) found.set(clip.app, (game = { app: clip.app, icon: clip.icon, clips: [], bytes: 0 }));
    game.clips.push(clip);
    game.bytes += clip.bytes;
  }
  return [...found.values()];
}

export function sessionCount(clips: Clip[]): number {
  return new Set(clips.map((clip) => clip.session)).size;
}

export type Query = {
  show: 'all' | 'starred' | 'today' | 'week';
  sort: 'newest' | 'oldest' | 'longest' | 'largest' | 'name' | 'game';
  search: string;
};

const orders: Partial<Record<Query['sort'], (a: Clip, b: Clip) => number>> = {
  oldest: (a, b) => a.modified - b.modified,
  longest: (a, b) => b.seconds - a.seconds,
  largest: (a, b) => b.bytes - a.bytes,
  name: (a, b) => title(a).localeCompare(title(b), undefined, { numeric: true }),
  game: (a, b) => a.app.localeCompare(b.app) || b.modified - a.modified
};

export function choose(clips: Clip[], query: Query): Clip[] {
  const since = { all: 0, starred: 0, today: new Date().setHours(0, 0, 0, 0), week: Date.now() - 7 * 864e5 }[query.show];
  const words = query.search.trim().toLowerCase();
  const kept = clips.filter(
    (clip) =>
      clip.modified >= since &&
      (query.show !== 'starred' || clip.starred) &&
      (!words || `${title(clip)} ${clip.name} ${clip.app}`.toLowerCase().includes(words))
  );
  const order = orders[query.sort];
  return order ? kept.sort(order) : kept;
}

export function sentence(text: string): string {
  return text.charAt(0).toUpperCase() + text.slice(1);
}
