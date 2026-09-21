//! A durable JSON-file implementation of `AsyncStorage`.
//!
//! Run with `cargo run --example json_storage -- target/tasks.json`.
//! The example creates a table, saves rows, updates one, deletes one, and
//! reopens the file to prove the final state was persisted.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use my_sqweel::AsyncEngine;
use my_sqweel::model::StoredRow;
use my_sqweel::sql::engine::EngineConfig;
use my_sqweel::storage::{
    AsyncStorage, MetadataMutation, RowMutation, RowPage, RowScan, StorageBatch, StorageCatalog,
    TableState,
};
use serde::{Deserialize, Serialize};

#[derive(Clone)]
struct JsonStorage {
    path: Arc<PathBuf>,
    state: Arc<Mutex<FileState>>,
}

#[derive(Clone, Default, Serialize, Deserialize)]
struct FileState {
    catalog: Option<StorageCatalog>,
    tables: BTreeMap<String, BTreeMap<String, TableState>>,
    rows: BTreeMap<String, BTreeMap<String, BTreeMap<String, StoredRow>>>,
}

impl JsonStorage {
    fn open(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        let state = if path.exists() {
            let bytes = std::fs::read(&path)
                .with_context(|| format!("read JSON storage file {}", path.display()))?;
            serde_json::from_slice(&bytes)
                .with_context(|| format!("decode JSON storage file {}", path.display()))?
        } else {
            FileState::default()
        };
        Ok(Self {
            path: Arc::new(path),
            state: Arc::new(Mutex::new(state)),
        })
    }
}

impl AsyncStorage for JsonStorage {
    async fn load_catalog(&self) -> Result<Option<StorageCatalog>> {
        Ok(self.state.lock().unwrap().catalog.clone())
    }

    async fn list_tables(&self, database: &str) -> Result<Vec<String>> {
        Ok(self
            .state
            .lock()
            .unwrap()
            .tables
            .get(database)
            .map(|tables| tables.keys().cloned().collect())
            .unwrap_or_default())
    }

    async fn load_table(&self, database: &str, table: &str) -> Result<Option<TableState>> {
        Ok(self
            .state
            .lock()
            .unwrap()
            .tables
            .get(database)
            .and_then(|tables| tables.get(table))
            .cloned())
    }

    async fn scan_rows(&self, scan: RowScan) -> Result<RowPage> {
        let state = self.state.lock().unwrap();
        let rows = state
            .rows
            .get(&scan.database)
            .and_then(|tables| tables.get(&scan.table));
        page_rows(rows, scan.cursor.as_deref(), scan.limit)
    }

    async fn commit(&self, batch: StorageBatch) -> Result<()> {
        if batch.is_empty() {
            return Ok(());
        }
        let mut state = self.state.lock().unwrap();
        let mut next = state.clone();
        apply_batch(&mut next, batch);
        write_json_atomically(&self.path, &next)?;
        *state = next;
        Ok(())
    }
}

fn page_rows(
    rows: Option<&BTreeMap<String, StoredRow>>,
    cursor: Option<&str>,
    limit: usize,
) -> Result<RowPage> {
    let mut page = BTreeMap::new();
    let mut more = false;
    for (key, row) in rows.into_iter().flat_map(|rows| rows.iter()) {
        if cursor.is_some_and(|cursor| key.as_str() <= cursor) {
            continue;
        }
        if page.len() == limit.max(1) {
            more = true;
            break;
        }
        page.insert(key.clone(), row.clone());
    }
    Ok(RowPage {
        next_cursor: more.then(|| page.last_key_value().unwrap().0.clone()),
        rows: page,
    })
}

fn apply_batch(state: &mut FileState, batch: StorageBatch) {
    for mutation in batch.metadata {
        match mutation {
            MetadataMutation::PutCatalog(catalog) => state.catalog = Some(catalog),
            MetadataMutation::DeleteDatabase { database } => {
                state.tables.remove(&database);
                state.rows.remove(&database);
            }
            MetadataMutation::PutTable {
                database,
                table,
                state: table_state,
            } => {
                state
                    .tables
                    .entry(database)
                    .or_default()
                    .insert(table, table_state);
            }
            MetadataMutation::DeleteTable { database, table } => {
                if let Some(tables) = state.tables.get_mut(&database) {
                    tables.remove(&table);
                }
                if let Some(tables) = state.rows.get_mut(&database) {
                    tables.remove(&table);
                }
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
            } => {
                state
                    .rows
                    .entry(database)
                    .or_default()
                    .entry(table)
                    .or_default()
                    .insert(key, row);
            }
            RowMutation::Delete {
                database,
                table,
                key,
            } => {
                if let Some(rows) = state
                    .rows
                    .get_mut(&database)
                    .and_then(|tables| tables.get_mut(&table))
                {
                    rows.remove(&key);
                }
            }
        }
    }
}

fn write_json_atomically(path: &Path, state: &FileState) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let temporary = path.with_extension(format!("{}.tmp", uuid::Uuid::new_v4()));
    let encoded = serde_json::to_vec_pretty(state)?;
    std::fs::write(&temporary, encoded)?;
    std::fs::rename(&temporary, path)
        .with_context(|| format!("replace JSON storage file {}", path.display()))?;
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let path = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("target/json-storage-example/tasks.json"));
    let storage = JsonStorage::open(&path)?;
    let database = AsyncEngine::open(EngineConfig::default(), storage).await?;

    database
        .execute_sql(
            "CREATE TABLE IF NOT EXISTS tasks (id INT PRIMARY KEY, title TEXT NOT NULL, done BOOLEAN NOT NULL DEFAULT false)",
        )
        .await?;
    database
        .execute_sql(
            "INSERT INTO tasks VALUES (1, 'write JSON backend', false), (2, 'remove stale task', false) ON DUPLICATE KEY UPDATE title = VALUES(title), done = VALUES(done)",
        )
        .await?;
    database
        .execute_sql("UPDATE tasks SET done = true WHERE id = 1")
        .await?;
    database
        .execute_sql("DELETE FROM tasks WHERE id = 2")
        .await?;
    drop(database);

    let reopened = AsyncEngine::open(EngineConfig::default(), JsonStorage::open(&path)?).await?;
    let result = reopened
        .execute_sql("SELECT id, title, done FROM tasks ORDER BY id")
        .await?;
    println!("persisted JSON file: {}", path.display());
    println!(
        "rows after reopen: {}",
        serde_json::to_string_pretty(&result[0].rows)?
    );
    Ok(())
}
