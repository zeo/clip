# changelog

dates are release-tag dates.

## [Unreleased]

### fixes
- a game you alt-tab back to within a minute keeps its icon and art in the library; before, it could lose them if its first clip came after another game's

## [0.1.3] - 2026-11-12

### fixes
- HDR10 long recordings survive a crash too: clip writes their fragments itself, since Windows' fragmented mp4 writer refuses HEVC, and the next start turns them into a playable clip with its track names and HDR10 screen

## [0.1.2] - 2026-11-12

### fixes
- clip's unfinished writes end in `.clip-part`, and a start clears only those, so another app's `.part` download in the clips folder is left alone
- the editor's export waits for the clip's tracks to be read, so an export started too soon no longer comes out silent and then recycles the original or discards the rest of a rewind
- a clip whose tracks can't play in the editor exports with all of them instead of none
- after a replace or its undo, the editor reads the new file's tracks instead of playing the old ones
- a second replace inside the undo's 8 seconds no longer lets the first one's timer recycle the newer original
- two quick switches in the top bar's menu (recording an app, the microphone) both stick instead of the second undoing the first
- when the long buffer's drive fails, every track goes back to memory together, the top bar says why, and plays stop waiting for a buffer that's gone; before, a failing audio track kept a longer span than the video and the status kept the old length
- writing the long buffer to disk no longer holds up a save or a rewind
- a recording whose capture or audio fails partway (a driver reset, an encoder error) starts again by itself, up to 3 times a minute, and the top bar and clip.log say why; before, the top bar kept saying it was recording while the buffer stood still
- when the encoder lets more than 2 seconds pass without a keyframe, clip asks it for one, so a save never has to reach further back than that
- an encoder that reports an error mid-stream counts as failed instead of being waited on until it stalls
- each crash leaves its own `crash-<time>.dmp` (the last 3 are kept) with the memory the stacks point into, the open handles and thread details, and releases ship `clip.pdb`, so a dump can be read back to source lines
- every encoder clip opens is shut down when it's done with, so the GPU's encoder sessions and worker threads don't pile up across recordings, exports and switches between games
- clip no longer crashes a moment after it closes an NVIDIA encoder (seen after hours of recording and when testing encoders over and over): the driver could still be running one last task inside the encoder after clip freed it, so clip now lets go of it a second later
- a crash on the audio thread no longer stops clip from recording anything until it's restarted
- the editor streams a clip's tracks instead of decoding them whole, and draws their waveforms from peaks clip works out as it reads them, so opening an hour-long recording no longer takes gigabytes of memory or a long wait
- a second shift click redraws the range from where the first started, so it can shrink as well as grow, and shift with the arrows starts from the clip in focus
- a clip whose preview never loads stops holding up the previews after it
- the library can no longer fall back to an older list when two refreshes cross
- a star shows the moment it's pressed, so pressing S or Star twice in quick succession turns it back off; before, the second press could land before the library was read again and star the clip a second time
- the window never gets the SteamGridDB key or the game state token back (the key field offers replace and clear), a folder joins the library only once picked in Windows' folder dialog or found by clip, and a link in the clips folder that leads elsewhere no longer counts as inside it
- a debug build runs as the `dev` profile unless `CLIP_PROFILE` says otherwise (empty for none), and each profile's clips folder defaults to its own name, so a build from source leaves the installed clip's settings, single instance and clips alone
- the update check asks the next rot installer when one can read clip's install but can't look clip up, so a shared installer in Program Files no longer fails every check (`http: empty string` in clip.log) while the per-user one would have answered
- Settings > About has a Test encoders button that runs the same encode, mix and save checks as `clip self-test` and adds their lines to Copy system info
- the release build runs without the signing key, which only reaches a separate signing job that runs for `v*` tags and builds the installer tools from a pinned commit
- a long recording in H.264 survives a crash or power loss: it's written in fragments of about a quarter second, and the next start turns what was written whole into a playable clip with its track names (on NTFS drives, where clip keeps them beside the unfinished file); HDR10 long recordings are still lost, since Windows' fragmented mp4 writer refuses HEVC

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
