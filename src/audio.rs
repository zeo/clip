use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use windows::core::{implement, Interface, Ref, Result, HRESULT};
use windows::Win32::Foundation::{CloseHandle, E_FAIL, FILETIME, S_OK};
use windows::Win32::Media::Audio::*;
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, IAgileObject, IAgileObject_Impl, BLOB, CLSCTX_ALL,
    COINIT_MULTITHREADED,
};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::Threading::{GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};
use windows::Win32::System::Variant::VT_BLOB;

use crate::encode::packet;
use crate::ring::{Clock, Packet, Ring, SECOND};

pub const RATE: u32 = 48_000;
pub const CHANNELS: u16 = 2;
pub const BLOCK: usize = 2 * CHANNELS as usize;
// 192 kbps, the highest rate the in-box encoder offers
pub const AAC_BYTES_PER_SECOND: u32 = 24_000;
const AAC_FRAME: i64 = 1024 * SECOND / RATE as i64;
const POLL: Duration = Duration::from_millis(10);
// room for the poll loop to stall while a new app's capture activates
const DEVICE_BUFFER: i64 = SECOND / 2;
const ACTIVATION_TIMEOUT: Duration = Duration::from_secs(5);
const RESCAN: Duration = Duration::from_secs(2);
const RETRY: Duration = Duration::from_secs(5);
const DISCORD: [&str; 3] = ["discord.exe", "discordptb.exe", "discordcanary.exe"];
// the microphone feed's key, a pid no process can have
const MIC: u32 = 0;

#[derive(Clone, Debug)]
pub enum Source {
    /// one process tree, for as long as it runs
    Process(u32),
    /// every process tree rooted at an exe with one of these lowercase names, followed across restarts
    Named(Vec<String>),
    /// every app playing sound that no other lane claims, mixed into one track
    Rest,
    /// everything outside one process tree as a single feed, system sounds and the game's launcher included
    Except(u32),
    Microphone,
}

pub struct Lane {
    pub name: String,
    pub source: Source,
}

/// The tracks a clip gets: the game, everything else, voice chat if it is running, any app asked
/// for by exe name, and the microphone.
pub fn lanes(game: Option<u32>, apps: &[String], mic: bool) -> Vec<Lane> {
    let mut lanes = Vec::new();
    if let Some(pid) = game {
        lanes.push(Lane { name: "game".into(), source: Source::Process(pid) });
    }
    let processes = Processes::snapshot();
    // the game's own sound is already on its track, a second lane for the same exe would stay silent
    let recorded = game.and_then(|pid| processes.0.get(&pid)).map(|(_, name)| name.as_str());
    let discord: Vec<String> = DISCORD.map(String::from).into();
    let voice = !recorded.is_some_and(|name| DISCORD.contains(&name)) && !processes.roots(&discord).is_empty();
    let apps: Vec<String> = apps
        .iter()
        .map(|app| app.to_ascii_lowercase().trim_end_matches(".exe").to_string())
        .filter(|name| recorded != Some(format!("{name}.exe").as_str()))
        .collect();
    // exclude mode carves out only one tree, so it can stand in for the rest only when no other lane claims one
    let rest =
        if voice || !apps.is_empty() { Source::Rest } else { Source::Except(game.unwrap_or_else(std::process::id)) };
    lanes.push(Lane { name: if game.is_some() { "others" } else { "desktop" }.into(), source: rest });
    if voice {
        lanes.push(Lane { name: "voice".into(), source: Source::Named(discord) });
    }
    for name in apps {
        lanes.push(Lane { source: Source::Named(vec![format!("{name}.exe")]), name });
    }
    if mic {
        lanes.push(Lane { name: "mic".into(), source: Source::Microphone });
    }
    lanes
}

/// The lowercase exe names of apps holding an audio session, Discord aside since it always gets
/// the voice track.
pub fn playing() -> Result<Vec<String>> {
    // whatever thread asks, the session api wants com, and an apartment already set up is fine too
    let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
    let pids = Sessions::new(Arc::new(AtomicBool::new(false)))?.pids()?;
    let processes = Processes::snapshot();
    let own = std::process::id();
    let mut names: Vec<String> = pids
        .into_iter()
        .filter(|&pid| pid != own)
        .filter_map(|pid| processes.0.get(&pid).map(|(_, name)| name.clone()))
        .filter(|name| !DISCORD.contains(&name.as_str()))
        .collect();
    names.sort_unstable();
    names.dedup();
    Ok(names)
}

/// Every running process's parent and lowercase exe name.
struct Processes(HashMap<u32, (u32, String)>);

impl Processes {
    fn snapshot() -> Self {
        let mut processes = HashMap::new();
        unsafe {
            let Ok(snapshot) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else { return Self(processes) };
            let mut entry = PROCESSENTRY32W { dwSize: size_of::<PROCESSENTRY32W>() as u32, ..Default::default() };
            let mut more = Process32FirstW(snapshot, &mut entry).is_ok();
            while more {
                let len = entry.szExeFile.iter().position(|&c| c == 0).unwrap_or(entry.szExeFile.len());
                let name = String::from_utf16_lossy(&entry.szExeFile[..len]).to_ascii_lowercase();
                processes.insert(entry.th32ProcessID, (entry.th32ParentProcessID, name));
                more = Process32NextW(snapshot, &mut entry).is_ok();
            }
            let _ = CloseHandle(snapshot);
        }
        let started = processes.keys().filter_map(|&pid| Some((pid, started(pid)?))).collect();
        unlink_reused(&mut processes, &started);
        Self(processes)
    }

    // parent ids can loop once a parent exits and its id is reused, so the walk is bounded
    fn ancestors(&self, pid: u32) -> impl Iterator<Item = u32> + '_ {
        std::iter::successors(self.0.get(&pid).map(|p| p.0), |p| self.0.get(p).map(|p| p.0)).take(64)
    }

    /// The topmost process of every tree of `names`, so a multi-process app is recorded once.
    fn roots(&self, names: &[String]) -> Vec<u32> {
        let named = |pid: &u32| self.0.get(pid).is_some_and(|(_, name)| names.contains(name));
        let mut roots: Vec<u32> =
            self.0.iter().filter(|(pid, (parent, _))| named(pid) && !named(parent)).map(|(&pid, _)| pid).collect();
        roots.sort_unstable();
        roots
    }
}

/// Cuts every link to a parent that started after its child: the real parent exited and windows
/// gave its id to a later, unrelated process. Processes whose start time can't be read keep their link.
fn unlink_reused(processes: &mut HashMap<u32, (u32, String)>, started: &HashMap<u32, u64>) {
    for (pid, (parent, _)) in processes.iter_mut() {
        if let (Some(child), Some(adult)) = (started.get(pid), started.get(parent)) {
            if adult > child {
                *parent = 0;
            }
        }
    }
}

/// When `pid` started, in 100 ns ticks. Limited query access is all this asks for, the same
/// a task manager uses, so it never reads another process's memory.
fn started(pid: u32) -> Option<u64> {
    unsafe {
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let [mut created, mut exited, mut kernel, mut user] = [FILETIME::default(); 4];
        let read = GetProcessTimes(process, &mut created, &mut exited, &mut kernel, &mut user);
        let _ = CloseHandle(process);
        read.ok()?;
        Some((created.dwHighDateTime as u64) << 32 | created.dwLowDateTime as u64)
    }
}

/// The processes each lane opens a feed on. Include capture takes a process and its direct children
/// only (on build 26340 a grandchild's sound never reaches it), so a lane gets a feed on its roots and
/// on every process of its own holding an audio session, unless the parent is already a feed of that
/// lane. Each process belongs to the first lane with a root among it and its ancestors, and the rest
/// lane gets the others. A feed whose direct child belongs elsewhere is skipped rather than leak it.
fn assign(sources: &[Source], processes: &Processes, sessions: &[u32], own: u32) -> Vec<Vec<u32>> {
    let roots: Vec<Vec<u32>> = sources
        .iter()
        .map(|source| match source {
            Source::Process(pid) if processes.0.contains_key(pid) => vec![*pid],
            Source::Named(names) => processes.roots(names),
            _ => Vec::new(),
        })
        .collect();
    let rest = sources.iter().position(|source| matches!(source, Source::Rest));
    let owner: HashMap<u32, Option<usize>> = processes
        .0
        .keys()
        .map(|&pid| {
            let line: Vec<u32> = std::iter::once(pid).chain(processes.ancestors(pid)).collect();
            // clip's own sound stays out of every lane but one asked for by its pid, as the sync test does
            let mine = line.contains(&own);
            let lane = (0..sources.len()).find(|&i| {
                (!mine || matches!(sources[i], Source::Process(_))) && roots[i].iter().any(|r| line.contains(r))
            });
            (pid, lane.or(rest.filter(|_| !mine)))
        })
        .collect();
    let mut wanted: Vec<u32> = roots.concat();
    wanted.extend(sessions.iter().filter(|pid| owner.contains_key(pid)));
    wanted.sort_unstable();
    wanted.dedup();
    wanted.sort_by_key(|&pid| processes.ancestors(pid).count());
    let mut trees: Vec<Vec<u32>> = sources
        .iter()
        .map(|source| match source {
            Source::Microphone => vec![MIC],
            Source::Except(pid) => vec![*pid],
            _ => Vec::new(),
        })
        .collect();
    // an exclude feed carves out its target's direct children and no deeper, so the lane rooted there keeps
    // to its root and leaves grandchildren to the exclude feed rather than record them twice
    let excepted = sources.iter().find_map(|source| if let Source::Except(pid) = source { Some(*pid) } else { None });
    for pid in wanted {
        let Some(lane) = owner[&pid] else { continue };
        if excepted.is_some_and(|x| roots[lane].contains(&x) && !roots[lane].contains(&pid)) {
            continue;
        }
        let covered = trees[lane].contains(&processes.0[&pid].0);
        let leaks = processes.0.iter().any(|(child, (parent, _))| *parent == pid && owner[child] != Some(lane));
        if !covered && !leaks {
            trees[lane].push(pid);
        }
    }
    trees.iter_mut().for_each(|tree| tree.sort_unstable());
    trees
}

/// Lanes recording on their own thread, each into a ring of AAC packets on the shared clock.
pub struct Recording {
    pub rings: Vec<Arc<Mutex<Ring>>>,
    thread: JoinHandle<Result<()>>,
}

impl Recording {
    /// Returns once every lane is recording, or with the reason one couldn't start.
    pub fn start(lanes: &[Lane], span: i64, clock: Clock, stop: Arc<AtomicBool>) -> Result<Self> {
        let rings: Vec<_> = lanes.iter().map(|_| Arc::new(Mutex::new(Ring::new(span)))).collect();
        let sources: Vec<Source> = lanes.iter().map(|lane| lane.source.clone()).collect();
        let (opened, ready) = mpsc::sync_channel(1);
        let thread = {
            let rings = rings.clone();
            std::thread::spawn(move || run(&sources, clock, &rings, &stop, opened))
        };
        match ready.recv() {
            Ok(Ok(())) => Ok(Self { rings, thread }),
            Ok(Err(e)) => {
                let _ = thread.join();
                Err(e)
            }
            Err(_) => Err(thread.join().expect("audio thread panicked").err().unwrap_or(E_FAIL.into())),
        }
    }

    pub fn finish(self) -> Result<()> {
        self.thread.join().expect("audio thread panicked")
    }
}

fn run(
    sources: &[Source],
    clock: Clock,
    rings: &[Arc<Mutex<Ring>>],
    stop: &AtomicBool,
    opened: mpsc::SyncSender<Result<()>>,
) -> Result<()> {
    unsafe { CoInitializeEx(None, COINIT_MULTITHREADED).ok()? };
    let changed = Arc::new(AtomicBool::new(false));
    let started = Sessions::new(changed.clone()).and_then(|mut sessions| {
        let mut channels = sources.iter().map(|source| Channel::new(source.clone())).collect::<Result<Vec<_>>>()?;
        rescan(&mut channels, &mut sessions)?;
        Ok((sessions, channels))
    });
    let _ = opened.send(started.as_ref().map(|_| ()).map_err(Clone::clone));
    let (mut sessions, mut channels) = started?;
    let mut scanned = Instant::now();
    while !stop.load(Ordering::Relaxed) {
        std::thread::sleep(POLL);
        if changed.swap(false, Ordering::Relaxed) || scanned.elapsed() >= RESCAN {
            if let Err(e) = rescan(&mut channels, &mut sessions) {
                eprintln!("couldn't look for new audio apps ({e})");
            }
            scanned = Instant::now();
        }
        for (channel, ring) in channels.iter_mut().zip(rings) {
            let (packets, lost) = channel.poll(&clock)?;
            if lost {
                changed.store(true, Ordering::Relaxed);
            }
            let mut ring = ring.lock().expect("ring poisoned");
            packets.into_iter().for_each(|p| ring.push(p));
        }
    }
    for (channel, ring) in channels.iter_mut().zip(rings) {
        let packets = channel.aac.finish()?;
        let mut ring = ring.lock().expect("ring poisoned");
        packets.into_iter().for_each(|p| ring.push(p));
    }
    Ok(())
}

fn rescan(channels: &mut [Channel], sessions: &mut Sessions) -> Result<()> {
    let sources: Vec<Source> = channels.iter().map(|channel| channel.source.clone()).collect();
    let targets = assign(&sources, &Processes::snapshot(), &sessions.pids()?, std::process::id());
    for (channel, pids) in channels.iter_mut().zip(targets) {
        channel.retarget(&pids);
    }
    Ok(())
}

/// One lane: a feed per process tree (or the microphone), mixed onto one timeline and encoded.
struct Channel {
    source: Source,
    feeds: Vec<Feed>,
    failed: Vec<(u32, Instant)>,
    position: i64,
    aac: Aac,
}

struct Feed {
    pid: u32,
    input: Input,
    timeline: Timeline,
}

impl Channel {
    /// The microphone opens here so a missing one stops the recording before it starts.
    fn new(source: Source) -> Result<Self> {
        let feeds = match source {
            Source::Microphone => vec![Feed { pid: MIC, input: Input::open(&source, MIC)?, timeline: Timeline::at(0) }],
            _ => Vec::new(),
        };
        Ok(Self { source, feeds, failed: Vec::new(), position: 0, aac: Aac::new()? })
    }

    fn retarget(&mut self, pids: &[u32]) {
        self.feeds.retain(|feed| pids.contains(&feed.pid));
        self.failed.retain(|(pid, at)| pids.contains(pid) && at.elapsed() < RETRY);
        for &pid in pids {
            if self.feeds.iter().any(|f| f.pid == pid) || self.failed.iter().any(|f| f.0 == pid) {
                continue;
            }
            match Input::open(&self.source, pid) {
                Ok(input) => self.feeds.push(Feed { pid, input, timeline: Timeline::at(self.position) }),
                Err(e) => {
                    eprintln!("can't record audio from process {pid} ({e})");
                    self.failed.push((pid, Instant::now()));
                }
            }
        }
    }

    /// The packets encoded since the last poll, and whether a feed was lost and needs reopening.
    fn poll(&mut self, clock: &Clock) -> Result<(Vec<Packet>, bool)> {
        let before = self.feeds.len();
        // a device unplugged or an app gone mid-game should cost that feed its sound, never the clip
        self.feeds.retain_mut(|feed| match feed.input.drain(clock, &mut feed.timeline) {
            Ok(()) => true,
            Err(e) => {
                eprintln!("audio from process {} lost ({e})", feed.pid);
                false
            }
        });
        let lost = self.feeds.len() < before;
        let target = (clock.now() - LAG) * RATE as i64 / SECOND;
        let mut timelines: Vec<&mut Timeline> = self.feeds.iter_mut().map(|feed| &mut feed.timeline).collect();
        let packets = match mix(&mut timelines, &mut self.position, target) {
            Some((pts, pcm)) => self.aac.encode(pts, &pcm)?,
            None => Vec::new(),
        };
        Ok((packets, lost))
    }
}

struct Input {
    client: IAudioClient,
    capture: IAudioCaptureClient,
}

impl Input {
    fn open(source: &Source, pid: u32) -> Result<Self> {
        unsafe {
            let (client, loopback) = match source {
                Source::Microphone => {
                    let devices: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
                    (devices.GetDefaultAudioEndpoint(eCapture, eConsole)?.Activate(CLSCTX_ALL, None)?, false)
                }
                Source::Except(_) => (process(pid, PROCESS_LOOPBACK_MODE_EXCLUDE_TARGET_PROCESS_TREE)?, true),
                _ => (process(pid, PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE)?, true),
            };
            // the engine converts to our format, so every lane feeds the encoder as is
            let flags = AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM
                | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY
                | if loopback { AUDCLNT_STREAMFLAGS_LOOPBACK } else { 0 };
            client.Initialize(AUDCLNT_SHAREMODE_SHARED, flags, DEVICE_BUFFER, 0, &wave(), None)?;
            let capture = client.GetService()?;
            client.Start()?;
            Ok(Self { client, capture })
        }
    }

    fn drain(&self, clock: &Clock, timeline: &mut Timeline) -> Result<()> {
        unsafe {
            while self.capture.GetNextPacketSize()? > 0 {
                let (mut data, mut frames, mut flags, mut qpc) = (std::ptr::null_mut(), 0, 0, 0);
                self.capture.GetBuffer(&mut data, &mut frames, &mut flags, None, Some(&mut qpc))?;
                let length = frames as usize * BLOCK;
                let at = match qpc {
                    0 => clock.now() - frames as i64 * SECOND / RATE as i64,
                    qpc => clock.at(qpc as i64),
                };
                if flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 {
                    timeline.push(at, &vec![0; length]);
                } else {
                    timeline.push(at, std::slice::from_raw_parts(data, length));
                }
                self.capture.ReleaseBuffer(frames)?;
            }
            Ok(())
        }
    }
}

impl Drop for Input {
    fn drop(&mut self) {
        let _ = unsafe { self.client.Stop() };
    }
}

/// The processes holding audio sessions on any active output, with `changed` raised whenever an
/// app opens a new session, so its sound is picked up as it starts rather than at the next rescan.
struct Sessions {
    devices: IMMDeviceEnumerator,
    managers: Vec<(String, IAudioSessionManager2)>,
    watch: IAudioSessionNotification,
}

#[implement(IAudioSessionNotification)]
struct Watch {
    changed: Arc<AtomicBool>,
}

impl IAudioSessionNotification_Impl for Watch_Impl {
    fn OnSessionCreated(&self, _: Ref<IAudioSessionControl>) -> Result<()> {
        self.changed.store(true, Ordering::Relaxed);
        Ok(())
    }
}

impl Sessions {
    fn new(changed: Arc<AtomicBool>) -> Result<Self> {
        let devices = unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)? };
        Ok(Self { devices, managers: Vec::new(), watch: Watch { changed }.into() })
    }

    fn pids(&mut self) -> Result<Vec<u32>> {
        let mut pids = Vec::new();
        unsafe {
            let endpoints = self.devices.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE)?;
            for i in 0..endpoints.GetCount()? {
                let device = endpoints.Item(i)?;
                let id = device.GetId()?;
                let key = id.to_string();
                CoTaskMemFree(Some(id.0 as _));
                let key = key?;
                let manager = match self.managers.iter().find(|(k, _)| *k == key) {
                    Some((_, manager)) => manager.clone(),
                    None => {
                        let manager: IAudioSessionManager2 = device.Activate(CLSCTX_ALL, None)?;
                        manager.RegisterSessionNotification(&self.watch)?;
                        self.managers.push((key, manager.clone()));
                        manager
                    }
                };
                // counting the sessions is also what switches notifications on
                let sessions = manager.GetSessionEnumerator()?;
                for j in 0..sessions.GetCount()? {
                    let control: IAudioSessionControl2 = sessions.GetSession(j)?.cast()?;
                    if control.IsSystemSoundsSession() == S_OK || control.GetState()? == AudioSessionStateExpired {
                        continue;
                    }
                    if let Ok(pid) = control.GetProcessId() {
                        pids.push(pid);
                    }
                }
            }
        }
        Ok(pids)
    }
}

impl Drop for Sessions {
    fn drop(&mut self) {
        for (_, manager) in &self.managers {
            let _ = unsafe { manager.UnregisterSessionNotification(&self.watch) };
        }
    }
}

#[implement(IActivateAudioInterfaceCompletionHandler, IAgileObject)]
struct Activated {
    done: mpsc::SyncSender<()>,
}

impl IActivateAudioInterfaceCompletionHandler_Impl for Activated_Impl {
    fn ActivateCompleted(&self, _: Ref<IActivateAudioInterfaceAsyncOperation>) -> Result<()> {
        let _ = self.done.try_send(());
        Ok(())
    }
}

impl IAgileObject_Impl for Activated_Impl {}

/// An audio client on one process tree's output, or on everything but it (Windows build 20348 and later).
unsafe fn process(pid: u32, mode: PROCESS_LOOPBACK_MODE) -> Result<IAudioClient> {
    unsafe {
        let params = AUDIOCLIENT_ACTIVATION_PARAMS {
            ActivationType: AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
            Anonymous: AUDIOCLIENT_ACTIVATION_PARAMS_0 {
                ProcessLoopbackParams: AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS {
                    TargetProcessId: pid,
                    ProcessLoopbackMode: mode,
                },
            },
        };
        // the crate's drop would PropVariantClear the blob, which points at the stack
        let mut variant = std::mem::ManuallyDrop::new(PROPVARIANT::default());
        let inner = &mut variant.Anonymous.Anonymous;
        inner.vt = VT_BLOB;
        inner.Anonymous.blob = BLOB { cbSize: size_of_val(&params) as u32, pBlobData: &params as *const _ as *mut u8 };
        let (done, completed) = mpsc::sync_channel(1);
        let handler: IActivateAudioInterfaceCompletionHandler = Activated { done }.into();
        let operation = ActivateAudioInterfaceAsync(
            VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK,
            &IAudioClient::IID,
            Some(&*variant),
            &handler,
        )?;
        completed
            .recv_timeout(ACTIVATION_TIMEOUT)
            .map_err(|_| windows::core::Error::new(E_FAIL, "audio activation timed out"))?;
        let mut result = HRESULT(0);
        let mut client = None;
        operation.GetActivateResult(&mut result, &mut client)?;
        result.ok()?;
        client.ok_or(windows::core::Error::from(E_FAIL))?.cast()
    }
}

/// Lays one feed's timestamped packets onto an unbroken run of samples counted from the clock epoch.
/// Gaps become silence and overlaps are trimmed, so a feed never drifts from video by more than
/// `SLACK`, and a feed that delivers nothing still advances with the clock.
struct Timeline {
    frames: i64,
    pending: Vec<u8>,
}

const SLACK: i64 = RATE as i64 / 100;
// how far behind the clock a quiet feed is filled, comfortably past any packet still in flight
const LAG: i64 = SECOND / 10;

impl Timeline {
    /// A timeline whose first sample is frame `frames`, where its lane's mix has got to.
    fn at(frames: i64) -> Self {
        Self { frames, pending: Vec::new() }
    }

    fn push(&mut self, at: i64, pcm: &[u8]) {
        let offset = at * RATE as i64 / SECOND - self.frames;
        let pcm = if offset > SLACK {
            self.silence(offset);
            pcm
        } else if offset < -SLACK {
            &pcm[(-offset as usize * BLOCK).min(pcm.len())..]
        } else {
            pcm
        };
        self.pending.extend_from_slice(pcm);
        self.frames += (pcm.len() / BLOCK) as i64;
    }

    fn fill(&mut self, frames: i64) {
        let behind = frames - self.frames;
        if behind > SLACK {
            self.silence(behind);
        }
    }

    fn silence(&mut self, frames: i64) {
        self.pending.resize(self.pending.len() + frames as usize * BLOCK, 0);
        self.frames += frames;
    }
}

/// Sums every feed from `position` up to the frame all of them have reached (or up to `target`
/// when the lane has none), returning the time of the first sample and the mixed samples. Every
/// timeline's pending samples start at `position`, before and after.
fn mix(timelines: &mut [&mut Timeline], position: &mut i64, target: i64) -> Option<(i64, Vec<u8>)> {
    timelines.iter_mut().for_each(|timeline| timeline.fill(target));
    let end = timelines.iter().map(|timeline| timeline.frames).min().unwrap_or(target);
    if end <= *position {
        return None;
    }
    let length = (end - *position) as usize * BLOCK;
    let mut sum = vec![0i32; length / 2];
    for timeline in timelines.iter_mut() {
        for (total, sample) in sum.iter_mut().zip(timeline.pending.drain(..length).as_slice().as_chunks::<2>().0) {
            *total += i16::from_le_bytes(*sample) as i32;
        }
    }
    let pcm =
        sum.iter().flat_map(|&total| (total.clamp(i16::MIN as i32, i16::MAX as i32) as i16).to_le_bytes()).collect();
    let pts = *position * SECOND / RATE as i64;
    *position = end;
    Some((pts, pcm))
}

/// The in-box AAC-LC encoder at 48 kHz stereo.
pub struct Aac {
    transform: IMFTransform,
    output_size: u32,
}

impl Aac {
    pub fn new() -> Result<Self> {
        unsafe {
            let input = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Audio, guidSubtype: MFAudioFormat_PCM };
            let output = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Audio, guidSubtype: MFAudioFormat_AAC };
            let mut found = std::ptr::null_mut();
            let mut count = 0;
            MFTEnumEx(
                MFT_CATEGORY_AUDIO_ENCODER,
                MFT_ENUM_FLAG_SYNCMFT | MFT_ENUM_FLAG_LOCALMFT | MFT_ENUM_FLAG_SORTANDFILTER,
                Some(&input),
                Some(&output),
                &mut found,
                &mut count,
            )?;
            let activates: Vec<Option<IMFActivate>> =
                (0..count as usize).map(|i| std::ptr::read(found.add(i))).collect();
            windows::Win32::System::Com::CoTaskMemFree(Some(found as _));
            let activate = activates
                .into_iter()
                .flatten()
                .next()
                .ok_or(windows::core::Error::new(MF_E_TOPO_CODEC_NOT_FOUND, "no aac encoder installed"))?;
            let transform: IMFTransform = activate.ActivateObject()?;

            let pcm = audio_type(MFAudioFormat_PCM)?;
            pcm.SetUINT32(&MF_MT_AUDIO_BLOCK_ALIGNMENT, BLOCK as u32)?;
            pcm.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, RATE * BLOCK as u32)?;
            transform.SetInputType(0, &pcm, 0)?;
            let aac = audio_type(MFAudioFormat_AAC)?;
            aac.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, AAC_BYTES_PER_SECOND)?;
            aac.SetUINT32(&MF_MT_AAC_PAYLOAD_TYPE, 0)?;
            transform.SetOutputType(0, &aac, 0)?;
            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;
            let output_size = transform.GetOutputStreamInfo(0)?.cbSize.max(8192);
            Ok(Self { transform, output_size })
        }
    }

    /// The encoder's output type, which carries the AudioSpecificConfig the muxer needs.
    pub fn output_type(&self) -> Result<IMFMediaType> {
        unsafe { self.transform.GetOutputCurrentType(0) }
    }

    pub fn encode(&mut self, pts: i64, pcm: &[u8]) -> Result<Vec<Packet>> {
        unsafe {
            let buffer = MFCreateMemoryBuffer(pcm.len() as u32)?;
            let mut target = std::ptr::null_mut();
            buffer.Lock(&mut target, None, None)?;
            std::ptr::copy_nonoverlapping(pcm.as_ptr(), target, pcm.len());
            buffer.Unlock()?;
            buffer.SetCurrentLength(pcm.len() as u32)?;
            let sample = MFCreateSample()?;
            sample.AddBuffer(&buffer)?;
            sample.SetSampleTime(pts)?;
            sample.SetSampleDuration((pcm.len() / BLOCK) as i64 * SECOND / RATE as i64)?;
            self.transform.ProcessInput(0, &sample, 0)?;
        }
        self.drain()
    }

    pub fn finish(&mut self) -> Result<Vec<Packet>> {
        unsafe { self.transform.ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0)? };
        self.drain()
    }

    fn drain(&mut self) -> Result<Vec<Packet>> {
        let mut out = Vec::new();
        loop {
            unsafe {
                let sample = MFCreateSample()?;
                sample.AddBuffer(&MFCreateMemoryBuffer(self.output_size)?)?;
                let mut buffer = [MFT_OUTPUT_DATA_BUFFER {
                    dwStreamID: 0,
                    pSample: std::mem::ManuallyDrop::new(Some(sample)),
                    dwStatus: 0,
                    pEvents: std::mem::ManuallyDrop::new(None),
                }];
                let mut status = 0;
                let result = self.transform.ProcessOutput(0, &mut buffer, &mut status);
                let sample = std::mem::ManuallyDrop::take(&mut buffer[0].pSample);
                drop(std::mem::ManuallyDrop::take(&mut buffer[0].pEvents));
                match result {
                    Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(out),
                    Err(e) => return Err(e),
                    Ok(()) => {
                        let sample = sample.ok_or(windows::core::Error::from(E_FAIL))?;
                        out.push(Packet { key: true, ..packet(&sample, AAC_FRAME)? });
                    }
                }
            }
        }
    }
}

/// 16 bit stereo pcm at `RATE`, the format every lane is captured and encoded in
pub fn wave() -> WAVEFORMATEX {
    WAVEFORMATEX {
        wFormatTag: WAVE_FORMAT_PCM as u16,
        nChannels: CHANNELS,
        nSamplesPerSec: RATE,
        nAvgBytesPerSec: RATE * BLOCK as u32,
        nBlockAlign: BLOCK as u16,
        wBitsPerSample: 16,
        cbSize: 0,
    }
}

pub fn audio_type(subtype: windows::core::GUID) -> Result<IMFMediaType> {
    unsafe {
        let media = MFCreateMediaType()?;
        media.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)?;
        media.SetGUID(&MF_MT_SUBTYPE, &subtype)?;
        media.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16)?;
        media.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, RATE)?;
        media.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, CHANNELS as u32)?;
        Ok(media)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: i64 = SECOND / 1000;
    const PER_MS: i64 = RATE as i64 / 1000;

    fn tone(ms: i64, value: i16) -> Vec<u8> {
        value.to_le_bytes().repeat((ms * PER_MS) as usize * CHANNELS as usize)
    }

    fn mixed(timelines: &mut [&mut Timeline], position: &mut i64, target_ms: i64) -> (i64, Vec<i16>) {
        let (pts, pcm) = mix(timelines, position, target_ms * PER_MS).unwrap();
        (pts, pcm.as_chunks::<2>().0.iter().map(|&s| i16::from_le_bytes(s)).collect())
    }

    fn values(ms: i64) -> usize {
        (ms * PER_MS) as usize * CHANNELS as usize
    }

    #[test]
    fn a_late_first_packet_is_preceded_by_silence() {
        let mut timeline = Timeline::at(0);
        timeline.push(50 * MS, &tone(10, 1));
        let (pts, pcm) = mixed(&mut [&mut timeline], &mut 0, 0);
        assert_eq!((pts, pcm.len()), (0, values(60)));
        assert!(pcm[..values(50)].iter().all(|&s| s == 0) && pcm[values(50)..].iter().all(|&s| s == 1));
    }

    #[test]
    fn jitter_within_slack_stays_contiguous() {
        let mut timeline = Timeline::at(0);
        timeline.push(0, &tone(10, 1));
        timeline.push(13 * MS, &tone(10, 1));
        timeline.push(18 * MS, &tone(10, 1));
        let (pts, pcm) = mixed(&mut [&mut timeline], &mut 0, 0);
        assert_eq!((pts, pcm.len()), (0, values(30)));
        assert!(pcm.iter().all(|&s| s == 1));
    }

    #[test]
    fn overlap_beyond_slack_is_trimmed() {
        let mut timeline = Timeline::at(0);
        timeline.push(0, &tone(100, 1));
        timeline.push(50 * MS, &tone(100, 1));
        assert_eq!(mixed(&mut [&mut timeline], &mut 0, 0).1.len(), values(150));
    }

    #[test]
    fn a_quiet_feed_keeps_pace_with_the_clock() {
        let mut timeline = Timeline::at(0);
        let mut position = 0;
        assert_eq!(mixed(&mut [&mut timeline], &mut position, 900).1.len(), values(900));
        assert!(mix(&mut [&mut timeline], &mut position, 905 * PER_MS).is_none());
        timeline.push(990 * MS, &tone(10, 1));
        let (pts, pcm) = mixed(&mut [&mut timeline], &mut position, 905);
        assert_eq!((pts, pcm.len()), (900 * MS, values(100)));
    }

    #[test]
    fn feeds_are_summed_and_clipped() {
        let (mut a, mut b) = (Timeline::at(0), Timeline::at(0));
        let mut position = 0;
        a.push(0, &tone(10, 30_000));
        b.push(0, &tone(10, 30_000));
        assert!(mixed(&mut [&mut a, &mut b], &mut position, 0).1.iter().all(|&s| s == i16::MAX));
        a.push(10 * MS, &tone(10, 100));
        b.push(10 * MS, &tone(10, -40));
        let (pts, pcm) = mixed(&mut [&mut a, &mut b], &mut position, 0);
        assert_eq!(pts, 10 * MS);
        assert!(pcm.iter().all(|&s| s == 60));
    }

    #[test]
    fn a_feed_joining_late_lines_up_with_the_mix() {
        let mut a = Timeline::at(0);
        let mut position = 0;
        a.push(0, &tone(20, 1));
        mixed(&mut [&mut a], &mut position, 0);
        let mut b = Timeline::at(position);
        b.push(15 * MS, &tone(15, 2));
        a.push(20 * MS, &tone(10, 1));
        let (pts, pcm) = mixed(&mut [&mut a, &mut b], &mut position, 0);
        assert_eq!((pts, pcm.len()), (20 * MS, values(10)));
        assert!(pcm.iter().all(|&s| s == 3));
    }

    #[test]
    fn a_lane_with_no_feeds_records_silence() {
        let (pts, pcm) = mixed(&mut [], &mut 0, 10);
        assert_eq!((pts, pcm.len()), (0, values(10)));
        assert!(pcm.iter().all(|&s| s == 0));
    }

    fn processes(list: &[(u32, u32, &str)]) -> Processes {
        Processes(list.iter().map(|&(pid, parent, name)| (pid, (parent, name.to_string()))).collect())
    }

    #[test]
    fn lanes_never_share_a_process_tree() {
        let running = processes(&[
            (1, 0, "explorer.exe"),
            (10, 1, "steam.exe"),
            (11, 10, "steamwebhelper.exe"),
            (20, 10, "game.exe"),
            (21, 20, "game-helper.exe"),
            (30, 1, "discord.exe"),
            (31, 30, "discord.exe"),
            (40, 1, "chrome.exe"),
            (41, 40, "chrome.exe"),
            (50, 1, "clip.exe"),
        ]);
        let sources = [
            Source::Process(20),
            Source::Rest,
            Source::Named(vec!["discord.exe".into()]),
            Source::Named(vec!["steam.exe".into()]),
            Source::Microphone,
        ];
        let trees = assign(&sources, &running, &[11, 21, 31, 41, 40, 10, 50, 99], 50);
        // steam is the game's parent, so its lane can't record steam itself, only the web helper under it
        assert_eq!(trees, vec![vec![20], vec![40], vec![30], vec![11], vec![MIC]]);
    }

    // fortnite as it runs here: epic's launcher holds a session four levels above the game
    fn fortnite() -> Processes {
        processes(&[
            (1, 0, "explorer.exe"),
            (9, 1, "epicgameslauncher.exe"),
            (10, 9, "fortnitebootstrapper.exe"),
            (11, 10, "fortnitelauncher.exe"),
            (12, 11, "fortniteclient-win64-shipping_eac_eos.exe"),
            (13, 12, "fortniteclient-win64-shipping.exe"),
            (14, 13, "crashreporter.exe"),
            (15, 14, "crashreporter.exe"),
        ])
    }

    #[test]
    fn a_sound_two_levels_below_a_feed_gets_its_own_feed() {
        assert_eq!(assign(&[Source::Rest], &fortnite(), &[9, 13], 7), vec![vec![9, 13]]);
        // a direct child is already in its parent's feed
        assert_eq!(assign(&[Source::Rest], &fortnite(), &[9, 10, 13, 14], 7), vec![vec![9, 13]]);
    }

    #[test]
    fn a_launcher_above_the_game_s_parent_is_recorded_and_the_game_s_grandchild_stays_in_its_lane() {
        let trees = assign(&[Source::Process(13), Source::Rest], &fortnite(), &[9, 13, 15], 7);
        assert_eq!(trees, vec![vec![13, 15], vec![9]]);
        // the game's direct parent can't be recorded without the game
        assert_eq!(assign(&[Source::Process(13), Source::Rest], &fortnite(), &[12], 7), vec![vec![13], vec![]]);
    }

    #[test]
    fn clip_s_parent_stays_out_but_its_grandparent_is_recorded() {
        let running =
            processes(&[(1, 0, "explorer.exe"), (2, 1, "terminal.exe"), (3, 2, "shell.exe"), (50, 3, "clip.exe")]);
        assert_eq!(assign(&[Source::Rest], &running, &[1, 2, 3, 50], 50), vec![vec![1]]);
    }

    #[test]
    fn a_reused_parent_id_stops_pulling_its_new_owner_into_the_game_tree() {
        // the game's launcher (20) exited and chrome now has its id, younger than the game it "parents"
        let mut running = processes(&[(1, 0, "explorer.exe"), (20, 1, "chrome.exe"), (21, 20, "game.exe")]).0;
        unlink_reused(&mut running, &HashMap::from([(1, 5), (20, 300), (21, 200)]));
        let trees = assign(&[Source::Process(21), Source::Rest], &Processes(running), &[20, 21], 7);
        assert_eq!(trees, vec![vec![21], vec![20]]);
    }

    #[test]
    fn a_parent_older_than_its_child_keeps_the_link() {
        let mut running = processes(&[(1, 0, "explorer.exe"), (20, 1, "steam.exe"), (21, 20, "game.exe")]).0;
        unlink_reused(&mut running, &HashMap::from([(1, 5), (20, 100), (21, 200)]));
        assert_eq!(running[&21].0, 20);
    }

    #[test]
    fn only_a_pid_lane_takes_clip_itself() {
        let running = processes(&[(1, 0, "explorer.exe"), (50, 1, "clip.exe")]);
        let sources = [Source::Named(vec!["clip.exe".into()]), Source::Process(50), Source::Rest];
        assert_eq!(assign(&sources, &running, &[50], 50), vec![vec![], vec![50], vec![]]);
    }

    #[test]
    fn an_except_lane_keeps_its_one_feed_whatever_else_runs() {
        let running = processes(&[(1, 0, "explorer.exe"), (10, 1, "steam.exe"), (20, 10, "game.exe")]);
        let trees = assign(&[Source::Process(20), Source::Except(20)], &running, &[10, 20], 7);
        assert_eq!(trees, vec![vec![20], vec![20]]);
        let trees = assign(&[Source::Process(13), Source::Except(13)], &fortnite(), &[9, 13, 15], 7);
        assert_eq!(trees, vec![vec![13], vec![13]]);
        assert_eq!(assign(&[Source::Except(20)], &processes(&[]), &[], 7), vec![vec![20]]);
    }

    #[test]
    fn a_game_that_exits_leaves_its_lane_empty() {
        let running = processes(&[(1, 0, "explorer.exe"), (40, 1, "chrome.exe")]);
        assert_eq!(assign(&[Source::Process(20), Source::Rest], &running, &[40], 7), vec![vec![], vec![40]]);
    }
}
