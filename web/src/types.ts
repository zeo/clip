export type Settings = {
  buffer: number;
  fps: number;
  height: number;
  mbps: number;
  hdr10: boolean;
  mic: boolean;
  folder: string;
  startWithWindows: boolean;
};

export type Status = { recording: boolean; message: string };

export type Clip = { path: string; name: string; bytes: number; modified: number };

// desktop and voice tracks at 192 kbps each, plus the mic when it's on
export function ramMB(s: Settings): number {
  const tracks = 2 + (s.mic ? 1 : 0);
  return Math.round((s.buffer * (s.mbps * 1e6 + tracks * 192e3)) / 8 / 1e6);
}

export function size(bytes: number): string {
  return bytes >= 1e9 ? `${(bytes / 1e9).toFixed(2)} GB` : `${(bytes / 1e6).toFixed(1)} MB`;
}
