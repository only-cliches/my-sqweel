//! A durable CSV-table implementation of `AsyncStorage`.
//!
//! Run with `cargo run --example csv_storage -- target/tasks-csv`.
//! Every successful SQL statement writes a new CSV generation and atomically
//! changes `CURRENT` to that generation. The example creates, saves, updates,
//! deletes, and then reopens a table.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, anyhow};
use my_sqweel::AsyncEngine;
use my_sqweel::model::StoredRow;
use my_sqweel::sql::engine::EngineConfig;
use my_sqweel::storage::{
    AsyncStorage, MetadataMutation, RowMutation, RowPage, RowScan, StorageBatch, StorageCatalog,
    TableState,
};
use serde::{Deserialize, Serialize};

#[derive(Clone)]
struct CsvStorage {
    root: Arc<PathBuf>,
    state: Arc<Mutex<CsvState>>,
}

#[derive(Clone, Default)]
struct CsvState {
    catalog: Option<StorageCatalog>,
    tables: BTreeMap<String, BTreeMap<String, TableState>>,
    rows: BTreeMap<String, BTreeMap<String, BTreeMap<String, StoredRow>>>,
}

#[derive(Serialize, Deserialize)]
struct CsvManifest {
    catalog: Option<StorageCatalog>,
    tables: BTreeMap<String, BTreeMap<String, TableState>>,
}

impl CsvStorage {
    fn open(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        std::fs::create_dir_all(root.join("generations"))?;
        let state = load_current_generation(&root)?;
        Ok(Self {
            root: Arc::new(root),
            state: Arc::new(Mutex::new(state)),
        })
    }
}

impl AsyncStorage for CsvStorage {
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
        write_generation_atomically(&self.root, &next)?;
        *state = next;
        Ok(())
    }
}

fn load_current_generation(root: &Path) -> Result<CsvState> {
    let current = root.join("CURRENT");
    if !current.exists() {
        return Ok(CsvState::default());
    }
    let name = std::fs::read_to_string(&current)?.trim().to_owned();
    let mut components = Path::new(&name).components();
    if !matches!(components.next(), Some(std::path::Component::Normal(_)))
        || components.next().is_some()
    {
        return Err(anyhow!("invalid CSV storage CURRENT pointer"));
    }
    let generation = root.join("generations").join(name);
    let manifest: CsvManifest =
        serde_json::from_slice(&std::fs::read(generation.join("manifest.json"))?)
            .context("decode CSV storage manifest")?;
    let mut rows = BTreeMap::new();
    for (database, tables) in &manifest.tables {
        let mut database_rows = BTreeMap::new();
        for table in tables.keys() {
            let path = generation.join("rows").join(row_file_name(database, table));
            database_rows.insert(table.clone(), read_rows_csv(&path)?);
        }
        rows.insert(database.clone(), database_rows);
    }
    Ok(CsvState {
        catalog: manifest.catalog,
        tables: manifest.tables,
        rows,
    })
}

fn write_generation_atomically(root: &Path, state: &CsvState) -> Result<()> {
    let generations = root.join("generations");
    std::fs::create_dir_all(&generations)?;
    let name = format!("generation-{}", uuid::Uuid::new_v4());
    let staging = generations.join(format!(".{name}.tmp"));
    let final_path = generations.join(&name);
    std::fs::create_dir_all(staging.join("rows"))?;

    let manifest = CsvManifest {
        catalog: state.catalog.clone(),
        tables: state.tables.clone(),
    };
    std::fs::write(
        staging.join("manifest.json"),
        serde_json::to_vec_pretty(&manifest)?,
    )?;
    for (database, tables) in &state.tables {
        for table in tables.keys() {
            let rows = state
                .rows
                .get(database)
                .and_then(|tables| tables.get(table))
                .cloned()
                .unwrap_or_default();
            write_rows_csv(
                &staging.join("rows").join(row_file_name(database, table)),
                &rows,
            )?;
        }
    }

    std::fs::rename(&staging, &final_path)?;
    let temporary_pointer = root.join(format!(".CURRENT.{}.tmp", uuid::Uuid::new_v4()));
    std::fs::write(&temporary_pointer, format!("{name}\n"))?;
    std::fs::rename(&temporary_pointer, root.join("CURRENT"))
        .context("replace CSV storage CURRENT pointer")?;
    Ok(())
}

fn write_rows_csv(path: &Path, rows: &BTreeMap<String, StoredRow>) -> Result<()> {
    let mut output = String::from("key,row_json\n");
    for (key, row) in rows {
        output.push_str(&escape_csv(key));
        output.push(',');
        output.push_str(&escape_csv(&serde_json::to_string(row)?));
        output.push('\n');
    }
    std::fs::write(path, output)?;
    Ok(())
}

fn read_rows_csv(path: &Path) -> Result<BTreeMap<String, StoredRow>> {
    let records = parse_csv(&std::fs::read_to_string(path)?)?;
    if records.first().map(Vec::as_slice) != Some(["key".into(), "row_json".into()].as_slice()) {
        return Err(anyhow!("invalid CSV row header in {}", path.display()));
    }
    records
        .into_iter()
        .skip(1)
        .map(|record| {
            let [key, row] = record
                .try_into()
                .map_err(|_: Vec<String>| anyhow!("invalid CSV row in {}", path.display()))?;
            Ok((key, serde_json::from_str(&row)?))
        })
        .collect()
}

fn escape_csv(value: &str) -> String {
    if value.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", value.replace('"', "\"\""))
    } else {
        value.to_owned()
    }
}

fn parse_csv(input: &str) -> Result<Vec<Vec<String>>> {
    let mut records = Vec::new();
    let mut record = Vec::new();
    let mut field = String::new();
    let mut quoted = false;
    let mut chars = input.chars().peekable();
    while let Some(character) = chars.next() {
        if quoted {
            if character == '"' {
                if chars.next_if_eq(&'"').is_some() {
                    field.push('"');
                } else {
                    quoted = false;
                }
            } else {
                field.push(character);
            }
            continue;
        }
        match character {
            '"' if field.is_empty() => quoted = true,
            ',' => {
                record.push(std::mem::take(&mut field));
            }
            '\n' => {
                record.push(std::mem::take(&mut field));
                records.push(std::mem::take(&mut record));
            }
            '\r' => {}
            value => field.push(value),
        }
    }
    if quoted {
        return Err(anyhow!("unterminated quoted CSV field"));
    }
    if !field.is_empty() || !record.is_empty() {
        record.push(field);
        records.push(record);
    }
    Ok(records)
}

fn row_file_name(database: &str, table: &str) -> String {
    format!("{}--{}.csv", hex_name(database), hex_name(table))
}

fn hex_name(value: &str) -> String {
    value
        .as_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
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

fn apply_batch(state: &mut CsvState, batch: StorageBatch) {
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

#[tokio::main]
async fn main() -> Result<()> {
    let root = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("target/csv-storage-example"));
    let storage = CsvStorage::open(&root)?;
    let database = AsyncEngine::open(EngineConfig::default(), storage).await?;

    database
        .execute_sql(
            "CREATE TABLE IF NOT EXISTS tasks (id INT PRIMARY KEY, title TEXT NOT NULL, done BOOLEAN NOT NULL DEFAULT false)",
        )
        .await?;
    database
        .execute_sql(
            "INSERT INTO tasks VALUES (1, 'write CSV backend', false), (2, 'remove stale task', false) ON DUPLICATE KEY UPDATE title = VALUES(title), done = VALUES(done)",
        )
        .await?;
    database
        .execute_sql("UPDATE tasks SET done = true WHERE id = 1")
        .await?;
    database
        .execute_sql("DELETE FROM tasks WHERE id = 2")
        .await?;
    drop(database);

    let reopened = AsyncEngine::open(EngineConfig::default(), CsvStorage::open(&root)?).await?;
    let result = reopened
        .execute_sql("SELECT id, title, done FROM tasks ORDER BY id")
        .await?;
    println!("persisted CSV directory: {}", root.display());
    println!(
        "rows after reopen: {}",
        serde_json::to_string_pretty(&result[0].rows)?
    );
    Ok(())
}
