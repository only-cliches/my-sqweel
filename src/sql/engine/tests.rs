use super::{Engine, EngineConfig, UniqueMode};
use chrono::{Duration, NaiveDateTime, Utc};
use serde_json::json;

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

#[test]
fn insert_values_evaluates_date_add_now_interval() {
    let engine = Engine::default();
    let before = Utc::now().naive_utc() + Duration::days(29);

    engine
        .execute_sql(
            "CREATE TABLE screen_tokens (id BIGINT PRIMARY KEY AUTO_INCREMENT, expires_at DATETIME NOT NULL);",
        )
        .unwrap();
    engine
        .execute_sql(
            "INSERT INTO screen_tokens (expires_at) VALUES (DATE_ADD(NOW(), INTERVAL 30 DAY));",
        )
        .unwrap();

    let out = engine
        .execute_sql("SELECT expires_at FROM screen_tokens;")
        .unwrap();
    let stored = out[0].rows[0]
        .get("expires_at")
        .and_then(|value| value.as_str())
        .expect("expires_at should be stored as datetime text");
    let expires_at = NaiveDateTime::parse_from_str(stored, "%Y-%m-%d %H:%M:%S%.f")
        .expect("DATE_ADD should store a parseable datetime");
    let after = Utc::now().naive_utc() + Duration::days(31);

    assert!(
        expires_at >= before && expires_at <= after,
        "expected expires_at within roughly 30 days, got {expires_at}"
    );
}

#[test]
fn evaluates_mysql_date_time_scalar_functions() {
    let engine = Engine::default();

    let out = engine
        .execute_sql(
            "SELECT \
                DATE_ADD('2026-01-31', INTERVAL 1 MONTH) AS plus_month, \
                DATE_SUB('2026-03-01', INTERVAL 1 DAY) AS minus_day, \
                TIMESTAMPADD(HOUR, 27, '2026-01-01 00:00:00') AS ts_added, \
                TIMESTAMPDIFF(HOUR, '2026-01-01 00:00:00', '2026-01-02 03:00:00') AS ts_diff, \
                DATEDIFF('2026-01-10 12:00:00', '2026-01-01') AS date_diff, \
                ADDTIME('2026-01-01 23:00:00', '02:30:00') AS add_time, \
                SUBTIME('03:30:00', '01:15:00') AS sub_time, \
                TIMEDIFF('03:30:00', '01:15:00') AS time_diff, \
                DATE_FORMAT('2026-04-05 06:07:08.123456', '%Y-%m-%d %H:%i:%s.%f') AS formatted, \
                EXTRACT(YEAR FROM '2026-04-05 06:07:08') AS extracted_year, \
                HOUR('2026-04-05 06:07:08') AS extracted_hour, \
                MICROSECOND('2026-04-05 06:07:08.123456') AS extracted_microsecond;",
        )
        .unwrap();
    let row = &out[0].rows[0];

    assert_eq!(
        row.get("plus_month").and_then(|v| v.as_str()),
        Some("2026-02-28 00:00:00")
    );
    assert_eq!(
        row.get("minus_day").and_then(|v| v.as_str()),
        Some("2026-02-28 00:00:00")
    );
    assert_eq!(
        row.get("ts_added").and_then(|v| v.as_str()),
        Some("2026-01-02 03:00:00")
    );
    assert_eq!(row.get("ts_diff").and_then(|v| v.as_i64()), Some(27));
    assert_eq!(row.get("date_diff").and_then(|v| v.as_i64()), Some(9));
    assert_eq!(
        row.get("add_time").and_then(|v| v.as_str()),
        Some("2026-01-02 01:30:00")
    );
    assert_eq!(
        row.get("sub_time").and_then(|v| v.as_str()),
        Some("02:15:00")
    );
    assert_eq!(
        row.get("time_diff").and_then(|v| v.as_str()),
        Some("02:15:00")
    );
    assert_eq!(
        row.get("formatted").and_then(|v| v.as_str()),
        Some("2026-04-05 06:07:08.123456")
    );
    assert_eq!(
        row.get("extracted_year").and_then(|v| v.as_i64()),
        Some(2026)
    );
    assert_eq!(row.get("extracted_hour").and_then(|v| v.as_i64()), Some(6));
    assert_eq!(
        row.get("extracted_microsecond").and_then(|v| v.as_i64()),
        Some(123456)
    );
}

#[test]
fn evaluates_current_utc_date_time_functions() {
    let engine = Engine::default();

    let out = engine
        .execute_sql(
            "SELECT CURRENT_TIME AS current_time, UTC_DATE AS utc_date, UTC_TIMESTAMP AS utc_timestamp;",
        )
        .unwrap();
    let row = &out[0].rows[0];

    let current_time = row
        .get("current_time")
        .and_then(|value| value.as_str())
        .expect("CURRENT_TIME should return text");
    NaiveDateTime::parse_from_str(
        &format!("1970-01-01 {current_time}"),
        "%Y-%m-%d %H:%M:%S%.f",
    )
    .expect("CURRENT_TIME should return a parseable time");

    let utc_date = row
        .get("utc_date")
        .and_then(|value| value.as_str())
        .expect("UTC_DATE should return text");
    chrono::NaiveDate::parse_from_str(utc_date, "%Y-%m-%d")
        .expect("UTC_DATE should return a parseable date");

    let utc_timestamp = row
        .get("utc_timestamp")
        .and_then(|value| value.as_str())
        .expect("UTC_TIMESTAMP should return text");
    NaiveDateTime::parse_from_str(utc_timestamp, "%Y-%m-%d %H:%M:%S%.f")
        .expect("UTC_TIMESTAMP should return a parseable timestamp");
}

#[test]
fn evaluates_json_string_math_and_conversion_functions() {
    let engine = Engine::default();

    let out = engine
        .execute_sql(
            "SELECT \
                JSON_EXTRACT('{\"user\":{\"name\":\"Ada\",\"tags\":[\"db\",\"sql\"]}}', '$.user.name') AS json_name, \
                JSON_UNQUOTE(JSON_EXTRACT('{\"user\":{\"name\":\"Ada\"}}', '$.user.name')) AS unquoted_name, \
                JSON_OBJECT('name', 'Ada', 'age', 36) AS json_object, \
                JSON_ARRAY(1, 'two', NULL) AS json_array, \
                JSON_CONTAINS('{\"a\":1,\"b\":2}', '{\"a\":1}') AS json_contains, \
                JSON_SET('{\"a\":1}', '$.b', 2) AS json_set, \
                JSON_REMOVE('{\"a\":1,\"b\":2}', '$.a') AS json_remove, \
                LEFT('abcdef', 3) AS left_part, \
                RIGHT('abcdef', 2) AS right_part, \
                LPAD('7', 3, '0') AS lpad_value, \
                RPAD('x', 3, '.') AS rpad_value, \
                LOCATE('bc', 'abcabc', 3) AS locate_value, \
                INSTR('abc', 'b') AS instr_value, \
                POSITION('b' IN 'abc') AS position_value, \
                REVERSE('abc') AS reverse_value, \
                REPEAT('ab', 3) AS repeat_value, \
                ASCII('A') AS ascii_value, \
                GREATEST(3, 9, 5) AS greatest_value, \
                LEAST(3, 9, 5) AS least_value, \
                SIGN(-4) AS sign_value, \
                SQRT(9) AS sqrt_value, \
                LOG(2, 8) AS log_value, \
                EXP(0) AS exp_value, \
                TRUNCATE(3.14159, 2) AS truncate_value, \
                MOD(10, 4) AS mod_value, \
                CAST('2026-04-05 06:07:08' AS DATE) AS cast_date, \
                CAST('2026-04-05 06:07:08' AS TIME) AS cast_time, \
                CAST('{\"a\":1}' AS JSON) AS cast_json, \
                CONVERT('42', SIGNED) AS convert_signed;",
        )
        .unwrap();
    let row = &out[0].rows[0];

    assert_eq!(row.get("json_name"), Some(&json!("Ada")));
    assert_eq!(row.get("unquoted_name"), Some(&json!("Ada")));
    assert_eq!(
        row.get("json_object"),
        Some(&json!({"name": "Ada", "age": 36}))
    );
    assert_eq!(row.get("json_array"), Some(&json!([1, "two", null])));
    assert_eq!(row.get("json_contains").and_then(|v| v.as_i64()), Some(1));
    assert_eq!(row.get("json_set"), Some(&json!({"a": 1, "b": 2})));
    assert_eq!(row.get("json_remove"), Some(&json!({"b": 2})));
    assert_eq!(row.get("left_part").and_then(|v| v.as_str()), Some("abc"));
    assert_eq!(row.get("right_part").and_then(|v| v.as_str()), Some("ef"));
    assert_eq!(row.get("lpad_value").and_then(|v| v.as_str()), Some("007"));
    assert_eq!(row.get("rpad_value").and_then(|v| v.as_str()), Some("x.."));
    assert_eq!(row.get("locate_value").and_then(|v| v.as_u64()), Some(5));
    assert_eq!(row.get("instr_value").and_then(|v| v.as_u64()), Some(2));
    assert_eq!(row.get("position_value").and_then(|v| v.as_u64()), Some(2));
    assert_eq!(
        row.get("reverse_value").and_then(|v| v.as_str()),
        Some("cba")
    );
    assert_eq!(
        row.get("repeat_value").and_then(|v| v.as_str()),
        Some("ababab")
    );
    assert_eq!(row.get("ascii_value").and_then(|v| v.as_u64()), Some(65));
    assert_eq!(row.get("greatest_value").and_then(|v| v.as_i64()), Some(9));
    assert_eq!(row.get("least_value").and_then(|v| v.as_i64()), Some(3));
    assert_eq!(row.get("sign_value").and_then(|v| v.as_i64()), Some(-1));
    assert_eq!(row.get("sqrt_value").and_then(|v| v.as_i64()), Some(3));
    assert_eq!(row.get("log_value").and_then(|v| v.as_i64()), Some(3));
    assert_eq!(row.get("exp_value").and_then(|v| v.as_i64()), Some(1));
    assert_eq!(
        row.get("truncate_value").and_then(|v| v.as_f64()),
        Some(3.14)
    );
    assert_eq!(row.get("mod_value").and_then(|v| v.as_i64()), Some(2));
    assert_eq!(
        row.get("cast_date").and_then(|v| v.as_str()),
        Some("2026-04-05")
    );
    assert_eq!(
        row.get("cast_time").and_then(|v| v.as_str()),
        Some("06:07:08")
    );
    assert_eq!(row.get("cast_json"), Some(&json!({"a": 1})));
    assert_eq!(row.get("convert_signed").and_then(|v| v.as_i64()), Some(42));
}

#[test]
fn evaluates_group_concat_order_separator_and_multi_distinct_count() {
    let engine = Engine::default();

    engine
        .execute_sql("CREATE TABLE metrics (name TEXT, score INT);")
        .unwrap();
    engine
        .execute_sql(
            "INSERT INTO metrics (name, score) VALUES ('low', 1), ('high', 3), ('mid', 2), ('mid', 2);",
        )
        .unwrap();

    let out = engine
        .execute_sql(
            "SELECT \
                GROUP_CONCAT(name ORDER BY score DESC SEPARATOR '|') AS ordered_names, \
                GROUP_CONCAT(DISTINCT name ORDER BY name ASC SEPARATOR ',') AS distinct_names, \
                COUNT(DISTINCT name, score) AS distinct_pairs \
            FROM metrics;",
        )
        .unwrap();
    let row = &out[0].rows[0];

    assert_eq!(
        row.get("ordered_names").and_then(|v| v.as_str()),
        Some("high|mid|mid|low")
    );
    assert_eq!(
        row.get("distinct_names").and_then(|v| v.as_str()),
        Some("high,low,mid")
    );
    assert_eq!(row.get("distinct_pairs").and_then(|v| v.as_u64()), Some(3));
}
