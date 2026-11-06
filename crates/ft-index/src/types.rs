//! File types: per-extension totals, and the extension of every node.
//!
//! `of_node[n]` is an index into `list` (sorted by total size, largest first), so colouring
//! a treemap tile or testing "is this a .mp4" is one array read. Secondary hard links get
//! their extension for colouring but are not added to the totals (counted once).

use crate::{FnvMap, Index, nflags};

/// Directories, synthetic nodes.
pub const NOT_FILE: u16 = u16::MAX;
/// Longest extension taken as such; longer suffixes count as "no extension".
const MAX_EXT: usize = 16;
/// Distinct extensions kept; the rest share the "no extension" bucket.
const MAX_TYPES: usize = 60_000;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ExtStat {
    /// Lower-case, without the dot; empty for files without an extension.
    pub ext: String,
    pub size: u64,
    pub alloc: u64,
    pub files: u64,
}

/// `list` is sorted by size when built; extensions that appear later (patches) are
/// appended, so ids never change.
#[derive(Default)]
pub struct FileTypes {
    pub of_node: Vec<u16>,
    pub list: Vec<ExtStat>,
    /// Extension -> id, built on the first patch.
    ids: Option<FnvMap<Box<[u8]>, u16>>,
}

/// The extension of a file name, lower-cased into `buf` (`None` if it has none).
fn extension<'a>(name: &str, buf: &'a mut Vec<u8>) -> Option<&'a [u8]> {
    let b = name.as_bytes();
    let p = b.iter().rposition(|&c| c == b'.')?;
    if p == 0 || p + 1 >= b.len() || b.len() - p - 1 > MAX_EXT * 4 {
        return None;
    }
    let ext = &name[p + 1..];
    buf.clear();
    if ext.is_ascii() {
        buf.extend(ext.bytes().map(|c| c.to_ascii_lowercase()));
    } else {
        buf.extend_from_slice(ext.to_lowercase().as_bytes());
    }
    (ext.chars().count() <= MAX_EXT && !ext.contains(' ')).then_some(&buf[..])
}

impl FileTypes {
    pub fn build(ix: &Index) -> FileTypes {
        let n = ix.len();
        let mut of_node = vec![NOT_FILE; n];
        let mut ids: FnvMap<Box<[u8]>, u16> = FnvMap::default();
        let mut list: Vec<ExtStat> = vec![ExtStat::default()]; // id 0: no extension
        let mut buf = Vec::with_capacity(32);
        for (i, slot) in of_node.iter_mut().enumerate() {
            let f = ix.flags[i];
            if f & (nflags::DIR | nflags::SYNTHETIC) != 0 {
                continue;
            }
            let id = match extension(ix.name(i as u32), &mut buf) {
                None => 0,
                Some(e) => match ids.get(e) {
                    Some(&id) => id,
                    None if list.len() < MAX_TYPES => {
                        let id = list.len() as u16;
                        ids.insert(e.into(), id);
                        list.push(ExtStat { ext: String::from_utf8_lossy(e).into_owned(), ..Default::default() });
                        id
                    }
                    None => 0,
                },
            };
            *slot = id;
            if f & nflags::DUP_LINK == 0 {
                let s = &mut list[id as usize];
                s.size += ix.size[i];
                s.alloc += ix.alloc[i];
                s.files += 1;
            }
        }
        // Largest first; remap the node ids accordingly.
        let mut order: Vec<u16> = (0..list.len() as u16).collect();
        order.sort_unstable_by(|&a, &b| list[b as usize].size.cmp(&list[a as usize].size).then(a.cmp(&b)));
        let mut remap = vec![0u16; list.len()];
        for (new, &old) in order.iter().enumerate() {
            remap[old as usize] = new as u16;
        }
        for id in of_node.iter_mut().filter(|id| **id != NOT_FILE) {
            *id = remap[*id as usize];
        }
        // Every id stays (an extension seen only on duplicate links has 0 files) so that
        // `of_node` keeps pointing at the right entry.
        let list = order.iter().map(|&o| std::mem::take(&mut list[o as usize])).collect();
        FileTypes { of_node, list, ids: None }
    }

    fn counts(ix: &Index, n: u32) -> bool {
        ix.flags[n as usize] & (nflags::DIR | nflags::SYNTHETIC | nflags::DUP_LINK | nflags::DELETED) == 0
    }

    /// Take node `n` out of its extension's totals (before a patch changes it).
    pub(crate) fn sub(&mut self, ix: &Index, n: u32) {
        let id = self.of_node.get(n as usize).copied().unwrap_or(NOT_FILE);
        if id == NOT_FILE || !Self::counts(ix, n) {
            return;
        }
        let (i, s) = (n as usize, &mut self.list[id as usize]);
        s.size = s.size.saturating_sub(ix.size[i]);
        s.alloc = s.alloc.saturating_sub(ix.alloc[i]);
        s.files = s.files.saturating_sub(1);
    }

    /// Classify node `n` by its current name and add it to the totals (after a patch).
    pub(crate) fn add(&mut self, ix: &Index, n: u32) {
        if self.of_node.len() < ix.len() {
            if self.of_node.capacity() < ix.len() {
                self.of_node.reserve_exact((ix.len() - self.of_node.len()).max(ix.len() / 512).max(256));
            }
            self.of_node.resize(ix.len(), NOT_FILE);
        }
        if ix.flags[n as usize] & (nflags::DIR | nflags::SYNTHETIC) != 0 {
            self.of_node[n as usize] = NOT_FILE;
            return;
        }
        let list = &mut self.list;
        let ids = self.ids.get_or_insert_with(|| list.iter().enumerate().map(|(i, s)| (s.ext.as_bytes().into(), i as u16)).collect());
        let mut buf = Vec::with_capacity(32);
        let key: &[u8] = extension(ix.name(n), &mut buf).unwrap_or(b"");
        let id = match ids.get(key) {
            Some(&id) => id,
            None if list.len() < MAX_TYPES => {
                let id = list.len() as u16;
                ids.insert(key.into(), id);
                list.push(ExtStat { ext: String::from_utf8_lossy(key).into_owned(), ..Default::default() });
                id
            }
            None => ids.get(&b""[..]).copied().unwrap_or(0),
        };
        self.of_node[n as usize] = id;
        if Self::counts(ix, n) {
            let (i, s) = (n as usize, &mut self.list[id as usize]);
            s.size += ix.size[i];
            s.alloc += ix.alloc[i];
            s.files += 1;
        }
    }

    pub fn heap_bytes(&self) -> usize {
        self.of_node.capacity() * 2 + self.list.iter().map(|s| s.ext.capacity() + size_of::<ExtStat>()).sum::<usize>()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ft_ntfs::{NameEntry, Rec, rflags};

    #[test]
    fn extensions_and_totals() {
        let files = [
            ("Movie.MKV", 900u64),
            ("b.mkv", 100),
            ("readme", 5),
            (".gitignore", 7),
            ("archive.tar.gz", 50),
            ("Отчёт.XLSX", 3),
            ("weird.name with space", 1),
        ];
        let mut recs = vec![Rec::default(); 16 + files.len()];
        recs[5] = Rec { flags: rflags::IN_USE | rflags::DIR, seq: 1, ..Default::default() };
        let mut names = Vec::new();
        let mut arena = Vec::new();
        for (i, (n, sz)) in files.iter().enumerate() {
            let r = 16 + i as u32;
            recs[r as usize] = Rec { size: *sz, alloc: *sz, flags: rflags::IN_USE, seq: 1, attrs: 0x2, ..Default::default() };
            names.push(NameEntry { record: r, parent: 5, off: arena.len() as u32, len: n.len() as u16, parent_seq: 1 });
            arena.extend_from_slice(n.as_bytes());
        }
        let (ix, _) = Index::build(&recs, &names, &arena, "C:").unwrap();
        let t = FileTypes::build(&ix);
        let exts: Vec<(&str, u64, u64)> = t.list.iter().map(|s| (s.ext.as_str(), s.size, s.files)).collect();
        assert_eq!(exts, vec![("mkv", 1000, 2), ("gz", 50, 1), ("", 13, 3), ("xlsx", 3, 1)]);
        let movie = ix.find("Movie.MKV").unwrap();
        assert_eq!(t.list[t.of_node[movie as usize] as usize].ext, "mkv");
        assert_eq!(t.of_node[0], NOT_FILE);
        assert!(ix.flags[movie as usize] & nflags::HIDDEN != 0, "attrs carried into node flags");
    }
}
