# Embedding and transactions

`Engine` supports synchronous and asynchronous in-process queries. It starts
no network listeners and uses the storage backend selected when it opens.

```rust
use my_sqweel::sql::engine::Engine;

fn run() -> anyhow::Result<()> {
    let engine = Engine::default();
    let mut session = engine.session();

    session.execute_sql("CREATE TABLE users (id INT PRIMARY KEY, name TEXT)")?;
    session.execute_sql("INSERT INTO users VALUES (1, 'Ada')")?;
    let result = session.execute_sql("SELECT id, name FROM users")?;

    assert_eq!(result[0].rows[0]["name"], "Ada");
    Ok(())
}
```

## Sessions and transactions

Create one session for each independent connection or caller. Direct calls on
`Engine::execute_sql` create a fresh session for each call; use an explicit session when a caller
needs transaction, prepared-statement, or session-variable state.

```rust
use my_sqweel::sql::engine::Engine;

fn transfer() -> anyhow::Result<()> {
    let engine = Engine::default();
    let mut db = engine.session();
    db.execute_sql("CREATE TABLE balances (id INT PRIMARY KEY, amount INT)")?;
    db.execute_sql("INSERT INTO balances VALUES (1, 100), (2, 0)")?;

    let mut session = engine.session();
    session.execute_sql("START TRANSACTION")?;
    session.execute_sql("UPDATE balances SET amount = amount - 25 WHERE id = 1")?;
    session.execute_sql("SAVEPOINT credited")?;
    session.execute_sql("UPDATE balances SET amount = amount + 25 WHERE id = 2")?;
    session.execute_sql("COMMIT")?;
    Ok(())
}
```

`ROLLBACK`, `ROLLBACK TO SAVEPOINT`, `RELEASE SAVEPOINT`, and `SET autocommit`
are supported. A dropped session rolls back its uncommitted work.

The supported isolation level is `REPEATABLE READ`. MySqweel serializes one
writer per logical database and can return MySQL error 1213 when a reader is
upgraded to a conflicting writer. Retry the transaction in that case.

## Parameters and results

Use `execute_sql_with_params` when values come from application input. Values
are `serde_json::Value` because result rows and document helpers use the same
representation.

```rust
use my_sqweel::sql::engine::Engine;
use serde_json::json;

fn insert_user() -> anyhow::Result<()> {
    let engine = Engine::default();
    engine.execute_sql("CREATE TABLE users (id INT PRIMARY KEY, name TEXT)")?;
    engine.execute_sql_with_params(
        "INSERT INTO users VALUES (?, ?)",
        &[json!(1), json!("Ada")],
    )?;
    Ok(())
}
```

Each `QueryResult` contains `rows_affected`, `last_insert_id`, column names and
metadata, JSON-object rows, and MySQL-style warnings. One API call can return
multiple results for a multi-statement SQL string.

## Custom functions

Register host functions before opening the engine. Callbacks receive and return
`serde_json::Value`; their declared SQL types control argument conversion and
result metadata. A deterministic, synchronous scalar can be used in stored
schema expressions.

```rust
use my_sqweel::{Engine, EngineConfig, FunctionSignature, MysqlColumnType, Storage};
use serde_json::json;

let mut builder = Engine::builder(EngineConfig::default(), Storage::Memory);
builder.functions().register_scalar(
    "twice",
    FunctionSignature {
        args: vec![MysqlColumnType::Integer],
        returns: MysqlColumnType::Integer,
        deterministic: true,
    },
    |args| Ok(json!(args[0].as_i64().unwrap() * 2)),
)?;
let engine = builder.open()?;
engine.execute_sql("SELECT twice(5) AS value")?;
# Ok::<(), anyhow::Error>(())
```

`register_async_scalar`, `register_aggregate`, and
`register_async_aggregate` cover asynchronous calls and grouped stateful
calculations. Async callbacks have a 30 second timeout by default; the
`*_with_timeout` registrations accept a `Duration`. Synchronous callbacks
cannot be interrupted by a timeout. Callbacks cannot call back into the same
engine. Host registrations are shared by all SQL sessions and must be supplied
again when reopening persistent storage.

SQL definitions persist with the database catalog:

```sql
CREATE FUNCTION add_two(x INT) RETURNS INT RETURN x + 2;
CREATE AGGREGATE FUNCTION mean_square(x DOUBLE) RETURNS DOUBLE RETURN SUM(x*x) / COUNT(x);
DROP FUNCTION add_two;
DROP AGGREGATE FUNCTION mean_square;
```

SQL aggregate bodies compose built-in aggregates. Definitions are scoped to
the current database and require its `CREATE` or `DROP` privilege. Custom
aggregate `DISTINCT`, ordering, and window modifiers are unsupported. Stored
schema expressions can call only deterministic synchronous scalar functions;
opening storage fails if a required host callback is missing.

## Databases, accounts, and events

The provisioning SQL subset supports logical databases, users, and
database-wide DML grants. Bootstrap credentials can be replaced before clients
connect with `engine.set_admin_credentials(username, password)`.

Subscribe to query lifecycle events when an application needs execution
metrics. These metrics describe logical rows/cells read and written; they are
not storage I/O or durable commit notifications.

```rust
use my_sqweel::sql::engine::{Engine, QueryEvent, QueryEventOptions};

let engine = Engine::default();
let events = engine.subscribe_query_events(QueryEventOptions::metadata_only());
engine.execute_sql("SELECT 1")?;
let _received = events.recv()?;
if let QueryEvent::Completed(completed) = events.recv()? {
    println!("rows read: {}", completed.metrics.rows_read);
}
# Ok::<(), anyhow::Error>(())
```

Continue with [persistence and maintenance](operations.md) for snapshots,
drift reporting, seeding, and durable local storage.

