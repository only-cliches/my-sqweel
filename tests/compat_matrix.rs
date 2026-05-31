use std::sync::{Mutex, MutexGuard, OnceLock};

use my_sqweel::sql::engine::Engine;

fn test_lock() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|err| err.into_inner())
}

#[test]
fn orm_compat_matrix_core_queries() {
    let _guard = test_lock();
    let engine = Engine::default();

    engine
        .execute_sql("CREATE TABLE users (id BIGINT PRIMARY KEY AUTO_INCREMENT, email VARCHAR(255), nickname TEXT, score BIGINT, created_at TEXT);")
        .unwrap();
    engine
        .execute_sql("CREATE TABLE posts (id BIGINT PRIMARY KEY AUTO_INCREMENT, user_id BIGINT, title TEXT);")
        .unwrap();
    engine
        .execute_sql("INSERT INTO users (email, nickname, score, created_at) VALUES ('a@example.com', NULL, 10, '2026-01-02 10:20:30'), ('b@example.com', 'bee', 20, '2026-01-03 11:22:33');")
        .unwrap();
    engine
        .execute_sql("INSERT INTO posts (user_id, title) VALUES (1, 'hello'), (2, 'world');")
        .unwrap();

    let matrix = [
        "SELECT id, email FROM users ORDER BY id",
        "SELECT id, IFNULL(nickname, 'none') AS nick FROM users ORDER BY id",
        "SELECT id, IF(score >= 20, 'high', 'low') AS bucket FROM users ORDER BY id",
        "SELECT DATE(created_at) AS d, YEAR(created_at) AS y, MONTH(created_at) AS m, DAY(created_at) AS day FROM users ORDER BY id",
        "SELECT users.email, posts.title FROM users LEFT JOIN posts ON posts.user_id = users.id ORDER BY users.id",
        "SELECT score, COUNT(*) AS n FROM users GROUP BY score HAVING n >= 1 ORDER BY score",
        "SELECT AVG(score) AS avg_score, COUNT(DISTINCT email) AS unique_emails FROM users",
        "SELECT email FROM users WHERE id IN (SELECT user_id FROM posts) ORDER BY id",
    ];

    for sql in matrix {
        let out = engine.execute_sql(sql);
        assert!(out.is_ok(), "compat matrix query failed: {sql}: {out:?}");
    }
}
