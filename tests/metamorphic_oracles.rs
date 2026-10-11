//! Independent NoREC and metamorphic checks, also compared with MariaDB.
mod common;

use my_sqweel::sql::engine::{Engine, EngineConfig};
use mysql::prelude::Queryable;
use mysql::{Conn, Opts, Value};

#[derive(Debug, PartialEq)]
struct ResultRows {
    columns: Vec<(String, String)>,
    rows: Vec<Vec<Value>>,
}

fn start_mysqweel() -> (String, my_sqweel::SqlEndpoint) {
    let engine = Engine::new(EngineConfig::mysql_strict());
    engine.execute_sql("CREATE DATABASE test").unwrap();
    let endpoint = engine
        .spawn_sql(my_sqweel::SqlEndpointConfig::new(
            "127.0.0.1:0".parse().unwrap(),
            my_sqweel::server::Authentication::AllowAll,
        ))
        .unwrap();
    (
        format!("mysql://root@{}/test", endpoint.local_addr()),
        endpoint,
    )
}

fn connect(url: &str) -> Conn {
    let mut conn = Conn::new(Opts::from_url(url).unwrap()).expect("connect to SQL server");
    for sql in [
        "SET SESSION sql_mode = 'STRICT_TRANS_TABLES,NO_ENGINE_SUBSTITUTION'",
        "SET SESSION time_zone = '+00:00'",
        "SET NAMES utf8mb4 COLLATE utf8mb4_general_ci",
    ] {
        conn.query_drop(sql).unwrap();
    }
    conn
}

fn observe(conn: &mut Conn, sql: &str) -> ResultRows {
    let mut result = conn
        .query_iter(sql)
        .unwrap_or_else(|e| panic!("{sql}: {e}"));
    let mut set = result.iter().expect("one result set");
    let columns = set
        .columns()
        .as_ref()
        .iter()
        .map(|c| (c.name_str().into_owned(), format!("{:?}", c.column_type())))
        .collect();
    let rows = set
        .by_ref()
        .map(|row| row.unwrap().unwrap().into_iter().collect())
        .collect();
    drop(set);
    assert!(result.iter().is_none(), "expected one result set for {sql}");
    ResultRows { columns, rows }
}

fn setup(conn: &mut Conn, table: &str, empty: &str) {
    for sql in [
        format!("CREATE TABLE {table} (id INT PRIMARY KEY, score INT NULL)"),
        format!("CREATE TABLE {empty} (id INT PRIMARY KEY, score INT NULL)"),
        format!("INSERT INTO {table} VALUES (1, -1), (2, 0), (3, 2), (4, NULL), (5, 2)"),
    ] {
        conn.query_drop(sql).unwrap();
    }
}

#[test]
fn norec_and_null_safe_metamorphic_queries_match_mariadb() {
    let _guard = common::test_lock();
    let target = common::mysql_compare_target();
    let (url, _endpoint) = start_mysqweel();
    let mut sqweel = connect(&url);
    let mut mariadb = target.as_ref().map(|t| connect(t.url()));
    let (table, empty) = ("metamorphic_rows", "metamorphic_empty");
    setup(&mut sqweel, table, empty);
    if let Some(conn) = mariadb.as_mut() {
        setup(conn, table, empty);
    }

    // NoREC: IF(NULL, 1, 0) contributes zero, matching WHERE's exclusion of UNKNOWN.
    // This equivalence is checked on nullable rows and on an empty input.
    let cases = [
        (table, "score > 0"),
        (table, "score = 2"),
        (empty, "score > 0"),
    ];
    let mut parity_queries = Vec::new();
    for (source, predicate) in cases {
        let filtered = format!("SELECT COUNT(*) FROM {source} WHERE {predicate}");
        let norec = format!("SELECT COALESCE(SUM(IF({predicate}, 1, 0)), 0) FROM {source}");
        for conn in std::iter::once(&mut sqweel).chain(mariadb.iter_mut()) {
            let filtered_count: i64 = conn.query_first(&filtered).unwrap().unwrap();
            let unreduced_count: i64 = conn.query_first(&norec).unwrap().unwrap();
            assert_eq!(
                filtered_count, unreduced_count,
                "NoREC failed: {filtered} vs {norec}"
            );
        }
        parity_queries.extend([filtered, norec]);
    }

    // For non-NULL integer score, NOT(score <> 2) and score = 2 select the same rows.
    // The explicit IS NOT NULL guards the three-valued result; both tables include NULL rows.
    let equivalents = [
        (
            format!("SELECT id FROM {table} WHERE score = 2 ORDER BY id"),
            format!(
                "SELECT id FROM {table} WHERE NOT (score <> 2) AND score IS NOT NULL ORDER BY id"
            ),
        ),
        (
            format!("SELECT id FROM {empty} WHERE score = 2 ORDER BY id"),
            format!(
                "SELECT id FROM {empty} WHERE NOT (score <> 2) AND score IS NOT NULL ORDER BY id"
            ),
        ),
    ];
    for (left, right) in &equivalents {
        for conn in std::iter::once(&mut sqweel).chain(mariadb.iter_mut()) {
            assert_eq!(
                observe(conn, left),
                observe(conn, right),
                "metamorphic mismatch: {left} vs {right}"
            );
        }
        parity_queries.extend([left.clone(), right.clone()]);
    }

    if let Some(reference) = mariadb.as_mut() {
        for sql in parity_queries {
            assert_eq!(
                observe(&mut sqweel, &sql),
                observe(reference, &sql),
                "MariaDB parity mismatch: {sql}"
            );
        }
        for conn in std::iter::once(&mut sqweel).chain(mariadb.iter_mut()) {
            conn.query_drop(format!("DROP TABLE {empty}, {table}"))
                .unwrap();
        }
    } else {
        sqweel
            .query_drop(format!("DROP TABLE {empty}, {table}"))
            .unwrap();
        eprintln!("MariaDB comparison skipped; set MARIADB_COMPARE_URL or enable Docker.");
    }
}
