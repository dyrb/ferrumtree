//! # ft-dupes
//!
//! Duplicate files below a node of an [`ft_index::Index`], reading as little as possible:
//!
//! 1. **size** - from the index, no I/O. Secondary hard links are skipped: they are the
//!    same file, not a copy.
//! 2. **head hash** - the first 64 KiB, only for files sharing a size. For files no larger
//!    than that, this already covers the whole content.
//! 3. **full hash** - only for files still sharing size and head hash.
//!
//! Files are read in parallel. Cloud placeholders and offline files are skipped (reading
//! them would trigger a download); files that cannot be opened are counted, not fatal.
//! The 128-bit content hash is not cryptographic: it is meant for honest data, where a
//! false match is astronomically unlikely.

use std::io::Read;
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use ft_index::{Index, nflags};

const HEAD: usize = 64 << 10;
const CHUNK: usize = 1 << 20;
const FILE_FLAG_SEQUENTIAL_SCAN: u32 = 0x0800_0000;
/// Offline, recall-on-open, recall-on-data-access: reading would fetch from the cloud.
const A_REMOTE: u32 = 0x1000 | 0x40000 | 0x400000;

#[derive(Clone, Debug)]
pub struct Options {
    pub min_size: u64,
    pub threads: usize,
}

impl Default for Options {
    fn default() -> Self {
        Options { min_size: 1 << 20, threads: 4 }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Group {
    pub size: u64,
    pub nodes: Vec<u32>,
}

impl Group {
    /// Bytes that deleting all but one copy would free.
    pub fn wasted(&self) -> u64 {
        self.size * (self.nodes.len() as u64 - 1)
    }
}

#[derive(Debug, Default)]
pub struct Report {
    /// Largest waste first.
    pub groups: Vec<Group>,
    pub files_considered: u64,
    pub same_size: u64,
    pub head_hashed: u64,
    pub full_hashed: u64,
    pub bytes_read: u64,
    pub skipped: u64,
    pub elapsed: Duration,
}

impl Report {
    pub fn wasted(&self) -> u64 {
        self.groups.iter().map(Group::wasted).sum()
    }
}

/// Streaming 128-bit hash: four multiply-rotate lanes, two independent finalisers.
pub struct Hasher128 {
    l: [u64; 4],
    len: u64,
}

impl Default for Hasher128 {
    fn default() -> Self {
        Hasher128 { l: [0x9E37_79B9_7F4A_7C15, 0xC2B2_AE3D_27D4_EB4F, 0x1656_67B1_9E37_79F9, 0x27D4_EB2F_1656_67C5], len: 0 }
    }
}

const K: u64 = 0x9E37_79B9_7F4A_7C15;

fn mix(mut h: u64) -> u64 {
    h ^= h >> 33;
    h = h.wrapping_mul(0xFF51_AFD7_ED55_8CCD);
    h ^= h >> 33;
    h = h.wrapping_mul(0xC4CE_B9FE_1A85_EC53);
    h ^ (h >> 33)
}

impl Hasher128 {
    /// Feed bytes. All calls but the last must pass a multiple of 32 bytes.
    pub fn update(&mut self, b: &[u8]) {
        let (blocks, rem) = b.as_chunks::<32>();
        for c in blocks {
            for (i, lane) in self.l.iter_mut().enumerate() {
                let w = u64::from_le_bytes(c[i * 8..i * 8 + 8].try_into().unwrap());
                *lane = (*lane ^ w).wrapping_mul(K).rotate_left(31);
            }
        }
        for (i, &x) in rem.iter().enumerate() {
            let lane = &mut self.l[i & 3];
            *lane = (*lane ^ x as u64 ^ ((i as u64) << 8)).wrapping_mul(K).rotate_left(23);
        }
        self.len += b.len() as u64;
    }

    pub fn finish(&self) -> u128 {
        let [a, b, c, d] = self.l;
        let h1 = mix(a ^ b.rotate_left(17) ^ self.len);
        let h2 = mix(c ^ d.rotate_left(29) ^ self.len.rotate_left(32) ^ h1);
        (h1 as u128) << 64 | h2 as u128
    }
}

/// Hash the first `limit` bytes (or the whole file with `usize::MAX`).
fn hash_file(path: &str, limit: usize, buf: &mut [u8]) -> Option<(u128, u64)> {
    let md = std::fs::symlink_metadata(path).ok()?;
    if md.file_attributes() & A_REMOTE != 0 {
        return None;
    }
    let mut f = std::fs::OpenOptions::new().read(true).share_mode(7).custom_flags(FILE_FLAG_SEQUENTIAL_SCAN).open(path).ok()?;
    let mut h = Hasher128::default();
    let mut total = 0u64;
    loop {
        let want = (limit - total as usize).min(buf.len());
        if want == 0 {
            break;
        }
        // Fill the whole buffer (so every block but the last is a multiple of 32 bytes).
        let mut got = 0;
        while got < want {
            match f.read(&mut buf[got..want]) {
                Ok(0) => break,
                Ok(n) => got += n,
                Err(_) => return None,
            }
        }
        h.update(&buf[..got]);
        total += got as u64;
        if got < want {
            break;
        }
    }
    Some((h.finish(), total))
}

/// Hash `items` in parallel; returns (item, hash) for the files that could be read.
fn hash_all(ix: &Index, items: &[u32], limit: usize, o: &Options, cancel: &AtomicBool, bytes: &AtomicU64) -> Vec<(u32, u128)> {
    let next = AtomicUsize::new(0);
    let out = Mutex::new(Vec::with_capacity(items.len()));
    thread::scope(|s| {
        for _ in 0..o.threads.max(1).min(items.len().max(1)) {
            s.spawn(|| {
                let mut buf = vec![0u8; CHUNK];
                let mut local = Vec::new();
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    if i >= items.len() || cancel.load(Ordering::Relaxed) {
                        break;
                    }
                    let n = items[i];
                    if let Some((h, read)) = hash_file(&ix.path(n), limit, &mut buf) {
                        bytes.fetch_add(read, Ordering::Relaxed);
                        local.push((n, h));
                    }
                }
                out.lock().unwrap().append(&mut local);
            });
        }
    });
    out.into_inner().unwrap()
}

/// Runs of equal keys with at least two members, as node lists.
fn runs<K: Ord + Copy>(mut v: Vec<(K, u32)>) -> Vec<Vec<u32>> {
    v.sort_unstable();
    let mut out = Vec::new();
    let mut i = 0;
    while i < v.len() {
        let j = i + v[i..].iter().take_while(|x| x.0 == v[i].0).count();
        if j - i >= 2 {
            out.push(v[i..j].iter().map(|x| x.1).collect());
        }
        i = j;
    }
    out
}

pub fn find(ix: &Index, root: u32, o: &Options, cancel: &AtomicBool) -> Report {
    let t0 = Instant::now();
    let mut rep = Report::default();
    // 1. Candidate files by size.
    let mut by_size: Vec<(u64, u32)> = Vec::new();
    let mut stack = vec![root];
    while let Some(n) = stack.pop() {
        if !ix.is_dir(n) {
            by_size.push((ix.size[n as usize], n));
            continue;
        }
        for c in ix.children(n) {
            let f = ix.flags[c as usize];
            if f & (nflags::DUP_LINK | nflags::REPARSE) != 0 {
                continue;
            }
            if f & nflags::DIR != 0 {
                stack.push(c);
            } else if ix.size[c as usize] >= o.min_size.max(1) {
                by_size.push((ix.size[c as usize], c));
            }
        }
    }
    rep.files_considered = by_size.len() as u64;
    let same_size = runs(by_size);
    let bytes = AtomicU64::new(0);
    let cand: Vec<u32> = same_size.iter().flatten().copied().collect();
    rep.same_size = cand.len() as u64;

    // 2. Head hash.
    let heads = hash_all(ix, &cand, HEAD, o, cancel, &bytes);
    rep.head_hashed = heads.len() as u64;
    rep.skipped = cand.len() as u64 - heads.len() as u64;
    let by_head = runs(heads.into_iter().map(|(n, h)| ((ix.size[n as usize], h), n)).collect());

    // 3. Full hash where the head did not cover the file.
    let (small, large): (Vec<Vec<u32>>, Vec<Vec<u32>>) = by_head.into_iter().partition(|g| ix.size[g[0] as usize] <= HEAD as u64);
    let large_items: Vec<u32> = large.into_iter().flatten().collect();
    let full = hash_all(ix, &large_items, usize::MAX, o, cancel, &bytes);
    rep.full_hashed = full.len() as u64;
    rep.skipped += large_items.len() as u64 - full.len() as u64;
    let by_full = runs(full.into_iter().map(|(n, h)| ((ix.size[n as usize], h), n)).collect());

    rep.groups = small
        .into_iter()
        .chain(by_full)
        .map(|mut nodes| {
            nodes.sort_unstable();
            Group { size: ix.size[nodes[0] as usize], nodes }
        })
        .collect();
    rep.groups.sort_unstable_by(|a, b| b.wasted().cmp(&a.wasted()).then(a.nodes.cmp(&b.nodes)));
    rep.bytes_read = bytes.into_inner();
    rep.elapsed = t0.elapsed();
    rep
}

#[cfg(test)]
mod tests {
    use super::*;
    use ft_ntfs::{NameEntry, Rec, rflags};
    use std::fs;

    #[test]
    fn hasher_is_streaming_consistent_and_discriminating() {
        let data: Vec<u8> = (0..300_000u32).map(|i| (i * 7 + i / 13) as u8).collect();
        let mut a = Hasher128::default();
        a.update(&data);
        let mut b = Hasher128::default();
        for c in data.chunks(64 * 1024) {
            b.update(c); // 64 KiB chunks: multiples of 32 except the last
        }
        assert_eq!(a.finish(), b.finish());
        let mut d2 = data.clone();
        d2[150_000] ^= 1;
        let mut c = Hasher128::default();
        c.update(&d2);
        assert_ne!(a.finish(), c.finish());
        let mut e = Hasher128::default();
        e.update(&data[..data.len() - 1]);
        assert_ne!(a.finish(), e.finish());
    }

    /// Builds an index whose paths point at real files in a temp directory.
    #[test]
    fn finds_copies_and_ignores_lookalikes() {
        let dir = std::env::temp_dir().join(format!("ft-dupes-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let big: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
        let mut big_tail = big.clone();
        *big_tail.last_mut().unwrap() ^= 0xFF; // same size and head, different tail
        let small = vec![7u8; 5000];
        let files: Vec<(&str, &[u8])> = vec![
            ("a.bin", &big),
            ("b.bin", &big),
            ("c.bin", &big_tail),
            ("s1.dat", &small),
            ("s2.dat", &small),
            ("s3.dat", &small),
            ("lonely.txt", b"unique"),
        ];
        for (n, d) in &files {
            fs::write(dir.join(n), d).unwrap();
        }
        let mut recs = vec![Rec::default(); 16 + files.len()];
        recs[5] = Rec { flags: rflags::IN_USE | rflags::DIR, seq: 1, ..Default::default() };
        let mut names = vec![NameEntry { record: 5, parent: 5, off: 0, len: 1, parent_seq: 1 }];
        let mut arena = b".".to_vec();
        for (i, (n, d)) in files.iter().enumerate() {
            let r = 16 + i as u32;
            recs[r as usize] = Rec { size: d.len() as u64, flags: rflags::IN_USE, seq: 1, ..Default::default() };
            names.push(NameEntry { record: r, parent: 5, off: arena.len() as u32, len: n.len() as u16, parent_seq: 1 });
            arena.extend_from_slice(n.as_bytes());
        }
        let root = dir.to_str().unwrap().to_string();
        let (ix, _) = Index::build(&recs, &names, &arena, &root).unwrap();
        let rep = find(&ix, 0, &Options { min_size: 1, threads: 3 }, &AtomicBool::new(false));
        let groups: Vec<Vec<&str>> = rep.groups.iter().map(|g| g.nodes.iter().map(|&n| ix.name(n)).collect::<Vec<_>>()).collect();
        let mut sorted: Vec<Vec<&str>> = groups
            .into_iter()
            .map(|mut g| {
                g.sort();
                g
            })
            .collect();
        sorted.sort();
        assert_eq!(sorted, vec![vec!["a.bin", "b.bin"], vec!["s1.dat", "s2.dat", "s3.dat"]]);
        assert_eq!(rep.wasted(), 200_000 + 2 * 5000);
        assert_eq!(rep.groups[0].size, 200_000, "largest waste first");
        assert_eq!(rep.full_hashed, 3, "only the three 200 KB files needed a full read");
        fs::remove_dir_all(&dir).unwrap();
    }
}
