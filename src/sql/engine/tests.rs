use super::{Engine, EngineConfig, UniqueMode};

#[test]
fn create_insert_select_alter_roundtrip() {
    let engine = Engine::default();

    engine
        .execute_sql(
            "CREATE TABLE users (id BIGINT PRIMARY KEY AUTO_INCREMENT, email VARCHAR(255), UNIQUE(email));",
        )
        .unwrap();

    engine
        .execute_sql("INSERT INTO users (email) VALUES ('a@example.com'), ('b@example.com');")
        .unwrap();

    let out = engine
        .execute_sql("SELECT id, email FROM users WHERE email = 'a@example.com';")
        .unwrap();
    assert_eq!(out[0].rows.len(), 1);

    engine
        .execute_sql("ALTER TABLE users ADD COLUMN display_name TEXT;")
        .unwrap();
}

#[test]
fn unique_enforce_mode() {
    let engine = Engine::new(EngineConfig {
        unique_mode: UniqueMode::Enforce,
        ..EngineConfig::default()
    });
    engine
        .execute_sql("CREATE TABLE users (id BIGINT PRIMARY KEY AUTO_INCREMENT, email VARCHAR(255), UNIQUE(email));")
        .unwrap();
    engine
        .execute_sql("INSERT INTO users (email) VALUES ('a@example.com');")
        .unwrap();
    let err = engine
        .execute_sql("INSERT INTO users (email) VALUES ('a@example.com');")
        .unwrap_err();
    assert!(err.to_string().contains("unique constraint"));
}
