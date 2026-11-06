//! # ft-index
//!
//! Compact, pointer-free file tree built from an MFT scan.
//!
//! Layout (structure of arrays, one slot per *node* = one displayed name):
//!
//! * Nodes are numbered in **BFS order** from the root. Consequences:
//!   * `parent[n] < n` for every non-root node, so directory totals are a single reverse
//!     linear pass (`for n in (1..N).rev() { total[parent[n]] += total[n] }`), no recursion
//!     and no sorting;
//!   * the children of any node are contiguous: `first_child[n] .. first_child[n + 1]`,
//!     so no per-node child vectors exist.
//! * Names live in one UTF-8 arena in node order: `names[name_off[n] .. name_off[n + 1]]`.
//!
//! Hard links: a file record with several names becomes several nodes. The first one
//! reached in BFS order is the primary; the others carry [`nflags::DUP_LINK`] and are shown
//! with their size but excluded from directory totals, so the volume total counts every
//! record exactly once.
//!
//! Records that cannot be reached from the root (deleted/reused parent, reference cycles,
//! corruption) are kept under a synthetic `<orphaned>` directory instead of being dropped.
//!
//! A built index is updated in place from catalog changes ([`Index::patch`], `patch.rs`):
//! node numbers stay stable, deleted nodes stay listed (flagged) but leave every total,
//! and totals change along the parent chain only. Nodes a patch adds or moves are outside
//! the BFS ranges, so [`Index::children`] also consults a small side table and filters by
//! `parent`.

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap};
use std::fmt;
use std::hash::{BuildHasherDefault, Hasher};
use std::ops::Range;
use std::time::{Duration, Instant};

use ft_ntfs::{NameEntry, ROOT_RECORD, Rec, rflags};

pub const NONE: u32 = u32::MAX;

const FILETIME_PER_MINUTE: u64 = 600_000_000;

/// Per-node flags. Bits 1..=8 mirror [`ft_ntfs::rflags`]; bit 0 and bit 15 are index-only.
pub mod nflags {
    use ft_ntfs::rflags;
    /// Secondary hard link: displayed, not counted in totals.
    pub const DUP_LINK: u16 = 1 << 0;
    pub const DIR: u16 = rflags::DIR;
    pub const HAS_ADS: u16 = rflags::HAS_ADS;
    pub const COMPRESSED: u16 = rflags::COMPRESSED;
    pub const SPARSE: u16 = rflags::SPARSE;
    pub const ENCRYPTED: u16 = rflags::ENCRYPTED;
    pub const REPARSE: u16 = rflags::REPARSE;
    pub const RESIDENT: u16 = rflags::RESIDENT;
    /// FILE_ATTRIBUTE_HIDDEN / FILE_ATTRIBUTE_SYSTEM (shown dimmed).
    pub const HIDDEN: u16 = 1 << 9;
    pub const SYSTEM: u16 = 1 << 10;
    /// Removed from the volume after the scan: still listed (struck through), counted in
    /// no total.
    pub const DELETED: u16 = 1 << 11;
    /// Renamed after the scan: the current name lives in a side table.
    pub const RENAMED: u16 = 1 << 12;
    /// Not backed by an MFT record (the `<orphaned>` container).
    pub const SYNTHETIC: u16 = 1 << 15;

    /// Node flag bits derived from Win32 file attributes.
    #[inline]
    pub fn from_attrs(attrs: u32) -> u16 {
        (if attrs & 0x2 != 0 { HIDDEN } else { 0 }) | (if attrs & 0x4 != 0 { SYSTEM } else { 0 })
    }
}

pub mod patch;
pub mod types;

/// FNV-1a: fast for the short keys used here (extensions, node numbers).
#[derive(Default)]
pub(crate) struct Fnv(u64);

impl Hasher for Fnv {
    fn write(&mut self, bytes: &[u8]) {
        let mut h = if self.0 == 0 { 0xcbf2_9ce4_8422_2325 } else { self.0 };
        for &b in bytes {
            h = (h ^ b as u64).wrapping_mul(0x0100_0000_01b3);
        }
        self.0 = h;
    }
    fn finish(&self) -> u64 {
        self.0
    }
}

pub(crate) type FnvMap<K, V> = HashMap<K, V, BuildHasherDefault<Fnv>>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BuildError {
    NoRoot,
    TooManyNodes,
}

impl fmt::Display for BuildError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BuildError::NoRoot => write!(f, "root directory record (5) missing or not a directory"),
            BuildError::TooManyNodes => write!(f, "more than 2^32 - 2 tree nodes"),
        }
    }
}

impl std::error::Error for BuildError {}

#[derive(Default, Debug, Clone)]
pub struct BuildStats {
    pub nodes: u64,
    pub dup_links: u64,
    pub orphan_roots: u64,
    pub nameless_dirs: u64,
    pub names_dropped: u64,
    pub t_group: Duration,
    pub t_reach: Duration,
    pub t_bfs: Duration,
    pub t_names: Duration,
    pub t_aggregate: Duration,
}

impl BuildStats {
    pub fn total(&self) -> Duration {
        self.t_group + self.t_reach + self.t_bfs + self.t_names + self.t_aggregate
    }
}

/// The compact tree. See the crate docs for the layout invariants.
#[derive(Default)]
pub struct Index {
    pub parent: Vec<u32>,
    /// `len() + 1` entries.
    pub first_child: Vec<u32>,
    /// `len() + 1` entries.
    pub name_off: Vec<u32>,
    pub names: Vec<u8>,
    /// Logical size; aggregated for directories.
    pub size: Vec<u64>,
    /// Bytes on disk; aggregated for directories (includes directory indexes).
    pub alloc: Vec<u64>,
    /// Last write time in minutes since 1601 (FILETIME / 600 000 000): what a UI shows,
    /// in half the memory of a FILETIME. See [`Index::mtime_filetime`].
    pub mtime: Vec<u32>,
    pub flags: Vec<u16>,
    /// Files below a directory (primary links only).
    pub files: Vec<u32>,
    /// Directories below a directory.
    pub dirs: Vec<u32>,
    /// MFT record -> its primary node (`NONE` if none): where patches enter the tree.
    pub node_of: Vec<u32>,
    /// Children a patch added or moved into a directory, outside its BFS range.
    extra: FnvMap<u32, Vec<u32>>,
    /// Current names of nodes flagged [`nflags::RENAMED`].
    renamed: FnvMap<u32, Box<str>>,
    /// A patch moved a node: a BFS range may hold nodes that have left it.
    moved: bool,
    /// Bumped by every patch (caches derived from the index compare it).
    pub revision: u64,
}

/// Children of a node: its BFS range, then nodes a patch placed there.
pub struct Children<'a> {
    ix: &'a Index,
    parent: u32,
    range: Range<u32>,
    extra: std::slice::Iter<'a, u32>,
    all: bool,
}

impl Iterator for Children<'_> {
    type Item = u32;
    #[inline]
    fn next(&mut self) -> Option<u32> {
        let ix = self.ix;
        for c in self.range.by_ref() {
            let f = ix.flags[c as usize];
            if (self.all || f & nflags::DELETED == 0) && (!ix.moved || ix.parent[c as usize] == self.parent) {
                return Some(c);
            }
        }
        for &c in self.extra.by_ref() {
            let f = ix.flags[c as usize];
            if (self.all || f & nflags::DELETED == 0) && ix.parent[c as usize] == self.parent {
                return Some(c);
            }
        }
        None
    }
}

struct Bits(Vec<u64>);
impl Bits {
    fn new(n: usize) -> Self {
        Bits(vec![0; n.div_ceil(64)])
    }
    #[inline]
    fn get(&self, i: usize) -> bool {
        self.0[i >> 6] >> (i & 63) & 1 != 0
    }
    #[inline]
    fn set(&mut self, i: usize) {
        self.0[i >> 6] |= 1 << (i & 63);
    }
}

/// Node arrays under construction during the BFS.
struct Bfs {
    parent: Vec<u32>,
    record: Vec<u32>,
    flags: Vec<u16>,
    size: Vec<u64>,
    alloc: Vec<u64>,
    mtime: Vec<u32>,
    /// Name source per node: (arena offset, length); offset `NONE` = synthetic label.
    src: Vec<(u32, u32)>,
    /// Synthetic labels (root, `<orphaned>`, nameless records), ascending by node.
    labels: Vec<(u32, Vec<u8>)>,
    seen: Bits,
}

impl Bfs {
    #[inline]
    fn node(&mut self, p: u32, r: u32, flags: u16, rr: &Rec, src: (u32, u32)) {
        self.parent.push(p);
        self.record.push(r);
        self.flags.push(flags | nflags::from_attrs(rr.attrs));
        self.size.push(if rr.is_dir() { 0 } else { rr.size });
        self.alloc.push(rr.alloc);
        self.mtime.push((rr.mtime / FILETIME_PER_MINUTE).min(u32::MAX as u64) as u32);
        self.src.push(src);
    }

    /// A name of record `r` under node `p`. The first name of a file is primary, later
    /// ones are marked as duplicate links; a directory is only ever placed once.
    #[inline]
    fn push(&mut self, recs: &[Rec], r: u32, e: &NameEntry, p: u32) {
        let rr = &recs[r as usize];
        let dup = self.seen.get(r as usize);
        if dup && rr.is_dir() {
            return;
        }
        self.seen.set(r as usize);
        let f = (rr.flags & !rflags::IN_USE) | if dup { nflags::DUP_LINK } else { 0 };
        self.node(p, r, f, rr, (e.off, e.len as u32));
    }

    fn push_synthetic(&mut self, p: u32, r: u32, flags: u16, label: Vec<u8>, rr: &Rec) {
        if r != NONE {
            self.seen.set(r as usize);
        }
        let i = self.parent.len() as u32;
        let len = label.len() as u32;
        self.labels.push((i, label));
        self.node(p, r, flags, rr, (NONE, len));
    }
}

/// Copy every node's name into `out` at `off[i]`, in parallel over disjoint node ranges.
fn gather_names(src: &[(u32, u32)], labels: &[(u32, Vec<u8>)], off: &[u32], arena: &[u8], out: &mut [u8]) {
    let n = src.len();
    let copy = |a: usize, b: usize, dst: &mut [u8]| {
        let base = off[a] as usize;
        for i in a..b {
            let (o, l) = (src[i].0 as usize, src[i].1 as usize);
            let d = off[i] as usize - base;
            if src[i].0 != NONE {
                dst[d..d + l].copy_from_slice(&arena[o..o + l]);
            } else if let Ok(j) = labels.binary_search_by_key(&(i as u32), |x| x.0) {
                dst[d..d + l].copy_from_slice(&labels[j].1);
            }
        }
    };
    let threads = std::thread::available_parallelism().map_or(1, |t| t.get()).min(8).min(n / 65_536 + 1);
    if threads <= 1 {
        copy(0, n, out);
        return;
    }
    let copy = &copy;
    std::thread::scope(|s| {
        let mut rest = out;
        let mut a = 0;
        for t in 0..threads {
            let b = if t + 1 == threads { n } else { n * (t + 1) / threads };
            let (chunk, r) = std::mem::take(&mut rest).split_at_mut((off[b] - off[a]) as usize);
            rest = r;
            s.spawn(move || copy(a, b, chunk));
            a = b;
        }
    });
}

const ST_UNKNOWN: u8 = 0;
const ST_VISITING: u8 = 1;
const ST_REACH: u8 = 2;
const ST_UNREACH: u8 = 3;
const ST_ROOTED: u8 = 4;

/// How secondary hard links enter directory totals.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum LinkPolicy {
    /// Every MFT record counts once, in the folder of its first-found name. Folder sums
    /// add up exactly to the volume's used space.
    #[default]
    Once,
    /// Logical size and file count are added for every link; on-disk bytes once
    /// (WizTree-compatible). Totals then exceed the volume's real usage.
    SizeEveryLink,
}

impl Index {
    /// Build the tree from scan output. `root_name` is shown for the root (e.g. `"C:"`).
    pub fn build(recs: &[Rec], names: &[NameEntry], arena: &[u8], root_name: &str) -> Result<(Index, BuildStats), BuildError> {
        Self::build_with(recs, names, arena, root_name, LinkPolicy::Once)
    }

    pub fn build_with(
        recs: &[Rec],
        names: &[NameEntry],
        arena: &[u8],
        root_name: &str,
        links: LinkPolicy,
    ) -> Result<(Index, BuildStats), BuildError> {
        let mut st = BuildStats::default();
        let n_rec = recs.len();
        let root = ROOT_RECORD as usize;
        if root >= n_rec || !recs[root].in_use() || !recs[root].is_dir() {
            return Err(BuildError::NoRoot);
        }
        let valid_parent = |e: &NameEntry| {
            let p = &recs[e.parent as usize];
            p.in_use() && p.is_dir() && p.seq == e.parent_seq && e.record != e.parent
        };

        // 1. One pass over the names: classify each (invalid / directory / file), count
        //    children per parent, remember each directory's canonical name and mark files
        //    that have a valid parent. Later passes use the class byte instead of touching
        //    `recs` again (random access).
        const C_DIR: u8 = 1;
        const C_FILE: u8 = 2;
        let t = Instant::now();
        let mut class = vec![0u8; names.len()];
        let mut start = vec![0u32; n_rec + 1];
        let mut dir_name = vec![NONE; n_rec];
        let mut state = vec![ST_UNKNOWN; n_rec];
        let mut valid = 0usize;
        for (i, e) in names.iter().enumerate() {
            let (r, p) = (e.record as usize, e.parent as usize);
            if r >= n_rec || p >= n_rec || !recs[r].in_use() {
                continue;
            }
            valid += 1;
            start[p + 1] += 1;
            if recs[r].is_dir() {
                class[i] = C_DIR;
                if dir_name[r] == NONE {
                    dir_name[r] = i as u32;
                }
            } else {
                class[i] = C_FILE;
                if valid_parent(e) {
                    state[r] = ST_REACH;
                }
            }
        }
        st.names_dropped = (names.len() - valid) as u64;
        for i in 1..=n_rec {
            start[i] += start[i - 1];
        }
        let mut by_parent = vec![0u32; valid];
        for (i, e) in names.iter().enumerate() {
            if class[i] != 0 {
                let c = &mut start[e.parent as usize];
                by_parent[*c as usize] = i as u32;
                *c += 1;
            }
        }
        start.copy_within(0..n_rec, 1);
        start[0] = 0;
        st.t_group = t.elapsed();

        // 2. Reachability. Directories follow their canonical (first) name upwards with
        //    memoisation; cycles are broken at the point of detection. Files without any
        //    valid parent become orphan roots under their first name.
        let t = Instant::now();
        state[root] = ST_REACH;
        let mut orphan_roots: Vec<(u32, u32)> = Vec::new(); // (record, name index or NONE)
        let mut stack = Vec::new();
        for d in 0..n_rec {
            if state[d] != ST_UNKNOWN || !recs[d].in_use() || !recs[d].is_dir() {
                continue;
            }
            stack.clear();
            let mut cur = d;
            let result = loop {
                state[cur] = ST_VISITING;
                stack.push(cur);
                let ni = dir_name[cur];
                if ni == NONE {
                    st.nameless_dirs += 1;
                    orphan_roots.push((cur as u32, NONE));
                    break ST_UNREACH;
                }
                let e = &names[ni as usize];
                if !valid_parent(e) {
                    orphan_roots.push((cur as u32, ni));
                    break ST_UNREACH;
                }
                let p = e.parent as usize;
                match state[p] {
                    ST_REACH => break ST_REACH,
                    ST_UNREACH => break ST_UNREACH,
                    ST_VISITING => {
                        orphan_roots.push((cur as u32, ni));
                        break ST_UNREACH;
                    }
                    _ => cur = p,
                }
            };
            for &x in &stack {
                state[x] = result;
            }
        }
        for (i, e) in names.iter().enumerate() {
            if class[i] == C_FILE && state[e.record as usize] == ST_UNKNOWN {
                state[e.record as usize] = ST_ROOTED;
                orphan_roots.push((e.record, i as u32));
            }
        }
        drop(state);
        drop(dir_name);
        drop(class);
        st.orphan_roots = orphan_roots.len() as u64;
        st.t_reach = t.elapsed();

        // 3. BFS. The node arrays double as the queue; per-node attributes are filled here
        //    because the record is already in cache.
        let t = Instant::now();
        let cap = valid + 2;
        if cap as u64 >= NONE as u64 {
            return Err(BuildError::TooManyNodes);
        }
        let mut b = Bfs {
            parent: Vec::with_capacity(cap),
            record: Vec::with_capacity(cap),
            flags: Vec::with_capacity(cap),
            size: Vec::with_capacity(cap),
            alloc: Vec::with_capacity(cap),
            mtime: Vec::with_capacity(cap),
            src: Vec::with_capacity(cap),
            labels: Vec::new(),
            seen: Bits::new(n_rec),
        };
        b.push_synthetic(NONE, ROOT_RECORD, nflags::DIR, root_name.as_bytes().to_vec(), &recs[root]);
        b.seen.set(root);
        let mut first_child: Vec<u32> = Vec::with_capacity(cap + 1);
        let mut k = 0usize;
        while k < b.parent.len() {
            first_child.push(b.parent.len() as u32);
            let f = b.flags[k];
            if f & nflags::DIR != 0 && f & nflags::DUP_LINK == 0 {
                if f & nflags::SYNTHETIC != 0 {
                    for &(r, ni) in &orphan_roots {
                        match names.get(ni as usize) {
                            Some(e) => b.push(recs, r, e, k as u32),
                            None => b.push_synthetic(
                                k as u32,
                                r,
                                recs[r as usize].flags & !rflags::IN_USE,
                                format!("<record {r}>").into_bytes(),
                                &recs[r as usize],
                            ),
                        }
                    }
                } else {
                    let rec = b.record[k] as usize;
                    let seq = recs[rec].seq;
                    for &ni in &by_parent[start[rec] as usize..start[rec + 1] as usize] {
                        let e = &names[ni as usize];
                        if e.record as usize == rec || e.parent_seq != seq {
                            continue;
                        }
                        b.push(recs, e.record, e, k as u32);
                    }
                }
                if k == 0 && !orphan_roots.is_empty() {
                    b.push_synthetic(0, NONE, nflags::DIR | nflags::SYNTHETIC, b"<orphaned>".to_vec(), &Rec::default());
                }
            }
            k += 1;
        }
        let n = b.parent.len();
        first_child.push(n as u32);
        drop(by_parent);
        drop(start);
        st.nodes = n as u64;
        st.dup_links = b.flags.iter().filter(|&&f| f & nflags::DUP_LINK != 0).count() as u64;
        st.t_bfs = t.elapsed();

        // 4. Names in node order: offsets by prefix sum, then a parallel gather from the
        //    arena into disjoint slices of the output.
        let t = Instant::now();
        let mut name_off: Vec<u32> = Vec::with_capacity(n + 1);
        let mut acc = 0usize;
        for s in &b.src {
            name_off.push(acc as u32);
            acc += s.1 as usize;
        }
        if acc > u32::MAX as usize {
            return Err(BuildError::TooManyNodes);
        }
        name_off.push(acc as u32);
        let mut out_names = vec![0u8; acc];
        gather_names(&b.src, &b.labels, &name_off, arena, &mut out_names);
        st.t_names = t.elapsed();

        // 5. Bottom-up aggregation: one reverse pass (parent[i] < i).
        let t = Instant::now();
        let Bfs { mut parent, flags, mut size, mut alloc, mut mtime, record, .. } = b;
        let mut node_of = vec![NONE; n_rec];
        for (i, &r) in record.iter().enumerate() {
            if r != NONE && flags[i] & nflags::DUP_LINK == 0 && node_of[r as usize] == NONE {
                node_of[r as usize] = i as u32;
            }
        }
        drop(record);
        let mut files = vec![0u32; n];
        let mut dirs = vec![0u32; n];
        for i in (1..n).rev() {
            let f = flags[i];
            let dup = f & nflags::DUP_LINK != 0;
            if dup && links == LinkPolicy::Once {
                continue;
            }
            let p = parent[i] as usize;
            size[p] += size[i];
            if !dup {
                alloc[p] += alloc[i];
            }
            if f & nflags::DIR != 0 {
                dirs[p] += dirs[i] + 1;
                files[p] += files[i];
            } else {
                files[p] += 1;
            }
        }
        st.t_aggregate = t.elapsed();

        // Capacities were sized for every name; give the slack back.
        let (mut flags, mut first_child, mut name_off, mut out_names) = (flags, first_child, name_off, out_names);
        parent.shrink_to_fit();
        first_child.shrink_to_fit();
        name_off.shrink_to_fit();
        out_names.shrink_to_fit();
        size.shrink_to_fit();
        alloc.shrink_to_fit();
        mtime.shrink_to_fit();
        flags.shrink_to_fit();
        Ok((
            Index {
                parent,
                first_child,
                name_off,
                names: out_names,
                size,
                alloc,
                mtime,
                flags,
                files,
                dirs,
                node_of,
                ..Default::default()
            },
            st,
        ))
    }

    /// Last write time of `n` as a FILETIME (minute precision).
    #[inline]
    pub fn mtime_filetime(&self, n: u32) -> u64 {
        self.mtime[n as usize] as u64 * FILETIME_PER_MINUTE
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.parent.len()
    }
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.parent.is_empty()
    }
    #[inline]
    pub fn is_dir(&self, n: u32) -> bool {
        self.flags[n as usize] & nflags::DIR != 0
    }
    #[inline]
    pub fn is_deleted(&self, n: u32) -> bool {
        self.flags[n as usize] & nflags::DELETED != 0
    }
    #[inline]
    pub fn name(&self, n: u32) -> &str {
        if self.flags[n as usize] & nflags::RENAMED != 0
            && let Some(s) = self.renamed.get(&n)
        {
            return s;
        }
        let (a, b) = (self.name_off[n as usize] as usize, self.name_off[n as usize + 1] as usize);
        std::str::from_utf8(&self.names[a..b]).unwrap_or("")
    }

    /// Current children of `n` (deleted ones skipped).
    #[inline]
    pub fn children(&self, n: u32) -> Children<'_> {
        self.children_impl(n, false)
    }

    /// Children of `n` including those deleted since the scan (listed struck through).
    #[inline]
    pub fn children_all(&self, n: u32) -> Children<'_> {
        self.children_impl(n, true)
    }

    #[inline]
    fn children_impl(&self, n: u32, all: bool) -> Children<'_> {
        let range = self.first_child[n as usize]..self.first_child[n as usize + 1];
        let extra = if self.extra.is_empty() { &[][..] } else { self.extra.get(&n).map_or(&[][..], |v| &v[..]) };
        Children { ix: self, parent: n, range, extra: extra.iter(), all }
    }

    /// Any child, deleted ones included.
    #[inline]
    pub fn has_children(&self, n: u32) -> bool {
        self.children_all(n).next().is_some()
    }

    /// Children sorted by size, largest first.
    pub fn children_by_size(&self, n: u32) -> Vec<u32> {
        let mut v: Vec<u32> = self.children(n).collect();
        v.sort_unstable_by_key(|&c| Reverse(self.size[c as usize]));
        v
    }

    pub fn depth(&self, mut n: u32) -> usize {
        let mut d = 0;
        while self.parent[n as usize] != NONE {
            n = self.parent[n as usize];
            d += 1;
        }
        d
    }

    /// Full path, e.g. `C:\Windows\System32`.
    pub fn path(&self, n: u32) -> String {
        let mut parts = Vec::new();
        let mut c = n;
        loop {
            parts.push(c);
            let p = self.parent[c as usize];
            if p == NONE {
                break;
            }
            c = p;
        }
        let mut s = String::new();
        for (i, &p) in parts.iter().rev().enumerate() {
            if i > 0 {
                s.push('\\');
            }
            s.push_str(self.name(p));
        }
        if parts.len() == 1 {
            s.push('\\');
        }
        s
    }

    /// Resolve a path such as `C:\Windows\System32` or `Windows/System32`
    /// (case-insensitive) to a node.
    pub fn find(&self, path: &str) -> Option<u32> {
        let root = self.name(0);
        let mut rest = path.trim();
        if rest.len() >= root.len() && rest[..root.len()].eq_ignore_ascii_case(root) {
            rest = &rest[root.len()..];
        }
        let mut n = 0u32;
        for comp in rest.split(['\\', '/']).filter(|c| !c.is_empty()) {
            let lc = comp.to_lowercase();
            n = self.children(n).find(|&c| {
                let nm = self.name(c);
                nm.eq_ignore_ascii_case(comp) || nm.to_lowercase() == lc
            })?;
        }
        Some(n)
    }

    /// The `k` largest nodes accepted by `filter`, by `key`, largest first.
    pub fn top_by(&self, k: usize, filter: impl Fn(u32) -> bool, key: impl Fn(u32) -> u64) -> Vec<u32> {
        let mut heap: BinaryHeap<Reverse<(u64, u32)>> = BinaryHeap::with_capacity(k + 1);
        for n in 0..self.len() as u32 {
            if !filter(n) {
                continue;
            }
            let v = key(n);
            if heap.len() < k {
                heap.push(Reverse((v, n)));
            } else if heap.peek().is_some_and(|Reverse((m, _))| v > *m) {
                heap.pop();
                heap.push(Reverse((v, n)));
            }
        }
        let mut out: Vec<(u64, u32)> = heap.into_iter().map(|Reverse(x)| x).collect();
        out.sort_unstable_by(|a, b| b.cmp(a));
        out.into_iter().map(|(_, n)| n).collect()
    }

    pub fn top_files(&self, k: usize) -> Vec<u32> {
        self.top_by(k, |n| self.flags[n as usize] & (nflags::DIR | nflags::DUP_LINK) == 0, |n| self.size[n as usize])
    }

    /// Bytes of heap owned by the index.
    pub fn heap_bytes(&self) -> usize {
        self.parent.capacity() * 4
            + self.first_child.capacity() * 4
            + self.name_off.capacity() * 4
            + self.names.capacity()
            + self.size.capacity() * 8
            + self.alloc.capacity() * 8
            + self.mtime.capacity() * 4
            + self.flags.capacity() * 2
            + self.files.capacity() * 4
            + self.dirs.capacity() * 4
            + self.node_of.capacity() * 4
            + self.extra.values().map(|v| v.capacity() * 4 + 32).sum::<usize>()
            + self.renamed.values().map(|v| v.len() + 32).sum::<usize>()
    }

    /// Check structural invariants (tests and `--verify-index`).
    pub fn check(&self) -> Result<(), String> {
        let n = self.len();
        if self.first_child.len() != n + 1 || self.name_off.len() != n + 1 {
            return Err("array lengths".into());
        }
        if self.parent.first() != Some(&NONE) {
            return Err("root parent".into());
        }
        let patched = self.revision != 0;
        for i in 1..n {
            let p = self.parent[i];
            if !patched && p as usize >= i {
                return Err(format!("node {i}: parent {p} not before child"));
            }
            if !self.children_all(p).any(|c| c == i as u32) {
                return Err(format!("node {i} not among its parent's children"));
            }
        }
        for i in 0..n {
            if self.first_child[i] > self.first_child[i + 1] {
                return Err(format!("first_child not monotonic at {i}"));
            }
            if !self.is_dir(i as u32) && self.has_children(i as u32) {
                return Err(format!("file node {i} has children"));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct T {
        recs: Vec<Rec>,
        names: Vec<NameEntry>,
        arena: Vec<u8>,
    }

    impl T {
        fn new(n: usize) -> Self {
            let mut t = T { recs: vec![Rec::default(); n], names: vec![], arena: vec![] };
            t.dir(5, 5, ".");
            t
        }
        fn rec(&mut self, r: u32, dir: bool, size: u64, alloc: u64) {
            let flags = rflags::IN_USE | if dir { rflags::DIR } else { 0 };
            self.recs[r as usize] = Rec { size, alloc, mtime: 0, attrs: 0, seq: 1, flags };
        }
        fn name(&mut self, r: u32, parent: u32, name: &str) {
            let off = self.arena.len() as u32;
            self.arena.extend_from_slice(name.as_bytes());
            self.names.push(NameEntry { record: r, parent, off, len: name.len() as u16, parent_seq: 1 });
        }
        fn dir(&mut self, r: u32, parent: u32, name: &str) {
            self.rec(r, true, 0, 4096);
            self.name(r, parent, name);
        }
        fn file(&mut self, r: u32, parent: u32, name: &str, size: u64) {
            self.rec(r, false, size, size.div_ceil(4096) * 4096);
            self.name(r, parent, name);
        }
        fn build(&self) -> (Index, BuildStats) {
            let (ix, st) = Index::build(&self.recs, &self.names, &self.arena, "C:").unwrap();
            ix.check().unwrap();
            (ix, st)
        }
    }

    #[test]
    fn aggregates_sizes_bottom_up() {
        let mut t = T::new(32);
        t.dir(16, 5, "Windows");
        t.dir(17, 16, "System32");
        t.file(18, 17, "ntoskrnl.exe", 10_000);
        t.file(19, 17, "hal.dll", 500);
        t.file(20, 16, "explorer.exe", 3_000);
        t.file(21, 5, "pagefile.sys", 1 << 30);
        let (ix, st) = t.build();
        assert_eq!(st.orphan_roots, 0);
        assert_eq!(ix.len(), 7);
        assert_eq!(ix.size[0], 10_000 + 500 + 3_000 + (1 << 30));
        let sys32 = ix.find(r"C:\windows\SYSTEM32").unwrap();
        assert_eq!(ix.size[sys32 as usize], 10_500);
        assert_eq!(ix.files[sys32 as usize], 2);
        assert_eq!(ix.files[0], 4);
        assert_eq!(ix.dirs[0], 2);
        // alloc includes the directory index allocations (3 dirs incl. root) + file clusters
        assert_eq!(ix.alloc[0], 3 * 4096 + 3 * 4096 + 4096 + 4096 + (1 << 30));
        assert_eq!(ix.path(sys32), r"C:\Windows\System32");
        assert_eq!(ix.path(0), r"C:\");
        assert_eq!(ix.name(ix.top_files(1)[0]), "pagefile.sys");
    }

    #[test]
    fn hard_links_counted_once() {
        let mut t = T::new(32);
        t.dir(16, 5, "a");
        t.dir(17, 5, "b");
        t.file(20, 16, "data.bin", 1000);
        t.name(20, 17, "link.bin");
        let (ix, st) = t.build();
        assert_eq!(st.dup_links, 1);
        assert_eq!(ix.size[0], 1000);
        assert_eq!(ix.files[0], 1);
        let link = ix.find(r"b\link.bin").unwrap();
        assert_eq!(ix.size[link as usize], 1000, "duplicate link still shows its size");
        assert!(ix.flags[link as usize] & nflags::DUP_LINK != 0);

        let (w, _) = Index::build_with(&t.recs, &t.names, &t.arena, "C:", LinkPolicy::SizeEveryLink).unwrap();
        assert_eq!(w.size[0], 2000, "logical size per link");
        assert_eq!(w.files[0], 2);
        assert_eq!(w.alloc[0], ix.alloc[0], "on-disk bytes still once");
        assert_eq!(w.size[w.find("b").unwrap() as usize], 1000);
    }

    #[test]
    fn stale_parent_reference_goes_to_orphans() {
        let mut t = T::new(32);
        t.dir(16, 5, "live");
        // parent 16 was reused: entry carries an old sequence number
        t.file(20, 16, "ghost.txt", 77);
        t.names.last_mut().unwrap().parent_seq = 9;
        // parent 25 not in use at all
        t.file(21, 25, "lost.txt", 5);
        let (ix, st) = t.build();
        assert_eq!(st.orphan_roots, 2);
        let orph = ix.find("<orphaned>").unwrap();
        assert!(ix.flags[orph as usize] & nflags::SYNTHETIC != 0);
        assert_eq!(ix.files[orph as usize], 2);
        assert_eq!(ix.size[0], 82, "orphans still count towards the volume");
        assert_eq!(ix.files[ix.find("live").unwrap() as usize], 0);
    }

    #[test]
    fn cycles_are_broken() {
        let mut t = T::new(32);
        t.dir(16, 17, "a");
        t.dir(17, 16, "b");
        t.file(18, 17, "f", 3);
        t.dir(19, 5, "ok");
        let (ix, st) = t.build();
        assert_eq!(st.orphan_roots, 1);
        let orph = ix.find("<orphaned>").unwrap();
        assert_eq!(ix.dirs[orph as usize], 2);
        assert_eq!(ix.files[orph as usize], 1);
        assert_eq!(ix.size[0], 3);
    }

    #[test]
    fn subtree_under_unreachable_dir_is_kept() {
        let mut t = T::new(40);
        t.dir(16, 30, "detached"); // parent 30 not in use
        t.dir(17, 16, "inner");
        t.file(18, 17, "deep.bin", 42);
        let (ix, _) = t.build();
        let n = ix.find(r"<orphaned>\detached\inner\deep.bin").unwrap();
        assert_eq!(ix.size[n as usize], 42);
    }

    #[test]
    fn bfs_children_are_contiguous() {
        let mut t = T::new(200);
        let mut r = 16;
        for d in 0..5u32 {
            let dr = r;
            t.dir(dr, 5, &format!("d{d}"));
            r += 1;
            for f in 0..10 {
                t.file(r, dr, &format!("f{f}"), (d * 10 + f) as u64);
                r += 1;
            }
        }
        let (ix, _) = t.build();
        for c in ix.children(0) {
            assert_eq!(ix.children(c).count(), 10);
        }
        assert_eq!(ix.size[0], (0..50).sum::<u64>());
    }

    #[test]
    fn missing_root_is_an_error() {
        let recs = vec![Rec::default(); 8];
        assert_eq!(Index::build(&recs, &[], &[], "C:").err(), Some(BuildError::NoRoot));
    }
}
