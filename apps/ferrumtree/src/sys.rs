//! Small Win32 helpers for reporting.

use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
use windows_sys::Win32::System::ProcessStatus::{K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
use windows_sys::Win32::System::Threading::GetCurrentProcess;

pub fn peak_working_set() -> u64 {
    // SAFETY: plain struct out-parameter of the documented size.
    unsafe {
        let mut c: PROCESS_MEMORY_COUNTERS = std::mem::zeroed();
        c.cb = size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
        if K32GetProcessMemoryInfo(GetCurrentProcess(), &mut c, c.cb) != 0 { c.PeakWorkingSetSize as u64 } else { 0 }
    }
}

/// (total bytes, free bytes) of the volume.
pub fn disk_space(letter: char) -> Option<(u64, u64)> {
    let root: Vec<u16> = format!("{letter}:\\").encode_utf16().chain([0]).collect();
    let (mut avail, mut total, mut free) = (0u64, 0u64, 0u64);
    // SAFETY: valid NUL-terminated path and out-pointers.
    let ok = unsafe { GetDiskFreeSpaceExW(root.as_ptr(), &mut avail, &mut total, &mut free) };
    (ok != 0).then_some((total, free))
}
