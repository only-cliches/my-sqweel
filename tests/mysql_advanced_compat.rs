mod common;

use my_sqweel::sql::engine::{CompatibilityProfile, Engine, EngineConfig, MysqlColumnType};
use serde_json::Value;

use common::test_lock;

#[test]
fn strict_profile_rejects_drift_and_exposes_typed_metadata() {
    let _guard = test_lock();
    let engine = Engine::new(EngineConfig::mysql_strict());
    assert_eq!(
        engine.compatibility_profile(),
        CompatibilityProfile::MysqlStrict
    );
    engine
        .execute_sql(
            "CREATE TABLE typed_values (id BIGINT UNSIGNED PRIMARY KEY, amount DECIMAL(12,2), happened_at DATETIME, payload JSON)",
        )
        .unwrap();

    assert!(
        engine
            .execute_sql("CREATE TABLE typed_values (id INT)")
            .is_err()
    );
    assert!(
        engine
            .execute_sql("INSERT INTO missing_table VALUES (1)")
            .is_err()
    );
    assert!(
        engine
            .execute_sql("INSERT INTO typed_values (id, missing) VALUES (1, 2)")
            .is_err()
    );

    let result = engine
        .execute_sql("SELECT id, amount, happened_at, payload FROM typed_values")
        .unwrap()
        .remove(0);
    assert_eq!(result.column_metadata.len(), 4);
    assert_eq!(
        result.column_metadata[0].column_type,
        MysqlColumnType::BigInt
    );
    assert!(result.column_metadata[0].unsigned);
    assert_eq!(
        result.column_metadata[1].column_type,
        MysqlColumnType::Decimal
    );
    assert_eq!(result.column_metadata[1].decimals, 2);
    assert_eq!(
        result.column_metadata[2].column_type,
        MysqlColumnType::DateTime
    );
    assert_eq!(result.column_metadata[3].column_type, MysqlColumnType::Json);
}

#[test]
fn qualified_wildcards_join_variants_and_derived_joins_work() {
    let _guard = test_lock();
    let engine = Engine::default();
    engine
        .execute_sql("CREATE TABLE join_left (id BIGINT PRIMARY KEY, left_name TEXT)")
        .unwrap();
    engine
        .execute_sql("CREATE TABLE join_right (id BIGINT PRIMARY KEY, right_name TEXT)")
        .unwrap();
    engine
        .execute_sql("INSERT INTO join_left VALUES (1, 'one'), (2, 'two')")
        .unwrap();
    engine
        .execute_sql("INSERT INTO join_right VALUES (2, 'dos'), (3, 'tres')")
        .unwrap();

    let qualified = engine
        .execute_sql(
            "SELECT r.* FROM join_left l RIGHT JOIN join_right r ON l.id = r.id ORDER BY r.id",
        )
        .unwrap();
    assert_eq!(qualified[0].columns, vec!["id", "right_name"]);
    assert_eq!(qualified[0].rows.len(), 2);
    assert_eq!(
        qualified[0].rows[1].get("id").and_then(Value::as_i64),
        Some(3)
    );

    let using_join = engine
        .execute_sql("SELECT l.id, r.right_name FROM join_left l JOIN join_right r USING (id)")
        .unwrap();
    assert_eq!(using_join[0].rows.len(), 1);

    let natural_join = engine
        .execute_sql("SELECT l.id FROM join_left l NATURAL JOIN join_right r")
        .unwrap();
    assert_eq!(natural_join[0].rows.len(), 1);

    let derived = engine
        .execute_sql(
            "SELECT l.id, d.right_name FROM join_left l JOIN (SELECT id, right_name FROM join_right) d ON d.id = l.id",
        )
        .unwrap();
    assert_eq!(derived[0].rows.len(), 1);
}

#[test]
fn nonrecursive_ctes_set_operations_and_windows_work() {
    let _guard = test_lock();
    let engine = Engine::default();
    engine
        .execute_sql(
            "CREATE TABLE analytic_values (id BIGINT PRIMARY KEY, grp TEXT, amount BIGINT)",
        )
        .unwrap();
    engine
        .execute_sql(
            "INSERT INTO analytic_values VALUES (1, 'a', 10), (2, 'a', 20), (3, 'b', 7), (4, 'a', 20)",
        )
        .unwrap();

    let cte = engine
        .execute_sql(
            "WITH selected (value_id, value_amount) AS (SELECT id, amount FROM analytic_values WHERE id <= 2) SELECT value_id FROM selected ORDER BY value_id",
        )
        .unwrap();
    assert_eq!(cte[0].rows.len(), 2);

    let intersection = engine
        .execute_sql("SELECT 1 AS n UNION ALL SELECT 1 INTERSECT ALL SELECT 1")
        .unwrap();
    assert!(!intersection[0].rows.is_empty());
    let difference = engine
        .execute_sql("SELECT 1 AS n UNION SELECT 2 EXCEPT SELECT 1")
        .unwrap();
    assert_eq!(difference[0].rows.len(), 1);
    assert_eq!(
        difference[0].rows[0].get("n").and_then(Value::as_i64),
        Some(2)
    );

    let window = engine
        .execute_sql(
            "SELECT id, grp, ROW_NUMBER() OVER (PARTITION BY grp ORDER BY amount) AS row_num, SUM(amount) OVER (PARTITION BY grp ORDER BY id ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW) AS running_total, LAG(amount, 1, 0) OVER (PARTITION BY grp ORDER BY id) AS previous_amount FROM analytic_values ORDER BY id",
        )
        .unwrap();
    assert_eq!(
        window[0].rows[0].get("row_num").and_then(Value::as_u64),
        Some(1)
    );
    assert_eq!(
        window[0].rows[1]
            .get("running_total")
            .and_then(Value::as_i64),
        Some(30)
    );
    assert_eq!(
        window[0].rows[1]
            .get("previous_amount")
            .and_then(Value::as_i64),
        Some(10)
    );

    let peer_windows = engine
        .execute_sql(
            "SELECT id, CUME_DIST() OVER (PARTITION BY grp ORDER BY amount) AS cumulative_distribution, SUM(amount) OVER (PARTITION BY grp ORDER BY amount) AS peer_running_total FROM analytic_values WHERE grp = 'a' ORDER BY id",
        )
        .unwrap();
    assert_eq!(
        peer_windows[0].column_metadata[1].column_type,
        MysqlColumnType::Double
    );
    assert_eq!(peer_windows[0].column_metadata[1].decimals, 10);
    assert_eq!(
        peer_windows[0].rows[1]
            .get("cumulative_distribution")
            .and_then(Value::as_f64),
        Some(1.0)
    );
    assert_eq!(
        peer_windows[0].rows[1]
            .get("peer_running_total")
            .and_then(Value::as_i64),
        Some(50)
    );
}

#[test]
fn mysql_multi_table_delete_forms_remove_only_joined_targets() {
    let _guard = test_lock();
    let engine = Engine::default();
    engine
        .execute_sql("CREATE TABLE delete_parent (id BIGINT PRIMARY KEY, name TEXT)")
        .unwrap();
    engine
        .execute_sql("CREATE TABLE delete_child (id BIGINT PRIMARY KEY, parent_id BIGINT)")
        .unwrap();
    engine
        .execute_sql("INSERT INTO delete_parent VALUES (1, 'keep'), (2, 'remove')")
        .unwrap();
    engine
        .execute_sql("INSERT INTO delete_child VALUES (10, 2), (11, 99)")
        .unwrap();

    let deleted = engine
        .execute_sql(
            "DELETE p FROM delete_parent p JOIN delete_child c ON c.parent_id = p.id WHERE c.id = 10",
        )
        .unwrap();
    assert_eq!(deleted[0].rows_affected, 1);
    let remaining = engine
        .execute_sql("SELECT id FROM delete_parent ORDER BY id")
        .unwrap();
    assert_eq!(remaining[0].rows.len(), 1);
    assert_eq!(
        remaining[0].rows[0].get("id").and_then(Value::as_i64),
        Some(1)
    );

    engine
        .execute_sql("INSERT INTO delete_parent VALUES (3, 'using')")
        .unwrap();
    engine
        .execute_sql("INSERT INTO delete_child VALUES (12, 3)")
        .unwrap();
    let using_deleted = engine
        .execute_sql(
            "DELETE FROM delete_child USING delete_child JOIN delete_parent ON delete_parent.id = delete_child.parent_id WHERE delete_parent.id = 3",
        )
        .unwrap();
    assert_eq!(using_deleted[0].rows_affected, 1);
}

#[test]
fn generated_columns_alter_positions_prefix_indexes_and_foreign_keys_work() {
    let _guard = test_lock();
    let engine = Engine::new(EngineConfig::mysql_strict());
    engine
        .execute_sql(
            "CREATE TABLE ddl_parent (id BIGINT PRIMARY KEY, name VARCHAR(64), qty INT, price INT, total INT GENERATED ALWAYS AS (qty * price) STORED)",
        )
        .unwrap();
    engine
        .execute_sql(
            "CREATE TABLE ddl_child (id BIGINT PRIMARY KEY, parent_id BIGINT, CONSTRAINT fk_ddl_parent FOREIGN KEY (parent_id) REFERENCES ddl_parent(id) ON DELETE CASCADE)",
        )
        .unwrap();
    engine
        .execute_sql("INSERT INTO ddl_parent (id, name, qty, price) VALUES (1, 'alphabet', 3, 7)")
        .unwrap();
    let generated = engine
        .execute_sql("SELECT total FROM ddl_parent WHERE id = 1")
        .unwrap();
    assert_eq!(
        generated[0].rows[0].get("total").and_then(Value::as_i64),
        Some(21)
    );
    assert!(
        engine
            .execute_sql(
                "INSERT INTO ddl_parent (id, name, qty, price, total) VALUES (2, 'bad', 1, 2, 99)",
            )
            .is_err()
    );
    assert!(
        engine
            .execute_sql("INSERT INTO ddl_child VALUES (9, 999)")
            .is_err()
    );
    engine
        .execute_sql("INSERT INTO ddl_child VALUES (10, 1)")
        .unwrap();

    engine
        .execute_sql("ALTER TABLE ddl_parent ADD COLUMN first_col INT FIRST")
        .unwrap();
    engine
        .execute_sql("ALTER TABLE ddl_parent ADD COLUMN after_name INT AFTER name")
        .unwrap();
    engine
        .execute_sql("CREATE INDEX idx_ddl_name ON ddl_parent (name(4))")
        .unwrap();
    let columns = engine.execute_sql("SHOW COLUMNS FROM ddl_parent").unwrap();
    assert_eq!(
        columns[0].rows[0].get("Field").and_then(Value::as_str),
        Some("first_col")
    );
    let indexes = engine.execute_sql("SHOW INDEX FROM ddl_parent").unwrap();
    let prefix = indexes[0]
        .rows
        .iter()
        .find(|row| row.get("Key_name").and_then(Value::as_str) == Some("idx_ddl_name"))
        .and_then(|row| row.get("Sub_part"))
        .and_then(Value::as_u64);
    assert_eq!(prefix, Some(4));

    engine
        .execute_sql("DELETE FROM ddl_parent WHERE id = 1")
        .unwrap();
    let child = engine.execute_sql("SELECT id FROM ddl_child").unwrap();
    assert!(child[0].rows.is_empty());
}
