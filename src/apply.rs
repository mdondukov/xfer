//! The apply command: moves marked duplicates to quarantine `<root>\_dupes_trash\<time>`.
//!
//! Safeguards against marking mistakes:
//! - every group keeps at least one copy, even if all of them were marked;
//! - before moving, the surviving copy is checked to exist and match the hash;
//! - each file to be moved is re-hashed too: if it changed after the report, it is left alone.

use crate::ApplyArgs;
use crate::hashing::{self, Hash};
use crate::paths::{self, TRASH_DIR};
use crate::progress::{self, Stage};
use crate::report::{self, human, num};
use anyhow::{Context, Result, anyhow, bail};
use rayon::prelude::*;
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

struct Row {
    delete: bool,
    group: String,
    keep_hint: bool,
    path: String,
    hash: Option<Hash>,
    size: u64,
    root: String,
}

pub fn run(args: &ApplyArgs) -> Result<()> {
    let dir = paths::fix_arg(&args.report);
    let mut rows = read_files(&dir.join("files.csv"))?;
    let folders = read_marked_folders(&dir.join("folders.csv"))?;
    for r in rows.iter_mut() {
        if !r.delete && folders.iter().any(|f| Path::new(&r.path).starts_with(f)) {
            r.delete = true;
        }
    }

    let mut groups: BTreeMap<&str, Vec<usize>> = BTreeMap::new();
    for (i, r) in rows.iter().enumerate() {
        groups.entry(r.group.as_str()).or_default().push(i);
    }
    let mut warnings = Vec::new();
    let mut unmark = Vec::new();
    let mut work: Vec<(Hash, Vec<usize>)> = Vec::new();
    for (g, idx) in &groups {
        if !idx.iter().any(|&i| rows[i].delete) {
            continue;
        }
        let Some(hash) = rows[idx[0]].hash else {
            warnings.push(format!("group {g}: hash in CSV is corrupted, skipping"));
            continue;
        };
        if idx.iter().all(|&i| rows[i].delete) {
            let keep = idx.iter().copied().find(|&i| rows[i].keep_hint).unwrap_or(idx[0]);
            unmark.push(keep);
            warnings.push(format!(
                "group {g}: all copies are marked, keeping {}",
                rows[keep].path
            ));
        }
        work.push((hash, idx.clone()));
    }
    for i in unmark {
        rows[i].delete = false;
    }

    let marked: usize = work.iter().map(|(_, idx)| idx.iter().filter(|&&i| rows[i].delete).count()).sum();
    if marked == 0 {
        for w in &warnings {
            println!("{w}");
        }
        println!("Nothing is marked for deletion.");
        return Ok(());
    }

    // Verify: the group has a live copy and the files to move have not changed.
    progress::set_stages(1);
    let stage = Stage::count("Verifying", work.len() as u64);
    let checked: Vec<Result<Vec<usize>, String>> = work
        .par_iter()
        .map(|(hash, idx)| {
            let res = check_group(&rows, *hash, idx);
            stage.bar().inc(1);
            res
        })
        .collect();
    stage.finish(format!("{} groups checked", num(work.len() as u64)));

    let mut to_move = Vec::new();
    for res in checked {
        match res {
            Ok(v) => to_move.extend(v),
            Err(w) => warnings.push(w),
        }
    }
    for w in &warnings {
        println!("{w}");
    }
    let bytes: u64 = to_move.iter().map(|&i| rows[i].size).sum();
    println!(
        "To move to quarantine: {} files, {}",
        num(to_move.len() as u64),
        human(bytes)
    );
    if to_move.is_empty() {
        return Ok(());
    }
    if args.dry_run {
        for &i in &to_move {
            println!("  {}", rows[i].path);
        }
        println!("Dry run (--dry-run): nothing was moved.");
        return Ok(());
    }
    if !args.yes && !confirm("Move?") {
        println!("Cancelled.");
        return Ok(());
    }

    let stamp = report::timestamp();
    let mut moved: HashMap<PathBuf, Vec<(String, PathBuf)>> = HashMap::new();
    let mut errors = Vec::new();
    // Stop between files on Ctrl+C, so moved.csv still lists everything moved.
    progress::set_graceful(true);
    for &i in &to_move {
        if progress::interrupted() {
            errors.push("interrupted: the remaining files were not moved".to_string());
            break;
        }
        let r = &rows[i];
        match quarantine(r, &stamp) {
            Ok((root, dest)) => moved.entry(root).or_default().push((r.path.clone(), dest)),
            Err(e) => errors.push(format!("{}: {e:#}", r.path)),
        }
    }

    let mut done = 0u64;
    for (root, list) in &moved {
        let trash = root.join(TRASH_DIR).join(&stamp);
        let mut w = report::excel_csv(&trash.join("moved.csv"))?;
        w.write_record(["original", "quarantine"])?;
        for (orig, dest) in list {
            w.write_record([orig.clone(), paths::display(dest)])?;
            done += 1;
        }
        w.flush()?;
        println!("Quarantine: {}", paths::display(&trash));
    }
    for e in &errors {
        eprintln!("Not moved: {e}");
    }
    println!("Moved: {} files. Errors: {}", num(done), num(errors.len() as u64));
    println!("To restore files, use moved.csv in the quarantine folder; to delete them for good, delete the quarantine folder.");
    Ok(())
}

/// Returns the group's rows to move that passed verification.
fn check_group(rows: &[Row], hash: Hash, idx: &[usize]) -> Result<Vec<usize>, String> {
    let group = &rows[idx[0]].group;
    let alive = idx
        .iter()
        .filter(|&&i| !rows[i].delete)
        .any(|&i| hashing::hash_file(&paths::to_long(&rows[i].path)).ok() == Some(hash));
    if !alive {
        return Err(format!(
            "group {group}: the remaining copy is missing or changed, leaving the group alone"
        ));
    }
    let mut ok = Vec::new();
    for &i in idx.iter().filter(|&&i| rows[i].delete) {
        match hashing::hash_file(&paths::to_long(&rows[i].path)) {
            Ok(h) if h == hash => ok.push(i),
            Ok(_) => eprintln!("{}: file changed after the report, leaving it alone", rows[i].path),
            Err(e) => eprintln!("{}: {e}, skipping", rows[i].path),
        }
    }
    Ok(ok)
}

/// `<root>\_dupes_trash\<time>\<path from root>`. Quarantine is on the same volume,
/// so the move is instant and needs no extra space.
fn quarantine(r: &Row, stamp: &str) -> Result<(PathBuf, PathBuf)> {
    let root = paths::to_long(&r.root);
    let src = paths::to_long(&r.path);
    let rel = src
        .strip_prefix(&root)
        .map_err(|_| anyhow!("file is not inside {}", r.root))?;
    let dest = root.join(TRASH_DIR).join(stamp).join(rel);
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::rename(&src, &dest).context("move failed")?;
    remove_empty_parents(&src, &root);
    Ok((root, dest))
}

fn remove_empty_parents(file: &Path, root: &Path) {
    let mut dir = file.parent();
    while let Some(d) = dir {
        if d == root || !d.starts_with(root) || fs::remove_dir(d).is_err() {
            break;
        }
        dir = d.parent();
    }
}

fn confirm(question: &str) -> bool {
    print!("{question} [y/N]: ");
    let _ = io::stdout().flush();
    let mut s = String::new();
    let _ = io::stdin().read_line(&mut s);
    matches!(s.trim().to_lowercase().as_str(), "y" | "yes")
}

fn is_marked(s: &str) -> bool {
    let s = s.trim().to_lowercase();
    !s.is_empty() && !matches!(s.as_str(), "0" | "-" | "no" | "false")
}

struct Csv {
    headers: Vec<String>,
    records: Vec<csv::StringRecord>,
}

impl Csv {
    fn col(&self, name: &str) -> Result<usize> {
        self.headers
            .iter()
            .position(|h| h == name)
            .ok_or_else(|| anyhow!("CSV has no column {name}"))
    }
}

/// Excel may have re-saved the file: accept a BOM and either delimiter.
fn read_csv(path: &Path) -> Result<Csv> {
    let bytes = fs::read(path).with_context(|| format!("cannot read {}", path.display()))?;
    let text = String::from_utf8(bytes).map_err(|_| {
        anyhow!(
            "{} is not UTF-8. In Excel, use Save As -> CSV UTF-8",
            path.display()
        )
    })?;
    let text = text.trim_start_matches('\u{feff}');
    let first = text.lines().next().unwrap_or_default();
    let delimiter = if first.contains(';') { b';' } else { b',' };
    let mut rdr = csv::ReaderBuilder::new()
        .delimiter(delimiter)
        .flexible(true)
        .from_reader(text.as_bytes());
    let headers = rdr.headers()?.iter().map(|h| h.trim().to_lowercase()).collect();
    let records = rdr.records().collect::<Result<Vec<_>, _>>()?;
    Ok(Csv { headers, records })
}

fn read_files(path: &Path) -> Result<Vec<Row>> {
    let csv = read_csv(path)?;
    let (c_del, c_group, c_sug, c_size, c_path, c_hash, c_root) = (
        csv.col("delete")?,
        csv.col("group")?,
        csv.col("suggest")?,
        csv.col("size")?,
        csv.col("path")?,
        csv.col("hash")?,
        csv.col("root")?,
    );
    let get = |r: &csv::StringRecord, c: usize| r.get(c).unwrap_or_default().trim().to_string();
    let rows: Vec<Row> = csv
        .records
        .iter()
        .filter(|r| !get(r, c_path).is_empty())
        .map(|r| Row {
            delete: is_marked(&get(r, c_del)),
            group: get(r, c_group),
            keep_hint: get(r, c_sug) == "keep",
            path: get(r, c_path),
            hash: hashing::from_hex(&get(r, c_hash)),
            size: get(r, c_size).replace([' ', '\u{a0}'], "").parse().unwrap_or(0),
            root: get(r, c_root),
        })
        .collect();
    if rows.is_empty() {
        bail!("{} has no rows", path.display());
    }
    Ok(rows)
}

fn read_marked_folders(path: &Path) -> Result<Vec<PathBuf>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let csv = read_csv(path)?;
    let (c_del, c_folder) = (csv.col("delete")?, csv.col("folder")?);
    Ok(csv
        .records
        .iter()
        .filter(|r| is_marked(r.get(c_del).unwrap_or_default()))
        .filter_map(|r| r.get(c_folder).map(|f| PathBuf::from(f.trim())))
        .collect())
}
