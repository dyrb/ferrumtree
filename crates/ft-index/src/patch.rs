//! Point updates of a built [`Index`] from catalog changes.
//!
//! A change is one MFT record before and after (its [`Rec`] and names). Applying it costs
//! O(depth) - the totals of the node's ancestors - instead of rebuilding the tree:
//!
//! * gone (or replaced by a new file in the same record): the node is flagged
//!   [`nflags::DELETED`] and leaves every total, but stays listed (struck through);
//! * new: a node is appended and linked to its parent through a side table;
//! * renamed / moved / resized / new attributes: updated in place.
//!
//! Directory totals stop at a deleted ancestor (its contribution already left the tree
//! above), so deleting a folder and then its files, in any order, stays consistent.
//!
//! Hard links (records with several names) are not patched: the caller rebuilds instead
//! (rare outside Windows Update). A new item whose parent cannot be found is reported as
//! unresolved, also for a rebuild.

use ft_ntfs::{ROOT_RECORD, Rec, rflags};

use crate::types::FileTypes;
use crate::{FILETIME_PER_MINUTE, Index, NONE, nflags};

/// One name of a record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Name {
    pub parent: u32,
    pub name: Box<str>,
    /// The parent reference is current (an in-use directory with the expected sequence).
    pub valid: bool,
}

/// A record before and after a catalog update. Records not in use have no names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Change {
    pub record: u32,
    pub old: Rec,
    pub new: Rec,
    pub old_names: Vec<Name>,
    pub new_names: Vec<Name>,
}

impl Change {
    /// Nothing the tree shows differs.
    pub fn is_noop(&self) -> bool {
        self.old == self.new && self.old_names == self.new_names
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PatchStats {
    pub created: u32,
    pub deleted: u32,
    pub moved: u32,
    pub renamed: u32,
    pub updated: u32,
    /// New items whose parent is not in the tree (a rebuild places them).
    pub unresolved: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PatchError {
    /// A record with several names (hard links) changed.
    HardLinks,
    /// The index has no record map (built from a directory walk).
    NoRecords,
}

/// What a node adds to its parent's totals: (size, alloc, files, dirs).
type Contrib = (i64, i64, i64, i64);

fn neg(c: Contrib) -> Contrib {
    (-c.0, -c.1, -c.2, -c.3)
}

fn minutes(filetime: u64) -> u32 {
    (filetime / FILETIME_PER_MINUTE).min(u32::MAX as u64) as u32
}

impl Index {
    /// Apply `changes` (any order) and keep `types` in step. Either the whole batch is
    /// applied or, on error, nothing is.
    pub fn patch(&mut self, changes: &[Change], types: &mut FileTypes) -> Result<PatchStats, PatchError> {
        if self.node_of.is_empty() {
            return Err(PatchError::NoRecords);
        }
        if changes.iter().any(|c| c.old_names.len() > 1 || c.new_names.len() > 1) {
            return Err(PatchError::HardLinks);
        }
        self.revision += 1;
        let mut st = PatchStats::default();

        // 1. Removals first: a record may be freed and reused within one batch.
        let mut todo: Vec<&Change> = Vec::new();
        let mut revived: Vec<(u32, u32)> = Vec::new();
        for c in changes {
            let node = self.node(c.record);
            let live = c.new.in_use() && !c.new_names.is_empty();
            let replaced =
                node != NONE && (!live || c.old.seq != c.new.seq || self.is_dir(node) != c.new.is_dir() || self.is_deleted(node));
            if replaced {
                // A node already flagged (deleted from the UI) that still exists - moved to
                // the Recycle Bin - reappears below as a new node; the flagged one stays.
                if !self.is_deleted(node) {
                    self.delete_node(node, types);
                    st.deleted += 1;
                } else if live && c.old.seq == c.new.seq {
                    revived.push((c.record, node));
                }
                self.node_of[c.record as usize] = NONE;
            }
            if live {
                todo.push(c);
            }
        }

        // 2. Creations and updates; a parent may itself be created by this batch, so
        //    repeat until nothing more can be placed.
        loop {
            let before = todo.len();
            let mut i = 0;
            while i < todo.len() {
                if self.place(todo[i], &revived, types, &mut st) {
                    todo.swap_remove(i);
                } else {
                    i += 1;
                }
            }
            if todo.is_empty() || todo.len() == before {
                break;
            }
        }
        st.unresolved = todo.len() as u32;
        Ok(st)
    }

    #[inline]
    fn node(&self, record: u32) -> u32 {
        self.node_of.get(record as usize).copied().unwrap_or(NONE)
    }

    /// The live directory node a name points into, if any.
    fn resolve(&self, nm: &Name) -> u32 {
        if !nm.valid {
            return NONE;
        }
        let q = self.node(nm.parent);
        if q == NONE || !self.is_dir(q) || self.is_deleted(q) { NONE } else { q }
    }

    /// Create or update the node of a live record. `false` if its parent is unknown yet.
    fn place(&mut self, c: &Change, revived: &[(u32, u32)], types: &mut FileTypes, st: &mut PatchStats) -> bool {
        let node = self.node(c.record);
        if node == 0 || c.record as usize == ROOT_RECORD as usize {
            if node != NONE {
                self.update_values(node, c);
            }
            return true;
        }
        let nm = &c.new_names[0];
        let q = self.resolve(nm);
        if node == NONE {
            if q == NONE {
                return false;
            }
            // A folder deleted from the UI and now in the Recycle Bin: its children stay
            // under the struck-through original, the new node carries its totals.
            let like = revived.iter().find(|x| x.0 == c.record).map(|x| x.1);
            let n = self.add_node(q, &c.new, &nm.name, like);
            if self.node_of.len() <= c.record as usize {
                self.node_of.resize(c.record as usize + 1, NONE);
            }
            self.node_of[c.record as usize] = n;
            types.add(self, n);
            st.created += 1;
            return true;
        }
        // Unknown new parent: only updates in place (e.g. an item under `<orphaned>` whose
        // parent reference did not change) can go ahead.
        let same_parent = c.old_names.first().is_some_and(|o| o.parent == nm.parent);
        if q == NONE && !same_parent {
            return false;
        }
        types.sub(self, node);
        if q != NONE && q != self.parent[node as usize] {
            self.move_node(node, q);
            st.moved += 1;
        }
        if self.name(node) != &*nm.name {
            self.renamed.insert(node, nm.name.clone());
            self.flags[node as usize] |= nflags::RENAMED;
            st.renamed += 1;
        }
        self.update_values(node, c);
        types.add(self, node);
        st.updated += 1;
        true
    }

    /// Size, attributes and time of an existing node from its record.
    fn update_values(&mut self, n: u32, c: &Change) {
        let i = n as usize;
        let (ds, da) = if self.is_dir(n) {
            // A directory's alloc is its own index plus everything below: apply the change
            // of its own part.
            (0, c.new.alloc as i64 - if c.old.in_use() { c.old.alloc as i64 } else { 0 })
        } else {
            (c.new.size as i64 - self.size[i] as i64, c.new.alloc as i64 - self.alloc[i] as i64)
        };
        if ds != 0 || da != 0 {
            self.adjust_up(n, (ds, da, 0, 0));
        }
        self.mtime[i] = minutes(c.new.mtime);
        let keep = self.flags[i] & (nflags::DUP_LINK | nflags::DELETED | nflags::RENAMED | nflags::SYNTHETIC);
        self.flags[i] = keep | (c.new.flags & !rflags::IN_USE) | nflags::from_attrs(c.new.attrs);
    }

    fn contrib(&self, n: u32) -> Contrib {
        let i = n as usize;
        let f = self.flags[i];
        if f & (nflags::DUP_LINK | nflags::DELETED) != 0 {
            (0, 0, 0, 0)
        } else if f & nflags::DIR != 0 {
            (self.size[i] as i64, self.alloc[i] as i64, self.files[i] as i64, self.dirs[i] as i64 + 1)
        } else {
            (self.size[i] as i64, self.alloc[i] as i64, 1, 0)
        }
    }

    /// Add `d` to `from` and its ancestors, stopping after a deleted one.
    fn adjust_up(&mut self, from: u32, d: Contrib) {
        let mut p = from;
        while p != NONE {
            let i = p as usize;
            self.size[i] = self.size[i].saturating_add_signed(d.0);
            self.alloc[i] = self.alloc[i].saturating_add_signed(d.1);
            self.files[i] = (self.files[i] as i64 + d.2).clamp(0, u32::MAX as i64) as u32;
            self.dirs[i] = (self.dirs[i] as i64 + d.3).clamp(0, u32::MAX as i64) as u32;
            if self.flags[i] & nflags::DELETED != 0 {
                break;
            }
            p = self.parent[i];
        }
    }

    /// Flag `n` as deleted and take it out of every total. It stays listed under its
    /// parent ([`Index::children_all`]). Used by patches and for deletions made from the
    /// UI on trees without a journal.
    pub fn delete_node(&mut self, n: u32, types: &mut FileTypes) {
        if n == 0 || self.is_deleted(n) {
            return;
        }
        types.sub(self, n);
        let c = self.contrib(n);
        self.flags[n as usize] |= nflags::DELETED;
        self.adjust_up(self.parent[n as usize], neg(c));
        self.revision += 1;
    }

    fn link(&mut self, n: u32, q: u32) {
        let (a, b) = (self.first_child[q as usize], self.first_child[q as usize + 1]);
        if !(a..b).contains(&n) {
            let e = self.extra.entry(q).or_default();
            if !e.contains(&n) {
                e.push(n);
            }
        }
    }

    fn move_node(&mut self, n: u32, q: u32) {
        let c = self.contrib(n);
        let old = self.parent[n as usize];
        self.adjust_up(old, neg(c));
        self.parent[n as usize] = q;
        self.moved = true;
        self.link(n, q);
        self.adjust_up(q, c);
    }

    /// Room for more nodes in small steps: the arrays were built exactly to size, and a
    /// doubling `push` would commit as much memory again as the whole tree.
    fn reserve_nodes(&mut self) {
        if self.parent.len() < self.parent.capacity() {
            return;
        }
        let k = (self.len() / 512).max(256);
        self.parent.reserve_exact(k);
        self.first_child.reserve_exact(k);
        self.name_off.reserve_exact(k);
        self.names.reserve_exact(k * 24);
        self.size.reserve_exact(k);
        self.alloc.reserve_exact(k);
        self.mtime.reserve_exact(k);
        self.flags.reserve_exact(k);
        self.files.reserve_exact(k);
        self.dirs.reserve_exact(k);
    }

    fn add_node(&mut self, q: u32, r: &Rec, name: &str, like: Option<u32>) -> u32 {
        self.reserve_nodes();
        if self.names.len() + name.len() > self.names.capacity() {
            self.names.reserve_exact(name.len() + (self.names.len() / 512).max(4096));
        }
        let n = self.len() as u32;
        let end = *self.first_child.last().unwrap_or(&0);
        self.parent.push(q);
        self.first_child.push(end);
        self.names.extend_from_slice(name.as_bytes());
        self.name_off.push(self.names.len() as u32);
        self.size.push(if r.is_dir() { 0 } else { r.size });
        self.alloc.push(r.alloc);
        self.mtime.push(minutes(r.mtime));
        self.flags.push((r.flags & !rflags::IN_USE) | nflags::from_attrs(r.attrs));
        match like.filter(|_| r.is_dir()) {
            Some(o) => {
                let o = o as usize;
                let (s, a, f, d) = (self.size[o], self.alloc[o], self.files[o], self.dirs[o]);
                self.size[n as usize] = s;
                self.alloc[n as usize] = a;
                self.files.push(f);
                self.dirs.push(d);
            }
            None => {
                self.files.push(0);
                self.dirs.push(0);
            }
        }
        self.link(n, q);
        let c = self.contrib(n);
        self.adjust_up(q, c);
        n
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, HashMap};

    use ft_ntfs::NameEntry;

    use super::*;

    /// A tiny volume: records with one name each (or none when free).
    #[derive(Clone)]
    struct Vol {
        recs: Vec<Rec>,
        names: Vec<Option<(u32, String)>>,
    }

    impl Vol {
        fn new() -> Vol {
            let mut v = Vol { recs: vec![Rec::default(); 16], names: vec![None; 16] };
            v.recs[5] = Rec { flags: rflags::IN_USE | rflags::DIR, seq: 1, alloc: 4096, ..Default::default() };
            v.names[5] = Some((5, ".".into()));
            v
        }

        fn free(&self) -> Option<u32> {
            (16..self.recs.len() as u32).find(|&r| !self.recs[r as usize].in_use())
        }

        fn create(&mut self, parent: u32, name: &str, dir: bool, size: u64, reuse: bool) -> u32 {
            let r = match self.free().filter(|_| reuse) {
                Some(r) => r,
                None => {
                    self.recs.push(Rec::default());
                    self.names.push(None);
                    self.recs.len() as u32 - 1
                }
            };
            let seq = self.recs[r as usize].seq + 1;
            let flags = rflags::IN_USE | if dir { rflags::DIR } else { 0 };
            let alloc = if dir { 4096 } else { size.div_ceil(4096) * 4096 };
            self.recs[r as usize] = Rec { size: if dir { 0 } else { size }, alloc, flags, seq, mtime: 600_000_000 * r as u64, attrs: 0 };
            self.names[r as usize] = Some((parent, name.into()));
            r
        }

        fn delete(&mut self, r: u32) {
            let seq = self.recs[r as usize].seq;
            self.recs[r as usize] = Rec { seq, ..Default::default() };
            self.names[r as usize] = None;
        }

        fn entries(&self) -> (Vec<NameEntry>, Vec<u8>) {
            let (mut names, mut arena) = (Vec::new(), Vec::new());
            for (r, n) in self.names.iter().enumerate() {
                if let Some((p, s)) = n {
                    let ps = self.recs[*p as usize].seq;
                    names.push(NameEntry { record: r as u32, parent: *p, off: arena.len() as u32, len: s.len() as u16, parent_seq: ps });
                    arena.extend_from_slice(s.as_bytes());
                }
            }
            (names, arena)
        }

        fn build(&self) -> (Index, FileTypes) {
            let (names, arena) = self.entries();
            let (ix, _) = Index::build(&self.recs, &names, &arena, "C:").unwrap();
            let t = FileTypes::build(&ix);
            (ix, t)
        }

        fn named(&self, r: u32) -> Vec<Name> {
            self.names
                .get(r as usize)
                .cloned()
                .flatten()
                .into_iter()
                .map(|(p, s)| {
                    let pr = &self.recs[p as usize];
                    Name { parent: p, name: s.into(), valid: pr.in_use() && pr.is_dir() }
                })
                .collect()
        }

        fn rec(&self, r: u32) -> Rec {
            self.recs.get(r as usize).copied().unwrap_or_default()
        }

        /// Changes for `touched` between `before` and `self`.
        fn changes(&self, before: &Vol, touched: &[u32]) -> Vec<Change> {
            let mut t = touched.to_vec();
            t.sort_unstable();
            t.dedup();
            t.into_iter()
                .map(|r| Change { record: r, old: before.rec(r), new: self.rec(r), old_names: before.named(r), new_names: self.named(r) })
                .filter(|c| !c.is_noop())
                .collect()
        }
    }

    /// path -> (size, alloc, files, dirs, mtime, flags of interest), live nodes only.
    fn snapshot(ix: &Index) -> BTreeMap<String, (u64, u64, u32, u32, u32, u16)> {
        let mut m = BTreeMap::new();
        let mut stack = vec![0u32];
        while let Some(n) = stack.pop() {
            let i = n as usize;
            let f = ix.flags[i] & (nflags::DIR | nflags::HIDDEN | nflags::SYSTEM);
            m.insert(ix.path(n), (ix.size[i], ix.alloc[i], ix.files[i], ix.dirs[i], ix.mtime[i], f));
            stack.extend(ix.children(n));
        }
        m
    }

    fn type_totals(ix: &Index, t: &FileTypes) -> HashMap<String, (u64, u64, u64)> {
        let _ = ix;
        t.list.iter().filter(|s| s.files > 0).map(|s| (s.ext.clone(), (s.size, s.alloc, s.files))).collect()
    }

    fn assert_same(v: &Vol, ix: &Index, t: &FileTypes, step: usize) {
        let (fresh, ft) = v.build();
        assert_eq!(snapshot(ix), snapshot(&fresh), "tree differs after step {step}");
        assert_eq!(type_totals(ix, t), type_totals(&fresh, &ft), "types differ after step {step}");
        ix.check().unwrap();
    }

    #[test]
    fn create_rename_move_resize_delete() {
        let mut v = Vol::new();
        let docs = v.create(5, "docs", true, 0, false);
        let a = v.create(docs, "a.txt", false, 100, false);
        let (mut ix, mut t) = v.build();

        let before = v.clone();
        let sub = v.create(docs, "sub", true, 0, false);
        let b = v.create(sub, "b.mp4", false, 5000, false);
        v.names[a as usize] = Some((sub, "renamed.TXT".into()));
        v.recs[docs as usize].alloc = 8192;
        let ch = v.changes(&before, &[sub, b, a, docs]);
        let st = ix.patch(&ch, &mut t).unwrap();
        assert_eq!((st.created, st.moved, st.renamed, st.unresolved), (2, 1, 1, 0));
        assert_same(&v, &ix, &t, 1);
        assert_eq!(ix.name(ix.find(r"docs\sub\renamed.TXT").unwrap()), "renamed.TXT");

        let before = v.clone();
        v.recs[b as usize].size = 10;
        v.recs[b as usize].attrs = 0x2;
        let ch = v.changes(&before, &[b]);
        ix.patch(&ch, &mut t).unwrap();
        assert_same(&v, &ix, &t, 2);

        // Deleting the folder before its files (journal order is not guaranteed).
        let before = v.clone();
        let sub_node = ix.find(r"docs\sub").unwrap();
        v.delete(sub);
        v.delete(b);
        v.delete(a);
        let ch = v.changes(&before, &[sub, b, a]);
        let st = ix.patch(&ch, &mut t).unwrap();
        assert_eq!(st.deleted, 3);
        assert_same(&v, &ix, &t, 3);
        assert!(ix.is_deleted(sub_node));
        assert!(ix.children_all(ix.find("docs").unwrap()).any(|c| c == sub_node), "still listed");
    }

    #[test]
    fn record_reuse_and_unknown_parent() {
        let mut v = Vol::new();
        let d = v.create(5, "d", true, 0, false);
        let f = v.create(d, "f.bin", false, 10, false);
        let (mut ix, mut t) = v.build();
        let before = v.clone();
        v.delete(f);
        let g = v.create(5, "g.dat", false, 77, true);
        assert_eq!(g, f, "the record is reused with a new sequence number");
        let ch = v.changes(&before, &[f]);
        ix.patch(&ch, &mut t).unwrap();
        assert_same(&v, &ix, &t, 1);

        // A name pointing at a parent that is not in the tree: left for a rebuild.
        let ghost = Change {
            record: 999,
            old: Rec::default(),
            new: Rec { flags: rflags::IN_USE, seq: 1, size: 1, ..Default::default() },
            old_names: vec![],
            new_names: vec![Name { parent: 4242, name: "x".into(), valid: true }],
        };
        assert_eq!(ix.patch(&[ghost], &mut t).unwrap().unresolved, 1);

        let links = Change {
            record: d,
            old: v.rec(d),
            new: v.rec(d),
            old_names: v.named(d),
            new_names: vec![v.named(d)[0].clone(), Name { parent: 5, name: "d2".into(), valid: true }],
        };
        assert_eq!(ix.patch(&[links], &mut t), Err(PatchError::HardLinks));
    }

    #[test]
    fn deleted_from_ui_then_moved_to_recycle_bin() {
        let mut v = Vol::new();
        let bin = v.create(5, "$Recycle.Bin", true, 0, false);
        let d = v.create(5, "old", true, 0, false);
        v.create(d, "x.iso", false, 1 << 30, false);
        let (mut ix, mut t) = v.build();
        let node = ix.find("old").unwrap();
        ix.delete_node(node, &mut t);
        let before = v.clone();
        v.names[d as usize] = Some((bin, "$R1234".into()));
        let ch = v.changes(&before, &[d]);
        ix.patch(&ch, &mut t).unwrap();
        assert!(ix.is_deleted(node), "the struck-through row stays where it was");
        let moved = ix.find(r"$Recycle.Bin\$R1234").unwrap();
        assert_ne!(moved, node);
        assert_eq!(ix.size[ix.find("$Recycle.Bin").unwrap() as usize], 1 << 30, "the bin holds the folder's size");
        assert_eq!(ix.size[0], 1 << 30);
    }

    /// Random batches against a model; after each, the patched tree must equal a fresh
    /// build exactly.
    #[test]
    fn random_batches_match_a_rebuild() {
        let mut seed = 0x9E37_79B9_7F4A_7C15u64;
        let mut rnd = move |n: usize| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed % n.max(1) as u64) as usize
        };
        let mut v = Vol::new();
        let mut dirs = vec![5u32];
        for i in 0..30 {
            let p = dirs[rnd(dirs.len())];
            let r = v.create(p, &format!("d{i}"), true, 0, false);
            dirs.push(r);
        }
        for i in 0..200 {
            let p = dirs[rnd(dirs.len())];
            v.create(p, &format!("f{i}.{}", ["txt", "bin", "mp4", "x"][i % 4]), false, rnd(1 << 20) as u64, false);
        }
        let (mut ix, mut t) = v.build();
        let mut uid = 1000;
        for step in 0..300 {
            let before = v.clone();
            let mut touched = Vec::new();
            for _ in 0..1 + rnd(8) {
                let live: Vec<u32> = (16..v.recs.len() as u32).filter(|&r| v.recs[r as usize].in_use()).collect();
                let live_dirs: Vec<u32> = std::iter::once(5).chain(live.iter().copied().filter(|&r| v.recs[r as usize].is_dir())).collect();
                let r = live[rnd(live.len())];
                uid += 1;
                match rnd(7) {
                    0 | 1 => {
                        let dir = rnd(4) == 0;
                        let p = live_dirs[rnd(live_dirs.len())];
                        let ext = ["txt", "mp4", "bin", "new", "Q"][rnd(5)];
                        let n = v.create(p, &format!("n{uid}.{ext}"), dir, rnd(1 << 16) as u64, rnd(2) == 0);
                        touched.push(n);
                    }
                    2 => {
                        // Delete a file, or an empty directory.
                        let busy = v.names.iter().any(|x| x.as_ref().is_some_and(|(p, _)| *p == r));
                        if !busy {
                            v.delete(r);
                            touched.push(r);
                        }
                    }
                    3 => {
                        let p = v.names[r as usize].as_ref().unwrap().0;
                        v.names[r as usize] = Some((p, format!("r{uid}.{}", ["txt", "mkv", ""][rnd(3)])));
                        touched.push(r);
                    }
                    4 => {
                        // Move, never into its own subtree.
                        let q = live_dirs[rnd(live_dirs.len())];
                        let mut x = q;
                        let mut inside = false;
                        while x != 5 {
                            if x == r {
                                inside = true;
                                break;
                            }
                            x = v.names[x as usize].as_ref().unwrap().0;
                        }
                        if !inside && q != r {
                            let name = v.names[r as usize].as_ref().unwrap().1.clone();
                            v.names[r as usize] = Some((q, name));
                            touched.push(r);
                        }
                    }
                    5 => {
                        let rec = &mut v.recs[r as usize];
                        if rec.is_dir() {
                            rec.alloc = rnd(64) as u64 * 4096;
                        } else {
                            rec.size = rnd(1 << 24) as u64;
                            rec.alloc = rec.size.div_ceil(4096) * 4096;
                        }
                        rec.mtime += 600_000_000;
                        touched.push(r);
                    }
                    _ => {
                        v.recs[r as usize].attrs ^= 0x2;
                        touched.push(r);
                    }
                }
            }
            let ch = v.changes(&before, &touched);
            let st = ix.patch(&ch, &mut t).unwrap();
            assert_eq!(st.unresolved, 0, "step {step}");
            assert_same(&v, &ix, &t, step);
        }
    }
}
