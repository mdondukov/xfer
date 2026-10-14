//! Finding identical files: size -> hash of head and tail -> full hash.
//! Each stage reads from disk only what the previous one did not rule out.

use crate::cache::HashCache;
use crate::disk::Disk;
use crate::paths;
use crate::progress::{self, Stage};
use crate::report;
use crate::scan::FileEntry;
use rayon::prelude::*;
use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

pub type Hash = [u8; 32];

const PARTIAL_BLOCK: u64 = 16 * 1024;
const HASH_CHUNK: usize = 1024 * 1024;
/// Files smaller than this are hashed in full right away: two passes cost more than one.
const PARTIAL_MIN: u64 = 4 * PARTIAL_BLOCK;

pub struct DupGroup {
    pub hash: Hash,
    pub size: u64,
    /// Indices into the file list, ascending.
    pub files: Vec<usize>,
}

impl DupGroup {
    /// Space taken by the extra copies.
    pub fn wasted(&self) -> u64 {
        self.size * (self.files.len() as u64 - 1)
    }
}

pub fn hash_file(path: &Path) -> io::Result<Hash> {
    hash_file_with(path, |_| {})
}

/// Hashes in 1 MB chunks, reporting progress after each one, so a multi-GB video
/// moves the bar smoothly and Ctrl+C doesn't have to wait for it to finish.
fn hash_file_with(path: &Path, on_progress: impl Fn(u64)) -> io::Result<Hash> {
    let mut f = File::open(path)?;
    let mut buf = vec![0u8; HASH_CHUNK];
    let mut h = blake3::Hasher::new();
    loop {
        if progress::interrupted() {
            return Err(io::Error::new(io::ErrorKind::Interrupted, "stopped by Ctrl+C"));
        }
        let n = match f.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        h.update(&buf[..n]);
        on_progress(n as u64);
    }
    Ok(*h.finalize().as_bytes())
}

fn partial_hash(path: &Path, size: u64) -> io::Result<Hash> {
    let mut f = File::open(path)?;
    let mut buf = vec![0u8; PARTIAL_BLOCK as usize];
    let mut h = blake3::Hasher::new();
    f.read_exact(&mut buf)?;
    h.update(&buf);
    f.seek(SeekFrom::Start(size - PARTIAL_BLOCK))?;
    f.read_exact(&mut buf)?;
    h.update(&buf);
    Ok(*h.finalize().as_bytes())
}

pub fn to_hex(h: &Hash) -> String {
    blake3::Hash::from_bytes(*h).to_hex().to_string()
}

pub fn from_hex(s: &str) -> Option<Hash> {
    blake3::Hash::from_hex(s.trim()).ok().map(|h| *h.as_bytes())
}

/// Returns groups of identical files (most wasteful first) and read errors.
/// Empty files are never duplicates: they take no space.
/// `disks[r]` is the disk of source root `r`: hard drives are read sequentially.
pub fn find_duplicates(
    files: &[FileEntry],
    disks: &[Disk],
    cache: &HashCache,
) -> (Vec<DupGroup>, Vec<String>) {
    let mut errors = Vec::new();

    let mut by_size: HashMap<u64, Vec<usize>> = HashMap::new();
    for (i, f) in files.iter().enumerate() {
        if f.size > 0 {
            by_size.entry(f.size).or_default().push(i);
        }
    }
    let mut need_full = Vec::new();
    let mut need_partial = Vec::new();
    let mut small = 0usize;
    for (size, idx) in by_size {
        if idx.len() < 2 {
            continue;
        }
        if size < PARTIAL_MIN {
            small += idx.len();
            need_full.extend(idx);
        } else {
            need_partial.extend(idx);
        }
    }

    // Ctrl+C stops after the files in progress; what was hashed stays in the cache.
    progress::set_graceful(true);
    let stage = Stage::count("Quick check", need_partial.len() as u64);
    let partial = per_disk(&need_partial, files, disks, |i| {
        if progress::interrupted() {
            return Err(io::Error::new(io::ErrorKind::Interrupted, "stopped by Ctrl+C"));
        }
        let r = partial_hash(&files[i].path, files[i].size);
        stage.bar().inc(1);
        r
    });
    exit_if_interrupted(cache);
    let mut by_partial: HashMap<(u64, Hash), Vec<usize>> = HashMap::new();
    for (i, r) in partial {
        match r {
            Ok(h) => by_partial.entry((files[i].size, h)).or_default().push(i),
            Err(e) => errors.push(read_error(&files[i].path, &e)),
        }
    }
    for idx in by_partial.into_values() {
        if idx.len() >= 2 {
            need_full.extend(idx);
        }
    }
    stage.finish(format!(
        "{} files with a same-size twin, {} need a full hash",
        report::num(need_partial.len() as u64 + small as u64),
        report::num(need_full.len() as u64)
    ));

    let entries: Vec<&FileEntry> = need_full.iter().map(|&i| &files[i]).collect();
    let cached = cache.get_many(&entries);
    let mut full: Vec<(usize, Hash)> = Vec::with_capacity(need_full.len());
    let mut todo = Vec::new();
    let cached_count = cached.iter().filter(|c| c.is_some()).count();
    for (&i, hit) in need_full.iter().zip(cached) {
        match hit {
            Some(h) => full.push((i, h)),
            None => todo.push(i),
        }
    }

    let total: u64 = todo.iter().map(|&i| files[i].size).sum();
    let stage = Stage::bytes("Hashing", total);
    let hashed = AtomicU64::new(0);
    let todo_count = report::num(todo.len() as u64);
    stage.bar().set_message(format!("0/{todo_count} files"));
    let computed = per_disk(&todo, files, disks, |i| {
        let f = &files[i];
        let r = hash_file_with(&f.path, |n| stage.bar().inc(n));
        if let Ok(h) = &r {
            cache.put(f, h);
        }
        if !progress::interrupted() {
            let n = hashed.fetch_add(1, Ordering::Relaxed) + 1;
            stage.bar().set_message(format!("{}/{todo_count} files", report::num(n)));
        }
        r
    });
    exit_if_interrupted(cache);
    progress::set_graceful(false);
    cache.flush();
    stage.finish(format!(
        "{} hashed, {} from cache",
        report::human(total),
        report::num(cached_count as u64)
    ));
    for (i, r) in computed {
        match r {
            Ok(h) => full.push((i, h)),
            Err(e) => errors.push(read_error(&files[i].path, &e)),
        }
    }

    let mut by_full: HashMap<(u64, Hash), Vec<usize>> = HashMap::new();
    for (i, h) in full {
        by_full.entry((files[i].size, h)).or_default().push(i);
    }
    let mut groups: Vec<DupGroup> = by_full
        .into_iter()
        .filter(|(_, idx)| idx.len() >= 2)
        .map(|((size, hash), mut idx)| {
            idx.sort_unstable();
            DupGroup { hash, size, files: idx }
        })
        .collect();
    groups.sort_by(|a, b| b.wasted().cmp(&a.wasted()).then(a.hash.cmp(&b.hash)));
    (groups, errors)
}

/// Runs `f` over files grouped by physical disk: disks in parallel, each hard
/// drive sequentially in scan order (neighbouring files tend to sit close on the
/// platter), each SSD on the rayon pool (`--jobs`).
fn per_disk<T, F>(idx: &[usize], files: &[FileEntry], disks: &[Disk], f: F) -> Vec<(usize, T)>
where
    T: Send,
    F: Fn(usize) -> T + Sync,
{
    let mut groups: BTreeMap<String, (bool, Vec<usize>)> = BTreeMap::new();
    for &i in idx {
        let d = &disks[files[i].root];
        groups.entry(d.key()).or_insert((d.sequential(), Vec::new())).1.push(i);
    }
    let f = &f;
    thread::scope(|s| {
        let handles: Vec<_> = groups
            .into_values()
            .map(|(sequential, mut group)| {
                s.spawn(move || {
                    group.sort_unstable();
                    if sequential {
                        group.iter().map(|&i| (i, f(i))).collect::<Vec<_>>()
                    } else {
                        group.par_iter().map(|&i| (i, f(i))).collect()
                    }
                })
            })
            .collect();
        handles
            .into_iter()
            .flat_map(|h| h.join().expect("hashing thread"))
            .collect()
    })
}

/// Which copy to keep: one under a --prefer folder; then one outside redundant
/// folders (`inside_redundant`), so an old backup can be dropped as a whole;
/// then the shortest path; then the oldest.
pub fn pick_keeper(
    group: &DupGroup,
    files: &[FileEntry],
    prefer: &[PathBuf],
    inside_redundant: &[bool],
) -> usize {
    *group
        .files
        .iter()
        .min_by_key(|&&i| {
            let f = &files[i];
            (
                !prefer.iter().any(|p| f.path.starts_with(p)),
                inside_redundant[i],
                f.path.as_os_str().len(),
                f.mtime,
                &f.path,
            )
        })
        .expect("a duplicate group has at least two files")
}

/// After Ctrl+C while hashing: save what was hashed and stop. A rerun picks
/// up from the cache, so nothing hashed so far is lost.
fn exit_if_interrupted(cache: &HashCache) {
    if progress::interrupted() {
        cache.flush();
        eprintln!("\nStopped. Hashes computed so far are saved and will be reused by the next run.");
        std::process::exit(130);
    }
}

fn read_error(path: &Path, e: &io::Error) -> String {
    format!("{}: {e}", paths::display(path))
}
