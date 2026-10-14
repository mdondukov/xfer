//! Source traversal: collect files that pass the filter and stats on what was filtered out.

use crate::filter::{self, Category, Decision, Filter};
use crate::paths;
use crate::progress::{self, Stage};
use crate::report;
use std::collections::BTreeMap;
use std::fs::Metadata;
use std::path::PathBuf;
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
pub fn scan(roots: &[PathBuf], filter: &Filter, exclude: &[PathBuf]) -> ScanResult {
    let stage = Stage::spinner("Scanning");
    let mut res = ScanResult::default();
    let mut seen: u64 = 0;
    let mut seen_bytes: u64 = 0;

    for (ri, root) in roots.iter().enumerate() {
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
            seen += 1;
            seen_bytes += size;
            if seen.is_multiple_of(500) {
                stage
                    .bar()
                    .set_message(progress::found_message(seen, seen_bytes, stage.elapsed()));
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
    stage.finish(format!(
        "{} files, {}",
        report::num(seen),
        report::human(seen_bytes)
    ));
    res
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
