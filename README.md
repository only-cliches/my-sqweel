<p align="center">
  <img src="logo.png" alt="MySqweel logo" width="280">
</p>

# MySqweel

<p align="center">
  <strong>Embeddable SQL for Rust applications, integration tests, and local development.</strong>
</p>

<p align="center">
  <a href="https://github.com/only-cliches/my-sqweel/actions/workflows/ci.yml"><img src="https://github.com/only-cliches/my-sqweel/actions/workflows/ci.yml/badge.svg" alt="CI status"></a>
</p>

MySqweel implements a subset of MySQL/MariaDB SQL in Rust. Run it in process,
connect existing clients through its MySQL wire server, or supply custom
storage through the async embedded API. It includes strict SQL schemas,
sessions and transactions, optional Lux persistence, and local maintenance
and search tools.

The project is intended for embedded development datasets, fixtures, ORM and
migration testing, and application integration. It does not provide full
MariaDB compatibility or production database durability and concurrency.

[Library guide](docs/README.md) · [Custom storage](docs/async-storage.md) ·
[Filters](docs/filters.md) · [Authentication](docs/server.md#wire-authentication-and-scopes) ·
[Compatibility](#mariadb-compatibility)

## Choose an API

| Entry point | What it provides | Current boundary |
| --- | --- | --- |
| `Engine` / `EngineSession` | Synchronous embedded SQL, sessions, transactions, snapshots, and query events | In-memory execution; optional directory-backed Lux persistence |
| `AsyncEngine<S>` / `AsyncEngineSession<S>` | Async storage integration and query/result filters | One `AsyncStorage` backend per instance; SQL evaluation is still synchronous |
| `server::run` / `spawn_with_engine` | MySQL wire connections plus debug/search HTTP | Uses `Arc<Engine>`; does not invoke async execution filters or custom async storage |
| `sqwl` | Server, SQL/maintenance REPL, and SQL inspection | Wraps the synchronous engine |

Lux is the only bundled storage backend. JSON and CSV implementations are
provided as [examples](#json-and-csv-examples). Multiple logical SQL databases
are supported, but a registry of multiple storage backends and filter-driven
backend routing are **not implemented**.

## Embed SQL in Rust

From an application alongside a checkout, add:

```toml
[dependencies]
my-sqweel = { path = "../my-sqweel" }
anyhow = "1"
serde_json = "1"
```

An embedded engine starts no network listeners:

```rust
use my_sqweel::sql::engine::Engine;
use serde_json::json;

fn main() -> anyhow::Result<()> {
    let engine = Engine::default();
    let mut db = engine.session();

    db.execute_sql(
        "CREATE TABLE tasks (id INT PRIMARY KEY, title TEXT NOT NULL, done BOOLEAN DEFAULT false)"
    )?;
    db.execute_sql_with_params(
        "INSERT INTO tasks (id, title) VALUES (?, ?)",
        &[json!(1), json!("Write a storage backend")],
    )?;
    db.execute_sql("UPDATE tasks SET done = true WHERE id = 1")?;

    let results = db.execute_sql("SELECT id, title, done FROM tasks ORDER BY id")?;
    assert_eq!(results[0].rows[0]["done"], json!(true));

    db.execute_sql("DELETE FROM tasks WHERE id = 1")?;
    Ok(())
}
```

Create one session per independent caller. Direct calls on `Engine` share its
default session. Query methods return a `Vec<QueryResult>`, with one entry per
SQL statement. Each result includes rows as JSON objects, column metadata,
affected-row counts, the last insert ID, and warnings.

SQL schemas are always strict. Declare tables and columns before writing;
duplicate keys, undeclared columns, and invalid values produce errors.
`EngineConfig::mysql_strict()` remains an alias for `EngineConfig::default()`.
The former `CompatibilityProfile`, `UniqueMode`, configuration fields, and
`--mysql-strict` / `--unique-mode` flags have been removed.

### Sessions and transactions

Sessions support `BEGIN` / `START TRANSACTION`, `COMMIT`, `ROLLBACK`,
savepoints, and `SET autocommit`. A failed statement rolls back its own
changes while preserving earlier successful work in the transaction.
Dropping a session discards its uncommitted writes.

The implemented isolation level is `REPEATABLE READ`. A read snapshot begins
at the first read. Writers serialize per logical database, and a conflicting
snapshot-to-writer upgrade can return retryable MySQL error 1213. Writer
acquisition has a five-second timeout. Plain `SELECT ... FOR UPDATE` takes
the database-wide writer lease.

DDL and catalog administration are rejected inside active transactions.
Cross-database SQL is unsupported, and a session cannot switch databases
during a transaction. Connection-owned `GET_LOCK()` and `RELEASE_LOCK()`
advisory locks survive transaction completion and are released on disconnect.

See [embedding and transactions](docs/embedding.md) for more examples.

## Async storage and filters

Add `tokio = { version = "1", features = ["full"] }` to the application
dependencies to run the async examples.

```rust
use my_sqweel::{AsyncEngine, QueryFilter, QueryFilterAction, QueryRequest};
use my_sqweel::sql::engine::EngineConfig;

struct CurrentTenant;

impl QueryFilter for CurrentTenant {
    async fn filter(
        &self,
        request: &mut QueryRequest,
    ) -> anyhow::Result<QueryFilterAction> {
        if request.sql == "SELECT current_tenant" {
            request.sql = "SELECT 'acme' AS tenant".into();
        }
        Ok(QueryFilterAction::Continue)
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let db = AsyncEngine::open_lux(EngineConfig::default(), None).await?;
    db.query_filters().push(CurrentTenant);

    let results = db.execute_sql("SELECT current_tenant").await?;
    assert_eq!(results[0].rows[0]["tenant"], "acme");
    Ok(())
}
```

`QueryFilter` and `ResultFilter` are native async traits. Their mutable,
ordered pipelines can be changed using `push` and `clear`.

| Hook | Available actions |
| --- | --- |
| Query filter | Modify SQL and continue, reject execution, or return synthetic results |
| Result filter | Modify results, replace them, or reject delivery to the caller |

Parameters are bound before query filters run. Synthetic results also pass
through result filters. Result filtering occurs after SQL execution;
rejecting a result does **not** undo the statement's writes. These hooks
apply to `AsyncEngine` and its sessions. See [execution filters](docs/filters.md).

### Implement a custom backend

Implement `my_sqweel::storage::AsyncStorage`, then pass your implementation to
`AsyncEngine::open(config, storage).await`.

| Method | Responsibility |
| --- | --- |
| `load_catalog` | Load database/account metadata, or return `None` for new storage |
| `list_tables` | List table names within a logical database |
| `load_table` | Load a table schema and its auto-increment counter |
| `scan_rows` | Return a bounded `RowPage` and optional continuation cursor |
| `commit` | Apply a `StorageBatch` of metadata and row puts/deletes |

Preserve the opaque catalog metadata. A backend must apply each batch
atomically, with metadata mutations before row mutations. Explicit SQL
transactions emit their committed changes together at `COMMIT`.

The API accepts pages and incremental mutations, but the current executor
loads every table's rows into memory when the async engine opens. It also
exports before/after state images to calculate changes. It does not stream
SQL scans from the backend or push predicates into CSV/HTTP services.
Execution is serialized through the async engine's commit gate.

A backend commit failure currently occurs after the in-memory SQL state has
changed; the async wrapper does not restore that state automatically. Backend
atomicity and recovery therefore need explicit handling in an application.
See [custom async storage](docs/async-storage.md) for the full contract.

### JSON and CSV examples

```sh
cargo run --example json_storage -- target/tasks.json
cargo run --example csv_storage -- target/tasks-csv
```

Both examples create a strict table, insert/upsert rows, update a row, delete
a row, and reopen storage to read the saved result.

- [JSON](examples/json_storage.rs) stores catalog, schemas, and rows in one
  JSON file, replacing it after applying a batch.
- [CSV](examples/csv_storage.rs) writes per-table `key,row_json` records and a
  JSON metadata manifest. Each commit creates a generation and replaces a
  `CURRENT` pointer. The JSON field preserves the complete `StoredRow`,
  including typed values and row metadata.

These are small, application-owned reference implementations. They load all
data into memory and rewrite the full file or generation on a commit.
Their filesystem work is blocking, they do not coordinate independent
processes or call `fsync`, and the CSV example retains older generations.
The CSV format is a storage format; importing arbitrary existing CSV columns
requires an application-specific mapping.

## Run the server

From a checkout, using a recent stable Rust toolchain:

```sh
cargo install --path .
sqwl serve
```

Or run directly with `cargo run --bin sqwl -- serve`.

| Setting | Default |
| --- | --- |
| MySQL wire listener | `127.0.0.1:3307` |
| Debug/search HTTP listener | `127.0.0.1:3407` |
| Selected database | `app` |
| Storage | In memory |
| Wire authentication | `Authentication::AllowAll` |

Connect using a MySQL/MariaDB client:

```sh
mariadb --protocol=TCP -h 127.0.0.1 -P 3307 -u root app
```

The wire server supports text queries, prepared statements, positional
parameters, typed results, session settings, and transaction status in
responses. It advertises `8.0.0-my-sqweel` for compatibility; this is not a
claim of complete MySQL 8 support.

### Authentication and accounts

Authentication is configurable through the Rust server API:

| Policy | Behavior |
| --- | --- |
| `Authentication::AllowAll` | Default: accepts usernames/passwords with full scope |
| `Authentication::EngineAccounts` | Authenticates against the SQL account catalog |
| `Authentication::static_users(...)` | Uses configured usernames/passwords and scopes |
| `Authentication::callback(...)` | Delegates to an `AsyncAuthenticator` |

For example, run a server with a read-only user for `app`:

```rust,no_run
use my_sqweel::server::{self, Authentication, ServerConfig, StaticUser};
use my_sqweel::sql::engine::{AuthPrivilege, AuthScope};

fn main() -> anyhow::Result<()> {
    server::run(ServerConfig {
        authentication: Authentication::static_users([StaticUser::new(
            "reporter",
            "example-password",
            [AuthScope::database("app", [AuthPrivilege::Select])],
        )]),
        ..ServerConfig::default()
    })
}
```

Database scopes cover `Select`, `Insert`, `Update`, and `Delete`;
`AuthScope::All` includes administrative access. External authenticators
receive the username, requested database, and native-password
challenge/response, not a plaintext client password. They return an
`AuthenticatedUser` with scopes, or `None` to deny access.

An authenticator can optionally implement `account_operation` to handle
`CREATE USER`, `ALTER USER`, `DROP USER`, `RENAME USER`, `SET PASSWORD`,
`GRANT`, and `REVOKE`. The callback receives the operation kind and SQL.
`Handled` acknowledges an operation completed externally; `Continue`
uses the built-in catalog. These callbacks require an administrator and run
only with callback authentication on the wire server.

The built-in catalog supports `CREATE/DROP DATABASE`, `CREATE/DROP USER`,
database-wide DML grants, and `REVOKE ALL PRIVILEGES`, with `%` as the
supported user host. It does not implement the full account-management
syntax accepted by an external callback. Fresh engines bootstrap `root`
with an empty password. `set_admin_credentials` changes catalog credentials;
wire connections use them only when `EngineAccounts` is selected.

Use `server::spawn_with_engine` to serve an existing `Arc<Engine>`;
dropping its `ServerHandle` stops the listeners. The CLI has no
authentication-policy flags. See [server and authentication](docs/server.md).

SQL authentication and grants do not protect the debug/search HTTP routes.
Those routes provide unauthenticated administrative access to `app`.
Listeners default to loopback; `--allow-remote` permits non-loopback bindings.

## CLI reference

```text
sqwl [options] serve [--repl]
sqwl [options] repl
sqwl explain <sql>
sqwl help
```

Place global options before the subcommand.

| Option | Purpose |
| --- | --- |
| `--bind <addr>` | MySQL bind address; default `127.0.0.1:3307` |
| `--debug-bind <addr>` | HTTP bind address; default SQL address with port + 100 |
| `--data-dir <dir>` | Enable locked Lux persistence |
| `--default-time-zone <offset>` | Initial session timezone; default UTC |
| `--allow-remote` | Permit non-loopback listeners |
| `--query-delay-ms <n>` | Add latency per SQL statement |
| `--fail-read-every <n>` | Fail every Nth read statement |
| `--fail-write-every <n>` | Fail every Nth write statement |
| `--snapshot-dir <path>` | Named snapshot directory; default `.my-sqweel/snapshots` |
| `--log-filter <filter>` | Tracing filter; default `my_sqweel=info` |

`sqwl explain` reports parsed statement kinds, tables, and normalized SQL
without executing it; it is not an optimizer execution plan.

## Local-data workflows

### Persistent local state

```sh
sqwl --data-dir .my-sqweel/data serve --repl
```

Embedded callers use
`Engine::open_with_data_dir(EngineConfig::default(), Some(path))`.
The data directory is locked against concurrent opens. Lux persists logical
databases, accounts, schemas, rows, counters, views, and index comments.

The synchronous engine writes incremental changes before publishing the
new SQL state. Its version-2 format rejects legacy unversioned stores and
`transaction-image.json`; there is no automatic migration for old
development data. The async Lux adapter uses a separate storage layout;
its directory is not interchangeable with the synchronous engine's directory.

The current Lux adapter uses command pipelines, so persistence does not
guarantee crash-atomic multi-key commits or synchronous durability. The
synchronous engine stops accepting operations after a persistence error and
requires reopening; a partial storage write may remain. Writes can also copy
an affected table and compare its rows linearly.

### Maintenance, seeding, and snapshots

Run `sqwl repl` for a standalone shell or `sqwl serve --repl` to combine
the shell with network listeners.

```text
status
drift report
drift check
snapshot save before-edit
snapshot restore before-edit
snapshot list
index rebuild --all
reset tasks
sql SELECT * FROM tasks
quit
```

`seed_json_rows` and `POST /_drift/tables/{table}/seed` require declared
tables and columns. Append/replace seeding, row resets, index rebuilds,
table swaps, and snapshot restoration operate on the default `app` database.

A synchronous `Snapshot` covers `app`, including schemas, rows, counters,
views, and index comments. It excludes other databases and the account
catalog. `Engine::export_state()` / `import_state()` work with a complete
`EngineState`; `AsyncEngine::snapshot()` also returns that full image.

Drift reports remain useful for restored or externally modified rows and
for stored row shapes retained across schema changes. They do not enable
relaxed SQL schemas. See [persistence and maintenance](docs/operations.md).

### Query events

`Engine::subscribe_query_events(QueryEventOptions::metadata_only())` returns
a blocking stream of received/completed events. Completion events include
duration, result sizes, logical rows/cells read and written, and errors.
Result payloads are optional. Events describe execution, not durable commits;
completed statements can still be rolled back.

## MariaDB compatibility

Compatibility tests target **MariaDB 10.11.7**. The supported subset includes:

| Area | Implemented surface |
| --- | --- |
| Schema | Tables, temporary tables, views, common `ALTER TABLE` forms, defaults, generated columns, and column positioning |
| Constraints and indexes | Primary/unique/secondary/prefix indexes; foreign keys with cascade, set-null, restrict, and no-action behavior |
| Writes | Values/select inserts, `INSERT IGNORE`, `REPLACE`, upserts, updates, common joined writes/deletes, and `RETURNING` |
| Queries | Projections, filtering, grouping, aggregates, ordering, limits, inner/left/right/full outer joins, derived tables, scalar and `IN`/`EXISTS` subqueries |
| Set operations and CTEs | `UNION`, `INTERSECT`, `EXCEPT`, CTE aliases, and supported recursive `UNION` forms |
| Windows | Ranking, offset/value functions, aggregate windows, and supported `ROWS`/`RANGE` frames |
| Expressions | String, numeric, date/time, conversion, and JSON functions; JSON aggregation and basic `JSON_TABLE` |
| Introspection | Common `SHOW` commands and `information_schema` views used by clients and ORMs |
| Wire protocol | Prepared statements, typed column metadata/results, native-password authentication, warnings, and transaction status |

Support for a syntax family does not imply support for every MariaDB option
or edge case. Unsupported forms generally return errors, and known
compatibility gaps remain in the audit suites.

`FULL [OUTER] JOIN` is a MySqweel query extension and is omitted from the
MariaDB differential corpus.

Explicit limits include isolation levels other than `REPEATABLE READ`,
fine-grained row locking, XA, cross-database queries,
unrestricted recursive CTEs, stored routines, triggers, events, replication,
and the full MariaDB permissions system. Optimizer behavior, collations,
window semantics, and JSON functionality are not complete MariaDB replicas.

The repository has engine, wire, ORM, error-code, and differential suites.
Upstream MTR gates use [pinned complete-file manifests](tests/mariadb-mtr-allowlist.txt)
plus [additional admitted cases](tests/query_coverage_mtr/).
[Scope and qualification rules](tests/mariadb-mtr-exclusions.md) distinguish
strict gates, known failures, discovery candidates, and derived scenarios.
Passing percentages apply only to their recorded test scope.

The differential suite also generates reproducible, schema-aware stateful
programs across transactions, DML, joins, grouping, and subqueries. It runs
each generated program against both engines and checks both observations and
canonical table state after mutation. The default 64 seeds cover every current
mutation/query interaction; set `STATEFUL_DIFFERENTIAL_SEEDS` to increase the
deterministic range locally or in the nightly job. Set
`STATEFUL_DIFFERENTIAL_SEED` to replay one failing program. A failing seed
prints its complete SQL program for reduction into a committed regression
fixture.

See [compatibility limits](docs/compatibility.md), [CHANGELOG.md](CHANGELOG.md),
and the [query coverage tooling](tools/query_coverage/README.md) for details.

## Local search and HTTP administration

The HTTP listener exposes a Meilisearch-shaped API over tables in `app`.
Document ingestion can discover fields and performs explicit document
upserts; it is separate from strict SQL and JSON seed validation.
Document mutations rebuild the derived Tantivy search index.

The API includes document/index CRUD, text search, filters, sorting, facets,
multi-search, settings, stats, local task-shaped responses, dumps, webhooks,
and key stubs. Vector search uses cosine similarity. Authentication and API
keys are permissive/stubbed, and relevance and task behavior do not promise
production Meilisearch parity.

See [search examples](docs/search.md) for index creation, document ingestion,
and text/vector queries.

### HTTP endpoint map

| Endpoint | Purpose |
| --- | --- |
| `GET /health`, `GET /version` | Health and version |
| `GET /_drift/health`, `GET /_drift/report` | Drift API health and report |
| `GET /_drift/tables` | List tables |
| `GET /_drift/tables/{table}/rows` | Inspect stored rows |
| `POST /_drift/tables/{table}/seed` | Seed declared table rows |
| `POST /_drift/snapshot`, `POST /_drift/restore` | Export/restore an app snapshot |
| `GET/POST /indexes` | List/create search indexes |
| `GET/POST/PUT/PATCH/DELETE /indexes/{uid}/documents` | Document operations |
| `GET/POST /indexes/{uid}/search` | Search |
| `POST /indexes/{uid}/facet-search`, `POST /multi-search` | Facet and multi-index search |
| `GET /tasks`, `GET /stats` | Tasks and statistics |

Additional routes cover individual documents, settings, index statistics,
index swaps, dumps, and webhooks.

## Development

From the checkout:

```sh
cargo test --all-targets --locked
cargo check --examples --locked
cargo fmt --all --check
cargo clippy --all-targets --all-features
```

Differential tests can skip when MariaDB is unavailable. To require the
comparison server and verify packaging, run the same entry point used by
the CI feature job:

```sh
tools/prepush.sh
```

It uses `MARIADB_COMPARE_URL` when supplied; otherwise it provisions the
pinned Docker image. It runs tooling tests, all Cargo targets with
`MARIADB_PARITY_REQUIRED=1`, and `cargo package --locked --allow-dirty`.
Use a disposable comparison database. Enable the optional Git hook with
`git config core.hooksPath .githooks`.

For upstream MTR reproduction and discovery, start with
[upstream test tooling](tests/mariadb-mtr-exclusions.md) and
[the query coverage worker](tools/query_coverage/README.md). CI output and
generated reports establish results for each revision; historical counts
are not guarantees for the current checkout.

## Project layout

```text
src/sql/engine/          SQL evaluation, transactions, catalog, and authorization
src/async_engine.rs      Async wrapper, storage coordination, and execution filters
src/storage/            Lux integration and public AsyncStorage contract
src/server/             MySQL wire protocol, authentication, debug/search HTTP
src/lib.rs              Public exports, CLI, and REPL
src/bin/sqwl.rs          Executable entry point
src/vendor/             Vendored Lux and wire-server implementations
examples/               JSON and CSV custom storage examples
docs/                   Library guides
tests/                  Engine, wire, ORM, and compatibility suites
tools/                  CI checks, MariaDB comparison, and coverage tooling
```

## License

[MIT](LICENSE)
