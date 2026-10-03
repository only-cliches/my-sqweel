pub(crate) mod custom;
pub(crate) mod delta;
pub use custom::{CustomStorage, Storage};
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Result, anyhow};
use rust_rocksdb::{DB, Direction, IteratorMode, Options, WriteBatch};
use tempfile::TempDir;

mod async_store;
pub use async_store::{
    AsyncStorage, DatabaseMetadata, MetadataMutation, RocksDbStorage, RowMutation, RowPage,
    RowScan, StorageBatch, StorageCatalog, TableState,
};

/// Atomic mutations stored in RocksDB's key-value database.
pub enum StorageWrite {
    HSet {
        key: String,
        field: String,
        value: String,
    },
    HDel {
        key: String,
        field: String,
    },
    Del {
        key: String,
    },
}

pub struct RocksDbStore {
    db: Arc<DB>,
    _temporary_dir: Option<TempDir>,
}

/// A RocksDB read view which keeps its database alive until the snapshot drops.
pub(crate) struct RocksDbSnapshot {
    snapshot: std::mem::ManuallyDrop<rust_rocksdb::Snapshot<'static>>,
    _db: Arc<DB>,
}

impl Drop for RocksDbSnapshot {
    fn drop(&mut self) {
        // The snapshot borrows `db`, which remains alive until after this call.
        unsafe { std::mem::ManuallyDrop::drop(&mut self.snapshot) };
    }
}

impl RocksDbSnapshot {
    pub(crate) fn scan_prefix(&self, prefix: &str) -> Result<Vec<(String, String)>> {
        let mut rows = Vec::new();
        for entry in self
            .snapshot
            .iterator(IteratorMode::From(prefix.as_bytes(), Direction::Forward))
        {
            let (key, value) = entry?;
            if !key.starts_with(prefix.as_bytes()) {
                break;
            }
            rows.push((
                String::from_utf8(key.to_vec())?,
                String::from_utf8(value.to_vec())?,
            ));
        }
        Ok(rows)
    }
}

impl RocksDbStore {
    pub fn open(data_dir: Option<&str>) -> Result<Self> {
        let temporary_dir = if data_dir.is_none() {
            Some(
                tempfile::Builder::new()
                    .prefix("my-sqweel-rocksdb-")
                    .tempdir()?,
            )
        } else {
            None
        };
        let path = data_dir
            .map(PathBuf::from)
            .or_else(|| temporary_dir.as_ref().map(|dir| dir.path().to_path_buf()))
            .expect("a persistent or temporary database path exists");
        if path.exists() && path.read_dir()?.next().is_some() && !path.join("CURRENT").exists() {
            return Err(anyhow!(
                "data directory contains unsupported legacy storage; use an empty directory"
            ));
        }
        std::fs::create_dir_all(&path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))?;
        }
        let mut options = Options::default();
        options.create_if_missing(true);
        let db = Arc::new(DB::open(&options, &path).map_err(|error| {
            anyhow!(
                "data directory is already open or could not be opened: {} ({error})",
                path.display()
            )
        })?);
        Ok(Self {
            db,
            _temporary_dir: temporary_dir,
        })
    }

    pub fn get(&self, key: &str) -> Result<Option<Vec<u8>>> {
        Ok(self.db.get(key.as_bytes())?)
    }

    pub(crate) fn snapshot(&self) -> RocksDbSnapshot {
        let db = self.db.clone();
        let snapshot = db.snapshot();
        // SAFETY: RocksDbSnapshot owns this Arc<DB> and drops the snapshot first.
        let snapshot = unsafe {
            std::mem::transmute::<rust_rocksdb::Snapshot<'_>, rust_rocksdb::Snapshot<'static>>(
                snapshot,
            )
        };
        RocksDbSnapshot {
            snapshot: std::mem::ManuallyDrop::new(snapshot),
            _db: db,
        }
    }

    pub fn scan_prefix(&self, prefix: &str) -> Result<Vec<(String, String)>> {
        self.scan_prefix_page(prefix, None, usize::MAX)
    }

    pub fn scan_prefix_page(
        &self,
        prefix: &str,
        cursor: Option<&str>,
        count: usize,
    ) -> Result<Vec<(String, String)>> {
        let start = cursor.unwrap_or(prefix);
        let mut rows = Vec::with_capacity(count.min(1024));
        for entry in self
            .db
            .iterator(IteratorMode::From(start.as_bytes(), Direction::Forward))
        {
            let (key, value) = entry?;
            if !key.starts_with(prefix.as_bytes()) {
                break;
            }
            let key = String::from_utf8(key.to_vec())?;
            if cursor.is_some_and(|cursor| key.as_str() <= cursor) {
                continue;
            }
            rows.push((key, String::from_utf8(value.to_vec())?));
            if rows.len() >= count.max(1) {
                break;
            }
        }
        Ok(rows)
    }

    pub fn hset(&self, key: &str, field: &str, value: &str) -> Result<()> {
        self.db
            .put(field_key(key, field).as_bytes(), value.as_bytes())?;
        Ok(())
    }

    pub fn hdel(&self, key: &str, field: &str) -> Result<()> {
        self.db.delete(field_key(key, field).as_bytes())?;
        Ok(())
    }

    pub fn hgetall(&self, key: &str) -> Result<BTreeMap<String, String>> {
        let prefix = namespace_prefix(key);
        self.scan_prefix(&prefix)?
            .into_iter()
            .map(|(key, value)| {
                Ok((
                    key.strip_prefix(&prefix)
                        .ok_or_else(|| anyhow!("invalid RocksDB namespace key"))?
                        .to_owned(),
                    value,
                ))
            })
            .collect()
    }

    pub fn hscan(
        &self,
        key: &str,
        cursor: Option<&str>,
        count: usize,
    ) -> Result<(String, Vec<(String, String)>)> {
        let prefix = namespace_prefix(key);
        let cursor = cursor.filter(|cursor| *cursor != "0");
        let start = cursor.map(|cursor| format!("{prefix}{cursor}"));
        let rows = self.scan_prefix_page(&prefix, start.as_deref(), count.saturating_add(1))?;
        let has_more = rows.len() > count.max(1);
        let mut rows = rows
            .into_iter()
            .take(count.max(1))
            .map(|(key, value)| (key[prefix.len()..].to_owned(), value))
            .collect::<Vec<_>>();
        let next = if has_more {
            rows.last()
                .map(|(field, _)| field.clone())
                .unwrap_or_default()
        } else {
            "0".to_owned()
        };
        Ok((next, std::mem::take(&mut rows)))
    }

    pub fn keys(&self, pattern: &str) -> Result<Vec<String>> {
        let prefix = pattern.strip_suffix('*').unwrap_or(pattern);
        let mut keys = BTreeSet::new();
        for (stored_key, _) in self.scan_prefix(prefix)? {
            if let Some((namespace, _)) = stored_key.split_once('\0') {
                keys.insert(namespace.to_owned());
            }
        }
        Ok(keys.into_iter().collect())
    }

    pub fn write_batch(&self, writes: Vec<StorageWrite>) -> Result<()> {
        let mut batch = WriteBatch::default();
        for write in writes {
            match write {
                StorageWrite::HSet { key, field, value } => {
                    batch.put(field_key(&key, &field).as_bytes(), value.as_bytes());
                }
                StorageWrite::HDel { key, field } => {
                    batch.delete(field_key(&key, &field).as_bytes());
                }
                StorageWrite::Del { key } => {
                    for (stored_key, _) in self.scan_prefix(&namespace_prefix(&key))? {
                        batch.delete(stored_key.as_bytes());
                    }
                }
            }
        }
        self.db.write(&batch)?;
        Ok(())
    }
}

fn namespace_prefix(key: &str) -> String {
    format!("{key}\0")
}

fn field_key(key: &str, field: &str) -> String {
    format!("{}{field}", namespace_prefix(key))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refuses_nonempty_directory_without_overwriting_existing_data() {
        let directory = tempfile::tempdir().unwrap();
        let legacy_file = directory.path().join("legacy.data");
        std::fs::write(&legacy_file, "keep me").unwrap();
        assert!(RocksDbStore::open(Some(directory.path().to_str().unwrap())).is_err());
        assert_eq!(std::fs::read_to_string(legacy_file).unwrap(), "keep me");
    }
}
