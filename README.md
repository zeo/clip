# memory

A game clipper for Windows 11 that keeps the last stretch of play in RAM and writes it out on demand. Nothing is injected into the game: video comes from Windows Graphics Capture and audio from per-process loopback, so anti-cheat has nothing to object to.

Started with no arguments, it sits in the tray and keeps the last 30 seconds of the primary monitor, with desktop and Discord audio. Alt+F9, or save clip in the tray menu, writes them to `Videos\memory`, with a beep once the file is written. The window, opened from the tray, sets the buffer length, quality, HDR, mic, folder and start with Windows, and lists saved clips with a player. The editor and game API auto-clips come later. The same exe is also a command line recorder:

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

The display is read again every second, and on every frame for a second after capture stalls, which switching HDR does. Switching HDR on or off, or moving the slider, carries into the clip being recorded. Frames composed while the switch happens are dropped and the last good frame repeats in their place, so the clip keeps its frame rate.

## audio

Each source gets its own AAC track (192 kbps) on the video's clock:

- `game`: the captured window's process and everything it launched
- `others`: every other app playing sound (`desktop` when a whole monitor is captured)
- `voice`: Discord, when it's running
- one track per `--app EXE`
- `mic`, with `--mic`

A process belongs to one track only, so nothing is heard twice. With no `voice` or `--app` track, `others` is a single capture of everything outside the game, so it also records Windows system sounds and the launcher that started the game.

Windows only gives a per-app capture the app's own sound and that of its direct children. memory opens a capture on every process that holds an audio session and isn't already a direct child of one it captures. That is how a game four processes below its launcher, like Fortnite under the Epic Games Launcher, still reaches a track.

## tests

`cargo test` needs no GPU. `memory self-test` runs the tonemap, HDR10 and scaling shaders against known values. It encodes two seconds of a moving pattern with every hardware and software encoder it finds, switches HDR halfway through, muxes the result with two audio tracks and reads it back. `--live` adds the sync test.

`memory sync` measures A/V sync on the real capture path. A nearly transparent window under every other window flashes six times, and a click plays at -72 dBFS with each flash. It records at the display's refresh rate unless `--fps` says otherwise. The test then finds both in the saved clip and prints how far apart they landed. It never takes focus and is safe to run mid match.

## limits

- AMD (AMF) and Intel (QSV) encoders are untested. The only GPU here is NVIDIA, so the software encoder and WARP stand in for them.
- An app muted or turned down in the Windows volume mixer is muted in its track as well, because process loopback taps the stream after per-app volume.
- With a `voice` or `--app` track running, Windows system sounds are left out: their audio session reports no process, so there is nothing to open a per-app capture on.
- In that case a launcher that is the game's direct parent, Steam for example, is also left out of `others`, since capturing it would record the game a second time. So is the process that started memory.
- Per-app capture reaching only direct children was measured on Windows build 26340. Microsoft's documentation says "child processes" without saying how deep. On a build that captured whole trees, a process with sound two levels below another one would be heard twice.
- Sync is measured to one display refresh, 8 ms at 120 Hz, since a flash only reaches the clip on the frame it was shown.
- The test click peaks at -72 dBFS. With headphones and the volume near maximum it may still be faintly audible.
- Mastering values in HDR10 files are whatever the display driver reports.
- Switching HDR was tested on a real display on build 26340 with an NVIDIA GPU. Each switch freezes the clip for about 0.4 s. After switching HDR on, Windows composes the first frames with SDR white at 80 nits, so they come out at a third of their linear brightness. That lasted from 0.13 s to 0.8 s across six switches at 60 and 120 fps. For about 1.5 s the driver reports a lower peak, which makes highlights roll off a little early.

## license

MIT or Apache-2.0, at your option.
