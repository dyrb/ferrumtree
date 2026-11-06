//! Rows of the tree list: a flattened view of the expanded part of the index.
//!
//! Only expanded directories contribute rows. A directory holding both folders and files
//! shows its files behind one group row ("[N files]"), so folder lists
//! stay readable; a directory with only files lists them directly. Group totals are
//! computed once, when the row is created, and cached.

use std::cmp::Ordering;
use std::collections::HashMap;

use ft_index::{Index, NONE, nflags};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Key {
    Name,
    Size,
    Alloc,
    Items,
    Files,
    Folders,
    Modified,
    Folder,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Sort {
    pub key: Key,
    pub desc: bool,
}

impl Default for Sort {
    fn default() -> Self {
        Sort { key: Key::Size, desc: true }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Node,
    /// The files of directory `node`, grouped.
    Files,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Row {
    pub node: u32,
    pub depth: u16,
    pub kind: Kind,
    pub expanded: bool,
}

#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct Vals {
    pub size: u64,
    pub alloc: u64,
    pub items: u64,
    pub files: u64,
    pub folders: u64,
    pub mtime: u64,
}

pub fn node_vals(ix: &Index, n: u32) -> Vals {
    let i = n as usize;
    let (files, folders) = if ix.is_dir(n) { (ix.files[i] as u64, ix.dirs[i] as u64) } else { (0, 0) };
    Vals { size: ix.size[i], alloc: ix.alloc[i], items: files + folders, files, folders, mtime: ix.mtime_filetime(n) }
}

fn cmp_vals(a: &Vals, b: &Vals, key: Key) -> Ordering {
    match key {
        Key::Size => a.size.cmp(&b.size),
        Key::Alloc => a.alloc.cmp(&b.alloc),
        Key::Items => a.items.cmp(&b.items),
        Key::Files => a.files.cmp(&b.files),
        Key::Folders => a.folders.cmp(&b.folders),
        Key::Modified => a.mtime.cmp(&b.mtime),
        Key::Name | Key::Folder => Ordering::Equal,
    }
}

fn name_cmp(a: &str, b: &str) -> Ordering {
    a.bytes().map(|c| c.to_ascii_lowercase()).cmp(b.bytes().map(|c| c.to_ascii_lowercase()))
}

/// Sort a flat node list (Files tab, search results).
pub fn sort_nodes(ix: &Index, nodes: &mut [u32], s: Sort) {
    let cmp = |a: &u32, b: &u32| -> Ordering {
        let o = match s.key {
            Key::Name => name_cmp(ix.name(*a), ix.name(*b)),
            Key::Folder => {
                let (pa, pb) = (ix.parent[*a as usize], ix.parent[*b as usize]);
                if pa == pb { Ordering::Equal } else { name_cmp(&ix.path(pa), &ix.path(pb)) }
            }
            k => cmp_vals(&node_vals(ix, *a), &node_vals(ix, *b), k),
        };
        let o = if s.desc { o.reverse() } else { o };
        o.then_with(|| name_cmp(ix.name(*a), ix.name(*b)))
    };
    if matches!(s.key, Key::Size | Key::Alloc) && s.desc {
        // The common case, on up to millions of rows: a plain key sort.
        let m = if s.key == Key::Size { &ix.size } else { &ix.alloc };
        nodes.sort_unstable_by_key(|&n| std::cmp::Reverse(m[n as usize]));
    } else {
        nodes.sort_unstable_by(cmp);
    }
}

#[derive(Default)]
pub struct Tree {
    pub rows: Vec<Row>,
    pub root: u32,
    agg: HashMap<u32, Vals>,
}

impl Tree {
    /// `root`, expanded one level.
    pub fn new(ix: &Index, sort: Sort, root: u32) -> Tree {
        let mut t = Tree { rows: vec![Row { node: root, depth: 0, kind: Kind::Node, expanded: false }], root, agg: HashMap::new() };
        if !ix.is_empty() {
            t.expand(ix, sort, 0);
        }
        t
    }

    pub fn expandable(ix: &Index, r: &Row) -> bool {
        match r.kind {
            Kind::Files => true,
            Kind::Node => ix.is_dir(r.node) && ix.flags[r.node as usize] & nflags::DUP_LINK == 0 && ix.has_children(r.node),
        }
    }

    pub fn vals(&self, ix: &Index, r: &Row) -> Vals {
        match r.kind {
            Kind::Node => node_vals(ix, r.node),
            Kind::Files => self.agg.get(&r.node).copied().unwrap_or_default(),
        }
    }

    /// The directory whose total a row's percentage refers to.
    pub fn parent_dir(ix: &Index, r: &Row) -> u32 {
        match r.kind {
            Kind::Files => r.node,
            Kind::Node => ix.parent[r.node as usize],
        }
    }

    /// Totals of the group row: current files only (deleted ones are listed, not counted).
    fn files_vals(ix: &Index, dir: u32) -> Vals {
        let mut v = Vals::default();
        for c in ix.children(dir).filter(|&c| !ix.is_dir(c)) {
            let i = c as usize;
            if ix.flags[i] & nflags::DUP_LINK == 0 {
                v.size += ix.size[i];
                v.alloc += ix.alloc[i];
                v.files += 1;
            }
            v.mtime = v.mtime.max(ix.mtime_filetime(c));
        }
        v.items = v.files;
        v
    }

    fn sort_rows(&self, ix: &Index, rows: &mut [Row], s: Sort) {
        rows.sort_by(|a, b| {
            let o = match s.key {
                Key::Name | Key::Folder => match (a.kind, b.kind) {
                    (Kind::Files, Kind::Node) => Ordering::Less,
                    (Kind::Node, Kind::Files) => Ordering::Greater,
                    _ => name_cmp(ix.name(a.node), ix.name(b.node)),
                },
                k => cmp_vals(&self.vals(ix, a), &self.vals(ix, b), k),
            };
            let o = if s.desc { o.reverse() } else { o };
            o.then_with(|| name_cmp(ix.name(a.node), ix.name(b.node)))
        });
    }

    /// Insert the children of row `i`. Returns the number of rows added.
    pub fn expand(&mut self, ix: &Index, s: Sort, i: usize) -> usize {
        let r = self.rows[i];
        if r.expanded || !Self::expandable(ix, &r) {
            return 0;
        }
        let d = r.depth + 1;
        let row = |n: u32| Row { node: n, depth: d, kind: Kind::Node, expanded: false };
        let mut new: Vec<Row> = match r.kind {
            Kind::Files => ix.children_all(r.node).filter(|&c| !ix.is_dir(c)).map(row).collect(),
            Kind::Node => {
                let (dirs, files): (Vec<u32>, Vec<u32>) = ix.children_all(r.node).partition(|&c| ix.is_dir(c));
                if !dirs.is_empty() && !files.is_empty() {
                    let v = Self::files_vals(ix, r.node);
                    self.agg.insert(r.node, v);
                    let mut rows: Vec<Row> = dirs.into_iter().map(row).collect();
                    rows.push(Row { node: r.node, depth: d, kind: Kind::Files, expanded: false });
                    rows
                } else {
                    dirs.into_iter().chain(files).map(row).collect()
                }
            }
        };
        self.sort_rows(ix, &mut new, s);
        let n = new.len();
        self.rows[i].expanded = true;
        self.rows.splice(i + 1..i + 1, new);
        n
    }

    /// Remove every row below row `i`. Returns the number of rows removed.
    pub fn collapse(&mut self, i: usize) -> usize {
        let d = self.rows[i].depth;
        self.rows[i].expanded = false;
        let end = self.rows[i + 1..].iter().position(|r| r.depth <= d).map_or(self.rows.len(), |p| i + 1 + p);
        self.rows.drain(i + 1..end);
        end - i - 1
    }

    pub fn toggle(&mut self, ix: &Index, s: Sort, i: usize) {
        if self.rows[i].expanded {
            self.collapse(i);
        } else {
            self.expand(ix, s, i);
        }
    }

    /// Expand row `i` and everything below it (bounded by `limit` rows in total).
    pub fn expand_all(&mut self, ix: &Index, s: Sort, i: usize, limit: usize) {
        let d = self.rows[i].depth;
        let mut j = i;
        while j < self.rows.len() && (j == i || self.rows[j].depth > d) && self.rows.len() < limit {
            self.expand(ix, s, j);
            j += 1;
        }
    }

    pub fn parent_row(&self, i: usize) -> Option<usize> {
        let d = self.rows[i].depth;
        (0..i).rev().find(|&j| self.rows[j].depth < d)
    }

    fn child_row(&self, i: usize, pred: impl Fn(&Row) -> bool) -> Option<usize> {
        let d = self.rows[i].depth;
        (i + 1..self.rows.len()).take_while(|&j| self.rows[j].depth > d).find(|&j| self.rows[j].depth == d + 1 && pred(&self.rows[j]))
    }

    /// Expand the ancestors of `node` (below the tree root) and return its row.
    pub fn reveal(&mut self, ix: &Index, s: Sort, node: u32) -> Option<usize> {
        let mut chain = vec![node];
        let mut n = node;
        while n != self.root {
            n = ix.parent[n as usize];
            if n == NONE {
                return None;
            }
            chain.push(n);
        }
        chain.reverse();
        let mut row = 0;
        for &next in &chain[1..] {
            self.expand(ix, s, row);
            row = match self.child_row(row, |r| r.kind == Kind::Node && r.node == next) {
                Some(j) => j,
                None => {
                    let g = self.child_row(row, |r| r.kind == Kind::Files)?;
                    self.expand(ix, s, g);
                    self.child_row(g, |r| r.node == next)?
                }
            };
        }
        Some(row)
    }

    /// Expanded rows as (path, kind), to survive an index rebuild or a re-sort.
    pub fn expanded_state(&self, ix: &Index) -> Vec<(String, Kind)> {
        self.rows.iter().filter(|r| r.expanded).map(|r| (ix.path(r.node), r.kind)).collect()
    }

    pub fn restore(ix: &Index, s: Sort, root: u32, state: &[(String, Kind)]) -> Tree {
        let nodes: Vec<(u32, Kind)> = state.iter().filter_map(|(p, k)| ix.find(p).map(|n| (n, *k))).collect();
        Self::restore_nodes(ix, s, root, &nodes)
    }

    /// Expanded rows by node: node numbers survive patches of the index.
    pub fn expanded_nodes(&self) -> Vec<(u32, Kind)> {
        self.rows.iter().filter(|r| r.expanded).map(|r| (r.node, r.kind)).collect()
    }

    /// Rows with the given nodes expanded (those still below `root`).
    pub fn restore_nodes(ix: &Index, s: Sort, root: u32, state: &[(u32, Kind)]) -> Tree {
        let mut t = Tree { rows: vec![Row { node: root, depth: 0, kind: Kind::Node, expanded: false }], root, agg: HashMap::new() };
        for &(n, kind) in state {
            if n as usize >= ix.len() {
                continue;
            }
            let Some(i) = t.reveal(ix, s, n) else { continue };
            match kind {
                Kind::Node => {
                    t.expand(ix, s, i);
                }
                Kind::Files => {
                    t.expand(ix, s, i);
                    if let Some(g) = t.child_row(i, |r| r.kind == Kind::Files) {
                        t.expand(ix, s, g);
                    }
                }
            }
        }
        if !t.rows[0].expanded {
            t.expand(ix, s, 0);
        }
        t
    }

    pub fn row_of(&self, node: u32) -> Option<usize> {
        self.rows.iter().position(|r| r.kind == Kind::Node && r.node == node)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ft_ntfs::{NameEntry, Rec, rflags};

    fn index() -> Index {
        let mut recs = vec![Rec::default(); 40];
        let mut names = Vec::new();
        let mut arena = Vec::new();
        let mut add = |r: u32, parent: u32, name: &str, dir: bool, size: u64| {
            let flags = rflags::IN_USE | if dir { rflags::DIR } else { 0 };
            recs[r as usize] = Rec { size, alloc: size, flags, seq: 1, ..Default::default() };
            names.push(NameEntry { record: r, parent, off: arena.len() as u32, len: name.len() as u16, parent_seq: 1 });
            arena.extend_from_slice(name.as_bytes());
        };
        add(5, 5, ".", true, 0);
        add(16, 5, "small", true, 0);
        add(17, 5, "big", true, 0);
        add(18, 17, "x.bin", false, 900);
        add(19, 17, "deep", true, 0);
        add(20, 19, "y.bin", false, 50);
        add(21, 16, "z.bin", false, 10);
        add(22, 5, "top.bin", false, 500);
        add(23, 5, "top2.bin", false, 5);
        Index::build(&recs, &names, &arena, "C:").unwrap().0
    }

    fn show(ix: &Index, t: &Tree) -> Vec<String> {
        t.rows
            .iter()
            .map(|r| {
                let n = match r.kind {
                    Kind::Node => ix.name(r.node).to_string(),
                    Kind::Files => format!("[files of {}]", ix.name(r.node)),
                };
                format!("{}{}", "  ".repeat(r.depth as usize), n)
            })
            .collect()
    }

    #[test]
    fn grouping_and_order() {
        let ix = index();
        let s = Sort::default();
        let mut t = Tree::new(&ix, s, 0);
        // root has folders and files: files go behind a group row (505 bytes)
        assert_eq!(show(&ix, &t), ["C:", "  big", "  [files of C:]", "  small"]);
        let g = t.rows.iter().position(|r| r.kind == Kind::Files).unwrap();
        assert_eq!(t.vals(&ix, &t.rows[g]).size, 505);
        assert_eq!(t.vals(&ix, &t.rows[g]).files, 2);
        t.expand(&ix, s, g);
        assert_eq!(&show(&ix, &t)[3..5], ["    top.bin", "    top2.bin"]);
        // "small" holds only files: listed directly
        let small = t.row_of(ix.find("small").unwrap()).unwrap();
        t.expand(&ix, s, small);
        assert_eq!(show(&ix, &t).last().unwrap(), "    z.bin");
    }

    #[test]
    fn sort_by_name_ascending() {
        let ix = index();
        let t = Tree::new(&ix, Sort { key: Key::Name, desc: false }, 0);
        assert_eq!(show(&ix, &t), ["C:", "  [files of C:]", "  big", "  small"]);
    }

    #[test]
    fn reveal_through_groups_and_restore() {
        let ix = index();
        let s = Sort::default();
        let mut t = Tree::new(&ix, s, 0);
        let x = ix.find(r"big\x.bin").unwrap();
        let row = t.reveal(&ix, s, x).unwrap();
        assert_eq!(t.rows[row].node, x);
        let top = ix.find("top2.bin").unwrap();
        let row = t.reveal(&ix, s, top).unwrap();
        assert_eq!(t.rows[row].node, top);
        let state = t.expanded_state(&ix);
        let t2 = Tree::restore(&ix, s, 0, &state);
        assert_eq!(t2.rows, t.rows);
    }

    #[test]
    fn subtree_root_and_expand_all() {
        let ix = index();
        let s = Sort::default();
        let big = ix.find("big").unwrap();
        let mut t = Tree::new(&ix, s, big);
        assert_eq!(show(&ix, &t), ["big", "  [files of big]", "  deep"]);
        t.expand_all(&ix, s, 0, 1000);
        assert!(t.rows.iter().any(|r| r.node == ix.find(r"big\deep\y.bin").unwrap()));
        assert!(t.reveal(&ix, s, ix.find("small").unwrap()).is_none(), "outside the root");
    }

    #[test]
    fn flat_sort() {
        let ix = index();
        let mut v: Vec<u32> = (0..ix.len() as u32).filter(|&n| !ix.is_dir(n)).collect();
        sort_nodes(&ix, &mut v, Sort::default());
        assert_eq!(ix.name(v[0]), "x.bin");
        sort_nodes(&ix, &mut v, Sort { key: Key::Name, desc: false });
        assert_eq!(ix.name(v[0]), "top.bin");
    }
}
