//! # ft-fallback
//!
//! Directory walking for anything the MFT reader cannot read: FAT32, exFAT, ReFS, network
//! shares, or a single folder. Produces the same record table and name list as
//! [`ft_ntfs::scan`], so `ft_index::Index::build`, search and treemap work unchanged.
//!
//! * `FindFirstFileExW` with `FindExInfoBasic` (no 8.3 names) and
//!   `FIND_FIRST_EX_LARGE_FETCH` (larger kernel buffers, fewer calls).
//! * N workers share one stack of directories; each keeps its own name arena.
//! * Paths go through `\\?\` / `\\?\UNC\`, so length is not limited to `MAX_PATH`.
//! * Reparse points (junctions, symlinks, mount points) are listed but not entered.
//! * On-disk size is the logical size rounded up to the cluster size; for sparse,
//!   compressed and cloud placeholder files it comes from `GetCompressedFileSizeW`.

use std::path::Path;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use ft_ntfs::{NameEntry, ROOT_RECORD, Rec, rflags};
use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
use windows_sys::Win32::Storage::FileSystem::{
    FIND_FIRST_EX_LARGE_FETCH, FindClose, FindExInfoBasic, FindExSearchNameMatch, FindFirstFileExW, FindNextFileW, GetCompressedFileSizeW,
    GetDiskFreeSpaceW, WIN32_FIND_DATAW,
};

const A_DIRECTORY: u32 = 0x10;
const A_REPARSE: u32 = 0x400;
const A_SPARSE: u32 = 0x200;
const A_COMPRESSED: u32 = 0x800;
const A_OFFLINE: u32 = 0x1000;
const A_ENCRYPTED: u32 = 0x4000;
const A_RECALL_ON_OPEN: u32 = 0x40000;
const A_RECALL_ON_DATA_ACCESS: u32 = 0x400000;
/// Files whose on-disk size cannot be derived from the logical size.
const A_PACKED: u32 = A_SPARSE | A_COMPRESSED | A_OFFLINE | A_RECALL_ON_OPEN | A_RECALL_ON_DATA_ACCESS;

#[derive(Debug, Default, Clone)]
pub struct WalkStats {
    pub dirs: u64,
    pub files: u64,
    /// Directories that could not be listed (access denied, vanished, ...).
    pub errors: u64,
    pub cluster_size: u64,
    pub threads: usize,
    pub elapsed: Duration,
}

/// Same shape as an MFT scan: records indexed by id, names with parent ids.
/// The root directory is record [`ROOT_RECORD`].
pub struct Walk {
    pub recs: Vec<Rec>,
    pub names: Vec<NameEntry>,
    pub arena: Vec<u8>,
    pub stats: WalkStats,
}

/// `C:\x`, `\\server\share\x`, `\\?\...` -> extended-length form, without a trailing `\`.
pub fn extended_path(p: &str) -> Vec<u16> {
    let p = p.replace('/', "\\");
    let s = if p.starts_with(r"\\?\") {
        p
    } else if let Some(unc) = p.strip_prefix(r"\\") {
        format!(r"\\?\UNC\{unc}")
    } else {
        let abs = std::path::absolute(&p).map(|a| a.to_string_lossy().into_owned()).unwrap_or(p);
        format!(r"\\?\{abs}")
    };
    let s = s.trim_end_matches('\\');
    s.encode_utf16().collect()
}

fn cluster_size(root: &str) -> u64 {
    // GetDiskFreeSpaceW wants the root of the volume or share, with a trailing slash.
    let r: String = if let Some(unc) = root.strip_prefix(r"\\") {
        let mut parts = unc.splitn(3, '\\');
        format!(r"\\{}\{}\", parts.next().unwrap_or(""), parts.next().unwrap_or(""))
    } else {
        root.chars().take(2).chain(['\\']).collect()
    };
    let w: Vec<u16> = r.encode_utf16().chain([0]).collect();
    let (mut spc, mut bps, mut free, mut total) = (0u32, 0u32, 0u32, 0u32);
    // SAFETY: plain out-parameters.
    if unsafe { GetDiskFreeSpaceW(w.as_ptr(), &mut spc, &mut bps, &mut free, &mut total) } != 0 {
        (spc as u64 * bps as u64).max(1)
    } else {
        1
    }
}

struct Job {
    path: Vec<u16>,
    id: u32,
}

struct Queue {
    stack: Vec<Job>,
    /// Jobs queued or being processed; the walk ends when it reaches zero.
    active: usize,
}

#[derive(Default)]
struct Local {
    recs: Vec<(u32, Rec)>,
    names: Vec<NameEntry>,
    arena: Vec<u8>,
    dirs: u64,
    files: u64,
    errors: u64,
}

/// UTF-16 -> UTF-8 straight into the arena (ASCII fast path, no temporary buffer).
fn push_wide(out: &mut Vec<u8>, name: &[u16]) {
    if name.iter().all(|&c| c < 0x80) {
        out.extend(name.iter().map(|&c| c as u8));
        return;
    }
    let mut buf = [0u8; 4];
    for ch in char::decode_utf16(name.iter().copied()) {
        out.extend_from_slice(ch.unwrap_or(char::REPLACEMENT_CHARACTER).encode_utf8(&mut buf).as_bytes());
    }
}

fn filetime(ft: windows_sys::Win32::Foundation::FILETIME) -> u64 {
    (ft.dwHighDateTime as u64) << 32 | ft.dwLowDateTime as u64
}

fn on_disk(path: &[u16], size: u64, attrs: u32, cluster: u64) -> u64 {
    if attrs & A_PACKED != 0 {
        let mut w: Vec<u16> = path.to_vec();
        w.push(0);
        let mut high = 0u32;
        // SAFETY: NUL-terminated path, plain out-parameter.
        let low = unsafe { GetCompressedFileSizeW(w.as_ptr(), &mut high) };
        if low != u32::MAX || high != 0 {
            return ((high as u64) << 32 | low as u64).div_ceil(cluster) * cluster;
        }
    }
    size.div_ceil(cluster) * cluster
}

/// List one directory: records and names for its entries, subdirectories as new jobs.
fn list(job: &Job, ids: &AtomicU32, cluster: u64, out: &mut Local, sub: &mut Vec<Job>) {
    let mut pattern = job.path.clone();
    pattern.extend("\\*".encode_utf16());
    pattern.push(0);
    // SAFETY: WIN32_FIND_DATAW is a plain C struct filled by the call.
    let mut d: WIN32_FIND_DATAW = unsafe { std::mem::zeroed() };
    let h = unsafe {
        FindFirstFileExW(
            pattern.as_ptr(),
            FindExInfoBasic,
            (&mut d as *mut WIN32_FIND_DATAW).cast(),
            FindExSearchNameMatch,
            std::ptr::null(),
            FIND_FIRST_EX_LARGE_FETCH,
        )
    };
    if h == INVALID_HANDLE_VALUE {
        out.errors += 1;
        return;
    }
    loop {
        let n = d.cFileName.iter().position(|&c| c == 0).unwrap_or(d.cFileName.len());
        let name = &d.cFileName[..n];
        let dots = name == [b'.' as u16] || name == [b'.' as u16, b'.' as u16];
        if !dots {
            let attrs = d.dwFileAttributes;
            let id = ids.fetch_add(1, Ordering::Relaxed);
            let dir = attrs & A_DIRECTORY != 0;
            let mut child = job.path.clone();
            child.push(b'\\' as u16);
            child.extend_from_slice(name);
            let size = if dir { 0 } else { (d.nFileSizeHigh as u64) << 32 | d.nFileSizeLow as u64 };
            let mut flags = rflags::IN_USE;
            if dir {
                flags |= rflags::DIR;
            }
            if attrs & A_REPARSE != 0 {
                flags |= rflags::REPARSE;
            }
            if attrs & A_COMPRESSED != 0 {
                flags |= rflags::COMPRESSED;
            }
            if attrs & A_SPARSE != 0 {
                flags |= rflags::SPARSE;
            }
            if attrs & A_ENCRYPTED != 0 {
                flags |= rflags::ENCRYPTED;
            }
            let alloc = if dir { 0 } else { on_disk(&child, size, attrs, cluster) };
            out.recs.push((id, Rec { size, alloc, mtime: filetime(d.ftLastWriteTime), attrs, seq: 1, flags }));
            let off = out.arena.len() as u32;
            push_wide(&mut out.arena, name);
            let len = (out.arena.len() as u32 - off) as u16;
            out.names.push(NameEntry { record: id, parent: job.id, off, len, parent_seq: 1 });
            if dir {
                out.dirs += 1;
                if attrs & A_REPARSE == 0 {
                    sub.push(Job { path: child, id });
                }
            } else {
                out.files += 1;
            }
        }
        // SAFETY: valid find handle and output struct.
        if unsafe { FindNextFileW(h, &mut d) } == 0 {
            break;
        }
    }
    // SAFETY: handle from FindFirstFileExW.
    unsafe { FindClose(h) };
}

/// Walk `root` (a drive root like `D:\`, a UNC share or any folder) with `threads` workers.
pub fn walk(root: &str, threads: usize) -> std::io::Result<Walk> {
    walk_progress(root, threads, None)
}

/// Files and folders listed so far, readable from another thread.
#[derive(Debug, Default)]
pub struct WalkProgress {
    pub files: AtomicU64,
    pub dirs: AtomicU64,
}

/// [`walk`], counting what has been listed so far into `progress` (two atomic adds per
/// directory).
pub fn walk_progress(root: &str, threads: usize, progress: Option<&WalkProgress>) -> std::io::Result<Walk> {
    let t0 = Instant::now();
    let base = extended_path(root);
    let probe = String::from_utf16_lossy(&base);
    if !Path::new(&probe).is_dir() {
        return Err(std::io::Error::new(std::io::ErrorKind::NotFound, format!("not a directory: {root}")));
    }
    let cluster = cluster_size(root);
    let threads = threads.max(1);
    let ids = AtomicU32::new(16);
    let queue = Mutex::new(Queue { stack: vec![Job { path: base, id: ROOT_RECORD }], active: 1 });
    let cv = Condvar::new();

    let locals: Vec<Local> = thread::scope(|s| {
        let hs: Vec<_> = (0..threads)
            .map(|_| {
                s.spawn(|| {
                    let mut local = Local::default();
                    let mut sub = Vec::new();
                    loop {
                        let job = {
                            let mut q = queue.lock().unwrap();
                            loop {
                                if let Some(j) = q.stack.pop() {
                                    break Some(j);
                                }
                                if q.active == 0 {
                                    break None;
                                }
                                q = cv.wait(q).unwrap();
                            }
                        };
                        let Some(job) = job else { break };
                        let before = local.recs.len();
                        list(&job, &ids, cluster, &mut local, &mut sub);
                        if let Some(p) = progress {
                            let listed = (local.recs.len() - before) as u64;
                            let dirs = sub.len() as u64;
                            p.dirs.fetch_add(dirs, Ordering::Relaxed);
                            p.files.fetch_add(listed.saturating_sub(dirs), Ordering::Relaxed);
                        }
                        let mut q = queue.lock().unwrap();
                        let n = sub.len();
                        q.active += n;
                        q.stack.append(&mut sub);
                        q.active -= 1;
                        let done = q.active == 0;
                        drop(q);
                        if done {
                            cv.notify_all();
                        } else if n > 0 {
                            for _ in 0..n.min(threads) {
                                cv.notify_one();
                            }
                        }
                    }
                    local
                })
            })
            .collect();
        hs.into_iter().map(|h| h.join().expect("walker panicked")).collect()
    });

    let n = ids.load(Ordering::Relaxed) as usize;
    let mut recs = vec![Rec::default(); n];
    recs[ROOT_RECORD as usize] = Rec { flags: rflags::IN_USE | rflags::DIR, seq: 1, attrs: A_DIRECTORY, ..Default::default() };
    let mut stats = WalkStats { cluster_size: cluster, threads, ..Default::default() };
    let mut names = Vec::with_capacity(locals.iter().map(|l| l.names.len()).sum());
    let mut arena = Vec::with_capacity(locals.iter().map(|l| l.arena.len()).sum());
    for l in locals {
        for (id, r) in l.recs {
            recs[id as usize] = r;
        }
        let base = arena.len() as u32;
        arena.extend_from_slice(&l.arena);
        names.extend(l.names.into_iter().map(|e| NameEntry { off: e.off + base, ..e }));
        stats.dirs += l.dirs;
        stats.files += l.files;
        stats.errors += l.errors;
    }
    stats.elapsed = t0.elapsed();
    Ok(Walk { recs, names, arena, stats })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn extended_paths() {
        let s = |p: &str| String::from_utf16(&extended_path(p)).unwrap();
        assert_eq!(s(r"C:\"), r"\\?\C:");
        assert_eq!(s(r"D:\Data\"), r"\\?\D:\Data");
        assert_eq!(s(r"\\nas\share\x"), r"\\?\UNC\nas\share\x");
        assert_eq!(s(r"\\?\E:\y"), r"\\?\E:\y");
    }

    #[test]
    fn walks_a_tree_into_an_index() {
        let root = std::env::temp_dir().join(format!("ft-fallback-{}", std::process::id()));
        let deep = root.join("a").join("b");
        fs::create_dir_all(&deep).unwrap();
        fs::write(root.join("top.txt"), vec![1u8; 1000]).unwrap();
        fs::write(deep.join("deep.bin"), vec![2u8; 5000]).unwrap();
        fs::write(root.join("a").join("Отчёт.xlsx"), vec![3u8; 10]).unwrap();
        let w = walk(root.to_str().unwrap(), 4).unwrap();
        assert_eq!((w.stats.files, w.stats.dirs, w.stats.errors), (3, 2, 0));
        let (ix, _) = ft_index::Index::build(&w.recs, &w.names, &w.arena, "ROOT").unwrap();
        ix.check().unwrap();
        assert_eq!(ix.size[0], 6010);
        assert_eq!(ix.files[0], 3);
        assert_eq!(ix.size[ix.find(r"a\b\deep.bin").unwrap() as usize], 5000);
        assert!(ix.find(r"a\Отчёт.xlsx").is_some());
        assert!(ix.alloc[0] >= 6010);
        fs::remove_dir_all(&root).unwrap();
    }
}
