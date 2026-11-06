//! Parallel MFT scanner.
//!
//! ```text
//!  $MFT record 0 ──► data runs + $MFT:$BITMAP ──► read plan: record-aligned jobs that
//!                                                 start/end on used records and are split
//!                                                 at large runs of unused records
//!
//!  worker 1..N:  claim next job (atomic counter)
//!                ├─ read it into the worker's own aligned buffer (overlapped request(s))
//!                └─ fixup + attribute walk straight into that job's slice of the table
//! ```
//!
//! With N workers there are up to N reads in flight (device queue depth), and when the
//! data comes from the system cache the copies run on N cores instead of one.
//! Every job owns a disjoint `&mut [Rec]` carved out of one preallocated table, so the
//! workers write without `unsafe`; extension-record data and names live in per-thread
//! [`Sink`]s and are merged at the end. No allocation per job.

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::boot::BootSector;
use crate::error::{Error, Result};
use crate::record::{self, AttrValue, NameEntry, ParseStats, Rec, Sink, attr, fixup_record, parse_attribute_list};
use crate::runlist::Extent;
use crate::volume::{AlignedBuf, IO_ALIGN, ReadSlot, Volume};

#[derive(Clone, Debug)]
pub struct ScanOptions {
    /// Worker threads. Each one reads and parses whole jobs, so this is also the number
    /// of read requests in flight.
    pub threads: usize,
    /// Bytes per read job.
    pub chunk_bytes: usize,
    /// Reads in flight at most. `None` = automatic: 4 for a raw drive (budget SATA SSDs
    /// and HDDs slow down under deeper queues), all workers for an image file (cached
    /// reads are memory copies that scale with cores).
    pub queue_depth: Option<usize>,
    /// Use `$MFT:$BITMAP` to skip never-used / freed record ranges.
    pub use_bitmap: bool,
    /// A run of unused records at least this long splits a read in two (with the bitmap).
    /// Smaller gaps are read through: one larger request beats two small ones.
    pub gap_bytes: usize,
    /// Optional live progress (records planned / parsed), readable from another thread.
    pub progress: Option<Arc<ScanProgress>>,
}

/// Lock-free scan progress: `total` is set once the read plan exists, `done` grows as
/// workers finish jobs; `files` and `dirs` count the records in use found so far.
#[derive(Debug, Default)]
pub struct ScanProgress {
    pub total: AtomicU64,
    pub done: AtomicU64,
    pub files: AtomicU64,
    pub dirs: AtomicU64,
}

impl ScanProgress {
    /// Fraction in 0..=1 (0 before the plan exists).
    pub fn fraction(&self) -> f64 {
        let t = self.total.load(Ordering::Relaxed);
        if t == 0 { 0.0 } else { (self.done.load(Ordering::Relaxed) as f64 / t as f64).min(1.0) }
    }
}

impl Default for ScanOptions {
    fn default() -> Self {
        ScanOptions {
            threads: thread::available_parallelism().map(|n| n.get()).unwrap_or(4),
            chunk_bytes: 4 << 20,
            queue_depth: None,
            use_bitmap: true,
            gap_bytes: 64 << 10,
            progress: None,
        }
    }
}

/// Where `$MFT` lives and which records are in use.
#[derive(Clone, Debug)]
pub struct MftLayout {
    pub extents: Vec<Extent>,
    /// Extents of `$MFT:$BITMAP` and of a non-resident `$MFT:$ATTRIBUTE_LIST`.
    pub aux_extents: Vec<Extent>,
    pub data_size: u64,
    pub initialized: u64,
    pub bitmap: Option<Vec<u8>>,
    pub record_size: u32,
    pub cluster_size: u32,
}

impl MftLayout {
    /// Records that can hold data (bounded by the valid data length).
    pub fn records(&self) -> u64 {
        self.data_size.min(self.initialized) / self.record_size as u64
    }

    #[inline]
    fn used(&self, i: u64) -> bool {
        match &self.bitmap {
            Some(b) => b.get((i / 8) as usize).is_some_and(|x| x >> (i % 8) & 1 != 0),
            None => true,
        }
    }

    pub fn used_records(&self) -> u64 {
        match &self.bitmap {
            Some(_) => (0..self.records()).filter(|&i| self.used(i)).count() as u64,
            None => self.records(),
        }
    }
}

#[derive(Default, Clone, Debug)]
pub struct ScanStats {
    pub parse: ParseStats,
    pub mft_bytes: u64,
    pub mft_extents: usize,
    pub records_total: u64,
    pub records_planned: u64,
    pub bytes_read: u64,
    pub read_calls: u64,
    pub jobs: usize,
    pub ext_applied: u64,
    pub ext_dropped: u64,
    pub t_layout: Duration,
    pub t_pipeline: Duration,
    pub t_merge: Duration,
    pub threads: usize,
    pub queue_depth: usize,
    /// Time spent in reads, summed over workers (device wait or cache copy).
    pub io_time: Duration,
    /// Time spent parsing, summed over workers.
    pub parse_time: Duration,
}

impl ScanStats {
    pub fn total(&self) -> Duration {
        self.t_layout + self.t_pipeline + self.t_merge
    }
}

/// Result of a full MFT scan: record table + names.
pub struct Scan {
    pub boot: BootSector,
    /// Indexed by MFT record number.
    pub recs: Vec<Rec>,
    pub names: Vec<NameEntry>,
    pub arena: Vec<u8>,
    pub stats: ScanStats,
}

impl Scan {
    pub fn name(&self, e: &NameEntry) -> &str {
        std::str::from_utf8(&self.arena[e.off as usize..e.off as usize + e.len as usize]).unwrap_or("")
    }
}

#[derive(Debug, Clone, Copy)]
struct Piece {
    disk: u64,
    buf_off: u32,
    len: u32,
}

#[derive(Debug, Clone)]
struct Job {
    first: u64,
    count: u32,
    pieces: Vec<Piece>,
}

// ---------------------------------------------------------------------------------------
// $MFT layout
// ---------------------------------------------------------------------------------------

fn round_up(v: u64, a: u64) -> u64 {
    v.div_ceil(a) * a
}

/// Read an entire non-resident attribute (cluster-granular reads, sparse runs = zeros).
pub fn read_nonresident(vol: &Volume, extents: &[Extent], size: u64) -> Result<Vec<u8>> {
    let cs = vol.boot.cluster_size as u64;
    let total = round_up(size.max(1), cs);
    if total > 1 << 32 {
        return Err(Error::Unsupported("metadata attribute too large"));
    }
    let mut buf = AlignedBuf::new(total as usize);
    for e in extents {
        let start = e.vcn * cs;
        if start >= total {
            break;
        }
        let len = (e.len * cs).min(total - start);
        if let Some(lcn) = e.lcn {
            vol.read_at(lcn * cs, &mut buf[start as usize..(start + len) as usize])?;
        }
    }
    Ok(buf[..size as usize].to_vec())
}

/// Byte offset -> (disk offset) pieces through the MFT's extents.
fn map_range(extents: &[Extent], cs: u64, start: u64, end: u64, out: &mut Vec<Piece>) -> Result<()> {
    let mut pos = start;
    let mut i = extents.partition_point(|e| (e.vcn + e.len) * cs <= start);
    while pos < end {
        let e = extents.get(i).ok_or(Error::BadMftRecord("$MFT shorter than its data size"))?;
        let es = e.vcn * cs;
        let ee = (e.vcn + e.len) * cs;
        if pos < es {
            return Err(Error::BadMftRecord("hole in $MFT runlist"));
        }
        let lcn = e.lcn.ok_or(Error::BadMftRecord("sparse $MFT run"))?;
        let n = ee.min(end) - pos;
        out.push(Piece { disk: lcn * cs + (pos - es), buf_off: (pos - start) as u32, len: n as u32 });
        pos += n;
        i += 1;
    }
    Ok(())
}

#[derive(Default)]
struct Segments {
    data: Vec<(u64, Vec<Extent>)>,
    data_size: Option<(u64, u64)>,
    bitmap: Vec<(u64, Vec<Extent>)>,
    bitmap_size: Option<u64>,
    bitmap_resident: Option<Vec<u8>>,
}

fn collect_segments(attrs: &[record::Attribute], seg: &mut Segments) {
    for a in attrs.iter().filter(|a| a.name.is_empty()) {
        match (a.ty, &a.value) {
            (attr::DATA, AttrValue::NonResident(nr)) => {
                if nr.lowest_vcn == 0 {
                    seg.data_size = Some((nr.data_size, nr.initialized));
                }
                if !seg.data.iter().any(|(v, _)| *v == nr.lowest_vcn) {
                    seg.data.push((nr.lowest_vcn, nr.extents.clone()));
                }
            }
            (attr::BITMAP, AttrValue::NonResident(nr)) => {
                if nr.lowest_vcn == 0 {
                    seg.bitmap_size = Some(nr.data_size);
                }
                if !seg.bitmap.iter().any(|(v, _)| *v == nr.lowest_vcn) {
                    seg.bitmap.push((nr.lowest_vcn, nr.extents.clone()));
                }
            }
            (attr::BITMAP, AttrValue::Resident(v)) => seg.bitmap_resident = Some(v.clone()),
            _ => {}
        }
    }
}

fn flatten(mut segs: Vec<(u64, Vec<Extent>)>) -> Vec<Extent> {
    segs.sort_by_key(|(v, _)| *v);
    segs.into_iter().flat_map(|(_, e)| e).collect()
}

/// Locate `$MFT` and read its bitmap, following `$ATTRIBUTE_LIST` if the MFT is so
/// fragmented that its runlist spills into extension records.
pub fn load_mft_layout(vol: &Volume) -> Result<MftLayout> {
    let b = vol.boot;
    let rs = b.record_size as usize;
    let cs = b.cluster_size as u64;
    let mut buf = AlignedBuf::new(rs.max(IO_ALIGN));
    vol.read_at(b.mft_lcn * cs, &mut buf[..rs])?;
    let rec0 = &mut buf[..rs];
    fixup_record(rec0)?;
    let attrs = record::attributes(rec0)?;
    let mut seg = Segments::default();
    collect_segments(&attrs, &mut seg);
    let mut aux_extents = Vec::new();

    if let Some(al) = attrs.iter().find(|a| a.ty == attr::ATTRIBUTE_LIST) {
        let raw = match &al.value {
            AttrValue::Resident(v) => v.clone(),
            AttrValue::NonResident(nr) => {
                aux_extents.extend_from_slice(&nr.extents);
                read_nonresident(vol, &nr.extents, nr.data_size)?
            }
        };
        let mut pending: Vec<u64> = parse_attribute_list(&raw)?
            .into_iter()
            .filter(|e| (e.ty == attr::DATA || e.ty == attr::BITMAP) && e.name_len == 0 && e.segment != 0)
            .map(|e| e.segment)
            .collect();
        pending.sort_unstable();
        pending.dedup();
        // Extension records are located through the runs known so far; repeat while the
        // known part of the runlist grows.
        while !pending.is_empty() {
            let known = flatten(seg.data.clone());
            let before = pending.len();
            let mut rest = Vec::new();
            for &r in &pending {
                let off = r * rs as u64;
                let mut pieces = Vec::new();
                if map_range(&known, cs, off, off + rs as u64, &mut pieces).is_err() {
                    rest.push(r);
                    continue;
                }
                for p in &pieces {
                    vol.read_at(p.disk, &mut buf[p.buf_off as usize..(p.buf_off + p.len) as usize])?;
                }
                let rec = &mut buf[..rs];
                fixup_record(rec)?;
                collect_segments(&record::attributes(rec)?, &mut seg);
            }
            if rest.len() == before {
                return Err(Error::BadMftRecord("unresolvable $MFT attribute list"));
            }
            pending = rest;
        }
    }

    let (data_size, initialized) = seg.data_size.ok_or(Error::BadMftRecord("no $DATA"))?;
    let extents = flatten(seg.data);
    let bitmap = if let Some(v) = seg.bitmap_resident {
        Some(v)
    } else if let Some(sz) = seg.bitmap_size {
        let bx = flatten(seg.bitmap);
        aux_extents.extend_from_slice(&bx);
        Some(read_nonresident(vol, &bx, sz)?)
    } else {
        None
    };
    Ok(MftLayout { extents, aux_extents, data_size, initialized, bitmap, record_size: b.record_size, cluster_size: b.cluster_size })
}

/// Cut the MFT into read jobs of at most `chunk_bytes`. With the bitmap, jobs start and
/// end on used records and are split wherever `gap_bytes` or more of unused records lie
/// between two used ones.
fn plan(layout: &MftLayout, chunk_bytes: usize, use_bitmap: bool, gap_bytes: usize) -> Result<Vec<Job>> {
    let rs = layout.record_size as u64;
    let cs = layout.cluster_size as u64;
    let per = (chunk_bytes as u64 / rs).max(1);
    let n = layout.records();
    let bitmap = use_bitmap && layout.bitmap.is_some();
    let gap = if bitmap { (gap_bytes as u64 / rs).max(1) } else { u64::MAX };
    let mut jobs = Vec::new();
    let mut i = 0u64;
    while i < n {
        if bitmap {
            // Skip whole zero bytes of the bitmap quickly, then single bits.
            let bm = layout.bitmap.as_deref().unwrap();
            while i < n && i.is_multiple_of(8) && bm.get((i / 8) as usize) == Some(&0) {
                i += 8;
            }
            while i < n && !layout.used(i) {
                i += 1;
            }
            if i >= n {
                break;
            }
        }
        let s = i;
        let mut last = i;
        let mut j = i + 1;
        let limit = (s + per).min(n);
        while j < limit {
            if !bitmap || layout.used(j) {
                last = j;
            } else if j - last >= gap {
                break;
            }
            j += 1;
        }
        let e = last + 1;
        let mut pieces = Vec::with_capacity(2);
        map_range(&layout.extents, cs, s * rs, e * rs, &mut pieces)?;
        jobs.push(Job { first: s, count: (e - s) as u32, pieces });
        i = e;
    }
    Ok(jobs)
}

// ---------------------------------------------------------------------------------------
// Pipeline: every worker pulls the next job, reads it into its own buffer, parses it.
// ---------------------------------------------------------------------------------------

/// Minimal counting semaphore bounding concurrent device reads.
struct Gate {
    free: Mutex<usize>,
    cv: Condvar,
}

impl Gate {
    fn new(n: usize) -> Self {
        Gate { free: Mutex::new(n.max(1)), cv: Condvar::new() }
    }
    fn acquire(&self) -> GatePass<'_> {
        let mut f = self.free.lock().unwrap();
        while *f == 0 {
            f = self.cv.wait(f).unwrap();
        }
        *f -= 1;
        GatePass(self)
    }
}

struct GatePass<'a>(&'a Gate);

impl Drop for GatePass<'_> {
    fn drop(&mut self) {
        *self.0.free.lock().unwrap() += 1;
        self.0.cv.notify_one();
    }
}

#[derive(Default)]
struct WorkerStats {
    bytes: u64,
    calls: u64,
    read: Duration,
    parse: Duration,
}

/// Read all pieces of `job` into `buf` (all requests in flight at once, then wait).
fn read_job(vol: &Volume, job: &Job, buf: &mut AlignedBuf, slots: &mut Vec<ReadSlot>, st: &mut WorkerStats) -> Result<()> {
    while slots.len() < job.pieces.len() {
        slots.push(ReadSlot::new()?);
    }
    let mut started = 0;
    let mut err: Option<std::io::Error> = None;
    for (p, slot) in job.pieces.iter().zip(slots.iter_mut()) {
        // SAFETY: every started request is waited for below before `buf` is touched again.
        match unsafe { vol.start_read(p.disk, buf.as_mut_ptr().add(p.buf_off as usize), p.len, slot) } {
            Ok(()) => started += 1,
            Err(e) => {
                err = Some(e);
                break;
            }
        }
    }
    st.calls += started as u64;
    for slot in &mut slots[..started] {
        match vol.finish_read(slot) {
            Ok(n) => st.bytes += n as u64,
            Err(e) => {
                err.get_or_insert(e);
            }
        }
    }
    match err {
        Some(e) => Err(Error::Io(e)),
        None => Ok(()),
    }
}

/// Scan the whole MFT of `vol`.
pub fn scan(vol: &Volume, opts: &ScanOptions) -> Result<Scan> {
    let t0 = Instant::now();
    let layout = load_mft_layout(vol)?;
    let rs = layout.record_size as usize;
    let n = layout.records();
    if n > u32::MAX as u64 {
        return Err(Error::Unsupported("more than 2^32 MFT records"));
    }
    let chunk_bytes = (opts.chunk_bytes.max(rs) / rs) * rs;
    let jobs = plan(&layout, chunk_bytes, opts.use_bitmap, opts.gap_bytes)?;
    let mut stats = ScanStats {
        mft_bytes: layout.data_size,
        mft_extents: layout.extents.len(),
        records_total: n,
        records_planned: jobs.iter().map(|j| j.count as u64).sum(),
        jobs: jobs.len(),
        ..Default::default()
    };
    let mut recs = vec![Rec::default(); n as usize];
    if let Some(p) = &opts.progress {
        p.done.store(0, Ordering::Relaxed);
        p.total.store(stats.records_planned, Ordering::Relaxed);
    }
    let progress = opts.progress.as_deref();
    stats.t_layout = t0.elapsed();

    let t1 = Instant::now();
    let threads = opts.threads.max(1).min(jobs.len().max(1));

    // One disjoint &mut slice of the record table per job, claimed exactly once.
    let mut slices: Vec<Mutex<Option<&mut [Rec]>>> = Vec::with_capacity(jobs.len());
    {
        let mut rest: &mut [Rec] = &mut recs;
        let mut pos = 0u64;
        for j in &jobs {
            let (_, r) = rest.split_at_mut((j.first - pos) as usize);
            let (s, r) = r.split_at_mut(j.count as usize);
            slices.push(Mutex::new(Some(s)));
            rest = r;
            pos = j.first + j.count as u64;
        }
    }

    let est_names = (stats.records_planned as usize / threads) * 9 / 8 + 16;
    let cluster_size = vol.boot.cluster_size;
    let next = AtomicUsize::new(0);
    let failed = AtomicBool::new(false);
    let qd = opts.queue_depth.unwrap_or(if vol.drive.is_some() { 4 } else { threads }).clamp(1, threads);
    let gate = Gate::new(qd);

    let results: Vec<Result<(Sink, WorkerStats)>> = thread::scope(|s| {
        let (jobs, slices, next, failed, gate) = (&jobs, &slices, &next, &failed, &gate);
        let hs: Vec<_> = (0..threads)
            .map(|_| {
                s.spawn(move || -> Result<(Sink, WorkerStats)> {
                    let mut buf = AlignedBuf::new(chunk_bytes);
                    let mut rslots: Vec<ReadSlot> = Vec::with_capacity(2);
                    let mut sink = Sink::new(cluster_size, est_names);
                    let mut st = WorkerStats::default();
                    loop {
                        let j = next.fetch_add(1, Ordering::Relaxed);
                        if j >= jobs.len() || failed.load(Ordering::Relaxed) {
                            break;
                        }
                        let job = &jobs[j];
                        let pass = gate.acquire();
                        let t = Instant::now();
                        if let Err(e) = read_job(vol, job, &mut buf, &mut rslots, &mut st) {
                            failed.store(true, Ordering::Relaxed);
                            return Err(e);
                        }
                        drop(pass);
                        let t2 = Instant::now();
                        st.read += t2 - t;
                        let recs = slices[j].lock().unwrap().take().expect("job claimed twice");
                        for (i, raw) in buf.chunks_exact_mut(rs).take(job.count as usize).enumerate() {
                            record::parse_record(raw, (job.first + i as u64) as u32, &mut recs[i], &mut sink);
                        }
                        st.parse += t2.elapsed();
                        if let Some(p) = progress {
                            let (mut f, mut d) = (0u64, 0u64);
                            for r in recs.iter().take(job.count as usize).filter(|r| r.in_use()) {
                                if r.is_dir() { d += 1 } else { f += 1 }
                            }
                            p.files.fetch_add(f, Ordering::Relaxed);
                            p.dirs.fetch_add(d, Ordering::Relaxed);
                            p.done.fetch_add(job.count as u64, Ordering::Relaxed);
                        }
                    }
                    Ok((sink, st))
                })
            })
            .collect();
        hs.into_iter().map(|h| h.join().expect("scan worker panicked")).collect()
    });
    drop(slices);
    let mut sinks = Vec::with_capacity(results.len());
    for r in results {
        let (sink, st) = r?;
        stats.bytes_read += st.bytes;
        stats.read_calls += st.calls;
        stats.io_time += st.read;
        stats.parse_time += st.parse;
        sinks.push(sink);
    }
    stats.threads = threads;
    stats.queue_depth = qd;
    stats.t_pipeline = t1.elapsed();

    // Merge: extension fragments into base records, then names into one arena.
    let t2 = Instant::now();
    let total_names: usize = sinks.iter().map(|s| s.names.len()).sum();
    let total_arena: usize = sinks.iter().map(|s| s.arena.len()).sum();
    let mut names = Vec::with_capacity(total_names);
    let mut arena = Vec::with_capacity(total_arena);
    for s in &sinks {
        stats.parse.add(&s.stats);
        for f in &s.ext {
            match recs.get_mut(f.base as usize) {
                Some(r) if r.in_use() && r.seq == f.base_seq => {
                    r.alloc = r.alloc.saturating_add(f.alloc);
                    if f.has_size {
                        r.size = f.size;
                    }
                    r.flags |= f.flags;
                    stats.ext_applied += 1;
                }
                _ => stats.ext_dropped += 1,
            }
        }
        let base = arena.len() as u32;
        arena.extend_from_slice(&s.arena);
        names.extend(s.names.iter().map(|e| NameEntry { off: e.off + base, ..*e }));
    }
    drop(sinks);
    stats.t_merge = t2.elapsed();

    Ok(Scan { boot: vol.boot, recs, names, arena, stats })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layout(extents: Vec<Extent>, records: u64, bitmap: Option<Vec<u8>>) -> MftLayout {
        MftLayout {
            extents,
            aux_extents: vec![],
            data_size: records * 1024,
            initialized: records * 1024,
            bitmap,
            record_size: 1024,
            cluster_size: 4096,
        }
    }

    #[test]
    fn map_range_splits_across_fragments() {
        let ext = vec![Extent { vcn: 0, len: 2, lcn: Some(100) }, Extent { vcn: 2, len: 3, lcn: Some(500) }];
        let mut p = Vec::new();
        map_range(&ext, 4096, 4096, 3 * 4096 + 1024, &mut p).unwrap();
        assert_eq!(p.len(), 2);
        assert_eq!((p[0].disk, p[0].buf_off, p[0].len), (101 * 4096, 0, 4096));
        assert_eq!((p[1].disk, p[1].buf_off, p[1].len), (500 * 4096, 4096, 4096 + 1024));
    }

    #[test]
    fn plan_trims_and_skips_with_bitmap() {
        // 64 records = 16 clusters; chunks of 16 records.
        let ext = vec![Extent { vcn: 0, len: 16, lcn: Some(10) }];
        // used: records 0..3 and 40..41 only
        let mut bm = vec![0u8; 8];
        bm[0] = 0b0000_0111;
        bm[5] = 0b0000_0011;
        let jobs = plan(&layout(ext, 64, Some(bm)), 16 * 1024, true, 64 << 10).unwrap();
        let spans: Vec<_> = jobs.iter().map(|j| (j.first, j.count)).collect();
        assert_eq!(spans, vec![(0, 3), (40, 2)]);
        assert_eq!(jobs[1].pieces[0].disk, 10 * 4096 + 40 * 1024);
    }

    #[test]
    fn plan_splits_on_large_gaps_only() {
        let ext = vec![Extent { vcn: 0, len: 16, lcn: Some(10) }];
        let mut bm = vec![0u8; 8];
        bm[0] = 0b0000_0001; // record 0
        bm[1] = 0b0000_0100; // record 10
        let spans = |gap: usize| -> Vec<(u64, u32)> {
            plan(&layout(ext.clone(), 64, Some(bm.clone())), 64 * 1024, true, gap).unwrap().iter().map(|j| (j.first, j.count)).collect()
        };
        assert_eq!(spans(4 * 1024), vec![(0, 1), (10, 1)]);
        assert_eq!(spans(16 * 1024), vec![(0, 11)]);
    }

    #[test]
    fn plan_without_bitmap_reads_everything() {
        let ext = vec![Extent { vcn: 0, len: 16, lcn: Some(10) }];
        let jobs = plan(&layout(ext, 64, None), 16 * 1024, true, 64 << 10).unwrap();
        assert_eq!(jobs.len(), 4);
        assert_eq!(jobs.iter().map(|j| j.count).sum::<u32>(), 64);
    }
}
