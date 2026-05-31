mod common;

use common::test_lock;
use my_sqweel::sql::engine::{Engine, EngineConfig, UniqueMode};

#[test]
fn unique_enforcement_mode_blocks_duplicates() {
    let _guard = test_lock();
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
    assert!(
        engine
            .execute_sql("INSERT INTO users (email) VALUES ('a@example.com');")
            .is_err()
    );
}

#[test]
fn unique_enforcement_allows_multiple_null_values() {
    let _guard = test_lock();
    let engine = Engine::new(EngineConfig {
        unique_mode: UniqueMode::Enforce,
        ..EngineConfig::default()
    });
    engine
        .execute_sql("CREATE TABLE users (id BIGINT PRIMARY KEY AUTO_INCREMENT, email VARCHAR(255), UNIQUE(email));")
        .unwrap();
    engine
        .execute_sql("INSERT INTO users (email) VALUES (NULL), (NULL);")
        .unwrap();

    let rows = engine.execute_sql("SELECT id, email FROM users").unwrap();
    assert_eq!(rows[0].rows.len(), 2);
    assert!(
        engine
            .execute_sql("INSERT INTO users (email) VALUES ('a@example.com'), ('a@example.com');")
            .is_err()
    );
}

#[test]
fn unique_overwrite_mode_replaces_conflicting_rows() {
    let _guard = test_lock();
    let engine = Engine::default();
    engine
        .execute_sql("CREATE TABLE users (id BIGINT PRIMARY KEY AUTO_INCREMENT, email VARCHAR(255), name TEXT, UNIQUE(email));")
        .unwrap();
    engine
        .execute_sql("INSERT INTO users (email, name) VALUES ('a@example.com', 'Alice');")
        .unwrap();
    engine
        .execute_sql("INSERT INTO users (email, name) VALUES ('a@example.com', 'Ada');")
        .unwrap();

    let rows = engine
        .execute_sql("SELECT email, name FROM users WHERE email = 'a@example.com'")
        .unwrap();
    assert_eq!(rows[0].rows.len(), 1);
    assert_eq!(
        rows[0].rows[0].get("name").and_then(|value| value.as_str()),
        Some("Ada")
    );

    engine
        .execute_sql("INSERT INTO users (email, name) VALUES ('b@example.com', 'Bob');")
        .unwrap();
    engine
        .execute_sql("UPDATE users SET email = 'a@example.com', name = 'Updated' WHERE email = 'b@example.com'")
        .unwrap();
    let rows = engine
        .execute_sql("SELECT email, name FROM users WHERE email = 'a@example.com'")
        .unwrap();
    assert_eq!(rows[0].rows.len(), 1);
    assert_eq!(
        rows[0].rows[0].get("name").and_then(|value| value.as_str()),
        Some("Updated")
    );
}

#[test]
fn supports_create_and_drop_index_metadata() {
    let _guard = test_lock();
    let engine = Engine::default();
    engine
        .execute_sql("CREATE TABLE users (id BIGINT PRIMARY KEY AUTO_INCREMENT, email VARCHAR(255), name TEXT);")
        .unwrap();
    engine
        .execute_sql("CREATE INDEX idx_users_email ON users (email)")
        .unwrap();

    let stats = engine
        .execute_sql("SELECT index_name, column_name, non_unique FROM information_schema.statistics WHERE table_name = 'users'")
        .unwrap();
    assert!(stats[0].rows.iter().any(|row| {
        row.get("index_name").and_then(|value| value.as_str()) == Some("PRIMARY")
            && row.get("column_name").and_then(|value| value.as_str()) == Some("id")
            && row.get("non_unique").and_then(|value| value.as_u64()) == Some(0)
    }));
    assert!(stats[0].rows.iter().any(|row| {
        row.get("index_name").and_then(|value| value.as_str()) == Some("idx_users_email")
            && row.get("column_name").and_then(|value| value.as_str()) == Some("email")
            && row.get("non_unique").and_then(|value| value.as_u64()) == Some(1)
    }));

    engine
        .execute_sql("DROP INDEX idx_users_email ON users")
        .unwrap();
    let after_drop = engine
        .execute_sql(
            "SELECT index_name FROM information_schema.statistics WHERE table_name = 'users'",
        )
        .unwrap();
    assert!(!after_drop[0].rows.iter().any(|row| {
        row.get("index_name").and_then(|value| value.as_str()) == Some("idx_users_email")
    }));
}

#[test]
fn duplicate_create_table_merges_existing_schema() {
    let _guard = test_lock();
    let engine = Engine::default();

    engine
        .execute_sql("CREATE TABLE users (id BIGINT PRIMARY KEY AUTO_INCREMENT, email TEXT);")
        .unwrap();
    engine
        .execute_sql("INSERT INTO users (email) VALUES ('a@example.com')")
        .unwrap();
    engine
        .execute_sql("CREATE TABLE users (id BIGINT PRIMARY KEY AUTO_INCREMENT, email TEXT UNIQUE, display_name TEXT DEFAULT 'anon');")
        .unwrap();

    let columns = engine.execute_sql("SHOW COLUMNS FROM users").unwrap();
    assert!(
        columns[0].rows.iter().any(|row| {
            row.get("Field").and_then(|value| value.as_str()) == Some("display_name")
        })
    );

    engine
        .execute_sql("INSERT INTO users (email) VALUES ('b@example.com')")
        .unwrap();
    let rows = engine
        .execute_sql("SELECT email, display_name FROM users ORDER BY id")
        .unwrap();
    assert_eq!(rows[0].rows.len(), 2);
    assert_eq!(
        rows[0].rows[1]
            .get("display_name")
            .and_then(|value| value.as_str()),
        Some("anon")
    );
}

#[test]
fn permissive_schema_ddl_infers_missing_metadata_and_noops_unknown_drops() {
    let _guard = test_lock();
    let engine = Engine::default();

    engine
        .execute_sql("ALTER TABLE missing_users ADD COLUMN email TEXT")
        .unwrap();
    engine
        .execute_sql("CREATE INDEX idx_missing_email ON missing_users (email)")
        .unwrap();
    let columns = engine
        .execute_sql("SHOW COLUMNS FROM missing_users")
        .unwrap();
    assert!(
        columns[0]
            .rows
            .iter()
            .any(|row| { row.get("Field").and_then(|value| value.as_str()) == Some("email") })
    );
    let indexes = engine.execute_sql("SHOW INDEX FROM missing_users").unwrap();
    assert!(indexes[0].rows.iter().any(|row| {
        row.get("Key_name").and_then(|value| value.as_str()) == Some("idx_missing_email")
    }));

    engine.execute_sql("TRUNCATE TABLE unknown_table").unwrap();
    engine.execute_sql("DROP TABLE unknown_table").unwrap();
    engine
        .execute_sql("DROP INDEX unknown_index ON missing_users")
        .unwrap();

    engine
        .execute_sql("ALTER TABLE missing_users SET TBLPROPERTIES ('vendor' = 'ignored')")
        .unwrap();
}

#[test]
fn insert_into_unknown_table_infers_schema_from_named_columns() {
    let _guard = test_lock();
    let engine = Engine::default();

    engine
        .execute_sql("INSERT INTO inferred_users (email, score) VALUES ('a@example.com', 10)")
        .unwrap();
    let rows = engine
        .execute_sql("SELECT email, score FROM inferred_users")
        .unwrap();
    assert_eq!(
        rows[0].rows[0]
            .get("email")
            .and_then(|value| value.as_str()),
        Some("a@example.com")
    );
    assert_eq!(
        rows[0].rows[0]
            .get("score")
            .and_then(|value| value.as_i64()),
        Some(10)
    );

    let columns = engine
        .execute_sql("SHOW COLUMNS FROM inferred_users")
        .unwrap();
    assert_eq!(columns[0].rows.len(), 2);
    assert!(
        columns[0]
            .rows
            .iter()
            .any(|row| { row.get("Field").and_then(|value| value.as_str()) == Some("email") })
    );
}

#[test]
fn positional_insert_into_unknown_table_generates_index_columns() {
    let _guard = test_lock();
    let engine = Engine::default();

    engine
        .execute_sql(
            "INSERT INTO positional_users VALUES ('a@example.com', 10), ('b@example.com', 20, true)",
        )
        .unwrap();

    let rows = engine
        .execute_sql("SELECT * FROM positional_users ORDER BY column_1")
        .unwrap();
    assert_eq!(rows[0].rows.len(), 2);
    assert_eq!(
        rows[0].rows[0]
            .get("column_1")
            .and_then(|value| value.as_str()),
        Some("a@example.com")
    );
    assert_eq!(
        rows[0].rows[0]
            .get("column_2")
            .and_then(|value| value.as_i64()),
        Some(10)
    );
    assert!(rows[0].rows[0].get("column_3").unwrap().is_null());
    assert_eq!(
        rows[0].rows[1]
            .get("column_3")
            .and_then(|value| value.as_bool()),
        Some(true)
    );

    let columns = engine
        .execute_sql("SHOW COLUMNS FROM positional_users")
        .unwrap();
    let fields = columns[0]
        .rows
        .iter()
        .map(|row| row.get("Field").and_then(|value| value.as_str()).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(fields, vec!["column_1", "column_2", "column_3"]);
}

#[test]
fn positional_insert_into_existing_table_uses_schema_order() {
    let _guard = test_lock();
    let engine = Engine::default();

    engine
        .execute_sql("CREATE TABLE ordered_users (id BIGINT PRIMARY KEY AUTO_INCREMENT, email TEXT, score BIGINT)")
        .unwrap();
    engine
        .execute_sql("INSERT INTO ordered_users VALUES (DEFAULT, 'a@example.com', 10)")
        .unwrap();

    let rows = engine
        .execute_sql("SELECT id, email, score FROM ordered_users")
        .unwrap();
    assert_eq!(
        rows[0].rows[0].get("id").and_then(|value| value.as_i64()),
        Some(1)
    );
    assert_eq!(
        rows[0].rows[0]
            .get("email")
            .and_then(|value| value.as_str()),
        Some("a@example.com")
    );
    assert_eq!(
        rows[0].rows[0]
            .get("score")
            .and_then(|value| value.as_i64()),
        Some(10)
    );
}

#[test]
fn supports_mysql_metadata_commands_and_database_ddl() {
    let _guard = test_lock();
    let engine = Engine::default();
    engine.execute_sql("CREATE DATABASE scratch").unwrap();
    engine
        .execute_sql("CREATE TABLE metadata_users (id BIGINT PRIMARY KEY AUTO_INCREMENT, email VARCHAR(255) NOT NULL, name TEXT DEFAULT 'anon');")
        .unwrap();
    engine
        .execute_sql("CREATE INDEX idx_metadata_users_email ON metadata_users (email)")
        .unwrap();

    let databases = engine.execute_sql("SHOW DATABASES").unwrap();
    assert!(
        databases[0]
            .rows
            .iter()
            .any(|row| { row.get("Database").and_then(|value| value.as_str()) == Some("app") })
    );

    let describe = engine.execute_sql("DESCRIBE metadata_users").unwrap();
    assert!(describe[0].rows.iter().any(|row| {
        row.get("Field").and_then(|value| value.as_str()) == Some("email")
            && row.get("Null").and_then(|value| value.as_str()) == Some("NO")
    }));

    let show_columns = engine
        .execute_sql("SHOW COLUMNS FROM metadata_users")
        .unwrap();
    assert_eq!(show_columns[0].rows.len(), describe[0].rows.len());

    let show_index = engine
        .execute_sql("SHOW INDEX FROM metadata_users")
        .unwrap();
    assert!(show_index[0].rows.iter().any(|row| {
        row.get("Key_name").and_then(|value| value.as_str()) == Some("idx_metadata_users_email")
    }));

    let show_create = engine
        .execute_sql("SHOW CREATE TABLE metadata_users")
        .unwrap();
    assert!(
        show_create[0].rows[0]
            .get("Create Table")
            .and_then(|value| value.as_str())
            .unwrap()
            .contains("CREATE TABLE `metadata_users`")
    );

    engine
        .execute_sql("RENAME TABLE metadata_users TO renamed_metadata_users")
        .unwrap();
    let after_rename = engine
        .execute_sql("SHOW COLUMNS FROM renamed_metadata_users")
        .unwrap();
    assert!(!after_rename[0].rows.is_empty());

    engine.execute_sql("DROP DATABASE scratch").unwrap();
}

#[test]
fn information_schema_filters_do_not_match_unsupported_predicates_by_default() {
    let _guard = test_lock();
    let engine = Engine::default();
    engine
        .execute_sql("CREATE TABLE users (id BIGINT PRIMARY KEY AUTO_INCREMENT);")
        .unwrap();

    let rows = engine
        .execute_sql(
            "SELECT table_name FROM information_schema.tables WHERE NOT (table_name = 'users')",
        )
        .unwrap();
    assert!(rows[0].rows.is_empty());
}

#[test]
fn captures_advisory_foreign_key_metadata() {
    let _guard = test_lock();
    let engine = Engine::default();
    engine
        .execute_sql("CREATE TABLE parents (id BIGINT PRIMARY KEY AUTO_INCREMENT);")
        .unwrap();
    engine
        .execute_sql("CREATE TABLE children (id BIGINT PRIMARY KEY AUTO_INCREMENT, parent_id BIGINT, CONSTRAINT fk_children_parent FOREIGN KEY (parent_id) REFERENCES parents (id) ON DELETE CASCADE ON UPDATE RESTRICT);")
        .unwrap();

    let constraints = engine
        .execute_sql("SELECT table_name, constraint_name, constraint_type FROM information_schema.table_constraints WHERE table_name = 'children'")
        .unwrap();
    assert!(constraints[0].rows.iter().any(|row| {
        row.get("constraint_name").and_then(|value| value.as_str()) == Some("fk_children_parent")
            && row.get("constraint_type").and_then(|value| value.as_str()) == Some("FOREIGN KEY")
    }));

    let usage = engine
        .execute_sql("SELECT column_name, referenced_table_name, referenced_column_name FROM information_schema.key_column_usage WHERE constraint_name = 'fk_children_parent'")
        .unwrap();
    assert_eq!(
        usage[0].rows[0]
            .get("referenced_table_name")
            .and_then(|v| v.as_str()),
        Some("parents")
    );
    assert_eq!(
        usage[0].rows[0]
            .get("referenced_column_name")
            .and_then(|v| v.as_str()),
        Some("id")
    );

    let referential = engine
        .execute_sql("SELECT constraint_name, delete_rule, update_rule FROM information_schema.referential_constraints WHERE constraint_name = 'fk_children_parent'")
        .unwrap();
    assert_eq!(
        referential[0].rows[0]
            .get("delete_rule")
            .and_then(|v| v.as_str()),
        Some("CASCADE")
    );
    assert_eq!(
        referential[0].rows[0]
            .get("update_rule")
            .and_then(|v| v.as_str()),
        Some("RESTRICT")
    );
}

#[test]
fn applies_type_defaults_and_null_semantics() {
    let _guard = test_lock();
    let engine = Engine::default();
    engine
        .execute_sql("CREATE TABLE typed_rows (id BIGINT PRIMARY KEY AUTO_INCREMENT, age BIGINT NOT NULL DEFAULT 7, active BOOLEAN DEFAULT TRUE, label TEXT NOT NULL);")
        .unwrap();

    let missing_required = engine.execute_sql("INSERT INTO typed_rows (age) VALUES (1)");
    assert!(missing_required.is_err());

    engine
        .execute_sql("INSERT INTO typed_rows (age, active, label) VALUES (DEFAULT, '0', 'ok');")
        .unwrap();
    let row = engine
        .execute_sql("SELECT age, active, label FROM typed_rows WHERE label != NULL")
        .unwrap();
    assert_eq!(row[0].rows.len(), 0);

    let row = engine
        .execute_sql("SELECT age, active FROM typed_rows WHERE label IS NOT NULL")
        .unwrap();
    assert_eq!(row[0].rows[0].get("age").unwrap().as_i64(), Some(7));
    assert_eq!(row[0].rows[0].get("active").unwrap().as_bool(), Some(false));
}

#[test]
fn supports_mysql_insert_modes() {
    let _guard = test_lock();
    let engine = Engine::default();
    engine
        .execute_sql("CREATE TABLE users (id BIGINT PRIMARY KEY AUTO_INCREMENT, email VARCHAR(255) UNIQUE, name TEXT);")
        .unwrap();

    engine
        .execute_sql("INSERT INTO users (email, name) VALUES ('a@example.com', 'Alice');")
        .unwrap();
    let ignored = engine
        .execute_sql("INSERT IGNORE INTO users (email, name) VALUES ('a@example.com', 'Ignored');")
        .unwrap();
    assert_eq!(ignored[0].rows_affected, 0);

    let after_ignore = engine
        .execute_sql("SELECT name FROM users WHERE email = 'a@example.com'")
        .unwrap();
    assert_eq!(
        after_ignore[0].rows[0]
            .get("name")
            .unwrap()
            .as_str()
            .unwrap(),
        "Alice"
    );

    engine
        .execute_sql("INSERT INTO users (email, name) VALUES ('a@example.com', 'Updated') ON DUPLICATE KEY UPDATE name = VALUES(name);")
        .unwrap();
    let after_upsert = engine
        .execute_sql("SELECT name FROM users WHERE email = 'a@example.com'")
        .unwrap();
    assert_eq!(
        after_upsert[0].rows[0]
            .get("name")
            .unwrap()
            .as_str()
            .unwrap(),
        "Updated"
    );

    engine
        .execute_sql("REPLACE INTO users (email, name) VALUES ('a@example.com', 'Replaced');")
        .unwrap();
    let after_replace = engine
        .execute_sql("SELECT name FROM users WHERE email = 'a@example.com'")
        .unwrap();
    assert_eq!(
        after_replace[0].rows[0]
            .get("name")
            .unwrap()
            .as_str()
            .unwrap(),
        "Replaced"
    );
}

#[test]
fn exposes_richer_information_schema_columns() {
    let _guard = test_lock();
    let engine = Engine::default();
    engine
        .execute_sql("CREATE TABLE users (id BIGINT PRIMARY KEY AUTO_INCREMENT, email VARCHAR(255) UNIQUE NOT NULL DEFAULT 'x@example.com');")
        .unwrap();

    let info = engine
        .execute_sql("SELECT column_name, ordinal_position, is_nullable, column_default, column_type, column_key, extra FROM information_schema.columns WHERE table_name = 'users'")
        .unwrap();

    let id = info[0]
        .rows
        .iter()
        .find(|row| row.get("column_name").and_then(|v| v.as_str()) == Some("id"))
        .expect("id column");
    assert_eq!(id.get("column_key").unwrap().as_str().unwrap(), "PRI");
    assert_eq!(id.get("extra").unwrap().as_str().unwrap(), "auto_increment");

    let email = info[0]
        .rows
        .iter()
        .find(|row| row.get("column_name").and_then(|v| v.as_str()) == Some("email"))
        .expect("email column");
    assert_eq!(email.get("is_nullable").unwrap().as_str().unwrap(), "NO");
    assert_eq!(email.get("column_key").unwrap().as_str().unwrap(), "UNI");
}

#[test]
fn supports_alter_table_metadata_expansion_and_more_introspection() {
    let _guard = test_lock();
    let engine = Engine::default();
    engine
        .execute_sql("CREATE TABLE users (id BIGINT PRIMARY KEY AUTO_INCREMENT, email VARCHAR(255) UNIQUE, legacy TEXT);")
        .unwrap();

    engine
        .execute_sql("ALTER TABLE users ADD COLUMN display_name TEXT")
        .unwrap();
    engine
        .execute_sql("ALTER TABLE users RENAME COLUMN display_name TO handle")
        .unwrap();
    engine
        .execute_sql("ALTER TABLE users DROP COLUMN legacy")
        .unwrap();
    engine
        .execute_sql("ALTER TABLE users MODIFY COLUMN handle VARCHAR(128) NOT NULL")
        .unwrap();

    let cols = engine
        .execute_sql("SELECT column_name, column_type, is_nullable FROM information_schema.columns WHERE table_name = 'users'")
        .unwrap();
    assert!(cols[0].rows.iter().any(|row| {
        row.get("column_name").and_then(|v| v.as_str()) == Some("handle")
            && row.get("column_type").and_then(|v| v.as_str()) == Some("VARCHAR(128)")
            && row.get("is_nullable").and_then(|v| v.as_str()) == Some("NO")
    }));
    assert!(
        !cols[0]
            .rows
            .iter()
            .any(|row| row.get("column_name").and_then(|v| v.as_str()) == Some("legacy"))
    );

    let schemata = engine
        .execute_sql(
            "SELECT schema_name FROM information_schema.schemata WHERE schema_name = 'app'",
        )
        .unwrap();
    assert_eq!(schemata[0].rows.len(), 1);

    let constraints = engine
        .execute_sql("SELECT table_name, constraint_type FROM information_schema.table_constraints WHERE table_name = 'users'")
        .unwrap();
    assert!(
        constraints[0].rows.iter().any(|row| {
            row.get("constraint_type").and_then(|v| v.as_str()) == Some("PRIMARY KEY")
        })
    );

    let show = engine.execute_sql("SHOW TABLES").unwrap();
    assert!(
        show[0]
            .rows
            .iter()
            .any(|row| { row.get("Tables_in_app").and_then(|v| v.as_str()) == Some("users") })
    );
}
