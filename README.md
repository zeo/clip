# memory

A game clipper for Windows 11 that keeps the last stretch of play in RAM and writes it out on demand. Nothing is injected into the game: video comes from Windows Graphics Capture and audio from per-process loopback, so anti-cheat has nothing to object to.

Started with no arguments, it sits in the tray and records the game in focus: its window, with its sound on a track of its own. Alt+F9, or save clip in the tray menu, writes the last 30 seconds to `Videos\memory\<app>`, with a short sound once the file is written: capscr's shutter played backwards, so the two are told apart by ear. A failed save plays the Windows error sound instead. Alt+F10 marks the moment and saves a clip by itself 15 seconds later, so the moment sits in the middle. The window, opened from the tray or by starting memory again, sets the buffer length, quality, HDR, mic, the apps that get their own audio track, the folder and start with Windows. It holds the clip library and a page per clip for trimming. Game API auto-clips come later. The same exe is also a command line recorder:

```
memory capture [--monitor N | --window TEXT] [--buffer SECONDS] [--for SECONDS]
               [--fps N] [--mbps N] [--height N] [--scale FILTER] [--software] [--hdr10] [--mic] [--app EXE]... [--out FILE]
memory encoders
memory sync
memory self-test [--live]
```

`capture` records for `--for` seconds (10 by default) and saves the last `--buffer` seconds (30) to `--out` (`clip.mp4`). The default output is 1080p60 H.264 at 20 Mbps. Frames are scaled on the GPU with an area filter and encoded by the GPU's Media Foundation encoder: NVENC, AMF or QSV. Packets sit in a ring that drops whole one-second GOPs, so every clip starts on a keyframe. Saving copies packets into the MP4 without re-encoding.

`encoders` lists the H.264 and HEVC Main10 encoders, hardware and software, that Media Foundation offers on the GPU memory captures with.

## following the app

The tray app looks at the focused window four times a second. Each app is named by its exe's file description, so `cs2.exe` lands in `Counter-Strike 2`, or by the exe name when it has none. Recording moves to a new app once it has kept focus for a second, so alt-tabbing past a window doesn't restart anything. The app just left keeps recording in the background for a minute. Alt-tab back within that minute and its buffer is whole, even with Alt+F9 pressed the moment it has focus again. While both run, the buffer takes twice the RAM settings shows.

Only games are followed: a window counts once it covers its whole monitor, the way fullscreen and borderless games do, and maximized windows don't. Settings lists the apps with a window open, and any switched on there are recorded in a window too. The desktop, taskbar, start menu, minimized and cloaked windows, tool windows and memory's own window never are. Focus moving to something else leaves the game recording. Once the recorded window closes, memory records nothing and the tray says it is waiting for a game, and Alt+F9 or Alt+F10 plays the Windows error sound.

Settings lists the apps playing sound right now. Each one switched on gets its own track in every clip, the way OBS adds an application audio capture source, and is left out of `others`. While that app is the one being recorded, its sound stays on `game` and it gets no second track.

## marks

Alt+F10 marks the moment, and the clip saves itself 15 seconds after the newest mark, or half the buffer when that is shorter. Another Alt+F10 in that wait moves the save on, but never so far that the first mark would fall out of the buffer. Alt+F9 during the wait saves at once, and the marked clip still comes after. Each clip keeps its marks in `library.json` next to the settings, as seconds into the clip, and the clip page draws them on the timeline. A trimmed copy keeps the marks inside the cut. Marks belong to the recording they were made in. Alt-tabbing or closing the game before the save leaves that recording running until its clip is written.

## library

Clips are grouped by app, then by session: clips from one app saved less than 30 minutes apart sit under one header, such as `Fortnite, today 14:02 to 14:47, 6 clips`. Each card shows a still, made once from the first second and kept in memory's cache folder, and plays the clip muted while the pointer is over it. Only one card plays at a time. Clips can be starred, renamed, and deleted to the Recycle Bin. Hovering a card also shows a check in its top left corner. Once one clip is checked, a click on any card checks it as well, Shift-click checks every clip in between, and a bar along the bottom stars or deletes them together, as one Recycle Bin operation. Esc clears the selection. Settings can keep the folder under a number of GB. After each save, the oldest unstarred clips past that limit are deleted for good, since the Recycle Bin would not free the space.

Opening a clip shows it large over a timeline with two trim handles and one lane per audio track, each with a waveform, mute and solo. The webview only plays a file's first audio track, so the video plays muted and every track is decoded to its own WAV and played through Web Audio, restarted where the video is if they drift more than 40 ms apart. Space plays, the arrow keys step one frame, I and O set the cut, Esc goes back. Save trimmed copy writes `<name> trimmed.mp4` next to the clip and leaves the original alone. The video is decoded and encoded again on the GPU, so the cut lands on the exact frame, at the clip's size, frame rate and bitrate. Only the tracks left audible go in, either mixed into one track, which Discord and most players expect, or kept apart. Copy for Discord writes `<name> discord.mp4` under 19 MB, since Discord's free upload limit is 20 MB, with the audible tracks mixed into one. The video gets whatever bitrate the audio leaves. Below 3.5 Mbps the picture drops to 720p, below 1.5 Mbps to 540p, and a copy that still comes out over is encoded again lower. A cut longer than about 4 minutes 50 seconds can't fit and is refused.

## staying out of the game's way

The tray app runs at below normal CPU priority and sets its GPU thread priority to -3 on the -7 to 7 scale, so a saturated GPU schedules the game's work ahead of memory's. It stays out of background mode, which would also let Windows page out the buffer. Closing the window destroys its WebView2 instead of hiding it.

## hdr

On an HDR display the capture is scRGB FP16. By default it is tonemapped to SDR with the shaders from [hdrkit](https://github.com/zeo/hdrkit): SDR content keeps its normal brightness at whatever the Windows SDR brightness slider is set to, and highlights roll off onto white at the panel's peak. `--hdr10` keeps HDR instead, as HEVC Main10 with PQ and BT.2020, and writes the panel's mastering values into the file. On an SDR display, or with no encoder that supports HEVC Main10, it records SDR.

The display is read again every second, and on every frame for a second after capture stalls, which switching HDR does. Switching HDR on or off, or moving the slider, carries into the clip being recorded. Frames composed while the switch happens are dropped and the last good frame repeats in their place, so the clip keeps its frame rate.

## audio

Each source gets its own AAC track (192 kbps) on the video's clock:

- `game`: the captured window's process and everything it launched
- `others`: every other app playing sound (`desktop` when a whole monitor is captured)
- `voice`: Discord, when it's running
- one track per `--app EXE`, or per app switched on in settings, named after the exe
- `mic`, with `--mic`

A process belongs to one track only, so nothing is heard twice. With no `voice` or `--app` track, `others` is a single capture of everything outside the game, so it also records Windows system sounds and the launcher that started the game.

Windows only gives a per-app capture the app's own sound and that of its direct children. memory opens a capture on every process that holds an audio session and isn't already a direct child of one it captures. That is how a game four processes below its launcher, like Fortnite under the Epic Games Launcher, still reaches a track.

## tests

`cargo test` needs no GPU. `memory self-test` runs the tonemap, HDR10 and scaling shaders against known values. It encodes two seconds of a moving pattern with every hardware and software encoder it finds, switches HDR halfway through, muxes the result with two audio tracks and reads it back. `--live` adds the sync test.

`memory sync` measures A/V sync on the real capture path. A nearly transparent window under every other window flashes six times, and a click plays at -72 dBFS with each flash. It records at the display's refresh rate unless `--fps` says otherwise. The test then finds both in the saved clip and prints how far apart they landed. It never takes focus and is safe to run mid match.

## limits

- Store (UWP) apps sit inside an Application Frame Host window. memory names the clip folder after that host and puts the app's sound in `others`, not `game`. Untested.
- Apps running as administrator are untested: Windows may refuse capture and per-app audio to a non-elevated memory.
- AMD (AMF) and Intel (QSV) encoders are untested. The only GPU here is NVIDIA, so the software encoder and WARP stand in for them.
- An app muted or turned down in the Windows volume mixer is muted in its track as well, because process loopback taps the stream after per-app volume.
- With a `voice` or `--app` track running, Windows system sounds are left out: their audio session reports no process, so there is nothing to open a per-app capture on.
- In that case a launcher that is the game's direct parent, Steam for example, is also left out of `others`, since capturing it would record the game a second time. So is the process that started memory.
- Per-app capture reaching only direct children was measured on Windows build 26340. Microsoft's documentation says "child processes" without saying how deep. On a build that captured whole trees, a process with sound two levels below another one would be heard twice.
- Sync is measured to one display refresh, 8 ms at 120 Hz, since a flash only reaches the clip on the frame it was shown.
- The test click peaks at -72 dBFS. With headphones and the volume near maximum it may still be faintly audible.
- Mastering values in HDR10 files are whatever the display driver reports.
- NVIDIA's HEVC encoder writes a wrong crop for widths that aren't a multiple of 64: 852 wide comes out showing 834. An HDR10 clip's Discord copy therefore only shrinks when the smaller width is such a multiple, as 1280x720 and 960x540 are for 16:9, and keeps its size otherwise.
- Switching HDR was tested on a real display on build 26340 with an NVIDIA GPU. Each switch freezes the clip for about 0.4 s. After switching HDR on, Windows composes the first frames with SDR white at 80 nits, so they come out at a third of their linear brightness. That lasted from 0.13 s to 0.8 s across six switches at 60 and 120 fps. For about 1.5 s the driver reports a lower peak, which makes highlights roll off a little early.

## credits

`assets/saved.wav` is `screenshot.wav` from [capscr](https://github.com/zeo/capscr), reversed with the leading silence trimmed. capscr is MIT licensed, copyright (c) 2024-2025 capscr contributors.

## license

MIT or Apache-2.0, at your option.
