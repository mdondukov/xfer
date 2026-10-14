//! The copy command: as is or deduplicated, with a manifest and resume after interruption.

use crate::CopyArgs;
use crate::dupes;
use crate::filter::{Filter, PARTIAL_SUFFIX};
use crate::hashing::{Hash, to_hex};
use crate::paths::{self, SERVICE_DIR};
use crate::report::{self, Text, human, num};
use crate::scan::{self, FileEntry};
use anyhow::{Context, Result, bail};
use filetime::FileTime;
use rayon::prelude::*;
use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

enum Outcome {
    Copied,
    Already,
    Failed(String),
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

    let filter = Filter::new(&args.filter);
    let scan = scan::scan(&roots, &filter, &exclude);
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

    let pb = report::bytes_bar(need, "Copying");
    let outcomes: Vec<Outcome> = plan
        .par_iter()
        .zip(already.par_iter())
        .map(|((i, dest), &done)| {
            if done {
                return Outcome::Already;
            }
            let f = &files[*i];
            let r = copy_one(&f.path, dest, f.mtime);
            pb.inc(f.size);
            match r {
                Ok(()) => Outcome::Copied,
                Err(e) => Outcome::Failed(e.to_string()),
            }
        })
        .collect();
    pb.finish_and_clear();

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

/// Write to a temporary file and rename at the end, so an interrupted copy
/// never leaves behind a file that looks complete.
fn copy_one(src: &Path, dest: &Path, mtime: SystemTime) -> io::Result<()> {
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut tmp = dest.as_os_str().to_owned();
    tmp.push(PARTIAL_SUFFIX);
    let tmp = PathBuf::from(tmp);
    let res = fs::copy(src, &tmp)
        .and_then(|_| filetime::set_file_mtime(&tmp, FileTime::from_system_time(mtime)))
        .and_then(|_| fs::rename(&tmp, dest));
    if res.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    res
}
