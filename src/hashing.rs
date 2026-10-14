//! Finding identical files: size -> hash of head and tail -> full hash.
//! Each stage reads from disk only what the previous one did not rule out.

use crate::cache::HashCache;
use crate::paths;
use crate::report;
use crate::scan::FileEntry;
use rayon::prelude::*;
use std::collections::HashMap;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

pub type Hash = [u8; 32];

const PARTIAL_BLOCK: u64 = 16 * 1024;
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
    let mut h = blake3::Hasher::new();
    h.update_reader(File::open(path)?)?;
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
pub fn find_duplicates(files: &[FileEntry], cache: &HashCache) -> (Vec<DupGroup>, Vec<String>) {
    let mut errors = Vec::new();

    let mut by_size: HashMap<u64, Vec<usize>> = HashMap::new();
    for (i, f) in files.iter().enumerate() {
        if f.size > 0 {
            by_size.entry(f.size).or_default().push(i);
        }
    }
    let mut need_full = Vec::new();
    let mut need_partial = Vec::new();
    for (size, idx) in by_size {
        if idx.len() < 2 {
            continue;
        }
        if size < PARTIAL_MIN {
            need_full.extend(idx);
        } else {
            need_partial.extend(idx);
        }
    }

    let pb = report::count_bar(need_partial.len() as u64, "Quick check");
    let partial: Vec<(usize, io::Result<Hash>)> = need_partial
        .par_iter()
        .map(|&i| {
            let r = partial_hash(&files[i].path, files[i].size);
            pb.inc(1);
            (i, r)
        })
        .collect();
    pb.finish_and_clear();
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

    let entries: Vec<&FileEntry> = need_full.iter().map(|&i| &files[i]).collect();
    let cached = cache.get_many(&entries);
    let mut full: Vec<(usize, Hash)> = Vec::with_capacity(need_full.len());
    let mut todo = Vec::new();
    for (&i, hit) in need_full.iter().zip(cached) {
        match hit {
            Some(h) => full.push((i, h)),
            None => todo.push(i),
        }
    }

    let total: u64 = todo.iter().map(|&i| files[i].size).sum();
    let pb = report::bytes_bar(total, "Hashing");
    let computed: Vec<(usize, io::Result<Hash>)> = todo
        .par_iter()
        .map(|&i| {
            let r = hash_file(&files[i].path);
            pb.inc(files[i].size);
            (i, r)
        })
        .collect();
    pb.finish_and_clear();
    let mut fresh = Vec::new();
    for (i, r) in computed {
        match r {
            Ok(h) => {
                full.push((i, h));
                fresh.push((&files[i], h));
            }
            Err(e) => errors.push(read_error(&files[i].path, &e)),
        }
    }
    cache.put_many(&fresh);

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

fn read_error(path: &Path, e: &io::Error) -> String {
    format!("{}: {e}", paths::display(path))
}
