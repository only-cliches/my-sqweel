mod common;

use my_sqweel::sql::engine::{Engine, EngineConfig};
use mysql::prelude::Queryable;
use mysql::{Conn, Opts};
use serde_json::json;

fn reference_schedule(url: &str) {
    let table = format!(
        "coverage_interleavings_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let mut writer = Conn::new(Opts::from_url(url).unwrap()).unwrap();
    let mut reader = Conn::new(Opts::from_url(url).unwrap()).unwrap();
    writer
        .query_drop(format!(
            "CREATE TABLE {table} (id INT PRIMARY KEY, amount INT NOT NULL)"
        ))
        .unwrap();
    writer
        .query_drop(format!("INSERT INTO {table} VALUES (1, 10)"))
        .unwrap();
    writer.query_drop("START TRANSACTION").unwrap();
    writer
        .query_drop(format!(
            "UPDATE {table} SET amount = amount + 5 WHERE id = 1"
        ))
        .unwrap();
    writer
        .query_drop(format!("INSERT INTO {table} VALUES (2, 20)"))
        .unwrap();
    assert_eq!(
        writer
            .query_first::<i64, _>(format!("SELECT COUNT(*) FROM {table}"))
            .unwrap(),
        Some(2)
    );
    assert_eq!(
        reader
            .query_first::<i64, _>(format!("SELECT COUNT(*) FROM {table}"))
            .unwrap(),
        Some(1)
    );
    reader.query_drop("START TRANSACTION").unwrap();
    assert_eq!(
        reader
            .query_first::<i64, _>(format!("SELECT amount FROM {table} WHERE id = 1"))
            .unwrap(),
        Some(10)
    );
    writer.query_drop("COMMIT").unwrap();
    assert_eq!(
        reader
            .query_first::<i64, _>(format!("SELECT amount FROM {table} WHERE id = 1"))
            .unwrap(),
        Some(10)
    );
    reader.query_drop("COMMIT").unwrap();
    assert_eq!(
        reader
            .query_first::<i64, _>(format!("SELECT COUNT(*) FROM {table}"))
            .unwrap(),
        Some(2)
    );
    writer.query_drop("START TRANSACTION").unwrap();
    writer.query_drop("SAVEPOINT keep").unwrap();
    writer
        .query_drop(format!("UPDATE {table} SET amount = 99 WHERE id = 1"))
        .unwrap();
    writer.query_drop("ROLLBACK TO keep").unwrap();
    writer
        .query_drop(format!("UPDATE {table} SET amount = 30 WHERE id = 1"))
        .unwrap();
    assert_eq!(
        writer
            .query_first::<i64, _>(format!("SELECT amount FROM {table} WHERE id = 1"))
            .unwrap(),
        Some(30)
    );
    writer.query_drop("ROLLBACK").unwrap();
    assert_eq!(
        reader
            .query_first::<i64, _>(format!("SELECT amount FROM {table} WHERE id = 1"))
            .unwrap(),
        Some(15)
    );
    writer.query_drop(format!("DROP TABLE {table}")).unwrap();
}

fn report_coverage(entry: serde_json::Value) {
    let Ok(dir) = std::env::var("COVERAGE_PATH_REPORT") else {
        return;
    };
    let dir = std::path::PathBuf::from(dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("interleavings.json");
    let temp = dir.join("interleavings.json.tmp");
    std::fs::write(&temp, serde_json::to_vec_pretty(&entry).unwrap()).unwrap();
    std::fs::rename(temp, path).unwrap();
}

fn ids(engine: &Engine, sql: &str) -> Vec<serde_json::Value> {
    engine.execute_sql(sql).unwrap()[0]
        .rows
        .iter()
        .map(|row| row["id"].clone())
        .collect()
}

#[test]
fn deterministic_two_session_commit_and_rollback_schedules() {
    let _guard = common::test_lock();
    let engine = Engine::new(EngineConfig::mysql_strict());
    engine.execute_sql("CREATE TABLE ledger (id INT PRIMARY KEY, amount INT NOT NULL); INSERT INTO ledger VALUES (1, 10)").unwrap();
    let mut writer = engine.session();
    let mut reader = engine.session();

    writer.execute_sql("BEGIN; UPDATE ledger SET amount = amount + 5 WHERE id = 1; INSERT INTO ledger VALUES (2, 20)").unwrap();
    assert_eq!(
        ids(&engine, "SELECT id FROM ledger ORDER BY id"),
        vec![json!(1)]
    );
    assert_eq!(session_ids(&mut writer), vec![json!(1), json!(2)]);
    reader.execute_sql("BEGIN").unwrap();
    assert_eq!(
        reader
            .execute_sql("SELECT amount FROM ledger WHERE id = 1")
            .unwrap()[0]
            .rows[0]["amount"],
        json!(10)
    );
    writer.execute_sql("COMMIT").unwrap();
    assert_eq!(
        reader
            .execute_sql("SELECT amount FROM ledger WHERE id = 1")
            .unwrap()[0]
            .rows[0]["amount"],
        json!(10)
    );
    reader.execute_sql("COMMIT").unwrap();
    assert_eq!(
        ids(&engine, "SELECT id FROM ledger ORDER BY id"),
        vec![json!(1), json!(2)]
    );

    writer.execute_sql("BEGIN; SAVEPOINT keep; UPDATE ledger SET amount = 99 WHERE id = 1; ROLLBACK TO keep; UPDATE ledger SET amount = 30 WHERE id = 1").unwrap();
    assert_eq!(
        writer
            .execute_sql("SELECT amount FROM ledger WHERE id = 1")
            .unwrap()[0]
            .rows[0]["amount"],
        json!(30)
    );
    assert!(writer.is_in_transaction());
    writer.execute_sql("ROLLBACK").unwrap();
    assert_eq!(
        engine
            .execute_sql("SELECT amount FROM ledger WHERE id = 1")
            .unwrap()[0]
            .rows[0]["amount"],
        json!(15)
    );
    let reference = std::env::var("MARIADB_COMPARE_URL")
        .ok()
        .or_else(|| std::env::var("MYSQL_COMPARE_URL").ok());
    if let Some(url) = &reference {
        reference_schedule(url);
    }
    report_coverage(
        json!({"kind":"interleavings","covered":["writer_read_your_writes","other_session_cannot_read_uncommitted_insert","repeatable_read_snapshot_before_and_after_commit","commit_becomes_visible_after_reader_transaction_ends","savepoint_rollback_then_transaction_rollback"],"reference_verified":reference.is_some()}),
    );
}

// Sessions are connection-owned; use the session itself for read-your-writes assertions.
fn session_ids(session: &mut my_sqweel::sql::engine::EngineSession) -> Vec<serde_json::Value> {
    session
        .execute_sql("SELECT id FROM ledger ORDER BY id")
        .unwrap()[0]
        .rows
        .iter()
        .map(|row| row["id"].clone())
        .collect()
}
