mod common;

use my_sqweel::sql::engine::Engine;
use serde_json::Value;

use common::test_lock;

#[test]
fn mysql_three_valued_logic_numeric_coercion_and_string_lengths() {
    let _guard = test_lock();
    let engine = Engine::default();

    let result = engine
        .execute_sql(
            "SELECT \
                NULL = 1 AS null_eq, \
                NOT NULL AS not_null, \
                TRUE AND NULL AS true_and_null, \
                FALSE AND NULL AS false_and_null, \
                TRUE OR NULL AS true_or_null, \
                FALSE OR NULL AS false_or_null, \
                1 NOT IN (2, NULL) AS not_in_null, \
                NULL BETWEEN 1 AND 2 AS null_between, \
                NULL <=> NULL AS null_safe_equal, \
                7 DIV 2 AS integer_division, \
                5 ^ 3 AS bitwise_xor, \
                '12tail' + 1 AS numeric_prefix, \
                FLOOR(-5.3) AS floor_value, \
                CEIL(10.1) AS ceil_value, \
                TRIM(BOTH 'x' FROM 'xxvaluexxx') AS trimmed_value, \
                LENGTH('é') AS byte_length, \
                CHAR_LENGTH('é') AS character_length",
        )
        .unwrap();
    let row = &result[0].rows[0];

    for column in [
        "null_eq",
        "not_null",
        "true_and_null",
        "false_or_null",
        "not_in_null",
        "null_between",
    ] {
        assert_eq!(row.get(column), Some(&Value::Null), "column {column}");
    }
    assert_eq!(
        row.get("false_and_null").and_then(Value::as_bool),
        Some(false)
    );
    assert_eq!(row.get("true_or_null").and_then(Value::as_bool), Some(true));
    assert_eq!(
        row.get("null_safe_equal").and_then(Value::as_bool),
        Some(true)
    );
    assert_eq!(row.get("integer_division").and_then(Value::as_i64), Some(3));
    assert_eq!(row.get("bitwise_xor").and_then(Value::as_i64), Some(6));
    assert_eq!(row.get("numeric_prefix").and_then(Value::as_i64), Some(13));
    assert_eq!(row.get("floor_value").and_then(Value::as_i64), Some(-6));
    assert_eq!(row.get("ceil_value").and_then(Value::as_i64), Some(11));
    assert_eq!(
        row.get("trimmed_value").and_then(Value::as_str),
        Some("value")
    );
    assert_eq!(row.get("byte_length").and_then(Value::as_u64), Some(2));
    assert_eq!(row.get("character_length").and_then(Value::as_u64), Some(1));
}

#[test]
fn distinct_nested_aggregates_and_empty_aggregate_sets_match_mysql() {
    let _guard = test_lock();
    let engine = Engine::default();
    engine
        .execute_sql(
            "CREATE TABLE compat_values (id BIGINT PRIMARY KEY, category VARCHAR(32), amount BIGINT);",
        )
        .unwrap();
    engine
        .execute_sql(
            "INSERT INTO compat_values (id, category, amount) VALUES \
             (1, 'alpha', 10), (2, 'alpha', NULL), (3, 'beta', 20);",
        )
        .unwrap();

    let distinct = engine
        .execute_sql("SELECT DISTINCT category FROM compat_values ORDER BY category")
        .unwrap();
    let categories = distinct[0]
        .rows
        .iter()
        .filter_map(|row| row.get("category").and_then(Value::as_str))
        .collect::<Vec<_>>();
    assert_eq!(categories, vec!["alpha", "beta"]);

    let aggregates = engine
        .execute_sql(
            "SELECT \
                SUM(amount) + 1 AS nested_sum, \
                COALESCE(SUM(amount), 0) AS coalesced_sum, \
                SUM(CASE WHEN amount >= 20 THEN amount ELSE 0 END) AS conditional_sum \
             FROM compat_values",
        )
        .unwrap();
    let row = &aggregates[0].rows[0];
    assert_eq!(row.get("nested_sum").and_then(Value::as_i64), Some(31));
    assert_eq!(row.get("coalesced_sum").and_then(Value::as_i64), Some(30));
    assert_eq!(row.get("conditional_sum").and_then(Value::as_i64), Some(20));

    let empty = engine
        .execute_sql(
            "SELECT SUM(amount) AS empty_sum, COALESCE(SUM(amount), 0) AS empty_coalesced \
             FROM compat_values WHERE id < 0",
        )
        .unwrap();
    assert_eq!(empty[0].rows[0].get("empty_sum"), Some(&Value::Null));
    assert_eq!(
        empty[0].rows[0]
            .get("empty_coalesced")
            .and_then(Value::as_i64),
        Some(0)
    );
}

#[test]
fn qualified_columns_cross_joins_and_scalar_subquery_cardinality_are_enforced() {
    let _guard = test_lock();
    let engine = Engine::default();
    engine
        .execute_sql("CREATE TABLE left_values (id BIGINT PRIMARY KEY, label TEXT);")
        .unwrap();
    engine
        .execute_sql("CREATE TABLE right_values (id BIGINT PRIMARY KEY, label TEXT);")
        .unwrap();
    engine
        .execute_sql("INSERT INTO left_values VALUES (1, 'left'), (2, 'second');")
        .unwrap();
    engine
        .execute_sql("INSERT INTO right_values VALUES (10, 'right');")
        .unwrap();

    let qualified = engine
        .execute_sql("SELECT l.id, l.label FROM left_values AS l WHERE l.id = 1")
        .unwrap();
    assert_eq!(qualified[0].rows.len(), 1);

    let cross = engine
        .execute_sql(
            "SELECT l.id AS left_id, r.id AS right_id \
             FROM left_values AS l CROSS JOIN right_values AS r ORDER BY l.id",
        )
        .unwrap();
    assert_eq!(cross[0].rows.len(), 2);

    let err = engine
        .execute_sql("SELECT (SELECT id FROM left_values) AS ambiguous_scalar")
        .unwrap_err();
    assert!(err.to_string().contains("more than one row"));
}

#[test]
fn unsupported_or_unresolved_sql_fails_closed() {
    let _guard = test_lock();
    let engine = Engine::default();
    engine
        .execute_sql("CREATE TABLE fail_closed_values (id BIGINT PRIMARY KEY);")
        .unwrap();

    for sql in [
        "SELECT definitely_not_a_mysql_function(1)",
        "SELECT * FROM missing_table",
        "SELECT missing_column FROM fail_closed_values",
    ] {
        let result = engine.execute_sql(sql);
        assert!(
            result.is_err(),
            "query should fail closed: {sql}: {result:?}"
        );
    }
}

#[test]
fn union_uses_left_branch_columns_and_enforces_arity() {
    let _guard = test_lock();
    let engine = Engine::default();

    let distinct = engine
        .execute_sql("SELECT 1 AS left_name UNION SELECT 1 AS right_name")
        .unwrap();
    assert_eq!(distinct[0].columns, vec!["left_name"]);
    assert_eq!(distinct[0].rows.len(), 1);
    assert_eq!(
        distinct[0].rows[0].get("left_name").and_then(Value::as_i64),
        Some(1)
    );

    let all = engine
        .execute_sql("SELECT 1 AS left_name UNION ALL SELECT 1 AS right_name")
        .unwrap();
    assert_eq!(all[0].columns, vec!["left_name"]);
    assert_eq!(all[0].rows.len(), 2);

    let error = engine
        .execute_sql("SELECT 1 AS one UNION SELECT 1 AS one, 2 AS two")
        .unwrap_err();
    assert!(error.to_string().contains("different column counts"));
}
