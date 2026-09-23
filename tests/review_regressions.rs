use my_sqweel::{
    AsyncEngine,
    sql::engine::{Engine, EngineConfig},
    storage::LuxStorage,
};
use serde_json::json;

fn foreign_keys(engine: &Engine) {
    engine.execute_sql("CREATE TABLE parent (id INT PRIMARY KEY); CREATE TABLE child (id INT PRIMARY KEY, parent_id INT, CONSTRAINT fk_parent FOREIGN KEY (parent_id) REFERENCES parent(id))").unwrap();
}

#[test]
fn read_only_user_cannot_drop_foreign_key() {
    let engine = Engine::default();
    foreign_keys(&engine);
    engine
        .execute_sql(
            "CREATE USER 'reader'@'%' IDENTIFIED BY ''; GRANT SELECT ON app.* TO 'reader'@'%'",
        )
        .unwrap();
    let mut reader = engine.session();
    assert!(reader.authenticate("reader", &[1; 20], &[]));
    assert!(
        reader
            .execute_sql("ALTER TABLE child ADD COLUMN forbidden INT")
            .is_err()
    );
    let result = reader.execute_sql("ALTER TABLE child DROP FOREIGN KEY fk_parent");
    assert!(result.is_err(), "SELECT-only user dropped FK: {result:?}");
}

#[test]
fn nonexistent_fk_name_must_not_drop_existing_constraint() {
    let engine = Engine::default();
    foreign_keys(&engine);
    let result = engine.execute_sql("ALTER TABLE child DROP FOREIGN KEY does_not_exist");
    assert!(result.is_err(), "wrong name succeeded: {result:?}");
    assert!(
        engine
            .execute_sql("INSERT INTO child VALUES (1, 99)")
            .is_err()
    );
    assert!(
        engine
            .execute_sql("ALTER TABLE child DROP FOREIGN KEY fk_parent, DROP FOREIGN KEY missing")
            .is_err()
    );
    assert!(
        engine
            .execute_sql("INSERT INTO child VALUES (1, 99)")
            .is_err()
    );
    engine.execute_sql("ALTER TABLE `app`.`child` DROP FOREIGN KEY `fk_parent`; INSERT INTO child VALUES (1, 99)").unwrap();
}

#[test]
fn temporary_tables_are_session_local() {
    let engine = Engine::default();
    let mut first = engine.session();
    first
        .execute_sql(
            "CREATE TEMPORARY TABLE scratch (id INT PRIMARY KEY); INSERT INTO scratch VALUES (7)",
        )
        .unwrap();
    let result = engine.session().execute_sql("SELECT * FROM scratch");
    assert!(result.is_err(), "other session sees temp rows: {result:?}");
}

#[test]
fn placeholders_in_comments_are_not_parameters() {
    let engine = Engine::default();
    let result = engine.execute_sql_with_params("SELECT ? AS actual /* why? */", &[json!(7)]);
    assert!(result.is_ok(), "valid query rejected: {result:?}");
}

#[test]
fn semicolon_in_comment_does_not_execute_commented_statement() {
    let engine = Engine::default();
    engine
        .execute_sql("CREATE TABLE items (id INT PRIMARY KEY); INSERT INTO items VALUES (1)")
        .unwrap();
    let result = engine.execute_sql("SELECT 1 -- comment; DELETE FROM items;\n;SELECT 2");
    assert!(result.is_ok(), "valid comment rejected: {result:?}");
    let results = engine.execute_sql("SELECT * FROM items").unwrap();
    assert_eq!(results[0].rows.len(), 1, "commented DELETE executed");
}

#[test]
fn successful_prefix_of_failed_async_batch_is_persisted() {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let storage = LuxStorage::open(None).await.unwrap();
            let engine = AsyncEngine::open(EngineConfig::default(), storage.clone())
                .await
                .unwrap();
            engine
                .execute_sql("CREATE TABLE items (id INT PRIMARY KEY)")
                .await
                .unwrap();
            assert!(
                engine
                    .execute_sql("INSERT INTO items VALUES (1); INSERT INTO items VALUES (1)")
                    .await
                    .is_err()
            );
            assert_eq!(
                engine.execute_sql("SELECT * FROM items").await.unwrap()[0]
                    .rows
                    .len(),
                1
            );
            let session = engine.session();
            assert!(
                session
                    .execute_sql("INSERT INTO items VALUES (2); INSERT INTO items VALUES (2)")
                    .await
                    .is_err()
            );
            drop(session);
            drop(engine);
            let reopened = AsyncEngine::open(EngineConfig::default(), storage)
                .await
                .unwrap();
            assert_eq!(
                reopened.execute_sql("SELECT * FROM items").await.unwrap()[0]
                    .rows
                    .len(),
                2,
                "committed prefix was lost on reopen"
            );
        });
}

#[test]
fn ctas_enforces_declared_primary_key() {
    let engine = Engine::default();
    engine
        .execute_sql("CREATE TABLE source (id INT); INSERT INTO source VALUES (10), (10)")
        .unwrap();
    let result =
        engine.execute_sql("CREATE TABLE copied (id INT PRIMARY KEY) AS SELECT id FROM source");
    assert!(
        result.is_err(),
        "CTAS accepted duplicate primary keys: {result:?}"
    );
    engine
        .execute_sql(
            "CREATE TEMPORARY TABLE grouped AS SELECT id, SUM(id) AS total FROM source GROUP BY id",
        )
        .unwrap();
    let rows = engine.execute_sql("SELECT id, total FROM grouped").unwrap();
    assert_eq!(rows[0].rows[0]["total"], json!("20"));
}

fn with_wire(test: impl FnOnce(&mut mysql::Conn)) {
    use std::net::TcpListener;
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let worker_stop = stop.clone();
    let worker = std::thread::spawn(move || {
        my_sqweel::server::WireServer::new(Arc::new(Engine::default()))
            .serve_listener_until(listener, worker_stop)
            .unwrap();
    });
    let mut conn = mysql::Conn::new(
        mysql::OptsBuilder::new()
            .ip_or_hostname(Some("127.0.0.1"))
            .tcp_port(address.port())
            .user(Some("root"))
            .db_name(Some("app")),
    )
    .unwrap();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| test(&mut conn)));
    drop(conn);
    stop.store(true, Ordering::Relaxed);
    worker.join().unwrap();
    if let Err(payload) = result {
        std::panic::resume_unwind(payload);
    }
}

#[test]
fn wire_binary_parameters_preserve_bytes() {
    use mysql::prelude::Queryable;
    with_wire(|conn| {
        conn.query_drop("CREATE TABLE blobs (id INT PRIMARY KEY, payload BLOB)")
            .unwrap();
        conn.exec_drop("INSERT INTO blobs VALUES (?, ?)", (1, vec![0_u8, 128, 255]))
            .unwrap();
        let actual: Option<String> = conn.query_first("SELECT HEX(payload) FROM blobs").unwrap();
        assert_eq!(actual.as_deref(), Some("0080FF"));
        let bytes: Option<Vec<u8>> = conn
            .exec_first("SELECT payload FROM blobs WHERE id = ? /* ? */", (1,))
            .unwrap();
        assert_eq!(bytes, Some(vec![0, 128, 255]));
        let bytes: Option<Vec<u8>> = conn.query_first("SELECT payload FROM blobs").unwrap();
        assert_eq!(bytes, Some(vec![0, 128, 255]));
        let value: Option<i32> = conn
            .exec_first("SELECT ? AS `literal?` /* ? */", (7,))
            .unwrap();
        assert_eq!(value, Some(7));
    });
}

#[test]
fn wire_system_variable_projection_respects_where() {
    use mysql::prelude::Queryable;
    with_wire(|conn| {
        let result: Vec<mysql::Row> = conn.query("SELECT @@autocommit WHERE 1 = 0").unwrap();
        assert!(result.is_empty(), "WHERE ignored: {result:?}");
        let identity: Option<(String, String)> =
            conn.query_first("SELECT USER(), CURRENT_USER()").unwrap();
        assert_eq!(identity, Some(("root@127.0.0.1".into(), "root@%".into())));
    });
}

#[test]
fn binary_literals_remain_bytes_and_character_columns_return_text() {
    use mysql::prelude::Queryable;
    with_wire(|conn| {
        conn.query_drop("CREATE TABLE latin_text (a VARCHAR(200) CHARACTER SET latin1)")
            .unwrap();
        conn.query_drop("INSERT INTO latin_text VALUES (UNHEX('22CA22'))")
            .unwrap();
        let text: Option<(String, String)> = conn
            .query_first("SELECT a, HEX(a) FROM latin_text")
            .unwrap();
        assert_eq!(text, Some(("\"Ê\"".into(), "22CA22".into())));
        let text: Option<String> = conn
            .exec_first(
                "SELECT a FROM latin_text WHERE a IS NOT NULL AND ? = 1",
                (1,),
            )
            .unwrap();
        assert_eq!(text.as_deref(), Some("\"Ê\""));
        let bytes: Option<Vec<u8>> = conn.query_first("SELECT UNHEX('22CA22')").unwrap();
        assert_eq!(bytes, Some(vec![0x22, 0xCA, 0x22]));
        let bytes: Option<Vec<u8>> = conn
            .exec_first("SELECT ?", (vec![0_u8, 128, 255],))
            .unwrap();
        assert_eq!(bytes, Some(vec![0, 128, 255]));
    });
}

#[test]
fn create_or_replace_preserves_a_shadowing_temporary_table() {
    let engine = Engine::default();
    let mut session = engine.session();
    session.execute_sql("CREATE TEMPORARY TABLE t (i INT); CREATE OR REPLACE TABLE t AS SELECT * FROM t; DROP TEMPORARY TABLE t; DROP TABLE t").unwrap();
    engine
        .execute_sql("CREATE TABLE t (i INT); INSERT INTO t VALUES (1)")
        .unwrap();
    session.execute_sql("CREATE TEMPORARY TABLE t (i INT); INSERT INTO t VALUES (7); CREATE OR REPLACE TABLE t AS SELECT i + 1 AS i FROM t").unwrap();
    assert_eq!(
        session.execute_sql("SELECT i FROM t").unwrap()[0].rows[0]["i"],
        json!(7)
    );
    assert_eq!(
        engine.execute_sql("SELECT i FROM t").unwrap()[0].rows[0]["i"],
        json!(8)
    );
    assert!(!engine.snapshot().schemas["t"].temporary);
    session.execute_sql("DROP TEMPORARY TABLE t").unwrap();
    assert_eq!(
        session.execute_sql("SELECT i FROM t").unwrap()[0].rows[0]["i"],
        json!(8)
    );
    session.execute_sql("DROP TABLE t").unwrap();
    assert!(engine.execute_sql("SELECT * FROM t").is_err());
}

#[test]
fn temporary_tables_shadow_permanent_tables_and_follow_transactions() {
    let engine = Engine::default();
    engine
        .execute_sql("CREATE TABLE scratch (id INT PRIMARY KEY); INSERT INTO scratch VALUES (1)")
        .unwrap();
    let mut first = engine.session();
    first
        .execute_sql(
            "CREATE TEMPORARY TABLE scratch (id INT PRIMARY KEY); INSERT INTO scratch VALUES (7)",
        )
        .unwrap();
    let ids = |session: &mut my_sqweel::sql::engine::EngineSession| {
        session
            .execute_sql("SELECT id FROM scratch ORDER BY id")
            .unwrap()[0]
            .rows
            .iter()
            .map(|r| r["id"].clone())
            .collect::<Vec<_>>()
    };
    assert_eq!(ids(&mut first), vec![json!(7)]);
    assert_eq!(ids(&mut engine.session()), vec![json!(1)]);
    assert!(!engine.snapshot().schemas["scratch"].temporary);
    first.execute_sql("BEGIN; SELECT COUNT(*) FROM scratch; INSERT INTO scratch VALUES (8); SAVEPOINT saved; INSERT INTO scratch VALUES (9); ROLLBACK TO saved; COMMIT").unwrap();
    assert_eq!(ids(&mut first), vec![json!(7), json!(8)]);
    first
        .execute_sql("BEGIN; DELETE FROM scratch; ROLLBACK")
        .unwrap();
    assert_eq!(ids(&mut first), vec![json!(7), json!(8)]);
    first.execute_sql("DROP TEMPORARY TABLE scratch").unwrap();
    assert_eq!(ids(&mut first), vec![json!(1)]);
    first
        .execute_sql("DROP TEMPORARY TABLE IF EXISTS scratch")
        .unwrap();
    assert_eq!(ids(&mut first), vec![json!(1)]);
    first
        .execute_sql("CREATE TEMPORARY TABLE local_only (id INT)")
        .unwrap();
    drop(first);
    assert!(engine.execute_sql("SELECT * FROM local_only").is_err());
}

#[test]
fn temporary_tables_are_not_persisted_and_are_database_scoped() {
    let dir = tempfile::tempdir().unwrap();
    {
        let engine =
            Engine::open_with_data_dir(EngineConfig::default(), dir.path().to_str()).unwrap();
        let mut session = engine.session();
        session.execute_sql("CREATE DATABASE other; CREATE TEMPORARY TABLE scratch (id INT); INSERT INTO scratch VALUES (1); USE other").unwrap();
        assert!(session.execute_sql("SELECT * FROM scratch").is_err());
        session
            .execute_sql(
                "CREATE TEMPORARY TABLE scratch (id INT); INSERT INTO scratch VALUES (2); USE app",
            )
            .unwrap();
        assert_eq!(
            session.execute_sql("SELECT id FROM scratch").unwrap()[0].rows[0]["id"],
            json!(1)
        );
    }
    let reopened =
        Engine::open_with_data_dir(EngineConfig::default(), dir.path().to_str()).unwrap();
    assert!(reopened.execute_sql("SELECT * FROM scratch").is_err());
    assert!(
        reopened
            .execute_sql("USE other; SELECT * FROM scratch")
            .is_err()
    );
}

#[test]
fn token_boundaries_preserve_literals_and_quoted_identifiers() {
    let engine = Engine::default();
    let results = engine.execute_sql_with_params("/* ? ; */ SELECT ? AS `é?;`, 'it\\'s ?; fine' AS literal -- ?; DELETE\r\n; # ?;\nSELECT ? AS second", &[json!("é\\'"), json!(9)]).unwrap();
    assert_eq!(results.len(), 2);
    assert_eq!(results[0].rows[0]["é?;"], json!("é\\'"));
    assert_eq!(results[0].rows[0]["literal"], json!("it's ?; fine"));
    assert_eq!(results[1].rows[0]["second"], json!(9));
    assert!(engine.execute_sql("SELECT 1; /* unterminated").is_err());
}

#[test]
fn identity_and_system_variables_follow_the_session() {
    let engine = Engine::default();
    engine.execute_sql("CREATE USER 'reader'@'%' IDENTIFIED BY ''; GRANT SELECT ON app.* TO 'reader'@'%'; CREATE TABLE items (id INT); INSERT INTO items VALUES (1), (2)").unwrap();
    let mut reader = engine.session();
    assert!(reader.authenticate("reader", &[1; 20], &[]));
    let result = reader
        .execute_sql(
            "SELECT USER() AS u, SESSION_USER() AS s, SYSTEM_USER() AS a, CURRENT_USER() AS c",
        )
        .unwrap();
    assert_eq!(result[0].rows[0]["u"], json!("reader@localhost"));
    assert_eq!(result[0].rows[0]["s"], result[0].rows[0]["u"]);
    assert_eq!(result[0].rows[0]["a"], result[0].rows[0]["u"]);
    assert_eq!(result[0].rows[0]["c"], json!("reader@%"));
    reader
        .execute_sql("SET autocommit = 0; SET time_zone = '+02:00'")
        .unwrap();
    let rows = reader.execute_sql("SELECT id, @@autocommit AS a, @@time_zone AS z FROM items WHERE id = 2 AND @@autocommit = 0").unwrap();
    assert_eq!(rows[0].rows.len(), 1);
    assert_eq!(rows[0].rows[0]["a"], json!(0));
    assert_eq!(rows[0].rows[0]["z"], json!("+02:00"));
    assert!(
        reader.execute_sql("SELECT @@autocommit LIMIT 0").unwrap()[0]
            .rows
            .is_empty()
    );
    assert_eq!(
        engine.execute_sql("SELECT @@autocommit AS a").unwrap()[0].rows[0]["a"],
        json!(1)
    );
}
