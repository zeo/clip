use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use windows::core::{implement, Interface, Ref, Result, HRESULT};
use windows::Win32::Foundation::{CloseHandle, E_FAIL};
use windows::Win32::Media::Audio::*;
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, IAgileObject, IAgileObject_Impl, BLOB, CLSCTX_ALL, COINIT_MULTITHREADED,
};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::Variant::VT_BLOB;

use crate::encode::packet;
use crate::ring::{Clock, Packet, Ring, SECOND};

pub const RATE: u32 = 48_000;
const CHANNELS: u16 = 2;
const BLOCK: usize = 2 * CHANNELS as usize;
// 192 kbps, the highest rate the in-box encoder offers
const AAC_BYTES_PER_SECOND: u32 = 24_000;
const AAC_FRAME: i64 = 1024 * SECOND / RATE as i64;
const POLL: Duration = Duration::from_millis(10);
const DEVICE_BUFFER: i64 = SECOND / 5;
const ACTIVATION_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, Debug)]
pub enum Source {
    /// a process and its children
    Include(u32),
    /// everything the system plays except a process and its children
    Exclude(u32),
    Microphone,
}

pub struct Lane {
    pub name: &'static str,
    pub source: Source,
}

/// The tracks a clip gets. With a game it is split from everything else; voice chat gets its own
/// lane too, and also stays in `others` because exclusion takes a single process tree.
pub fn lanes(game: Option<u32>, mic: bool) -> Vec<Lane> {
    let mut lanes = match game {
        Some(pid) => vec![
            Lane { name: "game", source: Source::Include(pid) },
            Lane { name: "others", source: Source::Exclude(pid) },
        ],
        None => vec![Lane { name: "desktop", source: Source::Exclude(std::process::id()) }],
    };
    if let Some(pid) = discord().filter(|&pid| Some(pid) != game) {
        lanes.push(Lane { name: "voice", source: Source::Include(pid) });
    }
    if mic {
        lanes.push(Lane { name: "mic", source: Source::Microphone });
    }
    lanes
}

/// The root discord process, whose tree plays voice chat.
fn discord() -> Option<u32> {
    const NAMES: [&str; 3] = ["discord.exe", "discordptb.exe", "discordcanary.exe"];
    let mut found = Vec::new();
    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0).ok()?;
        let mut entry = PROCESSENTRY32W { dwSize: size_of::<PROCESSENTRY32W>() as u32, ..Default::default() };
        let mut more = Process32FirstW(snapshot, &mut entry).is_ok();
        while more {
            let len = entry.szExeFile.iter().position(|&c| c == 0).unwrap_or(entry.szExeFile.len());
            let name = String::from_utf16_lossy(&entry.szExeFile[..len]);
            if NAMES.iter().any(|n| n.eq_ignore_ascii_case(&name)) {
                found.push((entry.th32ProcessID, entry.th32ParentProcessID));
            }
            more = Process32NextW(snapshot, &mut entry).is_ok();
        }
        let _ = CloseHandle(snapshot);
    }
    found.iter().find(|(_, parent)| !found.iter().any(|(pid, _)| pid == parent)).map(|&(pid, _)| pid)
}

/// Lanes recording on their own thread, each into a ring of AAC packets on the shared clock.
pub struct Recording {
    pub rings: Vec<Arc<Mutex<Ring>>>,
    thread: JoinHandle<Result<()>>,
}

impl Recording {
    /// Returns once every lane's device is open, or with the first one that failed.
    pub fn start(lanes: &[Lane], span: i64, clock: Clock, stop: Arc<AtomicBool>) -> Result<Self> {
        let rings: Vec<_> = lanes.iter().map(|_| Arc::new(Mutex::new(Ring::new(span)))).collect();
        let sources: Vec<Source> = lanes.iter().map(|lane| lane.source).collect();
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
    let channels = sources.iter().map(|&source| Channel::open(source)).collect::<Result<Vec<_>>>();
    let _ = opened.send(channels.as_ref().map(|_| ()).map_err(Clone::clone));
    let mut channels = channels?;
    while !stop.load(Ordering::Relaxed) {
        std::thread::sleep(POLL);
        for (channel, ring) in channels.iter_mut().zip(rings) {
            let packets = channel.poll(&clock)?;
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

struct Channel {
    input: Option<Input>,
    timeline: Timeline,
    aac: Aac,
}

impl Channel {
    fn open(source: Source) -> Result<Self> {
        Ok(Self { input: Some(Input::open(source)?), timeline: Timeline::default(), aac: Aac::new()? })
    }

    fn poll(&mut self, clock: &Clock) -> Result<Vec<Packet>> {
        if let Some(Err(e)) = self.input.as_ref().map(|input| input.drain(clock, &mut self.timeline)) {
            // a device unplugged mid-game should cost that lane its sound, never the clip
            eprintln!("audio input lost ({e}), recording silence on that track");
            self.input = None;
        }
        self.timeline.fill(clock.now());
        match self.timeline.take() {
            Some((pts, pcm)) => self.aac.encode(pts, &pcm),
            None => Ok(Vec::new()),
        }
    }
}

struct Input {
    client: IAudioClient,
    capture: IAudioCaptureClient,
}

impl Input {
    fn open(source: Source) -> Result<Self> {
        unsafe {
            let (client, loopback) = match source {
                Source::Include(pid) => (process(pid, PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE)?, true),
                Source::Exclude(pid) => (process(pid, PROCESS_LOOPBACK_MODE_EXCLUDE_TARGET_PROCESS_TREE)?, true),
                Source::Microphone => {
                    let devices: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
                    (devices.GetDefaultAudioEndpoint(eCapture, eConsole)?.Activate(CLSCTX_ALL, None)?, false)
                }
            };
            // the engine converts to our format, so every lane feeds the encoder as is
            let flags = AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM
                | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY
                | if loopback { AUDCLNT_STREAMFLAGS_LOOPBACK } else { 0 };
            let format = WAVEFORMATEX {
                wFormatTag: WAVE_FORMAT_PCM as u16,
                nChannels: CHANNELS,
                nSamplesPerSec: RATE,
                nAvgBytesPerSec: RATE * BLOCK as u32,
                nBlockAlign: BLOCK as u16,
                wBitsPerSample: 16,
                cbSize: 0,
            };
            client.Initialize(AUDCLNT_SHAREMODE_SHARED, flags, DEVICE_BUFFER, 0, &format, None)?;
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

/// An audio client on one process tree's output, included or excluded (Windows build 20348 and later).
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

/// Lays timestamped capture packets onto one unbroken run of samples counted from the clock epoch.
/// Gaps become silence and overlaps are trimmed, so a lane never drifts from video by more than
/// `SLACK`, and a lane that delivers nothing still advances with the clock.
#[derive(Default)]
struct Timeline {
    frames: i64,
    pending: Vec<u8>,
}

const SLACK: i64 = RATE as i64 / 100;
// how far behind the clock a quiet lane is filled, comfortably past any packet still in flight
const LAG: i64 = SECOND / 10;

impl Timeline {
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

    fn fill(&mut self, now: i64) {
        let behind = (now - LAG) * RATE as i64 / SECOND - self.frames;
        if behind > SLACK {
            self.silence(behind);
        }
    }

    fn silence(&mut self, frames: i64) {
        self.pending.resize(self.pending.len() + frames as usize * BLOCK, 0);
        self.frames += frames;
    }

    /// The samples gathered since the last call and the time of the first one.
    fn take(&mut self) -> Option<(i64, Vec<u8>)> {
        if self.pending.is_empty() {
            return None;
        }
        let start = self.frames - (self.pending.len() / BLOCK) as i64;
        Some((start * SECOND / RATE as i64, std::mem::take(&mut self.pending)))
    }
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

fn audio_type(subtype: windows::core::GUID) -> Result<IMFMediaType> {
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

    fn frames(ms: i64) -> Vec<u8> {
        vec![1; (ms * RATE as i64 / 1000) as usize * BLOCK]
    }

    fn take_frames(timeline: &mut Timeline) -> (i64, usize) {
        let (pts, pcm) = timeline.take().unwrap();
        (pts, pcm.len() / BLOCK)
    }

    #[test]
    fn a_late_first_packet_is_preceded_by_silence() {
        let mut timeline = Timeline::default();
        timeline.push(50 * MS, &frames(10));
        let (pts, pcm) = timeline.take().unwrap();
        assert_eq!(pts, 0);
        assert_eq!(pcm.len() / BLOCK, 60 * 48);
        assert!(pcm[..50 * 48 * BLOCK].iter().all(|&b| b == 0));
        assert!(pcm[50 * 48 * BLOCK..].iter().all(|&b| b == 1));
    }

    #[test]
    fn jitter_within_slack_stays_contiguous() {
        let mut timeline = Timeline::default();
        timeline.push(0, &frames(10));
        timeline.push(13 * MS, &frames(10));
        timeline.push(18 * MS, &frames(10));
        assert_eq!(take_frames(&mut timeline), (0, 30 * 48));
    }

    #[test]
    fn overlap_beyond_slack_is_trimmed() {
        let mut timeline = Timeline::default();
        timeline.push(0, &frames(100));
        timeline.push(50 * MS, &frames(100));
        assert_eq!(take_frames(&mut timeline), (0, 150 * 48));
    }

    #[test]
    fn a_quiet_lane_keeps_pace_with_the_clock() {
        let mut timeline = Timeline::default();
        timeline.fill(1000 * MS);
        assert_eq!(take_frames(&mut timeline), (0, 900 * 48));
        timeline.fill(1005 * MS);
        assert!(timeline.take().is_none());
        timeline.push(990 * MS, &frames(10));
        assert_eq!(take_frames(&mut timeline), (900 * MS, 100 * 48));
    }
}
