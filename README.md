# clip

A game clipper for Windows 11 that keeps the last stretch of play in RAM and writes it out on demand. Nothing is injected into the game: video comes from Windows Graphics Capture and audio from per-process loopback, so anti-cheat has nothing to object to.

Started with no arguments, it sits in the tray and records the game in focus: its window, with its sound on a track of its own. Alt+F9, or save clip in the tray menu, writes the last 30 seconds to `Videos\clip-recorder\<app>`. Once the file is written clip plays capscr's capture sound, and a small notice in a corner of the game's monitor says what was saved for 2.5 seconds, without taking focus or showing up in clips. A failed save plays the Windows error sound and the notice says why. Alt+F10 marks the moment and saves a clip by itself 15 seconds later, so the moment sits in the middle. Both are defaults. Settings rebinds them, adds more, and gives each save key a length of its own, so one key can keep the whole buffer and another only the last 15 seconds. Three more actions start unbound, since a key clip registers is taken from every game: a screenshot of the game, saved as a PNG beside its clips; mic mute; and a long recording, which runs from the press until the key comes again and is saved as `<time> recording.mp4`. It also stops when the game's recording ends or the drive gets under 2 GB free. Coming from, on the hotkeys page, copies the keys Medal, the NVIDIA App, SteelSeries Moments or AMD Software use, as unsaved changes that revert puts back. Pause recording, in the tray menu or the top bar, stops the buffer until it is switched back on.

The window opens from the tray or by starting clip again, or at every start once Settings says so. It fills the screen the first time and comes back where it was closed after that. Its close button hides it to the tray, or quits clip if Settings says that instead. A rail on the left picks Home, Library or Settings, also on keys 1 to 3, and a sidebar next to it changes with the area. Ctrl+B or the button at its top hides the sidebar, and Home starts with it hidden. In the library it filters to games, all clips, starred, today, this week, or screenshots and GIFs, lists every game with its clip count, and shows the space clips take on the drive. The top bar shows what is recording, faded over the game's banner. Clicking it opens quick controls: pause, stop recording this game, a long recording, a screenshot, the quality, and the microphone with its mute. Next to it each hotkey is shown with what it does, in red when another app already holds the key. Settings has pages for hotkeys, the recorder, quality, audio, clips, appearance, alerts, notifications, startup and game art, and its search finds a setting on any page. Nothing changes until apply, and the bar that says so also says whether applying drops what the buffer holds. Game API auto-clips come later. The same exe is also a command line recorder:

```
clip capture [--monitor N | --window TEXT] [--buffer SECONDS] [--for SECONDS]
               [--fps N] [--mbps N] [--height N] [--scale FILTER] [--software] [--hdr10] [--mic] [--app EXE]... [--out FILE]
clip encoders
clip sync
clip self-test [--live]
```

`capture` records for `--for` seconds (10 by default) and saves the last `--buffer` seconds (30) to `--out` (`clip.mp4`). The default output is 1080p60 H.264 at 20 Mbps. Frames are scaled on the GPU with an area filter and encoded by the GPU's Media Foundation encoder: NVENC, AMF or QSV. Packets sit in a ring that drops whole one-second GOPs, so every clip starts on a keyframe. Saving copies packets into the MP4 without re-encoding.

`encoders` lists the H.264 and HEVC Main10 encoders, hardware and software, that Media Foundation offers on the GPU clip captures with.

## following the app

The tray app looks at the focused window four times a second. Each app is named by its exe's file description, so `cs2.exe` lands in `Counter-Strike 2`, or by the exe name when it has none. Recording moves to a new app once it has kept focus for a second, so alt-tabbing past a window doesn't restart anything. The app just left keeps recording in the background for a minute. Alt-tab back within that minute and its buffer is whole, even with Alt+F9 pressed the moment it has focus again. While both run, the buffer takes twice the RAM settings shows.

Only games are followed: a window counts once it covers its whole monitor, the way fullscreen and borderless games do, and maximized windows don't. Settings lists the apps with a window open, and any switched on there are recorded in a window too. Games switched off on the recorder page, or from the top bar while they record, are never recorded. The desktop, taskbar, start menu, minimized and cloaked windows, tool windows and clip's own window never are. Focus moving to something else leaves the game recording. Once the recorded window closes, clip records nothing and the tray says it is waiting for a game, and Alt+F9 or Alt+F10 plays the Windows error sound.

Settings lists the apps playing sound right now. Each one switched on gets its own track in every clip, the way OBS adds an application audio capture source, and is left out of `others`. While that app is the one being recorded, its sound stays on `game` and it gets no second track.

## marks

Alt+F10 marks the moment, and the clip saves itself 15 seconds after the newest mark, or half the buffer when that is shorter. Another Alt+F10 in that wait moves the save on, but never so far that the first mark would fall out of the buffer. Alt+F9 during the wait saves at once, and the marked clip still comes after. Each clip keeps its marks in `library.json` next to the settings, as seconds into the clip, and the clip page draws them on the timeline and lists them beside it. M adds one where the video is, and Delete removes the one in focus. A trimmed copy keeps the marks inside the cut. Marks belong to the recording they were made in. Alt-tabbing or closing the game before the save leaves that recording running until its clip is written.

## library

Home lists the last 7 days of clips across every game. Games is a shelf of covers, one per game, and each cover opens the game's page under a banner of its art. The icon is read from the exe when recording starts and kept in clip's cache folder. Clips are grouped by session: clips from one app saved less than 30 minutes apart sit under one header, such as `Fortnite, today 14:02 to 14:47, 6 clips`. Each card shows a still, made once from the first second and kept in clip's cache folder, and plays the clip muted while the pointer is over it. Only one card plays at a time. Clips can be starred, renamed, and deleted to the Recycle Bin. Hovering a card also shows a check in its top left corner. Once one clip is checked, a click on any card checks it as well, Shift-click checks every clip in between, and a bar along the bottom stars, copies or deletes them together, as one Recycle Bin operation. Esc clears the selection. A slider in the toolbar sizes the cards from 170 to 340 px, and Ctrl+wheel steps it. The button beside it turns the grid into a list with a column each for name, game, date, length, size and marks, sorted by a click on the header. The grid is one Tab stop: the arrow keys, Home, End, Page Up and Page Down move through it, Enter opens a clip, Space checks it, S stars it, Delete deletes, Ctrl+A checks every clip and Ctrl+C copies the files, ready to paste into Discord or a folder. A delete waits 8 seconds behind an undo before the files go to the Recycle Bin, so quitting clip first keeps them. Screenshots, frames saved from clips and GIFs sit on their own shelf, and open in a viewer with star, delete, open, show in folder and copy. Settings can keep the folder under a number of GB, and clips for a number of days. After each save, unstarred clips past either limit are deleted for good, oldest first, since the Recycle Bin would not free the space. The day limit is also checked at start. Settings can also show a Windows notification for each clip, with the game, its length and size, and a button that opens it.

Opening a clip gives it the window: the video on top, a timeline with two trim handles and one lane per audio track below it, each lane with a waveform, mute and solo, the toolbar above with back, the previous and next clip, star, rename and delete, and a panel on the right with the clip's details, its marks, the cut and the export. The webview only plays a file's first audio track, so the video plays muted and every track is decoded to its own WAV and played through Web Audio, restarted where the video is if they drift more than 40 ms apart. Space or K plays, J and L skip 5 seconds, the arrow keys or comma and period step one frame, and `<` and `>` change the speed from a quarter to double. Up and Down change the volume, F fills the screen, I and O set the cut, Shift+N and Shift+P open the next and previous clip, and Esc goes back. A loop button repeats the cut. Save frame writes the paused frame as a full size PNG, and Copy file puts the clip on the clipboard to paste into a chat or a folder. `?` lists every key, anywhere in clip.

The export panel writes an MP4 or a GIF, of the whole picture or cropped to 9:16 for phones, with the crop dragged across the player. It offers original quality, 20 MB for Discord without Nitro, 50 MB for Nitro Basic or a server at boost level 2, 100 MB for boost level 3, 1 GB for Nitro, or a size in MB. Only the tracks left audible go in, mixed into one, which Discord and most players expect, or kept apart. A size always mixes, since Discord plays only the first track. The export is saved beside the clip, or replaces it: the original then waits 8 seconds behind an undo before it goes to the Recycle Bin, and the star, the name and the marks inside the cut stay. A bar shows how far it has got, and Cancel stops it. With a size picked, the finished file is also put on the clipboard.

An MP4 export is decoded and encoded again on the GPU, so the cut lands on the exact frame. Unlike capture it encodes for quality, not latency: peak constrained VBR up to 1.5 times the mean rate, at the encoder's slowest quality setting, with a keyframe every 4 seconds. For a size, the video gets what the audio leaves of 97% of it, which covers encoder overshoot and sites that count a megabyte as 2^20 bytes. Audio drops from 192 to 96 kbps at 50 MB or less. Below 0.06 bits for each pixel of each frame a 60 fps clip goes to 30 fps, and below 0.04 the picture steps down through 1080p, 720p, 540p and 360p. A copy that still comes out over is encoded again lower, and a whole clip with one track that already fits is copied as it is. 9:16 from 1080p comes out at 608x1080. HDR10 clips export as HEVC, which Discord may not play.

A GIF is drawn in the webview from the clip, up to 15 seconds long, 480 px on its long edge at 15 frames a second, or 10 with a size picked. Its palette is a median cut of up to 255 colours over the pixels that changed. Each frame gets a 4x4 ordered dither, which keeps still areas identical from frame to frame, and after the first only the rectangle that changed is stored, with unchanged pixels inside it left transparent. A GIF over its size is made again at 360 and then 320 px.

## other apps' folders

The clips page in Settings offers the folders other clipping apps save to, when this pc has them: Medal's, Outplayed's, Xbox Game Bar's `Videos\Captures` and the NVIDIA App's `Videos`. Any other folder can be added too. Their clips and images show in the library with the folder's name on the card, each folder inside one becoming a game. clip only reads them: delete, rename, replace and cleanup leave them alone, and exports and frames made from them go to clip's own folder, under the game's name. A file the clips folder has a copy of, by name and size, shows once. Copy into clip copies every clip and image across, skipping names already there, and leaves the originals where they are.

## seeing and reaching everything

Everything works from the keyboard, and `?` lists the keys, the user's own hotkeys included. Ctrl+= and Ctrl+- scale the whole window from 90 to 150%, Ctrl+0 goes back to 100%, and the scale is remembered. Text meets WCAG AA contrast. Saves, exports, applies and errors go through one status region that screen readers announce, shown as toasts along the bottom. Windows contrast themes get real borders, and focus and the current item in the theme's own colours. With reduced motion on in Windows, cards stop playing on hover and nothing slides or fades.

## game art

Steam games get their cover, banner and logo from Steam's store CDN, with no key. When recording starts, clip reads Steam's library folders from `libraryfolders.vdf` and takes the app id of the `appmanifest_*.acf` whose install folder holds the exe. Other games get art from SteamGridDB once an API key from the preferences of a SteamGridDB profile is pasted into Settings, on the game art page. Art is fetched once per game, only while the window is open, and kept in the cache folder. Missing art is asked for again after 7 days, or as soon as a key is added. A game without art shows its newest still and its exe icon.

## staying out of the game's way

The tray app runs at below normal CPU priority and sets its GPU thread priority to -3 on the -7 to 7 scale, so a saturated GPU schedules the game's work ahead of clip's. It stays out of background mode, which would also let Windows page out the buffer. Closing the window destroys its WebView2 instead of hiding it.

## hdr

On an HDR display the capture is scRGB FP16. By default it is tonemapped to SDR with the shaders from [hdrkit](https://github.com/zeo/hdrkit): SDR content keeps its normal brightness at whatever the Windows SDR brightness slider is set to, and highlights roll off onto white at the panel's peak. `--hdr10` keeps HDR instead, as HEVC Main10 with PQ and BT.2020, and writes the panel's mastering values into the file. On an SDR display, or with no encoder that supports HEVC Main10, it records SDR.

The display is read again every second, and on every frame for a second after capture stalls, which switching HDR does. Switching HDR on or off, or moving the slider, carries into the clip being recorded. Frames composed while the switch happens are dropped and the last good frame repeats in their place, so the clip keeps its frame rate.

## audio

Each source gets its own AAC track (192 kbps) on the video's clock:

- `game`: the captured window's process and everything it launched
- `others`: every other app playing sound (`desktop` when a whole monitor is captured)
- `voice`: Discord, when it's running
- one track per `--app EXE`, or per app switched on in settings, named after the exe
- `mic`, with `--mic`, or switched on in settings

Settings gives each track a volume from 0 to 200%, applied before encoding, and picks the microphone. A microphone that is gone falls back to the default device. Push to talk keeps the `mic` track silent until its key is held. The save sound has its own volume, up to 150%, and can be switched off.

A process belongs to one track only, so nothing is heard twice. With no `voice` or `--app` track, `others` is a single capture of everything outside the game, so it also records Windows system sounds and the launcher that started the game.

Windows only gives a per-app capture the app's own sound and that of its direct children. clip opens a capture on every process that holds an audio session and isn't already a direct child of one it captures. That is how a game four processes below its launcher, like Fortnite under the Epic Games Launcher, still reaches a track.

## tests

`cargo test` needs no GPU. `clip self-test` runs the tonemap, HDR10 and scaling shaders against known values. It encodes two seconds of a moving pattern with every hardware and software encoder it finds, switches HDR halfway through, muxes the result with two audio tracks and reads it back. `--live` adds the sync test.

`clip sync` measures A/V sync on the real capture path. A nearly transparent window under every other window flashes six times, and a click plays at -72 dBFS with each flash. It records at the display's refresh rate unless `--fps` says otherwise. The test then finds both in the saved clip and prints how far apart they landed. It never takes focus and is safe to run mid match.

## limits

- Store (UWP) apps sit inside an Application Frame Host window. clip names the clip folder after that host and puts the app's sound in `others`, not `game`. Untested.
- Apps running as administrator are untested: Windows may refuse capture and per-app audio to a non-elevated clip.
- AMD (AMF) and Intel (QSV) encoders are untested. The only GPU here is NVIDIA, so the software encoder and WARP stand in for them.
- An app muted or turned down in the Windows volume mixer is muted in its track as well, because process loopback taps the stream after per-app volume.
- With a `voice` or `--app` track running, Windows system sounds are left out: their audio session reports no process, so there is nothing to open a per-app capture on.
- In that case a launcher that is the game's direct parent, Steam for example, is also left out of `others`, since capturing it would record the game a second time. So is the process that started clip.
- Per-app capture reaching only direct children was measured on Windows build 26340. Microsoft's documentation says "child processes" without saying how deep. On a build that captured whole trees, a process with sound two levels below another one would be heard twice.
- Sync is measured to one display refresh, 8 ms at 120 Hz, since a flash only reaches the clip on the frame it was shown.
- The test click peaks at -72 dBFS. With headphones and the volume near maximum it may still be faintly audible.
- The notice, screenshot key, mic mute and long recordings are untested with a real game, since that needs live capture. The notice is kept out of captures with `WDA_EXCLUDEFROMCAPTURE`, which needs Windows 10 2004 or later.
- A screenshot waits up to 1.5 seconds for the game to draw a new frame. A game showing a still screen may draw none, and the notice says so.
- A long recording's MP4 index is written when it stops, so a crash loses it.
- Other apps' folders are read one level deep: a folder inside one is a game, and folders below that are skipped. The Medal and NVIDIA App folders offered are their usual defaults, found by name only.
- The NVIDIA App preset uses the keys guides most often give, and guides disagree.
- Mastering values in HDR10 files are whatever the display driver reports.
- Switching HDR was tested on a real display on build 26340 with an NVIDIA GPU. Each switch freezes the clip for about 0.4 s. After switching HDR on, Windows composes the first frames with SDR white at 80 nits, so they come out at a third of their linear brightness. That lasted from 0.13 s to 0.8 s across six switches at 60 and 120 fps. For about 1.5 s the driver reports a lower peak, which makes highlights roll off a little early.

## credits

`assets/saved.wav` is `screenshot.wav` from [capscr](https://github.com/zeo/capscr), unchanged. capscr is MIT licensed, copyright (c) 2024-2025 capscr contributors.

## license

MIT or Apache-2.0, at your option.
