use std::time::Duration;

mod common;

use my_sqweel::sql::engine::{Engine, EngineConfig};
use mysql::prelude::Queryable;
use mysql::{Conn, OptsBuilder};
use serde_json::json;

fn report_coverage(entry: serde_json::Value) {
    let Ok(dir) = std::env::var("COVERAGE_PATH_REPORT") else {
        return;
    };
    let dir = std::path::PathBuf::from(dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("execution_paths.json");
    let temp = dir.join("execution_paths.json.tmp");
    std::fs::write(&temp, serde_json::to_vec_pretty(&entry).unwrap()).unwrap();
    std::fs::rename(temp, path).unwrap();
}

const SETUP: &str = "CREATE TABLE readings (id INT PRIMARY KEY, group_name VARCHAR(8), value INT NULL); INSERT INTO readings VALUES (1,'a',4),(2,'a',NULL),(3,'b',7)";
const QUERY: &str = "SELECT group_name, COUNT(*) AS n, COUNT(value) AS present, COALESCE(SUM(value), 0) AS total FROM readings GROUP BY group_name ORDER BY group_name";

fn assert_embedded(engine: &Engine) {
    let rows = engine.execute_sql(QUERY).unwrap().remove(0).rows;
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["group_name"], json!("a"));
    assert_eq!(rows[0]["n"], json!(2));
    assert_eq!(rows[0]["present"], json!(1));
    assert_eq!(rows[0]["total"], json!(4));
    assert_eq!(rows[1]["group_name"], json!("b"));
    assert_eq!(rows[1]["n"], json!(1));
    assert_eq!(rows[1]["total"], json!(7));
}

#[test]
fn semantics_match_across_sync_async_wire_and_reopened_rocksdb() {
    let _guard = common::test_lock();
    let sync = Engine::new(EngineConfig::mysql_strict());
    sync.execute_sql(SETUP).unwrap();
    assert_embedded(&sync);

    let async_engine = Engine::new(EngineConfig::mysql_strict());
    async_engine.execute_sql(SETUP).unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let rows = runtime
        .block_on(async_engine.execute_sql_async(QUERY))
        .unwrap()
        .remove(0)
        .rows;
    assert_eq!(rows, sync.execute_sql(QUERY).unwrap().remove(0).rows);

    let wire_engine = std::sync::Arc::new(Engine::new(EngineConfig::mysql_strict()));
    wire_engine.execute_sql(SETUP).unwrap();
    let endpoint = wire_engine
        .spawn_sql(my_sqweel::SqlEndpointConfig::new(
            "127.0.0.1:0".parse().unwrap(),
            my_sqweel::server::Authentication::AllowAll,
        ))
        .unwrap();
    let mut conn = Conn::new(
        OptsBuilder::new()
            .ip_or_hostname(Some("127.0.0.1"))
            .tcp_port(endpoint.local_addr().port())
            .user(Some("root"))
            .read_timeout(Some(Duration::from_secs(10)))
            .write_timeout(Some(Duration::from_secs(10))),
    )
    .unwrap();
    let wire: Vec<(String, u64, u64, i64)> = conn.query(QUERY).unwrap();
    assert_eq!(wire, vec![("a".into(), 2, 1, 4), ("b".into(), 1, 1, 7)]);
    let prepared_query = QUERY.replacen(" GROUP BY", " WHERE group_name >= ? GROUP BY", 1);
    let prepared: Vec<(String, u64, u64, i64)> = conn.exec(&prepared_query, ("a",)).unwrap();
    assert_eq!(prepared, wire);
    drop(conn);
    drop(endpoint);
    drop(wire_engine);

    let mut reference_verified = false;
    if let Some(url) = std::env::var("MARIADB_COMPARE_URL")
        .ok()
        .or_else(|| std::env::var("MYSQL_COMPARE_URL").ok())
    {
        let table = "coverage_path_readings";
        let mut reference = Conn::new(mysql::Opts::from_url(&url).unwrap()).unwrap();
        reference.query_drop(format!("CREATE TEMPORARY TABLE {table} (id INT PRIMARY KEY, group_name VARCHAR(8), value INT NULL)")).unwrap();
        reference
            .query_drop(format!(
                "INSERT INTO {table} VALUES (1,'a',4),(2,'a',NULL),(3,'b',7)"
            ))
            .unwrap();
        let reference_query = QUERY.replace("readings", &table);
        let rows: Vec<(String, u64, u64, i64)> = reference.query(&reference_query).unwrap();
        assert_eq!(rows, wire);
        let reference_prepared_query =
            reference_query.replacen(" GROUP BY", " WHERE group_name >= ? GROUP BY", 1);
        let reference_prepared: Vec<(String, u64, u64, i64)> =
            reference.exec(reference_prepared_query, ("a",)).unwrap();
        assert_eq!(reference_prepared, prepared);
        reference_verified = true;
    }

    let storage = tempfile::tempdir().unwrap();
    let dir = storage.path().to_path_buf();
    {
        let rocks = Engine::open(
            EngineConfig::mysql_strict(),
            my_sqweel::Storage::RocksDb(dir.clone()),
        )
        .unwrap();
        rocks.execute_sql(SETUP).unwrap();
    }
    let reopened = Engine::open(
        EngineConfig::mysql_strict(),
        my_sqweel::Storage::RocksDb(dir.clone()),
    )
    .unwrap();
    assert_embedded(&reopened);
    drop(reopened);
    report_coverage(
        json!({"kind":"execution_paths","covered":["embedded_sync","embedded_async","wire_text_query","wire_prepared_query","reopened_rocksdb","prepared_statements","prepared_parameters"],"reference_verified":reference_verified}),
    );
}
