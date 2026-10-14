//! The copy command: as is or deduplicated, with a manifest and resume after interruption.

use crate::CopyArgs;
use crate::av;
use crate::dupes;
use crate::filter::{Filter, PARTIAL_SUFFIX};
use crate::hashing::{Hash, to_hex};
use crate::paths::{self, SERVICE_DIR};
use crate::progress::{self, Stage};
use crate::report::{self, Text, human, num};
use crate::scan::{self, FileEntry};
use anyhow::{Context, Result, bail};
use filetime::FileTime;
use rayon::prelude::*;
use std::collections::HashMap;
use indicatif::MultiProgress;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, SystemTime};

enum Outcome {
    Copied,
    Already,
    Failed(String),
    Interrupted,
}

/// Files from this size up go to the large-file lane.
const LARGE_FILE: u64 = 16 * 1024 * 1024;
/// Chunk size for streaming large files; also the progress granularity.
const CHUNK: usize = 4 * 1024 * 1024;

#[derive(Clone, Copy)]
struct Lanes {
    small: usize,
    large: usize,
}

pub fn run(args: &CopyArgs, no_cache: bool) -> Result<()> {
    let roots = paths::prepare_roots(&args.sources)?;
    let target_arg =
        std::path::absolute(paths::fix_arg(&args.to)).context("invalid target folder")?;
    let exclude: Vec<PathBuf> = fs::canonicalize(&target_arg).ok().into_iter().collect();
    if let Some(t) = exclude.first()
        && let Some(r) = roots.iter().find(|r| r.starts_with(t))
    {
        bail!("source {} is inside the target folder", paths::display(r));
    }

    // Second hop (external drive -> new PC): lay out the transfer folder as is,
    // without an extra `E\transfer` level.
    let relative: Vec<bool> = roots
        .iter()
        .map(|r| args.relative || r.join(SERVICE_DIR).is_dir())
        .collect();
    for (r, rel) in roots.iter().zip(&relative) {
        if *rel && !args.relative {
            println!(
                "{} is a transfer folder (contains {SERVICE_DIR}), copying its content without the drive prefix",
                paths::display(r)
            );
        }
    }

    // The check takes a few seconds, so it runs while we scan.
    let av_check = (!args.no_av_check).then(|| thread::spawn(av::check));

    let stages = 1 + if args.dedup { 3 } else { 0 } + if args.dry_run { 0 } else { 1 };
    progress::set_stages(stages);
    let filter = Filter::new(&args.filter);
    let scan = scan::scan(&roots, &filter, &exclude);
    if let Some(text) = av_check.and_then(|h| h.join().ok()).flatten() {
        eprintln!("{text}");
    }
    let files = &scan.files;
    let dedup = if args.dedup {
        Some(dupes::analyze(files, &roots, &args.prefer, no_cache)?)
    } else {
        None
    };

    let mut dup_of: Vec<Option<usize>> = vec![None; files.len()];
    if let Some(d) = &dedup {
        for (g, &k) in d.groups.iter().zip(&d.keepers) {
            for &i in &g.files {
                if i != k {
                    dup_of[i] = Some(k);
                }
            }
        }
    }
    let to_copy: Vec<usize> = (0..files.len()).filter(|&i| dup_of[i].is_none()).collect();
    let copy_bytes: u64 = to_copy.iter().map(|&i| files[i].size).sum();

    let mut text = Text::default();
    report::sources(&mut text, &roots);
    report::scan_section(&mut text, &scan, filter.only_useful);
    report::categories_section(&mut text, files);
    if let Some(d) = &dedup {
        report::dup_section(&mut text, files, &d.groups, &d.keepers, &d.folders, scan.total().bytes);
    }
    text.line(format!(
        "To copy: {} files, {} -> {}",
        num(to_copy.len() as u64),
        human(copy_bytes),
        paths::display(&target_arg)
    ));
    print!("{}", text.0);

    if args.dry_run {
        println!("\nDry run (--dry-run): nothing was copied.");
        return Ok(());
    }

    fs::create_dir_all(&target_arg)
        .with_context(|| format!("cannot create {}", target_arg.display()))?;
    let target = fs::canonicalize(&target_arg)?;
    let dest_of = |i: usize| {
        let f = &files[i];
        if relative[f.root] {
            paths::relative_path(&target, &roots[f.root], &f.path)
        } else {
            paths::mirror_path(&target, &f.path)
        }
    };
    let plan: Vec<(usize, PathBuf)> = to_copy.iter().map(|&i| (i, dest_of(i))).collect();

    // A rerun only copies what is not there yet.
    let already: Vec<bool> = plan
        .par_iter()
        .map(|(i, dest)| same_file(dest, &files[*i]))
        .collect();
    let need: u64 = plan
        .iter()
        .zip(&already)
        .filter(|(_, done)| !**done)
        .map(|((i, _), _)| files[*i].size)
        .sum();
    if need < copy_bytes {
        println!("Already copied in a previous run: {}", human(copy_bytes - need));
    }
    if let Ok(free) = fs4::available_space(&target)
        && need > free
    {
        bail!(
            "target drive has {} free, but {} is needed",
            human(free),
            human(need)
        );
    }

    let run_dir = report::unique_dir(&target.join(SERVICE_DIR).join(report::timestamp()))?;

    let lanes = Lanes {
        small: args.threads.into(),
        large: args.large_threads.into(),
    };
    let outcomes = copy_all(files, &plan, &already, need, lanes);

    let mut hash_of: HashMap<usize, Hash> = HashMap::new();
    if let Some(d) = &dedup {
        for g in &d.groups {
            for &i in &g.files {
                hash_of.insert(i, g.hash);
            }
        }
    }
    let hex = |i: usize| hash_of.get(&i).map(to_hex).unwrap_or_default();

    let mut errors: Vec<String> = scan.errors.clone();
    if let Some(d) = &dedup {
        errors.extend(d.errors.iter().cloned());
    }
    let mut copied = scan::Stat::default();
    let mut skipped = scan::Stat::default();
    let mut failed = 0u64;
    let mut interrupted = scan::Stat::default();

    let mut w = report::excel_csv(&run_dir.join("manifest.csv"))?;
    w.write_record(["status", "source", "destination", "size", "hash", "duplicate_of"])?;
    for ((i, dest), outcome) in plan.iter().zip(&outcomes) {
        let f = &files[*i];
        let status = match outcome {
            Outcome::Copied => {
                copied.add(f.size);
                "copied".to_string()
            }
            Outcome::Already => {
                skipped.add(f.size);
                "already-present".to_string()
            }
            Outcome::Failed(e) => {
                failed += 1;
                errors.push(format!("{}: {e}", paths::display(&f.path)));
                format!("error: {e}")
            }
            Outcome::Interrupted => {
                interrupted.add(f.size);
                "not-copied (interrupted)".to_string()
            }
        };
        w.write_record([
            status,
            paths::display(&f.path),
            paths::display(dest),
            f.size.to_string(),
            hex(*i),
            String::new(),
        ])?;
    }
    for (i, keeper) in dup_of.iter().enumerate() {
        let Some(k) = *keeper else { continue };
        let f = &files[i];
        w.write_record([
            "duplicate".to_string(),
            paths::display(&f.path),
            paths::display(&dest_of(k)),
            f.size.to_string(),
            hex(i),
            paths::display(&files[k].path),
        ])?;
    }
    w.flush()?;

    text.line(String::new());
    text.line(format!("Copied: {}", report::stat(copied)));
    if skipped.count > 0 {
        text.line(format!("Already present: {}", report::stat(skipped)));
    }
    if failed > 0 {
        text.line(format!("Copy errors: {} (see errors.txt)", num(failed)));
    }
    if interrupted.count > 0 {
        text.line(format!(
            "Interrupted, not copied yet: {}. Run the same command again to continue.",
            report::stat(interrupted)
        ));
    }
    fs::write(run_dir.join("summary.txt"), &text.0)?;
    report::write_filtered(&run_dir, &scan)?;
    report::write_errors(&run_dir, &errors)?;

    println!();
    println!("Copied: {}", report::stat(copied));
    if skipped.count > 0 {
        println!("Already present: {}", report::stat(skipped));
    }
    if failed > 0 {
        println!("Copy errors: {}", num(failed));
    }
    if interrupted.count > 0 {
        println!(
            "Interrupted, not copied yet: {}. Run the same command again to continue.",
            report::stat(interrupted)
        );
    }
    println!("Manifest and summary: {}", paths::display(&run_dir));
    Ok(())
}

/// A file counts as already copied when size and mtime match.
/// The 2-second tolerance covers FAT/exFAT timestamp precision.
fn same_file(dest: &Path, f: &FileEntry) -> bool {
    let Ok(m) = fs::metadata(dest) else { return false };
    let Ok(t) = m.modified() else { return false };
    m.len() == f.size && diff(t, f.mtime) <= Duration::from_secs(2)
}

fn diff(a: SystemTime, b: SystemTime) -> Duration {
    a.duration_since(b).or_else(|_| b.duration_since(a)).unwrap_or_default()
}

/// Copies the plan in two lanes running side by side:
/// - small files: many threads, since time goes into per-file overhead (open,
///   create, close, antivirus); copied with `fs::copy` (`CopyFileExW` on Windows);
/// - large files: few threads, biggest first, streamed in chunks so the progress
///   bar moves smoothly; more parallel writes would only fragment the target.
///
/// Ctrl+C stops the lanes after the files in progress; large files stop mid-way.
fn copy_all(
    files: &[FileEntry],
    plan: &[(usize, PathBuf)],
    already: &[bool],
    need: u64,
    lanes: Lanes,
) -> Vec<Outcome> {
    let mut small = Vec::new();
    let mut large = Vec::new();
    for (k, ((i, _), done)) in plan.iter().zip(already).enumerate() {
        if *done {
            continue;
        }
        if files[*i].size >= LARGE_FILE {
            large.push(k);
        } else {
            small.push(k);
        }
    }
    large.sort_by_key(|&k| std::cmp::Reverse(files[plan[k].0].size));
    let total_files = (small.len() + large.len()) as u64;

    let mp = MultiProgress::new();
    let stage = Stage::bytes("Copying", need);
    let main = mp.add(stage.bar().clone());
    let counter = progress::text_line(&mp);
    let done = AtomicU64::new(0);
    let failed = AtomicU64::new(0);
    // Interrupted files count neither as done nor as errors.
    let tick = |r: &io::Result<()>| {
        let failed_now = match r {
            Err(e) if e.kind() == io::ErrorKind::Interrupted => return,
            Err(_) => 1,
            Ok(()) => 0,
        };
        let d = done.fetch_add(1, Ordering::Relaxed) + 1;
        let f = failed.fetch_add(failed_now, Ordering::Relaxed) + failed_now;
        counter.set_message(format!("{}/{} files, {} errors", num(d), num(total_files), num(f)));
    };
    counter.set_message(format!("0/{} files, 0 errors", num(total_files)));

    let results: Vec<OnceLock<Outcome>> = (0..plan.len()).map(|_| OnceLock::new()).collect();
    let copy_small = |k: usize| {
        let (i, dest) = &plan[k];
        let f = &files[*i];
        let r = copy_file(&f.path, dest, f.mtime);
        main.inc(f.size);
        tick(&r);
        let _ = results[k].set(outcome(r));
    };
    let copy_large = |k: usize| {
        let (i, dest) = &plan[k];
        let f = &files[*i];
        let name = f.path.file_name().unwrap_or_default().to_string_lossy();
        let fb = progress::file_bar(&mp, &name, f.size);
        let r = stream_file(&f.path, dest, f.mtime, |n| {
            main.inc(n);
            fb.inc(n);
        });
        fb.finish_and_clear();
        mp.remove(&fb);
        tick(&r);
        let _ = results[k].set(outcome(r));
    };

    progress::set_graceful(true);
    let (next_small, next_large) = (AtomicUsize::new(0), AtomicUsize::new(0));
    thread::scope(|s| {
        run_lane(s, &small, lanes.small, &next_small, &copy_small);
        run_lane(s, &large, lanes.large, &next_large, &copy_large);
    });
    progress::set_graceful(false);

    counter.finish_and_clear();
    mp.remove(&counter);
    stage.finish(format!(
        "{} files, {} written",
        num(done.load(Ordering::Relaxed)),
        human(main.position())
    ));

    results
        .into_iter()
        .zip(already)
        .map(|(cell, &was_there)| match cell.into_inner() {
            Some(o) => o,
            None if was_there => Outcome::Already,
            None => Outcome::Interrupted,
        })
        .collect()
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

fn outcome(r: io::Result<()>) -> Outcome {
    match r {
        Ok(()) => Outcome::Copied,
        Err(e) if e.kind() == io::ErrorKind::Interrupted => Outcome::Interrupted,
        Err(e) => Outcome::Failed(e.to_string()),
    }
}

fn partial_path(dest: &Path) -> PathBuf {
    let mut tmp = dest.as_os_str().to_owned();
    tmp.push(PARTIAL_SUFFIX);
    PathBuf::from(tmp)
}

/// Write to a temporary file and rename at the end, so an interrupted copy
/// never leaves behind a file that looks complete.
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
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = partial_path(dest);
    let res = fs::copy(src, &tmp).map(|_| ());
    finish_copy(&tmp, dest, mtime, res)
}

/// Streams a large file in chunks, reporting progress after each one.
/// The target is preallocated to its final size to limit fragmentation.
fn stream_file(
    src: &Path,
    dest: &Path,
    mtime: SystemTime,
    on_progress: impl Fn(u64),
) -> io::Result<()> {
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = partial_path(dest);
    let res = (|| {
        let mut input = File::open(src)?;
        let size = input.metadata()?.len();
        let mut output = File::create(&tmp)?;
        output.set_len(size)?;
        let mut buf = vec![0u8; CHUNK];
        let mut written = 0u64;
        loop {
            if progress::interrupted() {
                return Err(io::Error::new(io::ErrorKind::Interrupted, "interrupted"));
            }
            let n = input.read(&mut buf)?;
            if n == 0 {
                break;
            }
            output.write_all(&buf[..n])?;
            written += n as u64;
            on_progress(n as u64);
        }
        // The source may have shrunk since we looked at it.
        if written != size {
            output.set_len(written)?;
        }
        Ok(())
    })();
    finish_copy(&tmp, dest, mtime, res)
}
