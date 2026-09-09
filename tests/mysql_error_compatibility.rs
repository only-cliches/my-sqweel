mod common;

use std::net::TcpListener;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use my_sqweel::server::WireServer;
use my_sqweel::sql::engine::{Engine, EngineConfig};
use mysql::prelude::Queryable;
use mysql::{Error, MySqlError, Opts, Pool};

use common::test_lock;

#[test]
fn strict_wire_errors_use_mysql_error_numbers() {
    let _guard = test_lock();
    let local_url = start_strict_server();
    let local_pool = Pool::new(Opts::from_url(&local_url).unwrap()).unwrap();
    let mut local = connect_with_retry(&local_pool);

    let mysql_target = common::mysql_compare_target();
    let mut mysql = mysql_target.as_ref().map(|target| {
        let pool = Pool::new(Opts::from_url(target.url()).unwrap()).unwrap();
        connect_with_retry(&pool)
    });

    let suffix = format!("{}_{}", std::process::id(), uuid::Uuid::new_v4().simple());
    let parent = format!("error_parent_{suffix}");
    let child = format!("error_child_{suffix}");
    let required = format!("error_required_{suffix}");

    let setup = [
        format!("CREATE TABLE {parent} (id BIGINT PRIMARY KEY, short_name VARCHAR(3))"),
        format!(
            "CREATE TABLE {child} (id BIGINT PRIMARY KEY, parent_id BIGINT, UNIQUE KEY uq_parent_id (parent_id), CONSTRAINT fk_error_parent FOREIGN KEY (parent_id) REFERENCES {parent}(id))"
        ),
        format!("CREATE TABLE {required} (id BIGINT PRIMARY KEY, required_value INT NOT NULL)"),
        format!("INSERT INTO {parent} VALUES (1, 'one')"),
        format!("INSERT INTO {child} VALUES (1, 1)"),
    ];
    for sql in &setup {
        local.query_drop(sql).unwrap();
        if let Some(mysql) = mysql.as_mut() {
            mysql.query_drop(sql).unwrap();
        }
    }

    let cases = [
        (format!("CREATE TABLE {parent} (id INT)"), 1050_u16),
        (format!("SELECT missing_column FROM {parent}"), 1054),
        (format!("SELECT * FROM missing_{suffix}"), 1146),
        (format!("INSERT INTO {parent} VALUES (1, 'two')"), 1062),
        (format!("INSERT INTO {required} (id) VALUES (1)"), 1364),
        (format!("INSERT INTO {required} VALUES (2, NULL)"), 1048),
        (format!("INSERT INTO {required} VALUES (3)"), 1136),
        (format!("INSERT INTO {parent} VALUES (2, 'toolong')"), 1406),
        (format!("INSERT INTO {child} VALUES (2, 999)"), 1452),
        (format!("DELETE FROM {parent} WHERE id = 1"), 1451),
    ];

    for (sql, expected) in cases {
        let local_code = mysql_error_code(local.query_drop(&sql).unwrap_err());
        assert_eq!(local_code, expected, "local error code for {sql}");
        if let Some(mysql) = mysql.as_mut() {
            let mysql_code = mysql_error_code(mysql.query_drop(&sql).unwrap_err());
            assert_eq!(local_code, mysql_code, "real-MySQL error parity for {sql}");
        }
    }

    for connection in std::iter::once(&mut local).chain(mysql.iter_mut()) {
        let _ = connection.query_drop(format!("DROP TABLE IF EXISTS {child}"));
        let _ = connection.query_drop(format!("DROP TABLE IF EXISTS {required}"));
        let _ = connection.query_drop(format!("DROP TABLE IF EXISTS {parent}"));
    }
}

fn start_strict_server() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    thread::spawn(move || {
        let engine = Arc::new(Engine::new(EngineConfig::mysql_strict()));
        engine.execute_sql("CREATE DATABASE test").unwrap();
        WireServer::new(engine).serve_listener(listener).unwrap();
    });
    format!("mysql://root@{address}/test")
}

fn connect_with_retry(pool: &Pool) -> mysql::PooledConn {
    for _ in 0..50 {
        if let Ok(connection) = pool.get_conn() {
            return connection;
        }
        thread::sleep(Duration::from_millis(50));
    }
    panic!("could not connect to MySQL test endpoint")
}

fn mysql_error_code(error: Error) -> u16 {
    match error {
        Error::MySqlError(MySqlError { code, .. }) => code,
        other => panic!("expected a server error, got {other}"),
    }
}
