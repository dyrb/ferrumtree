//! The record catalog: everything a full MFT scan produces (one [`Rec`] per record, all
//! names), plus the journal position it is current to.
//!
//! ## File format
//!
//! A 4 KiB header followed by four page-aligned sections - records, name entries, name
//! bytes, hot list - each with spare capacity. All little-endian raw arrays: loading is a
//! `read_exact` per section into zeroed memory, no parsing.
//!
//! Saving after an update writes only what changed: the 4 KiB pages holding modified
//! records or retired name entries, the appended names and name bytes, the hot list and
//! the header. A full rewrite (temp file + rename) happens only for a new catalog, after
//! name compaction, or when a section outgrows its capacity. The header carries a
//! checksum over all content and is written last, so an interrupted save fails
//! verification on the next load and leads to a full scan instead of wrong data.
//!
//! ## Names
//!
//! Names of a re-read record are retired in place (`record = DEAD`) and its current names
//! appended; nothing moves, so only touched pages need writing. When more than a quarter
//! of the entries are dead the list is compacted in one in-place pass (`names` are kept
//! ordered by arena offset, so data only ever moves down).
//!
//! ## Hot files
//!
//! `hot` lists files that still have an open writer according to the journal (their last
//! entry was not a close). NTFS logs each change reason once per handle, so such a file
//! can grow without new entries; hot records are re-read on every update until closed.

use std::alloc::{Layout, alloc_zeroed};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom};
use std::os::windows::fs::{FileExt, OpenOptionsExt};
use std::os::windows::io::AsRawHandle;
use std::path::{Path, PathBuf};

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
use windows_sys::Win32::System::Memory::{
    CreateFileMappingW, FILE_MAP_READ, FILE_MAP_WRITE, MEMORY_MAPPED_VIEW_ADDRESS, MapViewOfFile, PAGE_READWRITE, UnmapViewOfFile,
    VirtualUnlock,
};

use crate::region::Region;

use ft_index::patch::{Change, Name};
use ft_ntfs::record::{self, AttrValue, attr, parse_attribute_list, parse_record_unprotected};
use ft_ntfs::scan::Scan;
use ft_ntfs::{Extent, NameEntry, Rec, Sink, rflags};

#[cfg(not(target_endian = "little"))]
compile_error!("the catalog file format is little-endian");

const _: () = assert!(size_of::<Rec>() == 32 && size_of::<NameEntry>() == 16);

const MAGIC: [u8; 8] = *b"FERRUMCT";
const VERSION: u32 = 3;
const HEADER_LEN: usize = 4096;
const SECTIONS_AT: usize = 48;
const CHECKSUM_AT: usize = SECTIONS_AT + 4 * 24;
const PAGE: u64 = 4096;
/// Element size of each section: records, name entries, name bytes, hot list.
const ELEM: [u64; 4] = [32, 16, 1, 4];
/// Minimum spare capacity per section, in elements.
const SLACK: [u64; 4] = [4096, 8192, 256 << 10, 4096];

/// Owner of a retired name entry (its slot stays until the next compaction).
pub const DEAD: u32 = u32::MAX;

/// Types made only of integers with no padding: every bit pattern is a valid value.
///
/// # Safety
/// Implement only for such types.
unsafe trait Plain: Copy {}
unsafe impl Plain for Rec {}
unsafe impl Plain for NameEntry {}
unsafe impl Plain for u32 {}
unsafe impl Plain for u8 {}

fn bytes_of<T: Plain>(v: &[T]) -> &[u8] {
    // SAFETY: Plain types have no padding, so all bytes are initialised.
    unsafe { std::slice::from_raw_parts(v.as_ptr().cast(), size_of_val(v)) }
}

fn bytes_of_mut<T: Plain>(v: &mut [T]) -> &mut [u8] {
    // SAFETY: any byte pattern is a valid Plain value.
    unsafe { std::slice::from_raw_parts_mut(v.as_mut_ptr().cast(), size_of_val(v)) }
}

/// `n` zeroed elements straight from the allocator (zero pages, no memset pass), with
/// room for `cap` before reallocating.
fn zeroed_vec<T: Plain>(n: usize, cap: usize) -> io::Result<Vec<T>> {
    let cap = cap.max(n);
    if cap == 0 {
        return Ok(Vec::new());
    }
    let layout = Layout::array::<T>(cap).map_err(|_| io::Error::new(io::ErrorKind::OutOfMemory, "catalog too large"))?;
    // SAFETY: non-zero layout; all-zero is a valid T (Plain).
    unsafe {
        let p = alloc_zeroed(layout);
        if p.is_null() {
            std::alloc::handle_alloc_error(layout);
        }
        Ok(Vec::from_raw_parts(p.cast(), n, cap))
    }
}

/// 4-lane multiply-rotate hash: integrity check for the cache file, not cryptography.
fn hash(b: &[u8], seed: u64) -> u64 {
    const K: u64 = 0x9E37_79B9_7F4A_7C15;
    let mut l = [
        seed ^ K,
        seed.rotate_left(17) ^ 0xC2B2_AE3D_27D4_EB4F,
        seed.rotate_left(31) ^ 0x1656_67B1_9E37_79F9,
        seed.rotate_left(47) ^ 0x27D4_EB2F_1656_67C5,
    ];
    let chunks = b.chunks_exact(32);
    let rem = chunks.remainder();
    for c in chunks {
        for (i, lane) in l.iter_mut().enumerate() {
            let w = u64::from_le_bytes(c[i * 8..i * 8 + 8].try_into().unwrap());
            *lane = (*lane ^ w).wrapping_mul(K).rotate_left(31);
        }
    }
    let mut h = l[0] ^ l[1].rotate_left(7) ^ l[2].rotate_left(13) ^ l[3].rotate_left(19) ^ b.len() as u64;
    for &x in rem {
        h = (h ^ x as u64).wrapping_mul(K);
    }
    h ^= h >> 32;
    h = h.wrapping_mul(K);
    h ^ (h >> 29)
}

fn invalid(msg: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg)
}

fn write_all_at(f: &File, mut off: u64, mut buf: &[u8]) -> io::Result<u64> {
    let n = buf.len() as u64;
    while !buf.is_empty() {
        let w = f.seek_write(buf, off)?;
        if w == 0 {
            return Err(io::Error::new(io::ErrorKind::WriteZero, "catalog write"));
        }
        off += w as u64;
        buf = &buf[w..];
    }
    Ok(n)
}

/// Supplies MFT records for [`Catalog::apply`].
pub trait RecordSource {
    /// Copy record `rec` (update sequence already undone) into `buf`. `Ok(false)` if the
    /// record is not in use or lies beyond the end of the MFT.
    fn fetch(&mut self, rec: u64, buf: &mut [u8]) -> io::Result<bool>;
    /// Read a non-resident attribute (only needed for a non-resident `$ATTRIBUTE_LIST`).
    fn read_attribute(&mut self, extents: &[Extent], size: u64) -> io::Result<Vec<u8>>;
}

#[derive(Default, Debug, Clone, Copy)]
pub struct ApplyStats {
    /// Records re-read (files, their parent directories).
    pub records: u64,
    /// Extension records re-read for files with an attribute list.
    pub segments: u64,
    /// Speculative re-reads skipped because they would need the raw device.
    pub deferred: u64,
    /// Records whose stored [`Rec`] actually changed.
    pub modified: u64,
    pub created: u64,
    pub deleted: u64,
    pub names_removed: u64,
    pub names_added: u64,
    /// The name list was compacted (the next save rewrites the file).
    pub compacted: bool,
}

/// Placement of the four sections in the cache file: (offset, capacity in elements).
#[derive(Clone, Debug, PartialEq, Eq)]
struct FileLayout {
    path: PathBuf,
    sections: [(u64, u64); 4],
}

/// What changed since the catalog was last loaded or saved (runtime only).
#[derive(Default, Debug)]
struct Dirty {
    /// The next save must rewrite the whole file.
    full: bool,
    /// Record indices whose [`Rec`] changed.
    recs: Vec<u32>,
    /// Name entries retired in place.
    names: Vec<u32>,
    /// Entries / bytes from here on were appended.
    names_from: usize,
    arena_from: usize,
}

/// Result of [`Catalog::save`]; hand it back through [`Catalog::saved`].
#[derive(Debug)]
pub struct Saved {
    pub bytes_written: u64,
    pub full_rewrite: bool,
    layout: FileLayout,
}

pub struct Catalog {
    pub serial: u64,
    pub record_size: u32,
    pub cluster_size: u32,
    /// USN journal the catalog is current to (`journal_id == 0`: none).
    pub journal_id: u64,
    pub next_usn: i64,
    pub recs: Region<Rec>,
    /// Name entries, ordered by arena offset; retired ones have `record == DEAD`.
    pub names: Region<NameEntry>,
    pub arena: Region<u8>,
    /// Sorted record numbers of files with an open writer (see module docs).
    pub hot: Region<u32>,
    file: Option<FileLayout>,
    dirty: Dirty,
    /// The cache file mapped into memory, when the sections live in it (declared last:
    /// dropped after the regions that point into it).
    map: Option<Mapping>,
}

/// (offset, capacity in elements) of the four sections.
type Sections = [(u64, u64); 4];

/// A writable view of the whole cache file.
struct Mapping {
    _file: File,
    handle: HANDLE,
    view: *mut u8,
    len: usize,
}

// SAFETY: the view is owned by one catalog, used by one thread at a time.
unsafe impl Send for Mapping {}
// SAFETY: shared access never touches the view (only `&mut Catalog` writes the header).
unsafe impl Sync for Mapping {}

impl Drop for Mapping {
    fn drop(&mut self) {
        // SAFETY: view and handle created in `Catalog::open_mapped`, released once.
        unsafe {
            UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS { Value: self.view.cast() });
            CloseHandle(self.handle);
        }
    }
}

/// Fields of a validated header.
struct Meta {
    serial: u64,
    journal_id: u64,
    next_usn: i64,
    record_size: u32,
    cluster_size: u32,
    sections: [(u64, u64); 4],
    lens: [u64; 4],
}

fn parse_header(h: &[u8], file_len: u64) -> io::Result<Meta> {
    let u32at = |o: usize| u32::from_le_bytes(h[o..o + 4].try_into().unwrap());
    let u64at = |o: usize| u64::from_le_bytes(h[o..o + 8].try_into().unwrap());
    if h.len() < HEADER_LEN || h[0..8] != MAGIC || u32at(8) != VERSION || u32at(12) as usize != HEADER_LEN {
        return Err(invalid("not a FerrumTree catalog of this version"));
    }
    let mut sections = [(0u64, 0u64); 4];
    let mut lens = [0u64; 4];
    for i in 0..4 {
        let o = SECTIONS_AT + i * 24;
        let (off, cap, len) = (u64at(o), u64at(o + 8), u64at(o + 16));
        let end = cap.checked_mul(ELEM[i]).and_then(|b| b.checked_add(off));
        if len > cap || len > u32::MAX as u64 || off < HEADER_LEN as u64 || off % PAGE != 0 || end.is_none_or(|e| e > file_len) {
            return Err(invalid("catalog sections out of range"));
        }
        sections[i] = (off, cap);
        lens[i] = len;
    }
    Ok(Meta {
        serial: u64at(16),
        journal_id: u64at(24),
        next_usn: u64at(32) as i64,
        record_size: u32at(40),
        cluster_size: u32at(44),
        sections,
        lens,
    })
}

impl Catalog {
    pub fn new(
        serial: u64,
        record_size: u32,
        cluster_size: u32,
        journal: Option<(u64, i64)>,
        recs: Vec<Rec>,
        names: Vec<NameEntry>,
        arena: Vec<u8>,
    ) -> Catalog {
        let (journal_id, next_usn) = journal.unwrap_or((0, 0));
        Catalog {
            serial,
            record_size,
            cluster_size,
            journal_id,
            next_usn,
            recs: recs.into(),
            names: names.into(),
            arena: arena.into(),
            hot: Region::default(),
            file: None,
            dirty: Dirty { full: true, ..Default::default() },
            map: None,
        }
    }

    /// The sections live in a mapping of the cache file (see [`Catalog::open_mapped`]).
    pub fn is_mapped(&self) -> bool {
        self.map.is_some()
    }

    /// Release the mapped file's pages from the working set until they are next touched.
    /// They stay in the OS file cache (modified ones are written to the file), so the
    /// next access is a soft fault, not a disk read. No effect on a heap catalog.
    pub fn trim(&self) {
        if let Some(m) = &self.map {
            // SAFETY: the whole view is committed; on unlocked pages VirtualUnlock only
            // removes them from the working set (and reports ERROR_NOT_LOCKED).
            unsafe { VirtualUnlock(m.view.cast(), m.len) };
        }
    }

    pub fn from_scan(scan: Scan, journal: Option<(u64, i64)>) -> Catalog {
        let b = scan.boot;
        Catalog::new(b.serial, b.record_size, b.cluster_size, journal, scan.recs, scan.names, scan.arena)
    }

    /// Records to re-read on every update even without journal entries. Sorted, unique.
    ///
    /// * NTFS metadata files (records 0..16 and the `$Extend` tree: `$UsnJrnl`, `$ObjId`,
    ///   `$Reparse`, ...): NTFS never journals changes to its own metadata.
    /// * Hot files and their parent directories (NTFS refreshes a directory's index entry
    ///   and timestamp as an open file in it changes, also without logging).
    pub fn volatile(&self) -> Vec<u64> {
        const EXTEND: u32 = 11;
        let words = self.recs.len().div_ceil(64);
        let mut hot = vec![0u64; words];
        for &r in &self.hot {
            hot[(r >> 6) as usize] |= 1 << (r & 63);
        }
        let get = |m: &[u64], r: u32| m.get((r >> 6) as usize).is_some_and(|w| w >> (r & 63) & 1 != 0);
        let mut out: Vec<u64> = (0..16).chain(self.hot.iter().map(|&r| r as u64)).collect();
        // One pass: hot parents and $Extend children; a second one for the grandchildren
        // ($RmMetadata\$Txf, ...).
        let mut ext = vec![0u64; words];
        for e in &self.names {
            if get(&hot, e.record) {
                out.push(e.parent as u64);
            }
            if e.parent == EXTEND && e.record != EXTEND && e.record != DEAD {
                ext[(e.record >> 6) as usize] |= 1 << (e.record & 63);
                out.push(e.record as u64);
            }
        }
        for e in &self.names {
            if e.record != e.parent && get(&ext, e.parent) {
                out.push(e.record as u64);
            }
        }
        out.retain(|&r| (r as usize) < self.recs.len());
        out.sort_unstable();
        out.dedup();
        out
    }

    /// Hot set after a batch: old hot files plus files left open, minus files closed in
    /// the batch and records no longer in use.
    pub fn update_hot(&mut self, open: &[u64], closed: &[u64]) {
        let mut hot: Vec<u32> = self.hot.iter().copied().chain(open.iter().map(|&r| r as u32)).collect();
        hot.sort_unstable();
        hot.dedup();
        hot.retain(|&r| closed.binary_search(&(r as u64)).is_err() && self.recs.get(r as usize).is_some_and(|x| x.in_use()));
        self.hot.set(&hot);
    }

    /// Heap memory owned (mapped sections are file-backed and count as none).
    pub fn heap_bytes(&self) -> usize {
        self.recs.heap_bytes() + self.names.heap_bytes() + self.arena.heap_bytes() + self.hot.heap_bytes()
    }

    fn lens(&self) -> [u64; 4] {
        [self.recs.len() as u64, self.names.len() as u64, self.arena.len() as u64, self.hot.len() as u64]
    }

    fn parts(&self) -> [&[u8]; 4] {
        [bytes_of(&self.recs[..]), bytes_of(&self.names[..]), &self.arena[..], bytes_of(&self.hot[..])]
    }

    fn header(&self, sections: &[(u64, u64); 4]) -> Vec<u8> {
        let mut h = vec![0u8; HEADER_LEN];
        h[0..8].copy_from_slice(&MAGIC);
        h[8..12].copy_from_slice(&VERSION.to_le_bytes());
        h[12..16].copy_from_slice(&(HEADER_LEN as u32).to_le_bytes());
        h[16..24].copy_from_slice(&self.serial.to_le_bytes());
        h[24..32].copy_from_slice(&self.journal_id.to_le_bytes());
        h[32..40].copy_from_slice(&self.next_usn.to_le_bytes());
        h[40..44].copy_from_slice(&self.record_size.to_le_bytes());
        h[44..48].copy_from_slice(&self.cluster_size.to_le_bytes());
        let lens = self.lens();
        for i in 0..4 {
            let o = SECTIONS_AT + i * 24;
            h[o..o + 8].copy_from_slice(&sections[i].0.to_le_bytes());
            h[o + 8..o + 16].copy_from_slice(&sections[i].1.to_le_bytes());
            h[o + 16..o + 24].copy_from_slice(&lens[i].to_le_bytes());
        }
        let mut sum = hash(&h[..CHECKSUM_AT], 0);
        for p in self.parts() {
            sum = hash(p, sum);
        }
        h[CHECKSUM_AT..CHECKSUM_AT + 8].copy_from_slice(&sum.to_le_bytes());
        h
    }

    /// Persist to `path`: only the changed pages when `path` holds the previous state and
    /// every section still fits, otherwise a full rewrite through a temporary file.
    /// Takes `&self` so it can run next to readers; report the result with [`Self::saved`].
    pub fn save(&self, path: &Path) -> io::Result<Saved> {
        let lens = self.lens();
        let fits = |l: &FileLayout| l.path == path && (0..4).all(|i| lens[i] <= l.sections[i].1);
        match &self.file {
            Some(l) if !self.dirty.full && fits(l) => self.save_delta(l),
            _ => self.save_full(path),
        }
    }

    /// Record that `s` was written: later saves are relative to it.
    pub fn saved(&mut self, s: Saved) {
        self.file = Some(s.layout);
        self.dirty = Dirty { names_from: self.names.len(), arena_from: self.arena.len(), ..Default::default() };
    }

    fn save_full(&self, path: &Path) -> io::Result<Saved> {
        let (tmp, sections, written) = self.write_full(path)?;
        fs::rename(&tmp, path)?;
        Ok(Saved { bytes_written: written, full_rewrite: true, layout: FileLayout { path: path.to_path_buf(), sections } })
    }

    /// Write the whole catalog with fresh spare capacity to `path`'s temp sibling.
    fn write_full(&self, path: &Path) -> io::Result<(PathBuf, Sections, u64)> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        let lens = self.lens();
        let mut sections = [(0u64, 0u64); 4];
        let mut off = HEADER_LEN as u64;
        for i in 0..4 {
            let cap = lens[i] + lens[i] / 8 + SLACK[i];
            sections[i] = (off, cap);
            off = (off + cap * ELEM[i]).div_ceil(PAGE) * PAGE;
        }
        let tmp = path.with_extension("tmp");
        let mut written = 0;
        {
            let f = File::create(&tmp)?;
            f.set_len(off)?;
            for (i, p) in self.parts().into_iter().enumerate() {
                written += write_all_at(&f, sections[i].0, p)?;
            }
            written += write_all_at(&f, 0, &self.header(&sections))?;
        }
        Ok((tmp, sections, written))
    }

    fn save_delta(&self, l: &FileLayout) -> io::Result<Saved> {
        let f = OpenOptions::new().write(true).open(&l.path)?;
        let mut written = 0;
        // Whole 4 KiB pages around each touched element of a fixed-size section.
        let mut pages = |sec: usize, data: &[u8], idx: &mut dyn Iterator<Item = u64>| -> io::Result<()> {
            let per = PAGE / ELEM[sec];
            let mut last = u64::MAX;
            for i in idx {
                let p = i / per;
                if p == last {
                    continue;
                }
                last = p;
                let a = (p * PAGE) as usize;
                let b = ((p + 1) * PAGE).min(data.len() as u64) as usize;
                written += write_all_at(&f, l.sections[sec].0 + a as u64, &data[a..b])?;
            }
            Ok(())
        };
        let mut recs: Vec<u64> = self.dirty.recs.iter().map(|&r| r as u64).collect();
        recs.sort_unstable();
        pages(0, bytes_of(&self.recs[..]), &mut recs.into_iter())?;
        let mut names: Vec<u64> = self.dirty.names.iter().map(|&n| n as u64).collect();
        names.sort_unstable();
        names.extend(self.dirty.names_from as u64..self.names.len() as u64);
        pages(1, bytes_of(&self.names[..]), &mut names.into_iter())?;
        let from = self.dirty.arena_from.min(self.arena.len());
        written += write_all_at(&f, l.sections[2].0 + from as u64, &self.arena[from..])?;
        written += write_all_at(&f, l.sections[3].0, bytes_of(&self.hot[..]))?;
        // Header last: until it lands, the old header's checksum rejects the mixed state.
        written += write_all_at(&f, 0, &self.header(&l.sections))?;
        Ok(Saved { bytes_written: written, full_rewrite: false, layout: l.clone() })
    }

    pub fn load(path: &Path) -> io::Result<Catalog> {
        let mut f = File::open(path)?;
        let file_len = f.metadata()?.len();
        let mut h = vec![0u8; HEADER_LEN];
        f.read_exact(&mut h)?;
        let m = parse_header(&h, file_len)?;
        let (sections, lens) = (m.sections, m.lens);
        let read = |f: &mut File, i: usize, buf: &mut [u8]| -> io::Result<()> {
            f.seek(SeekFrom::Start(sections[i].0))?;
            f.read_exact(buf)
        };
        let cap = |i: usize| (sections[i].1 as usize).max(lens[i] as usize);
        let mut recs: Vec<Rec> = zeroed_vec(lens[0] as usize, cap(0))?;
        let mut names: Vec<NameEntry> = zeroed_vec(lens[1] as usize, cap(1))?;
        let mut arena: Vec<u8> = zeroed_vec(lens[2] as usize, cap(2))?;
        let mut hot: Vec<u32> = zeroed_vec(lens[3] as usize, cap(3))?;
        read(&mut f, 0, bytes_of_mut(&mut recs))?;
        read(&mut f, 1, bytes_of_mut(&mut names))?;
        read(&mut f, 2, &mut arena)?;
        read(&mut f, 3, bytes_of_mut(&mut hot))?;
        let mut cat = Catalog {
            serial: m.serial,
            journal_id: m.journal_id,
            next_usn: m.next_usn,
            record_size: m.record_size,
            cluster_size: m.cluster_size,
            recs: recs.into(),
            names: names.into(),
            arena: arena.into(),
            hot: hot.into(),
            file: None,
            dirty: Dirty::default(),
            map: None,
        };
        cat.verify(&h, &sections, &lens)?;
        cat.file = Some(FileLayout { path: path.to_path_buf(), sections });
        cat.dirty = Dirty { names_from: cat.names.len(), arena_from: cat.arena.len(), ..Default::default() };
        Ok(cat)
    }

    fn verify(&self, h: &[u8], sections: &[(u64, u64); 4], lens: &[u64; 4]) -> io::Result<()> {
        if self.header(sections)[CHECKSUM_AT..CHECKSUM_AT + 8] != h[CHECKSUM_AT..CHECKSUM_AT + 8] {
            return Err(invalid("catalog checksum mismatch"));
        }
        let mut end = 0u64;
        for e in self.names.iter() {
            let (o, l) = (e.off as u64, e.len as u64);
            if o < end || o + l > lens[2] || (e.record as u64 >= lens[0] && e.record != DEAD) {
                return Err(invalid("catalog name table inconsistent"));
            }
            end = o + l;
        }
        Ok(())
    }

    /// Open the cache file as a writable memory mapping: no copy into the heap, the
    /// sections are the file's pages, and updates are written to the file in place
    /// ([`Catalog::persist`] then only publishes a new header).
    pub fn open_mapped(path: &Path) -> io::Result<Catalog> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .share_mode(1 | 4 /* FILE_SHARE_READ | FILE_SHARE_DELETE: may be renamed while mapped */)
            .open(path)?;
        let file_len = file.metadata()?.len();
        if file_len < HEADER_LEN as u64 || file_len > isize::MAX as u64 {
            return Err(invalid("catalog file size"));
        }
        // SAFETY: mapping the whole file we just opened read/write; checked below.
        let map = unsafe {
            let handle = CreateFileMappingW(file.as_raw_handle(), std::ptr::null(), PAGE_READWRITE, 0, 0, std::ptr::null());
            if handle.is_null() {
                return Err(io::Error::last_os_error());
            }
            let view = MapViewOfFile(handle, FILE_MAP_READ | FILE_MAP_WRITE, 0, 0, 0).Value as *mut u8;
            if view.is_null() {
                let e = io::Error::last_os_error();
                CloseHandle(handle);
                return Err(e);
            }
            Mapping { _file: file, handle, view, len: file_len as usize }
        };
        // SAFETY: the view covers the whole file.
        let bytes = unsafe { std::slice::from_raw_parts(map.view, file_len as usize) };
        let m = parse_header(&bytes[..HEADER_LEN], file_len)?;
        let h = bytes[..HEADER_LEN].to_vec();
        let (sections, lens) = (m.sections, m.lens);
        // SAFETY: sections are page-aligned and inside the file (parse_header), and the
        // mapping lives in the same catalog, after the regions.
        let region = |i: usize| unsafe { map.view.add(sections[i].0 as usize) };
        let (recs, names, arena, hot) = unsafe {
            (
                Region::mapped(region(0).cast::<Rec>(), lens[0] as usize, sections[0].1 as usize),
                Region::mapped(region(1).cast::<NameEntry>(), lens[1] as usize, sections[1].1 as usize),
                Region::mapped(region(2), lens[2] as usize, sections[2].1 as usize),
                Region::mapped(region(3).cast::<u32>(), lens[3] as usize, sections[3].1 as usize),
            )
        };
        let cat = Catalog {
            serial: m.serial,
            journal_id: m.journal_id,
            next_usn: m.next_usn,
            record_size: m.record_size,
            cluster_size: m.cluster_size,
            recs,
            names,
            arena,
            hot,
            file: Some(FileLayout { path: path.to_path_buf(), sections }),
            dirty: Dirty::default(),
            map: Some(map),
        };
        cat.verify(&h, &sections, &lens)?;
        Ok(cat)
    }

    /// Make the current state durable at `path`, whatever the storage:
    /// * mapped, every section still inside the file: the data is already there, only a
    ///   new header (lengths, journal position, checksum) is written;
    /// * mapped, a section outgrew its reserved space: a fresh file is written and mapped;
    /// * heap: [`Catalog::save`].
    ///
    /// Returns the bytes written and whether the file was rewritten.
    pub fn persist(&mut self, path: &Path) -> io::Result<(u64, bool)> {
        let Some(map) = &self.map else {
            let s = self.save(path)?;
            let r = (s.bytes_written, s.full_rewrite);
            self.saved(s);
            return Ok(r);
        };
        let in_place = self.recs.is_mapped() && self.names.is_mapped() && self.arena.is_mapped() && self.hot.is_mapped();
        if let Some(l) = self.file.as_ref().filter(|l| in_place && l.path == path) {
            let h = self.header(&l.sections);
            // SAFETY: the header area (first HEADER_LEN bytes) is not part of any region.
            unsafe { std::ptr::copy_nonoverlapping(h.as_ptr(), map.view, HEADER_LEN) };
            return Ok((HEADER_LEN as u64, false));
        }
        let (tmp, _, written) = self.write_full(path)?;
        // Map the new file before letting go of the old state: any failure up to here
        // leaves this catalog untouched. The old view is unmapped when it is replaced.
        *self = Catalog::open_mapped(&tmp)?;
        fs::rename(&tmp, path)?;
        if let Some(l) = &mut self.file {
            l.path = path.to_path_buf();
        }
        Ok((written, true))
    }

    /// Re-read `changed` (sorted, unique record numbers) from `src` and update the
    /// catalog: their [`Rec`]s are replaced, their old names retired, current names added.
    ///
    /// Records also listed in `shallow` (sorted) are refreshed only from NTFS memory: if
    /// one needs a raw device read (a non-resident `$ATTRIBUTE_LIST`) it is left as it is.
    /// Used for records re-read speculatively rather than because the journal named them.
    pub fn apply(&mut self, changed: &[u64], shallow: &[u64], src: &mut dyn RecordSource) -> io::Result<ApplyStats> {
        self.apply_impl(changed, shallow, src, None)
    }

    /// [`Catalog::apply`], also reporting each record that changed - before and after,
    /// with its names - for patching a tree built from this catalog
    /// ([`ft_index::Index::patch`]). Records that came out identical are left out.
    pub fn apply_tracked(
        &mut self,
        changed: &[u64],
        shallow: &[u64],
        src: &mut dyn RecordSource,
        out: &mut Vec<Change>,
    ) -> io::Result<ApplyStats> {
        self.apply_impl(changed, shallow, src, Some(out))
    }

    fn apply_impl(
        &mut self,
        changed: &[u64],
        shallow: &[u64],
        src: &mut dyn RecordSource,
        out: Option<&mut Vec<Change>>,
    ) -> io::Result<ApplyStats> {
        let mut st = ApplyStats::default();
        let mut out = out;
        // (record, Rec before, range of its new names in `sink.names`)
        let mut spans: Vec<(u32, Rec, usize, usize)> = Vec::new();
        let changed = &changed[..changed.partition_point(|&r| r < DEAD as u64)];
        let Some(&max) = changed.last() else { return Ok(st) };
        if max as usize >= self.recs.len() {
            let grow = max as usize + 1 - self.recs.len();
            self.recs.reserve(grow + grow / 8 + 1024);
            self.recs.resize(max as usize + 1, Rec::default());
        }
        let rs = self.record_size as usize;
        let mut buf = vec![0u8; rs];
        let mut seg = vec![0u8; rs];
        let mut sink = Sink::new(self.cluster_size, changed.len());
        let mut mark = vec![0u64; self.recs.len().div_ceil(64)];

        for &r in changed {
            let was_in_use = self.recs[r as usize].in_use();
            let mut rec = Rec::default();
            let (names0, ext0) = (sink.names.len(), sink.ext.len());
            if src.fetch(r, &mut buf)? {
                parse_record_unprotected(&mut buf, r as u32, &mut rec, &mut sink);
                if rec.in_use() && rec.flags & rflags::HAS_ATTR_LIST != 0 {
                    let allow_raw = shallow.binary_search(&r).is_err();
                    let Some(segs) = extension_segments(&buf, r, src, allow_raw)? else {
                        sink.names.truncate(names0);
                        sink.ext.truncate(ext0);
                        st.deferred += 1;
                        continue;
                    };
                    for s in segs {
                        if src.fetch(s, &mut seg)? {
                            st.segments += 1;
                            let mut ignored = Rec::default();
                            parse_record_unprotected(&mut seg, s as u32, &mut ignored, &mut sink);
                        }
                    }
                }
            }
            mark[(r >> 6) as usize] |= 1 << (r & 63);
            st.records += 1;
            // Keep only what belongs to base record r (a stale reference may point at a
            // record that is now an extension of something else, or was reused).
            if rec.in_use() {
                for f in &sink.ext[ext0..] {
                    if f.base == r as u32 && f.base_seq == rec.seq {
                        rec.alloc = rec.alloc.saturating_add(f.alloc);
                        if f.has_size {
                            rec.size = f.size;
                        }
                        rec.flags |= f.flags;
                    }
                }
                let mut k = names0;
                for i in names0..sink.names.len() {
                    if sink.names[i].record == r as u32 {
                        sink.names[k] = sink.names[i];
                        k += 1;
                    }
                }
                sink.names.truncate(k);
            } else {
                sink.names.truncate(names0);
            }
            spans.push((r as u32, self.recs[r as usize], names0, sink.names.len()));
            sink.ext.truncate(ext0);
            if rec != self.recs[r as usize] {
                st.modified += 1;
                self.dirty.recs.push(r as u32);
            }
            match (was_in_use, rec.in_use()) {
                (false, true) => st.created += 1,
                (true, false) => st.deleted += 1,
                _ => {}
            }
            self.recs[r as usize] = rec;
        }

        // Old names of the re-read records, in one pass; retired entries counted on the way.
        let is_marked = |rec: u32| mark.get((rec >> 6) as usize).is_some_and(|w| w >> (rec & 63) & 1 != 0);
        let mut dead = 0usize;
        let mut old: Vec<(u32, u32)> = Vec::new();
        for (i, e) in self.names.iter().enumerate() {
            if e.record == DEAD {
                dead += 1;
            } else if is_marked(e.record) {
                old.push((e.record, i as u32));
            }
        }
        old.sort_unstable();
        let text = |arena: &[u8], off: u32, len: u16| -> Vec<u8> { arena[off as usize..off as usize + len as usize].to_vec() };
        for &(r, was, a, b) in &spans {
            let olds = &old[old.partition_point(|x| x.0 < r)..old.partition_point(|x| x.0 <= r)];
            let now = self.recs[r as usize];
            let same_names = olds.len() == b - a
                && olds.iter().zip(&sink.names[a..b]).all(|(&(_, i), n)| {
                    let o = self.names[i as usize];
                    o.parent == n.parent
                        && o.parent_seq == n.parent_seq
                        && self.arena[o.off as usize..o.off as usize + o.len as usize]
                            == sink.arena[n.off as usize..n.off as usize + n.len as usize]
                });
            // Re-read (speculatively or not) and identical: nothing is rewritten, so hot files
            // re-read on every batch do not churn the name table.
            if same_names && was == now {
                continue;
            }
            if let Some(out) = out.as_mut() {
                let old_names = olds
                    .iter()
                    .map(|&(_, i)| {
                        let e = self.names[i as usize];
                        Name { parent: e.parent, name: String::from_utf8_lossy(&text(&self.arena, e.off, e.len)).into(), valid: true }
                    })
                    .collect();
                let new_names = sink.names[a..b]
                    .iter()
                    .map(|e| {
                        let p = self.recs.get(e.parent as usize).copied().unwrap_or_default();
                        Name {
                            parent: e.parent,
                            name: String::from_utf8_lossy(&text(&sink.arena, e.off, e.len)).into(),
                            valid: p.in_use() && p.is_dir() && p.seq == e.parent_seq && e.parent != r,
                        }
                    })
                    .collect();
                let c = Change { record: r, old: was, new: now, old_names, new_names };
                if !c.is_noop() {
                    out.push(c);
                }
            }
            for &(_, i) in olds {
                self.names[i as usize].record = DEAD;
                dead += 1;
                st.names_removed += 1;
                self.dirty.names.push(i);
            }
            self.names.reserve(b - a);
            for e in &sink.names[a..b] {
                let off = self.arena.len();
                if off + e.len as usize > u32::MAX as usize {
                    return Err(io::Error::new(io::ErrorKind::OutOfMemory, "name arena exceeds 4 GiB"));
                }
                self.arena.extend_from_slice(&sink.arena[e.off as usize..e.off as usize + e.len as usize]);
                self.names.push(NameEntry { off: off as u32, ..*e });
                st.names_added += 1;
            }
        }
        if dead * 4 > self.names.len() {
            self.compact();
            st.compacted = true;
        }
        Ok(st)
    }

    /// Drop retired names and close the arena gaps in one in-place pass (offsets only move
    /// down because entries are ordered by offset).
    fn compact(&mut self) {
        let (mut wn, mut wa) = (0usize, 0usize);
        for i in 0..self.names.len() {
            let e = self.names[i];
            if e.record == DEAD {
                continue;
            }
            let (o, l) = (e.off as usize, e.len as usize);
            if o != wa {
                self.arena.copy_within(o..o + l, wa);
            }
            self.names[wn] = NameEntry { off: wa as u32, ..e };
            wn += 1;
            wa += l;
        }
        self.names.truncate(wn);
        self.arena.truncate(wa);
        self.dirty.full = true;
    }
}

/// Extension records listed in a base record's `$ATTRIBUTE_LIST`; `None` if the list is
/// non-resident and raw reads are not allowed.
fn extension_segments(base: &[u8], r: u64, src: &mut dyn RecordSource, allow_raw: bool) -> io::Result<Option<Vec<u64>>> {
    let attrs = record::attributes(base).map_err(|_| invalid("unreadable attribute list"))?;
    let Some(al) = attrs.iter().find(|a| a.ty == attr::ATTRIBUTE_LIST) else { return Ok(Some(Vec::new())) };
    let raw = match &al.value {
        AttrValue::Resident(v) => v.clone(),
        AttrValue::NonResident(_) if !allow_raw => return Ok(None),
        AttrValue::NonResident(nr) => src.read_attribute(&nr.extents, nr.data_size)?,
    };
    let mut segs: Vec<u64> = parse_attribute_list(&raw)
        .map_err(|_| invalid("unreadable attribute list"))?
        .into_iter()
        .map(|e| e.segment)
        .filter(|&s| s != r)
        .collect();
    segs.sort_unstable();
    segs.dedup();
    Ok(Some(segs))
}

/// Differences between two catalogs, ignoring records in `exclude` (sorted).
#[derive(Default, Debug)]
pub struct Diff {
    pub compared: u64,
    pub records: Vec<(u64, Rec, Rec)>,
    pub names: Vec<u64>,
}

pub fn diff(a: &Catalog, b: &Catalog, exclude: &[u64]) -> Diff {
    let excluded = |r: u64| exclude.binary_search(&r).is_ok();
    let mut d = Diff::default();
    for r in 0..a.recs.len().max(b.recs.len()) as u64 {
        if excluded(r) {
            continue;
        }
        let ra = a.recs.get(r as usize).copied().unwrap_or_default();
        let rb = b.recs.get(r as usize).copied().unwrap_or_default();
        if ra.in_use() || rb.in_use() {
            d.compared += 1;
        }
        if ra != rb {
            d.records.push((r, ra, rb));
        }
    }
    fn key(c: &Catalog) -> Vec<(u32, u32, u16, &[u8])> {
        let mut v: Vec<(u32, u32, u16, &[u8])> = c
            .names
            .iter()
            .filter(|e| e.record != DEAD)
            .map(|e| (e.record, e.parent, e.parent_seq, &c.arena[e.off as usize..e.off as usize + e.len as usize]))
            .collect();
        v.sort_unstable();
        v
    }
    let (ka, kb) = (key(a), key(b));
    let (mut i, mut j) = (0, 0);
    while i < ka.len() || j < kb.len() {
        let ra = ka.get(i).map_or(u32::MAX, |x| x.0);
        let rb = kb.get(j).map_or(u32::MAX, |x| x.0);
        let r = ra.min(rb);
        let ea = ka[i..].partition_point(|x| x.0 == r) + i;
        let eb = kb[j..].partition_point(|x| x.0 == r) + j;
        if ka[i..ea] != kb[j..eb] && !excluded(r as u64) {
            d.names.push(r as u64);
        }
        i = ea;
        j = eb;
    }
    d
}

#[cfg(test)]
mod tests {
    use super::*;
    use ft_ntfs::record::apply_fixup;
    use ft_ntfs::record::ns;
    use ft_ntfs::record::synth::RecordBuilder;
    use std::collections::HashMap;

    const RS: usize = 1024;

    /// In-memory "volume": records stored unprotected, as FSCTL_GET_NTFS_FILE_RECORD
    /// returns them.
    #[derive(Default)]
    struct Mem(HashMap<u64, Vec<u8>>);

    impl Mem {
        fn put(&mut self, r: u64, b: RecordBuilder) {
            let mut raw = b.build(0x55);
            assert!(apply_fixup(&mut raw));
            self.0.insert(r, raw);
        }
        fn scan(&self, n: usize) -> Catalog {
            let mut recs = vec![Rec::default(); n];
            let mut sink = Sink::new(4096, 0);
            let mut keys: Vec<_> = self.0.keys().copied().collect();
            keys.sort();
            for k in keys {
                let mut raw = self.0[&k].clone();
                parse_record_unprotected(&mut raw, k as u32, &mut recs[k as usize], &mut sink);
            }
            for f in &sink.ext {
                let r = &mut recs[f.base as usize];
                r.alloc += f.alloc;
                if f.has_size {
                    r.size = f.size;
                }
                r.flags |= f.flags;
            }
            Catalog::new(7, RS as u32, 4096, Some((1, 100)), recs, sink.names, sink.arena)
        }
    }

    impl RecordSource for Mem {
        fn fetch(&mut self, rec: u64, buf: &mut [u8]) -> io::Result<bool> {
            match self.0.get(&rec) {
                Some(r) if u16::from_le_bytes([r[0x16], r[0x17]]) & 1 != 0 => {
                    buf.copy_from_slice(r);
                    Ok(true)
                }
                _ => Ok(false),
            }
        }
        fn read_attribute(&mut self, _: &[Extent], _: u64) -> io::Result<Vec<u8>> {
            unimplemented!("raw device reads are not available in tests")
        }
    }

    fn file(parent: u32, name: &str, size: u64, seq: u16) -> RecordBuilder {
        RecordBuilder::new(RS).seq(seq).std_info(0, 0).file_name(parent, 1, name, ns::WIN32).resident(
            attr::DATA,
            "",
            &vec![0u8; size as usize],
        )
    }
    fn dir(parent: u32, name: &str) -> RecordBuilder {
        RecordBuilder::new(RS).seq(1).dir().std_info(0, 0x10).file_name(parent, 1, name, ns::WIN32)
    }

    fn base() -> Mem {
        let mut m = Mem::default();
        m.put(5, dir(5, "."));
        m.put(16, dir(5, "docs"));
        m.put(17, file(16, "a.txt", 10, 1));
        m.put(18, file(16, "b.txt", 20, 1));
        m
    }

    fn names_of(c: &Catalog, r: u32) -> Vec<String> {
        let mut v: Vec<String> = c
            .names
            .iter()
            .filter(|e| e.record == r)
            .map(|e| String::from_utf8_lossy(&c.arena[e.off as usize..(e.off + e.len as u32) as usize]).into_owned())
            .collect();
        v.sort();
        v
    }

    fn tmp(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("ft-usn-test-{}-{name}", std::process::id())).join("cat.ftc")
    }

    fn same(a: &Catalog, b: &Catalog) {
        assert_eq!(a.recs, b.recs);
        assert_eq!(a.names, b.names);
        assert_eq!(a.arena, b.arena);
        assert_eq!(a.hot, b.hot);
        assert_eq!((a.serial, a.journal_id, a.next_usn), (b.serial, b.journal_id, b.next_usn));
    }

    #[test]
    fn rename_delete_create_match_a_fresh_scan() {
        let mut m = base();
        let mut cat = m.scan(19);
        // rename a.txt, delete b.txt, create c.txt in a record past the old end
        m.put(17, file(16, "renamed.txt", 11, 1));
        m.put(18, file(16, "b.txt", 20, 1).unused());
        m.put(40, file(16, "c.txt", 30, 1));
        let st = cat.apply(&[16, 17, 18, 40], &[], &mut m).unwrap();
        assert_eq!((st.created, st.deleted), (1, 1));
        let fresh = m.scan(41);
        let d = diff(&cat, &fresh, &[]);
        assert!(d.records.is_empty() && d.names.is_empty(), "{d:?}");
        assert_eq!(names_of(&cat, 17), ["renamed.txt"]);
        assert!(names_of(&cat, 18).is_empty());
        assert!(cat.names.windows(2).all(|w| w[0].off + w[0].len as u32 <= w[1].off));
    }

    #[test]
    fn compaction_when_a_quarter_is_dead() {
        let mut m = base();
        let mut cat = m.scan(19);
        m.put(17, file(16, "a-renamed.txt", 10, 1));
        m.put(18, file(16, "b-renamed.txt", 20, 1));
        let st = cat.apply(&[16, 17, 18], &[], &mut m).unwrap();
        assert!(st.compacted, "3 of 4 names retired");
        assert!(cat.names.iter().all(|e| e.record != DEAD));
        let total: usize = cat.names.iter().map(|e| e.len as usize).sum();
        assert_eq!(total, cat.arena.len());
        let d = diff(&cat, &m.scan(19), &[]);
        assert!(d.records.is_empty() && d.names.is_empty(), "{d:?}");
    }

    #[test]
    fn reused_record_with_new_sequence() {
        let mut m = base();
        let mut cat = m.scan(19);
        m.put(17, file(5, "new-owner.bin", 99, 2));
        cat.apply(&[17], &[], &mut m).unwrap();
        assert_eq!(cat.recs[17].seq, 2);
        assert_eq!(cat.recs[17].size, 99);
        assert_eq!(names_of(&cat, 17), ["new-owner.bin"]);
    }

    #[test]
    fn attribute_list_extensions_are_followed() {
        let mut m = base();
        // record 20: base with a resident attribute list pointing at extension 30,
        // which holds the data stream and a second hard link.
        let mut al = vec![0u8; 0x20];
        al[0..4].copy_from_slice(&attr::DATA.to_le_bytes());
        al[4..6].copy_from_slice(&0x20u16.to_le_bytes());
        al[7] = 0x1A;
        al[0x10..0x18].copy_from_slice(&(30u64 | 3 << 48).to_le_bytes());
        m.put(
            20,
            RecordBuilder::new(RS).seq(3).std_info(0, 0).resident(attr::ATTRIBUTE_LIST, "", &al).file_name(16, 1, "big.iso", ns::WIN32),
        );
        let runs = [Extent { vcn: 0, len: 4, lcn: Some(900) }];
        m.put(
            30,
            RecordBuilder::new(RS).extension_of(20, 3).file_name(5, 1, "link.iso", ns::POSIX).non_resident(
                attr::DATA,
                "",
                0,
                0,
                4 * 4096,
                15000,
                None,
                &runs,
            ),
        );
        let mut cat = base().scan(31);
        let st = cat.apply(&[20], &[], &mut m).unwrap();
        assert_eq!(st.segments, 1);
        assert_eq!(cat.recs[20].size, 15000);
        assert_eq!(cat.recs[20].alloc, 4 * 4096);
        assert_eq!(names_of(&cat, 20), ["big.iso", "link.iso"]);
        let d = diff(&cat, &m.scan(31), &[]);
        assert!(d.records.is_empty() && d.names.is_empty(), "{d:?}");
    }

    #[test]
    fn shallow_refresh_never_touches_the_raw_device() {
        // Base record with a non-resident attribute list: reading it needs the raw device,
        // which Mem::read_attribute refuses. Shallow mode must leave the record alone.
        let list = [Extent { vcn: 0, len: 1, lcn: Some(777) }];
        let fragmented = |name: &str| {
            RecordBuilder::new(RS).std_info(0, 0).non_resident(attr::ATTRIBUTE_LIST, "", 0, 0, 4096, 64, None, &list).file_name(
                16,
                1,
                name,
                ns::WIN32,
            )
        };
        let mut m = base();
        m.put(20, fragmented("disk.vhdx"));
        let mut cat = m.scan(21);
        let (before, names_before) = (cat.recs[20], names_of(&cat, 20));
        m.put(20, fragmented("disk-renamed.vhdx")); // would show up if re-read
        m.put(17, file(16, "a2.txt", 10, 1));
        let st = cat.apply(&[17, 20], &[20], &mut m).unwrap();
        assert_eq!((st.records, st.deferred), (1, 1));
        assert_eq!(cat.recs[20], before);
        assert_eq!(names_of(&cat, 20), names_before);
        assert_eq!(names_of(&cat, 17), ["a2.txt"]);
    }

    #[test]
    fn volatile_covers_metadata_hot_files_and_their_dirs() {
        let mut m = base();
        m.put(11, dir(5, "$Extend"));
        m.put(24, dir(11, "$RmMetadata"));
        m.put(25, file(24, "$Txf", 0, 1));
        m.put(26, file(11, "$UsnJrnl", 0, 1));
        let mut cat = m.scan(27);
        cat.hot = vec![17].into();
        let v = cat.volatile();
        for r in [0, 9, 15, 16, 17, 24, 25, 26] {
            assert!(v.binary_search(&r).is_ok(), "{r} missing from {v:?}");
        }
        assert!(v.binary_search(&18).is_err());
    }

    #[test]
    fn delta_saves_write_little_and_reload_identically() {
        let p = tmp("delta");
        let mut m = base();
        // enough records that a delta is clearly smaller than the file
        for r in 100..3000u64 {
            m.put(r, file(16, &format!("f{r}.dat"), r % 50, 1));
        }
        let mut cat = m.scan(3000);
        let s = cat.save(&p).unwrap();
        assert!(s.full_rewrite);
        let full_bytes = s.bytes_written;
        cat.saved(s);

        let mut cat = Catalog::load(&p).unwrap();
        m.put(17, file(16, "a-edited.txt", 12, 1));
        m.put(40, file(16, "new.txt", 3, 1)); // inside the record capacity
        cat.apply(&[16, 17, 40], &[], &mut m).unwrap();
        cat.update_hot(&[17], &[]);
        cat.next_usn = 200;
        let s = cat.save(&p).unwrap();
        assert!(!s.full_rewrite);
        assert!(s.bytes_written * 4 < full_bytes, "delta {} vs full {full_bytes}", s.bytes_written);
        cat.saved(s);
        same(&Catalog::load(&p).unwrap(), &cat);

        // a second delta on top of the first
        m.put(18, file(16, "b-edited.txt", 21, 1));
        cat.apply(&[18], &[], &mut m).unwrap();
        let s = cat.save(&p).unwrap();
        assert!(!s.full_rewrite);
        cat.saved(s);
        let back = Catalog::load(&p).unwrap();
        same(&back, &cat);
        let d = diff(&back, &m.scan(3000), &[]);
        assert!(d.records.is_empty() && d.names.is_empty(), "{d:?}");
        fs::remove_dir_all(p.parent().unwrap()).unwrap();
    }

    /// Every live node by path: (size, alloc, files, dirs, mtime).
    fn tree_of(ix: &ft_index::Index) -> std::collections::BTreeMap<String, (u64, u64, u32, u32, u32)> {
        let mut out = std::collections::BTreeMap::new();
        let mut stack = vec![0u32];
        while let Some(n) = stack.pop() {
            let i = n as usize;
            out.insert(ix.path(n), (ix.size[i], ix.alloc[i], ix.files[i], ix.dirs[i], ix.mtime[i]));
            stack.extend(ix.children(n));
        }
        out
    }

    fn build(c: &Catalog) -> ft_index::Index {
        ft_index::Index::build(&c.recs[..], &c.names[..], &c.arena[..], "C:").unwrap().0
    }

    #[test]
    fn tracked_changes_patch_the_tree_like_a_rebuild() {
        let mut m = base();
        m.put(19, dir(16, "sub"));
        let mut cat = m.scan(40);
        let mut ix = build(&cat);
        let mut types = ft_index::types::FileTypes::build(&ix);

        // Move + rename, grow, a new folder with a file in it, an untouched re-read.
        m.put(17, file(19, "a-moved.txt", 15, 1));
        m.put(18, file(16, "b.txt", 300, 1));
        m.put(20, dir(19, "new"));
        m.put(21, file(20, "c.bin", 7, 1));
        let mut ch = Vec::new();
        cat.apply_tracked(&[16, 17, 18, 19, 20, 21], &[], &mut m, &mut ch).unwrap();
        assert_eq!(ch.iter().map(|c| c.record).collect::<Vec<_>>(), [17, 18, 20, 21], "unchanged records are left out");
        let st = ix.patch(&ch, &mut types).unwrap();
        assert_eq!((st.created, st.moved, st.renamed, st.unresolved), (2, 1, 1, 0));
        assert_eq!(tree_of(&ix), tree_of(&build(&cat)));

        // Delete the new folder and its file.
        m.0.remove(&20);
        m.0.remove(&21);
        let mut ch = Vec::new();
        cat.apply_tracked(&[20, 21], &[], &mut m, &mut ch).unwrap();
        assert_eq!(ix.patch(&ch, &mut types).unwrap().deleted, 2);
        assert_eq!(tree_of(&ix), tree_of(&build(&cat)));
        assert!(ix.children_all(ix.find(r"docs\sub").unwrap()).any(|n| ix.is_deleted(n)), "listed, struck through");
    }

    #[test]
    fn mapped_catalog_updates_in_place() {
        let p = tmp("mapped");
        let mut m = base();
        for r in 100..3000u64 {
            m.put(r, file(16, &format!("f{r}.dat"), r % 50, 1));
        }
        let cat = m.scan(3000);
        let s = cat.save(&p).unwrap();
        assert!(s.full_rewrite);
        drop(cat);

        let mut cat = Catalog::open_mapped(&p).unwrap();
        assert!(cat.is_mapped());
        assert_eq!(cat.heap_bytes(), 0, "sections live in the file, not the heap");
        m.put(17, file(16, "a-edited.txt", 12, 1));
        m.put(40, file(16, "new.txt", 3, 1));
        cat.apply(&[16, 17, 40], &[], &mut m).unwrap();
        cat.update_hot(&[17], &[]);
        cat.next_usn = 300;
        let (bytes, full) = cat.persist(&p).unwrap();
        assert!(!full);
        assert_eq!(bytes, HEADER_LEN as u64, "only the header is written");
        assert_eq!(cat.heap_bytes(), 0);
        drop(cat);

        let back = Catalog::load(&p).unwrap();
        assert_eq!(&back.hot[..], [17]);
        assert_eq!(back.next_usn, 300);
        let d = diff(&back, &m.scan(3000), &[]);
        assert!(d.records.is_empty() && d.names.is_empty(), "{d:?}");
        fs::remove_dir_all(p.parent().unwrap()).unwrap();
    }

    #[test]
    fn mapped_catalog_outgrowing_its_file_is_rewritten() {
        let p = tmp("grow");
        let m0 = base();
        m0.scan(19).save(&p).unwrap();
        let mut cat = Catalog::open_mapped(&p).unwrap();
        // A record far past the reserved capacity forces the records to the heap.
        let far = cat.recs.capacity() as u64 + 10;
        let mut m = base();
        m.put(far, file(16, "far.bin", 7, 1));
        cat.apply(&[far], &[], &mut m).unwrap();
        assert!(!cat.recs.is_mapped());
        let (_, full) = cat.persist(&p).unwrap();
        assert!(full);
        assert!(cat.is_mapped() && cat.recs.is_mapped(), "mapped again after the rewrite");
        assert_eq!(cat.recs[far as usize].size, 7);
        let d = diff(&Catalog::load(&p).unwrap(), &m.scan(far as usize + 1), &[]);
        assert!(d.records.is_empty() && d.names.is_empty(), "{d:?}");
        drop(cat);
        fs::remove_dir_all(p.parent().unwrap()).unwrap();
    }

    #[test]
    fn mapped_compaction_needs_no_rewrite() {
        let p = tmp("compact");
        let mut m = base();
        m.scan(19).save(&p).unwrap();
        let mut cat = Catalog::open_mapped(&p).unwrap();
        m.put(17, file(16, "a-renamed.txt", 10, 1));
        m.put(18, file(16, "b-renamed.txt", 20, 1));
        let st = cat.apply(&[16, 17, 18], &[], &mut m).unwrap();
        assert!(st.compacted);
        let (_, full) = cat.persist(&p).unwrap();
        assert!(!full, "compaction happened inside the mapped file");
        drop(cat);
        let d = diff(&Catalog::load(&p).unwrap(), &m.scan(19), &[]);
        assert!(d.records.is_empty() && d.names.is_empty(), "{d:?}");
        fs::remove_dir_all(p.parent().unwrap()).unwrap();
    }

    #[test]
    fn corruption_and_truncation_are_detected() {
        let p = tmp("corrupt");
        let mut cat = base().scan(19);
        cat.update_hot(&[17, 18], &[]);
        let s = cat.save(&p).unwrap();
        cat.saved(s);
        same(&Catalog::load(&p).unwrap(), &cat);
        let good = fs::read(&p).unwrap();
        // flip one byte inside the records section
        let mut raw = good.clone();
        raw[HEADER_LEN + 17 * 32 + 8] ^= 1;
        fs::write(&p, &raw).unwrap();
        assert!(Catalog::load(&p).is_err());
        // truncate into the sections (trailing spare capacity alone may be cut harmlessly)
        fs::write(&p, &good[..good.len() / 2]).unwrap();
        assert!(Catalog::load(&p).is_err());
        fs::remove_dir_all(p.parent().unwrap()).unwrap();
    }

    #[test]
    fn diff_respects_exclusions() {
        let m = base();
        let a = m.scan(19);
        let mut b = m.scan(19);
        b.recs[17].size = 1;
        assert_eq!(diff(&a, &b, &[]).records.len(), 1);
        assert!(diff(&a, &b, &[17]).records.is_empty());
    }
}
