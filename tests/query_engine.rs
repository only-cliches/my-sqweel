mod common;

use common::test_lock;
use my_sqweel::sql::engine::{Engine, EngineConfig, FailureInjectionConfig};

#[test]
fn supports_naive_join_and_basic_introspection() {
    let _guard = test_lock();
    let engine = Engine::default();
    engine
        .execute_sql("CREATE TABLE users (id BIGINT PRIMARY KEY AUTO_INCREMENT, email VARCHAR(255), UNIQUE(email));")
        .unwrap();
    engine
        .execute_sql("CREATE TABLE posts (id BIGINT PRIMARY KEY AUTO_INCREMENT, user_id BIGINT, title TEXT);")
        .unwrap();
    engine
        .execute_sql("INSERT INTO users (email) VALUES ('a@example.com'),('b@example.com');")
        .unwrap();
    engine
        .execute_sql("INSERT INTO posts (user_id, title) VALUES (1,'p1'),(1,'p2'),(2,'p3');")
        .unwrap();

    let joined = engine
        .execute_sql("SELECT users.id, posts.title FROM users LEFT JOIN posts ON posts.user_id = users.id WHERE users.id = 1")
        .unwrap();
    assert_eq!(joined[0].rows.len(), 2);

    let info_tables = engine
        .execute_sql("SELECT table_name FROM information_schema.tables")
        .unwrap();
    assert!(info_tables[0].rows.len() >= 2);

    let info_cols = engine
        .execute_sql("SELECT table_name, column_name FROM information_schema.columns")
        .unwrap();
    assert!(!info_cols[0].rows.is_empty());
}

#[test]
fn supports_alias_qualified_joins_without_ambiguous_fallbacks() {
    let _guard = test_lock();
    let engine = Engine::default();
    engine
        .execute_sql("CREATE TABLE users (id BIGINT PRIMARY KEY AUTO_INCREMENT, email TEXT);")
        .unwrap();
    engine
        .execute_sql("CREATE TABLE posts (id BIGINT PRIMARY KEY AUTO_INCREMENT, user_id BIGINT, title TEXT);")
        .unwrap();
    engine
        .execute_sql("INSERT INTO users (email) VALUES ('a@example.com');")
        .unwrap();
    engine
        .execute_sql("INSERT INTO posts (user_id, title) VALUES (1, 'first'), (1, 'second');")
        .unwrap();

    let joined = engine
        .execute_sql("SELECT u.email, p.title FROM users AS u JOIN posts AS p ON p.user_id = u.id WHERE p.id = 2")
        .unwrap();
    assert_eq!(joined[0].rows.len(), 1);
    assert_eq!(
        joined[0].rows[0]
            .get("p.title")
            .and_then(|value| value.as_str()),
        Some("second")
    );
}

#[test]
fn supports_order_limit_offset_count_truncate_and_params() {
    let _guard = test_lock();
    let engine = Engine::default();
    engine
        .execute_sql("CREATE TABLE users (id BIGINT PRIMARY KEY AUTO_INCREMENT, email VARCHAR(255), score BIGINT);")
        .unwrap();
    engine
        .execute_sql(
            "INSERT INTO users (email, score) VALUES ('low@example.com', 10), ('high@example.com', 30), ('mid@example.com', 20);",
        )
        .unwrap();

    let ordered = engine
        .execute_sql("SELECT email FROM users ORDER BY score DESC LIMIT 1 OFFSET 1")
        .unwrap();
    assert_eq!(
        ordered[0].rows[0].get("email").unwrap().as_str().unwrap(),
        "mid@example.com"
    );

    let count = engine
        .execute_sql("SELECT count(*) FROM users WHERE score >= 20")
        .unwrap();
    assert_eq!(
        count[0].rows[0].get("count(*)").unwrap().as_u64().unwrap(),
        2
    );

    let prepared_like = engine
        .execute_sql_with_params(
            "SELECT email FROM users WHERE score = ?",
            &[serde_json::json!(30)],
        )
        .unwrap();
    assert_eq!(
        prepared_like[0].rows[0]
            .get("email")
            .unwrap()
            .as_str()
            .unwrap(),
        "high@example.com"
    );

    engine.execute_sql("TRUNCATE TABLE users").unwrap();
    let after_truncate = engine.execute_sql("SELECT count(*) FROM users").unwrap();
    assert_eq!(
        after_truncate[0].rows[0]
            .get("count(*)")
            .unwrap()
            .as_u64()
            .unwrap(),
        0
    );

    engine
        .execute_sql("INSERT INTO users (email, score) VALUES ('reset@example.com', 1)")
        .unwrap();
    let reset = engine.execute_sql("SELECT id FROM users").unwrap();
    assert_eq!(reset[0].rows[0].get("id").unwrap().as_i64().unwrap(), 1);
}

#[test]
fn supports_last_insert_id_and_scalar_expressions() {
    let _guard = test_lock();
    let engine = Engine::default();
    engine
        .execute_sql("CREATE TABLE users (id BIGINT PRIMARY KEY AUTO_INCREMENT, name TEXT, nickname TEXT, score BIGINT DEFAULT 10);")
        .unwrap();

    let insert = engine
        .execute_sql("INSERT INTO users (name, nickname) VALUES ('Alice', NULL), ('Bob', 'bobby');")
        .unwrap();
    assert_eq!(insert[0].rows_affected, 2);
    assert_eq!(insert[0].last_insert_id, 1);

    let last_insert_id = engine
        .execute_sql("SELECT LAST_INSERT_ID() AS inserted")
        .unwrap();
    assert_eq!(
        last_insert_id[0].rows[0].get("inserted").unwrap().as_u64(),
        Some(1)
    );

    let scalar = engine
        .execute_sql("SELECT 1 + 2 AS total, CONCAT('w', 'db') AS label, COALESCE(NULL, 'fallback') AS fallback")
        .unwrap();
    assert_eq!(scalar[0].rows[0].get("total").unwrap().as_i64(), Some(3));
    assert_eq!(
        scalar[0].rows[0].get("label").unwrap().as_str(),
        Some("wdb")
    );
    assert_eq!(
        scalar[0].rows[0].get("fallback").unwrap().as_str(),
        Some("fallback")
    );

    let row_expr = engine
        .execute_sql("SELECT id, score + 5 AS bumped, CONCAT(name, '-', IFNULL(nickname, 'none')) AS label FROM users WHERE id = 1")
        .unwrap();
    assert_eq!(
        row_expr[0].rows[0].get("bumped").unwrap().as_i64(),
        Some(15)
    );
    assert_eq!(
        row_expr[0].rows[0].get("label").unwrap().as_str(),
        Some("Alice-none")
    );

    let ignored = engine
        .execute_sql("INSERT IGNORE INTO users (id, name) VALUES (1, 'Ignored');")
        .unwrap();
    assert_eq!(ignored[0].rows_affected, 0);
    assert_eq!(ignored[0].last_insert_id, 0);
}

#[test]
fn supports_returning_on_write_statements() {
    let _guard = test_lock();
    let engine = Engine::default();
    engine
        .execute_sql(
            "CREATE TABLE users (id BIGINT PRIMARY KEY AUTO_INCREMENT, name TEXT, score BIGINT DEFAULT 10);",
        )
        .unwrap();

    let inserted = engine
        .execute_sql(
            "INSERT INTO users (name, score) VALUES ('Alice', 7), ('Bob', 11) RETURNING id, name, score + 1 AS next_score;",
        )
        .unwrap();
    assert_eq!(inserted[0].rows_affected, 2);
    assert_eq!(inserted[0].last_insert_id, 1);
    assert_eq!(inserted[0].columns, vec!["id", "name", "next_score"]);
    assert_eq!(
        inserted[0].rows[0].get("id").and_then(|v| v.as_i64()),
        Some(1)
    );
    assert_eq!(
        inserted[0].rows[0].get("name").and_then(|v| v.as_str()),
        Some("Alice")
    );
    assert_eq!(
        inserted[0].rows[0]
            .get("next_score")
            .and_then(|v| v.as_i64()),
        Some(8)
    );

    let updated = engine
        .execute_sql(
            "UPDATE users SET score = score + 5 WHERE name = 'Bob' RETURNING id, score AS updated_score;",
        )
        .unwrap();
    assert_eq!(updated[0].rows_affected, 1);
    assert_eq!(
        updated[0].rows[0]
            .get("updated_score")
            .and_then(|v| v.as_i64()),
        Some(16)
    );

    let deleted = engine
        .execute_sql("DELETE FROM users WHERE score < 10 RETURNING *;")
        .unwrap();
    assert_eq!(deleted[0].rows_affected, 1);
    assert_eq!(deleted[0].columns, vec!["id", "name", "score"]);
    assert_eq!(
        deleted[0].rows[0].get("name").and_then(|v| v.as_str()),
        Some("Alice")
    );

    let remaining = engine.execute_sql("SELECT name FROM users").unwrap();
    assert_eq!(remaining[0].rows.len(), 1);
    assert_eq!(
        remaining[0].rows[0].get("name").and_then(|v| v.as_str()),
        Some("Bob")
    );
}

#[test]
fn supports_contains_like_and_order_by_projection_alias() {
    let _guard = test_lock();
    let engine = Engine::default();
    engine
        .execute_sql("CREATE TABLE users (id BIGINT PRIMARY KEY AUTO_INCREMENT, email VARCHAR(255), score BIGINT);")
        .unwrap();
    engine
        .execute_sql("INSERT INTO users (email, score) VALUES ('alpha@example.com', 10), ('beta@example.com', 30), ('gamma@test.com', 20);")
        .unwrap();

    let contains = engine
        .execute_sql("SELECT email FROM users WHERE email LIKE '%example%' ORDER BY id")
        .unwrap();
    assert_eq!(contains[0].rows.len(), 2);

    let ordered = engine
        .execute_sql("SELECT email, score + 1 AS bumped FROM users ORDER BY bumped DESC LIMIT 1")
        .unwrap();
    assert_eq!(
        ordered[0].rows[0]
            .get("email")
            .and_then(|value| value.as_str()),
        Some("beta@example.com")
    );

    engine
        .execute_sql("INSERT INTO users (email, score) VALUES (NULL, 40);")
        .unwrap();
    let null_like = engine
        .execute_sql("SELECT id FROM users WHERE email LIKE '%' ORDER BY id")
        .unwrap();
    assert_eq!(null_like[0].rows.len(), 3);
}

#[test]
fn update_validates_unique_constraints_and_rekeys_primary_key_rows() {
    let _guard = test_lock();
    let engine = Engine::new(EngineConfig {
        unique_mode: my_sqweel::sql::engine::UniqueMode::Enforce,
        ..EngineConfig::default()
    });
    engine
        .execute_sql(
            "CREATE TABLE users (id BIGINT PRIMARY KEY AUTO_INCREMENT, email TEXT, UNIQUE(email));",
        )
        .unwrap();
    engine
        .execute_sql("INSERT INTO users (email) VALUES ('a@example.com'), ('b@example.com');")
        .unwrap();

    assert!(
        engine
            .execute_sql("UPDATE users SET email = 'a@example.com' WHERE email = 'b@example.com'")
            .is_err()
    );

    engine
        .execute_sql("UPDATE users SET id = 10 WHERE email = 'a@example.com'")
        .unwrap();
    let moved = engine
        .execute_sql("SELECT email FROM users WHERE id = 10")
        .unwrap();
    assert_eq!(
        moved[0].rows[0]
            .get("email")
            .and_then(|value| value.as_str()),
        Some("a@example.com")
    );
    let old_key = engine
        .execute_sql("SELECT email FROM users WHERE id = 1")
        .unwrap();
    assert!(old_key[0].rows.is_empty());
}

#[test]
fn system_variable_projection_does_not_capture_table_queries() {
    let _guard = test_lock();
    let engine = Engine::default();
    engine
        .execute_sql("CREATE TABLE users (id BIGINT PRIMARY KEY AUTO_INCREMENT, email TEXT);")
        .unwrap();
    engine
        .execute_sql("INSERT INTO users (email) VALUES ('a@example.com');")
        .unwrap();

    let rows = engine
        .execute_sql("SELECT @@version AS version, email FROM users")
        .unwrap();
    assert_eq!(rows[0].rows.len(), 1);
    assert_eq!(
        rows[0].rows[0]
            .get("email")
            .and_then(|value| value.as_str()),
        Some("a@example.com")
    );
}

#[test]
fn delete_returns_predicate_errors_instead_of_suppressing_them() {
    let _guard = test_lock();
    let engine = Engine::default();
    engine
        .execute_sql("CREATE TABLE users (id BIGINT PRIMARY KEY AUTO_INCREMENT, email TEXT);")
        .unwrap();
    engine
        .execute_sql("INSERT INTO users (email) VALUES ('a@example.com');")
        .unwrap();

    let err = engine
        .execute_sql(
            "DELETE FROM users WHERE EXISTS (SELECT id FROM users INTERSECT SELECT id FROM users)",
        )
        .unwrap_err();
    assert!(err.to_string().contains("unsupported set operation"));
}

#[test]
fn supports_conditional_and_date_functions() {
    let _guard = test_lock();
    let engine = Engine::default();

    let scalar = engine
        .execute_sql(
            "SELECT IF(1 = 1, 'ok', 'nope') AS branch_ok, NULLIF('x', 'x') AS nullified, DATE('2026-02-03 04:05:06') AS d, YEAR('2026-02-03') AS y, MONTH('2026-02-03') AS m, DAY('2026-02-03') AS day",
        )
        .unwrap();
    let row = &scalar[0].rows[0];
    assert_eq!(row.get("branch_ok").and_then(|v| v.as_str()), Some("ok"));
    assert!(row.get("nullified").is_some_and(|value| value.is_null()));
    assert_eq!(row.get("d").and_then(|v| v.as_str()), Some("2026-02-03"));
    assert_eq!(row.get("y").and_then(|v| v.as_i64()), Some(2026));
    assert_eq!(row.get("m").and_then(|v| v.as_i64()), Some(2));
    assert_eq!(row.get("day").and_then(|v| v.as_i64()), Some(3));
}

#[test]
fn supports_mysql_compatibility_query_edges() {
    let _guard = test_lock();
    let engine = Engine::default();
    engine
        .execute_sql(
            "CREATE TABLE users (id BIGINT PRIMARY KEY AUTO_INCREMENT, email TEXT, nickname TEXT, score BIGINT, active BOOL);",
        )
        .unwrap();
    engine
        .execute_sql(
            "INSERT INTO users (email, nickname, score, active) VALUES \
             ('a@example.com', NULL, 10, true), \
             ('b@example.com', 'bee', 20, false), \
             ('c@example.com', NULL, 30, true);",
        )
        .unwrap();

    let predicates = engine
        .execute_sql(
            "SELECT email FROM users \
             WHERE score BETWEEN 10 AND 30 \
             AND id NOT IN (2) \
             AND email LIKE '_@example.com' \
             AND active \
             ORDER BY score DESC;",
        )
        .unwrap();
    assert_eq!(predicates[0].rows.len(), 2);
    assert_eq!(
        predicates[0].rows[0].get("email").and_then(|v| v.as_str()),
        Some("c@example.com")
    );
    assert_eq!(
        predicates[0].rows[1].get("email").and_then(|v| v.as_str()),
        Some("a@example.com")
    );

    let projections = engine
        .execute_sql(
            "SELECT id, \
             CASE WHEN nickname IS NULL THEN 'missing' ELSE nickname END AS nick_state, \
             score + 5 AS bumped \
             FROM users ORDER BY bumped DESC;",
        )
        .unwrap();
    assert_eq!(
        projections[0].rows[0]
            .get("nick_state")
            .and_then(|v| v.as_str()),
        Some("missing")
    );
    assert_eq!(
        projections[0].rows[0]
            .get("bumped")
            .and_then(|v| v.as_i64()),
        Some(35)
    );

    engine
        .execute_sql("CREATE TABLE high_scores (email TEXT, doubled BIGINT);")
        .unwrap();
    let inserted = engine
        .execute_sql(
            "INSERT INTO high_scores (email, doubled) \
             SELECT email, score * 2 FROM users WHERE score >= 20;",
        )
        .unwrap();
    assert_eq!(inserted[0].rows_affected, 2);

    let copied = engine
        .execute_sql("SELECT email, doubled FROM high_scores ORDER BY doubled")
        .unwrap();
    assert_eq!(
        copied[0].rows[0].get("email").and_then(|v| v.as_str()),
        Some("b@example.com")
    );
    assert_eq!(
        copied[0].rows[1].get("doubled").and_then(|v| v.as_i64()),
        Some(60)
    );
}

#[test]
fn supports_failure_injection_knobs() {
    let _guard = test_lock();

    let engine = Engine::new(EngineConfig {
        failure_injection: FailureInjectionConfig {
            fail_write_every: 2,
            ..FailureInjectionConfig::default()
        },
        ..EngineConfig::default()
    });
    engine
        .execute_sql(
            "CREATE TABLE users (id BIGINT PRIMARY KEY AUTO_INCREMENT, email VARCHAR(255));",
        )
        .unwrap();
    let write_err = engine
        .execute_sql("INSERT INTO users (email) VALUES ('blocked@example.com')")
        .unwrap_err()
        .to_string();
    assert!(write_err.contains("simulated write failure"));

    let engine = Engine::new(EngineConfig {
        failure_injection: FailureInjectionConfig {
            fail_read_every: 1,
            ..FailureInjectionConfig::default()
        },
        ..EngineConfig::default()
    });
    let read_err = engine
        .execute_sql("SELECT 1 AS ok")
        .unwrap_err()
        .to_string();
    assert!(read_err.contains("simulated read failure"));
}

#[test]
fn supports_group_by_and_aggregate_expressions() {
    let _guard = test_lock();
    let engine = Engine::default();
    engine
        .execute_sql(
            "CREATE TABLE events (id BIGINT PRIMARY KEY AUTO_INCREMENT, kind TEXT, score BIGINT);",
        )
        .unwrap();
    engine
        .execute_sql("INSERT INTO events (kind, score) VALUES ('a', 10), ('a', 20), ('b', 30);")
        .unwrap();

    let grouped = engine
        .execute_sql("SELECT kind, COUNT(*) AS n, SUM(score) AS total, MIN(score) AS min_score, MAX(score) AS max_score FROM events GROUP BY kind HAVING n >= 2 ORDER BY total DESC")
        .unwrap();
    assert_eq!(grouped[0].rows.len(), 1);
    assert_eq!(grouped[0].rows[0].get("kind").unwrap().as_str(), Some("a"));
    assert_eq!(grouped[0].rows[0].get("n").unwrap().as_u64(), Some(2));
    assert_eq!(grouped[0].rows[0].get("total").unwrap().as_i64(), Some(30));
    assert_eq!(
        grouped[0].rows[0].get("min_score").unwrap().as_i64(),
        Some(10)
    );
    assert_eq!(
        grouped[0].rows[0].get("max_score").unwrap().as_i64(),
        Some(20)
    );

    let global = engine
        .execute_sql("SELECT AVG(score) AS avg_score, COUNT(DISTINCT kind) AS kinds FROM events")
        .unwrap();
    assert_eq!(
        global[0].rows[0].get("avg_score").unwrap().as_i64(),
        Some(20)
    );
    assert_eq!(global[0].rows[0].get("kinds").unwrap().as_u64(), Some(2));
}

#[test]
fn supports_common_subquery_shapes() {
    let _guard = test_lock();
    let engine = Engine::default();
    engine
        .execute_sql("CREATE TABLE users (id BIGINT PRIMARY KEY AUTO_INCREMENT, email VARCHAR(255), score BIGINT);")
        .unwrap();
    engine
        .execute_sql("CREATE TABLE posts (id BIGINT PRIMARY KEY AUTO_INCREMENT, user_id BIGINT, title TEXT);")
        .unwrap();
    engine
        .execute_sql("INSERT INTO users (email, score) VALUES ('a@example.com', 10), ('b@example.com', 20), ('c@example.com', 30);")
        .unwrap();
    engine
        .execute_sql("INSERT INTO posts (user_id, title) VALUES (1, 'p1'), (3, 'p3');")
        .unwrap();

    let in_subquery = engine
        .execute_sql("SELECT email FROM users WHERE id IN (SELECT user_id FROM posts WHERE title LIKE 'p%') ORDER BY id")
        .unwrap();
    assert_eq!(in_subquery[0].rows.len(), 2);
    assert_eq!(
        in_subquery[0].rows[0].get("email").unwrap().as_str(),
        Some("a@example.com")
    );
    assert_eq!(
        in_subquery[0].rows[1].get("email").unwrap().as_str(),
        Some("c@example.com")
    );

    let scalar = engine
        .execute_sql("SELECT (SELECT COUNT(*) FROM posts) AS post_count")
        .unwrap();
    assert_eq!(
        scalar[0].rows[0].get("post_count").unwrap().as_u64(),
        Some(2)
    );

    let derived = engine
        .execute_sql("SELECT d.email FROM (SELECT email, score FROM users WHERE score >= 20) AS d WHERE d.score = 20")
        .unwrap();
    assert_eq!(derived[0].rows.len(), 1);
    assert_eq!(
        derived[0].rows[0].get("d.email").unwrap().as_str(),
        Some("b@example.com")
    );

    let exists = engine
        .execute_sql("SELECT email FROM users WHERE EXISTS (SELECT id FROM posts WHERE user_id = 1) ORDER BY id LIMIT 1")
        .unwrap();
    assert_eq!(
        exists[0].rows[0].get("email").unwrap().as_str(),
        Some("a@example.com")
    );
}
