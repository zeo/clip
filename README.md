# memory

A game clipper for Windows 11 that keeps the last stretch of play in RAM and writes it out on demand. Nothing is injected into the game: video comes from Windows Graphics Capture and audio from per-process loopback, so anti-cheat has nothing to object to.

Right now it is a command line recorder. The tray app, hotkeys, editor and game API auto-clips come later.

```
memory capture [--monitor N | --window TEXT] [--buffer SECONDS] [--for SECONDS]
               [--fps N] [--mbps N] [--height N] [--scale FILTER] [--software] [--hdr10] [--mic] [--app EXE]... [--out FILE]
memory encoders
memory sync
memory self-test [--live]
```

`capture` records for `--for` seconds (10 by default) and saves the last `--buffer` seconds (30) to `--out` (`clip.mp4`). The default output is 1080p60 H.264 at 20 Mbps. Frames are scaled on the GPU with an area filter and encoded by the GPU's Media Foundation encoder: NVENC, AMF or QSV. Packets sit in a ring that drops whole one-second GOPs, so every clip starts on a keyframe. Saving copies packets into the MP4 without re-encoding.

`encoders` lists the H.264 and HEVC Main10 encoders, hardware and software, that Media Foundation offers on the GPU memory captures with.

## hdr

On an HDR display the capture is scRGB FP16. By default it is tonemapped to SDR with the shaders from [hdrkit](https://github.com/zeo/hdrkit): SDR content keeps its normal brightness at whatever the Windows SDR brightness slider is set to, and highlights roll off onto white at the panel's peak. `--hdr10` keeps HDR instead, as HEVC Main10 with PQ and BT.2020, and writes the panel's mastering values into the file. On an SDR display, or with no encoder that supports HEVC Main10, it records SDR.

The display is read again every second. Switching HDR on or off, or moving the slider, carries into the clip being recorded.

## audio

Each source gets its own AAC track (192 kbps) on the video's clock:

- `game`: the captured window's process and everything it launched
- `others`: every other app playing sound (`desktop` when a whole monitor is captured)
- `voice`: Discord, when it's running
- one track per `--app EXE`
- `mic`, with `--mic`

A process tree belongs to one track only, so nothing is heard twice.

## tests

`cargo test` needs no GPU. `memory self-test` runs the tonemap, HDR10 and scaling shaders against known values. It encodes two seconds of a moving pattern with every hardware and software encoder it finds, switches HDR halfway through, muxes the result with two audio tracks and reads it back. `--live` adds the sync test.

`memory sync` measures A/V sync on the real capture path. A nearly transparent window under every other window flashes six times, and a click plays at -54 dBFS with each flash. The test then finds both in the saved clip and prints how far apart they landed. It never takes focus and is safe to run mid match.

## limits

- AMD (AMF) and Intel (QSV) encoders are untested. The only GPU here is NVIDIA, so the software encoder and WARP stand in for them.
- An app muted or turned down in the Windows volume mixer is muted in its track as well, because process loopback taps the stream after per-app volume.
- Windows system sounds belong to no app process, so no track records them.
- A launcher that started the game, Steam for example, is left out of `others`, since recording its process tree would record the game a second time.
- Sync is measured to about one frame (17 ms at 60 fps).
- The -54 dBFS test click may be faintly audible on a quiet system with headphones.
- Mastering values in HDR10 files are whatever the display driver reports.
- Switching HDR mid recording is covered by the self-test, which swaps the display state on synthetic frames. It hasn't been tested by toggling a real display.

## license

MIT or Apache-2.0, at your option.
