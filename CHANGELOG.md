# changelog

dates are release-tag dates.

## [0.1.0] - 2026-11-11

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
