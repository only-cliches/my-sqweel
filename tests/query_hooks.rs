use my_sqweel::{
    QueryHookError, QueryHookEvent as Event, QueryHookKey as Key, QueryHookOptions,
    QueryHookSubscription, sql::engine::Engine,
};
use serde_json::json;
use std::{sync::Arc, time::Duration};
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

fn all() -> QueryHookOptions {
    QueryHookOptions {
        read: true,
        table_created: true,
        table_updated: true,
        table_dropped: true,
        ..Default::default()
    }
}
fn listen(
    engine: &Engine,
    options: QueryHookOptions,
) -> (QueryHookSubscription, UnboundedReceiver<Arc<Event>>) {
    let (tx, rx) = unbounded_channel();
    let handle = engine
        .subscribe_query_hooks(options, move |event| {
            tx.send(event).unwrap();
            async { Ok(()) }
        })
        .unwrap();
    (handle, rx)
}
async fn next(rx: &mut UnboundedReceiver<Arc<Event>>) -> Arc<Event> {
    tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .unwrap()
        .unwrap()
}
async fn empty(rx: &mut UnboundedReceiver<Arc<Event>>) {
    // Let already-enqueued events reach the callback before asserting silence.
    tokio::task::yield_now().await;
    assert!(rx.try_recv().is_err());
}
fn primary(id: i64) -> Key {
    Key::Primary(json!({"id":id}).as_object().unwrap().clone())
}

#[tokio::test]
async fn transactions_emit_final_rows_and_keys_only_after_commit() {
    let engine = Engine::default();
    engine
        .execute_sql("CREATE TABLE items (id INT PRIMARY KEY, value INT DEFAULT 9)")
        .unwrap();
    let (_handle, mut rx) = listen(&engine, QueryHookOptions::default());
    let mut session = engine.session();
    session.execute_sql("BEGIN; INSERT INTO items (id) VALUES (1), (2); UPDATE items SET value = 10 WHERE id = 1; SAVEPOINT keep; UPDATE items SET value = 11 WHERE id = 1; ROLLBACK TO keep; DELETE FROM items WHERE id = 2").unwrap();
    empty(&mut rx).await;
    session.execute_sql("COMMIT").unwrap();
    match &*next(&mut rx).await {
        Event::Write { rows, .. } => {
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].key, primary(1));
            assert_eq!(
                rows[0].row,
                json!({"id":1,"value":10}).as_object().unwrap().clone()
            );
        }
        event => panic!("{event:?}"),
    }
    session.execute_sql("BEGIN; UPDATE items SET value=12; UPDATE items SET value=10; COMMIT; BEGIN; DELETE FROM items; ROLLBACK; UPDATE items SET value=10").unwrap();
    empty(&mut rx).await;
    session.execute_sql("UPDATE items SET id=3").unwrap();
    assert!(
        matches!(&*next(&mut rx).await, Event::Delete { keys, .. } if keys == &vec![primary(1)])
    );
    assert!(
        matches!(&*next(&mut rx).await, Event::Write { rows, .. } if rows[0].key == primary(3))
    );
    assert!(
        session
            .execute_sql("INSERT INTO items VALUES (4, 8); INSERT INTO missing VALUES (1)")
            .is_err()
    );
    assert!(
        matches!(&*next(&mut rx).await, Event::Write { rows, .. } if rows[0].key == primary(4))
    );
    empty(&mut rx).await;
}

#[tokio::test]
async fn schema_events_order_ddl_rows_and_exclude_temporary_and_restore() {
    let engine = Engine::default();
    let (_handle, mut rx) = listen(
        &engine,
        QueryHookOptions {
            read: false,
            ..all()
        },
    );
    engine
        .execute_sql("CREATE TABLE source (id INT PRIMARY KEY); INSERT INTO source VALUES (1)")
        .unwrap();
    assert!(
        matches!(&*next(&mut rx).await, Event::TableCreated { schema, .. } if schema.primary_key == ["id"])
    );
    next(&mut rx).await;
    engine
        .execute_sql("CREATE TABLE copied AS SELECT * FROM source")
        .unwrap();
    assert!(
        matches!(&*next(&mut rx).await, Event::TableCreated { table, .. } if table == "copied")
    );
    assert!(matches!(&*next(&mut rx).await, Event::Write { table, .. } if table == "copied"));
    engine
        .execute_sql("ALTER TABLE source ADD COLUMN label INT DEFAULT 7")
        .unwrap();
    assert!(
        matches!(&*next(&mut rx).await, Event::TableUpdated { schema, .. } if schema.columns.contains_key("label"))
    );
    assert!(
        matches!(&*next(&mut rx).await, Event::Write { rows, .. } if rows[0].row["label"] == json!(7))
    );
    engine
        .execute_sql("CREATE INDEX labels ON source (label)")
        .unwrap();
    assert!(
        matches!(&*next(&mut rx).await, Event::TableUpdated { schema, .. } if schema.indexes.iter().any(|index| index.name == "labels"))
    );
    engine
        .execute_sql("ALTER TABLE source DROP COLUMN label")
        .unwrap();
    assert!(
        matches!(&*next(&mut rx).await, Event::TableUpdated { schema, .. } if !schema.columns.contains_key("label"))
    );
    assert!(
        matches!(&*next(&mut rx).await, Event::Write { rows, .. } if rows[0].row == json!({"id":1}).as_object().unwrap().clone())
    );
    engine
        .execute_sql("RENAME TABLE source TO renamed")
        .unwrap();
    assert!(matches!(&*next(&mut rx).await, Event::Delete { table, .. } if table == "source"));
    assert!(
        matches!(&*next(&mut rx).await, Event::TableDropped { table, .. } if table == "source")
    );
    assert!(
        matches!(&*next(&mut rx).await, Event::TableCreated { table, .. } if table == "renamed")
    );
    assert!(matches!(&*next(&mut rx).await, Event::Write { table, .. } if table == "renamed"));
    engine.execute_sql("TRUNCATE TABLE renamed").unwrap();
    assert!(matches!(&*next(&mut rx).await, Event::Delete { .. }));
    empty(&mut rx).await;
    engine.execute_sql("DROP TABLE copied").unwrap();
    assert!(matches!(&*next(&mut rx).await, Event::Delete { .. }));
    assert!(matches!(&*next(&mut rx).await, Event::TableDropped { .. }));
    engine.execute_sql("CREATE TEMPORARY TABLE temporary (id INT); INSERT INTO temporary VALUES (1); ALTER TABLE temporary ADD COLUMN v INT; DROP TEMPORARY TABLE temporary; CREATE TABLE IF NOT EXISTS renamed (id INT)").unwrap();
    let snapshot = engine.snapshot();
    engine.restore_snapshot(snapshot).unwrap();
    engine.import_state(engine.export_state().unwrap()).unwrap();
    empty(&mut rx).await;
}

#[tokio::test]
async fn composite_and_opaque_keys_cascades_and_generated_rows() {
    let engine = Engine::default();
    engine.execute_sql("CREATE TABLE parent (id INT PRIMARY KEY); CREATE TABLE child (id INT, parent_id INT, value INT DEFAULT 5, doubled INT GENERATED ALWAYS AS (value * 2) STORED, PRIMARY KEY (id, parent_id), FOREIGN KEY (parent_id) REFERENCES parent(id) ON DELETE CASCADE); CREATE TABLE keyless (v INT)").unwrap();
    let (_handle, mut rx) = listen(&engine, QueryHookOptions::default());
    engine.execute_sql("INSERT INTO parent VALUES (1); INSERT INTO child (id, parent_id) VALUES (2,1); INSERT INTO keyless VALUES (3)").unwrap();
    next(&mut rx).await;
    assert!(
        matches!(&*next(&mut rx).await, Event::Write { rows, .. } if rows[0].key == Key::Primary(json!({"id":2,"parent_id":1}).as_object().unwrap().clone()) && rows[0].row["doubled"] == json!(10))
    );
    let opaque = match &*next(&mut rx).await {
        Event::Write { rows, .. } => rows[0].key.clone(),
        other => panic!("{other:?}"),
    };
    assert!(matches!(&opaque, Key::Opaque(_)));
    engine.execute_sql("DELETE FROM keyless").unwrap();
    assert!(matches!(&*next(&mut rx).await, Event::Delete { keys, .. } if keys == &vec![opaque]));
    engine.execute_sql("DELETE FROM parent").unwrap();
    assert!(matches!(&*next(&mut rx).await, Event::Delete { table, .. } if table == "child"));
    assert!(matches!(&*next(&mut rx).await, Event::Delete { table, .. } if table == "parent"));
    empty(&mut rx).await;
}

#[tokio::test]
async fn reads_are_results_and_callbacks_can_reenter() {
    let engine = Arc::new(Engine::default());
    engine
        .execute_sql("CREATE TABLE items (id INT PRIMARY KEY); INSERT INTO items VALUES (1), (2)")
        .unwrap();
    let (_reader, mut rx) = listen(
        &engine,
        QueryHookOptions {
            read: true,
            write: false,
            delete: false,
            ..Default::default()
        },
    );
    engine
        .execute_sql_with_params(
            "SELECT id AS projected FROM items WHERE id > ?",
            &[json!(1)],
        )
        .unwrap();
    assert!(
        matches!(&*next(&mut rx).await, Event::Read { results, .. } if results[0].rows[0] == json!({"projected":2}).as_object().unwrap().clone())
    );
    engine
        .execute_sql("SELECT COUNT(*) AS n FROM items a JOIN items b ON a.id=b.id")
        .unwrap();
    assert!(
        matches!(&*next(&mut rx).await, Event::Read { results, .. } if results[0].rows[0]["n"] == json!(2))
    );
    engine
        .execute_sql("SELECT * FROM items WHERE FALSE")
        .unwrap();
    assert!(
        matches!(&*next(&mut rx).await, Event::Read { results, .. } if results[0].rows.is_empty())
    );
    engine
        .execute_sql("PREPARE q FROM 'SELECT id FROM items'; EXECUTE q")
        .unwrap();
    assert!(
        matches!(&*next(&mut rx).await, Event::Read { results, .. } if results.last().unwrap().rows.len() == 2)
    );
    assert!(engine.execute_sql("SELECT * FROM absent").is_err());
    empty(&mut rx).await;
    let callback_engine = engine.clone();
    let _writer = engine
        .subscribe_query_hooks(QueryHookOptions::default(), move |_| {
            callback_engine.execute_sql("SELECT id FROM items").unwrap();
            async { Ok(()) }
        })
        .unwrap();
    engine.execute_sql("INSERT INTO items VALUES (3)").unwrap();
    assert!(
        matches!(&*next(&mut rx).await, Event::Read { results, .. } if results[0].rows.len() == 3)
    );
}

#[tokio::test]
async fn failures_overflow_and_cancellation_are_isolated() {
    let engine = Engine::default();
    engine
        .execute_sql("CREATE TABLE items (id INT PRIMARY KEY)")
        .unwrap();
    let mut overflow = engine
        .subscribe_query_hooks(
            QueryHookOptions {
                capacity: 1,
                ..Default::default()
            },
            |_| async { std::future::pending::<anyhow::Result<()>>().await },
        )
        .unwrap();
    let mut failed = engine
        .subscribe_query_hooks(QueryHookOptions::default(), |_| async {
            anyhow::bail!("offline")
        })
        .unwrap();
    let mut panicked = engine
        .subscribe_query_hooks(QueryHookOptions::default(), |_| async {
            panic!("bad integration")
        })
        .unwrap();
    let (mut cancelled, mut cancelled_rx) = listen(&engine, QueryHookOptions::default());
    let (_healthy, mut healthy_rx) = listen(&engine, QueryHookOptions::default());
    cancelled.cancel();
    cancelled.wait().await.unwrap();
    engine
        .execute_sql("INSERT INTO items VALUES (1); INSERT INTO items VALUES (2)")
        .unwrap();
    assert_eq!(overflow.wait().await, Err(QueryHookError::Overflow));
    assert_eq!(
        failed.wait().await,
        Err(QueryHookError::Callback("offline".into()))
    );
    assert_eq!(panicked.wait().await, Err(QueryHookError::Panicked));
    assert!(
        matches!(&*next(&mut healthy_rx).await, Event::Write { rows, .. } if rows[0].key == primary(1))
    );
    assert!(
        matches!(&*next(&mut healthy_rx).await, Event::Write { rows, .. } if rows[0].key == primary(2))
    );
    empty(&mut cancelled_rx).await;
    let (dropped, mut dropped_rx) = listen(&engine, QueryHookOptions::default());
    drop(dropped);
    engine.execute_sql("INSERT INTO items VALUES (3)").unwrap();
    empty(&mut dropped_rx).await;
}

#[tokio::test]
async fn mysql_clients_use_the_same_hook_registry() {
    use mysql::prelude::Queryable;
    use std::sync::atomic::{AtomicBool, Ordering};
    let engine = Arc::new(Engine::default());
    let (_handle, mut rx) = listen(
        &engine,
        QueryHookOptions {
            read: false,
            ..all()
        },
    );
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let worker_stop = stop.clone();
    let server_engine = engine.clone();
    let worker = std::thread::spawn(move || {
        my_sqweel::server::WireServer::new(server_engine)
            .serve_listener_until(listener, worker_stop)
            .unwrap()
    });
    let mut conn = mysql::Conn::new(
        mysql::OptsBuilder::new()
            .ip_or_hostname(Some("127.0.0.1"))
            .tcp_port(address.port())
            .user(Some("root"))
            .db_name(Some("app")),
    )
    .unwrap();
    conn.query_drop("CREATE TABLE wire_items (id INT PRIMARY KEY)")
        .unwrap();
    conn.exec_drop("INSERT INTO wire_items VALUES (?)", (7,))
        .unwrap();
    drop(conn);
    stop.store(true, Ordering::Relaxed);
    worker.join().unwrap();
    assert!(
        matches!(&*next(&mut rx).await, Event::TableCreated { table, .. } if table == "wire_items")
    );
    assert!(
        matches!(&*next(&mut rx).await, Event::Write { rows, .. } if rows[0].key == primary(7))
    );
}

#[tokio::test]
async fn schema_key_changes_and_other_databases_use_current_row_identity() {
    let engine = Engine::default();
    engine.execute_sql("CREATE DATABASE other; USE other; CREATE TABLE items (id INT, v INT); INSERT INTO items VALUES (1, 4)").unwrap();
    let (_subscription, mut rx) = listen(
        &engine,
        QueryHookOptions {
            read: false,
            ..all()
        },
    );
    engine
        .execute_sql("ALTER TABLE items ADD PRIMARY KEY (id)")
        .unwrap();
    assert!(
        matches!(&*next(&mut rx).await, Event::Delete { database, keys, .. } if database == "other" && matches!(&keys[0], Key::Opaque(_)))
    );
    assert!(
        matches!(&*next(&mut rx).await, Event::TableUpdated { database, schema, .. } if database == "other" && schema.primary_key == ["id"])
    );
    assert!(
        matches!(&*next(&mut rx).await, Event::Write { rows, .. } if rows[0].key == primary(1))
    );
    engine.execute_sql("INSERT INTO items VALUES (1, 8) ON DUPLICATE KEY UPDATE v=VALUES(v); REPLACE INTO items VALUES (1, 9)").unwrap();
    for value in [8, 9] {
        assert!(
            matches!(&*next(&mut rx).await, Event::Write { rows, .. } if rows[0].row["v"] == json!(value))
        );
    }
    let state = engine.export_state().unwrap();
    assert_eq!(state.databases["other"].rows["items"].len(), 1);
    engine.execute_sql("DROP DATABASE other").unwrap();
    let deleted = next(&mut rx).await;
    assert!(
        matches!(&*deleted, Event::Delete { database, keys, .. } if database == "other" && keys == &vec![primary(1)])
    );
    assert!(
        matches!(&*next(&mut rx).await, Event::TableDropped { database, .. } if database == "other")
    );
    empty(&mut rx).await;
}
