mod common;

use common::{temp_lux_dir, test_lock};
use my_sqweel::sql::engine::{Engine, EngineConfig};

#[test]
fn snapshot_roundtrip_restores_rows() {
    let _guard = test_lock();
    let engine = Engine::default();
    engine
        .execute_sql(
            "CREATE TABLE users (id BIGINT PRIMARY KEY AUTO_INCREMENT, email VARCHAR(255));",
        )
        .unwrap();
    engine
        .execute_sql("INSERT INTO users (email) VALUES ('x@example.com');")
        .unwrap();

    let snap = engine.snapshot();

    engine
        .execute_sql("DELETE FROM users WHERE id = 1")
        .unwrap();
    let after_delete = engine.execute_sql("SELECT id FROM users").unwrap();
    assert_eq!(after_delete[0].rows.len(), 0);

    engine.restore_snapshot(snap);
    let restored = engine.execute_sql("SELECT id FROM users").unwrap();
    assert_eq!(restored[0].rows.len(), 1);
}

#[test]
fn reports_schema_drift() {
    let _guard = test_lock();
    let engine = Engine::default();
    engine
        .execute_sql("CREATE TABLE users (id BIGINT PRIMARY KEY AUTO_INCREMENT, email VARCHAR(255), name TEXT);")
        .unwrap();
    engine
        .execute_sql("INSERT INTO users (email, legacy) VALUES ('a@example.com', 'old-shape');")
        .unwrap();

    let report = engine.drift_report();
    let users = &report["tables"]["users"];
    assert_eq!(users["rowCount"].as_u64(), Some(1));
    assert_eq!(users["missingColumns"]["name"].as_u64(), Some(1));
    assert_eq!(users["extraColumns"]["legacy"].as_u64(), Some(1));
}

#[test]
fn queries_materialize_rows_against_current_schema_without_rewriting_storage() {
    let _guard = test_lock();
    let engine = Engine::default();
    engine
        .execute_sql("CREATE TABLE users (id BIGINT PRIMARY KEY AUTO_INCREMENT, age TEXT, active TEXT, profile TEXT, legacy TEXT);")
        .unwrap();
    engine
        .execute_sql("INSERT INTO users (age, active, profile, legacy) VALUES ('42', 'true', '{\"tier\":\"pro\"}', 'remove-me');")
        .unwrap();

    engine
        .execute_sql("ALTER TABLE users MODIFY COLUMN age BIGINT")
        .unwrap();
    engine
        .execute_sql("ALTER TABLE users MODIFY COLUMN active BOOLEAN")
        .unwrap();
    engine
        .execute_sql("ALTER TABLE users MODIFY COLUMN profile JSON")
        .unwrap();
    engine
        .execute_sql("ALTER TABLE users ADD COLUMN name TEXT DEFAULT 'anon'")
        .unwrap();
    engine
        .execute_sql("ALTER TABLE users DROP COLUMN legacy")
        .unwrap();

    let selected = engine.execute_sql("SELECT * FROM users").unwrap();
    let row = &selected[0].rows[0];
    assert_eq!(row.get("age").unwrap().as_i64(), Some(42));
    assert_eq!(row.get("active").unwrap().as_bool(), Some(true));
    assert_eq!(row["profile"]["tier"].as_str(), Some("pro"));
    assert_eq!(
        row.get("name").and_then(|value| value.as_str()),
        Some("anon")
    );
    assert!(!row.contains_key("legacy"));

    let dropped_column = engine.execute_sql("SELECT legacy FROM users").unwrap();
    assert!(dropped_column[0].rows[0].get("legacy").unwrap().is_null());

    let snapshot = engine.snapshot();
    let raw = snapshot
        .rows
        .get("users")
        .and_then(|rows| rows.values().next())
        .expect("raw stored row");
    assert_eq!(
        raw.data.get("age").and_then(|value| value.as_str()),
        Some("42")
    );
    assert_eq!(
        raw.data.get("legacy").and_then(|value| value.as_str()),
        Some("remove-me")
    );
    assert!(!raw.data.contains_key("name"));
}

#[test]
fn lux_storage_rehydrates_tables_and_rows_from_configured_directory() {
    let _guard = test_lock();
    let dir = temp_lux_dir("rehydrate");

    {
        let engine = Engine::open_with_data_dir(EngineConfig::default(), Some(&dir)).unwrap();
        engine
            .execute_sql(
                "CREATE TABLE users (id BIGINT PRIMARY KEY AUTO_INCREMENT, email VARCHAR(255));",
            )
            .unwrap();
        engine
            .execute_sql("INSERT INTO users (email) VALUES ('persisted@example.com');")
            .unwrap();
    }

    {
        let engine = Engine::open_with_data_dir(EngineConfig::default(), Some(&dir)).unwrap();
        let rows = engine
            .execute_sql("SELECT id, email FROM users WHERE id = 1")
            .unwrap();
        assert_eq!(rows[0].rows.len(), 1);
        assert_eq!(
            rows[0].rows[0].get("email").unwrap().as_str(),
            Some("persisted@example.com")
        );
    }

    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn lux_storage_rejects_second_open_of_configured_directory() {
    let _guard = test_lock();
    let dir = temp_lux_dir("lock");

    let engine = Engine::open_with_data_dir(EngineConfig::default(), Some(&dir)).unwrap();
    let err = match Engine::open_with_data_dir(EngineConfig::default(), Some(&dir)) {
        Ok(_) => panic!("second open should fail while the first engine is alive"),
        Err(err) => err,
    };
    assert!(err.to_string().contains("already open"));

    drop(engine);

    let reopened = Engine::open_with_data_dir(EngineConfig::default(), Some(&dir)).unwrap();
    drop(reopened);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn lux_storage_rehydrates_incremental_mutations() {
    let _guard = test_lock();
    let dir = temp_lux_dir("incremental");

    {
        let engine = Engine::open_with_data_dir(EngineConfig::default(), Some(&dir)).unwrap();
        engine
            .execute_sql("CREATE TABLE users (id BIGINT PRIMARY KEY AUTO_INCREMENT, email VARCHAR(255), name TEXT);")
            .unwrap();
        engine
            .execute_sql("CREATE INDEX idx_users_email ON users (email)")
            .unwrap();
        engine
            .execute_sql("INSERT INTO users (email, name) VALUES ('a@example.com', 'Alice'), ('b@example.com', 'Bob');")
            .unwrap();
        engine
            .execute_sql(
                "UPDATE users SET name = CONCAT(name, ' Updated') WHERE email = 'b@example.com'",
            )
            .unwrap();
        engine
            .execute_sql("DELETE FROM users WHERE email = 'a@example.com'")
            .unwrap();
    }

    {
        let engine = Engine::open_with_data_dir(EngineConfig::default(), Some(&dir)).unwrap();
        let rows = engine
            .execute_sql("SELECT email, name FROM users ORDER BY id")
            .unwrap();
        assert_eq!(rows[0].rows.len(), 1);
        assert_eq!(
            rows[0].rows[0].get("email").unwrap().as_str(),
            Some("b@example.com")
        );
        assert_eq!(
            rows[0].rows[0].get("name").unwrap().as_str(),
            Some("Bob Updated")
        );

        let stats = engine
            .execute_sql(
                "SELECT index_name FROM information_schema.statistics WHERE table_name = 'users'",
            )
            .unwrap();
        assert!(stats[0].rows.iter().any(|row| {
            row.get("index_name").and_then(|value| value.as_str()) == Some("idx_users_email")
        }));
    }

    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn lux_storage_does_not_rehydrate_rows_overwritten_by_unique_mode() {
    let _guard = test_lock();
    let dir = temp_lux_dir("unique-overwrite");

    {
        let engine = Engine::open_with_data_dir(EngineConfig::default(), Some(&dir)).unwrap();
        engine
            .execute_sql("CREATE TABLE users (id BIGINT PRIMARY KEY AUTO_INCREMENT, email VARCHAR(255) UNIQUE, name TEXT);")
            .unwrap();
        engine
            .execute_sql("INSERT INTO users (email, name) VALUES ('a@example.com', 'Alice'), ('a@example.com', 'Ada');")
            .unwrap();
    }

    {
        let engine = Engine::open_with_data_dir(EngineConfig::default(), Some(&dir)).unwrap();
        let rows = engine
            .execute_sql("SELECT email, name FROM users WHERE email = 'a@example.com'")
            .unwrap();
        assert_eq!(rows[0].rows.len(), 1);
        assert_eq!(
            rows[0].rows[0].get("name").and_then(|value| value.as_str()),
            Some("Ada")
        );
    }

    let _ = std::fs::remove_dir_all(dir);
}
