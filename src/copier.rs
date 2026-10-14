//! Copy engines. Both write each file to `<name>.xfer-partial` and rename it at
//! the end, so an interrupted copy never leaves a file that looks complete.
//!
//! - `parallel`: for SSD -> SSD. Two lanes side by side: many threads for small
//!   files (bound by per-file overhead), a couple for large ones (bound by bandwidth).
//! - `pipeline`: when a hard drive is involved. One reader per source disk walks
//!   files in scan order, one writer writes them in arrival order, and a bounded
//!   in-memory queue between them lets reading the next file overlap with writing
//!   the current one. Each disk sees a single sequential stream, so total time
//!   approaches that of the slower disk rather than the sum of both.

use crate::filter::PARTIAL_SUFFIX;
use crate::progress::{self, Stage};
use crate::report::{human, num};
use crate::scan::FileEntry;
use filetime::FileTime;
use indicatif::{MultiProgress, ProgressBar};
use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::thread;
use std::time::SystemTime;

pub enum Outcome {
    Copied,
    Already,
    Failed(String),
    Interrupted,
}

/// One planned copy: index into the file list and the destination path.
pub type Plan = [(usize, PathBuf)];

/// Parallel engine: files from this size up go to the large-file lane.
const LARGE_FILE: u64 = 16 * 1024 * 1024;
/// Parallel engine: chunk size for streaming large files.
const CHUNK: usize = 4 * 1024 * 1024;
/// Pipeline: files up to this size travel as one message; larger ones in chunks.
const PIPE_CHUNK: usize = 1024 * 1024;
/// Pipeline: queue length in messages, so at most ~256 MB is in flight.
const PIPE_DEPTH: usize = 256;

#[derive(Clone, Copy)]
pub struct Lanes {
    pub small: usize,
    pub large: usize,
}

/// Progress for both engines: a bytes bar, a file counter line, and one bar per
/// large file in progress.
struct Ui {
    mp: MultiProgress,
    stage: Stage,
    main: ProgressBar,
    counter: ProgressBar,
    done: AtomicU64,
    failed: AtomicU64,
    total: u64,
}

impl Ui {
    fn new(need: u64, total: u64) -> Ui {
        let mp = MultiProgress::new();
        let stage = Stage::bytes("Copying", need);
        let main = mp.add(stage.bar().clone());
        let counter = progress::text_line(&mp);
        counter.set_message(format!("0/{} files, 0 errors", num(total)));
        Ui { mp, stage, main, counter, done: AtomicU64::new(0), failed: AtomicU64::new(0), total }
    }

    /// Interrupted files count neither as done nor as errors.
    fn tick(&self, r: &io::Result<()>) {
        let failed_now = match r {
            Err(e) if e.kind() == io::ErrorKind::Interrupted => return,
            Err(_) => 1,
            Ok(()) => 0,
        };
        let d = self.done.fetch_add(1, Ordering::Relaxed) + 1;
        let f = self.failed.fetch_add(failed_now, Ordering::Relaxed) + failed_now;
        self.counter
            .set_message(format!("{}/{} files, {} errors", num(d), num(self.total), num(f)));
    }

    fn file_bar(&self, f: &FileEntry) -> ProgressBar {
        let name = f.path.file_name().unwrap_or_default().to_string_lossy();
        progress::file_bar(&self.mp, &name, f.size)
    }

    fn end_file_bar(&self, pb: &ProgressBar) {
        pb.finish_and_clear();
        self.mp.remove(pb);
    }

    fn finish(self) {
        self.counter.finish_and_clear();
        self.mp.remove(&self.counter);
        self.stage.finish(format!(
            "{} files, {} written",
            num(self.done.load(Ordering::Relaxed)),
            human(self.main.position())
        ));
    }
}

/// Copies `pending` (indices into `plan`) in two parallel lanes; see the module docs.
/// Returns an outcome for every file that was attempted.
pub fn parallel(
    files: &[FileEntry],
    plan: &Plan,
    pending: &[usize],
    need: u64,
    lanes: Lanes,
) -> Vec<Option<Outcome>> {
    let (mut large, small): (Vec<usize>, Vec<usize>) = pending
        .iter()
        .partition(|&&k| files[plan[k].0].size >= LARGE_FILE);
    large.sort_by_key(|&k| std::cmp::Reverse(files[plan[k].0].size));

    let ui = Ui::new(need, pending.len() as u64);
    let results: Vec<OnceLock<Outcome>> = (0..plan.len()).map(|_| OnceLock::new()).collect();
    let copy_small = |k: usize| {
        let (i, dest) = &plan[k];
        let f = &files[*i];
        let r = copy_file(&f.path, dest, f.mtime);
        ui.main.inc(f.size);
        ui.tick(&r);
        let _ = results[k].set(outcome(r));
    };
    let copy_large = |k: usize| {
        let (i, dest) = &plan[k];
        let f = &files[*i];
        let fb = ui.file_bar(f);
        let r = stream_file(&f.path, dest, f.mtime, |n| {
            ui.main.inc(n);
            fb.inc(n);
        });
        ui.end_file_bar(&fb);
        ui.tick(&r);
        let _ = results[k].set(outcome(r));
    };

    progress::set_graceful(true);
    let (next_small, next_large) = (AtomicUsize::new(0), AtomicUsize::new(0));
    thread::scope(|s| {
        run_lane(s, &small, lanes.small, &next_small, &copy_small);
        run_lane(s, &large, lanes.large, &next_large, &copy_large);
    });
    progress::set_graceful(false);
    ui.finish();
    results.into_iter().map(OnceLock::into_inner).collect()
}

/// Worker threads pulling jobs from a shared index until the lane is empty.
fn run_lane<'scope, 'env, F>(
    s: &'scope thread::Scope<'scope, 'env>,
    jobs: &'env [usize],
    threads: usize,
    next: &'env AtomicUsize,
    work: &'env F,
) where
    F: Fn(usize) + Sync,
{
    for _ in 0..threads.min(jobs.len()) {
        s.spawn(move || {
            while !progress::interrupted() {
                let n = next.fetch_add(1, Ordering::Relaxed);
                let Some(&k) = jobs.get(n) else { break };
                work(k);
            }
        });
    }
}

enum Msg {
    /// A whole small file.
    Whole { k: usize, data: Vec<u8> },
    /// A large file follows in chunks.
    Start { k: usize, size: u64 },
    Chunk { k: usize, data: Vec<u8> },
    End { k: usize },
    /// The reader failed (or was interrupted) on this file.
    Abort { k: usize, err: io::Error },
}

/// Copies with one reader per entry of `readers` (each a list of plan indices on
/// one source disk, in scan order) and a single writer; see the module docs.
pub fn pipeline(
    files: &[FileEntry],
    plan: &Plan,
    readers: &[Vec<usize>],
    need: u64,
) -> Vec<Option<Outcome>> {
    let total = readers.iter().map(Vec::len).sum::<usize>() as u64;
    let ui = Ui::new(need, total);
    // Set by the writer when it gives up on a file, so the reader stops sending it.
    let cancelled: Vec<AtomicBool> = (0..plan.len()).map(|_| AtomicBool::new(false)).collect();
    let mut results: Vec<Option<Outcome>> = (0..plan.len()).map(|_| None).collect();
    let (tx, rx) = mpsc::sync_channel::<Msg>(PIPE_DEPTH);

    progress::set_graceful(true);
    thread::scope(|s| {
        for jobs in readers {
            let tx = tx.clone();
            let cancelled = &cancelled;
            s.spawn(move || read_all(files, plan, jobs, &tx, cancelled));
        }
        drop(tx);
        write_all(files, plan, rx, &cancelled, &mut results, &ui);
    });
    progress::set_graceful(false);
    ui.finish();
    results
}

fn read_all(
    files: &[FileEntry],
    plan: &Plan,
    jobs: &[usize],
    tx: &SyncSender<Msg>,
    cancelled: &[AtomicBool],
) {
    for &k in jobs {
        if progress::interrupted() {
            return;
        }
        if let Err(err) = read_one(&files[plan[k].0].path, k, tx, &cancelled[k])
            && (err.kind() == io::ErrorKind::BrokenPipe || tx.send(Msg::Abort { k, err }).is_err())
        {
            return; // the writer is gone
        }
    }
}

fn read_one(path: &Path, k: usize, tx: &SyncSender<Msg>, cancelled: &AtomicBool) -> io::Result<()> {
    let send = |m: Msg| {
        tx.send(m)
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "writer stopped"))
    };
    let mut f = File::open(path)?;
    let size = f.metadata()?.len();
    if size <= PIPE_CHUNK as u64 {
        let mut data = Vec::with_capacity(size as usize);
        f.read_to_end(&mut data)?;
        return send(Msg::Whole { k, data });
    }
    send(Msg::Start { k, size })?;
    loop {
        if progress::interrupted() {
            return Err(stopped());
        }
        if cancelled.load(Ordering::Relaxed) {
            return Ok(());
        }
        let mut buf = vec![0u8; PIPE_CHUNK];
        let n = read_some(&mut f, &mut buf)?;
        if n == 0 {
            break;
        }
        buf.truncate(n);
        send(Msg::Chunk { k, data: buf })?;
    }
    send(Msg::End { k })
}

/// A large file being written by the pipeline writer.
struct Open {
    file: File,
    tmp: PathBuf,
    size: u64,
    written: u64,
    bar: ProgressBar,
}

fn write_all(
    files: &[FileEntry],
    plan: &Plan,
    rx: Receiver<Msg>,
    cancelled: &[AtomicBool],
    results: &mut [Option<Outcome>],
    ui: &Ui,
) {
    let record = |results: &mut [Option<Outcome>], k: usize, r: io::Result<()>| {
        ui.tick(&r);
        results[k] = Some(outcome(r));
    };
    let discard = |o: Open| {
        drop(o.file);
        let _ = fs::remove_file(&o.tmp);
        ui.end_file_bar(&o.bar);
    };
    let mut open: HashMap<usize, Open> = HashMap::new();

    for msg in rx {
        match msg {
            Msg::Whole { k, data } => {
                let (i, dest) = &plan[k];
                let r = write_whole(dest, &data, files[*i].mtime);
                ui.main.inc(data.len() as u64);
                record(results, k, r);
            }
            Msg::Start { k, size } => {
                let (i, dest) = &plan[k];
                match start_file(dest, size) {
                    Ok((file, tmp)) => {
                        let bar = ui.file_bar(&files[*i]);
                        open.insert(k, Open { file, tmp, size, written: 0, bar });
                    }
                    Err(e) => {
                        cancelled[k].store(true, Ordering::Relaxed);
                        record(results, k, Err(e));
                    }
                }
            }
            Msg::Chunk { k, data } => {
                // Chunks of a file the writer already gave up on are dropped.
                let Some(o) = open.get_mut(&k) else { continue };
                let n = data.len() as u64;
                match o.file.write_all(&data) {
                    Ok(()) => {
                        o.written += n;
                        o.bar.inc(n);
                        ui.main.inc(n);
                    }
                    Err(e) => {
                        if let Some(o) = open.remove(&k) {
                            discard(o);
                        }
                        cancelled[k].store(true, Ordering::Relaxed);
                        record(results, k, Err(e));
                    }
                }
            }
            Msg::End { k } => {
                let Some(o) = open.remove(&k) else { continue };
                // The source may have shrunk since the reader looked at it.
                let r = if o.written != o.size { o.file.set_len(o.written) } else { Ok(()) };
                let Open { file, tmp, bar, .. } = o;
                drop(file);
                ui.end_file_bar(&bar);
                let (i, dest) = &plan[k];
                record(results, k, finish_copy(&tmp, dest, files[*i].mtime, r));
            }
            Msg::Abort { k, err } => {
                if let Some(o) = open.remove(&k) {
                    discard(o);
                }
                if results[k].is_none() {
                    record(results, k, Err(err));
                }
            }
        }
    }
    for (_, o) in open.drain() {
        discard(o);
    }
}

pub fn outcome(r: io::Result<()>) -> Outcome {
    match r {
        Ok(()) => Outcome::Copied,
        Err(e) if e.kind() == io::ErrorKind::Interrupted => Outcome::Interrupted,
        Err(e) => Outcome::Failed(e.to_string()),
    }
}

/// The error used for Ctrl+C; `outcome` maps it to `Outcome::Interrupted`.
fn stopped() -> io::Error {
    io::Error::new(io::ErrorKind::Interrupted, "stopped by Ctrl+C")
}

/// `read` that retries OS-level EINTR, so `Interrupted` only ever means Ctrl+C.
fn read_some(f: &mut File, buf: &mut [u8]) -> io::Result<usize> {
    loop {
        match f.read(buf) {
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            r => return r,
        }
    }
}

fn partial_path(dest: &Path) -> PathBuf {
    let mut tmp = dest.as_os_str().to_owned();
    tmp.push(PARTIAL_SUFFIX);
    PathBuf::from(tmp)
}

fn create_parent(dest: &Path) -> io::Result<()> {
    match dest.parent() {
        Some(parent) => fs::create_dir_all(parent),
        None => Ok(()),
    }
}

/// Sets the modification time and renames the temporary file into place;
/// removes it if anything failed.
fn finish_copy(tmp: &Path, dest: &Path, mtime: SystemTime, res: io::Result<()>) -> io::Result<()> {
    let res = res
        .and_then(|_| filetime::set_file_mtime(tmp, FileTime::from_system_time(mtime)))
        .and_then(|_| fs::rename(tmp, dest));
    if res.is_err() {
        let _ = fs::remove_file(tmp);
    }
    res
}

fn copy_file(src: &Path, dest: &Path, mtime: SystemTime) -> io::Result<()> {
    create_parent(dest)?;
    let tmp = partial_path(dest);
    let res = fs::copy(src, &tmp).map(|_| ());
    finish_copy(&tmp, dest, mtime, res)
}

fn write_whole(dest: &Path, data: &[u8], mtime: SystemTime) -> io::Result<()> {
    create_parent(dest)?;
    let tmp = partial_path(dest);
    let res = fs::write(&tmp, data);
    finish_copy(&tmp, dest, mtime, res)
}

/// Creates the temporary file preallocated to its final size, which limits
/// fragmentation of the target.
fn start_file(dest: &Path, size: u64) -> io::Result<(File, PathBuf)> {
    create_parent(dest)?;
    let tmp = partial_path(dest);
    let file = File::create(&tmp)?;
    if let Err(e) = file.set_len(size) {
        drop(file);
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }
    Ok((file, tmp))
}

/// Streams a large file in chunks, reporting progress after each one.
fn stream_file(
    src: &Path,
    dest: &Path,
    mtime: SystemTime,
    on_progress: impl Fn(u64),
) -> io::Result<()> {
    let mut input = File::open(src)?;
    let size = input.metadata()?.len();
    let (mut output, tmp) = start_file(dest, size)?;
    let res = (|| {
        let mut buf = vec![0u8; CHUNK];
        let mut written = 0u64;
        loop {
            if progress::interrupted() {
                return Err(stopped());
            }
            let n = read_some(&mut input, &mut buf)?;
            if n == 0 {
                break;
            }
            output.write_all(&buf[..n])?;
            written += n as u64;
            on_progress(n as u64);
        }
        if written != size {
            output.set_len(written)?;
        }
        Ok(())
    })();
    drop(output);
    finish_copy(&tmp, dest, mtime, res)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::UNIX_EPOCH;

    /// Both engines must produce byte-identical copies, including files larger
    /// than one chunk and empty files.
    fn check_engine(pipelined: bool) {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        let dst = dir.path().join("dst");
        fs::create_dir_all(&src).unwrap();
        let sizes = [0usize, 10, PIPE_CHUNK, PIPE_CHUNK + 1, 3 * PIPE_CHUNK + 7, CHUNK + 5];
        let mut files = Vec::new();
        let mut plan = Vec::new();
        for (n, &size) in sizes.iter().enumerate() {
            let data: Vec<u8> = (0..size).map(|b| (b * 31 + n) as u8).collect();
            let p = src.join(format!("f{n}.bin"));
            fs::write(&p, &data).unwrap();
            files.push(FileEntry {
                path: p,
                root: 0,
                size: size as u64,
                mtime: UNIX_EPOCH + std::time::Duration::from_secs(1_000_000_000 + n as u64),
                category: None,
            });
            plan.push((n, dst.join("sub").join(format!("f{n}.bin"))));
        }
        let pending: Vec<usize> = (0..plan.len()).collect();
        let need = sizes.iter().sum::<usize>() as u64;
        let results = if pipelined {
            pipeline(&files, &plan, &[pending], need)
        } else {
            parallel(&files, &plan, &pending, need, Lanes { small: 4, large: 2 })
        };
        for ((i, dest), r) in plan.iter().zip(results) {
            assert!(matches!(r, Some(Outcome::Copied)), "{} not copied", dest.display());
            assert_eq!(fs::read(dest).unwrap(), fs::read(&files[*i].path).unwrap());
            let mtime = fs::metadata(dest).unwrap().modified().unwrap();
            assert_eq!(mtime, files[*i].mtime);
            assert!(!partial_path(dest).exists());
        }
    }

    #[test]
    fn pipeline_copies_exactly() {
        check_engine(true);
    }

    #[test]
    fn parallel_copies_exactly() {
        check_engine(false);
    }
}
