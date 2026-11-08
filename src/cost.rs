//! What clip itself costs the machine, for the quick panel.

use std::time::Instant;

use serde::Serialize;
use windows::core::HSTRING;
use windows::Win32::Foundation::FILETIME;
use windows::Win32::System::Performance::{
    PdhAddEnglishCounterW, PdhCloseQuery, PdhCollectQueryData, PdhGetFormattedCounterArrayW, PdhOpenQueryW,
    PDH_CSTATUS_VALID_DATA, PDH_FMT_COUNTERVALUE_ITEM_W, PDH_FMT_DOUBLE, PDH_HCOUNTER, PDH_HQUERY, PDH_MORE_DATA,
};
use windows::Win32::System::ProcessStatus::{
    K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX,
};
use windows::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};

#[derive(Serialize)]
pub struct Cost {
    /// share of the whole cpu, in percent, once there is a span to measure over
    cpu: Option<f64>,
    /// bytes clip has committed, which is mostly the buffer
    memory: u64,
    /// how busy the gpu's video encoder is with clip's frames, in percent, while one runs
    encoder: Option<f64>,
}

pub struct Meter {
    query: PDH_HQUERY,
    counter: PDH_HCOUNTER,
    last: (Instant, u64),
}

// the handles are only used by whoever holds the meter
unsafe impl Send for Meter {}

impl Meter {
    pub fn new() -> Self {
        let (query, counter) = encoder_query();
        Self { query, counter, last: (Instant::now(), busy()) }
    }

    pub fn read(&mut self) -> Cost {
        let (then, was) = std::mem::replace(&mut self.last, (Instant::now(), busy()));
        let wall = self.last.0 - then;
        let cores = std::thread::available_parallelism().map_or(1, |n| n.get()) as f64;
        let cpu = (wall.as_millis() >= 500).then(|| (self.last.1 - was) as f64 / 1e5 / wall.as_secs_f64() / cores);
        Cost { cpu, memory: memory(), encoder: self.encoder() }
    }

    fn encoder(&mut self) -> Option<f64> {
        let busiest = unsafe { busiest(self.query, self.counter) };
        // the encoder's engines only show up once a session uses them, so look again next time
        if busiest.is_err() {
            unsafe { PdhCloseQuery(self.query) };
            (self.query, self.counter) = encoder_query();
        }
        busiest.ok().flatten()
    }
}

impl Drop for Meter {
    fn drop(&mut self) {
        unsafe { PdhCloseQuery(self.query) };
    }
}

/// The video encode engines this process uses, one instance per engine.
fn encoder_query() -> (PDH_HQUERY, PDH_HCOUNTER) {
    let (mut query, mut counter) = (PDH_HQUERY::default(), PDH_HCOUNTER::default());
    let path = format!("\\GPU Engine(pid_{}_*engtype_VideoEncode)\\Utilization Percentage", std::process::id());
    unsafe {
        PdhOpenQueryW(None, 0, &mut query);
        PdhAddEnglishCounterW(query, &HSTRING::from(path), 0, &mut counter);
        PdhCollectQueryData(query);
    }
    (query, counter)
}

/// The busiest engine since the last call, `Err` while there are none to read.
unsafe fn busiest(query: PDH_HQUERY, counter: PDH_HCOUNTER) -> Result<Option<f64>, u32> {
    let status = unsafe { PdhCollectQueryData(query) };
    if status != 0 {
        return Err(status);
    }
    let (mut size, mut count) = (0, 0);
    let status = unsafe { PdhGetFormattedCounterArrayW(counter, PDH_FMT_DOUBLE, &mut size, &mut count, None) };
    if status != PDH_MORE_DATA {
        return Err(status);
    }
    let mut items =
        vec![PDH_FMT_COUNTERVALUE_ITEM_W::default(); size as usize / size_of::<PDH_FMT_COUNTERVALUE_ITEM_W>() + 1];
    let status = unsafe {
        PdhGetFormattedCounterArrayW(counter, PDH_FMT_DOUBLE, &mut size, &mut count, Some(items.as_mut_ptr()))
    };
    if status != 0 {
        return Err(status);
    }
    let valid = items[..count as usize].iter().filter(|item| item.FmtValue.CStatus == PDH_CSTATUS_VALID_DATA);
    Ok(valid.map(|item| unsafe { item.FmtValue.Anonymous.doubleValue }).reduce(f64::max))
}

/// Cpu time this process has used, in 100 ns ticks.
fn busy() -> u64 {
    let ticks = |time: FILETIME| (time.dwHighDateTime as u64) << 32 | time.dwLowDateTime as u64;
    let (mut created, mut exited, mut kernel, mut user) = Default::default();
    let read = unsafe { GetProcessTimes(GetCurrentProcess(), &mut created, &mut exited, &mut kernel, &mut user) };
    read.map_or(0, |()| ticks(kernel) + ticks(user))
}

fn memory() -> u64 {
    let mut counters =
        PROCESS_MEMORY_COUNTERS_EX { cb: size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32, ..Default::default() };
    let ok = unsafe {
        K32GetProcessMemoryInfo(GetCurrentProcess(), (&raw mut counters).cast::<PROCESS_MEMORY_COUNTERS>(), counters.cb)
    };
    if ok.as_bool() {
        counters.PrivateUsage as u64
    } else {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_its_own_cost() {
        let mut meter = Meter::new();
        let spin = Instant::now();
        while spin.elapsed().as_millis() < 600 {
            std::hint::black_box(0u64);
        }
        let cost = meter.read();
        assert!(cost.cpu.is_some_and(|cpu| cpu > 0.0 && cpu <= 100.0), "{:?}", cost.cpu);
        assert!(cost.memory > 1 << 20);
        // nothing encodes in a test
        assert_eq!(cost.encoder, None);
    }
}
