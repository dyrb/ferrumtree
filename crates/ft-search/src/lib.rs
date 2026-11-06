//! # ft-search
//!
//! Name search over an [`ft_index::Index`].
//!
//! All names are lower-cased once into one blob laid out as `\0name\0name\0...\0`.
//! A query is reduced to its longest literal piece, which is searched with SIMD `memmem`
//! across the whole blob in parallel; the separators make anchors free:
//!
//! | query     | meaning                  | prefilter needle |
//! |-----------|--------------------------|------------------|
//! | `report`  | substring                | `report`         |
//! | `*.mp4`   | glob, whole name         | `.mp4\0`         |
//! | `img_*`   | glob, whole name         | `\0img_`         |
//! | `a*b?.c`  | glob, whole name         | longest piece, then full glob check |
//!
//! Matching is case-insensitive (Unicode lower-casing, so Cyrillic works too).

use std::thread;

use ft_index::Index;
use memchr::memmem;

pub struct SearchIndex {
    blob: Vec<u8>,
    /// `offs[n]` = start of node n's name in `blob`; `len() == nodes + 1`.
    offs: Vec<u32>,
}

#[derive(Debug, Clone)]
pub struct Query {
    /// Lower-cased pattern chars; `None` for plain substring queries.
    glob: Option<Vec<char>>,
    needle: Vec<u8>,
    /// The needle starts with the `\0` separator.
    lead_sep: bool,
}

fn push_lower(out: &mut Vec<u8>, s: &str) {
    if s.is_ascii() {
        out.extend(s.bytes().map(|b| b.to_ascii_lowercase()));
    } else {
        out.extend_from_slice(s.to_lowercase().as_bytes());
    }
}

impl Query {
    pub fn parse(pattern: &str) -> Query {
        let lower = pattern.to_lowercase();
        if !lower.contains(['*', '?']) {
            return Query { glob: None, needle: lower.into_bytes(), lead_sep: false };
        }
        let chars: Vec<char> = lower.chars().collect();
        // Literal pieces between wildcards, with anchor information.
        let mut best: (usize, Vec<u8>, bool) = (0, Vec::new(), false);
        let pieces: Vec<&str> = lower.split(['*', '?']).collect();
        let last = pieces.len() - 1;
        for (i, p) in pieces.iter().enumerate() {
            if p.is_empty() {
                continue;
            }
            let mut nd = Vec::with_capacity(p.len() + 2);
            let lead = i == 0;
            if lead {
                nd.push(0);
            }
            nd.extend_from_slice(p.as_bytes());
            if i == last {
                nd.push(0);
            }
            if p.len() > best.0 {
                best = (p.len(), nd, lead);
            }
        }
        Query { glob: Some(chars), needle: best.1, lead_sep: best.2 }
    }

    fn accepts(&self, name_lower: &str) -> bool {
        match &self.glob {
            None => true, // the needle hit is the match
            Some(g) => glob_match(g, name_lower),
        }
    }
}

/// Iterative glob with single-star backtracking: O(n,m) worst case, linear in practice.
pub fn glob_match(pat: &[char], name: &str) -> bool {
    let s: Vec<char> = name.chars().collect();
    let (mut p, mut i) = (0usize, 0usize);
    let (mut star, mut mark) = (usize::MAX, 0usize);
    while i < s.len() {
        if p < pat.len() && (pat[p] == '?' || pat[p] == s[i]) {
            p += 1;
            i += 1;
        } else if p < pat.len() && pat[p] == '*' {
            star = p;
            mark = i;
            p += 1;
        } else if star != usize::MAX {
            p = star + 1;
            mark += 1;
            i = mark;
        } else {
            return false;
        }
    }
    while p < pat.len() && pat[p] == '*' {
        p += 1;
    }
    p == pat.len()
}

impl SearchIndex {
    pub fn build(ix: &Index) -> SearchIndex {
        let n = ix.len();
        let mut blob = Vec::with_capacity(ix.names.len() + n + 1);
        let mut offs = Vec::with_capacity(n + 1);
        blob.push(0);
        for i in 0..n as u32 {
            offs.push(blob.len() as u32);
            push_lower(&mut blob, ix.name(i));
            blob.push(0);
        }
        offs.push(blob.len() as u32 + 1);
        SearchIndex { blob, offs }
    }

    pub fn heap_bytes(&self) -> usize {
        self.blob.capacity() + self.offs.capacity() * 4
    }

    fn nodes(&self) -> usize {
        self.offs.len() - 1
    }

    fn name_lower(&self, n: usize) -> &str {
        let a = self.offs[n] as usize;
        let b = self.offs[n + 1] as usize - 1;
        std::str::from_utf8(&self.blob[a..b]).unwrap_or("")
    }

    fn search_range(&self, q: &Query, a: usize, b: usize, out: &mut Vec<u32>) {
        if q.needle.is_empty() {
            out.extend((a..b).filter(|&n| q.accepts(self.name_lower(n))).map(|n| n as u32));
            return;
        }
        let lo = self.offs[a] as usize - 1;
        let hi = (self.offs[b] as usize).min(self.blob.len());
        let hay = &self.blob[lo..hi];
        let shift = q.lead_sep as usize;
        let mut last = u32::MAX;
        for pos in memmem::find_iter(hay, &q.needle) {
            let abs = lo + pos + shift;
            let node = (self.offs[a..=b].partition_point(|&o| o as usize <= abs) - 1 + a) as u32;
            if node == last {
                continue;
            }
            if q.accepts(self.name_lower(node as usize)) {
                out.push(node);
                last = node;
            }
        }
    }

    /// All matching nodes, ascending node order (= BFS order: shallow paths first).
    pub fn search(&self, q: &Query, threads: usize) -> Vec<u32> {
        let n = self.nodes();
        let threads = threads.clamp(1, 64).min(n.div_ceil(65_536).max(1));
        if threads == 1 {
            let mut out = Vec::new();
            self.search_range(q, 0, n, &mut out);
            return out;
        }
        // Split by blob bytes, not node counts, so threads get equal work.
        let total = self.blob.len();
        let mut cuts = vec![0usize];
        for t in 1..threads {
            let target = total * t / threads;
            let c = self.offs.partition_point(|&o| (o as usize) < target).min(n);
            if c > *cuts.last().unwrap() {
                cuts.push(c);
            }
        }
        cuts.push(n);
        let parts: Vec<Vec<u32>> = thread::scope(|s| {
            let hs: Vec<_> = cuts
                .windows(2)
                .map(|w| {
                    let (a, b) = (w[0], w[1]);
                    s.spawn(move || {
                        let mut v = Vec::new();
                        self.search_range(q, a, b, &mut v);
                        v
                    })
                })
                .collect();
            hs.into_iter().map(|h| h.join().unwrap()).collect()
        });
        parts.concat()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ft_ntfs::{NameEntry, Rec, rflags};

    fn index(names: &[&str]) -> Index {
        let mut recs = vec![Rec::default(); 16 + names.len()];
        recs[5] = Rec { flags: rflags::IN_USE | rflags::DIR, seq: 1, ..Default::default() };
        let mut arena = Vec::new();
        let mut ents = Vec::new();
        for (i, nm) in names.iter().enumerate() {
            let r = 16 + i as u32;
            recs[r as usize] = Rec { flags: rflags::IN_USE, seq: 1, size: i as u64, ..Default::default() };
            ents.push(NameEntry { record: r, parent: 5, off: arena.len() as u32, len: nm.len() as u16, parent_seq: 1 });
            arena.extend_from_slice(nm.as_bytes());
        }
        Index::build(&recs, &ents, &arena, "C:").unwrap().0
    }

    fn names(ix: &Index, s: &SearchIndex, q: &str, threads: usize) -> Vec<String> {
        let mut v: Vec<String> = s.search(&Query::parse(q), threads).into_iter().map(|n| ix.name(n).to_string()).collect();
        v.sort();
        v
    }

    #[test]
    fn substring_and_globs() {
        let ix = index(&["Report.PDF", "report-2024.docx", "movie.mp4", "mp4list.txt", "IMG_001.jpg", "Отчёт.xlsx"]);
        let s = SearchIndex::build(&ix);
        assert_eq!(names(&ix, &s, "report", 1), ["Report.PDF", "report-2024.docx"]);
        assert_eq!(names(&ix, &s, "*.mp4", 1), ["movie.mp4"]);
        assert_eq!(names(&ix, &s, "mp4*", 1), ["mp4list.txt"]);
        assert_eq!(names(&ix, &s, "img_???.JPG", 1), ["IMG_001.jpg"]);
        assert_eq!(names(&ix, &s, "*o*t*", 1), ["Report.PDF", "report-2024.docx"]);
        assert_eq!(names(&ix, &s, "ОТЧЁТ", 1), ["Отчёт.xlsx"]);
        assert!(names(&ix, &s, "nothing", 1).is_empty());
    }

    #[test]
    fn parallel_equals_serial() {
        let owned: Vec<String> = (0..200_000).map(|i| format!("file_{i}.{}", ["txt", "log", "bin"][i % 3])).collect();
        let refs: Vec<&str> = owned.iter().map(|s| s.as_str()).collect();
        let ix = index(&refs);
        let s = SearchIndex::build(&ix);
        for q in ["99", "*.log", "file_1*", "*_12?.bin", "*"] {
            let a = s.search(&Query::parse(q), 1);
            let b = s.search(&Query::parse(q), 8);
            assert_eq!(a, b, "query {q}");
        }
        assert_eq!(s.search(&Query::parse("*.log"), 8).len(), 200_000 / 3);
    }

    #[test]
    fn glob_basics() {
        let g = |p: &str, s: &str| glob_match(&p.chars().collect::<Vec<_>>(), s);
        assert!(g("*", ""));
        assert!(g("a*", "abc"));
        assert!(!g("a*d", "abc"));
        assert!(g("*b*", "abc"));
        assert!(g("?b?", "abc"));
        assert!(g("a**c", "abc"));
    }
}
