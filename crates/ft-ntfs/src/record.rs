//! `FILE` record (FILE_RECORD_SEGMENT_HEADER) parsing.
//!
//! Hot path: [`parse_record`] runs once per MFT record on the parser threads. It applies
//! the Update Sequence Array fixup in place, walks the attributes once and writes a
//! fixed-size [`Rec`] plus zero or more [`NameEntry`]s. No per-record heap allocation:
//! names go into a per-thread byte arena.
//!
//! Cold path: [`attributes`] decodes every attribute of one record into owned values; it
//! is used for `$MFT` itself (record 0) and for diagnostics.

use crate::bytes::*;
use crate::error::{Error, Result};
use crate::runlist::{self, Extent};

/// Attribute type codes.
pub mod attr {
    pub const STANDARD_INFORMATION: u32 = 0x10;
    pub const ATTRIBUTE_LIST: u32 = 0x20;
    pub const FILE_NAME: u32 = 0x30;
    pub const DATA: u32 = 0x80;
    pub const INDEX_ROOT: u32 = 0x90;
    pub const INDEX_ALLOCATION: u32 = 0xA0;
    pub const BITMAP: u32 = 0xB0;
    pub const REPARSE_POINT: u32 = 0xC0;
    pub const END: u32 = 0xFFFF_FFFF;
}

/// Flags stored in [`Rec::flags`].
pub mod rflags {
    pub const IN_USE: u16 = 1 << 0;
    pub const DIR: u16 = 1 << 1;
    /// Has at least one named `$DATA` stream (ADS).
    pub const HAS_ADS: u16 = 1 << 2;
    pub const COMPRESSED: u16 = 1 << 3;
    pub const SPARSE: u16 = 1 << 4;
    pub const ENCRYPTED: u16 = 1 << 5;
    pub const REPARSE: u16 = 1 << 6;
    /// Unnamed `$DATA` is resident (stored inside the MFT record itself).
    pub const RESIDENT: u16 = 1 << 7;
    pub const HAS_ATTR_LIST: u16 = 1 << 8;
}

const REC_IN_USE: u16 = 0x0001;
const REC_DIRECTORY: u16 = 0x0002;

const AF_COMPRESSED: u16 = 0x0001;
const AF_ENCRYPTED: u16 = 0x4000;
const AF_SPARSE: u16 = 0x8000;

const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
const FILE_ATTRIBUTE_ENCRYPTED: u32 = 0x4000;

/// `$FILE_NAME` namespaces.
pub mod ns {
    pub const POSIX: u8 = 0;
    pub const WIN32: u8 = 1;
    pub const DOS: u8 = 2;
    pub const WIN32_AND_DOS: u8 = 3;
}

/// Per-record result, indexed by MFT record number. 32 bytes.
#[repr(C)]
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct Rec {
    /// Logical size of the unnamed `$DATA` stream.
    pub size: u64,
    /// Bytes actually allocated on disk by all non-resident attributes (all streams,
    /// directory indexes, ...). Compressed/sparse attributes count real clusters only.
    pub alloc: u64,
    /// Last write time (FILETIME) from `$STANDARD_INFORMATION`.
    pub mtime: u64,
    /// Win32 file attributes from `$STANDARD_INFORMATION`.
    pub attrs: u32,
    /// Record sequence number (detects reused records in parent references).
    pub seq: u16,
    pub flags: u16,
}

impl Rec {
    #[inline]
    pub fn in_use(&self) -> bool {
        self.flags & rflags::IN_USE != 0
    }
    #[inline]
    pub fn is_dir(&self) -> bool {
        self.flags & rflags::DIR != 0
    }
}

/// One `$FILE_NAME` (one hard link). DOS 8.3 aliases are dropped during parsing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NameEntry {
    /// Base record that owns this name.
    pub record: u32,
    /// Parent directory record.
    pub parent: u32,
    /// Offset of the UTF-8 name in the arena.
    pub off: u32,
    /// Length of the UTF-8 name in bytes (<= 765).
    pub len: u16,
    pub parent_seq: u16,
}

/// Attribute data found in an extension record, to be merged into its base record.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExtFrag {
    pub base: u32,
    pub base_seq: u16,
    pub flags: u16,
    pub has_size: bool,
    pub size: u64,
    pub alloc: u64,
}

#[derive(Default, Clone, Copy, Debug)]
pub struct ParseStats {
    pub records: u64,
    pub in_use: u64,
    pub base: u64,
    pub extension: u64,
    pub corrupt: u64,
    pub names: u64,
    pub dos_names_skipped: u64,
}

impl ParseStats {
    pub fn add(&mut self, o: &ParseStats) {
        self.records += o.records;
        self.in_use += o.in_use;
        self.base += o.base;
        self.extension += o.extension;
        self.corrupt += o.corrupt;
        self.names += o.names;
        self.dos_names_skipped += o.dos_names_skipped;
    }
}

/// Per-thread output buffers.
pub struct Sink {
    pub names: Vec<NameEntry>,
    pub arena: Vec<u8>,
    pub ext: Vec<ExtFrag>,
    pub stats: ParseStats,
    /// Volume cluster size; on-disk bytes = allocated clusters × this.
    pub cluster_size: u64,
}

impl Sink {
    pub fn new(cluster_size: u32, names: usize) -> Self {
        Sink {
            names: Vec::with_capacity(names),
            arena: Vec::with_capacity(names * 16),
            ext: Vec::new(),
            stats: ParseStats::default(),
            cluster_size: cluster_size as u64,
        }
    }

    pub fn name(&self, e: &NameEntry) -> &str {
        // The arena only ever receives valid UTF-8.
        std::str::from_utf8(&self.arena[e.off as usize..e.off as usize + e.len as usize]).unwrap_or("")
    }
}

/// Apply the Update Sequence Array fixup in place. Returns `false` for a torn or
/// malformed record. The USA always protects 512-byte strides, independent of the
/// physical sector size.
#[inline]
pub fn apply_fixup(rec: &mut [u8]) -> bool {
    let usa_off = u16le(rec, 4) as usize;
    let usa_cnt = u16le(rec, 6) as usize;
    if usa_cnt < 2 || usa_off + usa_cnt * 2 > rec.len() || (usa_cnt - 1) * 512 > rec.len() {
        return false;
    }
    let usn = [rec[usa_off], rec[usa_off + 1]];
    for i in 1..usa_cnt {
        let end = i * 512 - 2;
        if rec[end] != usn[0] || rec[end + 1] != usn[1] {
            return false;
        }
        rec[end] = rec[usa_off + 2 * i];
        rec[end + 1] = rec[usa_off + 2 * i + 1];
    }
    true
}

#[derive(Default)]
struct Acc {
    size: u64,
    has_size: bool,
    alloc: u64,
    mtime: u64,
    attrs: u32,
    flags: u16,
}

/// Parse one raw record (as read from disk) in place.
///
/// * Unused, zeroed or foreign records leave `out` untouched (all zero).
/// * Base records fill `out`.
/// * Extension records push an [`ExtFrag`] and their names (attributed to the base).
pub fn parse_record(rec: &mut [u8], rec_no: u32, out: &mut Rec, sink: &mut Sink) {
    parse_impl::<true>(rec, rec_no, out, sink)
}

/// Same as [`parse_record`] for a record whose update sequence has already been undone,
/// as returned by `FSCTL_GET_NTFS_FILE_RECORD` (NTFS's in-memory copy).
pub fn parse_record_unprotected(rec: &mut [u8], rec_no: u32, out: &mut Rec, sink: &mut Sink) {
    parse_impl::<false>(rec, rec_no, out, sink)
}

#[inline(always)]
fn parse_impl<const FIXUP: bool>(rec: &mut [u8], rec_no: u32, out: &mut Rec, sink: &mut Sink) {
    sink.stats.records += 1;
    if rec.len() < 0x30 {
        return;
    }
    match &rec[0..4] {
        b"FILE" => {}
        b"BAAD" => {
            sink.stats.corrupt += 1;
            return;
        }
        _ => return,
    }
    let hflags = u16le(rec, 0x16);
    if hflags & REC_IN_USE == 0 {
        return;
    }
    if FIXUP && !apply_fixup(rec) {
        sink.stats.corrupt += 1;
        return;
    }
    sink.stats.in_use += 1;
    let seq = u16le(rec, 0x10);
    let base_ref = u64le(rec, 0x20);
    let base = base_ref & 0xFFFF_FFFF_FFFF;
    let target = if base == 0 {
        rec_no
    } else if base <= u32::MAX as u64 {
        base as u32
    } else {
        sink.stats.corrupt += 1;
        return;
    };

    let mut acc = Acc::default();
    if walk_attrs(rec, target, &mut acc, sink).is_err() {
        sink.stats.corrupt += 1;
    }

    if base == 0 {
        sink.stats.base += 1;
        let mut flags = rflags::IN_USE | acc.flags;
        if hflags & REC_DIRECTORY != 0 {
            flags |= rflags::DIR;
        }
        *out = Rec { size: acc.size, alloc: acc.alloc, mtime: acc.mtime, attrs: acc.attrs, seq, flags };
    } else {
        sink.stats.extension += 1;
        if acc.alloc != 0 || acc.has_size || acc.flags != 0 {
            sink.ext.push(ExtFrag {
                base: target,
                base_seq: (base_ref >> 48) as u16,
                flags: acc.flags,
                has_size: acc.has_size,
                size: acc.size,
                alloc: acc.alloc,
            });
        }
    }
}

#[inline]
fn walk_attrs(rec: &[u8], target: u32, acc: &mut Acc, sink: &mut Sink) -> std::result::Result<(), ()> {
    let used = (u32le(rec, 0x18) as usize).min(rec.len());
    let mut off = u16le(rec, 0x14) as usize;
    loop {
        if off + 4 > used {
            return Err(());
        }
        let ty = u32le(rec, off);
        if ty == attr::END {
            return Ok(());
        }
        if off + 8 > used {
            return Err(());
        }
        let len = u32le(rec, off + 4) as usize;
        if len < 0x18 || off + len > used {
            return Err(());
        }
        let a = &rec[off..off + len];
        off += len;

        let non_res = a[8] != 0;
        let named = a[9] != 0;
        let aflags = u16le(a, 0x0C);

        if !non_res {
            let vlen = u32le(a, 0x10) as usize;
            let voff = u16le(a, 0x14) as usize;
            if voff + vlen > len {
                return Err(());
            }
            let v = &a[voff..voff + vlen];
            match ty {
                attr::STANDARD_INFORMATION if vlen >= 0x24 => {
                    acc.mtime = u64le(v, 0x08);
                    acc.attrs = u32le(v, 0x20);
                    if acc.attrs & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
                        acc.flags |= rflags::REPARSE;
                    }
                    if acc.attrs & FILE_ATTRIBUTE_ENCRYPTED != 0 {
                        acc.flags |= rflags::ENCRYPTED;
                    }
                }
                attr::FILE_NAME if vlen >= 0x42 => {
                    let nlen = v[0x40] as usize;
                    let nsp = v[0x41];
                    if 0x42 + 2 * nlen > vlen {
                        return Err(());
                    }
                    if nsp == ns::DOS {
                        sink.stats.dos_names_skipped += 1;
                        continue;
                    }
                    let pref = u64le(v, 0);
                    let parent = pref & 0xFFFF_FFFF_FFFF;
                    if parent > u32::MAX as u64 {
                        continue;
                    }
                    let start = sink.arena.len();
                    push_utf16_as_utf8(&mut sink.arena, &v[0x42..0x42 + 2 * nlen]);
                    let nl = sink.arena.len() - start;
                    if start > u32::MAX as usize - nl {
                        // Arena would overflow u32 offsets; drop the name rather than wrap.
                        sink.arena.truncate(start);
                        continue;
                    }
                    sink.names.push(NameEntry {
                        record: target,
                        parent: parent as u32,
                        off: start as u32,
                        len: nl as u16,
                        parent_seq: (pref >> 48) as u16,
                    });
                    sink.stats.names += 1;
                }
                attr::DATA => {
                    if named {
                        acc.flags |= rflags::HAS_ADS;
                    } else {
                        acc.size = vlen as u64;
                        acc.has_size = true;
                        acc.flags |= rflags::RESIDENT;
                    }
                }
                attr::ATTRIBUTE_LIST => acc.flags |= rflags::HAS_ATTR_LIST,
                _ => {}
            }
        } else {
            if len < 0x40 {
                return Err(());
            }
            // On-disk bytes: real (non-sparse) clusters of this segment. Every segment of a
            // multi-segment attribute contributes its own runs.
            let mp = u16le(a, 0x20) as usize;
            if mp >= len {
                return Err(());
            }
            let clusters = runlist::allocated_clusters(&a[mp..]).ok_or(())?;
            acc.alloc = acc.alloc.saturating_add(clusters.saturating_mul(sink.cluster_size));
            // Logical sizes are only valid in the first segment (lowest VCN 0).
            if u64le(a, 0x10) != 0 {
                continue;
            }
            match ty {
                attr::DATA => {
                    if named {
                        acc.flags |= rflags::HAS_ADS;
                    } else {
                        acc.size = u64le(a, 0x30);
                        acc.has_size = true;
                        if aflags & AF_COMPRESSED != 0 {
                            acc.flags |= rflags::COMPRESSED;
                        }
                        if aflags & AF_SPARSE != 0 {
                            acc.flags |= rflags::SPARSE;
                        }
                        if aflags & AF_ENCRYPTED != 0 {
                            acc.flags |= rflags::ENCRYPTED;
                        }
                    }
                }
                attr::ATTRIBUTE_LIST => acc.flags |= rflags::HAS_ATTR_LIST,
                _ => {}
            }
        }
    }
}

/// UTF-16LE -> UTF-8 with an ASCII fast path. Unpaired surrogates (legal in NTFS names)
/// become U+FFFD.
#[inline]
pub fn push_utf16_as_utf8(out: &mut Vec<u8>, raw: &[u8]) {
    let (pairs, _) = raw.as_chunks::<2>();
    let ascii = pairs.iter().all(|c| c[1] == 0 && c[0] < 0x80);
    if ascii {
        out.reserve(pairs.len());
        out.extend(pairs.iter().map(|c| c[0]));
        return;
    }
    let units = pairs.iter().map(|&c| u16::from_le_bytes(c));
    let mut buf = [0u8; 4];
    for ch in char::decode_utf16(units) {
        let ch = ch.unwrap_or(char::REPLACEMENT_CHARACTER);
        out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
    }
}

// ---------------------------------------------------------------------------------------
// Cold path: full attribute decoding (used for $MFT record 0 and diagnostics).
// ---------------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy)]
pub struct RecordHeader {
    pub seq: u16,
    pub link_count: u16,
    pub flags: u16,
    pub base_ref: u64,
    pub bytes_in_use: u32,
}

impl RecordHeader {
    pub fn in_use(&self) -> bool {
        self.flags & REC_IN_USE != 0
    }
}

#[derive(Debug, Clone)]
pub struct NonResident {
    pub lowest_vcn: u64,
    pub highest_vcn: u64,
    pub allocated: u64,
    pub data_size: u64,
    pub initialized: u64,
    pub extents: Vec<Extent>,
}

#[derive(Debug, Clone)]
pub enum AttrValue {
    Resident(Vec<u8>),
    NonResident(NonResident),
}

#[derive(Debug, Clone)]
pub struct Attribute {
    pub ty: u32,
    pub name: String,
    pub flags: u16,
    pub value: AttrValue,
}

/// Validate signature, apply fixup and decode the header.
pub fn fixup_record(rec: &mut [u8]) -> Result<RecordHeader> {
    if rec.len() < 0x30 || &rec[0..4] != b"FILE" {
        return Err(Error::BadMftRecord("signature"));
    }
    if !apply_fixup(rec) {
        return Err(Error::BadMftRecord("update sequence"));
    }
    Ok(RecordHeader {
        seq: u16le(rec, 0x10),
        link_count: u16le(rec, 0x12),
        flags: u16le(rec, 0x16),
        base_ref: u64le(rec, 0x20),
        bytes_in_use: u32le(rec, 0x18),
    })
}

/// Decode all attributes of a record that has already been fixed up.
pub fn attributes(rec: &[u8]) -> Result<Vec<Attribute>> {
    let used = (u32le(rec, 0x18) as usize).min(rec.len());
    let mut off = u16le(rec, 0x14) as usize;
    let mut out = Vec::new();
    loop {
        if off + 4 > used {
            return Err(Error::BadMftRecord("attribute overrun"));
        }
        let ty = u32le(rec, off);
        if ty == attr::END {
            return Ok(out);
        }
        if off + 8 > used {
            return Err(Error::BadMftRecord("attribute overrun"));
        }
        let len = u32le(rec, off + 4) as usize;
        if len < 0x18 || off + len > used {
            return Err(Error::BadMftRecord("attribute length"));
        }
        let a = &rec[off..off + len];
        off += len;
        let nlen = a[9] as usize;
        let noff = u16le(a, 0x0A) as usize;
        if noff + 2 * nlen > len {
            return Err(Error::BadMftRecord("attribute name"));
        }
        let mut nb = Vec::new();
        push_utf16_as_utf8(&mut nb, &a[noff..noff + 2 * nlen]);
        let name = String::from_utf8(nb).unwrap_or_default();
        let flags = u16le(a, 0x0C);
        let value = if a[8] == 0 {
            let vlen = u32le(a, 0x10) as usize;
            let voff = u16le(a, 0x14) as usize;
            if voff + vlen > len {
                return Err(Error::BadMftRecord("resident value"));
            }
            AttrValue::Resident(a[voff..voff + vlen].to_vec())
        } else {
            if len < 0x40 {
                return Err(Error::BadMftRecord("non-resident header"));
            }
            let lowest_vcn = u64le(a, 0x10);
            let mp = u16le(a, 0x20) as usize;
            if mp > len {
                return Err(Error::BadMftRecord("mapping pairs offset"));
            }
            let mut extents = Vec::new();
            runlist::decode(&a[mp..], lowest_vcn, &mut extents)?;
            AttrValue::NonResident(NonResident {
                lowest_vcn,
                highest_vcn: u64le(a, 0x18),
                allocated: u64le(a, 0x28),
                data_size: u64le(a, 0x30),
                initialized: u64le(a, 0x38),
                extents,
            })
        };
        out.push(Attribute { ty, name, flags, value });
    }
}

/// One `$ATTRIBUTE_LIST` entry.
#[derive(Debug, Clone, Copy)]
pub struct AttrListEntry {
    pub ty: u32,
    pub lowest_vcn: u64,
    pub segment: u64,
    pub segment_seq: u16,
    pub name_len: u8,
}

pub fn parse_attribute_list(v: &[u8]) -> Result<Vec<AttrListEntry>> {
    let mut out = Vec::new();
    let mut off = 0;
    while off + 0x1A <= v.len() {
        let ty = u32le(v, off);
        let rlen = u16le(v, off + 4) as usize;
        if rlen < 0x1A || off + rlen > v.len() {
            return Err(Error::BadMftRecord("attribute list entry"));
        }
        let sref = u64le(v, off + 0x10);
        out.push(AttrListEntry {
            ty,
            name_len: v[off + 6],
            lowest_vcn: u64le(v, off + 8),
            segment: sref & 0xFFFF_FFFF_FFFF,
            segment_seq: (sref >> 48) as u16,
        });
        off += rlen;
    }
    Ok(out)
}

// ---------------------------------------------------------------------------------------
// Synthetic record builder (tests, fuzzing, benchmarks).
// ---------------------------------------------------------------------------------------

/// Builds valid, fixup-protected FILE records in memory.
pub mod synth {
    use super::*;

    pub struct RecordBuilder {
        size: usize,
        seq: u16,
        flags: u16,
        base: u64,
        attrs: Vec<Vec<u8>>,
    }

    fn pad8(v: &mut Vec<u8>) {
        while !v.len().is_multiple_of(8) {
            v.push(0);
        }
    }

    fn utf16(s: &str) -> Vec<u8> {
        s.encode_utf16().flat_map(|u| u.to_le_bytes()).collect()
    }

    impl RecordBuilder {
        pub fn new(size: usize) -> Self {
            RecordBuilder { size, seq: 1, flags: REC_IN_USE, base: 0, attrs: Vec::new() }
        }
        pub fn seq(mut self, s: u16) -> Self {
            self.seq = s;
            self
        }
        pub fn dir(mut self) -> Self {
            self.flags |= REC_DIRECTORY;
            self
        }
        pub fn unused(mut self) -> Self {
            self.flags &= !REC_IN_USE;
            self
        }
        pub fn extension_of(mut self, base: u32, base_seq: u16) -> Self {
            self.base = base as u64 | (base_seq as u64) << 48;
            self
        }
        pub fn std_info(self, mtime: u64, attrs: u32) -> Self {
            let mut v = vec![0u8; 0x48];
            v[0x08..0x10].copy_from_slice(&mtime.to_le_bytes());
            v[0x20..0x24].copy_from_slice(&attrs.to_le_bytes());
            self.resident(attr::STANDARD_INFORMATION, "", &v)
        }
        pub fn file_name(self, parent: u32, parent_seq: u16, name: &str, namespace: u8) -> Self {
            let n = utf16(name);
            let mut v = vec![0u8; 0x42];
            let pref = parent as u64 | (parent_seq as u64) << 48;
            v[0..8].copy_from_slice(&pref.to_le_bytes());
            v[0x40] = (n.len() / 2) as u8;
            v[0x41] = namespace;
            v.extend_from_slice(&n);
            self.resident(attr::FILE_NAME, "", &v)
        }
        pub fn resident(mut self, ty: u32, name: &str, value: &[u8]) -> Self {
            let n = utf16(name);
            let mut a = vec![0u8; 0x18];
            a.extend_from_slice(&n);
            pad8(&mut a);
            let voff = a.len();
            a.extend_from_slice(value);
            pad8(&mut a);
            let len = a.len() as u32;
            a[0..4].copy_from_slice(&ty.to_le_bytes());
            a[4..8].copy_from_slice(&len.to_le_bytes());
            a[8] = 0;
            a[9] = (n.len() / 2) as u8;
            a[0x0A..0x0C].copy_from_slice(&0x18u16.to_le_bytes());
            a[0x10..0x14].copy_from_slice(&(value.len() as u32).to_le_bytes());
            a[0x14..0x16].copy_from_slice(&(voff as u16).to_le_bytes());
            self.attrs.push(a);
            self
        }
        #[allow(clippy::too_many_arguments)]
        pub fn non_resident(
            mut self,
            ty: u32,
            name: &str,
            aflags: u16,
            lowest_vcn: u64,
            allocated: u64,
            data_size: u64,
            compressed: Option<u64>,
            runs: &[Extent],
        ) -> Self {
            let n = utf16(name);
            let hdr = if compressed.is_some() { 0x48 } else { 0x40 };
            let mut a = vec![0u8; hdr];
            a.extend_from_slice(&n);
            pad8(&mut a);
            let mp = a.len();
            a.extend_from_slice(&runlist::encode(runs));
            pad8(&mut a);
            let len = a.len() as u32;
            let highest = runs.last().map(|e| e.vcn + e.len - 1).unwrap_or(0);
            a[0..4].copy_from_slice(&ty.to_le_bytes());
            a[4..8].copy_from_slice(&len.to_le_bytes());
            a[8] = 1;
            a[9] = (n.len() / 2) as u8;
            a[0x0A..0x0C].copy_from_slice(&(hdr as u16).to_le_bytes());
            a[0x0C..0x0E].copy_from_slice(&aflags.to_le_bytes());
            a[0x10..0x18].copy_from_slice(&lowest_vcn.to_le_bytes());
            a[0x18..0x20].copy_from_slice(&highest.to_le_bytes());
            a[0x20..0x22].copy_from_slice(&(mp as u16).to_le_bytes());
            a[0x28..0x30].copy_from_slice(&allocated.to_le_bytes());
            a[0x30..0x38].copy_from_slice(&data_size.to_le_bytes());
            a[0x38..0x40].copy_from_slice(&data_size.to_le_bytes());
            if let Some(c) = compressed {
                a[0x40..0x48].copy_from_slice(&c.to_le_bytes());
            }
            self.attrs.push(a);
            self
        }

        /// Serialize and protect with the update sequence `usn`.
        pub fn build(&self, usn: u16) -> Vec<u8> {
            let mut r = vec![0u8; self.size];
            let usa_cnt = self.size / 512 + 1;
            let usa_off = 0x30usize;
            let first = (usa_off + 2 * usa_cnt).div_ceil(8) * 8;
            r[0..4].copy_from_slice(b"FILE");
            r[4..6].copy_from_slice(&(usa_off as u16).to_le_bytes());
            r[6..8].copy_from_slice(&(usa_cnt as u16).to_le_bytes());
            r[0x10..0x12].copy_from_slice(&self.seq.to_le_bytes());
            r[0x12..0x14].copy_from_slice(&1u16.to_le_bytes());
            r[0x14..0x16].copy_from_slice(&(first as u16).to_le_bytes());
            r[0x16..0x18].copy_from_slice(&self.flags.to_le_bytes());
            r[0x1C..0x20].copy_from_slice(&(self.size as u32).to_le_bytes());
            r[0x20..0x28].copy_from_slice(&self.base.to_le_bytes());
            let mut off = first;
            for a in &self.attrs {
                r[off..off + a.len()].copy_from_slice(a);
                off += a.len();
            }
            r[off..off + 4].copy_from_slice(&attr::END.to_le_bytes());
            off += 8;
            assert!(off <= self.size - 2, "attributes overflow the record");
            r[0x18..0x1C].copy_from_slice(&(off as u32).to_le_bytes());
            // protect
            r[usa_off..usa_off + 2].copy_from_slice(&usn.to_le_bytes());
            for i in 1..usa_cnt {
                let end = i * 512 - 2;
                let (s0, s1) = (r[end], r[end + 1]);
                r[usa_off + 2 * i] = s0;
                r[usa_off + 2 * i + 1] = s1;
                r[end..end + 2].copy_from_slice(&usn.to_le_bytes());
            }
            r
        }
    }
}

#[cfg(test)]
mod tests {
    use super::synth::RecordBuilder;
    use super::*;

    fn parse(raw: &mut [u8], no: u32) -> (Rec, Sink) {
        let mut out = Rec::default();
        let mut sink = Sink::new(4096, 0);
        parse_record(raw, no, &mut out, &mut sink);
        (out, sink)
    }

    #[test]
    fn small_resident_file() {
        let mut raw = RecordBuilder::new(1024)
            .seq(7)
            .std_info(132_000_000_000_000_000, 0x20)
            .file_name(5, 5, "readme.txt", ns::WIN32_AND_DOS)
            .resident(attr::DATA, "", b"hello world")
            .build(0x1234);
        let (r, sink) = parse(&mut raw, 42);
        assert!(r.in_use() && !r.is_dir());
        assert_eq!(r.seq, 7);
        assert_eq!(r.size, 11);
        assert_eq!(r.alloc, 0);
        assert_eq!(r.mtime, 132_000_000_000_000_000);
        assert!(r.flags & rflags::RESIDENT != 0);
        assert_eq!(sink.names.len(), 1);
        let e = sink.names[0];
        assert_eq!((e.record, e.parent, e.parent_seq), (42, 5, 5));
        assert_eq!(sink.name(&e), "readme.txt");
    }

    #[test]
    fn dos_names_are_dropped_and_hardlinks_kept() {
        let mut raw = RecordBuilder::new(1024)
            .std_info(0, 0)
            .file_name(100, 1, "LONGFI~1.TXT", ns::DOS)
            .file_name(100, 1, "long file name.txt", ns::WIN32)
            .file_name(200, 3, "link.txt", ns::POSIX)
            .build(1);
        let (_, sink) = parse(&mut raw, 9);
        let names: Vec<_> = sink.names.iter().map(|e| sink.name(e).to_string()).collect();
        assert_eq!(names, ["long file name.txt", "link.txt"]);
        assert_eq!(sink.stats.dos_names_skipped, 1);
    }

    #[test]
    fn non_resident_sparse_and_ads() {
        let runs = [Extent { vcn: 0, len: 4, lcn: Some(1000) }, Extent { vcn: 4, len: 252, lcn: None }];
        let mut raw = RecordBuilder::new(1024)
            .std_info(0, 0x200)
            .file_name(5, 5, "sparse.bin", ns::WIN32_AND_DOS)
            .non_resident(attr::DATA, "", AF_SPARSE, 0, 256 * 4096, 1 << 20, Some(4 * 4096), &runs)
            .non_resident(attr::DATA, "Zone.Identifier", 0, 0, 4096, 100, None, &[Extent { vcn: 0, len: 1, lcn: Some(77) }])
            .build(9);
        let (r, _) = parse(&mut raw, 50);
        assert_eq!(r.size, 1 << 20);
        assert_eq!(r.alloc, 4 * 4096 + 4096);
        assert!(r.flags & rflags::SPARSE != 0);
        assert!(r.flags & rflags::HAS_ADS != 0);
    }

    #[test]
    fn unflagged_sparse_stream_uses_runs_not_header() {
        // Like $BadClus:$Bad - header claims the whole volume, every run is sparse.
        let mut raw = RecordBuilder::new(1024)
            .std_info(0, 6)
            .file_name(5, 5, "$BadClus", ns::WIN32_AND_DOS)
            .resident(attr::DATA, "", &[])
            .non_resident(attr::DATA, "$Bad", 0, 0, 1 << 40, 1 << 40, None, &[Extent { vcn: 0, len: (1 << 40) / 4096, lcn: None }])
            .build(4);
        let (r, _) = parse(&mut raw, 8);
        assert_eq!(r.alloc, 0);
        assert_eq!(r.size, 0);
    }

    #[test]
    fn directory_index_allocation_counts_on_disk() {
        let mut raw = RecordBuilder::new(1024)
            .dir()
            .std_info(0, 0x10)
            .file_name(5, 5, "Windows", ns::WIN32_AND_DOS)
            .resident(attr::INDEX_ROOT, "$I30", &[0u8; 32])
            .non_resident(attr::INDEX_ALLOCATION, "$I30", 0, 0, 3 * 4096, 3 * 4096, None, &[Extent { vcn: 0, len: 3, lcn: Some(5000) }])
            .build(3);
        let (r, _) = parse(&mut raw, 60);
        assert!(r.is_dir());
        assert_eq!(r.size, 0);
        assert_eq!(r.alloc, 3 * 4096);
    }

    #[test]
    fn extension_record_contributes_to_base() {
        let mut raw = RecordBuilder::new(1024)
            .extension_of(70, 4)
            .file_name(5, 5, "second-link", ns::WIN32)
            .non_resident(attr::DATA, "", 0, 0, 8192, 5000, None, &[Extent { vcn: 0, len: 2, lcn: Some(9) }])
            .non_resident(attr::DATA, "", 0, 2, 0, 0, None, &[Extent { vcn: 2, len: 2, lcn: Some(99) }])
            .build(5);
        let (r, sink) = parse(&mut raw, 900);
        assert_eq!(r, Rec::default(), "extension records do not fill their own slot");
        assert_eq!(sink.stats.extension, 1);
        assert_eq!(
            sink.ext,
            // both segments' clusters count; the size comes from the VCN-0 segment only
            vec![ExtFrag { base: 70, base_seq: 4, flags: 0, has_size: true, size: 5000, alloc: 4 * 4096 }]
        );
        assert_eq!(sink.names[0].record, 70);
    }

    #[test]
    fn torn_record_is_rejected() {
        let mut raw = RecordBuilder::new(1024).std_info(0, 0).build(0xABCD);
        raw[1022] ^= 0xFF; // corrupt the second sector's USN slot
        let (r, sink) = parse(&mut raw, 1);
        assert_eq!(r, Rec::default());
        assert_eq!(sink.stats.corrupt, 1);
    }

    #[test]
    fn unused_and_zero_records_are_skipped() {
        let mut raw = RecordBuilder::new(1024).unused().std_info(0, 0).build(1);
        let (r, sink) = parse(&mut raw, 1);
        assert_eq!(r, Rec::default());
        assert_eq!(sink.stats.in_use, 0);
        let mut zero = vec![0u8; 1024];
        let (r, _) = parse(&mut zero, 2);
        assert_eq!(r, Rec::default());
    }

    #[test]
    fn non_ascii_names() {
        let mut raw = RecordBuilder::new(4096).std_info(0, 0).file_name(5, 5, "Документы 📁", ns::WIN32).build(2);
        let (_, sink) = parse(&mut raw, 3);
        assert_eq!(sink.name(&sink.names[0]), "Документы 📁");
    }

    #[test]
    fn cold_path_decodes_attributes() {
        let runs = [Extent { vcn: 0, len: 16, lcn: Some(786432) }];
        let mut raw = RecordBuilder::new(1024)
            .std_info(0, 6)
            .file_name(5, 5, "$MFT", ns::WIN32_AND_DOS)
            .non_resident(attr::DATA, "", 0, 0, 65536, 65536, None, &runs)
            .build(1);
        let h = fixup_record(&mut raw).unwrap();
        assert!(h.in_use());
        let attrs = attributes(&raw).unwrap();
        let data = attrs.iter().find(|a| a.ty == attr::DATA).unwrap();
        match &data.value {
            AttrValue::NonResident(nr) => {
                assert_eq!(nr.extents, runs);
                assert_eq!(nr.data_size, 65536);
            }
            _ => panic!("expected non-resident"),
        }
    }
}
