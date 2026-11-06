//! # ft-usn
//!
//! Keeps the MFT record catalog current without rescanning:
//!
//! ```text
//! cache file ──load──► Catalog (as of USN p)
//!                        │  FSCTL_READ_USN_JOURNAL from p  -> changed record numbers
//!                        │  FSCTL_GET_NTFS_FILE_RECORD each -> fresh Rec + names
//!                        ▼
//!                      Catalog (as of USN p') ──save (only if changed)──► cache file
//! ```
//!
//! A full scan happens only when there is no usable cache: no journal, another volume,
//! journal recreated or wrapped past `p`, or so many changes that a scan is cheaper.
//! The journal position of a full scan is taken *before* the scan starts, so changes made
//! during the scan are replayed next time (re-applying a record is idempotent).
//!
//! Nothing here writes to the scanned volume except the cache file itself, and the journal
//! is only queried and read, never created or modified.

pub mod catalog;
pub mod journal;
pub mod region;

use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use ft_ntfs::scan::{ScanOptions, ScanStats, read_nonresident, scan};
use ft_ntfs::volume::{ReadSlot, Volume};
use ft_ntfs::{Error, Extent, Result};
use windows_sys::Win32::System::Ioctl::FSCTL_GET_NTFS_FILE_RECORD;

pub use catalog::{ApplyStats, Catalog, Diff, RecordSource, diff};
pub use journal::{Changes, JournalError, JournalInfo};

/// Re-reads records through NTFS (its in-memory, always current copy), not from the raw
/// device, which may lag behind by a few seconds of lazy writing.
pub struct VolumeSource<'a> {
    vol: &'a Volume,
    slot: ReadSlot,
    out: Vec<u8>,
}

impl<'a> VolumeSource<'a> {
    pub fn new(vol: &'a Volume) -> io::Result<Self> {
        Ok(VolumeSource { vol, slot: ReadSlot::new()?, out: vec![0u8; 16 + vol.boot.record_size as usize] })
    }
}

impl RecordSource for VolumeSource<'_> {
    fn fetch(&mut self, rec: u64, buf: &mut [u8]) -> io::Result<bool> {
        let input = rec.to_le_bytes();
        let n = match self.vol.ioctl(&mut self.slot, FSCTL_GET_NTFS_FILE_RECORD, &input, &mut self.out) {
            Ok(n) => n as usize,
            // Past the end of the MFT.
            Err(e) if e.raw_os_error() == Some(87) => return Ok(false),
            Err(e) => return Err(e),
        };
        if n < 12 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "short NTFS_FILE_RECORD_OUTPUT_BUFFER"));
        }
        // For a free record NTFS returns the nearest lower in-use one instead.
        let got = u64::from_le_bytes(self.out[0..8].try_into().unwrap()) & journal::FRN_MASK;
        if got != rec {
            return Ok(false);
        }
        let len = u32::from_le_bytes(self.out[8..12].try_into().unwrap()) as usize;
        if len != buf.len() || 12 + len > n {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "unexpected file record length"));
        }
        buf.copy_from_slice(&self.out[12..12 + len]);
        Ok(true)
    }

    fn read_attribute(&mut self, extents: &[Extent], size: u64) -> io::Result<Vec<u8>> {
        read_nonresident(self.vol, extents, size).map_err(|e| match e {
            Error::Io(e) => e,
            other => io::Error::new(io::ErrorKind::InvalidData, other.to_string()),
        })
    }
}

/// Union of two sorted, unique lists.
pub fn merge_sorted(a: &[u64], b: &[u64]) -> Vec<u64> {
    let mut out = Vec::with_capacity(a.len() + b.len());
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        let (x, y) = (a[i], b[j]);
        out.push(x.min(y));
        i += (x <= y) as usize;
        j += (y <= x) as usize;
    }
    out.extend_from_slice(&a[i..]);
    out.extend_from_slice(&b[j..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sorted_set_ops() {
        assert_eq!(merge_sorted(&[1, 4, 9], &[2, 4, 10]), [1, 2, 4, 9, 10]);
        assert_eq!(merge_sorted(&[], &[3]), [3]);
        assert_eq!(minus_sorted(&[1, 2, 4, 9, 10], &[2, 9, 11]), [1, 4, 10]);
        assert_eq!(minus_sorted(&[5], &[]), [5]);
    }
}

/// Elements of sorted `a` that are not in sorted `b`.
pub fn minus_sorted(a: &[u64], b: &[u64]) -> Vec<u64> {
    let mut j = 0;
    a.iter()
        .copied()
        .filter(|&x| {
            while j < b.len() && b[j] < x {
                j += 1;
            }
            j >= b.len() || b[j] != x
        })
        .collect()
}

/// NTFS file reference (record number) of an existing file or directory.
pub fn file_id(path: &Path) -> Option<u64> {
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{BY_HANDLE_FILE_INFORMATION, FILE_FLAG_BACKUP_SEMANTICS, GetFileInformationByHandle};
    // No access rights needed for the file index; backup semantics allow directories.
    let f = std::fs::OpenOptions::new().access_mode(0).share_mode(7).custom_flags(FILE_FLAG_BACKUP_SEMANTICS).open(path).ok()?;
    // SAFETY: valid handle and out-parameter.
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    if unsafe { GetFileInformationByHandle(f.as_raw_handle(), &mut info) } == 0 {
        return None;
    }
    Some(((info.nFileIndexHigh as u64) << 32 | info.nFileIndexLow as u64) & journal::FRN_MASK)
}

/// Journal filter for the cache file at `cache` and its temporary copy: their entries are
/// our own writes (otherwise every save would look like a change to apply and save).
pub fn cache_ignore(cache: &Path) -> Option<journal::Ignore> {
    let dir = cache.parent().and_then(file_id)?;
    let prefix = cache.file_stem()?.to_string_lossy().encode_utf16().collect();
    Some(journal::Ignore { dir, prefix })
}

/// Name prefix of every cache file.
pub const CACHE_PREFIX: &str = "FerrumTree-";

/// The folder of the running program: a portable program keeps its files next to itself.
pub fn program_dir() -> Option<PathBuf> {
    std::env::current_exe().ok()?.parent().map(Path::to_path_buf)
}

/// `<program folder>\FerrumTree-<volume serial>.ftc`
pub fn default_cache_path(serial: u64) -> Option<PathBuf> {
    Some(program_dir()?.join(format!("{CACHE_PREFIX}{serial:016X}.ftc")))
}

/// Delete every cache file next to the program. Returns the bytes freed.
pub fn remove_caches() -> u64 {
    let Some(dir) = program_dir() else { return 0 };
    let Ok(rd) = std::fs::read_dir(dir) else { return 0 };
    let mut freed = 0;
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().to_ascii_lowercase();
        let ours = name.starts_with(&CACHE_PREFIX.to_ascii_lowercase()) && (name.ends_with(".ftc") || name.ends_with(".tmp"));
        if ours {
            let len = e.metadata().map_or(0, |m| m.len());
            if std::fs::remove_file(e.path()).is_ok() {
                freed += len;
            }
        }
    }
    freed
}

#[derive(Clone, Debug, Default)]
pub struct OpenOptions {
    pub scan: ScanOptions,
    /// Catalog cache file; `None` disables caching (always a full scan, nothing written).
    pub cache: Option<PathBuf>,
    /// Ignore the cache, scan, and rewrite the cache.
    pub rescan: bool,
    /// Keep a cached catalog as a writable mapping of the cache file
    /// ([`Catalog::open_mapped`]) instead of reading it into the heap. For long-lived
    /// catalogs: their bulk then stays in file-backed pages; save with [`Catalog::persist`].
    pub mapped: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Source {
    /// Full MFT scan, with the reason the cache could not be used.
    Full(&'static str),
    Incremental,
}

#[derive(Debug)]
pub struct OpenReport {
    pub source: Source,
    pub scan: Option<ScanStats>,
    pub journal: Option<JournalInfo>,
    pub changes: Changes,
    pub apply: ApplyStats,
    pub t_load: Duration,
    pub t_journal: Duration,
    pub t_apply: Duration,
    /// The catalog changed and should be written here ([`Catalog::save`]). Left to the
    /// caller so it can overlap the write with building the tree.
    pub save_to: Option<PathBuf>,
}

/// More changed records than this fraction of the MFT: a full scan is cheaper than
/// one IOCTL per record.
const MAX_CHANGED_DIVISOR: usize = 4;

/// How far back a full scan's saved journal position reaches (see
/// [`journal::position_before`]).
const REPLAY_WINDOW_SECS: u64 = 120;

/// Journal tail read after a full scan to find files with an open writer.
const HOT_LOOKBACK_BYTES: i64 = 64 << 20;

fn try_incremental(
    vol: &Volume,
    path: &Path,
    j: &JournalInfo,
    slot: &mut ReadSlot,
    rep: &mut OpenReport,
    mapped: bool,
) -> std::result::Result<(Catalog, bool), &'static str> {
    let t = Instant::now();
    let loaded = if mapped { Catalog::open_mapped(path) } else { Catalog::load(path) };
    let mut cat = match loaded {
        Ok(c) => c,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Err("no cache yet"),
        Err(_) => return Err("cache unreadable or corrupt"),
    };
    rep.t_load = t.elapsed();
    let b = &vol.boot;
    if cat.serial != b.serial || cat.record_size != b.record_size || cat.cluster_size != b.cluster_size {
        return Err("cache belongs to another volume");
    }
    if cat.journal_id != j.id {
        return Err("USN journal was recreated");
    }
    if cat.next_usn < j.lowest_valid_usn.max(j.first_usn) {
        return Err("USN journal wrapped since the last run");
    }
    if cat.next_usn > j.next_usn {
        return Err("cache is ahead of the USN journal");
    }
    let from = cat.next_usn;
    let t = Instant::now();
    let mut buf = vec![0u8; 1 << 20];
    let ignore = cache_ignore(path);
    rep.changes = match journal::read_changes(vol, slot, j.id, cat.next_usn, &mut buf, ignore.as_ref()) {
        Ok(c) => c,
        Err(JournalError::EntriesDeleted) => return Err("USN journal wrapped since the last run"),
        Err(_) => return Err("USN journal unreadable"),
    };
    rep.t_journal = t.elapsed();
    // Journal changes, plus records that change without journal entries (re-read from
    // NTFS memory only, see `Catalog::apply`).
    let speculative = minus_sorted(&cat.volatile(), &rep.changes.records);
    let todo = merge_sorted(&rep.changes.records, &speculative);
    if todo.len() > cat.recs.len() / MAX_CHANGED_DIVISOR + 1024 {
        return Err("too many changes since the last run");
    }
    let t = Instant::now();
    let mut src = VolumeSource::new(vol).map_err(|_| "cannot create event")?;
    rep.apply = cat.apply(&todo, &speculative, &mut src).map_err(|_| "re-reading changed records failed")?;
    cat.next_usn = rep.changes.next_usn;
    let hot_before = cat.hot.len();
    cat.update_hot(&rep.changes.open, &rep.changes.closed);
    rep.t_apply = t.elapsed();
    let dirty = cat.next_usn != from || rep.apply.modified != 0 || cat.hot.len() != hot_before;
    Ok((cat, dirty))
}

/// Current catalog for `vol`: from the cache plus the journal when possible, otherwise by
/// a full scan.
pub fn open(vol: &Volume, o: &OpenOptions) -> Result<(Catalog, OpenReport)> {
    let mut slot = ReadSlot::new()?;
    let journal = match journal::query(vol, &mut slot) {
        Ok(j) => Some(j),
        Err(JournalError::Io(e)) => return Err(Error::Io(e)),
        Err(_) => None,
    };
    let mut rep = OpenReport {
        source: Source::Full(""),
        scan: None,
        journal,
        changes: Changes::default(),
        apply: ApplyStats::default(),
        t_load: Duration::ZERO,
        t_journal: Duration::ZERO,
        t_apply: Duration::ZERO,
        save_to: None,
    };
    // Without a journal a cache could never be validated, so it is not used at all.
    let cache = o.cache.as_deref().filter(|_| journal.is_some());

    let reason = match (journal.as_ref(), cache) {
        (None, _) => "no USN journal",
        (_, None) => "cache disabled",
        _ if o.rescan => "rescan requested",
        (Some(j), Some(path)) => match try_incremental(vol, path, j, &mut slot, &mut rep, o.mapped) {
            Ok((cat, dirty)) => {
                rep.source = Source::Incremental;
                rep.save_to = dirty.then(|| path.to_path_buf());
                return Ok((cat, rep));
            }
            Err(why) => why,
        },
    };

    // Position first, then scan: anything that changes during the scan - or shortly before
    // it, while NTFS may not have written the record to disk yet - is replayed next time.
    let cutoff = journal::filetime_now().saturating_sub(REPLAY_WINDOW_SECS * 10_000_000);
    let pos = journal.map(|j| (j.id, journal::position_before(vol, &mut slot, &j, cutoff)));
    let writers = journal.map(|j| journal::open_writers(vol, &mut slot, &j, HOT_LOOKBACK_BYTES)).unwrap_or_default();
    let sc = scan(vol, &o.scan)?;
    rep.scan = Some(sc.stats.clone());
    let mut cat = Catalog::from_scan(sc, pos);
    cat.update_hot(&writers, &[]);
    rep.source = Source::Full(reason);
    rep.save_to = cache.map(Path::to_path_buf);
    Ok((cat, rep))
}
