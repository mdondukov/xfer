//! Full-hash cache shared between runs. Key: path; value: size, mtime and hash.
//! An entry whose size or mtime no longer matches the file is treated as stale.

use crate::hashing::Hash;
use crate::scan::FileEntry;
use anyhow::{Context, Result};
use redb::{Database, ReadableDatabase, TableDefinition};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

const TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("hashes");
const VALUE_LEN: usize = 8 + 16 + 32;

pub struct HashCache {
    db: Option<Database>,
}

impl HashCache {
    pub fn open(disabled: bool) -> Self {
        if disabled {
            return HashCache { db: None };
        }
        match open_db() {
            Ok(db) => HashCache { db: Some(db) },
            Err(e) => {
                eprintln!("Hash cache unavailable ({e:#}), continuing without it");
                HashCache { db: None }
            }
        }
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

    pub fn put_many(&self, items: &[(&FileEntry, Hash)]) {
        let Some(db) = &self.db else { return };
        let store = || -> Result<()> {
            let tx = db.begin_write()?;
            {
                let mut table = tx.open_table(TABLE)?;
                for (f, h) in items {
                    let key = f.path.to_string_lossy();
                    table.insert(key.as_ref(), &encode(f, h)[..])?;
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
