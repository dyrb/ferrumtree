//! FerrumTree command-line front end.

use std::collections::HashMap;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use ft_index::{BuildStats, Index, LinkPolicy, NONE, nflags};
use ft_ntfs::scan::{ScanOptions, scan};
use ft_ntfs::volume::{IoMode, ReadSlot, Volume, parse_drive_letter};
use ft_search::{Query, SearchIndex};
use ft_usn::{Catalog, Source, VolumeSource, journal};

mod sys;

const USAGE: &str = "\
FerrumTree command line. Reads volumes only; writes nothing but its cache file.

USAGE:
  ferrumtree scan   <VOL>            [opts]   scan, print timings, totals, largest items
  ferrumtree tree   <VOL> [PATH]     [opts]   largest folders as a tree
  ferrumtree search <VOL> <PATTERN>  [opts]   name search: text, *.ext, prefix*, a?c
  ferrumtree dupes  <VOL> [PATH]     [opts]   duplicate files: size, then head hash, then full hash
  ferrumtree watch  <VOL>            [opts]   follow the USN journal, keep the tree current
  ferrumtree verify <PATH>           [opts]   compare a folder against a Win32 directory walk
  ferrumtree check-usn <VOL>         [opts]   compare the journal-updated catalog with a full scan

NTFS volumes are read through the MFT. Other volumes (FAT32, exFAT, ReFS), network shares
(\\\\server\\share) and plain folders are read with a parallel directory walk.

Repeated runs load the catalog cache (FerrumTree-<serial>.ftc next to the program) and
apply only the changes recorded in the USN journal since the last run; the first run
does a full scan.

VOL is a drive letter (C, C:, E:\\) or a raw NTFS volume image file.

OPTIONS:
  --no-cache        never read or write the catalog cache (always a full scan)
  --cache FILE      use FILE as the catalog cache
  --rescan          ignore the cache, full scan, rewrite the cache
  --debounce-ms N   watch: collect changes for N ms after the first one (default 300)
  --links once|every  hard links in folder totals: once (default, sums = real usage) or
                    every link's size/count, on-disk once (WizTree-compatible)
  --exact           print sizes in bytes
  --direct          bypass the file cache (FILE_FLAG_NO_BUFFERING), i.e. cold reads
  --threads N       worker threads = reads in flight (default: logical CPUs)
  --chunk-kb N      read job size in KiB (default 4096)
  --qd N            max reads in flight (default: 4 for drives, all workers for images)
  --gap-kb N        split reads at runs of unused records >= N KiB (default 64)
  --no-bitmap       read every MFT record, ignore $MFT:$BITMAP
  --top N           rows to print (default 15)
  --depth N         tree depth (default 2)
  --limit N         search results to print (default 30)
  --search-threads N
  --min-kb N        dupes: ignore files smaller than N KiB (default 1024)
";

enum Cache {
    Default,
    Off,
    File(std::path::PathBuf),
}

struct Args {
    cmd: String,
    pos: Vec<String>,
    opts: ScanOptions,
    mode: IoMode,
    links: LinkPolicy,
    cache: Cache,
    rescan: bool,
    debounce_ms: u64,
    top: usize,
    depth: usize,
    limit: usize,
    search_threads: usize,
    min_kb: u64,
}

fn parse_args() -> Result<Args, String> {
    let mut it = std::env::args().skip(1);
    let cmd = it.next().ok_or_else(|| USAGE.to_string())?;
    let mut a = Args {
        cmd,
        pos: Vec::new(),
        opts: ScanOptions::default(),
        mode: IoMode::Buffered,
        links: LinkPolicy::Once,
        cache: Cache::Default,
        rescan: false,
        debounce_ms: 300,
        top: 15,
        depth: 2,
        limit: 30,
        search_threads: std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4),
        min_kb: 1024,
    };
    let num = |v: Option<String>, name: &str| -> Result<usize, String> {
        v.ok_or(format!("{name} needs a value"))?.parse().map_err(|_| format!("{name}: not a number"))
    };
    while let Some(s) = it.next() {
        match s.as_str() {
            "--direct" => a.mode = IoMode::Direct,
            "--exact" => EXACT.store(true, std::sync::atomic::Ordering::Relaxed),
            "--links" => {
                a.links = match it.next().as_deref() {
                    Some("once") => LinkPolicy::Once,
                    Some("every") => LinkPolicy::SizeEveryLink,
                    _ => return Err("--links expects once|every".into()),
                }
            }
            "--no-cache" => a.cache = Cache::Off,
            "--cache" => a.cache = Cache::File(it.next().ok_or("--cache needs a file")?.into()),
            "--rescan" => a.rescan = true,
            "--debounce-ms" => a.debounce_ms = num(it.next(), "--debounce-ms")? as u64,
            "--no-bitmap" => a.opts.use_bitmap = false,
            "--threads" => a.opts.threads = num(it.next(), "--threads")?,
            "--chunk-kb" => a.opts.chunk_bytes = num(it.next(), "--chunk-kb")? * 1024,
            "--qd" => a.opts.queue_depth = Some(num(it.next(), "--qd")?),
            "--gap-kb" => a.opts.gap_bytes = num(it.next(), "--gap-kb")? * 1024,
            "--top" => a.top = num(it.next(), "--top")?,
            "--depth" => a.depth = num(it.next(), "--depth")?,
            "--limit" => a.limit = num(it.next(), "--limit")?,
            "--min-kb" => a.min_kb = num(it.next(), "--min-kb")? as u64,
            "--search-threads" => a.search_threads = num(it.next(), "--search-threads")?,
            "-h" | "--help" => return Err(USAGE.to_string()),
            _ if s.starts_with("--") => return Err(format!("unknown option {s}\n\n{USAGE}")),
            _ => a.pos.push(s),
        }
    }
    Ok(a)
}

// ---------------------------------------------------------------------------------------

static EXACT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn bytes(n: u64) -> String {
    if EXACT.load(std::sync::atomic::Ordering::Relaxed) {
        return n.to_string();
    }
    const U: [&str; 6] = ["B", "KB", "MB", "GB", "TB", "PB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 { format!("{n} B") } else { format!("{v:.1} {}", U[i]) }
}

fn ms(d: Duration) -> String {
    format!("{:.1} ms", d.as_secs_f64() * 1e3)
}

fn group(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(' ');
        }
        out.push(c);
    }
    out
}

fn open_volume(spec: &str, a: &Args) -> Result<Volume, String> {
    Volume::open(spec, a.mode).map_err(|e| {
        let hint = match &e {
            ft_ntfs::Error::Io(io) if io.raw_os_error() == Some(5) => "\nhint: raw volume access needs an elevated (Administrator) prompt",
            _ => "",
        };
        format!("cannot open {spec}: {e}{hint}")
    })
}

fn open_options(a: &Args, serial: u64) -> ft_usn::OpenOptions {
    ft_usn::OpenOptions {
        scan: a.opts.clone(),
        cache: match &a.cache {
            Cache::Off => None,
            Cache::File(p) => Some(p.clone()),
            Cache::Default => ft_usn::default_cache_path(serial),
        },
        rescan: a.rescan,
        mapped: false,
    }
}

struct Loaded {
    drive: Option<char>,
    vol_name: String,
    /// NTFS volumes (MFT engine).
    boot: Option<ft_ntfs::BootSector>,
    rep: Option<ft_usn::OpenReport>,
    /// Everything else (directory walk).
    walk: Option<ft_fallback::WalkStats>,
    build: BuildStats,
    ix: Index,
    /// Sum of on-disk bytes over every in-use base record (independent of the tree).
    accounted_alloc: u64,
    in_use: u64,
    names: u64,
    hot: usize,
    catalog_heap: usize,
    /// Cache write (overlapped with the tree build), or the error.
    save: Option<Result<SaveInfo, String>>,
    t_total: Duration,
}

struct SaveInfo {
    time: Duration,
    bytes: u64,
    full: bool,
}

impl SaveInfo {
    fn describe(&self) -> String {
        format!("{}, {} written ({})", ms(self.time), bytes(self.bytes), if self.full { "full rewrite" } else { "changed pages only" })
    }
}

type Built = (Index, BuildStats, Option<Result<SaveInfo, String>>);

/// Build the tree while the catalog cache is written on another thread.
fn build_and_save(cat: &mut Catalog, save_to: Option<&std::path::Path>, root: &str, links: LinkPolicy) -> Result<Built, String> {
    let (built, saved) = std::thread::scope(|s| {
        let c: &Catalog = cat;
        let saver = save_to.map(|p| {
            s.spawn(move || {
                let t = Instant::now();
                c.save(p).map(|r| (t.elapsed(), r)).map_err(|e| format!("{}: {e}", p.display()))
            })
        });
        let built = Index::build_with(&c.recs, &c.names, &c.arena, root, links).map_err(|e| e.to_string());
        (built, saver.map(|h| h.join().expect("cache writer panicked")))
    });
    let info = saved.map(|r| {
        r.map(|(time, sv)| {
            let i = SaveInfo { time, bytes: sv.bytes_written, full: sv.full_rewrite };
            cat.saved(sv);
            i
        })
    });
    built.map(|(ix, b)| (ix, b, info))
}

/// A folder or UNC path that is not a bare drive letter: walked, not MFT-scanned.
fn walk_root(spec: &str) -> Option<String> {
    (parse_drive_letter(spec).is_none() && std::path::Path::new(spec).is_dir()).then(|| spec.to_string())
}

fn load_walk(root: &str, a: &Args, t: Instant, drive: Option<char>) -> Result<Loaded, String> {
    let w = ft_fallback::walk(root, a.opts.threads).map_err(|e| format!("cannot walk {root}: {e}"))?;
    let (in_use, accounted_alloc) = w.recs.iter().filter(|r| r.in_use()).fold((0u64, 0u64), |(n, s), r| (n + 1, s + r.alloc));
    let name = root.trim_end_matches(['\\', '/']).to_string();
    let (ix, build) = Index::build_with(&w.recs, &w.names, &w.arena, &name, a.links).map_err(|e| e.to_string())?;
    Ok(Loaded {
        drive,
        vol_name: name,
        boot: None,
        rep: None,
        build,
        ix,
        accounted_alloc,
        in_use,
        names: w.names.len() as u64,
        hot: 0,
        catalog_heap: w.recs.capacity() * 32 + w.names.capacity() * 16 + w.arena.capacity(),
        save: None,
        t_total: t.elapsed(),
        walk: Some(w.stats),
    })
}

fn load(vol_spec: &str, a: &Args) -> Result<Loaded, String> {
    let t = Instant::now();
    if let Some(root) = walk_root(vol_spec) {
        return load_walk(&root, a, t, None);
    }
    let vol = match Volume::open(vol_spec, a.mode) {
        // FAT32, exFAT, ReFS, ...: no MFT, walk the directories instead.
        Err(ft_ntfs::Error::NotNtfs) => {
            let letter = parse_drive_letter(vol_spec).ok_or("not an NTFS volume")?;
            return load_walk(&format!("{letter}:\\"), a, t, Some(letter));
        }
        _ => open_volume(vol_spec, a)?,
    };
    let (mut cat, rep) = ft_usn::open(&vol, &open_options(a, vol.boot.serial)).map_err(|e| format!("scan failed: {e}"))?;
    let (in_use, accounted_alloc) = cat.recs.iter().filter(|r| r.in_use()).fold((0u64, 0u64), |(n, s), r| (n + 1, s + r.alloc));
    let (ix, build, save) = build_and_save(&mut cat, rep.save_to.as_deref(), &vol.name, a.links)?;
    Ok(Loaded {
        drive: vol.drive,
        vol_name: vol.name.clone(),
        boot: Some(vol.boot),
        rep: Some(rep),
        walk: None,
        build,
        ix,
        accounted_alloc,
        in_use,
        names: cat.names.len() as u64,
        hot: cat.hot.len(),
        catalog_heap: cat.heap_bytes(),
        save,
        t_total: t.elapsed(),
    })
}

fn print_walk(l: &Loaded, w: &ft_fallback::WalkStats) {
    let b = &l.build;
    println!("Source   directory walk ({} threads, cluster {} B) of {}", w.threads, w.cluster_size, l.vol_name);
    println!(
        "         {} files, {} folders, {} unreadable folders in {} ({:.0} entries/s)",
        group(w.files),
        group(w.dirs),
        w.errors,
        ms(w.elapsed),
        (w.files + w.dirs) as f64 / w.elapsed.as_secs_f64().max(1e-9)
    );
    println!("  tree build + aggregate       {:>10}", ms(b.total()));
    println!("  TOTAL                        {:>10}", ms(l.t_total));
    println!("Tree     {} nodes: {} files, {} folders", group(b.nodes), group(l.ix.files[0] as u64), group(l.ix.dirs[0] as u64));
    let ixb = l.ix.heap_bytes();
    println!(
        "Memory   index {} ({:.1} B/node), walk buffers (freed after build) {}, peak working set {}",
        bytes(ixb as u64),
        ixb as f64 / l.ix.len().max(1) as f64,
        bytes(l.catalog_heap as u64),
        bytes(sys::peak_working_set())
    );
}

fn print_timings(l: &Loaded, a: &Args) {
    let (Some(r), Some(boot)) = (&l.rep, &l.boot) else {
        if let Some(w) = &l.walk {
            print_walk(l, w);
        }
        return;
    };
    let b = &l.build;
    println!(
        "Volume {}  cluster {} B, MFT record {} B, {} total",
        l.vol_name,
        boot.cluster_size,
        boot.record_size,
        bytes(boot.volume_bytes())
    );
    match (&r.source, &r.scan) {
        (Source::Incremental, _) => {
            let c = &r.changes;
            println!(
                "Source   cache + USN journal: {} entries ({}) -> {} records re-read ({} changed, {} created, {} deleted, {} extension segments)",
                group(c.entries),
                bytes(c.bytes),
                group(r.apply.records),
                r.apply.modified,
                r.apply.created,
                r.apply.deleted,
                r.apply.segments
            );
            println!("Timing");
            println!("  cache load                   {:>10}", ms(r.t_load));
            println!("  journal read                 {:>10}", ms(r.t_journal));
            println!("  re-read changed records      {:>10}", ms(r.t_apply));
        }
        (Source::Full(why), Some(s)) => {
            let mbps = s.bytes_read as f64 / s.t_pipeline.as_secs_f64().max(1e-9) / 1e6;
            println!("Source   full MFT scan ({why})");
            println!(
                "$MFT     {} in {} extent(s), {} records, {} read ({} records) in {} requests",
                bytes(s.mft_bytes),
                s.mft_extents,
                group(s.records_total),
                bytes(s.bytes_read),
                group(s.records_planned),
                s.read_calls
            );
            println!(
                "I/O      {} mode, chunk {} KiB, gap {} KiB, {} workers, QD {}",
                match a.mode {
                    IoMode::Buffered => "buffered",
                    IoMode::Direct => "direct",
                },
                a.opts.chunk_bytes / 1024,
                a.opts.gap_bytes / 1024,
                s.threads,
                s.queue_depth
            );
            println!("Timing");
            println!("  layout ($MFT runs + bitmap)  {:>10}", ms(s.t_layout));
            println!(
                "  read + parse (parallel)      {:>10}   {:.0} MB/s; summed over workers: read {}, parse {}",
                ms(s.t_pipeline),
                mbps,
                ms(s.io_time),
                ms(s.parse_time)
            );
            println!("  merge                        {:>10}", ms(s.t_merge));
        }
        (Source::Full(why), None) => println!("Source   full MFT scan ({why})"),
    }
    println!(
        "  tree build + aggregate       {:>10}   (group {}, reach {}, bfs {}, names {}, sum {})",
        ms(b.total()),
        ms(b.t_group),
        ms(b.t_reach),
        ms(b.t_bfs),
        ms(b.t_names),
        ms(b.t_aggregate)
    );
    match &l.save {
        Some(Ok(sv)) => println!("  cache save (parallel)        {}", sv.describe()),
        Some(Err(_)) => println!("  cache save                       failed"),
        None => println!("  cache save                   not needed"),
    }
    println!("  TOTAL                        {:>10}", ms(l.t_total));
    if let Some(Err(e)) = &l.save {
        println!("warning: catalog cache not saved: {e}");
    }
    if let Some(s) = &r.scan {
        let p = &s.parse;
        println!(
            "Records  {} in use = {} base + {} extension; {} corrupt; {} names ({} DOS aliases skipped)",
            group(p.in_use),
            group(p.base),
            group(p.extension),
            p.corrupt,
            group(p.names),
            group(p.dos_names_skipped)
        );
    }
    println!(
        "Catalog  {} records in use, {} names, {} files open for writing (re-read every update)",
        group(l.in_use),
        group(l.names),
        l.hot
    );
    println!(
        "Tree     {} nodes: {} files, {} folders, {} extra hard links, {} orphan roots",
        group(b.nodes),
        group(l.ix.files[0] as u64),
        group(l.ix.dirs[0] as u64),
        group(b.dup_links),
        b.orphan_roots
    );
    let ixb = l.ix.heap_bytes();
    println!(
        "Memory   index {} ({:.1} B/node), catalog (freed after build) {}, peak working set {}",
        bytes(ixb as u64),
        ixb as f64 / l.ix.len().max(1) as f64,
        bytes(l.catalog_heap as u64),
        bytes(sys::peak_working_set())
    );
}

fn print_volume_check(l: &Loaded) {
    if let Some((total, free)) = l.drive.and_then(sys::disk_space) {
        let used = total - free;
        println!(
            "Check    volume used {} (GetDiskFreeSpaceEx) vs on-disk bytes found in MFT {} ({:+.3}%)",
            bytes(used),
            bytes(l.accounted_alloc),
            (l.accounted_alloc as f64 - used as f64) / used.max(1) as f64 * 100.0
        );
    }
}

fn row(ix: &Index, n: u32, total: u64, indent: usize) {
    let i = n as usize;
    let pct = ix.size[i] as f64 / total.max(1) as f64 * 100.0;
    let kind = if ix.is_dir(n) { format!("{:>9} files", group(ix.files[i] as u64)) } else { String::new() };
    let mark = if ix.flags[i] & nflags::DUP_LINK != 0 { " (hard link)" } else { "" };
    println!(
        "{:>14} {:>14} {:>5.1}%  {:<15} {}{}{}",
        bytes(ix.size[i]),
        bytes(ix.alloc[i]),
        pct,
        kind,
        "  ".repeat(indent),
        if indent == 0 { ix.path(n) } else { ix.name(n).to_string() },
        mark
    );
}

fn header() {
    println!("{:>14} {:>14} {:>6}  {:<15} Path", "Size", "On disk", "%", "");
}

fn print_tree(ix: &Index, n: u32, depth: usize, top: usize, level: usize, total: u64) {
    row(ix, n, total, level);
    if level >= depth {
        return;
    }
    for c in ix.children_by_size(n).into_iter().filter(|&c| ix.is_dir(c)).take(top) {
        print_tree(ix, c, depth, top, level + 1, total);
    }
}

fn cmd_scan(a: &Args) -> Result<(), String> {
    let vol = a.pos.first().ok_or("scan: missing volume")?;
    let l = load(vol, a)?;
    print_timings(&l, a);
    print_volume_check(&l);
    let ix = &l.ix;
    println!("\nLargest folders under the root");
    header();
    print_tree(ix, 0, 1, a.top, 0, ix.size[0]);
    println!("\nLargest files");
    header();
    for n in ix.top_files(a.top) {
        row(ix, n, ix.size[0], 0);
    }
    Ok(())
}

fn cmd_tree(a: &Args) -> Result<(), String> {
    let vol = a.pos.first().ok_or("tree: missing volume")?;
    let l = load(vol, a)?;
    let ix = &l.ix;
    let start = match a.pos.get(1) {
        Some(p) => ix.find(p).ok_or(format!("path not found: {p}"))?,
        None => 0,
    };
    println!("scanned + indexed in {}", ms(l.t_total));
    header();
    print_tree(ix, start, a.depth, a.top, 0, ix.size[start as usize]);
    Ok(())
}

fn cmd_search(a: &Args) -> Result<(), String> {
    let vol = a.pos.first().ok_or("search: missing volume")?;
    let pat = a.pos.get(1).ok_or("search: missing pattern")?;
    let l = load(vol, a)?;
    let ix = &l.ix;
    let t = Instant::now();
    let si = SearchIndex::build(ix);
    let t_build = t.elapsed();
    let q = Query::parse(pat);
    // Warm-up run, then the timed run.
    let _ = si.search(&q, a.search_threads);
    let t = Instant::now();
    let hits = si.search(&q, a.search_threads);
    let t_q = t.elapsed();
    println!(
        "{} nodes indexed; search index built in {} ({}); query {:?}: {} matches in {}",
        group(ix.len() as u64),
        ms(t_build),
        bytes(si.heap_bytes() as u64),
        pat,
        group(hits.len() as u64),
        ms(t_q)
    );
    let mut shown: Vec<u32> = hits;
    shown.sort_unstable_by_key(|&n| std::cmp::Reverse(ix.size[n as usize]));
    header();
    for &n in shown.iter().take(a.limit) {
        row(ix, n, ix.size[0], 0);
    }
    Ok(())
}

fn cmd_check_usn(a: &Args) -> Result<(), String> {
    let spec = a.pos.first().ok_or("check-usn: missing volume")?;
    let vol = open_volume(spec, a)?;
    let o = open_options(a, vol.boot.serial);
    if o.cache.is_none() {
        return Err("check-usn needs the catalog cache".into());
    }
    let (mut inc, rep) = ft_usn::open(&vol, &o).map_err(|e| e.to_string())?;
    if let Some(p) = &rep.save_to {
        let sv = inc.save(p).map_err(|e| e.to_string())?;
        inc.saved(sv);
    }
    if let Source::Full(why) = &rep.source {
        println!("no usable cache ({why}): did a full scan and saved it. Change some files and run check-usn again.");
        return Ok(());
    }
    println!(
        "incremental catalog: {} journal entries -> {} records re-read in {}",
        group(rep.changes.entries),
        group(rep.apply.records),
        ms(rep.t_apply)
    );
    let t = Instant::now();
    let full = Catalog::from_scan(scan(&vol, &a.opts).map_err(|e| e.to_string())?, None);
    println!("reference full scan: {}", ms(t.elapsed()));
    // Records touched between the incremental read and the end of the reference scan, and
    // files open for writing (they change without journal entries), legitimately differ.
    let mut slot = ReadSlot::new().map_err(|e| e.to_string())?;
    let mut buf = vec![0u8; 1 << 20];
    let since = journal::read_changes(&vol, &mut slot, inc.journal_id, inc.next_usn, &mut buf, None).map_err(|e| e.to_string())?;
    let volatile = ft_usn::merge_sorted(&since.records, &inc.volatile());
    let d = ft_usn::diff(&inc, &full, &volatile);
    // NTFS refreshes a directory's own timestamp lazily, after the child operation that
    // the journal records, and without an entry of its own: such lag is expected.
    let (lazy_dir_time, real): (Vec<_>, Vec<_>) =
        d.records.iter().partition(|(_, x, y)| x.is_dir() && y.is_dir() && ft_ntfs::Rec { mtime: y.mtime, ..*x } == *y);
    println!("compared {} in-use records ({} changed meanwhile or open for writing, excluded)", group(d.compared), volatile.len());
    println!(
        "  {} mismatches, {} name-set mismatches; {} directories with a lagging timestamp (NTFS lazy update, sizes equal)",
        real.len(),
        d.names.len(),
        lazy_dir_time.len()
    );
    for (r, x, y) in real.iter().take(10) {
        println!(
            "  record {r}: incremental size {} alloc {} seq {} flags {:#x} attrs {:#x} mtime {} | full size {} alloc {} seq {} flags {:#x} attrs {:#x} mtime {}",
            x.size, x.alloc, x.seq, x.flags, x.attrs, x.mtime, y.size, y.alloc, y.seq, y.flags, y.attrs, y.mtime
        );
    }
    for r in d.names.iter().take(10) {
        println!("  names differ for record {r}");
    }
    if real.is_empty() && d.names.is_empty() {
        println!("MATCH");
        Ok(())
    } else {
        Err("incremental catalog differs from a full scan".into())
    }
}

fn cmd_watch(a: &Args) -> Result<(), String> {
    let spec = a.pos.first().ok_or("watch: missing volume")?;
    let vol = open_volume(spec, a)?;
    let o = open_options(a, vol.boot.serial);
    let (mut cat, rep) = ft_usn::open(&vol, &o).map_err(|e| e.to_string())?;
    if rep.journal.is_none() {
        return Err("watch needs an active USN journal".into());
    }
    let (ix, _, _) = build_and_save(&mut cat, rep.save_to.as_deref(), &vol.name, a.links)?;
    println!("watching {}: {} in {} files. Ctrl+C to stop.", vol.name, bytes(ix.size[0]), group(ix.files[0] as u64));
    drop(ix);
    let mut slot = ReadSlot::new().map_err(|e| e.to_string())?;
    let mut src = VolumeSource::new(&vol).map_err(|e| e.to_string())?;
    let mut buf = vec![0u8; 1 << 20];
    let ignore = o.cache.as_deref().and_then(ft_usn::cache_ignore);
    loop {
        // Blocks inside NTFS until the journal grows: no polling.
        journal::wait_for_entries(&vol, &mut slot, cat.journal_id, cat.next_usn, &mut buf, std::ptr::null_mut())
            .map_err(|e| e.to_string())?;
        std::thread::sleep(Duration::from_millis(a.debounce_ms));
        let t = Instant::now();
        let ch =
            journal::read_changes(&vol, &mut slot, cat.journal_id, cat.next_usn, &mut buf, ignore.as_ref()).map_err(|e| e.to_string())?;
        if ch.records.is_empty() {
            // Only our own cache writes: nothing to show, nothing to save.
            cat.next_usn = ch.next_usn;
            continue;
        }
        let speculative = ft_usn::minus_sorted(&cat.volatile(), &ch.records);
        let todo = ft_usn::merge_sorted(&ch.records, &speculative);
        let st = cat.apply(&todo, &speculative, &mut src).map_err(|e| e.to_string())?;
        cat.next_usn = ch.next_usn;
        cat.update_hot(&ch.open, &ch.closed);
        let t_apply = t.elapsed();
        // Keep the cache current too: a delta save writes only the touched pages.
        let (ix, b, saved) = build_and_save(&mut cat, o.cache.as_deref(), &vol.name, a.links)?;
        println!(
            "{} entries -> {} records (+{} -{}) applied in {}, tree rebuilt in {}, cache {}: {} in {} files",
            group(ch.entries),
            st.records,
            st.created,
            st.deleted,
            ms(t_apply),
            ms(b.total()),
            match &saved {
                Some(Ok(sv)) => sv.describe(),
                Some(Err(e)) => format!("not saved ({e})"),
                None => "off".into(),
            },
            bytes(ix.size[0]),
            group(ix.files[0] as u64)
        );
    }
}

// ---------------------------------------------------------------------------------------
// verify: compare the index against an independent Win32 directory walk.
// ---------------------------------------------------------------------------------------

#[derive(Default)]
struct Walk {
    files: HashMap<String, u64>,
    dirs: u64,
    errors: u64,
}

fn walk(dir: &std::path::Path, rel: &str, w: &mut Walk) {
    use std::os::windows::fs::MetadataExt;
    let Ok(rd) = std::fs::read_dir(dir) else {
        w.errors += 1;
        return;
    };
    for ent in rd.flatten() {
        let name = ent.file_name().to_string_lossy().into_owned();
        let r = if rel.is_empty() { name.clone() } else { format!("{rel}\\{name}") };
        let Ok(md) = ent.metadata() else {
            w.errors += 1;
            continue;
        };
        let attrs = md.file_attributes();
        let is_reparse = attrs & 0x400 != 0;
        if attrs & 0x10 != 0 {
            // FILE_ATTRIBUTE_DIRECTORY: real dirs, junctions and directory symlinks.
            w.dirs += 1;
            if !is_reparse {
                walk(&ent.path(), &r, w);
            }
        } else {
            // The size cached in the directory entry (FindFirstFile) is updated lazily by
            // NTFS (open files, secondary hard links); open the file for the real size.
            let size = std::fs::symlink_metadata(ent.path()).map(|m| m.file_size()).unwrap_or(md.file_size());
            w.files.insert(r.to_lowercase(), size);
        }
    }
}

fn cmd_verify(a: &Args) -> Result<(), String> {
    let path = a.pos.first().ok_or("verify: missing path")?;
    let letter = path.get(..2).and_then(parse_drive_letter).ok_or("verify: path must start with a drive letter")?;
    let l = load(&format!("{letter}:"), a)?;
    let ix = &l.ix;
    let start = ix.find(path).ok_or(format!("not in index: {path}"))?;
    let t = Instant::now();
    let mut w = Walk::default();
    walk(std::path::Path::new(path), "", &mut w);
    let t_walk = t.elapsed();

    // Index side: every file node below `start`, keyed by relative path.
    let mut idx_files: HashMap<String, u64> = HashMap::new();
    let mut idx_dirs = 0u64;
    let mut stack = vec![start];
    while let Some(n) = stack.pop() {
        for c in ix.children(n) {
            if ix.is_dir(c) {
                idx_dirs += 1;
                if ix.flags[c as usize] & nflags::REPARSE == 0 {
                    stack.push(c);
                }
            } else {
                let mut parts = Vec::new();
                let mut x = c;
                while x != start && x != NONE {
                    parts.push(ix.name(x));
                    x = ix.parent[x as usize];
                }
                parts.reverse();
                idx_files.insert(parts.join("\\").to_lowercase(), ix.size[c as usize]);
            }
        }
    }

    println!(
        "index: scan+build {}; Win32 walk {} ({} files, {} dirs, {} access errors)",
        ms(l.t_total),
        ms(t_walk),
        group(w.files.len() as u64),
        group(w.dirs),
        w.errors
    );
    let mut only_idx: Vec<_> = idx_files.keys().filter(|k| !w.files.contains_key(*k)).collect();
    let mut only_walk: Vec<_> = w.files.keys().filter(|k| !idx_files.contains_key(*k)).collect();
    let mut size_diff: Vec<_> = w.files.iter().filter_map(|(k, &s)| idx_files.get(k).filter(|&&i| i != s).map(|&i| (k, s, i))).collect();
    only_idx.sort();
    only_walk.sort();
    size_diff.sort();
    let sum_w: u64 = w.files.values().sum();
    let sum_i: u64 = idx_files.values().sum();
    println!(
        "files: index {} / walk {}   dirs: index {} / walk {}   bytes: index {} / walk {}",
        group(idx_files.len() as u64),
        group(w.files.len() as u64),
        group(idx_dirs),
        group(w.dirs),
        group(sum_i),
        group(sum_w)
    );
    for k in only_idx.iter().take(10) {
        println!("  only in index: {k}");
    }
    for k in only_walk.iter().take(10) {
        println!("  only in walk:  {k}");
    }
    for (k, s, i) in size_diff.iter().take(10) {
        println!("  size differs:  {k}  walk {s}  index {i}");
    }
    let ok = only_idx.is_empty() && only_walk.is_empty() && size_diff.is_empty() && idx_dirs == w.dirs;
    println!(
        "{}  ({} only-index, {} only-walk, {} size mismatches)",
        if ok { "MATCH" } else { "DIFFERENCES" },
        only_idx.len(),
        only_walk.len(),
        size_diff.len()
    );
    if ok { Ok(()) } else { Err("verification found differences".into()) }
}

fn cmd_dupes(a: &Args) -> Result<(), String> {
    let vol = a.pos.first().ok_or("dupes: missing volume")?;
    let l = load(vol, a)?;
    let ix = &l.ix;
    let root = match a.pos.get(1) {
        Some(p) => ix.find(p).ok_or(format!("path not found: {p}"))?,
        None => 0,
    };
    let o = ft_dupes::Options { min_size: a.min_kb * 1024, threads: a.opts.threads.min(8) };
    let rep = ft_dupes::find(ix, root, &o, &std::sync::atomic::AtomicBool::new(false));
    println!(
        "{}: {} files >= {} KiB, {} share a size; {} head-hashed, {} fully hashed, {} read, {} unreadable/cloud; {}",
        ix.path(root),
        group(rep.files_considered),
        a.min_kb,
        group(rep.same_size),
        group(rep.head_hashed),
        group(rep.full_hashed),
        bytes(rep.bytes_read),
        rep.skipped,
        ms(rep.elapsed)
    );
    println!("{} duplicate groups, {} reclaimable\n", group(rep.groups.len() as u64), bytes(rep.wasted()));
    for g in rep.groups.iter().take(a.limit) {
        println!("{} x {}  (wasted {})", g.nodes.len(), bytes(g.size), bytes(g.wasted()));
        for &n in &g.nodes {
            println!("    {}", ix.path(n));
        }
    }
    Ok(())
}

/// One MFT record as NTFS has it in memory (FSCTL) and as it is on the device (raw).
fn main() -> ExitCode {
    let a = match parse_args() {
        Ok(a) => a,
        Err(msg) => {
            eprintln!("{msg}");
            return ExitCode::from(2);
        }
    };
    let r = match a.cmd.as_str() {
        "scan" => cmd_scan(&a),
        "tree" => cmd_tree(&a),
        "search" => cmd_search(&a),
        "verify" => cmd_verify(&a),
        "check-usn" => cmd_check_usn(&a),
        "watch" => cmd_watch(&a),
        "dupes" => cmd_dupes(&a),
        _ => Err(format!("unknown command {}\n\n{USAGE}", a.cmd)),
    };
    match r {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}
