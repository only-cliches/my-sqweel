# Persistence, snapshots, and maintenance

## Directory-backed synchronous engine

Use `Engine::open_with_data_dir` for durable local development state. MySqweel
opens a locked embedded Lux store in that directory; a second process cannot
open the same directory at the same time.

```rust,no_run
use my_sqweel::sql::engine::{Engine, EngineConfig};

let engine = Engine::open_with_data_dir(EngineConfig::default(), Some(".my-sqweel"))?;
# Ok::<(), anyhow::Error>(())
```

This is local-development persistence. It is not a replacement for a
production MariaDB durability, availability, or replication setup.

## Snapshots

`snapshot` and `restore_snapshot` work on the default `app` database. A
`Snapshot` is serializable and includes table schemas, stored rows,
auto-increment counters, views, and index comments.

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
[README](../README.md#local-data-workflows) lists their commands and endpoints.

## Failure injection

`EngineConfig::failure_injection` provides development-only query delay and
periodic read/write failures. Use it to exercise retry and error paths; do not
enable it for normal application operation.
