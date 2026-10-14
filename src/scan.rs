//! Source traversal: collect files that pass the filter and stats on what was filtered out.

use crate::disk::Disk;
use crate::filter::{self, Category, Decision, Filter};
use crate::paths;
use crate::progress::{self, Stage};
use crate::report;
use std::collections::BTreeMap;
use std::fs::Metadata;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};
use walkdir::WalkDir;

pub struct FileEntry {
    pub path: PathBuf,
    /// Index of the source root the file came from.
    pub root: usize,
    pub size: u64,
    pub mtime: SystemTime,
    pub category: Option<Category>,
}

#[derive(Default, Clone, Copy)]
pub struct Stat {
    pub count: u64,
    pub bytes: u64,
}

impl Stat {
    pub fn add(&mut self, bytes: u64) {
        self.count += 1;
        self.bytes += bytes;
    }
}

#[derive(Default)]
pub struct ScanResult {
    pub files: Vec<FileEntry>,
    pub filtered: Vec<(PathBuf, u64)>,
    pub filtered_by_ext: BTreeMap<String, Stat>,
    pub skipped_dirs: Vec<PathBuf>,
    /// OneDrive cloud-only files: reading them would trigger a download.
    pub cloud_only: Stat,
    pub errors: Vec<String>,
}

impl ScanResult {
    pub fn total(&self) -> Stat {
        let mut s = Stat::default();
        for f in &self.files {
            s.add(f.size);
        }
        s
    }

    pub fn filtered_total(&self) -> Stat {
        let mut s = Stat::default();
        for st in self.filtered_by_ext.values() {
            s.count += st.count;
            s.bytes += st.bytes;
        }
        s
    }
}

/// `exclude` - folders not to descend into (a target folder inside a source).
/// Roots on different physical disks are scanned in parallel; roots on the same
/// disk one after another, so a hard drive is never walked by two threads at once.
pub fn scan(roots: &[PathBuf], disks: &[Disk], filter: &Filter, exclude: &[PathBuf]) -> ScanResult {
    let stage = Stage::spinner("Scanning");
    let seen = AtomicU64::new(0);
    let seen_bytes = AtomicU64::new(0);
    let counters = &Counters { stage: &stage, seen: &seen, seen_bytes: &seen_bytes };

    let mut by_disk: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    for (ri, d) in disks.iter().enumerate() {
        by_disk.entry(d.key()).or_default().push(ri);
    }
    let mut parts: Vec<(usize, ScanResult)> = thread::scope(|s| {
        let handles: Vec<_> = by_disk
            .into_values()
            .map(|group| {
                s.spawn(move || {
                    let mut res = ScanResult::default();
                    for &ri in &group {
                        scan_root(&roots[ri], ri, filter, exclude, &mut res, counters);
                    }
                    (group[0], res)
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().expect("scan thread")).collect()
    });

    // Merge in source order, so the file list doesn't depend on thread timing.
    parts.sort_by_key(|(first_root, _)| *first_root);
    let mut res = ScanResult::default();
    for (_, part) in parts {
        res.files.extend(part.files);
        res.filtered.extend(part.filtered);
        for (ext, st) in part.filtered_by_ext {
            let e = res.filtered_by_ext.entry(ext).or_default();
            e.count += st.count;
            e.bytes += st.bytes;
        }
        res.skipped_dirs.extend(part.skipped_dirs);
        res.cloud_only.count += part.cloud_only.count;
        res.cloud_only.bytes += part.cloud_only.bytes;
        res.errors.extend(part.errors);
    }
    stage.finish(format!(
        "{} files, {}",
        report::num(seen.load(Ordering::Relaxed)),
        report::human(seen_bytes.load(Ordering::Relaxed))
    ));
    res
}

/// Progress shared by the per-disk scan threads.
struct Counters<'a> {
    stage: &'a Stage,
    seen: &'a AtomicU64,
    seen_bytes: &'a AtomicU64,
}

fn scan_root(
    root: &Path,
    ri: usize,
    filter: &Filter,
    exclude: &[PathBuf],
    res: &mut ScanResult,
    counters: &Counters,
) {
    let mut skipped = Vec::new();
    let walker = WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| {
            if e.depth() == 0 || !e.file_type().is_dir() {
                return true;
            }
            let p = e.path();
            if exclude.iter().any(|x| x.as_path() == p) || filter.skip_dir(p) {
                skipped.push(p.to_path_buf());
                return false;
            }
            true
        });

    for entry in walker {
        let entry = match entry {
            Ok(e) => e,
            Err(err) => {
                let at = err.path().map(paths::display).unwrap_or_default();
                res.errors.push(format!("{at}: {err}"));
                continue;
            }
        };
        // Symlinks and junctions are neither copied nor followed.
        if !entry.file_type().is_file() {
            continue;
        }
        if filter::skip_file(&entry.file_name().to_string_lossy()) {
            continue;
        }
        let meta = match entry.metadata() {
            Ok(m) => m,
            Err(err) => {
                res.errors.push(format!("{}: {err}", paths::display(entry.path())));
                continue;
            }
        };
        let size = meta.len();
        if is_cloud_only(&meta) {
            res.cloud_only.add(size);
            continue;
        }
        let seen = counters.seen.fetch_add(1, Ordering::Relaxed) + 1;
        let bytes = counters.seen_bytes.fetch_add(size, Ordering::Relaxed) + size;
        if seen.is_multiple_of(500) {
            let stage = counters.stage;
            stage
                .bar()
                .set_message(progress::found_message(seen, bytes, stage.elapsed()));
        }
        match filter.decide(entry.path()) {
            Decision::Drop { ext } => {
                res.filtered_by_ext.entry(ext).or_default().add(size);
                res.filtered.push((entry.into_path(), size));
            }
            Decision::Keep(category) => res.files.push(FileEntry {
                mtime: meta.modified().unwrap_or(UNIX_EPOCH),
                path: entry.into_path(),
                root: ri,
                size,
                category,
            }),
        }
    }
    res.skipped_dirs.extend(skipped);
}

#[cfg(windows)]
fn is_cloud_only(meta: &Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_OFFLINE: u32 = 0x1000;
    const FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS: u32 = 0x40_0000;
    meta.file_attributes() & (FILE_ATTRIBUTE_OFFLINE | FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS) != 0
}

#[cfg(not(windows))]
fn is_cloud_only(_meta: &Metadata) -> bool {
    false
}
