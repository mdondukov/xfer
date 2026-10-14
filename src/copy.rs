//! The copy command: as is or deduplicated, with a manifest and resume after interruption.

use crate::CopyArgs;
use crate::av;
use crate::dupes;
use crate::filter::Filter;
use crate::hashing::{Hash, to_hex};
use crate::paths::{self, SERVICE_DIR};
use crate::copier::{self, Lanes, Outcome};
use crate::disk::{self, Disk};
use crate::progress;
use crate::report::{self, Text, human, num};
use crate::scan::{self, FileEntry};
use anyhow::{Context, Result, bail};
use crate::CopyMode;
use rayon::prelude::*;
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, SystemTime};

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
    let source_disks: Vec<Disk> = roots.iter().map(|r| disk::probe(r)).collect();
    let target_disk = disk::probe(&target_arg);
    let mode = match args.copy_mode {
        CopyMode::Auto => {
            if source_disks.iter().chain([&target_disk]).any(Disk::sequential) {
                CopyMode::Pipeline
            } else {
                CopyMode::Parallel
            }
        }
        m => m,
    };

    let filter = Filter::new(&args.filter);
    let scan = scan::scan(&roots, &source_disks, &filter, &exclude);
    if let Some(text) = av_check.and_then(|h| h.join().ok()).flatten() {
        eprintln!("{text}");
    }
    let files = &scan.files;
    let dedup = if args.dedup {
        Some(dupes::analyze(files, &roots, &source_disks, &args.prefer, no_cache)?)
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
    report::disks_section(&mut text, &roots, &source_disks, Some((&target_arg, &target_disk)));
    text.line(format!("Copy mode: {}", mode_text(mode, args, &source_disks)));
    report::scan_section(&mut text, &scan, filter.only_useful, !args.dry_run);
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

    // A rerun only copies what is not there yet. Checked sequentially on a hard drive.
    let check = |(i, dest): &(usize, PathBuf)| same_file(dest, &files[*i]);
    let already: Vec<bool> = if target_disk.sequential() {
        plan.iter().map(check).collect()
    } else {
        plan.par_iter().map(check).collect()
    };
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

    let pending: Vec<usize> = (0..plan.len()).filter(|&k| !already[k]).collect();
    let results = match mode {
        CopyMode::Pipeline => {
            // One reader per source disk; plan order is scan order.
            let mut readers: BTreeMap<String, Vec<usize>> = BTreeMap::new();
            for &k in &pending {
                let d = &source_disks[files[plan[k].0].root];
                readers.entry(d.key()).or_default().push(k);
            }
            let readers: Vec<Vec<usize>> = readers.into_values().collect();
            copier::pipeline(files, &plan, &readers, need)
        }
        _ => {
            let lanes = Lanes {
                small: args.threads.into(),
                large: args.large_threads.into(),
            };
            copier::parallel(files, &plan, &pending, need, lanes)
        }
    };
    let outcomes: Vec<Outcome> = results
        .into_iter()
        .zip(&already)
        .map(|(r, &was_there)| match r {
            Some(o) => o,
            None if was_there => Outcome::Already,
            None => Outcome::Interrupted,
        })
        .collect();

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

fn mode_text(mode: CopyMode, args: &CopyArgs, sources: &[Disk]) -> String {
    let why = if args.copy_mode == CopyMode::Auto { "chosen automatically" } else { "set by --copy-mode" };
    match mode {
        CopyMode::Pipeline => {
            let readers = sources.iter().map(Disk::key).collect::<std::collections::BTreeSet<_>>().len();
            let reader_text = if readers == 1 { "1 reader".to_string() } else { format!("{readers} readers, one per source disk") };
            format!("pipeline ({reader_text}, 1 writer, sequential access to each disk; {why})")
        }
        _ => format!(
            "parallel ({} threads for small files, {} for large; {why})",
            args.threads, args.large_threads
        ),
    }
}
