# changelog

dates are release-tag dates.

## [Unreleased]

### fixes
- clip's unfinished writes end in `.clip-part`, and a start clears only those, so another app's `.part` download in the clips folder is left alone
- the editor's export waits for the clip's tracks to be read, so an export started too soon no longer comes out silent and then recycles the original or discards the rest of a rewind
- a clip whose tracks can't play in the editor exports with all of them instead of none
- after a replace or its undo, the editor reads the new file's tracks instead of playing the old ones
- a second replace inside the undo's 8 seconds no longer lets the first one's timer recycle the newer original

## [0.1.1] - 2026-11-11

The first public release.

### recording
- the game in focus is recorded from Windows Graphics Capture, with no injection, on the GPU's NVENC, AMF or QSV encoder; with none, the CPU records at 720p, 30 fps and SDR and the top bar says so
- the buffer runs from 15 seconds to 2 hours, in RAM up to 10 minutes and in self-deleting files on disk past that
- every sound source gets its own AAC track: the game, everything else, Discord, the microphone and any app switched on
- HDR displays are tonemapped to SDR, or kept as HEVC HDR10
- Alt+F9 saves, Alt+F10 marks and saves around the mark; screenshot, mic mute, push to talk, long recording and rewind keys can be bound

### auto clips
- Counter-Strike 2 and Dota 2 clip their own big plays through game state integration, and League of Legends through Riot's live client data; plays close together make one clip named after the last

### library
- clips grouped by session, with Steam or SteamGridDB art, stars, marks, renames, a strip of each session and drag-out to Discord or a folder
- trim, split, join, 9:16 crops and exports sized for Discord, as MP4 or GIF
- cleanup by size or age deletes only files clip wrote

### app
- a three-step tour on the first start
- Settings > About checks for updates every 6 hours through the rot installer, opens the logs folder and copies system info for a bug report
- settings and the library are written whole or not at all, and a file that doesn't read is kept as `.bad`
- uninstalling takes away the start with Windows value, the notification id and the auto clips files in game folders (`clip uninstall`)
- a failed update check is written to `clip.log`

## [0.1.0] - 2026-11-11

Tagged, never published.
