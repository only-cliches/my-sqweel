# Persistence, snapshots, and maintenance

## Persistent engine

Use `Engine::open` with `Storage::RocksDb(path)` for durable local development state. MySqweel
opens a RocksDB store in that directory; RocksDB prevents a second process
from opening the same directory concurrently.

```rust,no_run
use my_sqweel::sql::engine::{Engine, EngineConfig};

let engine = Engine::open(EngineConfig::default(), my_sqweel::Storage::RocksDb(".my-sqweel".into()))?;
# Ok::<(), anyhow::Error>(())
```

This is local-development persistence. It is not a replacement for a
production MariaDB durability, availability, or replication setup.

## Snapshots

`snapshot` and `restore_snapshot` work on the default `app` database. A
`Snapshot` is serializable and includes table schemas, stored rows,
auto-increment counters, views, and index comments.

With RocksDB, `engine.try_snapshot()` reports storage read errors; the
`snapshot()` convenience method expects the read to succeed.

```rust,no_run
use my_sqweel::sql::engine::Engine;

let engine = Engine::default();
let snapshot = engine.snapshot();
let encoded = serde_json::to_vec_pretty(&snapshot)?;
std::fs::write("app-snapshot.json", encoded)?;

let restored = serde_json::from_slice(&std::fs::read("app-snapshot.json")?)?;
engine.restore_snapshot(restored)?;
# Ok::<(), anyhow::Error>(())
```

For an async custom backend, persist the catalog/table/row mutations in each
`StorageBatch`, not only an app `Snapshot`. This retains account and
multi-database state without requiring a whole-instance image. See
[custom async storage](async-storage.md).

## CSV and JSON imports

`Engine::import_csv` reads a header row; `Engine::import_json` accepts an object,
an array of objects, or newline-delimited objects. Both accept `std::io::Read`
inputs and return an `ImportReport` with read, imported, and skipped row counts.
By default, an import creates a missing table and infers `BOOLEAN`, `BIGINT`,
`DOUBLE`, `DATE`, `DATETIME`, `JSON`, or `TEXT` columns from the included rows.
A non-null `id` column becomes a primary key when its values fit an integer or
`VARCHAR(255)`. Empty CSV fields become `NULL`.

Use `ImportOptions::columns` to define the new table explicitly, or set
`ImportTable::Existing` to require a table that is already present. For example:

```rust,no_run
use my_sqweel::{Engine, ImportColumn, ImportMode, ImportOptions, ImportTable};

let engine = Engine::default();
let columns = vec![
    ImportColumn::new("id", "BIGINT").primary_key(),
    ImportColumn::new("name", "VARCHAR(80)"),
];
engine.import_csv(
    "people",
    "id,name\n1,Ada\n".as_bytes(),
    ImportOptions { columns: Some(columns), ..ImportOptions::default() },
)?;
engine.import_json(
    "people",
    r#"{"id":1,"name":"Ada Lovelace"}"#.as_bytes(),
    ImportOptions {
        table: ImportTable::Existing,
        mode: ImportMode::UpsertMerge,
        ..ImportOptions::default()
    },
)?;
# Ok::<(), anyhow::Error>(())
```

`UpsertMerge` changes only supplied columns. `UpsertReplace` replaces the entire
row, applying defaults to omitted columns. Both use SQL primary or unique key
conflicts. An optional `visitor` receives each row and its one-based source row
number before inference; it can edit the row or return `ImportDecision::Skip`.

Rows are grouped into SQL batches and committed in one transaction. If a write
fails, all imported rows roll back. A table created for the import remains empty
because table creation runs before the row transaction. Inference examines the
entire input, so an import temporarily uses memory proportional to its input.

## JSON ingestion and reset

Use `upsert_json_documents` or `seed_json_rows` to load JSON objects directly.
`SeedMode` selects append or replace behavior; `upsert_json_documents` accepts
a `merge` boolean for document-level merge behavior. The `reset_all_rows` and
`reset_table_rows` methods retain schemas while removing rows.

```rust,no_run
use my_sqweel::sql::engine::{Engine, SeedMode};
use serde_json::json;

let engine = Engine::default();
engine.execute_sql("CREATE TABLE users (id INT PRIMARY KEY, name TEXT)")?;
engine.seed_json_rows(
    "users",
    vec![json!({"id": 1, "name": "Ada"}).as_object().unwrap().clone()],
    SeedMode::Replace,
)?;
# Ok::<(), anyhow::Error>(())
```

## Drift and indexes

`engine.drift_report()` returns JSON describing schema differences in restored
snapshots or state changed through low-level storage APIs. Use `rebuild_indexes_for_table` or
`rebuild_indexes_for_all_tables` after deliberate low-level fixture changes.
`swap_tables` atomically exchanges named tables from the engine's perspective,
which is useful for local rebuild workflows.

The CLI REPL and debug HTTP server expose corresponding workflows. The root
[README](../README.md#local-data-workflows) introduces the CLI workflow;
[search and debug HTTP](search.md#drift-and-snapshot-operations) lists the HTTP routes.

## Failure injection

`EngineConfig::failure_injection` provides development-only query delay and
periodic read/write failures. Use it to exercise retry and error paths; do not
enable it for normal application operation.
