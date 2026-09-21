# Embedding and transactions

`Engine` is the synchronous in-process API. It starts no network listeners and
keeps data in memory unless opened with a data directory.

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
`Engine::execute_sql` share its default session; use a session when a caller
needs transaction, prepared-statement, or session-variable state.

```rust
use my_sqweel::sql::engine::Engine;

fn transfer() -> anyhow::Result<()> {
    let engine = Engine::default();
    let mut db = engine.session();
    db.execute_sql("CREATE TABLE balances (id INT PRIMARY KEY, amount INT)")?;
    db.execute_sql("INSERT INTO balances VALUES (1, 100), (2, 0)")?;

    db.execute_sql("START TRANSACTION")?;
    db.execute_sql("UPDATE balances SET amount = amount - 25 WHERE id = 1")?;
    db.execute_sql("SAVEPOINT credited")?;
    db.execute_sql("UPDATE balances SET amount = amount + 25 WHERE id = 2")?;
    db.execute_sql("COMMIT")?;
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

