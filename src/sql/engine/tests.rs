use super::{Engine, EngineConfig, QueryEvent, QueryEventOptions, UniqueMode};
use chrono::{Duration, NaiveDateTime, Utc};
use serde_json::json;
use std::time::Duration as StdDuration;

#[test]
fn ordinary_selects_use_the_direct_parser_path() {
    let engine = Engine::default();
    assert!(engine.can_parse_without_compat_rewrites("SELECT 1 AS value"));
    assert!(engine.can_parse_without_compat_rewrites(
        "select id from users where id = 1 order by id limit 1"
    ));
    assert!(!engine.can_parse_without_compat_rewrites("SELECT SQL_NO_CACHE id FROM users"));
    assert!(
        !engine.can_parse_without_compat_rewrites("SELECT id FROM users FORCE INDEX (PRIMARY)")
    );
    assert!(!engine.can_parse_without_compat_rewrites("CREATE TABLE users (id INT)"));

    // If an otherwise ordinary SELECT fails direct parsing, retain the old
    // compatibility rewrite fallback rather than surfacing a new parser error.
    let duplicate_distinct = engine
        .execute_sql("SELECT DISTINCT   DISTINCT 1 AS value")
        .unwrap();
    assert_eq!(duplicate_distinct[0].rows[0]["value"], 1);
}

#[test]
fn supports_mysql_user_variables_and_server_prepared_statements() {
    let engine = Engine::new(EngineConfig::mysql_strict());
    let result = engine
        .execute_sql(
            "SET @a=7; SELECT @a, @a+1; \
             SET @sql='SELECT ? + 1'; PREPARE p FROM @sql; \
             SET @b=4; EXECUTE p USING @b; DEALLOCATE PREPARE p;",
        )
        .expect("user variables and prepared statements should execute");
    assert_eq!(result[1].rows[0]["@a"], 7);
    assert_eq!(result[1].rows[0]["@a+1"], 8);
    assert_eq!(result[5].rows[0].values().next(), Some(&json!(5)));
}

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
fn exposes_mtr_metadata_and_ignores_mtr_suppression_calls() {
    let engine = Engine::new(EngineConfig::mysql_strict());

    let variables = engine
        .execute_sql("SHOW GLOBAL VARIABLES")
        .unwrap()
        .remove(0);
    assert_eq!(variables.rows[0]["Variable_name"], "version");
    assert_eq!(
        variables.rows[0]["Value"],
        "8.0.0-my-sqweel-intentkit-tx-v1"
    );

    assert!(
        engine
            .execute_sql("CALL mtr.add_suppression('expected warning')")
            .is_ok()
    );
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
        Some("2026-02-28")
    );
    assert_eq!(
        row.get("minus_day").and_then(|v| v.as_str()),
        Some("2026-02-28")
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
                CONVERT('42', SIGNED) AS convert_signed, \
                JSON_SET('{\"a\":[1,2]}', '$.a[1]', 99) AS json_set_array;",
        )
        .unwrap();
    let row = &out[0].rows[0];

    assert_eq!(row.get("json_name"), Some(&json!("\"Ada\"")));
    assert_eq!(row.get("unquoted_name"), Some(&json!("Ada")));
    assert_eq!(
        row.get("json_object"),
        Some(&json!({"name": "Ada", "age": 36}))
    );
    assert_eq!(row.get("json_array"), Some(&json!([1, "two", null])));
    assert_eq!(row.get("json_contains").and_then(|v| v.as_i64()), Some(1));
    assert_eq!(row.get("json_set"), Some(&json!({"a": 1, "b": 2})));
    assert_eq!(row.get("json_set_array"), Some(&json!({"a": [1, 99]})));
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
        Some(314.0 / 100.0)
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
fn json_constructors_preserve_typed_column_values_when_nested() {
    let engine = Engine::new(EngineConfig::mysql_strict());
    engine
        .execute_sql(
            "CREATE TABLE json_constructor_values (\
                id BIGINT PRIMARY KEY, username TEXT, score BIGINT, payload TEXT\
             ); \
             INSERT INTO json_constructor_values VALUES \
                (1, 'Ada', 10, '{\"tier\":\"pro\"}')",
        )
        .unwrap();

    let out = engine
        .execute_sql(
            "SELECT \
                JSON_OBJECT(\
                    'name', username, \
                    'score', score, \
                    'tier', JSON_UNQUOTE(JSON_EXTRACT(payload, '$.tier'))\
                ) AS object_value, \
                JSON_EXTRACT(\
                    JSON_OBJECT(\
                        'name', username, \
                        'score', score, \
                        'tier', JSON_UNQUOTE(JSON_EXTRACT(payload, '$.tier'))\
                    ), \
                    '$.score'\
                ) AS object_score, \
                JSON_EXTRACT(JSON_ARRAY(username, score), '$[1]') AS array_score, \
                'score' AS literal_value \
             FROM json_constructor_values",
        )
        .unwrap();

    assert_eq!(
        out[0].rows[0].get("object_value"),
        Some(&json!({"name": "Ada", "score": 10, "tier": "pro"}))
    );
    assert_eq!(out[0].rows[0].get("object_score"), Some(&json!("10")));
    assert_eq!(out[0].rows[0].get("array_score"), Some(&json!("10")));
    assert_eq!(out[0].rows[0].get("literal_value"), Some(&json!("score")));
}

#[test]
fn evaluates_extended_json_functions() {
    let engine = Engine::new(EngineConfig::mysql_strict());
    let result = engine
        .execute_sql(
            "SELECT \
                JSON_TYPE('{\"a\":[1,2]}') AS json_type, \
                JSON_DEPTH('{\"a\":[1,2]}') AS json_depth, \
                JSON_LENGTH('{\"a\":[1,2]}', '$.a') AS json_length, \
                JSON_KEYS('{\"a\":1,\"b\":2}') AS json_keys, \
                JSON_CONTAINS_PATH('{\"a\":1}', 'one', '$.a') AS contains_path, \
                JSON_OVERLAPS('[1,2]', '[2,3]') AS overlaps, \
                JSON_VALID('{bad json}') AS invalid_json, \
                JSON_QUOTE('Ada') AS quoted, \
                JSON_SEARCH('{\"name\":\"Ada\",\"other\":\"Bob\"}', 'one', 'A%') AS found_path, \
                JSON_VALUE('{\"score\":42}', '$.score') AS json_value, \
                JSON_SCHEMA_VALID('{\"type\":\"object\",\"required\":[\"a\"]}', '{\"a\":1}') AS schema_valid, \
                JSON_STORAGE_SIZE('{\"a\":1}') AS storage_size, \
                JSON_STORAGE_FREE('{\"a\":1}') AS storage_free, \
                JSON_EXTRACT(JSON_ARRAY_APPEND('[1,2]', '$', 3), '$[2]') AS appended, \
                JSON_EXTRACT(JSON_ARRAY_INSERT('[1,3]', '$[1]', 2), '$') AS inserted, \
                JSON_EXTRACT(JSON_MERGE_PATCH('{\"a\":1,\"b\":2}', '{\"a\":9,\"b\":null}'), '$') AS merged"
        )
        .expect("extended JSON functions should execute");
    let row = &result[0].rows[0];
    assert_eq!(row["json_type"], "OBJECT");
    assert_eq!(row["json_depth"], 3);
    assert_eq!(row["json_length"], 2);
    assert_eq!(row["contains_path"], 1);
    assert_eq!(row["overlaps"], 1);
    assert_eq!(row["invalid_json"], 0);
    assert_eq!(row["quoted"], "\"Ada\"");
    assert_eq!(row["found_path"], "\"$.name\"");
    assert_eq!(row["json_value"], "42");
    assert_eq!(row["schema_valid"], 1);
    assert_eq!(row["storage_size"], 7);
    assert_eq!(row["storage_free"], 0);
    assert_eq!(row["appended"], "3");
    assert_eq!(row["inserted"], "[1,2,3]");
    assert_eq!(row["merged"], "{\"a\":9}");

    engine
        .execute_sql(
            "CREATE TABLE json_aggregate_rows (name TEXT, score INT); \
             INSERT INTO json_aggregate_rows VALUES ('Ada', 10), ('Bob', 20);",
        )
        .expect("JSON aggregate input should load");
    let result = engine
        .execute_sql(
            "SELECT JSON_ARRAYAGG(score) AS scores, JSON_OBJECTAGG(name, score) AS score_map \
             FROM json_aggregate_rows",
        )
        .expect("JSON aggregate functions should execute");
    let row = &result[0].rows[0];
    assert_eq!(row["scores"], json!([10, 20]));
    assert_eq!(row["score_map"], json!({"Ada": 10, "Bob": 20}));

    let result = engine
        .execute_sql(
            "SELECT jt.ord, jt.name, jt.score \
             FROM JSON_TABLE(\'{\"items\":[{\"name\":\"Ada\",\"score\":10},{\"name\":\"Bob\",\"score\":20}]}\', \
                 \'$.items[*]\' COLUMNS (\
                     ord FOR ORDINALITY, \
                     name VARCHAR(20) PATH \'$.name\', \
                     score INT PATH \'$.score\'\
                 )) AS jt \
             ORDER BY jt.ord",
        )
        .expect("basic JSON_TABLE should execute");
    assert_eq!(result[0].rows.len(), 2);
    assert_eq!(result[0].rows[0]["ord"], 1);
    assert_eq!(result[0].rows[0]["name"], "Ada");
    assert_eq!(result[0].rows[1]["score"], 20);
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

#[test]
fn order_by_uses_mysql_declared_type_rules_for_non_integer_columns() {
    let engine = Engine::new(EngineConfig::mysql_strict());
    engine
        .execute_sql(
            "CREATE TABLE sort_types (
                id INT PRIMARY KEY,
                decimal_value DECIMAL(12,3),
                time_value TIME(6),
                enum_value ENUM('low','Medium','HIGH'),
                set_value SET('red','Green','blue'),
                json_value JSON
            )",
        )
        .unwrap();
    engine
        .execute_sql(
            "INSERT INTO sort_types VALUES
                (1, 10.000, '02:00:00', 'HIGH', 'red,blue', '10'),
                (2, 2.500, '10:00:00', 'low', 'Green', '\"text\"'),
                (3, NULL, NULL, NULL, NULL, NULL),
                (4, 10.000, '03:00:00', 'Medium', 'red', 'true')",
        )
        .unwrap();

    let ids = |sql: &str| {
        engine
            .execute_sql(sql)
            .unwrap()
            .remove(0)
            .rows
            .into_iter()
            .map(|row| row["id"].as_i64().unwrap())
            .collect::<Vec<_>>()
    };

    assert_eq!(
        ids("SELECT id FROM sort_types ORDER BY decimal_value, id"),
        [3, 2, 1, 4]
    );
    assert_eq!(
        ids("SELECT id FROM sort_types ORDER BY decimal_value DESC, id"),
        [1, 4, 2, 3]
    );
    assert_eq!(
        ids("SELECT id FROM sort_types ORDER BY time_value, id"),
        [3, 1, 4, 2]
    );
    assert_eq!(
        ids("SELECT id FROM sort_types ORDER BY enum_value, id"),
        [3, 2, 4, 1]
    );
    assert_eq!(
        ids("SELECT id FROM sort_types ORDER BY set_value, id"),
        [3, 4, 2, 1]
    );
    assert_eq!(
        ids("SELECT id FROM sort_types ORDER BY json_value, id"),
        [3, 1, 2, 4]
    );
    assert_eq!(
        ids("SELECT id FROM sort_types ORDER BY decimal_value, time_value DESC, id"),
        [3, 2, 4, 1]
    );
}

#[test]
fn compound_sorting_covers_precision_collation_temporal_enum_set_json_and_all_paths() {
    let engine = Engine::new(EngineConfig::mysql_strict());
    engine
        .execute_sql(
            "CREATE TABLE sort_edges (
                id INT PRIMARY KEY,
                group_id INT,
                tie INT,
                unsigned_value BIGINT UNSIGNED,
                decimal_value DECIMAL(20,6),
                float_value FLOAT,
                text_value VARCHAR(32),
                binary_value BINARY(3),
                varbinary_value VARBINARY(3),
                time_value TIME(6),
                datetime_value DATETIME(6),
                timestamp_value TIMESTAMP(6),
                enum_value ENUM('low','Medium','HIGH'),
                set_value SET('red','Green','blue'),
                json_value JSON
            )",
        )
        .unwrap();
    engine
        .execute_sql(
            "INSERT INTO sort_edges VALUES
                (1, 1, 2, 9007199254740993, 1.000001, 16777217, 'Éclair', 'a', 'a', '838:59:59.999999', '2024-01-01 00:00:00.000001', '2024-01-01 00:00:00.000001', 'low', 'red', 'null'),
                (2, 1, 1, 9007199254740992, 1.000000, 16777216, 'eclair ', 'a ', 'a ', '-838:59:59.999999', '2024-01-01 00:00:00.000002', '2024-01-01 00:00:00.000002', 'Medium', 'Green', '1'),
                (3, 1, 1, 18446744073709551615, -0.000001, 16777217, '', 'ab', 'ab', '00:00:00.000000', '2024-01-01 00:00:00.000003', '2024-01-01 00:00:00.000003', 'HIGH', '', NULL),
                (4, 2, 2, 2, 10.250000, 1.5, 'ECLAIR', 'b', 'b', '12:00:00', '2024-01-02 00:00:00', '2024-01-02 00:00:00', 'low', 'red,Green', '[1,2]'),
                (5, 2, 1, 1, 10.249999, 1.5, 'z', 'aa', 'aa', '01:02:03.123456', '2023-12-31 23:59:59.999999', '2023-12-31 23:59:59.999999', 'Medium', 'blue', '{}')",
        )
        .unwrap();

    let ids = |sql: &str| {
        engine
            .execute_sql(sql)
            .unwrap()
            .remove(0)
            .rows
            .into_iter()
            .map(|row| row["id"].as_i64().unwrap())
            .collect::<Vec<_>>()
    };

    assert_eq!(
        ids("SELECT id FROM sort_edges ORDER BY unsigned_value ASC, group_id DESC, tie DESC, id"),
        [5, 4, 2, 1, 3]
    );
    assert_eq!(
        ids("SELECT id FROM sort_edges ORDER BY decimal_value ASC, text_value DESC, id"),
        [3, 2, 1, 5, 4]
    );
    assert_eq!(
        ids("SELECT id FROM sort_edges ORDER BY float_value ASC, id"),
        [4, 5, 1, 2, 3]
    );
    assert_eq!(
        ids("SELECT id FROM sort_edges ORDER BY text_value ASC, id"),
        [3, 1, 2, 4, 5]
    );
    assert_eq!(
        ids("SELECT id FROM sort_edges ORDER BY binary_value ASC, varbinary_value DESC, id"),
        [1, 2, 5, 3, 4]
    );
    assert_eq!(
        ids("SELECT id FROM sort_edges ORDER BY time_value ASC, datetime_value DESC, id"),
        [2, 3, 5, 4, 1]
    );
    assert_eq!(
        ids("SELECT id FROM sort_edges ORDER BY enum_value ASC, set_value ASC, id"),
        [1, 4, 2, 5, 3]
    );
    assert_eq!(
        ids("SELECT id FROM sort_edges ORDER BY json_value ASC, id"),
        [3, 1, 2, 5, 4]
    );
    assert_eq!(
        ids("SELECT id, decimal_value AS amount FROM sort_edges ORDER BY amount ASC, id"),
        [3, 2, 1, 5, 4]
    );
    assert_eq!(
        ids("SELECT id, decimal_value + 0 AS amount FROM sort_edges ORDER BY amount ASC, id"),
        [3, 2, 1, 5, 4]
    );

    let window_ids = ids(
        "SELECT id FROM (
            SELECT id, ROW_NUMBER() OVER (ORDER BY decimal_value ASC, unsigned_value DESC, id) AS row_order
            FROM sort_edges
        ) AS ordered_rows
        ORDER BY row_order",
    );
    assert_eq!(window_ids, [3, 2, 1, 5, 4]);

    let group_concat = engine
        .execute_sql("SELECT GROUP_CONCAT(id ORDER BY unsigned_value ASC SEPARATOR ',') AS ids FROM sort_edges")
        .unwrap()
        .remove(0)
        .rows
        .remove(0);
    assert_eq!(group_concat["ids"].as_str(), Some("5,4,2,1,3"));

    assert_eq!(
        ids(
            "SELECT id FROM (SELECT id, decimal_value FROM sort_edges) AS d ORDER BY decimal_value, id"
        ),
        [3, 2, 1, 5, 4]
    );
    assert_eq!(
        ids(
            "SELECT id FROM sort_edges WHERE id <= 2 UNION ALL SELECT id FROM sort_edges WHERE id >= 4 ORDER BY id DESC"
        ),
        [5, 4, 2, 1]
    );
    assert_eq!(
        ids(
            "SELECT id, unsigned_value AS ordering FROM sort_edges WHERE id <= 2 UNION ALL SELECT id, unsigned_value FROM sort_edges WHERE id >= 4 ORDER BY ordering DESC, id"
        ),
        [1, 2, 4, 5]
    );

    engine
        .execute_sql("DELETE FROM sort_edges ORDER BY unsigned_value DESC LIMIT 1")
        .unwrap();
    assert_eq!(
        ids("SELECT id FROM sort_edges ORDER BY unsigned_value DESC, id"),
        [1, 2, 4, 5]
    );
}

#[test]
fn query_events_report_lifecycle_timing_size_and_optional_results() {
    let engine = Engine::new(EngineConfig::mysql_strict());
    let metadata_stream = engine.subscribe_query_events(QueryEventOptions::default());

    let first_results = engine
        .execute_sql("SELECT 1 AS value")
        .expect("query should succeed");
    let received = metadata_stream.recv().expect("received event");
    let first_id = match received {
        QueryEvent::Received(event) => {
            assert_eq!(event.query, "SELECT 1 AS value");
            event.query_id
        }
        QueryEvent::Completed(_) => panic!("received event should come first"),
    };
    let completed = metadata_stream.recv().expect("completed event");
    match completed {
        QueryEvent::Completed(event) => {
            assert_eq!(event.query_id, first_id);
            assert_eq!(event.result_set_count, 1);
            assert_eq!(event.result_set_size, 1);
            assert!(event.duration <= StdDuration::from_secs(1));
            assert!(event.results.is_none());
            assert!(event.error.is_none());
        }
        QueryEvent::Received(_) => panic!("completed event should follow received event"),
    }
    assert_eq!(first_results.len(), 1);

    let payload_stream = engine.subscribe_query_events(QueryEventOptions::with_results());
    let expected_results = engine
        .execute_sql_with_params("SELECT ? AS value", &[json!(7)])
        .expect("prepared query should succeed");
    assert!(matches!(
        payload_stream.recv().expect("prepared received event"),
        QueryEvent::Received(event)
            if event.query == "SELECT ? AS value" && event.query_id > first_id
    ));
    let payload_completed = payload_stream.recv().expect("prepared completed event");
    match payload_completed {
        QueryEvent::Completed(event) => {
            assert_eq!(event.result_set_count, 1);
            assert_eq!(event.result_set_size, 1);
            let actual_results = event.results.expect("results should be included");
            assert_eq!(actual_results.len(), expected_results.len());
            assert_eq!(actual_results[0].rows, expected_results[0].rows);
            assert!(event.error.is_none());
        }
        QueryEvent::Received(_) => panic!("completed event should follow received event"),
    }

    let failure_stream = engine.subscribe_query_events(QueryEventOptions::metadata_only());
    assert!(
        engine
            .execute_sql("SELECT * FROM missing_query_event_table")
            .is_err()
    );
    let failure_received = failure_stream.recv().expect("failed received event");
    let failure_id = match failure_received {
        QueryEvent::Received(event) => event.query_id,
        QueryEvent::Completed(_) => panic!("received event should come first"),
    };
    match failure_stream.recv().expect("failed completed event") {
        QueryEvent::Completed(event) => {
            assert_eq!(event.query_id, failure_id);
            assert_eq!(event.result_set_count, 0);
            assert_eq!(event.result_set_size, 0);
            assert!(event.results.is_none());
            assert!(event.error.is_some());
        }
        QueryEvent::Received(_) => panic!("completed event should follow received event"),
    }
}

#[test]
fn query_events_report_logical_read_and_write_metrics() {
    let engine = Engine::new(EngineConfig::mysql_strict());
    engine
        .execute_sql("CREATE TABLE metric_rows (id INT PRIMARY KEY, category VARCHAR(16))")
        .unwrap();
    let stream = engine.subscribe_query_events(QueryEventOptions::metadata_only());

    engine
        .execute_sql("INSERT INTO metric_rows VALUES (1, 'a'), (2, 'b'), (3, 'a')")
        .unwrap();
    let _ = stream.recv().unwrap();
    let inserted = match stream.recv().unwrap() {
        QueryEvent::Completed(event) => event,
        QueryEvent::Received(_) => panic!("completed event should follow received event"),
    };
    assert_eq!(inserted.metrics.rows_read, 3);
    assert_eq!(inserted.metrics.cells_read, 6);
    assert_eq!(inserted.metrics.rows_written, 3);
    assert_eq!(inserted.metrics.cells_written, 6);

    let result = engine
        .execute_sql("SELECT id FROM metric_rows WHERE category = 'a'")
        .unwrap();
    assert_eq!(result[0].rows.len(), 2);
    let _ = stream.recv().unwrap();
    let selected = match stream.recv().unwrap() {
        QueryEvent::Completed(event) => event,
        QueryEvent::Received(_) => panic!("completed event should follow received event"),
    };
    assert_eq!(selected.metrics.rows_read, 3);
    assert_eq!(selected.metrics.cells_read, 6);
    assert_eq!(selected.metrics.rows_written, 0);
    assert_eq!(selected.metrics.cells_written, 0);

    engine
        .execute_sql("UPDATE metric_rows SET category = 'z' WHERE id = 1")
        .unwrap();
    let _ = stream.recv().unwrap();
    let updated = match stream.recv().unwrap() {
        QueryEvent::Completed(event) => event,
        QueryEvent::Received(_) => panic!("completed event should follow received event"),
    };
    assert!(updated.metrics.rows_read >= 3);
    assert!(updated.metrics.cells_read >= 6);
    assert_eq!(updated.metrics.rows_written, 1);
    assert_eq!(updated.metrics.cells_written, 1);

    engine
        .execute_sql("UPDATE metric_rows SET category = 'z' WHERE id = 1")
        .unwrap();
    let _ = stream.recv().unwrap();
    let unchanged = match stream.recv().unwrap() {
        QueryEvent::Completed(event) => event,
        QueryEvent::Received(_) => panic!("completed event should follow received event"),
    };
    assert!(unchanged.metrics.rows_read >= 3);
    assert!(unchanged.metrics.cells_read >= 6);
    assert_eq!(unchanged.metrics.rows_written, 0);
    assert_eq!(unchanged.metrics.cells_written, 0);

    engine
        .execute_sql("DELETE FROM metric_rows WHERE id = 2")
        .unwrap();
    let _ = stream.recv().unwrap();
    let deleted = match stream.recv().unwrap() {
        QueryEvent::Completed(event) => event,
        QueryEvent::Received(_) => panic!("completed event should follow received event"),
    };
    assert!(deleted.metrics.rows_read >= 3);
    assert!(deleted.metrics.cells_read >= 6);
    assert_eq!(deleted.metrics.rows_written, 1);
    assert_eq!(deleted.metrics.cells_written, 0);

    engine
        .execute_sql("SELECT id FROM metric_rows; SELECT id FROM metric_rows")
        .unwrap();
    let _ = stream.recv().unwrap();
    let multi_statement = match stream.recv().unwrap() {
        QueryEvent::Completed(event) => event,
        QueryEvent::Received(_) => panic!("completed event should follow received event"),
    };
    assert!(multi_statement.metrics.rows_read >= 4);
    assert!(multi_statement.metrics.cells_read >= 4);
}
