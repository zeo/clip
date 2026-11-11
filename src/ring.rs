use std::collections::VecDeque;
use std::fs::{File, OpenOptions};
use std::os::windows::fs::{FileExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Instant;

use windows::core::HSTRING;
use windows::Win32::Storage::FileSystem::{
    GetDiskFreeSpaceExW, SetFileAttributesW, FILE_ATTRIBUTE_HIDDEN, FILE_FLAG_DELETE_ON_CLOSE,
};
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};

/// media foundation time base: 100 ns ticks
pub const SECOND: i64 = 10_000_000;

/// The longest buffer kept in memory, in seconds. Longer ones go to disk.
pub const IN_MEMORY: u32 = 600;

/// How much one of a ring's files on disk holds. Whole files go once they age out.
const SEGMENT: i64 = 60 * SECOND;

/// One timeline for every stream, in ticks since `start` on the performance counter that wasapi
/// stamps packets with and that `Instant` reads on windows.
#[derive(Clone, Copy)]
pub struct Clock {
    epoch: i64,
    pub started: Instant,
}

impl Clock {
    pub fn start() -> Self {
        Self { started: Instant::now(), epoch: counter() }
    }

    pub fn now(&self) -> i64 {
        counter() - self.epoch
    }

    /// Converts a performance counter reading already in 100 ns units, as wasapi reports them.
    pub fn at(&self, counter: i64) -> i64 {
        counter - self.epoch
    }
}

fn counter() -> i64 {
    let (mut count, mut frequency) = (0, 1);
    unsafe {
        let _ = QueryPerformanceCounter(&mut count);
        let _ = QueryPerformanceFrequency(&mut frequency);
    }
    (count as i128 * SECOND as i128 / frequency as i128) as i64
}

#[derive(Debug)]
pub struct Packet {
    pub pts: i64,
    pub duration: i64,
    pub key: bool,
    pub data: Vec<u8>,
}

/// A packet a ring holds, in memory or in one of its files, read back with [`Held::load`].
#[derive(Clone)]
pub struct Held {
    pub pts: i64,
    pub duration: i64,
    pub key: bool,
    pub len: usize,
    body: Body,
}

#[derive(Clone)]
enum Body {
    Memory(Arc<Packet>),
    Disk(Arc<File>, u64),
}

impl From<Arc<Packet>> for Held {
    fn from(packet: Arc<Packet>) -> Self {
        let (pts, duration, key, len) = (packet.pts, packet.duration, packet.key, packet.data.len());
        Self { pts, duration, key, len, body: Body::Memory(packet) }
    }
}

impl Held {
    pub fn load(&self) -> std::io::Result<Arc<Packet>> {
        let (file, at) = match &self.body {
            Body::Memory(packet) => return Ok(packet.clone()),
            Body::Disk(file, at) => (file, *at),
        };
        let mut data = vec![0; self.len];
        let mut done = 0;
        while done < data.len() {
            match file.seek_read(&mut data[done..], at + done as u64)? {
                0 => return Err(std::io::ErrorKind::UnexpectedEof.into()),
                read => done += read,
            }
        }
        Ok(Arc::new(Packet { pts: self.pts, duration: self.duration, key: self.key, data }))
    }
}

pub fn load(held: &[Held]) -> std::io::Result<Vec<Arc<Packet>>> {
    held.iter().map(Held::load).collect()
}

/// Where a session's long buffer goes: a hidden folder of files that windows deletes as soon as
/// clip lets go of them, also when it crashes.
#[derive(Clone)]
pub struct Spool {
    dir: PathBuf,
    session: String,
}

impl Spool {
    /// Opens `dir` for a buffer that needs `need` bytes, as long as the drive keeps [`ROOM`] free
    /// beside it.
    pub fn open(dir: &Path, need: u64) -> Result<Self, String> {
        static SESSIONS: AtomicU32 = AtomicU32::new(0);
        let free = room(dir);
        if free < need.saturating_add(ROOM) {
            let gb = |bytes: u64| (bytes as f64 / (1u64 << 30) as f64).ceil();
            return Err(format!("it needs {} GB and the drive has {} GB free", gb(need + ROOM), gb(free)));
        }
        std::fs::create_dir_all(dir).map_err(|e| format!("can't make {}: {e}", dir.display()))?;
        let _ = unsafe { SetFileAttributesW(&HSTRING::from(dir), FILE_ATTRIBUTE_HIDDEN) };
        let session = format!("{}.{}", std::process::id(), SESSIONS.fetch_add(1, Ordering::Relaxed));
        Ok(Self { dir: dir.into(), session })
    }

    /// Deletes what a power cut left behind, while no session of this run has files there.
    pub fn clear(dir: &Path) {
        for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

// a long recording stops before the drive fills up, and a long buffer won't start that close
pub const ROOM: u64 = 2_000_000_000;

/// Free bytes on the drive `path` is on, or as many as it takes when the drive can't say.
pub fn room(path: &Path) -> u64 {
    let found = path.ancestors().find(|dir| dir.is_dir());
    let mut free = u64::MAX;
    if let Some(dir) = found {
        let _ = unsafe { GetDiskFreeSpaceExW(&HSTRING::from(dir), Some(&mut free), None, None) };
    }
    free
}

/// A ring's files in a spool, a new one every [`SEGMENT`] from a keyframe on.
struct Disk {
    spool: Spool,
    name: String,
    files: u32,
    /// the file being written, the pts it starts at and where the next packet goes
    open: Option<(Arc<File>, i64, u64)>,
}

impl Disk {
    fn write(&mut self, packet: &Packet) -> std::io::Result<Held> {
        if self.open.as_ref().is_none_or(|(_, started, _)| packet.key && packet.pts - started >= SEGMENT) {
            let path = self.spool.dir.join(format!("{}-{}-{}", self.spool.session, self.name, self.files));
            self.files += 1;
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .custom_flags(FILE_FLAG_DELETE_ON_CLOSE.0)
                .open(path)?;
            self.open = Some((Arc::new(file), packet.pts, 0));
        }
        let (file, _, at) = self.open.as_mut().expect("opened above");
        let mut done = 0;
        while done < packet.data.len() {
            match file.seek_write(&packet.data[done..], *at + done as u64)? {
                0 => return Err(std::io::ErrorKind::WriteZero.into()),
                wrote => done += wrote,
            }
        }
        let body = Body::Disk(file.clone(), *at);
        *at += done as u64;
        Ok(Held { pts: packet.pts, duration: packet.duration, key: packet.key, len: done, body })
    }
}

/// Encoded packets covering at least `span` of the most recent stream, always starting on a keyframe.
/// Whole GOPs are evicted from the front so any snapshot decodes on its own.
pub struct Ring {
    packets: VecDeque<Held>,
    keys: VecDeque<i64>,
    span: i64,
    disk: Option<Disk>,
}

impl Ring {
    pub fn new(span: i64) -> Self {
        Self { packets: VecDeque::new(), keys: VecDeque::new(), span, disk: None }
    }

    /// A ring that keeps its packets in files in `spool` named after `name`.
    pub fn spooled(span: i64, spool: Spool, name: &str) -> Self {
        Self { disk: Some(Disk { spool, name: name.into(), files: 0, open: None }), ..Self::new(span) }
    }

    pub fn push(&mut self, packet: Packet) {
        if self.packets.is_empty() && !packet.key {
            return;
        }
        let held = match self.disk.as_mut().map(|disk| disk.write(&packet)) {
            Some(Ok(held)) => held,
            None => Held::from(Arc::new(packet)),
            // a drive that fills up or goes away keeps what it holds, and memory takes the rest
            Some(Err(_)) => {
                self.disk = None;
                self.span = self.span.min(IN_MEMORY as i64 * SECOND);
                Held::from(Arc::new(packet))
            }
        };
        if held.key {
            self.keys.push_back(held.pts);
        }
        let end = held.pts + held.duration;
        self.packets.push_back(held);
        while self.keys.len() > 1 && end - self.keys[1] >= self.span {
            self.keys.pop_front();
            while self.packets.front().is_some_and(|p| p.pts < self.keys[0]) {
                self.packets.pop_front();
            }
        }
    }

    /// Packets from the last keyframe at or before `since` through the newest one.
    pub fn since(&self, since: i64) -> Vec<Held> {
        let start = self.keys.iter().rev().find(|&&k| k <= since).or(self.keys.front()).copied();
        let Some(start) = start else {
            return Vec::new();
        };
        self.packets.iter().skip_while(|p| p.pts < start).cloned().collect()
    }

    pub fn end(&self) -> Option<i64> {
        self.packets.back().map(|p| p.pts + p.duration)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FRAME: i64 = SECOND / 10;

    fn feed(ring: &mut Ring, frames: i64, gop: i64) {
        for i in 0..frames {
            ring.push(Packet { pts: i * FRAME, duration: FRAME, key: i % gop == 0, data: vec![0; 100] });
        }
    }

    #[test]
    fn keeps_at_least_the_span_and_starts_on_a_keyframe() {
        let mut ring = Ring::new(3 * SECOND);
        feed(&mut ring, 100, 10);
        let all = ring.since(i64::MIN);
        assert!(all[0].key);
        let covered = ring.end().unwrap() - all[0].pts;
        assert!(covered >= 3 * SECOND, "covered {covered}");
        assert!(covered < 4 * SECOND, "kept a whole extra gop: {covered}");
    }

    #[test]
    fn drops_packets_before_the_first_keyframe() {
        let mut ring = Ring::new(SECOND);
        ring.push(Packet { pts: 0, duration: FRAME, key: false, data: vec![1] });
        assert!(ring.since(i64::MIN).is_empty());
        ring.push(Packet { pts: FRAME, duration: FRAME, key: true, data: vec![2] });
        assert_eq!(ring.since(i64::MIN).len(), 1);
    }

    #[test]
    fn a_spooled_ring_reads_back_what_memory_holds_and_lets_old_files_go() {
        let dir = std::env::temp_dir().join(format!("clip-spool-{}", std::process::id()));
        let (mut memory, mut disk) =
            (Ring::new(150 * SECOND), Ring::spooled(150 * SECOND, Spool::open(&dir, 0).unwrap(), "video"));
        for i in 0..3000 {
            let packet =
                || Packet { pts: i * FRAME, duration: FRAME, key: i % 10 == 0, data: i.to_le_bytes().repeat(9) };
            memory.push(packet());
            disk.push(packet());
        }
        let (kept, read) = (load(&memory.since(i64::MIN)).unwrap(), load(&disk.since(i64::MIN)).unwrap());
        assert_eq!(kept.len(), read.len());
        assert!(kept.iter().zip(&read).all(|(a, b)| (a.pts, a.key, &a.data) == (b.pts, b.key, &b.data)));
        let files = || std::fs::read_dir(&dir).unwrap().count();
        // 150 s back from 300 s reaches into the file started at 120 s
        assert_eq!(files(), 3);
        let snapshot = disk.since(i64::MIN);
        drop(disk);
        assert_eq!(files(), 3, "a snapshot keeps its files");
        drop(snapshot);
        assert_eq!(files(), 0);
        std::fs::remove_dir(&dir).unwrap();
    }

    #[test]
    fn since_snaps_back_to_the_previous_keyframe() {
        let mut ring = Ring::new(10 * SECOND);
        feed(&mut ring, 50, 10);
        let clip = ring.since(25 * FRAME);
        assert_eq!(clip[0].pts, 20 * FRAME);
        assert!(clip[0].key);
        assert_eq!(clip.len(), 30);
        assert_eq!(ring.since(-5).first().unwrap().pts, 0);
    }
}
