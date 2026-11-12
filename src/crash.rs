//! A crash leaves a minidump in the cache and starts clip again out of sight, so a long game keeps its recorder.

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

use windows::core::{HSTRING, PWSTR};
use windows::Win32::Foundation::CloseHandle;
use windows::Win32::Storage::FileSystem::{
    CreateFileW, CREATE_ALWAYS, FILE_ATTRIBUTE_NORMAL, FILE_GENERIC_WRITE, FILE_SHARE_NONE,
};
use windows::Win32::System::Diagnostics::Debug::{
    MiniDumpWithHandleData, MiniDumpWithIndirectlyReferencedMemory, MiniDumpWithProcessThreadData,
    MiniDumpWithThreadInfo, MiniDumpWithUnloadedModules, MiniDumpWriteDump, SetUnhandledExceptionFilter,
    EXCEPTION_EXECUTE_HANDLER, EXCEPTION_POINTERS, MINIDUMP_EXCEPTION_INFORMATION,
};
use windows::Win32::System::SystemInformation::GetTickCount64;
use windows::Win32::System::Threading::{
    CreateProcessW, GetCurrentProcess, GetCurrentProcessId, GetCurrentThreadId, OpenProcess, WaitForSingleObject,
    PROCESS_CREATION_FLAGS, PROCESS_INFORMATION, PROCESS_SYNCHRONIZE, STARTUPINFOW,
};

/// The argument a restarted clip gets, followed by the pid of the one that crashed.
pub const RESTARTED: &str = "after-crash";

static DUMP: OnceLock<HSTRING> = OnceLock::new();
// built ahead, since the heap may be what broke
static RELAUNCH: Mutex<Vec<u16>> = Mutex::new(Vec::new());
static STARTED: AtomicU64 = AtomicU64::new(0);

/// The crash dumps kept in the cache, this run's included.
const KEPT: usize = 3;

/// Writes a crash to a dump in `dir` named after when this run started, and starts clip again in its place.
pub fn guard(dir: &Path) {
    let started = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let _ = DUMP.set(HSTRING::from(&*dir.join(format!("crash-{started}.dmp"))));
    let mut dumps: Vec<_> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|entry| entry.file_name().to_string_lossy().to_ascii_lowercase().ends_with(".dmp"))
        .filter_map(|entry| Some((entry.metadata().ok()?.modified().ok()?, entry.path())))
        .collect();
    dumps.sort_unstable_by(|a, b| b.0.cmp(&a.0));
    for (_, old) in dumps.iter().skip(KEPT - 1) {
        let _ = std::fs::remove_file(old);
    }
    if let Ok(exe) = std::env::current_exe() {
        let line = format!("\"{}\" {RESTARTED} {}", exe.display(), std::process::id());
        *RELAUNCH.lock().expect("relaunch poisoned") = line.encode_utf16().chain([0]).collect();
    }
    STARTED.store(unsafe { GetTickCount64() }, Ordering::Relaxed);
    unsafe { SetUnhandledExceptionFilter(Some(caught)) };
}

/// Waits for the clip that crashed to be gone, since it still holds the single instance.
pub fn wait_for(pid: &str) {
    let Ok(pid) = pid.parse() else { return };
    if let Ok(process) = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, false, pid) } {
        unsafe {
            WaitForSingleObject(process, 10_000);
            let _ = CloseHandle(process);
        }
    }
}

unsafe extern "system" fn caught(info: *const EXCEPTION_POINTERS) -> i32 {
    if let Some(path) = DUMP.get() {
        if let Ok(file) = unsafe {
            CreateFileW(path, FILE_GENERIC_WRITE.0, FILE_SHARE_NONE, None, CREATE_ALWAYS, FILE_ATTRIBUTE_NORMAL, None)
        } {
            let exception = MINIDUMP_EXCEPTION_INFORMATION {
                ThreadId: unsafe { GetCurrentThreadId() },
                ExceptionPointers: info.cast_mut(),
                ClientPointers: false.into(),
            };
            unsafe {
                // the stacks, what they point into and the open handles; full memory would run to gigabytes
                let kind = MiniDumpWithThreadInfo
                    | MiniDumpWithUnloadedModules
                    | MiniDumpWithIndirectlyReferencedMemory
                    | MiniDumpWithHandleData
                    | MiniDumpWithProcessThreadData;
                let _ = MiniDumpWriteDump(
                    GetCurrentProcess(),
                    GetCurrentProcessId(),
                    file,
                    kind,
                    Some(&exception),
                    None,
                    None,
                );
                let _ = CloseHandle(file);
            }
        }
    }
    // like windows' own restart, only after a minute, so a crash on start can't loop
    let lasted = unsafe { GetTickCount64() } - STARTED.load(Ordering::Relaxed);
    if let (true, Ok(mut line)) = (lasted >= 60_000, RELAUNCH.try_lock()) {
        if !line.is_empty() {
            let start = STARTUPINFOW { cb: size_of::<STARTUPINFOW>() as u32, ..Default::default() };
            let mut process = PROCESS_INFORMATION::default();
            let flags = PROCESS_CREATION_FLAGS(0);
            if unsafe {
                CreateProcessW(
                    None,
                    Some(PWSTR(line.as_mut_ptr())),
                    None,
                    None,
                    false,
                    flags,
                    None,
                    None,
                    &start,
                    &mut process,
                )
            }
            .is_ok()
            {
                unsafe {
                    let _ = CloseHandle(process.hProcess);
                    let _ = CloseHandle(process.hThread);
                }
            }
        }
    }
    EXCEPTION_EXECUTE_HANDLER
}
