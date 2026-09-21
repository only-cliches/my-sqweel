//! Demand-driven async persistence boundary for embedders.
//!
//! Storage implementations expose catalog, schema, and row operations. They
//! never need to construct an in-memory database image: table scans use a
//! cursor and commits contain only the changed metadata and rows.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Result, anyhow};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::model::StoredRow;
use crate::schema::TableSchemaHint;

use super::{LuxRedisStore, RedisStore, StorageWrite};

const CATALOG_KEY: &str = "sqweel:async:v2:catalog";
const TABLES_KEY: &str = "sqweel:async:v2:tables";
const ROWS_PREFIX: &str = "sqweel:async:v2:rows:";

/// Metadata shared by the tables in one logical database.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct DatabaseMetadata {
    #[serde(default)]
    pub views: BTreeMap<String, String>,
    #[serde(default)]
    pub index_comments: BTreeMap<String, String>,
}

/// The catalog and non-row metadata required to open an engine.
///
/// `metadata` is MySqweel's opaque account/database catalog. Backends must
/// preserve it byte-for-byte through a load/commit round trip.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StorageCatalog {
    pub version: u32,
    pub metadata: Value,
    pub databases: BTreeMap<String, DatabaseMetadata>,
}

/// Schema and table-local metadata. Row contents are read separately.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TableState {
    pub schema: TableSchemaHint,
    pub auto_increment: Option<i64>,
}

/// A bounded page request for one table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RowScan {
    pub database: String,
    pub table: String,
    /// Opaque continuation token returned by the previous page. `None` starts
    /// a new scan. Backends may encode a primary key, an offset, or a remote
    /// service cursor in this value.
    pub cursor: Option<String>,
    /// Maximum number of rows to return. Backends may return fewer rows.
    pub limit: usize,
}

/// One page returned by [`AsyncStorage::scan_rows`].
#[derive(Debug, Clone)]
pub struct RowPage {
    pub rows: BTreeMap<String, StoredRow>,
    /// Pass this opaque token as `RowScan::cursor` for the next page. `None`
    /// means the table scan is complete.
    pub next_cursor: Option<String>,
}

/// A row-level change included in an atomic storage commit.
#[derive(Debug, Clone)]
pub enum RowMutation {
    Put {
        database: String,
        table: String,
        key: String,
        row: StoredRow,
    },
    Delete {
        database: String,
        table: String,
        key: String,
    },
}

/// A schema or catalog-level change included in an atomic storage commit.
#[derive(Debug, Clone)]
pub enum MetadataMutation {
    PutCatalog(StorageCatalog),
    DeleteDatabase {
        database: String,
    },
    PutTable {
        database: String,
        table: String,
        state: TableState,
    },
    DeleteTable {
        database: String,
        table: String,
    },
}

/// All changes from one successful SQL execution.
///
/// `metadata` is applied before `rows`; `commit` must apply the complete
/// batch atomically. Backends can map this to a database transaction, a
/// versioned document update, or an atomic file replacement.
#[derive(Debug, Clone, Default)]
pub struct StorageBatch {
    pub metadata: Vec<MetadataMutation>,
    pub rows: Vec<RowMutation>,
}

impl StorageBatch {
    pub fn is_empty(&self) -> bool {
        self.metadata.is_empty() && self.rows.is_empty()
    }
}

/// Async table storage and retrieval contract for a MySqweel instance.
///
/// The contract is deliberately granular. Implementations can fetch a schema
/// on demand and produce rows from a cursor, without loading a whole table or
/// database into their own memory. The engine never asks a backend for an
/// `EngineState` image.
#[allow(async_fn_in_trait)]
pub trait AsyncStorage: Send + Sync + 'static {
    /// Return `None` only for a new, empty backend.
    async fn load_catalog(&self) -> Result<Option<StorageCatalog>>;
    async fn list_tables(&self, database: &str) -> Result<Vec<String>>;
    async fn load_table(&self, database: &str, table: &str) -> Result<Option<TableState>>;
    async fn scan_rows(&self, scan: RowScan) -> Result<RowPage>;
    async fn commit(&self, batch: StorageBatch) -> Result<()>;
}

/// The built-in async backend, backed by the embedded Lux store.
#[derive(Clone)]
pub struct LuxStorage {
    inner: Arc<LuxRedisStore>,
}

impl LuxStorage {
    pub async fn open(data_dir: Option<PathBuf>) -> Result<Self> {
        let inner = tokio::task::spawn_blocking(move || {
            let path = data_dir
                .as_ref()
                .map(|path| path.to_string_lossy().into_owned());
            LuxRedisStore::open(path.as_deref())
        })
        .await
        .map_err(|error| anyhow!("Lux storage worker failed: {error}"))??;
        Ok(Self {
            inner: Arc::new(inner),
        })
    }
}

impl AsyncStorage for LuxStorage {
    async fn load_catalog(&self) -> Result<Option<StorageCatalog>> {
        let store = self.inner.clone();
        tokio::task::spawn_blocking(move || {
            store
                .hgetall(CATALOG_KEY)?
                .get("catalog")
                .map(|encoded| serde_json::from_str(encoded).map_err(Into::into))
                .transpose()
        })
        .await
        .map_err(|error| anyhow!("Lux storage worker failed: {error}"))?
    }

    async fn list_tables(&self, database: &str) -> Result<Vec<String>> {
        let store = self.inner.clone();
        let database = database.to_owned();
        tokio::task::spawn_blocking(move || {
            Ok(store
                .hgetall(TABLES_KEY)?
                .into_keys()
                .filter_map(|field| decode_table_key(&field))
                .filter(|(candidate, _)| candidate == &database)
                .map(|(_, table)| table)
                .collect())
        })
        .await
        .map_err(|error| anyhow!("Lux storage worker failed: {error}"))?
    }

    async fn load_table(&self, database: &str, table: &str) -> Result<Option<TableState>> {
        let store = self.inner.clone();
        let field = encode_table_key(database, table)?;
        tokio::task::spawn_blocking(move || {
            store
                .hgetall(TABLES_KEY)?
                .get(&field)
                .map(|encoded| serde_json::from_str(encoded).map_err(Into::into))
                .transpose()
        })
        .await
        .map_err(|error| anyhow!("Lux storage worker failed: {error}"))?
    }

    async fn scan_rows(&self, scan: RowScan) -> Result<RowPage> {
        let store = self.inner.clone();
        tokio::task::spawn_blocking(move || {
            let key = rows_key(&scan.database, &scan.table)?;
            let (next_cursor, values) = store.hscan(&key, scan.cursor.as_deref(), scan.limit)?;
            let rows = values
                .into_iter()
                .map(|(key, value)| Ok((key, serde_json::from_str(&value)?)))
                .collect::<Result<BTreeMap<_, _>>>()?;
            Ok(RowPage {
                rows,
                next_cursor: (next_cursor != "0").then_some(next_cursor),
            })
        })
        .await
        .map_err(|error| anyhow!("Lux storage worker failed: {error}"))?
    }

    async fn commit(&self, batch: StorageBatch) -> Result<()> {
        if batch.is_empty() {
            return Ok(());
        }
        let store = self.inner.clone();
        tokio::task::spawn_blocking(move || store.write_batch(lux_writes(batch)?))
            .await
            .map_err(|error| anyhow!("Lux storage worker failed: {error}"))?
    }
}

fn lux_writes(batch: StorageBatch) -> Result<Vec<StorageWrite>> {
    let mut writes = Vec::new();
    for mutation in batch.metadata {
        match mutation {
            MetadataMutation::PutCatalog(catalog) => writes.push(StorageWrite::HSet {
                key: CATALOG_KEY.into(),
                field: "catalog".into(),
                value: serde_json::to_string(&catalog)?,
            }),
            MetadataMutation::DeleteDatabase { .. } => {}
            MetadataMutation::PutTable {
                database,
                table,
                state,
            } => writes.push(StorageWrite::HSet {
                key: TABLES_KEY.into(),
                field: encode_table_key(&database, &table)?,
                value: serde_json::to_string(&state)?,
            }),
            MetadataMutation::DeleteTable { database, table } => {
                writes.push(StorageWrite::HDel {
                    key: TABLES_KEY.into(),
                    field: encode_table_key(&database, &table)?,
                });
                writes.push(StorageWrite::Del {
                    key: rows_key(&database, &table)?,
                });
            }
        }
    }
    for mutation in batch.rows {
        match mutation {
            RowMutation::Put {
                database,
                table,
                key,
                row,
            } => writes.push(StorageWrite::HSet {
                key: rows_key(&database, &table)?,
                field: key,
                value: serde_json::to_string(&row)?,
            }),
            RowMutation::Delete {
                database,
                table,
                key,
            } => writes.push(StorageWrite::HDel {
                key: rows_key(&database, &table)?,
                field: key,
            }),
        }
    }
    Ok(writes)
}

fn encode_table_key(database: &str, table: &str) -> Result<String> {
    Ok(serde_json::to_string(&(database, table))?)
}

fn decode_table_key(value: &str) -> Option<(String, String)> {
    serde_json::from_str(value).ok()
}

fn rows_key(database: &str, table: &str) -> Result<String> {
    Ok(format!(
        "{ROWS_PREFIX}{}",
        encode_table_key(database, table)?
    ))
}
