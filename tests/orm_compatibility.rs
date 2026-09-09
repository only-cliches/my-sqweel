mod common;

use std::net::TcpListener;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use my_sqweel::server::WireServer;
use my_sqweel::sql::engine::{Engine, EngineConfig};
use mysql::prelude::Queryable;
use mysql::{Opts, Pool, Row, Value as MyValue};

use common::test_lock;

#[test]
fn common_orm_migration_introspection_and_prepared_crud_shapes_work() {
    let _guard = test_lock();
    let url = start_strict_server();
    let pool = Pool::new(Opts::from_url(&url).unwrap()).unwrap();
    let mut connection = connect_with_retry(&pool);
    let suffix = format!("{}_{}", std::process::id(), uuid::Uuid::new_v4().simple());
    let users = format!("orm_users_{suffix}");
    let posts = format!("orm_posts_{suffix}");

    // Migration shapes emitted by Diesel, Drizzle/Knex, Prisma, and SeaORM.
    connection
        .query_drop(format!(
            "CREATE TABLE {users} (id BIGINT AUTO_INCREMENT PRIMARY KEY, email VARCHAR(191) NOT NULL UNIQUE, active TINYINT(1) NOT NULL DEFAULT 1, display_name VARCHAR(64), created_at DATETIME, birthday DATE, elapsed TIME(6), balance DECIMAL(12,2), profile JSON)"
        ))
        .unwrap();
    connection
        .query_drop(format!(
            "CREATE TABLE {posts} (id BIGINT AUTO_INCREMENT PRIMARY KEY, user_id BIGINT NOT NULL, title VARCHAR(255) NOT NULL, CONSTRAINT fk_orm_user FOREIGN KEY (user_id) REFERENCES {users}(id) ON DELETE CASCADE)"
        ))
        .unwrap();
    connection
        .query_drop(format!(
            "ALTER TABLE {users} ADD COLUMN sort_order INT NOT NULL DEFAULT 0 AFTER active"
        ))
        .unwrap();
    connection
        .query_drop(format!(
            "CREATE INDEX idx_orm_email_prefix ON {users} (email(16))"
        ))
        .unwrap();

    // Prepared INSERT/SELECT/UPDATE/UPSERT forms exercise the binary protocol.
    connection
        .exec_drop(
            format!(
                "INSERT INTO {users} (email, active, display_name, created_at) VALUES (?, ?, ?, ?)"
            ),
            ("first@example.test", 1_i8, "First", "2026-07-14 12:00:00"),
        )
        .unwrap();
    let user_id = connection.last_insert_id();
    connection
        .exec_drop(
            format!("INSERT INTO {posts} (user_id, title) VALUES (?, ?)"),
            (user_id, "Hello"),
        )
        .unwrap();
    connection
        .exec_drop(
            format!(
                "INSERT INTO {users} (id, email, active) VALUES (?, ?, ?) ON DUPLICATE KEY UPDATE display_name = VALUES(display_name), active = VALUES(active)"
            ),
            (user_id, "first@example.test", 0_i8),
        )
        .unwrap();
    connection
        .exec_drop(
            format!("UPDATE {users} SET display_name = ? WHERE id = ?"),
            ("Updated", user_id),
        )
        .unwrap();
    connection
        .exec_drop(
            format!(
                "UPDATE {users} SET birthday = ?, elapsed = ?, balance = ?, profile = ? WHERE id = ?"
            ),
            (
                MyValue::Date(2026, 7, 14, 0, 0, 0, 0),
                MyValue::Time(true, 0, 2, 3, 4, 500_000),
                "12.34",
                r#"{"role":"admin"}"#,
                user_id,
            ),
        )
        .unwrap();

    let rows: Vec<Row> = connection
        .exec(
            format!(
                "SELECT u.id AS user_id, u.email, u.active, p.title FROM {users} AS u LEFT JOIN {posts} AS p ON p.user_id = u.id WHERE u.id >= ? ORDER BY u.id LIMIT ?"
            ),
            (user_id, 10_u64),
        )
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].get::<u64, _>("user_id"), Some(user_id));

    let typed: Row = connection
        .exec_first(
            format!("SELECT birthday, elapsed, balance, profile FROM {users} WHERE id = ?"),
            (user_id,),
        )
        .unwrap()
        .expect("typed row");
    assert_eq!(
        typed.get::<MyValue, _>("birthday"),
        Some(MyValue::Date(2026, 7, 14, 0, 0, 0, 0))
    );
    assert_eq!(
        typed.get::<MyValue, _>("elapsed"),
        Some(MyValue::Time(true, 0, 2, 3, 4, 500_000))
    );
    assert_eq!(
        typed.get::<MyValue, _>("balance"),
        Some(MyValue::Bytes(b"12.34".to_vec()))
    );
    assert_eq!(
        typed.get::<MyValue, _>("profile"),
        Some(MyValue::Bytes(br#"{"role":"admin"}"#.to_vec()))
    );

    // Introspection forms used during schema diffing and model generation.
    let columns: Vec<Row> = connection
        .exec(
            "SELECT column_name, data_type, is_nullable, column_default, column_key, extra FROM information_schema.columns WHERE table_schema = DATABASE() AND table_name = ? ORDER BY ordinal_position",
            (&users,),
        )
        .unwrap();
    assert!(columns.len() >= 6);
    assert!(
        columns
            .iter()
            .any(|row| { row.get::<String, _>("column_name").as_deref() == Some("sort_order") })
    );
    let indexes: Vec<Row> = connection
        .query(format!("SHOW INDEX FROM {users}"))
        .unwrap();
    assert!(indexes.iter().any(|row| {
        row.get::<String, _>("Key_name").as_deref() == Some("idx_orm_email_prefix")
            && row.get::<u64, _>("Sub_part") == Some(16)
    }));
    let create_rows: Vec<Row> = connection
        .query(format!("SHOW CREATE TABLE {users}"))
        .unwrap();
    assert_eq!(create_rows.len(), 1);

    // Cascading cleanup is a common ORM relation test and verifies FK actions.
    connection
        .exec_drop(format!("DELETE FROM {users} WHERE id = ?"), (user_id,))
        .unwrap();
    let post_count: Option<u64> = connection
        .query_first(format!("SELECT COUNT(*) FROM {posts}"))
        .unwrap();
    assert_eq!(post_count, Some(0));

    connection
        .query_drop(format!("DROP TABLE IF EXISTS {posts}"))
        .unwrap();
    connection
        .query_drop(format!("DROP TABLE IF EXISTS {users}"))
        .unwrap();
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
    panic!("could not connect to MySqweel")
}
