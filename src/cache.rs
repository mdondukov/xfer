//! Full-hash cache shared between runs. Key: path; value: size, mtime and hash.
//! An entry whose size or mtime no longer matches the file is treated as stale.
//!
//! New hashes are buffered and committed every few seconds, so an interrupted
//! run keeps what it already hashed; on a hard drive that can be hours of work.

use crate::hashing::Hash;
use crate::scan::FileEntry;
use anyhow::{Context, Result};
use redb::{Database, ReadableDatabase, TableDefinition};
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("hashes");
const VALUE_LEN: usize = 8 + 16 + 32;

/// Commit buffered hashes at least this often...
const FLUSH_EVERY: Duration = Duration::from_secs(10);
/// ...or once this many have piled up.
const FLUSH_BATCH: usize = 1000;

pub struct HashCache {
    db: Option<Database>,
    pending: Mutex<Pending>,
}

struct Pending {
    items: Vec<(String, [u8; VALUE_LEN])>,
    last_flush: Instant,
}

impl HashCache {
    pub fn open(disabled: bool) -> Self {
        let db = if disabled {
            None
        } else {
            match open_db() {
                Ok(db) => Some(db),
                Err(e) => {
                    eprintln!("Hash cache unavailable ({e:#}), continuing without it");
                    None
                }
            }
        };
        let pending = Pending { items: Vec::new(), last_flush: Instant::now() };
        HashCache { db, pending: Mutex::new(pending) }
    }

    pub fn get_many(&self, files: &[&FileEntry]) -> Vec<Option<Hash>> {
        let Some(db) = &self.db else {
            return vec![None; files.len()];
        };
        let lookup = || -> Result<Vec<Option<Hash>>> {
            let tx = db.begin_read()?;
            let table = tx.open_table(TABLE)?;
            let mut out = Vec::with_capacity(files.len());
            for f in files {
                let key = f.path.to_string_lossy();
                let hit = table.get(key.as_ref())?.and_then(|v| decode(v.value(), f));
                out.push(hit);
            }
            Ok(out)
        };
        lookup().unwrap_or_else(|_| vec![None; files.len()])
    }

    /// Buffers one fresh hash; safe to call from many threads.
    pub fn put(&self, f: &FileEntry, h: &Hash) {
        if self.db.is_none() {
            return;
        }
        let batch = {
            let mut p = self.pending.lock().expect("cache lock");
            p.items.push((f.path.to_string_lossy().into_owned(), encode(f, h)));
            if p.items.len() < FLUSH_BATCH && p.last_flush.elapsed() < FLUSH_EVERY {
                return;
            }
            p.last_flush = Instant::now();
            std::mem::take(&mut p.items)
        };
        self.write(&batch);
    }

    /// Commits everything buffered so far.
    pub fn flush(&self) {
        let batch = std::mem::take(&mut self.pending.lock().expect("cache lock").items);
        self.write(&batch);
    }

    fn write(&self, items: &[(String, [u8; VALUE_LEN])]) {
        let Some(db) = &self.db else { return };
        if items.is_empty() {
            return;
        }
        let store = || -> Result<()> {
            let tx = db.begin_write()?;
            {
                let mut table = tx.open_table(TABLE)?;
                for (key, value) in items {
                    table.insert(key.as_str(), &value[..])?;
                }
            }
            tx.commit()?;
            Ok(())
        };
        if let Err(e) = store() {
            eprintln!("Failed to save hash cache: {e:#}");
        }
    }
}

impl Drop for HashCache {
    fn drop(&mut self) {
        self.flush();
    }
}

fn open_db() -> Result<Database> {
    let dir = cache_dir().context("cache folder not found")?;
    std::fs::create_dir_all(&dir)?;
    let db = Database::create(dir.join("hash-cache.redb"))?;
    // Create the table up front, otherwise the first read fails with "table does not exist".
    let tx = db.begin_write()?;
    tx.open_table(TABLE)?;
    tx.commit()?;
    Ok(db)
}

fn cache_dir() -> Option<PathBuf> {
    if cfg!(windows) {
        return std::env::var_os("LOCALAPPDATA").map(|d| PathBuf::from(d).join("xfer"));
    }
    std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
        .map(|d| d.join("xfer"))
}

fn nanos(t: SystemTime) -> u128 {
    t.duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0)
}

fn encode(f: &FileEntry, h: &Hash) -> [u8; VALUE_LEN] {
    let mut buf = [0u8; VALUE_LEN];
    buf[..8].copy_from_slice(&f.size.to_le_bytes());
    buf[8..24].copy_from_slice(&nanos(f.mtime).to_le_bytes());
    buf[24..].copy_from_slice(h);
    buf
}

fn decode(buf: &[u8], f: &FileEntry) -> Option<Hash> {
    if buf.len() != VALUE_LEN {
        return None;
    }
    let size = u64::from_le_bytes(buf[..8].try_into().ok()?);
    let mtime = u128::from_le_bytes(buf[8..24].try_into().ok()?);
    if size != f.size || mtime != nanos(f.mtime) {
        return None;
    }
    buf[24..].try_into().ok()
}
