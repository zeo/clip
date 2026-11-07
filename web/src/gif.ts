// animated gifs drawn from a clip in the webview, so nothing new ships in the binary

export const LONGEST = 15;
export const CANCELLED = 'the export was cancelled';

export type Cut = { start: number; end: number; middle: number | null; fit: number };

// biggest edge in px and frames a second, best first
const RUNGS = [
  { edge: 480, fps: 15 },
  { edge: 480, fps: 10 },
  { edge: 360, fps: 10 },
  { edge: 320, fps: 10 }
];
const CLEAR = 255;
// 4x4 ordered dither, a fixed pattern keeps still areas the same from frame to frame
const BAYER = [0, 8, 2, 10, 12, 4, 14, 6, 3, 11, 1, 9, 15, 7, 13, 5].map((v) => (v - 7.5) * 1.2);

class Bytes {
  data = new Uint8Array(1 << 20);
  length = 0;
  room(n: number) {
    if (this.length + n <= this.data.length) return;
    const grown = new Uint8Array(Math.max(this.data.length * 2, this.length + n));
    grown.set(this.data);
    this.data = grown;
  }
  byte(b: number) {
    this.room(1);
    this.data[this.length++] = b;
  }
  put(...bytes: number[]) {
    for (const b of bytes) this.byte(b);
  }
  bytes(bytes: Uint8Array) {
    this.room(bytes.length);
    this.data.set(bytes, this.length);
    this.length += bytes.length;
  }
  word(n: number) {
    this.put(n & 255, n >> 8);
  }
  view = () => this.data.subarray(0, this.length);
}

export async function gif(src: string, cut: Cut, progress: (pct: number) => void, cancelled: () => boolean) {
  const video = document.createElement('video');
  video.crossOrigin = 'anonymous';
  video.muted = true;
  video.preload = 'auto';
  video.src = src;
  try {
    await new Promise((done, fail) => {
      video.onloadeddata = done;
      video.onerror = () => fail("couldn't open the clip");
    });
    let rung = cut.fit ? 1 : 0;
    for (;;) {
      const { edge, fps } = RUNGS[rung];
      const out = await encode(video, cut, edge, fps, progress, cancelled);
      if (!cut.fit || out.length <= cut.fit) return out;
      const rate = (r: (typeof RUNGS)[number]) => r.fps * r.edge * r.edge;
      const room = (rate(RUNGS[rung]) * cut.fit) / out.length;
      if (rung === RUNGS.length - 1) {
        throw `couldn't get the gif under ${Math.round(cut.fit / 1e6)} MB, trim it shorter or pick a bigger size`;
      }
      const smaller = RUNGS.findIndex((r, i) => i > rung && rate(r) <= room);
      rung = smaller === -1 ? RUNGS.length - 1 : smaller;
    }
  } finally {
    video.removeAttribute('src');
    video.load();
  }
}

async function encode(video: HTMLVideoElement, cut: Cut, edge: number, fps: number, progress: (pct: number) => void, cancelled: () => boolean) {
  const narrow = cut.middle === null ? 1 : Math.min(1, (video.videoHeight * 9) / 16 / video.videoWidth);
  const sw = video.videoWidth * narrow;
  const sx = cut.middle === null ? 0 : Math.min(video.videoWidth - sw, Math.max(0, (cut.middle - narrow / 2) * video.videoWidth));
  const scale = Math.min(1, edge / Math.max(sw, video.videoHeight));
  const width = Math.max(1, Math.round(sw * scale));
  const height = Math.max(1, Math.round(video.videoHeight * scale));
  const canvas = document.createElement('canvas');
  canvas.width = width;
  canvas.height = height;
  const pen = canvas.getContext('2d', { willReadFrequently: true })!;
  pen.imageSmoothingQuality = 'high';

  const count = Math.max(1, Math.floor((cut.end - cut.start) * fps));
  const frames: Uint16Array[] = [];
  // like ffmpeg's stats_mode=diff, the palette goes to what moves
  const counts = new Uint32Array(1 << 15);
  for (let i = 0; i < count; i++) {
    if (cancelled()) throw CANCELLED;
    await seek(video, Math.min(cut.start + i / fps, cut.end - 0.001));
    pen.drawImage(video, sx, 0, sw, video.videoHeight, 0, 0, width, height);
    const rgba = pen.getImageData(0, 0, width, height).data;
    const keys = new Uint16Array(width * height);
    const before = frames[i - 1];
    for (let y = 0, p = 0; y < height; y++) {
      for (let x = 0; x < width; x++, p++) {
        const d = BAYER[((y & 3) << 2) | (x & 3)];
        const q = (v: number) => Math.min(31, Math.max(0, (v + d) >> 3));
        const key = (q(rgba[p * 4]) << 10) | (q(rgba[p * 4 + 1]) << 5) | q(rgba[p * 4 + 2]);
        keys[p] = key;
        if (!before || before[p] !== key) counts[key]++;
      }
    }
    frames.push(keys);
    progress(Math.round((80 * (i + 1)) / count));
  }

  const palette = median(counts, CLEAR);
  const nearest = new Int16Array(1 << 15).fill(-1);
  const index = (key: number) => {
    if (nearest[key] >= 0) return nearest[key];
    const r = ((key >> 10) << 3) + 4;
    const g = (((key >> 5) & 31) << 3) + 4;
    const b = ((key & 31) << 3) + 4;
    let best = 0;
    let gap = Infinity;
    for (let i = 0; i < palette.length; i += 3) {
      const d = 2 * (palette[i] - r) ** 2 + 4 * (palette[i + 1] - g) ** 2 + 3 * (palette[i + 2] - b) ** 2;
      if (d < gap) [gap, best] = [d, i / 3];
    }
    return (nearest[key] = best);
  };

  const out = new Bytes();
  out.put(...[...'GIF89a'].map((c) => c.charCodeAt(0)));
  out.word(width);
  out.word(height);
  out.put(0xf7, 0, 0);
  for (let i = 0; i < 768; i++) out.byte(palette[i] ?? 0);
  out.put(0x21, 0xff, 11, ...[...'NETSCAPE2.0'].map((c) => c.charCodeAt(0)), 3, 1, 0, 0, 0);

  const shown = new Int16Array(width * height).fill(-1);
  const pixels = new Uint8Array(width * height);
  let pending: { left: number; top: number; w: number; h: number; data: Uint8Array } | null = null;
  let since = 0;
  const flush = (upto: number) => {
    if (!pending) return;
    const delay = Math.round((100 * upto) / fps) - Math.round((100 * since) / fps);
    out.put(0x21, 0xf9, 4, 0x05);
    out.word(Math.max(2, delay));
    out.put(CLEAR, 0, 0x2c);
    out.word(pending.left);
    out.word(pending.top);
    out.word(pending.w);
    out.word(pending.h);
    out.put(0);
    lzw(pending.data, out);
    since = upto;
  };
  for (let i = 0; i < frames.length; i++) {
    if (cancelled()) throw CANCELLED;
    const keys = frames[i];
    let [left, top, right, bottom] = [width, height, -1, -1];
    for (let y = 0, p = 0; y < height; y++) {
      for (let x = 0; x < width; x++, p++) {
        const k = index(keys[p]);
        pixels[p] = k;
        if (shown[p] !== k) {
          if (x < left) left = x;
          if (x > right) right = x;
          if (y < top) top = y;
          bottom = y;
        }
      }
    }
    frames[i] = new Uint16Array(0);
    if (right < 0) continue;
    const w = right - left + 1;
    const h = bottom - top + 1;
    const data = new Uint8Array(w * h);
    for (let y = 0; y < h; y++) {
      for (let x = 0; x < w; x++) {
        const p = (top + y) * width + left + x;
        if (shown[p] === pixels[p]) data[y * w + x] = CLEAR;
        else data[y * w + x] = shown[p] = pixels[p];
      }
    }
    flush(i);
    pending = { left, top, w, h, data };
    progress(80 + Math.round((20 * (i + 1)) / frames.length));
  }
  flush(frames.length);
  out.put(0x3b);
  return out.view();
}

function seek(video: HTMLVideoElement, t: number) {
  return new Promise<void>((done, fail) => {
    video.onseeked = () => done();
    video.onerror = () => fail("couldn't read the clip");
    video.currentTime = t;
  });
}

// median cut over 5 bit a channel colors, weighted by how often each shows up
function median(counts: Uint32Array, size: number) {
  type Box = { keys: number[]; weight: number };
  const channel = (key: number, c: number) => (key >> (10 - 5 * c)) & 31;
  const keys = [...counts.keys()].filter((key) => counts[key] > 0);
  const boxes: Box[] = [{ keys, weight: keys.reduce((sum, key) => sum + counts[key], 0) }];
  const spread = (box: Box) => {
    let best = { c: 0, range: -1 };
    for (let c = 0; c < 3; c++) {
      let [lo, hi] = [31, 0];
      for (const key of box.keys) {
        const v = channel(key, c);
        if (v < lo) lo = v;
        if (v > hi) hi = v;
      }
      if (hi - lo > best.range) best = { c, range: hi - lo };
    }
    return best;
  };
  while (boxes.length < size) {
    let pick = -1;
    let score = 0;
    boxes.forEach((box, i) => {
      const s = box.keys.length > 1 ? box.weight * (spread(box).range + 1) : 0;
      if (s > score) [score, pick] = [s, i];
    });
    if (pick < 0) break;
    const box = boxes[pick];
    const { c } = spread(box);
    box.keys.sort((a, b) => channel(a, c) - channel(b, c));
    let half = 0;
    let cut = 0;
    while (cut < box.keys.length - 1 && half + counts[box.keys[cut]] <= box.weight / 2) half += counts[box.keys[cut++]];
    cut = Math.max(1, cut);
    const low = box.keys.slice(0, cut);
    const high = box.keys.slice(cut);
    const weigh = (part: number[]) => part.reduce((sum, key) => sum + counts[key], 0);
    boxes.splice(pick, 1, { keys: low, weight: weigh(low) }, { keys: high, weight: weigh(high) });
  }
  const palette: number[] = [];
  for (const box of boxes) {
    const sum = [0, 0, 0];
    for (const key of box.keys) for (let c = 0; c < 3; c++) sum[c] += ((channel(key, c) << 3) + 4) * counts[key];
    palette.push(...sum.map((v) => Math.round(v / (box.weight || 1))));
  }
  if (!palette.length) palette.push(0, 0, 0);
  return palette;
}

// gif's variable width lzw, codes grow to 12 bits and the table starts over when it fills
const codes = new Int16Array(4096 << 8);

function lzw(pixels: Uint8Array, out: Bytes) {
  const clear = 256;
  codes.fill(-1);
  const packed = new Bytes();
  let size = 9;
  let next = 258;
  let bits = 0;
  let held = 0;
  const emit = (code: number) => {
    held |= code << bits;
    bits += size;
    while (bits >= 8) {
      packed.byte(held & 255);
      held >>>= 8;
      bits -= 8;
    }
  };
  emit(clear);
  let prefix = pixels[0];
  for (let i = 1; i < pixels.length; i++) {
    const k = pixels[i];
    const key = (prefix << 8) | k;
    if (codes[key] >= 0) {
      prefix = codes[key];
      continue;
    }
    emit(prefix);
    if (next === 4096) {
      emit(clear);
      codes.fill(-1);
      size = 9;
      next = 258;
    } else {
      if (next >= 1 << size) size++;
      codes[key] = next++;
    }
    prefix = k;
  }
  emit(prefix);
  emit(clear + 1);
  if (bits > 0) packed.byte(held & 255);
  out.byte(8);
  const data = packed.view();
  for (let i = 0; i < data.length; i += 255) {
    const block = data.subarray(i, i + 255);
    out.byte(block.length);
    out.bytes(block);
  }
  out.byte(0);
}
