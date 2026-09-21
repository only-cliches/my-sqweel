# Async storage and custom backends

`AsyncEngine<S>` has one active storage backend. `S` implements the native
async `AsyncStorage` trait. The crate supplies `LuxStorage`; applications can
implement the trait for a CSV collection, an HTTP service, an object store, or
another database.

The interface is table-oriented. A backend is never asked to produce a whole
`EngineState`, a complete table, or a serialized engine snapshot.

## Use the bundled Lux backend

```rust,no_run
use my_sqweel::AsyncEngine;
use my_sqweel::sql::engine::EngineConfig;
use my_sqweel::storage::LuxStorage;

async fn open() -> anyhow::Result<AsyncEngine<LuxStorage>> {
    let storage = LuxStorage::open(Some(".my-sqweel".into())).await?;
    AsyncEngine::open(EngineConfig::default(), storage).await
}
```

Pass `None` to `LuxStorage::open` for an in-memory backend. `AsyncEngine::open_lux`
combines these two calls when Lux is the selected backend.

## Storage contract

```rust
pub trait AsyncStorage: Send + Sync + 'static {
    async fn load_catalog(&self) -> anyhow::Result<Option<StorageCatalog>>;
    async fn list_tables(&self, database: &str) -> anyhow::Result<Vec<String>>;
    async fn load_table(&self, database: &str, table: &str)
        -> anyhow::Result<Option<TableState>>;
    async fn scan_rows(&self, scan: RowScan) -> anyhow::Result<RowPage>;
    async fn commit(&self, batch: StorageBatch) -> anyhow::Result<()>;
}
```

`StorageCatalog` contains opaque MySqweel account/catalog metadata and
per-database view/index metadata. Preserve the opaque `metadata` value without
interpreting or dropping fields.

`TableState` contains one table's schema and auto-increment counter. Rows are
separate. `scan_rows` receives a table, an opaque continuation cursor, and a
maximum page size. Return `next_cursor` only when another page exists; callers
pass it unchanged in the next `RowScan`. A cursor may be a primary key, an
offset, or a backend-provided token.

`commit` receives only changes: catalog/table upserts or deletes, plus row
upserts and deletes. It must apply the metadata and rows in one atomic backend
operation. A database transaction, conditional HTTP write, or write-new/fsync/
rename file sequence are all suitable implementations.

## Minimal custom backend

This example keeps each table separately. It demonstrates that a backend owns
small metadata records and bounded row pages, rather than a database image.

```rust
use std::collections::BTreeMap;
use std::sync::Arc;

use my_sqweel::storage::{
    AsyncStorage, RowPage, RowScan, StorageBatch, StorageCatalog, TableState,
};

#[derive(Clone, Default)]
struct ApplicationStore {
    catalog: Arc<tokio::sync::RwLock<Option<StorageCatalog>>>,
    tables: Arc<tokio::sync::RwLock<BTreeMap<(String, String), TableState>>>,
}

impl AsyncStorage for ApplicationStore {
    async fn load_catalog(&self) -> anyhow::Result<Option<StorageCatalog>> {
        Ok(self.catalog.read().await.clone())
    }

    async fn list_tables(&self, database: &str) -> anyhow::Result<Vec<String>> {
        Ok(self.tables.read().await.keys()
            .filter(|(db, _)| db == database)
            .map(|(_, table)| table.clone())
            .collect())
    }

    async fn load_table(&self, database: &str, table: &str) -> anyhow::Result<Option<TableState>> {
        Ok(self.tables.read().await.get(&(database.into(), table.into())).cloned())
    }

    async fn scan_rows(&self, scan: RowScan) -> anyhow::Result<RowPage> {
        // Fetch one bounded page from CSV, HTTP, or an application database.
        let _ = scan;
        Ok(RowPage { rows: BTreeMap::new(), next_cursor: None })
    }

    async fn commit(&self, batch: StorageBatch) -> anyhow::Result<()> {
        // Apply batch.metadata and batch.rows atomically in the real backend.
        let _ = batch;
        Ok(())
    }
}
```

## Complete JSON and CSV examples

The repository includes two runnable file-backed implementations:

- [`examples/json_storage.rs`](../examples/json_storage.rs) stores catalog,
  schemas, and rows in one JSON file, replacing that file only after every
  `StorageBatch` has been applied.
- [`examples/csv_storage.rs`](../examples/csv_storage.rs) stores each table's
  rows as CSV. It writes a new generation and atomically switches a `CURRENT`
  pointer, so readers see either the old generation or the complete new one.

Both examples create a table, insert or save rows, update a row, delete a row,
and reopen the backend before reading the persisted result:

```sh
cargo run --example json_storage -- target/tasks.json
cargo run --example csv_storage -- target/tasks-csv
```

The CSV example uses `key,row_json` columns. `row_json` preserves the complete
`StoredRow` metadata and JSON values while the primary-key storage key stays
separate for cursor paging. For a human-oriented CSV format, map your own
columns to `StoredRow::data` and retain the metadata in a sidecar manifest.
These small file examples keep a current generation in memory while applying a
batch. For large CSV collections, retain the same manifest/generation design
but stream the affected table during `commit` and read only the requested page
in `scan_rows`.

## CSV and HTTP guidance

For CSV, keep a small catalog/schema manifest and one file per table. Use the
row cursor as the last stable primary-key value; write changed rows to a new
file and atomically replace it during `commit`.

For HTTP, map `load_table` to a table-metadata endpoint and `scan_rows` to a
cursor API such as `GET /tables/{table}/rows?cursor=…&limit=…`. Send the entire
`StorageBatch` to one idempotent mutation endpoint with an expected backend
version. Do not issue one remote request per row mutation unless the remote
system supplies a transaction that groups them.

## Current execution boundary

The storage API is granular today. The compatibility SQL evaluator currently
hydrates every configured table page into its in-process execution image when
an async engine opens. A backend never has to materialize a table for the
engine, but the evaluator itself is not yet streaming. A future executor can
consume `RowPage` directly so large scans stay bounded in the engine too.

## Async sessions and transactions

`AsyncEngine::session()` creates an `AsyncEngineSession`. Its SQL methods are
`async` and retain session state. At `COMMIT`, the resulting row/schema changes
are emitted as one `StorageBatch`; a backend should make that batch atomic.

See [execution filters](filters.md) for query routing and policy hooks.
