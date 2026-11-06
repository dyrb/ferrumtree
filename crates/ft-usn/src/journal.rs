//! USN change journal: query and read (never create or modify it).

use std::fmt;
use std::io;

use ft_ntfs::volume::{ReadSlot, Volume};
use windows_sys::Win32::System::Ioctl::{FSCTL_QUERY_USN_JOURNAL, FSCTL_READ_USN_JOURNAL};

const ERROR_JOURNAL_DELETE_IN_PROGRESS: i32 = 1178;
const ERROR_JOURNAL_NOT_ACTIVE: i32 = 1179;
const ERROR_JOURNAL_ENTRY_DELETED: i32 = 1181;

/// Low 48 bits of a file reference = MFT record number.
pub const FRN_MASK: u64 = 0xFFFF_FFFF_FFFF;

/// The handle that caused the changes was closed. Until then NTFS reports each reason
/// only once per handle, so an open file can keep growing without new entries.
pub const USN_REASON_CLOSE: u32 = 0x8000_0000;

#[derive(Debug)]
pub enum JournalError {
    /// No journal on this volume (or it is being deleted). We never create one.
    NotActive,
    /// The requested position has been overwritten (journal wrapped or was truncated).
    EntriesDeleted,
    Io(io::Error),
}

impl fmt::Display for JournalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            JournalError::NotActive => write!(f, "USN journal not active"),
            JournalError::EntriesDeleted => write!(f, "USN journal position no longer available"),
            JournalError::Io(e) => write!(f, "USN journal I/O: {e}"),
        }
    }
}

impl std::error::Error for JournalError {}

impl From<io::Error> for JournalError {
    fn from(e: io::Error) -> Self {
        match e.raw_os_error() {
            Some(ERROR_JOURNAL_NOT_ACTIVE | ERROR_JOURNAL_DELETE_IN_PROGRESS) => JournalError::NotActive,
            Some(ERROR_JOURNAL_ENTRY_DELETED) => JournalError::EntriesDeleted,
            // Files and non-NTFS volumes reject the control code.
            Some(1 | 50 | 87) => JournalError::NotActive,
            _ => JournalError::Io(e),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct JournalInfo {
    pub id: u64,
    pub first_usn: i64,
    pub next_usn: i64,
    pub lowest_valid_usn: i64,
}

fn le_u64(b: &[u8], o: usize) -> u64 {
    u64::from_le_bytes(b[o..o + 8].try_into().unwrap())
}
fn le_u32(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}
fn le_u16(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes(b[o..o + 2].try_into().unwrap())
}

pub fn query(vol: &Volume, slot: &mut ReadSlot) -> Result<JournalInfo, JournalError> {
    let mut out = [0u8; 80]; // USN_JOURNAL_DATA_V0..V2 share the first 56 bytes
    let n = vol.ioctl(slot, FSCTL_QUERY_USN_JOURNAL, &[], &mut out)?;
    if n < 56 {
        return Err(JournalError::Io(io::Error::new(io::ErrorKind::InvalidData, "short USN_JOURNAL_DATA")));
    }
    Ok(JournalInfo {
        id: le_u64(&out, 0),
        first_usn: le_u64(&out, 8) as i64,
        next_usn: le_u64(&out, 16) as i64,
        lowest_valid_usn: le_u64(&out, 24) as i64,
    })
}

/// READ_USN_JOURNAL_DATA_V0 (40 bytes).
fn read_input(start: i64, journal_id: u64, wait_bytes: u64) -> [u8; 40] {
    let mut b = [0u8; 40];
    b[0..8].copy_from_slice(&start.to_le_bytes());
    b[8..12].copy_from_slice(&u32::MAX.to_le_bytes()); // every reason
    // ReturnOnlyOnClose = 0, Timeout = 0
    b[24..32].copy_from_slice(&wait_bytes.to_le_bytes());
    b[32..40].copy_from_slice(&journal_id.to_le_bytes());
    b
}

/// Record numbers touched since a journal position.
#[derive(Default, Debug, Clone)]
pub struct Changes {
    /// Sorted, unique MFT record numbers: every file named in an entry and its parent
    /// directory (whose index, and so on-disk size, changed too).
    pub records: Vec<u64>,
    /// Files whose latest entry is not a close: a handle is still open on them.
    pub open: Vec<u64>,
    /// Files whose latest entry is a close.
    pub closed: Vec<u64>,
    pub entries: u64,
    pub bytes: u64,
    /// Position to continue from next time.
    pub next_usn: i64,
}

/// Entries to drop: files in directory `dir` whose name starts with `prefix` (ASCII
/// case-insensitive) - FerrumTree's own cache files, whose writes would otherwise look like
/// changes to apply and save again.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ignore {
    pub dir: u64,
    pub prefix: Vec<u16>,
}

impl Ignore {
    fn matches(&self, parent: u64, name: &[u8]) -> bool {
        parent == self.dir
            && name.len() >= self.prefix.len() * 2
            && self.prefix.iter().enumerate().all(|(i, &p)| {
                let c = u16::from_le_bytes([name[2 * i], name[2 * i + 1]]);
                fold(c) == fold(p)
            })
    }
}

fn fold(c: u16) -> u16 {
    if (b'A' as u16..=b'Z' as u16).contains(&c) { c + 32 } else { c }
}

/// Collect FRNs (file and parent) into `out` and `(file, reason)` in journal order into
/// `last` from a buffer of USN_RECORD_V2/V3 entries, skipping entries matched by `ignore`.
/// Returns the number of entries kept.
pub fn parse_records(mut b: &[u8], out: &mut Vec<u64>, last: &mut Vec<(u64, u32)>, ignore: Option<&Ignore>) -> u64 {
    let mut entries = 0;
    while b.len() >= 8 {
        let len = le_u32(b, 0) as usize;
        if len < 8 || len > b.len() {
            break;
        }
        // (file, parent, reason, offset of the name length/offset pair)
        let (frn, parent, reason, at) = match le_u16(b, 4) {
            2 if len >= 0x3C => (le_u64(b, 8), le_u64(b, 16), le_u32(b, 0x28), 0x38),
            // FILE_ID_128: on NTFS the low 64 bits are the classic file reference.
            3 if len >= 0x4C => (le_u64(b, 8), le_u64(b, 24), le_u32(b, 0x38), 0x48),
            // V4 range records and unknown versions carry nothing we need.
            _ => {
                b = &b[len..];
                continue;
            }
        };
        let (frn, parent) = (frn & FRN_MASK, parent & FRN_MASK);
        let (nl, no) = (le_u16(b, at) as usize, le_u16(b, at + 2) as usize);
        let name = b.get(no..no + nl).unwrap_or(&[]);
        let skip = ignore.is_some_and(|ig| ig.matches(parent, name));
        b = &b[len..];
        if skip {
            continue;
        }
        out.push(frn);
        out.push(parent);
        last.push((frn, reason));
        entries += 1;
    }
    entries
}

/// Read every entry from `from` to the current end of the journal, without those matched
/// by `ignore`.
pub fn read_changes(
    vol: &Volume,
    slot: &mut ReadSlot,
    journal_id: u64,
    from: i64,
    buf: &mut [u8],
    ignore: Option<&Ignore>,
) -> Result<Changes, JournalError> {
    let mut ch = Changes { next_usn: from, ..Default::default() };
    let mut last: Vec<(u64, u32)> = Vec::new();
    loop {
        let n = vol.ioctl(slot, FSCTL_READ_USN_JOURNAL, &read_input(ch.next_usn, journal_id, 0), buf)? as usize;
        if n < 8 {
            break;
        }
        let next = le_u64(buf, 0) as i64;
        ch.entries += parse_records(&buf[8..n], &mut ch.records, &mut last, ignore);
        ch.bytes += (n - 8) as u64;
        let progressed = next != ch.next_usn;
        ch.next_usn = next;
        if n == 8 || !progressed {
            break;
        }
    }
    ch.records.sort_unstable();
    ch.records.dedup();
    // Stable sort keeps journal order inside each file: the group's last entry decides.
    last.sort_by_key(|e| e.0);
    for (i, &(frn, reason)) in last.iter().enumerate() {
        if last.get(i + 1).is_some_and(|n| n.0 == frn) {
            continue;
        }
        if reason & USN_REASON_CLOSE != 0 { ch.closed.push(frn) } else { ch.open.push(frn) }
    }
    Ok(ch)
}

/// Current time as a FILETIME (100 ns ticks since 1601).
pub fn filetime_now() -> u64 {
    let d = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
    (d.as_secs() + 11_644_473_600) * 10_000_000 + d.subsec_nanos() as u64 / 100
}

const PAGE: i64 = 4096;

/// Timestamp of the first entry on journal page `usn` (page-aligned: NTFS starts every
/// `$J` page with an entry and accepts page-aligned positions; unaligned ones fail).
fn page_time(vol: &Volume, slot: &mut ReadSlot, journal_id: u64, usn: i64, buf: &mut [u8]) -> Option<u64> {
    let n = vol.ioctl(slot, FSCTL_READ_USN_JOURNAL, &read_input(usn, journal_id, 0), buf).ok()? as usize;
    let e = buf.get(8..n)?;
    match le_u16(e, 4) {
        2 if e.len() >= 0x28 => Some(le_u64(e, 0x20)),
        3 if e.len() >= 0x38 => Some(le_u64(e, 0x30)),
        _ => None,
    }
}

/// A journal position at or before the first entry newer than `cutoff` (FILETIME), found
/// by binary search over pages. A full scan saves this instead of the current position:
/// NTFS writes MFT records to disk lazily, so a raw scan can read a record that is a few
/// seconds stale; replaying the last minutes through `FSCTL_GET_NTFS_FILE_RECORD` heals it.
pub fn position_before(vol: &Volume, slot: &mut ReadSlot, j: &JournalInfo, cutoff: u64) -> i64 {
    let floor = j.first_usn.max(j.lowest_valid_usn);
    if j.next_usn <= floor {
        return j.next_usn;
    }
    let mut buf = [0u8; 4096];
    let (mut lo, mut hi) = ((floor + PAGE - 1) / PAGE, (j.next_usn - 1) / PAGE);
    match page_time(vol, slot, j.id, lo * PAGE, &mut buf) {
        Some(t) if t <= cutoff => {}
        // Even the oldest page is newer than the cutoff (or unreadable): replay it all.
        _ => return floor,
    }
    // Invariant: page `lo` starts at or before the cutoff.
    while lo < hi {
        let mid = lo + (hi - lo + 1) / 2;
        match page_time(vol, slot, j.id, mid * PAGE, &mut buf) {
            Some(t) if t <= cutoff => lo = mid,
            _ => hi = mid - 1,
        }
    }
    lo * PAGE
}

/// Files that currently have an open writer, judged from the last `lookback` bytes of the
/// journal (their latest entry is not a close). Used to seed the hot set after a full scan:
/// such files keep changing without producing new entries until they are closed.
pub fn open_writers(vol: &Volume, slot: &mut ReadSlot, j: &JournalInfo, lookback: i64) -> Vec<u64> {
    let floor = j.first_usn.max(j.lowest_valid_usn);
    let want = j.next_usn - lookback;
    let start = if want <= floor { floor } else { (want + PAGE - 1) / PAGE * PAGE };
    let mut buf = vec![0u8; 1 << 20];
    read_changes(vol, slot, j.id, start, &mut buf, None).map(|c| c.open).unwrap_or_default()
}

/// Block until at least one new entry exists after `from` (no polling), or until the
/// event `cancel` (if not null) is signaled. Returns `false` when cancelled.
pub fn wait_for_entries(
    vol: &Volume,
    slot: &mut ReadSlot,
    journal_id: u64,
    from: i64,
    buf: &mut [u8],
    cancel: windows_sys::Win32::Foundation::HANDLE,
) -> Result<bool, JournalError> {
    Ok(vol.ioctl_unless(slot, FSCTL_READ_USN_JOURNAL, &read_input(from, journal_id, 1), buf, cancel)?.is_some())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v2(frn: u64, parent: u64, name: &str, reason: u32) -> Vec<u8> {
        let n: Vec<u8> = name.encode_utf16().flat_map(|u| u.to_le_bytes()).collect();
        let len = (0x3C + n.len()).div_ceil(8) * 8;
        let mut b = vec![0u8; len];
        b[0..4].copy_from_slice(&(len as u32).to_le_bytes());
        b[4..6].copy_from_slice(&2u16.to_le_bytes());
        b[8..16].copy_from_slice(&frn.to_le_bytes());
        b[16..24].copy_from_slice(&parent.to_le_bytes());
        b[0x28..0x2C].copy_from_slice(&reason.to_le_bytes());
        b[0x38..0x3A].copy_from_slice(&(n.len() as u16).to_le_bytes());
        b[0x3A..0x3C].copy_from_slice(&0x3Cu16.to_le_bytes());
        b[0x3C..0x3C + n.len()].copy_from_slice(&n);
        b
    }

    fn v3(frn: u64, parent: u64, reason: u32) -> Vec<u8> {
        let mut b = vec![0u8; 0x50];
        b[0..4].copy_from_slice(&0x50u32.to_le_bytes());
        b[4..6].copy_from_slice(&3u16.to_le_bytes());
        b[8..16].copy_from_slice(&frn.to_le_bytes());
        b[24..32].copy_from_slice(&parent.to_le_bytes());
        b[0x38..0x3C].copy_from_slice(&reason.to_le_bytes());
        b
    }

    #[test]
    fn parses_v2_and_v3_and_masks_sequence() {
        let mut buf = v2(0x0003_0000_0000_1234, 0x0001_0000_0000_0005, "a.txt", 0x2);
        buf.extend(v3(0x0009_0000_0000_0042, 0x0002_0000_0000_0010, USN_REASON_CLOSE | 0x2));
        let (mut out, mut last) = (Vec::new(), Vec::new());
        assert_eq!(parse_records(&buf, &mut out, &mut last, None), 2);
        assert_eq!(out, vec![0x1234, 5, 0x42, 0x10]);
        assert_eq!(last, vec![(0x1234, 0x2), (0x42, USN_REASON_CLOSE | 0x2)]);
    }

    #[test]
    fn stops_on_truncated_entry() {
        let mut buf = v2(7, 5, "x", 0);
        buf.extend_from_slice(&[0xFF, 0, 0, 0, 2, 0]); // claims 255 bytes
        let (mut out, mut last) = (Vec::new(), Vec::new());
        assert_eq!(parse_records(&buf, &mut out, &mut last, None), 1);
    }

    #[test]
    fn own_cache_files_are_dropped_by_name() {
        let ig = Ignore { dir: 30, prefix: "FerrumTree-0042".encode_utf16().collect() };
        let mut buf = v2(40, 30, "ferrumtree-0042.ftc", 0x2); // the cache, any case
        buf.extend(v2(44, 30, "FerrumTree-0042.tmp", 0x100)); // its temporary copy
        buf.extend(v2(45, 30, "notes.txt", 0x2)); // a neighbour in the same folder
        buf.extend(v2(41, 7, "FerrumTree-0042.ftc", 0x2)); // same name elsewhere
        let (mut out, mut last) = (Vec::new(), Vec::new());
        assert_eq!(parse_records(&buf, &mut out, &mut last, Some(&ig)), 2);
        assert_eq!(out, vec![45, 30, 41, 7]);
    }
}
