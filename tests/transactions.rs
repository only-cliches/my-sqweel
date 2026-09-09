use std::path::PathBuf;
use std::sync::{Arc, mpsc};
use std::time::Duration;

use my_sqweel::sql::engine::{Engine, EngineConfig, EngineSession};
use serde_json::{Value, json};

fn strict_engine() -> Engine {
    Engine::new(EngineConfig::mysql_strict())
}

fn values(session: &mut EngineSession, sql: &str, column: &str) -> Vec<Value> {
    session.execute_sql(sql).unwrap()[0]
        .rows
        .iter()
        .map(|row| row[column].clone())
        .collect()
}

#[test]
fn failed_multirow_insert_rolls_back_statement_but_preserves_transaction() {
    let engine = strict_engine();
    engine
        .execute_sql("CREATE TABLE users (id BIGINT PRIMARY KEY, email TEXT UNIQUE)")
        .unwrap();
    let mut session = engine.session();
    assert!(
        session
            .execute_sql("INSERT INTO users VALUES (1, 'same'), (2, 'same')")
            .is_err()
    );
    assert!(values(&mut session, "SELECT id FROM users", "id").is_empty());
    session
        .execute_sql("BEGIN; INSERT INTO users VALUES (3, 'earlier')")
        .unwrap();
    assert!(
        session
            .execute_sql("INSERT INTO users VALUES (4, 'same'), (5, 'same')")
            .is_err()
    );
    assert_eq!(
        values(&mut session, "SELECT id FROM users ORDER BY id", "id"),
        vec![json!(3)]
    );
    session.execute_sql("COMMIT").unwrap();
    assert_eq!(
        values(
            &mut engine.session(),
            "SELECT id FROM users ORDER BY id",
            "id"
        ),
        vec![json!(3)]
    );
}

#[test]
fn explicit_rollback_restores_multiple_tables_and_secondary_indexes() {
    let engine = strict_engine();
    engine.execute_sql("CREATE TABLE accounts (id BIGINT PRIMARY KEY, balance BIGINT); CREATE INDEX balance_idx ON accounts (balance); CREATE TABLE ledger (id BIGINT PRIMARY KEY, amount BIGINT); INSERT INTO accounts VALUES (1, 10)").unwrap();
    let mut session = engine.session();
    session.execute_sql("BEGIN; UPDATE accounts SET balance = 20 WHERE id = 1; INSERT INTO ledger VALUES (1, 10)").unwrap();
    assert_eq!(
        values(
            &mut session,
            "SELECT id FROM accounts WHERE balance = 20",
            "id"
        ),
        vec![json!(1)]
    );
    session.execute_sql("ROLLBACK").unwrap();
    assert_eq!(
        values(
            &mut session,
            "SELECT id FROM accounts WHERE balance = 10",
            "id"
        ),
        vec![json!(1)]
    );
    assert!(
        values(
            &mut session,
            "SELECT id FROM accounts WHERE balance = 20",
            "id"
        )
        .is_empty()
    );
    assert!(values(&mut session, "SELECT id FROM ledger", "id").is_empty());
}

#[test]
fn cascade_before_restrict_failure_cannot_leave_child_changes() {
    let engine = strict_engine();
    engine.execute_sql("CREATE TABLE parents (id BIGINT PRIMARY KEY); CREATE TABLE cascading (id BIGINT PRIMARY KEY, parent_id BIGINT, FOREIGN KEY (parent_id) REFERENCES parents(id) ON DELETE CASCADE); CREATE TABLE restricting (id BIGINT PRIMARY KEY, parent_id BIGINT, FOREIGN KEY (parent_id) REFERENCES parents(id) ON DELETE RESTRICT); INSERT INTO parents VALUES (1); INSERT INTO cascading VALUES (1, 1); INSERT INTO restricting VALUES (1, 1)").unwrap();
    assert!(
        engine
            .execute_sql("DELETE FROM parents WHERE id = 1")
            .is_err()
    );
    let mut session = engine.session();
    for table in ["parents", "cascading", "restricting"] {
        assert_eq!(
            values(&mut session, &format!("SELECT id FROM {table}"), "id"),
            vec![json!(1)],
            "{table} changed after failed delete"
        );
    }
}

#[test]
fn savepoints_restore_state_and_release_removes_the_checkpoint() {
    let engine = strict_engine();
    engine
        .execute_sql("CREATE TABLE items (id BIGINT PRIMARY KEY)")
        .unwrap();
    let mut session = engine.session();
    session.execute_sql("BEGIN; INSERT INTO items VALUES (1); SAVEPOINT first; INSERT INTO items VALUES (2); SAVEPOINT second; INSERT INTO items VALUES (3); ROLLBACK TO SAVEPOINT first").unwrap();
    assert_eq!(
        values(&mut session, "SELECT id FROM items ORDER BY id", "id"),
        vec![json!(1)]
    );
    assert!(session.execute_sql("ROLLBACK TO SAVEPOINT second").is_err());
    session
        .execute_sql(
            "INSERT INTO items VALUES (4); ROLLBACK TO SAVEPOINT first; RELEASE SAVEPOINT first",
        )
        .unwrap();
    assert!(session.execute_sql("ROLLBACK TO SAVEPOINT first").is_err());
    session
        .execute_sql("INSERT INTO items VALUES (5); COMMIT")
        .unwrap();
    assert_eq!(
        values(
            &mut engine.session(),
            "SELECT id FROM items ORDER BY id",
            "id"
        ),
        vec![json!(1), json!(5)]
    );
}

#[test]
fn dropping_a_connection_discards_writes_and_releases_writer() {
    let engine = strict_engine();
    engine
        .execute_sql("CREATE TABLE items (id BIGINT PRIMARY KEY)")
        .unwrap();
    let mut session = engine.session();
    session
        .execute_sql("BEGIN; INSERT INTO items VALUES (1)")
        .unwrap();
    drop(session);
    engine.execute_sql("INSERT INTO items VALUES (2)").unwrap();
    assert_eq!(
        values(
            &mut engine.session(),
            "SELECT id FROM items ORDER BY id",
            "id"
        ),
        vec![json!(2)]
    );
}

#[test]
fn writers_serialize_while_other_connections_read_committed_state() {
    let engine = Arc::new(strict_engine());
    engine.execute_sql("CREATE TABLE counters (id BIGINT PRIMARY KEY, amount BIGINT); INSERT INTO counters VALUES (1, 10)").unwrap();
    let mut first = engine.session();
    first
        .execute_sql("BEGIN; UPDATE counters SET amount = amount + 1 WHERE id = 1")
        .unwrap();
    assert_eq!(
        values(&mut first, "SELECT amount FROM counters", "amount"),
        vec![json!(11)]
    );

    let (started_tx, started_rx) = mpsc::channel();
    let (finished_tx, finished_rx) = mpsc::channel();
    let other_engine = engine.clone();
    let writer = std::thread::spawn(move || {
        let mut second = other_engine.session();
        started_tx.send(()).unwrap();
        let result = second.execute_sql("UPDATE counters SET amount = amount + 1 WHERE id = 1");
        finished_tx.send(result).unwrap();
    });
    started_rx.recv_timeout(Duration::from_secs(2)).unwrap();

    let (read_tx, read_rx) = mpsc::channel();
    let reader_engine = engine.clone();
    let reader = std::thread::spawn(move || {
        read_tx
            .send(values(
                &mut reader_engine.session(),
                "SELECT amount FROM counters",
                "amount",
            ))
            .unwrap();
    });
    // Reader progress is part of the contract: lease validation uses another connection.
    assert_eq!(
        read_rx.recv_timeout(Duration::from_secs(2)).unwrap(),
        vec![json!(10)]
    );
    assert!(matches!(
        finished_rx.try_recv(),
        Err(mpsc::TryRecvError::Empty)
    ));
    first.execute_sql("COMMIT").unwrap();
    finished_rx
        .recv_timeout(Duration::from_secs(2))
        .unwrap()
        .unwrap();
    writer.join().unwrap();
    reader.join().unwrap();
    assert_eq!(
        values(
            &mut engine.session(),
            "SELECT amount FROM counters",
            "amount"
        ),
        vec![json!(12)]
    );
}

struct TestDirectory(PathBuf);
impl TestDirectory {
    fn new() -> Self {
        Self(std::env::temp_dir().join(format!("sqweel-transactions-{}", uuid::Uuid::new_v4())))
    }
    fn open(&self) -> Engine {
        Engine::open_with_data_dir(EngineConfig::mysql_strict(), Some(self.0.to_str().unwrap()))
            .unwrap()
    }
}
impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn reopen_recovers_commits_views_and_indexes_but_not_pending_writes() {
    let directory = TestDirectory::new();
    let engine = directory.open();
    engine.execute_sql("CREATE TABLE items (id BIGINT PRIMARY KEY, label TEXT); CREATE INDEX label_idx ON items (label); CREATE VIEW selected_items AS SELECT id, label FROM items WHERE label = 'keep'").unwrap();
    let mut session = engine.session();
    session.execute_sql("BEGIN; INSERT INTO items VALUES (1, 'keep'); COMMIT; BEGIN; INSERT INTO items VALUES (2, 'discard')").unwrap();
    drop(session);
    drop(engine);

    let engine = directory.open();
    assert_eq!(
        values(
            &mut engine.session(),
            "SELECT id FROM items ORDER BY id",
            "id"
        ),
        vec![json!(1)]
    );
    assert_eq!(
        values(&mut engine.session(), "SELECT id FROM selected_items", "id"),
        vec![json!(1)]
    );
    assert_eq!(
        values(
            &mut engine.session(),
            "SELECT id FROM items WHERE label = 'keep'",
            "id"
        ),
        vec![json!(1)]
    );
    let indexes = engine.execute_sql("SHOW INDEX FROM items").unwrap();
    assert!(
        indexes[0]
            .rows
            .iter()
            .any(|row| row.get("Key_name") == Some(&json!("label_idx")))
    );
    engine
        .execute_sql("UPDATE items SET label = 'changed' WHERE id = 1")
        .unwrap();
    assert!(values(&mut engine.session(), "SELECT id FROM selected_items", "id").is_empty());
}

#[test]
fn maintenance_swap_and_restore_failures_do_not_publish_partial_changes() {
    let engine = strict_engine();
    engine.execute_sql("CREATE TABLE a (id INT PRIMARY KEY); CREATE TABLE b (id INT PRIMARY KEY); CREATE TABLE c (id INT PRIMARY KEY); INSERT INTO a VALUES (1); INSERT INTO b VALUES (2); INSERT INTO c VALUES (3)").unwrap();
    assert!(
        engine
            .swap_tables(&[("a".into(), "b".into()), ("c".into(), "missing".into())])
            .is_err()
    );
    let mut session = engine.session();
    assert_eq!(
        values(&mut session, "SELECT id FROM a", "id"),
        vec![json!(1)]
    );
    assert_eq!(
        values(&mut session, "SELECT id FROM b", "id"),
        vec![json!(2)]
    );
    let mut snapshot = engine.snapshot();
    snapshot.version = 999;
    assert!(engine.restore_snapshot(snapshot).is_err());
    assert_eq!(
        values(&mut session, "SELECT id FROM c", "id"),
        vec![json!(3)]
    );
}

#[test]
fn unsupported_transaction_modes_fail_without_mutating_the_database() {
    let engine = strict_engine();
    engine
        .execute_sql("CREATE TABLE items (id INT PRIMARY KEY)")
        .unwrap();
    let mut session = engine.session();
    assert!(
        session
            .execute_sql("SELECT * FROM items FOR UPDATE SKIP LOCKED")
            .is_err()
    );
    assert!(
        session
            .execute_sql("SET TRANSACTION ISOLATION LEVEL READ COMMITTED")
            .is_err()
    );
    session.execute_sql("BEGIN").unwrap();
    assert!(
        session
            .execute_sql("CREATE VIEW forbidden AS SELECT * FROM items")
            .is_err()
    );
    session.execute_sql("ROLLBACK").unwrap();
    assert!(!engine.snapshot().views.contains_key("forbidden"));
}

#[test]
fn read_snapshot_upgrade_preserves_other_database_commits_and_savepoints() {
    let engine = strict_engine();
    engine
        .execute_sql("CREATE TABLE items(id INT PRIMARY KEY); CREATE DATABASE other")
        .unwrap();
    let mut first = engine.session();
    let mut other = engine.session();
    other.use_database("other").unwrap();
    first
        .execute_sql("BEGIN; SAVEPOINT initial; SELECT * FROM items")
        .unwrap();
    other
        .execute_sql("CREATE TABLE durable(id INT PRIMARY KEY); INSERT INTO durable VALUES(8)")
        .unwrap();
    first
        .execute_sql(
            "INSERT INTO items VALUES(1); ROLLBACK TO initial; INSERT INTO items VALUES(2); COMMIT",
        )
        .unwrap();
    assert_eq!(
        values(&mut other, "SELECT id FROM durable", "id"),
        vec![json!(8)]
    );
    assert_eq!(
        values(&mut first, "SELECT id FROM items", "id"),
        vec![json!(2)]
    );
}

#[test]
fn first_write_refreshes_unobserved_snapshot_and_earlier_savepoints() {
    let engine = strict_engine();
    engine
        .execute_sql("CREATE TABLE items(id INT PRIMARY KEY)")
        .unwrap();
    let mut session = engine.session();
    session.execute_sql("BEGIN; SAVEPOINT initial").unwrap();
    engine.execute_sql("INSERT INTO items VALUES(1)").unwrap();
    session
        .execute_sql(
            "INSERT INTO items VALUES(2); ROLLBACK TO initial; INSERT INTO items VALUES(3); COMMIT",
        )
        .unwrap();
    assert_eq!(
        values(
            &mut engine.session(),
            "SELECT id FROM items ORDER BY id",
            "id"
        ),
        vec![json!(1), json!(3)]
    );
    session.execute_sql("BEGIN").unwrap();
    engine.execute_sql("INSERT INTO items VALUES(4)").unwrap();
    assert_eq!(
        values(&mut session, "SELECT id FROM items ORDER BY id", "id"),
        vec![json!(1), json!(3), json!(4)]
    );
    session.execute_sql("ROLLBACK").unwrap();
}

#[test]
fn unrelated_outbox_commit_does_not_abort_business_edit_or_get_lost_at_savepoint() {
    let engine = strict_engine();
    engine.execute_sql("CREATE TABLE __intent_database(id INT PRIMARY KEY); INSERT INTO __intent_database VALUES(1); CREATE TABLE records(id INT PRIMARY KEY, value INT); INSERT INTO records VALUES(1,10); CREATE TABLE search_outbox(id INT PRIMARY KEY, attempts INT); INSERT INTO search_outbox VALUES(1,0)").unwrap();
    let mut editor = engine.session();
    editor.execute_sql("BEGIN; SELECT id FROM __intent_database; SELECT value FROM records WHERE id=1; SAVEPOINT before_edit").unwrap();
    engine
        .execute_sql("UPDATE search_outbox SET attempts=1 WHERE id=1")
        .unwrap();
    editor.execute_sql("UPDATE records SET value=20 WHERE id=1; ROLLBACK TO before_edit; UPDATE records SET value=30 WHERE id=1; COMMIT").unwrap();
    assert_eq!(
        values(&mut engine.session(), "SELECT value FROM records", "value"),
        vec![json!(30)]
    );
    assert_eq!(
        values(
            &mut engine.session(),
            "SELECT attempts FROM search_outbox",
            "attempts"
        ),
        vec![json!(1)]
    );
}

#[test]
fn changed_observed_tables_and_unknown_read_dependencies_still_abort_upgrade() {
    for select in [
        "SELECT value FROM records",
        "SELECT count(*) FROM records",
        "SELECT value FROM records WHERE id IN (SELECT id FROM records)",
    ] {
        let engine = strict_engine();
        engine.execute_sql("CREATE TABLE records(id INT PRIMARY KEY,value INT); INSERT INTO records VALUES(1,10); CREATE TABLE outbox(id INT PRIMARY KEY); INSERT INTO outbox VALUES(1)").unwrap();
        let mut editor = engine.session();
        editor.execute_sql("BEGIN").unwrap();
        editor.execute_sql(select).unwrap();
        engine
            .execute_sql("UPDATE records SET value=20 WHERE id=1")
            .unwrap();
        assert!(
            editor
                .execute_sql("UPDATE records SET value=30 WHERE id=1")
                .unwrap_err()
                .to_string()
                .contains("retry transaction")
        );
        assert!(!editor.is_in_transaction());
        assert_eq!(
            values(&mut engine.session(), "SELECT value FROM records", "value"),
            vec![json!(20)]
        );
    }
    let engine = strict_engine();
    engine.execute_sql("CREATE TABLE records(id INT PRIMARY KEY); INSERT INTO records VALUES(1); CREATE TABLE outbox(id INT PRIMARY KEY)").unwrap();
    let mut editor = engine.session();
    editor
        .execute_sql("BEGIN; SELECT count(*) FROM records")
        .unwrap();
    engine.execute_sql("INSERT INTO outbox VALUES(1)").unwrap();
    assert!(
        editor
            .execute_sql("INSERT INTO records VALUES(2)")
            .unwrap_err()
            .to_string()
            .contains("retry transaction")
    );
}

#[test]
fn projected_identity_read_allows_unobserved_settings_updates() {
    let engine = strict_engine();
    engine.execute_sql("CREATE TABLE settings(id INT PRIMARY KEY, city TEXT, active INT); INSERT INTO settings VALUES(1,'Old',1); CREATE TABLE screens(id INT PRIMARY KEY)").unwrap();
    let mut editor = engine.session();
    editor
        .execute_sql(
            "BEGIN; SELECT id FROM settings WHERE active=1 LIMIT 1; SAVEPOINT before_insert",
        )
        .unwrap();
    engine
        .execute_sql("UPDATE settings SET city='New' WHERE id=1")
        .unwrap();
    editor.execute_sql("INSERT INTO screens VALUES(1); ROLLBACK TO before_insert; INSERT INTO screens VALUES(2); COMMIT").unwrap();
    assert_eq!(
        values(&mut engine.session(), "SELECT city FROM settings", "city"),
        vec![json!("New")]
    );
    assert_eq!(
        values(&mut engine.session(), "SELECT id FROM screens", "id"),
        vec![json!(2)]
    );
}

#[test]
fn projected_read_still_validates_predicates_ordering_rows_and_all_prior_reads() {
    for (read, mutation) in [
        (
            "SELECT id FROM settings WHERE active=1",
            "UPDATE settings SET active=0",
        ),
        (
            "SELECT id FROM settings ORDER BY city LIMIT 1",
            "UPDATE settings SET city='New'",
        ),
        (
            "SELECT CITY FROM settings",
            "UPDATE settings SET city='New'",
        ),
        (
            "SELECT id FROM settings",
            "INSERT INTO settings VALUES(2,'Other',1)",
        ),
        ("SELECT id FROM settings", "DELETE FROM settings"),
        (
            "SELECT * FROM settings; SELECT id FROM settings",
            "UPDATE settings SET city='New'",
        ),
        (
            "SELECT city FROM settings; SELECT id FROM settings",
            "UPDATE settings SET city='New'",
        ),
    ] {
        let engine = strict_engine();
        engine.execute_sql("CREATE TABLE settings(id INT PRIMARY KEY, city TEXT, active INT); INSERT INTO settings VALUES(1,'Old',1); CREATE TABLE screens(id INT PRIMARY KEY)").unwrap();
        let mut editor = engine.session();
        editor.execute_sql(&format!("BEGIN; {read}")).unwrap();
        engine.execute_sql(mutation).unwrap();
        assert!(
            editor
                .execute_sql("INSERT INTO screens VALUES(1)")
                .unwrap_err()
                .to_string()
                .contains("retry transaction"),
            "{read}: {mutation}"
        );
        assert!(values(&mut engine.session(), "SELECT id FROM screens", "id").is_empty());
    }
}

#[test]
fn nested_database_writes_commit_independently_and_survive_reopen() {
    let directory = TestDirectory::new();
    {
        let engine = directory.open();
        engine
            .execute_sql("CREATE TABLE items(id INT PRIMARY KEY); CREATE DATABASE platform")
            .unwrap();
        let mut platform = engine.session();
        platform.use_database("platform").unwrap();
        platform
            .execute_sql("CREATE TABLE credentials(id INT PRIMARY KEY)")
            .unwrap();
        let mut shard = engine.session();
        shard
            .execute_sql("BEGIN; INSERT INTO items VALUES(1)")
            .unwrap();
        platform
            .execute_sql("BEGIN; INSERT INTO credentials VALUES(1); COMMIT")
            .unwrap();
        shard
            .execute_sql("COMMIT; BEGIN; INSERT INTO items VALUES(2)")
            .unwrap();
        platform
            .execute_sql("BEGIN; INSERT INTO credentials VALUES(2); COMMIT")
            .unwrap();
        shard.execute_sql("ROLLBACK").unwrap();
    }
    let engine = directory.open();
    assert_eq!(
        values(&mut engine.session(), "SELECT id FROM items", "id"),
        vec![json!(1)]
    );
    let mut platform = engine.session();
    platform.use_database("platform").unwrap();
    assert_eq!(
        values(
            &mut platform,
            "SELECT id FROM credentials ORDER BY id",
            "id"
        ),
        vec![json!(1), json!(2)]
    );
}

#[test]
fn concurrent_database_commits_merge_without_losing_either_database() {
    let directory = TestDirectory::new();
    {
        let engine = Arc::new(directory.open());
        engine
            .execute_sql("CREATE TABLE items(id INT PRIMARY KEY); CREATE DATABASE other")
            .unwrap();
        let mut other = engine.session();
        other.use_database("other").unwrap();
        other
            .execute_sql("CREATE TABLE items(id INT PRIMARY KEY)")
            .unwrap();
        let (ready_tx, ready_rx) = mpsc::channel();
        let mut workers = Vec::new();
        let mut permits = Vec::new();
        for database in ["app", "other"] {
            let engine = engine.clone();
            let ready = ready_tx.clone();
            let (permit, receive) = mpsc::channel();
            permits.push(permit);
            workers.push(std::thread::spawn(move || {
                let mut session = engine.session();
                session.use_database(database).unwrap();
                session
                    .execute_sql("BEGIN; INSERT INTO items VALUES(1)")
                    .unwrap();
                ready.send(()).unwrap();
                if receive.recv_timeout(Duration::from_secs(5)).is_ok() {
                    session.execute_sql("COMMIT").unwrap();
                }
            }));
        }
        for _ in 0..2 {
            ready_rx.recv_timeout(Duration::from_secs(3)).unwrap();
        }
        for permit in permits {
            permit.send(()).unwrap();
        }
        for worker in workers {
            worker.join().unwrap();
        }
    }
    let engine = directory.open();
    for database in ["app", "other"] {
        let mut session = engine.session();
        session.use_database(database).unwrap();
        assert_eq!(
            values(&mut session, "SELECT id FROM items", "id"),
            vec![json!(1)]
        );
    }
}

#[test]
fn same_database_writer_and_catalog_mutations_wait_for_active_writer() {
    let engine = Arc::new(strict_engine());
    engine
        .execute_sql("CREATE TABLE items(id INT PRIMARY KEY)")
        .unwrap();
    let mut writer = engine.session();
    writer
        .execute_sql("BEGIN; INSERT INTO items VALUES(1)")
        .unwrap();
    let mut workers = Vec::new();
    for sql in ["INSERT INTO items VALUES(2)", "CREATE DATABASE blocked"] {
        let engine = engine.clone();
        workers.push(std::thread::spawn(move || {
            assert!(
                engine
                    .session()
                    .execute_sql(sql)
                    .unwrap_err()
                    .to_string()
                    .contains("Lock wait timeout")
            );
        }));
    }
    for worker in workers {
        worker.join().unwrap();
    }
    writer.execute_sql("ROLLBACK").unwrap();
    engine
        .execute_sql("CREATE DATABASE blocked; INSERT INTO items VALUES(2)")
        .unwrap();
    assert_eq!(
        values(&mut engine.session(), "SELECT id FROM items", "id"),
        vec![json!(2)]
    );
}

#[test]
fn dropped_database_with_read_only_savepoints_returns_error_without_resurrection() {
    for read in ["", "SELECT id FROM items;"] {
        let engine = strict_engine();
        engine.execute_sql("CREATE DATABASE disposable").unwrap();
        let mut session = engine.session();
        session.use_database("disposable").unwrap();
        session
            .execute_sql("CREATE TABLE items(id INT PRIMARY KEY)")
            .unwrap();
        session
            .execute_sql(&format!("BEGIN; {read} SAVEPOINT before_insert"))
            .unwrap();
        engine.execute_sql("DROP DATABASE disposable").unwrap();
        assert!(session.execute_sql("INSERT INTO items VALUES(1)").is_err());
        assert!(session.execute_sql("SELECT id FROM items").is_err());
        drop(session);
        assert!(engine.session().use_database("disposable").is_err());
    }
}
