//! Background engine: one thread per scan target.
//!
//! ```text
//! drive or folder on a local NTFS volume -> catalog (cache + USN journal, or full MFT scan)
//!     -> tree + file types -> Ready snapshot ──PostMessage──► UI thread
//!     -> block in FSCTL_READ_USN_JOURNAL until the volume changes (no polling)
//!     -> re-read the changed records -> Patch (record before/after) ──► UI patches its tree
//! anything else (FAT/exFAT/ReFS, network, folders elsewhere) -> parallel directory walk
//! ```
//!
//! Live updates follow the [`Live`] setting. Off: the thread ends after the first snapshot,
//! nothing runs in the background. Active: while the window is inactive the thread sleeps
//! on an event (no journal reads, no CPU) and catches up when the window is activated.
//!
//! A folder on an NTFS volume is served from the volume's index (the view starts at the
//! folder), which with a warm cache is far faster than walking it.
//! Every start bumps a generation counter; a stale thread exits at its next wake-up and
//! its late messages are ignored. Engines on the same volume run one at a time: the
//! catalog is a mapping of the cache file, which cannot be replaced while mapped.

use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use ft_index::patch::Change;
use ft_index::types::FileTypes;
use ft_index::{Index, LinkPolicy};
use ft_ntfs::scan::{ScanOptions, ScanProgress};
use ft_ntfs::volume::{IoMode, ReadSlot, Volume, parse_drive_letter};
use ft_usn::{Catalog, OpenOptions, VolumeSource, journal};
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
use windows_sys::Win32::Storage::FileSystem::{GetDriveTypeW, GetVolumeInformationW};
use windows_sys::Win32::System::Threading::{CreateEventW, INFINITE, ResetEvent, SetEvent, WaitForSingleObject};
use windows_sys::Win32::UI::WindowsAndMessaging::{PostMessageW, WM_APP};

use crate::lang::{self, S};
use crate::settings::Live;
use crate::util::wide;

pub const WM_ENGINE: u32 = WM_APP + 1;

/// More changed records than this in one batch: a fresh tree is cheaper than patching.
const PATCH_LIMIT: usize = 100_000;
/// After the first new journal entry, wait this long for the rest of a burst (copying a
/// folder) so it arrives as one patch.
const BURST_MS: u32 = 40;
/// At most this many patches per second: a lone change shows within `BURST_MS`, steady
/// background writes (browsers, logs) are merged.
const MIN_GAP: Duration = Duration::from_millis(250);

static GENERATION: AtomicU64 = AtomicU64::new(0);
static PROGRESS: Mutex<Option<Arc<ScanProgress>>> = Mutex::new(None);
static WALKED: ft_fallback::WalkProgress = ft_fallback::WalkProgress { files: AtomicU64::new(0), dirs: AtomicU64::new(0) };
/// One engine per volume at a time (see the module notes).
static VOLUMES: [Mutex<()>; 26] = [const { Mutex::new(()) }; 26];
/// Wake-up event of the live thread, if one runs.
static LIVE: Mutex<Option<Arc<Signal>>> = Mutex::new(None);
static MODE: AtomicU8 = AtomicU8::new(1);
static ACTIVE: AtomicBool = AtomicBool::new(true);
static REBUILD: AtomicBool = AtomicBool::new(false);

/// Manual-reset event.
struct Signal(HANDLE);

// SAFETY: event handles may be used from any thread.
unsafe impl Send for Signal {}
unsafe impl Sync for Signal {}

impl Signal {
    fn new() -> Result<Signal, String> {
        // SAFETY: plain Win32 call; null name / default security.
        let h = unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) };
        if h.is_null() { Err(std::io::Error::last_os_error().to_string()) } else { Ok(Signal(h)) }
    }
    fn set(&self) {
        // SAFETY: owned, open handle.
        unsafe { SetEvent(self.0) };
    }
    fn reset(&self) {
        // SAFETY: as above.
        unsafe { ResetEvent(self.0) };
    }
    /// `true` if signaled within `ms`.
    fn wait(&self, ms: u32) -> bool {
        // SAFETY: as above.
        unsafe { WaitForSingleObject(self.0, ms) == 0 }
    }
}

impl Drop for Signal {
    fn drop(&mut self) {
        // SAFETY: owned handle, closed once.
        unsafe { CloseHandle(self.0) };
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Wake the live thread so it re-reads the flags (mode, activity, retirement).
fn wake_live() {
    if let Some(s) = lock(&LIVE).as_ref() {
        s.set();
    }
}

/// Wake the live thread, if any, so it notices it is stale and exits.
fn retire_live() {
    if let Some(s) = lock(&LIVE).take() {
        s.set();
    }
}

fn mode() -> Live {
    match MODE.load(Ordering::SeqCst) {
        0 => Live::Off,
        2 => Live::Always,
        _ => Live::Active,
    }
}

/// Live-update setting; takes effect at once (switching Off ends the live thread).
pub fn set_live(l: Live) {
    MODE.store(
        match l {
            Live::Off => 0,
            Live::Active => 1,
            Live::Always => 2,
        },
        Ordering::SeqCst,
    );
    wake_live();
}

/// Whether the application is active (`WM_ACTIVATEAPP`).
pub fn set_active(a: bool) {
    if ACTIVE.swap(a, Ordering::SeqCst) != a {
        wake_live();
    }
}

/// The UI could not patch its tree: send a fresh one.
pub fn request_rebuild() {
    REBUILD.store(true, Ordering::SeqCst);
    wake_live();
}

/// A live thread is running (patches will follow).
pub fn is_live() -> bool {
    lock(&LIVE).is_some()
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    Drive(char),
    Folder(String),
}

impl Target {
    pub fn parse(s: &str) -> Option<Target> {
        if let Some(l) = parse_drive_letter(s) {
            return Some(Target::Drive(l));
        }
        Path::new(s).is_dir().then(|| Target::Folder(s.trim_end_matches(['\\', '/']).to_string()))
    }
    pub fn label(&self) -> String {
        match self {
            Target::Drive(l) => format!("{l}:"),
            Target::Folder(p) => p.clone(),
        }
    }
}

/// Where a snapshot came from.
#[derive(Clone, Copy, Debug)]
pub enum How {
    /// The volume's MFT (from the cache and the journal, or read in full).
    Volume,
    /// A directory walk; `errors` folders could not be listed.
    Walk { errors: u64 },
}

pub struct Snapshot {
    pub generation: u64,
    pub target: Target,
    /// Drive letter of the volume the data comes from (for free-space figures).
    pub drive: Option<char>,
    pub ix: Index,
    pub types: FileTypes,
    /// Folder to show as the root (a folder target served from its volume's index).
    pub view_root: Option<String>,
    pub how: How,
    pub elapsed: Duration,
    /// A replacement of the current snapshot (same target): keep the UI state.
    pub live: bool,
}

/// Changed records since the last snapshot or patch.
pub struct Patch {
    pub changes: Vec<Change>,
}

pub enum Event {
    Status(u64, String),
    Ready(Box<Snapshot>),
    Patch(u64, Box<Patch>),
    /// The journal moved on too far to follow (the window was inactive for very long):
    /// scan again.
    Stale(u64),
    Failed(u64, String),
}

pub fn generation() -> u64 {
    GENERATION.load(Ordering::SeqCst)
}

/// Progress of the running scan: the fraction done (MFT scans only) and the folders and
/// files found so far.
pub struct Progress {
    pub fraction: Option<f64>,
    pub dirs: u64,
    pub files: u64,
}

pub fn progress() -> Progress {
    let g = lock(&PROGRESS);
    match g.as_ref() {
        Some(p) => Progress { fraction: Some(p.fraction()), dirs: p.dirs.load(Ordering::Relaxed), files: p.files.load(Ordering::Relaxed) },
        None => Progress { fraction: None, dirs: WALKED.dirs.load(Ordering::Relaxed), files: WALKED.files.load(Ordering::Relaxed) },
    }
}

fn current(id: u64) -> bool {
    GENERATION.load(Ordering::SeqCst) == id
}

fn post(hwnd: isize, ev: Event) {
    let p = Box::into_raw(Box::new(ev));
    // SAFETY: ownership travels with the message; reclaimed by `take`, or here if the
    // message could not be queued.
    if unsafe { PostMessageW(hwnd as _, WM_ENGINE, 0, p as isize) } == 0 {
        drop(unsafe { Box::from_raw(p) });
    }
}

/// Reclaim the event posted with a `WM_ENGINE` message.
///
/// # Safety
/// `lparam` must come from a `WM_ENGINE` message posted by this module.
pub unsafe fn take(lparam: isize) -> Event {
    *unsafe { Box::from_raw(lparam as *mut Event) }
}

pub fn stop() {
    GENERATION.fetch_add(1, Ordering::SeqCst);
    retire_live();
}

/// The window is closing: [`stop`], and give a live engine up to `limit` to make its
/// cache current (an unfinished scan is not waited for longer).
pub fn shutdown(limit: Duration) {
    stop();
    let until = Instant::now() + limit;
    for v in &VOLUMES {
        while matches!(v.try_lock(), Err(std::sync::TryLockError::WouldBlock)) {
            if Instant::now() >= until {
                return;
            }
            thread::sleep(Duration::from_millis(5));
        }
    }
}

/// [`stop`], then wait until no engine holds a volume (and so the cache file). Blocks
/// while a scan finishes: call it from a worker thread.
pub fn stop_and_wait() {
    stop();
    for v in &VOLUMES {
        drop(lock(v));
    }
}

/// File system name and drive type of a drive letter.
pub fn volume_kind(letter: char) -> (String, u32) {
    let root = wide(&format!("{letter}:\\"));
    let mut fs = [0u16; 32];
    // SAFETY: local buffers of the stated sizes.
    unsafe {
        let kind = GetDriveTypeW(root.as_ptr());
        let ok = GetVolumeInformationW(
            root.as_ptr(),
            std::ptr::null_mut(),
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            fs.as_mut_ptr(),
            32,
        );
        let n = fs.iter().position(|&c| c == 0).unwrap_or(0);
        (if ok != 0 { String::from_utf16_lossy(&fs[..n]) } else { String::new() }, kind)
    }
}

const DRIVE_REMOTE: u32 = 4;

/// Start scanning `target`; any previous engine thread retires. `cache`: keep the
/// catalog file next to the program.
pub fn start(hwnd: isize, target: Target, rescan: bool, cache: bool) {
    let id = GENERATION.fetch_add(1, Ordering::SeqCst) + 1;
    retire_live();
    REBUILD.store(false, Ordering::SeqCst);
    WALKED.files.store(0, Ordering::Relaxed);
    WALKED.dirs.store(0, Ordering::Relaxed);
    if let Ok(mut g) = PROGRESS.lock() {
        *g = None;
    }
    thread::spawn(move || {
        let letter = match &target {
            Target::Drive(l) => Some(*l),
            Target::Folder(p) => parse_drive_letter(&p.chars().take(2).collect::<String>()),
        };
        let ntfs = letter.is_some_and(|l| {
            let (fs, kind) = volume_kind(l);
            fs == "NTFS" && kind != DRIVE_REMOTE
        });
        let view_root = match &target {
            Target::Folder(p) if ntfs => Some(p.clone()),
            _ => None,
        };
        let r = match (ntfs, letter) {
            (true, Some(l)) => run_ntfs(hwnd, target.clone(), l, view_root, rescan, cache, id),
            _ => run_walk(hwnd, target.clone(), letter, id),
        };
        if let Err(e) = r {
            post(hwnd, Event::Failed(id, e));
        }
    });
}

fn walk_root(target: &Target) -> String {
    match target {
        Target::Drive(l) => format!("{l}:\\"),
        Target::Folder(p) => p.clone(),
    }
}

fn run_walk(hwnd: isize, target: Target, drive: Option<char>, id: u64) -> Result<(), String> {
    let root = walk_root(&target);
    post(hwnd, Event::Status(id, root.clone()));
    let t = Instant::now();
    let threads = thread::available_parallelism().map_or(4, |n| n.get());
    let w = ft_fallback::walk_progress(&root, threads, Some(&WALKED)).map_err(|e| e.to_string())?;
    let name = target.label();
    let (ix, _) = Index::build_with(&w.recs, &w.names, &w.arena, &name, LinkPolicy::Once).map_err(|e| e.to_string())?;
    let errors = w.stats.errors;
    drop(w);
    let types = FileTypes::build(&ix);
    if !current(id) {
        return Ok(());
    }
    let how = How::Walk { errors };
    let snap = Snapshot { generation: id, target, drive, ix, types, view_root: None, how, elapsed: t.elapsed(), live: false };
    post(hwnd, Event::Ready(Box::new(snap)));
    Ok(())
}

fn tree(cat: &Catalog, name: &str) -> Result<(Index, FileTypes), String> {
    let (ix, _) = Index::build_with(&cat.recs, &cat.names, &cat.arena, name, LinkPolicy::Once).map_err(|e| e.to_string())?;
    let types = FileTypes::build(&ix);
    Ok((ix, types))
}

/// Tree + file types, and the catalog made durable at `cache`.
///
/// A mapped catalog already holds its data in the file: only a new header is written
/// (or the whole file, if a section outgrew it). A heap catalog (after a full scan) is
/// written on a second thread while the tree is built, then swapped for a mapping of
/// the file, which frees its heap copy. Afterwards the mapping leaves the working set
/// until the next update. A failed cache write only costs speed next time.
fn build(cat: &mut Catalog, cache: Option<&Path>, name: &str) -> Result<(Index, FileTypes), String> {
    if cat.is_mapped() {
        let built = tree(cat, name);
        if let Some(p) = cache {
            let _ = cat.persist(p);
        }
        cat.trim();
        return built;
    }
    let (built, saved) = thread::scope(|s| {
        let c: &Catalog = cat;
        let saver = cache.map(|p| s.spawn(move || c.save(p)));
        (tree(c, name), saver.map(|h| h.join()))
    });
    if let (Some(Ok(Ok(sv))), Some(p)) = (saved, cache) {
        cat.saved(sv);
        if let Ok(m) = Catalog::open_mapped(p) {
            *cat = m;
            cat.trim();
        }
    }
    built
}

fn run_ntfs(
    hwnd: isize,
    target: Target,
    drive: char,
    view_root: Option<String>,
    rescan: bool,
    use_cache: bool,
    id: u64,
) -> Result<(), String> {
    let spec = format!("{drive}:");
    post(hwnd, Event::Status(id, spec.clone()));
    let vol = match Volume::open(&spec, IoMode::Buffered) {
        Ok(v) => v,
        Err(ft_ntfs::Error::Io(io)) if io.raw_os_error() == Some(5) => {
            return Err(lang::t(S::NeedAdmin).into());
        }
        // No raw access after all: walk the directories instead.
        Err(_) => return run_walk(hwnd, target, Some(drive), id),
    };
    let slot = (drive.to_ascii_uppercase() as usize).wrapping_sub('A' as usize);
    let _volume = VOLUMES.get(slot).map(lock);
    if !current(id) {
        return Ok(());
    }
    let progress = Arc::new(ScanProgress::default());
    if let Ok(mut g) = PROGRESS.lock() {
        *g = Some(progress.clone());
    }
    let cache = if use_cache { ft_usn::default_cache_path(vol.boot.serial) } else { None };
    let o =
        OpenOptions { scan: ScanOptions { progress: Some(progress), ..Default::default() }, cache: cache.clone(), rescan, mapped: true };
    let t = Instant::now();
    let (mut cat, rep) = ft_usn::open(&vol, &o).map_err(|e| e.to_string())?;
    // A mapped catalog may hold in-place edits even when nothing reported a change:
    // always publish its header.
    let save_to = if cat.is_mapped() { cache.as_deref() } else { rep.save_to.as_deref() };
    let (ix, types) = build(&mut cat, save_to, &vol.name)?;
    let elapsed = t.elapsed();
    if !current(id) {
        return Ok(());
    }
    // Later trees of this target (rebuilt while following the journal) keep the scan's
    // own figures.
    let snap = |ix, types, live| Snapshot {
        generation: id,
        target: target.clone(),
        drive: Some(drive),
        ix,
        types,
        view_root: view_root.clone(),
        how: How::Volume,
        elapsed,
        live,
    };
    post(hwnd, Event::Ready(Box::new(snap(ix, types, false))));
    if rep.journal.is_none() || mode() == Live::Off {
        return Ok(());
    }

    let wake = Arc::new(Signal::new()?);
    *lock(&LIVE) = Some(wake.clone());
    // Registered first, checked second: a later `start` either is seen here or wakes us.
    let r = live(hwnd, id, &vol, &mut cat, cache.as_deref(), &wake, &snap);
    let mut g = lock(&LIVE);
    if g.as_ref().is_some_and(|s| Arc::ptr_eq(s, &wake)) {
        *g = None;
    }
    r
}

/// How often the cache header (a checksum over the whole catalog) is refreshed while
/// following the journal without pause. It is also written whenever the engine goes idle
/// or stops; until then a crash only costs a full scan next time.
const PERSIST_EVERY: Duration = Duration::from_secs(600);

/// Follow the journal until retired or switched off; see the module notes.
fn live(
    hwnd: isize,
    id: u64,
    vol: &Volume,
    cat: &mut Catalog,
    cache: Option<&Path>,
    wake: &Signal,
    snap: &dyn Fn(Index, FileTypes, bool) -> Snapshot,
) -> Result<(), String> {
    let mut dirty = false;
    let mut saved = Instant::now();
    let r = follow(hwnd, id, vol, cat, cache, wake, snap, &mut dirty, &mut saved);
    if dirty && let Some(p) = cache {
        let _ = cat.persist(p);
    }
    r
}

#[allow(clippy::too_many_arguments)]
fn follow(
    hwnd: isize,
    id: u64,
    vol: &Volume,
    cat: &mut Catalog,
    cache: Option<&Path>,
    wake: &Signal,
    snap: &dyn Fn(Index, FileTypes, bool) -> Snapshot,
    dirty: &mut bool,
    saved: &mut Instant,
) -> Result<(), String> {
    let ignore = cache.and_then(ft_usn::cache_ignore);
    let mut slot = ReadSlot::new().map_err(|e| e.to_string())?;
    let mut src = VolumeSource::new(vol).map_err(|e| e.to_string())?;
    let mut buf = vec![0u8; 1 << 20];
    let mut last_patch = Instant::now() - MIN_GAP;
    loop {
        // Reset before reading the flags: a change after this point sets it again.
        wake.reset();
        if !current(id) {
            return Ok(());
        }
        match mode() {
            Live::Off => return Ok(()),
            Live::Active if !ACTIVE.load(Ordering::SeqCst) => {
                // Going idle: make the cache current and hand its pages back.
                if std::mem::take(dirty)
                    && let Some(p) = cache
                {
                    let _ = cat.persist(p);
                    *saved = Instant::now();
                }
                cat.trim();
                wake.wait(INFINITE);
                continue;
            }
            _ => {}
        }
        if REBUILD.swap(false, Ordering::SeqCst) {
            let (ix, types) = build(cat, cache, &vol.name)?;
            *dirty = false;
            *saved = Instant::now();
            if current(id) {
                post(hwnd, Event::Ready(Box::new(snap(ix, types, true))));
            }
            continue;
        }
        if *dirty
            && saved.elapsed() >= PERSIST_EVERY
            && let Some(p) = cache
        {
            let _ = cat.persist(p);
            cat.trim();
            *dirty = false;
            *saved = Instant::now();
        }
        // Blocks inside NTFS until the journal grows (no polling) or we are woken.
        match journal::wait_for_entries(vol, &mut slot, cat.journal_id, cat.next_usn, &mut buf, wake.0) {
            Ok(true) => {}
            Ok(false) => continue,
            Err(_) => {
                post(hwnd, Event::Stale(id));
                return Ok(());
            }
        }
        let pause = MIN_GAP.saturating_sub(last_patch.elapsed()).max(Duration::from_millis(BURST_MS as u64));
        if wake.wait(pause.as_millis() as u32) || !current(id) {
            continue;
        }
        let ch = match journal::read_changes(vol, &mut slot, cat.journal_id, cat.next_usn, &mut buf, ignore.as_ref()) {
            Ok(c) => c,
            Err(_) => {
                post(hwnd, Event::Stale(id));
                return Ok(());
            }
        };
        if ch.records.is_empty() {
            cat.next_usn = ch.next_usn;
            *dirty = true;
            continue;
        }
        let speculative = ft_usn::minus_sorted(&cat.volatile(), &ch.records);
        let todo = ft_usn::merge_sorted(&ch.records, &speculative);
        let mut changes = Vec::new();
        cat.apply_tracked(&todo, &speculative, &mut src, &mut changes).map_err(|e| e.to_string())?;
        cat.next_usn = ch.next_usn;
        cat.update_hot(&ch.open, &ch.closed);
        *dirty = true;
        if changes.is_empty() || !current(id) {
            continue;
        }
        if changes.len() > PATCH_LIMIT {
            let (ix, types) = build(cat, cache, &vol.name)?;
            *dirty = false;
            *saved = Instant::now();
            post(hwnd, Event::Ready(Box::new(snap(ix, types, true))));
        } else {
            post(hwnd, Event::Patch(id, Box::new(Patch { changes })));
        }
        last_patch = Instant::now();
    }
}
