use my_sqweel::sql::engine::{Engine, EngineConfig, EngineSession};
use serde_json::json;

fn authenticate(engine: &Engine, username: &str, password: &str) -> EngineSession {
    let mut session = engine.session();
    let salt = b"01234567890123456789";
    let first = sha1_smol::Sha1::from(password).digest().bytes();
    let second = sha1_smol::Sha1::from(&first).digest().bytes();
    let mut challenge = sha1_smol::Sha1::new();
    challenge.update(salt);
    challenge.update(&second);
    let mask = challenge.digest().bytes();
    let response: [u8; 20] = std::array::from_fn(|index| first[index] ^ mask[index]);
    assert!(session.authenticate(username, salt, &response));
    session
}

fn provision(engine: &Engine) {
    engine.execute_sql("CREATE DATABASE shard_a; CREATE DATABASE shard_b; CREATE USER 'tenant'@'%' IDENTIFIED BY 'secret'; GRANT SELECT,INSERT,UPDATE,DELETE ON shard_a.* TO 'tenant'@'%'").unwrap();
    for (database, value) in [("app", 10), ("shard_a", 20), ("shard_b", 30)] {
        let mut session = engine.session();
        session.use_database(database).unwrap();
        session
            .execute_sql("CREATE TABLE items (id INT PRIMARY KEY, amount INT)")
            .unwrap();
        session
            .execute_sql(&format!("INSERT INTO items VALUES (1, {value})"))
            .unwrap();
    }
}

#[test]
fn same_table_names_are_independent_and_qualified_nested_sql_cannot_escape() {
    let engine = Engine::new(EngineConfig::mysql_strict());
    provision(&engine);
    let mut tenant = authenticate(&engine, "tenant", "secret");
    tenant.use_database("shard_a").unwrap();
    assert_eq!(
        tenant
            .execute_sql("SELECT amount FROM shard_a.items")
            .unwrap()[0]
            .rows[0]["amount"],
        json!(20)
    );
    for forbidden in [
        "SELECT * FROM shard_b.items",
        "SELECT * FROM items WHERE id IN (SELECT id FROM shard_b.items)",
        "WITH secret AS (SELECT id FROM shard_b.items) SELECT * FROM secret",
        "UPDATE items SET amount = (SELECT amount FROM shard_b.items WHERE id = 1)",
        "DELETE FROM items WHERE id IN (SELECT id FROM shard_b.items)",
        "INSERT INTO items SELECT * FROM shard_b.items",
        "SELECT shard_b.items.amount FROM items",
        "CREATE TABLE stolen (id INT)",
        "DROP TABLE items",
        "CREATE DATABASE stolen",
        "GRANT SELECT ON shard_b.* TO 'tenant'@'%'",
        "USE shard_b",
    ] {
        assert!(
            tenant.execute_sql(forbidden).is_err(),
            "allowed {forbidden}"
        );
    }
    tenant
        .execute_sql("UPDATE items SET amount = 21 WHERE id = 1")
        .unwrap();
    for (database, value) in [("app", 10), ("shard_a", 21), ("shard_b", 30)] {
        let mut session = engine.session();
        session.use_database(database).unwrap();
        assert_eq!(
            session.execute_sql("SELECT amount FROM items").unwrap()[0].rows[0]["amount"],
            json!(value)
        );
    }
}

#[test]
fn cross_database_schema_references_are_rejected_even_for_admin() {
    let engine = Engine::new(EngineConfig::mysql_strict());
    provision(&engine);
    let mut admin = engine.session();
    admin.use_database("shard_a").unwrap();
    for forbidden in [
        "CREATE TABLE children (id INT PRIMARY KEY, parent_id INT REFERENCES shard_b.items(id))",
        "CREATE TABLE children (id INT PRIMARY KEY, parent_id INT, FOREIGN KEY (parent_id) REFERENCES shard_b.items(id))",
        "ALTER TABLE items ADD CONSTRAINT foreign_parent FOREIGN KEY (id) REFERENCES shard_b.items(id)",
        "CREATE TABLE copy LIKE shard_b.items",
        "ALTER TABLE items RENAME TO shard_b.renamed",
        "DROP TABLE shard_b.items",
    ] {
        assert!(admin.execute_sql(forbidden).is_err(), "allowed {forbidden}");
    }
    admin.execute_sql("CREATE TABLE children (id INT PRIMARY KEY, parent_id INT, FOREIGN KEY (parent_id) REFERENCES shard_a.items(id))").unwrap();
    admin
        .execute_sql("INSERT INTO children VALUES (1, 1)")
        .unwrap();
}

#[test]
fn revocation_is_immediate_for_existing_connections_and_partial_grants_are_enforced() {
    let engine = Engine::new(EngineConfig::mysql_strict());
    provision(&engine);
    let mut tenant = authenticate(&engine, "tenant", "secret");
    tenant.use_database("shard_a").unwrap();
    engine
        .execute_sql("REVOKE ALL PRIVILEGES, GRANT OPTION FROM 'tenant'@'%'")
        .unwrap();
    assert!(tenant.execute_sql("SELECT * FROM items").is_err());
    engine
        .execute_sql("GRANT SELECT ON shard_a.* TO 'tenant'@'%'")
        .unwrap();
    tenant.execute_sql("SELECT * FROM items").unwrap();
    assert!(tenant.execute_sql("UPDATE items SET amount = 99").is_err());
    engine.execute_sql("DROP USER 'tenant'@'%'").unwrap();
    assert!(tenant.execute_sql("SELECT * FROM items").is_err());
    engine.execute_sql("CREATE USER 'tenant'@'%' IDENTIFIED BY 'replacement'; GRANT SELECT ON shard_a.* TO 'tenant'@'%'").unwrap();
    assert!(tenant.execute_sql("SELECT * FROM items").is_err());
    let mut replacement = authenticate(&engine, "tenant", "replacement");
    replacement.use_database("shard_a").unwrap();
    replacement.execute_sql("SELECT * FROM items").unwrap();
}

#[test]
fn restricted_connections_cannot_disable_constraints_or_set_global_options() {
    let engine = Engine::new(EngineConfig::mysql_strict());
    provision(&engine);
    let mut tenant = authenticate(&engine, "tenant", "secret");
    tenant.use_database("shard_a").unwrap();
    for forbidden in [
        "SET GLOBAL autocommit = 0",
        "SET @@global.autocommit = 0",
        "SET FOREIGN_KEY_CHECKS = 0",
        "SET UNIQUE_CHECKS = 0",
    ] {
        assert!(
            tenant.execute_sql(forbidden).is_err(),
            "allowed {forbidden}"
        );
    }
    tenant.execute_sql("BEGIN; SAVEPOINT before_update; UPDATE items SET amount = 99; ROLLBACK TO SAVEPOINT before_update; RELEASE SAVEPOINT before_update; COMMIT").unwrap();
    assert_eq!(
        tenant.execute_sql("SELECT amount FROM items").unwrap()[0].rows[0]["amount"],
        json!(20)
    );
}

struct Directory(std::path::PathBuf);
impl Directory {
    fn new() -> Self {
        Self(std::env::temp_dir().join(format!("sqweel-catalog-{}", uuid::Uuid::new_v4())))
    }
    fn open(&self) -> Engine {
        Engine::open_with_data_dir(EngineConfig::mysql_strict(), Some(self.0.to_str().unwrap()))
            .unwrap()
    }
}
impl Drop for Directory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn credentials_grants_and_databases_survive_reopen_without_resurrecting_dropped_grants() {
    let directory = Directory::new();
    {
        let engine = directory.open();
        provision(&engine);
    }
    {
        let engine = directory.open();
        let mut tenant = authenticate(&engine, "tenant", "secret");
        assert!(
            !engine
                .session()
                .authenticate("tenant", b"01234567890123456789", &[])
        );
        tenant.use_database("shard_a").unwrap();
        assert_eq!(
            tenant.execute_sql("SELECT amount FROM items").unwrap()[0].rows[0]["amount"],
            json!(20)
        );
        assert!(tenant.use_database("shard_b").is_err());
        engine
            .execute_sql("DROP DATABASE shard_a; CREATE DATABASE shard_a")
            .unwrap();
        assert!(tenant.execute_sql("SELECT * FROM items").is_err());
    }
    let engine = directory.open();
    let mut tenant = authenticate(&engine, "tenant", "secret");
    assert!(tenant.use_database("shard_a").is_err());
}

#[test]
fn shard_metadata_uses_selected_database_and_cannot_leak_other_schemas() {
    let engine = Engine::new(EngineConfig::mysql_strict());
    provision(&engine);
    let mut admin = engine.session();
    admin.use_database("shard_a").unwrap();
    admin.execute_sql("CREATE TABLE children (id INT PRIMARY KEY, parent_id INT, CONSTRAINT fk_parent FOREIGN KEY (parent_id) REFERENCES items(id))").unwrap();
    let mut tenant = authenticate(&engine, "tenant", "secret");
    tenant.use_database("shard_a").unwrap();
    for (sql, columns) in [
        (
            "SELECT schema_name FROM information_schema.schemata",
            vec!["schema_name"],
        ),
        (
            "SELECT table_schema FROM information_schema.tables WHERE table_schema = 'shard_a'",
            vec!["table_schema"],
        ),
        (
            "SELECT table_schema FROM information_schema.columns WHERE table_schema = 'shard_a'",
            vec!["table_schema"],
        ),
        (
            "SELECT table_schema, constraint_schema FROM information_schema.table_constraints WHERE table_schema = 'shard_a'",
            vec!["table_schema", "constraint_schema"],
        ),
        (
            "SELECT table_schema FROM information_schema.statistics WHERE table_schema = 'shard_a'",
            vec!["table_schema"],
        ),
        (
            "SELECT constraint_schema, referenced_table_schema FROM information_schema.key_column_usage WHERE constraint_name = 'fk_parent'",
            vec!["constraint_schema", "referenced_table_schema"],
        ),
        (
            "SELECT constraint_schema, unique_constraint_schema FROM information_schema.referential_constraints WHERE constraint_name = 'fk_parent'",
            vec!["constraint_schema", "unique_constraint_schema"],
        ),
    ] {
        let results = tenant.execute_sql(sql).unwrap();
        assert!(!results[0].rows.is_empty(), "metadata missing: {sql}");
        for row in &results[0].rows {
            for column in &columns {
                assert_eq!(row[*column], json!("shard_a"), "wrong namespace: {sql}");
            }
        }
    }
    assert!(
        tenant
            .execute_sql(
                "SELECT table_name FROM information_schema.tables WHERE table_schema = 'shard_b'"
            )
            .unwrap()[0]
            .rows
            .is_empty()
    );
}

#[test]
fn sql_prepared_statements_keep_account_and_database_boundaries() {
    let engine = Engine::new(EngineConfig::mysql_strict());
    provision(&engine);
    let mut tenant = authenticate(&engine, "tenant", "secret");
    tenant.use_database("shard_a").unwrap();
    tenant.execute_sql("SET @query = 'SELECT amount FROM items WHERE id = ?'; PREPARE lookup FROM @query; SET @id = 1").unwrap();
    assert_eq!(
        tenant.execute_sql("EXECUTE lookup USING @id").unwrap()[0].rows[0]["amount"],
        json!(20)
    );
    for forbidden in [
        "PREPARE escape FROM 'SELECT * FROM shard_b.items'",
        "PREPARE escape FROM 'SELECT * FROM items WHERE id IN (SELECT id FROM shard_b.items)'",
        "PREPARE escape FROM 'CREATE TABLE forbidden (id INT)'",
        "PREPARE escape FROM 'SELECT 1; DELETE FROM items'",
        "PREPARE escape FROM 'EXECUTE lookup'",
    ] {
        assert!(
            tenant.execute_sql(forbidden).is_err(),
            "allowed {forbidden}"
        );
    }
    engine
        .execute_sql("REVOKE ALL PRIVILEGES, GRANT OPTION FROM 'tenant'@'%'")
        .unwrap();
    assert!(tenant.execute_sql("EXECUTE lookup USING @id").is_err());
}

#[test]
fn mysql_rename_keeps_namespace_validation_and_transaction_boundary() {
    let engine = Engine::new(EngineConfig::mysql_strict());
    provision(&engine);
    let mut admin = engine.session();
    admin.use_database("shard_a").unwrap();
    assert!(
        admin
            .execute_sql("RENAME TABLE items TO shard_b.renamed")
            .is_err()
    );
    admin
        .execute_sql("RENAME TABLE shard_a.items TO shard_a.renamed")
        .unwrap();
    assert_eq!(
        admin.execute_sql("SELECT amount FROM renamed").unwrap()[0].rows[0]["amount"],
        json!(20)
    );
    admin.execute_sql("BEGIN").unwrap();
    assert!(admin.execute_sql("RENAME TABLE renamed TO items").is_err());
    admin.execute_sql("ROLLBACK").unwrap();
}

#[test]
fn dropping_an_index_preserves_other_tables_and_database_boundaries() {
    let engine = Engine::new(EngineConfig::mysql_strict());
    engine.execute_sql("CREATE TABLE first_table (id INT); CREATE TABLE second_table (id INT); CREATE INDEX same_name ON first_table (id); CREATE INDEX same_name ON second_table (id)").unwrap();
    engine
        .execute_sql("DROP INDEX same_name ON app.first_table")
        .unwrap();
    let snapshot = engine.snapshot();
    assert!(snapshot.schemas["first_table"].indexes.is_empty());
    assert_eq!(snapshot.schemas["second_table"].indexes.len(), 1);
    assert!(
        engine
            .execute_sql("DROP INDEX same_name ON other.second_table")
            .is_err()
    );
    let mut session = engine.session();
    session.execute_sql("BEGIN").unwrap();
    assert!(
        session
            .execute_sql("DROP INDEX same_name ON second_table")
            .is_err()
    );
    session.execute_sql("ROLLBACK").unwrap();
    assert_eq!(engine.snapshot().schemas["second_table"].indexes.len(), 1);
}

#[test]
fn returning_rows_requires_select_privilege_and_denial_is_atomic() {
    let engine = Engine::new(EngineConfig::mysql_strict());
    provision(&engine);
    engine.execute_sql("REVOKE ALL PRIVILEGES, GRANT OPTION FROM 'tenant'@'%'; GRANT INSERT,UPDATE,DELETE ON shard_a.* TO 'tenant'@'%'").unwrap();
    let mut tenant = authenticate(&engine, "tenant", "secret");
    tenant.use_database("shard_a").unwrap();
    for sql in [
        "INSERT INTO items VALUES (2, 22) RETURNING *",
        "UPDATE items SET amount = 99 RETURNING *",
        "DELETE FROM items RETURNING *",
    ] {
        assert!(tenant.execute_sql(sql).is_err());
    }
    let mut admin = engine.session();
    admin.use_database("shard_a").unwrap();
    assert_eq!(
        admin.execute_sql("SELECT amount FROM items").unwrap()[0].rows[0]["amount"],
        json!(20)
    );
}

#[test]
fn schema_catalog_lists_only_accessible_databases() {
    let engine = Engine::new(EngineConfig::mysql_strict());
    provision(&engine);
    let rows = engine
        .execute_sql("SELECT schema_name FROM information_schema.SCHEMATA ORDER BY schema_name")
        .unwrap()
        .remove(0)
        .rows;
    assert_eq!(rows.len(), 3);
    let mut tenant = authenticate(&engine, "tenant", "secret");
    tenant.use_database("shard_a").unwrap();
    let rows = tenant
        .execute_sql("SELECT schema_name FROM information_schema.SCHEMATA")
        .unwrap()
        .remove(0)
        .rows;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["schema_name"], "shard_a");
    assert!(
        tenant
            .execute_sql(
                "SELECT schema_name FROM information_schema.SCHEMATA WHERE schema_name='shard_b'"
            )
            .unwrap()[0]
            .rows
            .is_empty()
    );
}

#[test]
fn advisory_locks_are_connection_owned_recursive_and_released_on_disconnect() {
    let engine = Engine::new(EngineConfig::mysql_strict());
    let mut first = engine.session();
    let mut second = engine.session();
    let acquire = "SELECT GET_LOCK(CONCAT('intent-provision:',DATABASE()),0) AS acquired";
    let release = "SELECT RELEASE_LOCK(CONCAT('intent-provision:',DATABASE())) AS released";
    for _ in 0..2 {
        assert_eq!(
            first.execute_sql(acquire).unwrap()[0].rows[0]["acquired"],
            1
        );
    }
    assert_eq!(
        second.execute_sql(acquire).unwrap()[0].rows[0]["acquired"],
        0
    );
    assert_eq!(
        second.execute_sql(release).unwrap()[0].rows[0]["released"],
        0
    );
    first.execute_sql("BEGIN; ROLLBACK").unwrap();
    assert_eq!(
        first.execute_sql(release).unwrap()[0].rows[0]["released"],
        1
    );
    assert_eq!(
        second.execute_sql(acquire).unwrap()[0].rows[0]["acquired"],
        0
    );
    drop(first);
    assert_eq!(
        second.execute_sql(acquire).unwrap()[0].rows[0]["acquired"],
        1
    );
    assert_eq!(
        second.execute_sql(release).unwrap()[0].rows[0]["released"],
        1
    );
    assert!(second.execute_sql(release).unwrap()[0].rows[0]["released"].is_null());
    assert!(
        second
            .execute_sql("SELECT GET_LOCK('bad',0) WHERE FALSE")
            .is_err()
    );
}

#[test]
fn physical_fingerprint_metadata_exposes_character_and_generated_columns() {
    let engine = Engine::new(EngineConfig::mysql_strict());
    engine.execute_sql("CREATE TABLE physical (id INT PRIMARY KEY, title VARCHAR(191), doubled INT GENERATED ALWAYS AS (id * 2) STORED)").unwrap();
    let result = engine.execute_sql("SELECT COLUMN_NAME AS column_name, CHARACTER_SET_NAME AS charset, COLLATION_NAME AS collation, GENERATION_EXPRESSION AS expression FROM information_schema.COLUMNS WHERE TABLE_SCHEMA=DATABASE() AND TABLE_NAME='physical'").unwrap();
    let title = result[0]
        .rows
        .iter()
        .find(|row| row["column_name"] == "title")
        .unwrap();
    assert_eq!(title["charset"], "utf8mb4");
    assert_eq!(title["collation"], "utf8mb4_general_ci");
    let generated = result[0]
        .rows
        .iter()
        .find(|row| row["column_name"] == "doubled")
        .unwrap();
    assert!(!generated["expression"].as_str().unwrap().is_empty());
    assert!(generated["collation"].is_null());
    assert_eq!(
        engine
            .execute_sql(
                "SELECT TABLE_COLLATION FROM information_schema.TABLES WHERE TABLE_NAME='physical'"
            )
            .unwrap()[0]
            .rows[0]["TABLE_COLLATION"],
        "utf8mb4_general_ci"
    );
}

#[test]
fn metadata_ordering_precedes_projection_for_repeatable_physical_fingerprints() {
    let engine = Engine::new(EngineConfig::mysql_strict());
    engine
        .execute_sql("CREATE TABLE zebra (z INT, a INT); CREATE TABLE alpha (last INT, first INT)")
        .unwrap();
    let expected = json!([
        {"TABLE_NAME":"alpha","COLUMN_NAME":"last"},
        {"TABLE_NAME":"alpha","COLUMN_NAME":"first"},
        {"TABLE_NAME":"zebra","COLUMN_NAME":"z"},
        {"TABLE_NAME":"zebra","COLUMN_NAME":"a"}
    ]);
    for _ in 0..10 {
        let rows = engine.execute_sql("SELECT TABLE_NAME,COLUMN_NAME FROM information_schema.COLUMNS WHERE TABLE_SCHEMA=DATABASE() ORDER BY TABLE_NAME,ORDINAL_POSITION").unwrap().remove(0).rows;
        assert_eq!(json!(rows), expected);
    }
}

#[test]
fn quoted_column_projection_uses_identifier_value_as_wire_label() {
    let engine = Engine::new(EngineConfig::mysql_strict());
    engine
        .execute_sql("CREATE TABLE names (`key` VARCHAR(50)); INSERT INTO names VALUES ('store-a')")
        .unwrap();
    for sql in ["SELECT `key` FROM names", "SELECT names.`key` FROM names"] {
        let result = engine.execute_sql(sql).unwrap();
        assert_eq!(result[0].columns, vec!["key"]);
        assert_eq!(result[0].rows[0]["key"], "store-a");
    }
}

#[test]
fn drizzle_empty_metadata_keeps_fields_and_qualified_predicates() {
    let engine = Engine::new(EngineConfig::mysql_strict());
    for query in [
        "SELECT * FROM information_schema.COLUMNS WHERE TABLE_SCHEMA='app' ORDER BY ORDINAL_POSITION",
        "SELECT * FROM information_schema.STATISTICS WHERE INFORMATION_SCHEMA.STATISTICS.TABLE_SCHEMA='app' AND INFORMATION_SCHEMA.STATISTICS.INDEX_NAME != 'PRIMARY' ORDER BY SEQ_IN_INDEX",
    ] {
        let result = engine.execute_sql(query).unwrap();
        assert!(result[0].rows.is_empty());
        assert!(
            !result[0].columns.is_empty(),
            "empty metadata must remain a result set: {query}"
        );
    }
}

#[test]
fn separately_named_indexes_on_same_columns_survive_drop_and_reopen() {
    let directory =
        std::env::temp_dir().join(format!("sqweel-index-names-{}", uuid::Uuid::new_v4()));
    {
        let engine =
            Engine::open_with_data_dir(EngineConfig::mysql_strict(), directory.to_str()).unwrap();
        engine.execute_sql("CREATE TABLE records(id INT PRIMARY KEY,value INT); CREATE INDEX first_idx ON records(value); CREATE INDEX second_idx ON records(value); INSERT INTO records VALUES(1,10)").unwrap();
        assert_eq!(engine.execute_sql("SELECT INDEX_NAME FROM information_schema.STATISTICS WHERE TABLE_NAME='records' AND INDEX_NAME!='PRIMARY'").unwrap()[0].rows.len(), 2);
        engine
            .execute_sql("DROP INDEX first_idx ON records")
            .unwrap();
    }
    {
        let engine =
            Engine::open_with_data_dir(EngineConfig::mysql_strict(), directory.to_str()).unwrap();
        let rows = engine.execute_sql("SELECT INDEX_NAME FROM information_schema.STATISTICS WHERE TABLE_NAME='records' AND INDEX_NAME!='PRIMARY'").unwrap().remove(0).rows;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["INDEX_NAME"], "second_idx");
        assert_eq!(
            engine
                .execute_sql("SELECT id FROM records WHERE value=10")
                .unwrap()[0]
                .rows[0]["id"],
            1
        );
        engine.execute_sql("CREATE UNIQUE INDEX unique_one ON records(value); CREATE UNIQUE INDEX unique_two ON records(value); DROP INDEX unique_one ON records").unwrap();
        assert!(
            engine
                .execute_sql("INSERT INTO records VALUES(2,10)")
                .is_err()
        );
    }
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn compatibility_syntax_keeps_privilege_and_database_checks() {
    let engine = Engine::new(EngineConfig::mysql_strict());
    provision(&engine);
    let mut admin = engine.session();
    admin.use_database("shard_a").unwrap();
    let mut tenant = authenticate(&engine, "tenant", "secret");
    tenant.use_database("shard_a").unwrap();
    for sql in [
        "CHECK TABLE shard_b.items",
        "CREATE OR REPLACE INDEX lookup ON shard_b.items(amount)",
        "EXPLAIN FORMAT=JSON SELECT * FROM shard_b.items",
        "INSERT INTO items SELECT * FROM shard_b.items RETURNING *",
        "REPLACE INTO items SELECT * FROM shard_b.items RETURNING *",
        "UPDATE items SET amount=(@counter:= (SELECT amount FROM shard_b.items))",
    ] {
        assert!(admin.execute_sql(sql).is_err(), "allowed {sql}");
        assert!(tenant.execute_sql(sql).is_err(), "allowed {sql}");
    }
    for sql in [
        "CHECK TABLE items",
        "CREATE OR REPLACE INDEX lookup ON items(amount)",
    ] {
        assert!(tenant.execute_sql(sql).is_err(), "allowed {sql}");
        admin.execute_sql(sql).unwrap();
    }
    admin.execute_sql("BEGIN").unwrap();
    assert!(
        admin
            .execute_sql("CREATE OR REPLACE INDEX lookup ON items(id)")
            .is_err()
    );
    admin.execute_sql("ROLLBACK").unwrap();
    engine.execute_sql("REVOKE ALL PRIVILEGES, GRANT OPTION FROM 'tenant'@'%'; GRANT INSERT ON shard_a.* TO 'tenant'@'%'").unwrap();
    assert!(
        tenant
            .execute_sql("INSERT INTO items SELECT 2, 25 RETURNING *")
            .is_err()
    );
    assert_eq!(
        admin
            .execute_sql("SELECT COUNT(*) AS n FROM items")
            .unwrap()[0]
            .rows[0]["n"],
        1
    );
}

#[test]
fn conditional_index_drop_preserves_notes_and_table_scope() {
    let engine = Engine::new(EngineConfig::mysql_strict());
    engine.execute_sql("CREATE TABLE left_table (id INT); CREATE TABLE right_table (id INT); CREATE INDEX shared_name ON left_table(id); CREATE INDEX shared_name ON right_table(id)").unwrap();
    let first = engine
        .execute_sql("DROP INDEX IF EXISTS shared_name ON left_table")
        .unwrap();
    assert!(first[0].warnings.is_empty());
    let second = engine
        .execute_sql("DROP INDEX IF EXISTS shared_name ON left_table")
        .unwrap();
    assert_eq!(second[0].warnings.len(), 1);
    assert_eq!(second[0].warnings[0].code, 1091);
    assert!(
        engine
            .execute_sql("DROP INDEX shared_name ON left_table")
            .is_err()
    );
    assert_eq!(
        engine.execute_sql("SHOW INDEX FROM right_table").unwrap()[0]
            .rows
            .len(),
        1
    );
}
