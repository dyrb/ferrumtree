//! Raw volume access: `\\.\X:` opened with `GENERIC_READ` only, sector-aligned buffers,
//! overlapped reads so the scanner can keep several large requests in flight.

use std::alloc::{Layout, alloc_zeroed, dealloc};
use std::io;
use std::ops::{Deref, DerefMut};
use std::ptr::{self, NonNull};

use windows_sys::Win32::Foundation::{CloseHandle, ERROR_IO_PENDING, GENERIC_READ, GetLastError, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_NO_BUFFERING, FILE_FLAG_OVERLAPPED, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    ReadFile,
};
use windows_sys::Win32::System::IO::{CancelIoEx, DeviceIoControl, GetOverlappedResult, OVERLAPPED};
use windows_sys::Win32::System::Threading::{CreateEventW, INFINITE, WaitForMultipleObjects};

use crate::boot::BootSector;
use crate::error::{Error, Result};

/// Alignment that satisfies `FILE_FLAG_NO_BUFFERING` for every sector size NTFS supports.
pub const IO_ALIGN: usize = 4096;

/// Heap buffer aligned to [`IO_ALIGN`].
pub struct AlignedBuf {
    ptr: NonNull<u8>,
    len: usize,
}

// SAFETY: plain owned heap memory.
unsafe impl Send for AlignedBuf {}
unsafe impl Sync for AlignedBuf {}

impl AlignedBuf {
    pub fn new(len: usize) -> Self {
        let layout = Layout::from_size_align(len.max(1), IO_ALIGN).expect("buffer layout");
        // SAFETY: non-zero size.
        let p = unsafe { alloc_zeroed(layout) };
        let ptr = NonNull::new(p).unwrap_or_else(|| std::alloc::handle_alloc_error(layout));
        AlignedBuf { ptr, len }
    }
}

impl Deref for AlignedBuf {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        // SAFETY: ptr is valid for len bytes for the lifetime of self.
        unsafe { std::slice::from_raw_parts(self.ptr.as_ptr(), self.len) }
    }
}

impl DerefMut for AlignedBuf {
    fn deref_mut(&mut self) -> &mut [u8] {
        // SAFETY: as above, and we have unique access.
        unsafe { std::slice::from_raw_parts_mut(self.ptr.as_ptr(), self.len) }
    }
}

impl Drop for AlignedBuf {
    fn drop(&mut self) {
        let layout = Layout::from_size_align(self.len.max(1), IO_ALIGN).unwrap();
        // SAFETY: allocated in `new` with the same layout.
        unsafe { dealloc(self.ptr.as_ptr(), layout) }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IoMode {
    /// Through the system file cache. Fast on repeated scans.
    Buffered,
    /// `FILE_FLAG_NO_BUFFERING`: straight from the device, no cache pollution.
    Direct,
}

/// One reusable overlapped request: OVERLAPPED block (heap-pinned) + manual-reset event.
pub struct ReadSlot {
    ov: Box<OVERLAPPED>,
    event: HANDLE,
    expected: u32,
}

// SAFETY: the event handle and the OVERLAPPED block are only touched by the owning thread.
unsafe impl Send for ReadSlot {}

impl ReadSlot {
    pub fn new() -> io::Result<Self> {
        // SAFETY: plain Win32 call; null name / default security.
        let event = unsafe { CreateEventW(ptr::null(), 1, 0, ptr::null()) };
        if event.is_null() {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: OVERLAPPED is a plain C struct; all-zero is its documented initial state.
        let ov = Box::new(unsafe { std::mem::zeroed::<OVERLAPPED>() });
        Ok(ReadSlot { ov, event, expected: 0 })
    }
}

impl Drop for ReadSlot {
    fn drop(&mut self) {
        // SAFETY: we own the event handle.
        unsafe { CloseHandle(self.event) };
    }
}

/// An open NTFS volume. Read-only by construction: the handle has no write access.
pub struct Volume {
    handle: HANDLE,
    pub boot: BootSector,
    pub mode: IoMode,
    /// Drive letter, or `None` for an image file.
    pub drive: Option<char>,
    /// Display name: `C:` or the image file name.
    pub name: String,
}

// SAFETY: a Win32 file handle may be used from any thread; all reads are overlapped with
// caller-owned OVERLAPPED blocks.
unsafe impl Send for Volume {}
unsafe impl Sync for Volume {}

/// Accepts `C`, `C:`, `C:\`, `\\.\C:`.
pub fn parse_drive_letter(spec: &str) -> Option<char> {
    let s = spec.trim().trim_start_matches(r"\\.\");
    let c = s.chars().next()?;
    let rest = &s[c.len_utf8()..];
    if c.is_ascii_alphabetic() && (rest.is_empty() || rest == ":" || rest == r":\" || rest == ":/") {
        Some(c.to_ascii_uppercase())
    } else {
        None
    }
}

impl Volume {
    /// Open a drive (`C:`) or a raw volume image file.
    pub fn open(spec: &str, mode: IoMode) -> Result<Volume> {
        let (drive, name, path) = if let Some(l) = parse_drive_letter(spec) {
            (Some(l), format!("{l}:"), format!(r"\\.\{l}:"))
        } else if std::path::Path::new(spec).is_file() {
            let name = std::path::Path::new(spec).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| spec.to_string());
            (None, name, spec.to_string())
        } else {
            return Err(Error::Unsupported("expected a drive letter or an image file"));
        };
        let path: Vec<u16> = path.encode_utf16().chain([0]).collect();
        let flags = FILE_FLAG_OVERLAPPED
            | match mode {
                IoMode::Direct => FILE_FLAG_NO_BUFFERING,
                // No FILE_FLAG_SEQUENTIAL_SCAN: it makes the cache manager drop pages
                // behind the reader, which defeats fast repeated scans.
                IoMode::Buffered => 0,
            };
        // SAFETY: valid NUL-terminated path; GENERIC_READ only.
        let handle = unsafe {
            CreateFileW(
                path.as_ptr(),
                GENERIC_READ,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                ptr::null(),
                OPEN_EXISTING,
                flags,
                ptr::null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return Err(Error::Io(io::Error::last_os_error()));
        }
        let mut vol = Volume {
            handle,
            boot: BootSector {
                bytes_per_sector: 512,
                cluster_size: 4096,
                total_sectors: 0,
                mft_lcn: 0,
                mftmirr_lcn: 0,
                record_size: 1024,
                index_record_size: 4096,
                serial: 0,
            },
            mode,
            drive,
            name,
        };
        let mut b = AlignedBuf::new(IO_ALIGN);
        vol.read_at(0, &mut b)?;
        vol.boot = BootSector::parse(&b[..512])?;
        Ok(vol)
    }

    /// Start an overlapped read of `len` bytes at byte offset `off` into `dst`.
    ///
    /// # Safety
    /// `dst..dst+len` must stay valid and unaliased until [`Volume::finish_read`] returns
    /// for this slot.
    pub unsafe fn start_read(&self, off: u64, dst: *mut u8, len: u32, slot: &mut ReadSlot) -> io::Result<()> {
        // SAFETY: zeroed OVERLAPPED is valid; fields set below.
        *slot.ov = unsafe { std::mem::zeroed() };
        slot.ov.hEvent = slot.event;
        slot.ov.Anonymous.Anonymous.Offset = off as u32;
        slot.ov.Anonymous.Anonymous.OffsetHigh = (off >> 32) as u32;
        slot.expected = len;
        // SAFETY: caller guarantees the destination; OVERLAPPED is heap-pinned in the slot.
        let ok = unsafe { ReadFile(self.handle, dst, len, ptr::null_mut(), &mut *slot.ov) };
        if ok == 0 {
            let e = unsafe { GetLastError() };
            if e != ERROR_IO_PENDING {
                return Err(io::Error::from_raw_os_error(e as i32));
            }
        }
        Ok(())
    }

    /// Wait for a read started with [`Volume::start_read`]. Errors on short reads.
    pub fn finish_read(&self, slot: &mut ReadSlot) -> io::Result<u32> {
        let mut n = 0u32;
        // SAFETY: slot.ov belongs to an operation started on this handle.
        let ok = unsafe { GetOverlappedResult(self.handle, &*slot.ov, &mut n, 1) };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        if n != slot.expected {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, format!("short read: {n} of {} bytes", slot.expected)));
        }
        Ok(n)
    }

    /// Synchronous `DeviceIoControl` on the volume handle (which is overlapped, so the call
    /// needs an OVERLAPPED block; `slot` is reused across calls to avoid creating events).
    /// Returns the number of output bytes.
    pub fn ioctl(&self, slot: &mut ReadSlot, code: u32, input: &[u8], output: &mut [u8]) -> io::Result<u32> {
        self.ioctl_unless(slot, code, input, output, ptr::null_mut()).map(|n| n.unwrap_or(0))
    }

    /// Like [`Volume::ioctl`], but gives up when the event `cancel` (if not null) is
    /// signaled first: the request is cancelled and `None` returned. For requests that
    /// block until something happens, such as waiting for USN journal entries.
    pub fn ioctl_unless(&self, slot: &mut ReadSlot, code: u32, input: &[u8], output: &mut [u8], cancel: HANDLE) -> io::Result<Option<u32>> {
        // SAFETY: zeroed OVERLAPPED is valid; both buffers outlive the request, which has
        // completed (or been cancelled and reaped) before returning.
        unsafe {
            *slot.ov = std::mem::zeroed();
            slot.ov.hEvent = slot.event;
            let ok = DeviceIoControl(
                self.handle,
                code,
                input.as_ptr().cast(),
                input.len() as u32,
                output.as_mut_ptr().cast(),
                output.len() as u32,
                ptr::null_mut(),
                &mut *slot.ov,
            );
            if ok == 0 {
                let e = GetLastError();
                if e != ERROR_IO_PENDING {
                    return Err(io::Error::from_raw_os_error(e as i32));
                }
                if !cancel.is_null() {
                    let events = [slot.event, cancel];
                    if WaitForMultipleObjects(2, events.as_ptr(), 0, INFINITE) == 1 {
                        CancelIoEx(self.handle, &*slot.ov);
                        let mut n = 0u32;
                        // Reap the request (completed or cancelled) before the buffers go.
                        if GetOverlappedResult(self.handle, &*slot.ov, &mut n, 1) != 0 {
                            return Ok(Some(n));
                        }
                        return Ok(None);
                    }
                }
            }
            let mut n = 0u32;
            if GetOverlappedResult(self.handle, &*slot.ov, &mut n, 1) == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(Some(n))
        }
    }

    /// Synchronous read. In direct mode `off`, `buf.len()` and the buffer address must be
    /// sector aligned (use [`AlignedBuf`]).
    pub fn read_at(&self, off: u64, buf: &mut [u8]) -> io::Result<()> {
        let mut slot = ReadSlot::new()?;
        let mut done = 0usize;
        while done < buf.len() {
            let len = (buf.len() - done).min(64 << 20) as u32;
            // SAFETY: buf outlives the wait right below.
            unsafe { self.start_read(off + done as u64, buf.as_mut_ptr().add(done), len, &mut slot)? };
            self.finish_read(&mut slot)?;
            done += len as usize;
        }
        Ok(())
    }
}

impl Drop for Volume {
    fn drop(&mut self) {
        // SAFETY: we own the handle.
        unsafe { CloseHandle(self.handle) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drive_letter_forms() {
        assert_eq!(parse_drive_letter("c"), Some('C'));
        assert_eq!(parse_drive_letter("C:"), Some('C'));
        assert_eq!(parse_drive_letter(r"e:\"), Some('E'));
        assert_eq!(parse_drive_letter(r"\\.\D:"), Some('D'));
        assert_eq!(parse_drive_letter("C:\\Windows"), None);
        assert_eq!(parse_drive_letter("1:"), None);
    }

    #[test]
    fn aligned_buf_is_aligned() {
        let b = AlignedBuf::new(10_000);
        assert_eq!(b.as_ptr() as usize % IO_ALIGN, 0);
        assert!(b.iter().all(|&x| x == 0));
    }
}
