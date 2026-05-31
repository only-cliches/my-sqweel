use std::net::TcpListener;
use std::process::Command;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use my_sqweel::server::WireServer;
use my_sqweel::sql::engine::Engine;
use mysql::prelude::Queryable;
use mysql::{Opts, Pool, Row, Value as MyValue};

const MYSQL_DOCKER_IMAGE: &str = "mysql:8";
const MYSQL_DOCKER_PASSWORD: &str = "my-sqweel";
const MYSQL_DOCKER_DATABASE: &str = "test";

enum MysqlTarget {
    External(String),
    Docker(DockerMysql),
}

impl MysqlTarget {
    fn url(&self) -> &str {
        match self {
            MysqlTarget::External(url) => url,
            MysqlTarget::Docker(container) => &container.url,
        }
    }
}

struct DockerMysql {
    name: String,
    url: String,
}

impl Drop for DockerMysql {
    fn drop(&mut self) {
        let _ = Command::new("docker")
            .args(["rm", "-f", &self.name])
            .output();
    }
}

fn mysql_compare_target() -> Option<MysqlTarget> {
    if let Ok(url) = std::env::var("MYSQL_COMPARE_URL") {
        return Some(MysqlTarget::External(url));
    }

    match start_docker_mysql() {
        Ok(container) => Some(MysqlTarget::Docker(container)),
        Err(err) => {
            eprintln!("skipping parity test: {err}");
            None
        }
    }
}

fn start_docker_mysql() -> Result<DockerMysql, String> {
    if !docker_available() {
        return Err(
            "MYSQL_COMPARE_URL is not set and Docker is not available from this test process"
                .to_string(),
        );
    }
    if !docker_image_available(MYSQL_DOCKER_IMAGE) {
        return Err(format!(
            "MYSQL_COMPARE_URL is not set and Docker image {MYSQL_DOCKER_IMAGE:?} is not local; run `docker pull {MYSQL_DOCKER_IMAGE}` first"
        ));
    }

    let name = format!(
        "my-sqweel-mysql-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_millis())
            .unwrap_or_default()
    );
    let output = Command::new("docker")
        .args([
            "run",
            "-d",
            "--rm",
            "--name",
            &name,
            "-e",
            &format!("MYSQL_ROOT_PASSWORD={MYSQL_DOCKER_PASSWORD}"),
            "-e",
            &format!("MYSQL_DATABASE={MYSQL_DOCKER_DATABASE}"),
            "-p",
            "127.0.0.1::3306",
            MYSQL_DOCKER_IMAGE,
        ])
        .output()
        .map_err(|err| format!("failed to start Docker MySQL: {err}"))?;
    if !output.status.success() {
        return Err(format!(
            "failed to start Docker MySQL: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }

    let port = docker_container_port(&name)?;
    let url =
        format!("mysql://root:{MYSQL_DOCKER_PASSWORD}@127.0.0.1:{port}/{MYSQL_DOCKER_DATABASE}");
    wait_for_mysql(&url).map_err(|err| {
        let _ = Command::new("docker").args(["logs", &name]).status();
        let _ = Command::new("docker").args(["rm", "-f", &name]).status();
        err
    })?;

    Ok(DockerMysql { name, url })
}

fn docker_available() -> bool {
    Command::new("docker")
        .args(["info", "--format", "{{.ServerVersion}}"])
        .output()
        .is_ok_and(|output| output.status.success())
}

fn docker_image_available(image: &str) -> bool {
    Command::new("docker")
        .args(["image", "inspect", image])
        .output()
        .is_ok_and(|output| output.status.success())
}

fn docker_container_port(name: &str) -> Result<u16, String> {
    let output = Command::new("docker")
        .args(["port", name, "3306/tcp"])
        .output()
        .map_err(|err| format!("failed to inspect Docker MySQL port: {err}"))?;
    if !output.status.success() {
        return Err(format!(
            "failed to inspect Docker MySQL port: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let endpoint = stdout
        .lines()
        .next()
        .ok_or_else(|| "Docker did not publish MySQL port 3306".to_string())?;
    endpoint
        .rsplit_once(':')
        .and_then(|(_, port)| port.trim().parse::<u16>().ok())
        .ok_or_else(|| format!("could not parse Docker MySQL port from {endpoint:?}"))
}

fn wait_for_mysql(url: &str) -> Result<(), String> {
    let opts = Opts::from_url(url).map_err(|err| format!("invalid Docker MySQL URL: {err}"))?;
    for _ in 0..90 {
        if let Ok(pool) = Pool::new(opts.clone()) {
            if pool.get_conn().is_ok() {
                return Ok(());
            }
        }
        thread::sleep(Duration::from_secs(1));
    }
    Err("Docker MySQL did not become ready within 90 seconds".to_string())
}

fn start_whatever_server() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind local port");
    let addr = listener.local_addr().expect("local addr");
    drop(listener);

    let bind_addr = addr;
    thread::spawn(move || {
        let engine = std::sync::Arc::new(Engine::default());
        let wire = WireServer::new(engine);
        wire.serve(bind_addr).expect("wire server should run");
    });

    thread::sleep(Duration::from_millis(120));
    format!("mysql://root@127.0.0.1:{}/test", addr.port())
}

fn fetch_rows(conn: &mut mysql::PooledConn, sql: &str) -> mysql::Result<Vec<Vec<String>>> {
    let rows: Vec<Row> = conn.query(sql)?;
    Ok(rows.into_iter().map(normalize_row).collect())
}

fn fetch_prepared_rows<P: Into<mysql::Params>>(
    conn: &mut mysql::PooledConn,
    sql: &str,
    params: P,
) -> mysql::Result<Vec<Vec<String>>> {
    let rows: Vec<Row> = conn.exec(sql, params)?;
    Ok(rows.into_iter().map(normalize_row).collect())
}

fn normalize_row(row: Row) -> Vec<String> {
    row.unwrap().into_iter().map(normalize_value).collect()
}

fn normalize_value(value: MyValue) -> String {
    match value {
        MyValue::NULL => "NULL".to_string(),
        MyValue::Bytes(v) => String::from_utf8_lossy(&v).to_string(),
        MyValue::Int(v) => v.to_string(),
        MyValue::UInt(v) => v.to_string(),
        MyValue::Float(v) => format!("{v:.6}"),
        MyValue::Double(v) => format!("{v:.6}"),
        MyValue::Date(y, m, d, hh, mm, ss, us) => {
            format!("{y:04}-{m:02}-{d:02} {hh:02}:{mm:02}:{ss:02}.{us:06}")
        }
        MyValue::Time(is_neg, d, h, m, s, us) => {
            let total_hours = d * 24 + u32::from(h);
            format!(
                "{}{:02}:{:02}:{:02}.{:06}",
                if is_neg { "-" } else { "" },
                total_hours,
                m,
                s,
                us
            )
        }
    }
}

fn exec_drop_with_stats(conn: &mut mysql::PooledConn, sql: &str) -> mysql::Result<(u64, u64)> {
    conn.query_drop(sql)?;
    Ok((conn.affected_rows(), conn.last_insert_id()))
}

fn exec_prepared_drop_with_stats<P: Into<mysql::Params>>(
    conn: &mut mysql::PooledConn,
    sql: &str,
    params: P,
) -> mysql::Result<(u64, u64)> {
    conn.exec_drop(sql, params)?;
    Ok((conn.affected_rows(), conn.last_insert_id()))
}

fn assert_query_parity(mysql: &mut mysql::PooledConn, whatever: &mut mysql::PooledConn, sql: &str) {
    let mysql_rows = fetch_rows(mysql, sql).expect("mysql select");
    let whatever_rows = fetch_rows(whatever, sql).expect("whatever select");
    assert_eq!(whatever_rows, mysql_rows, "query mismatch: {sql}");
}

fn assert_query_parity_unordered(
    mysql: &mut mysql::PooledConn,
    whatever: &mut mysql::PooledConn,
    sql: &str,
) {
    let mut mysql_rows = fetch_rows(mysql, sql).expect("mysql select");
    let mut whatever_rows = fetch_rows(whatever, sql).expect("whatever select");
    mysql_rows.sort();
    whatever_rows.sort();
    assert_eq!(whatever_rows, mysql_rows, "query mismatch: {sql}");
}

fn assert_exec_parity(mysql: &mut mysql::PooledConn, whatever: &mut mysql::PooledConn, sql: &str) {
    let mysql_stats = exec_drop_with_stats(mysql, sql).expect("mysql exec");
    let whatever_stats = exec_drop_with_stats(whatever, sql).expect("whatever exec");
    assert_eq!(
        whatever_stats.0, mysql_stats.0,
        "rows_affected mismatch for: {sql}"
    );
}

fn assert_prepared_query_parity<P: Into<mysql::Params> + Clone>(
    mysql: &mut mysql::PooledConn,
    whatever: &mut mysql::PooledConn,
    sql: &str,
    params: P,
) {
    let mysql_rows = fetch_prepared_rows(mysql, sql, params.clone()).expect("mysql prepared");
    let whatever_rows = fetch_prepared_rows(whatever, sql, params).expect("whatever prepared");
    assert_eq!(whatever_rows, mysql_rows, "prepared query mismatch: {sql}");
}

fn assert_prepared_exec_parity<P: Into<mysql::Params> + Clone>(
    mysql: &mut mysql::PooledConn,
    whatever: &mut mysql::PooledConn,
    sql: &str,
    params: P,
) {
    let mysql_stats =
        exec_prepared_drop_with_stats(mysql, sql, params.clone()).expect("mysql prepared exec");
    let whatever_stats =
        exec_prepared_drop_with_stats(whatever, sql, params).expect("whatever prepared exec");
    assert_eq!(
        whatever_stats.0, mysql_stats.0,
        "prepared rows_affected mismatch for: {sql}"
    );
}

#[test]
fn parity_with_mysql_for_supported_semantics() {
    let Some(mysql_target) = mysql_compare_target() else {
        return;
    };
    let mysql_url = mysql_target.url();

    let whatever_url = start_whatever_server();

    let mysql_pool = Pool::new(Opts::from_url(mysql_url).expect("valid MySQL compare URL"))
        .expect("connect to mysql");
    let whatever_pool = Pool::new(Opts::from_url(&whatever_url).expect("valid MySqweel URL"))
        .expect("connect to my-sqweel");

    let mut mysql_conn = mysql_pool.get_conn().expect("mysql conn");
    let mut whatever_conn = whatever_pool.get_conn().expect("whatever conn");

    let pid = std::process::id();
    let users = format!("wdb_parity_users_{pid}");
    let posts = format!("wdb_parity_posts_{pid}");
    let parents = format!("wdb_parity_parents_{pid}");
    let children = format!("wdb_parity_children_{pid}");
    let scratch = format!("wdb_parity_scratch_{pid}");
    let posts_archive = format!("wdb_parity_posts_archive_{pid}");

    for sql in [
        format!("DROP TABLE IF EXISTS {children}"),
        format!("DROP TABLE IF EXISTS {parents}"),
        format!("DROP TABLE IF EXISTS {posts_archive}"),
        format!("DROP TABLE IF EXISTS {posts}"),
        format!("DROP TABLE IF EXISTS {users}"),
        format!("DROP DATABASE IF EXISTS {scratch}"),
    ] {
        let _ = mysql_conn.query_drop(&sql);
        let _ = whatever_conn.query_drop(&sql);
    }

    // Database compatibility commands should succeed on both backends.
    assert_exec_parity(
        &mut mysql_conn,
        &mut whatever_conn,
        &format!("CREATE DATABASE {scratch}"),
    );
    assert_query_parity_unordered(&mut mysql_conn, &mut whatever_conn, "SHOW DATABASES");

    assert_exec_parity(
        &mut mysql_conn,
        &mut whatever_conn,
        &format!(
            "CREATE TABLE {users} (id BIGINT PRIMARY KEY AUTO_INCREMENT, email VARCHAR(255) UNIQUE NOT NULL, name TEXT, nickname TEXT, score BIGINT DEFAULT 10, created_at TEXT, legacy TEXT)"
        ),
    );
    assert_exec_parity(
        &mut mysql_conn,
        &mut whatever_conn,
        &format!(
            "CREATE TABLE {posts} (id BIGINT PRIMARY KEY AUTO_INCREMENT, user_id BIGINT, title TEXT)"
        ),
    );
    assert_exec_parity(
        &mut mysql_conn,
        &mut whatever_conn,
        &format!("CREATE INDEX idx_{users}_score ON {users} (score)"),
    );
    assert_exec_parity(
        &mut mysql_conn,
        &mut whatever_conn,
        &format!(
            "INSERT INTO {users} (email, name, nickname, score, created_at, legacy) VALUES ('a@example.com', 'Alice', NULL, 10, '2026-01-02 10:20:30', 'drop-me'), ('b@example.com', 'Bob', 'bee', 20, '2026-01-03 11:22:33', 'drop-me'), ('c@example.com', 'Cara', NULL, 30, '2026-01-04 12:24:36', 'drop-me')"
        ),
    );
    assert_exec_parity(
        &mut mysql_conn,
        &mut whatever_conn,
        &format!("INSERT INTO {posts} (user_id, title) VALUES (1, 'p1'), (1, 'p2'), (3, 'p3')"),
    );

    assert_exec_parity(
        &mut mysql_conn,
        &mut whatever_conn,
        &format!("ALTER TABLE {users} ADD COLUMN display_name TEXT"),
    );
    assert_exec_parity(
        &mut mysql_conn,
        &mut whatever_conn,
        &format!("UPDATE {users} SET display_name = CONCAT(name, '!') WHERE id <= 2"),
    );
    assert_exec_parity(
        &mut mysql_conn,
        &mut whatever_conn,
        &format!("ALTER TABLE {users} RENAME COLUMN display_name TO handle"),
    );
    assert_exec_parity(
        &mut mysql_conn,
        &mut whatever_conn,
        &format!("ALTER TABLE {users} MODIFY COLUMN handle VARCHAR(128) NOT NULL"),
    );
    assert_exec_parity(
        &mut mysql_conn,
        &mut whatever_conn,
        &format!("ALTER TABLE {users} DROP COLUMN legacy"),
    );

    assert_query_parity(
        &mut mysql_conn,
        &mut whatever_conn,
        &format!("SELECT id, email, name, score, handle FROM {users} ORDER BY id"),
    );
    assert_query_parity(
        &mut mysql_conn,
        &mut whatever_conn,
        &format!("SELECT email FROM {users} ORDER BY score DESC LIMIT 1 OFFSET 1"),
    );
    assert_query_parity(
        &mut mysql_conn,
        &mut whatever_conn,
        &format!(
            "SELECT COUNT(*) AS n, SUM(score) AS total, AVG(score) AS avg_score, MIN(score) AS min_score, MAX(score) AS max_score FROM {users}"
        ),
    );
    assert_query_parity(
        &mut mysql_conn,
        &mut whatever_conn,
        &format!(
            "SELECT score, COUNT(*) AS n FROM {users} GROUP BY score HAVING n >= 1 ORDER BY score"
        ),
    );
    assert_query_parity(
        &mut mysql_conn,
        &mut whatever_conn,
        &format!(
            "SELECT {users}.id, {posts}.title FROM {users} LEFT JOIN {posts} ON {posts}.user_id = {users}.id WHERE {users}.id = 1 ORDER BY {posts}.title"
        ),
    );
    assert_query_parity(
        &mut mysql_conn,
        &mut whatever_conn,
        &format!(
            "SELECT id, IFNULL(nickname, 'none') AS nick, COALESCE(nickname, name, 'fallback') AS label, NULLIF(name, 'Alice') AS not_alice FROM {users} ORDER BY id"
        ),
    );
    assert_query_parity(
        &mut mysql_conn,
        &mut whatever_conn,
        &format!(
            "SELECT id, CONCAT_WS('-', email, nickname, name) AS label, LOWER(name) AS lower_name, UPPER(name) AS upper_name, TRIM(CONCAT(' ', name, ' ')) AS trimmed, LENGTH(name) AS name_len FROM {users} ORDER BY id"
        ),
    );
    assert_query_parity(
        &mut mysql_conn,
        &mut whatever_conn,
        &format!(
            "SELECT id, ABS(score - 25) AS distance, ROUND(score / 3, 2) AS rounded, CAST(score AS CHAR) AS score_text FROM {users} ORDER BY id"
        ),
    );
    assert_query_parity(
        &mut mysql_conn,
        &mut whatever_conn,
        &format!(
            "SELECT id, YEAR(created_at) AS y, MONTH(created_at) AS m, DAY(created_at) AS d FROM {users} ORDER BY id"
        ),
    );
    assert_query_parity(
        &mut mysql_conn,
        &mut whatever_conn,
        &format!(
            "SELECT COUNT(*) AS all_rows, COUNT(nickname) AS nick_rows, COUNT(DISTINCT nickname) AS nick_distinct FROM {users}"
        ),
    );
    assert_query_parity(
        &mut mysql_conn,
        &mut whatever_conn,
        &format!("SELECT email FROM {users} WHERE nickname IS NULL ORDER BY id"),
    );
    assert_query_parity(
        &mut mysql_conn,
        &mut whatever_conn,
        &format!("SELECT email FROM {users} WHERE nickname IS NOT NULL OR score >= 30 ORDER BY id"),
    );
    assert_query_parity(
        &mut mysql_conn,
        &mut whatever_conn,
        &format!(
            "SELECT email FROM {users} WHERE NOT (score < 20) AND email IN ('b@example.com', 'c@example.com') ORDER BY id"
        ),
    );
    assert_query_parity(
        &mut mysql_conn,
        &mut whatever_conn,
        &format!("SELECT email FROM {users} WHERE email LIKE '_@example.com' ORDER BY email"),
    );

    // Nested SELECT parity coverage.
    assert_query_parity(
        &mut mysql_conn,
        &mut whatever_conn,
        &format!(
            "SELECT email FROM {users} WHERE id IN (SELECT user_id FROM {posts} WHERE title LIKE 'p%') ORDER BY id"
        ),
    );
    assert_query_parity(
        &mut mysql_conn,
        &mut whatever_conn,
        &format!(
            "SELECT email FROM {users} WHERE EXISTS (SELECT id FROM {posts} WHERE user_id = 1) ORDER BY id LIMIT 1"
        ),
    );
    assert_query_parity(
        &mut mysql_conn,
        &mut whatever_conn,
        &format!("SELECT (SELECT COUNT(*) FROM {posts}) AS post_count"),
    );
    assert_query_parity(
        &mut mysql_conn,
        &mut whatever_conn,
        &format!(
            "SELECT d.email FROM (SELECT email, score FROM {users} WHERE score >= 20) AS d WHERE d.score = 20"
        ),
    );
    assert_query_parity(
        &mut mysql_conn,
        &mut whatever_conn,
        &format!(
            "SELECT id, (SELECT MAX(score) FROM {users}) AS max_score FROM {users} WHERE id = 1"
        ),
    );

    assert_prepared_query_parity(
        &mut mysql_conn,
        &mut whatever_conn,
        &format!("SELECT email FROM {users} WHERE id = ?"),
        (1_u64,),
    );
    assert_prepared_query_parity(
        &mut mysql_conn,
        &mut whatever_conn,
        &format!("SELECT email FROM {users} WHERE score >= ? AND name != ? ORDER BY id"),
        (20_u64, "Cara"),
    );
    assert_prepared_exec_parity(
        &mut mysql_conn,
        &mut whatever_conn,
        &format!(
            "INSERT INTO {users} (email, name, nickname, score, handle, created_at) VALUES (?, ?, ?, ?, ?, ?)"
        ),
        (
            "d@example.com",
            "Dana",
            Option::<&str>::None,
            40_u64,
            "Dana!",
            "2026-01-05 13:14:15",
        ),
    );
    assert_query_parity(
        &mut mysql_conn,
        &mut whatever_conn,
        &format!(
            "SELECT email, name, nickname, score, handle FROM {users} WHERE email = 'd@example.com'"
        ),
    );
    assert_exec_parity(
        &mut mysql_conn,
        &mut whatever_conn,
        &format!(
            "UPDATE {users} SET score = score + 5, handle = CONCAT(name, '?') WHERE email = 'd@example.com'"
        ),
    );
    assert_query_parity(
        &mut mysql_conn,
        &mut whatever_conn,
        &format!("SELECT score, handle FROM {users} WHERE email = 'd@example.com'"),
    );
    assert_exec_parity(
        &mut mysql_conn,
        &mut whatever_conn,
        &format!(
            "INSERT INTO {users} (email, name, handle) VALUES ('e@example.com', 'Eve', 'Eve!')"
        ),
    );
    assert_query_parity(
        &mut mysql_conn,
        &mut whatever_conn,
        &format!(
            "SELECT email, score, nickname, created_at FROM {users} WHERE email = 'e@example.com'"
        ),
    );
    assert_exec_parity(
        &mut mysql_conn,
        &mut whatever_conn,
        &format!("DELETE FROM {users} WHERE email = 'e@example.com'"),
    );
    assert_query_parity(
        &mut mysql_conn,
        &mut whatever_conn,
        &format!("SELECT COUNT(*) AS n FROM {users} WHERE email = 'e@example.com'"),
    );

    assert_exec_parity(
        &mut mysql_conn,
        &mut whatever_conn,
        &format!(
            "INSERT IGNORE INTO {users} (email, name, score, handle) VALUES ('a@example.com', 'Ignored', 99, 'Ignored!')"
        ),
    );
    assert_exec_parity(
        &mut mysql_conn,
        &mut whatever_conn,
        &format!(
            "INSERT INTO {users} (email, name, score, handle) VALUES ('a@example.com', 'Updated', 11, 'Updated!') ON DUPLICATE KEY UPDATE name = VALUES(name), score = VALUES(score), handle = VALUES(handle)"
        ),
    );
    assert_exec_parity(
        &mut mysql_conn,
        &mut whatever_conn,
        &format!(
            "REPLACE INTO {users} (email, name, score, handle) VALUES ('a@example.com', 'Replaced', 12, 'Replaced!')"
        ),
    );
    assert_query_parity(
        &mut mysql_conn,
        &mut whatever_conn,
        &format!("SELECT email, name, score FROM {users} WHERE email = 'a@example.com'"),
    );
    assert_query_parity(
        &mut mysql_conn,
        &mut whatever_conn,
        "SELECT LAST_INSERT_ID() AS last_insert_id",
    );

    // SHOW/DESCRIBE and information_schema checks for claimed metadata compatibility.
    assert_query_parity_unordered(&mut mysql_conn, &mut whatever_conn, &format!("SHOW TABLES"));
    assert_query_parity_unordered(
        &mut mysql_conn,
        &mut whatever_conn,
        &format!("SHOW COLUMNS FROM {users}"),
    );
    assert_query_parity_unordered(
        &mut mysql_conn,
        &mut whatever_conn,
        &format!("DESCRIBE {users}"),
    );
    assert_query_parity_unordered(
        &mut mysql_conn,
        &mut whatever_conn,
        &format!(
            "SELECT table_name, column_name, ordinal_position, is_nullable, column_default, column_type, column_key, extra FROM information_schema.columns WHERE table_name = '{users}' ORDER BY ordinal_position"
        ),
    );
    assert_query_parity_unordered(
        &mut mysql_conn,
        &mut whatever_conn,
        &format!(
            "SELECT table_name, index_name, column_name, seq_in_index, non_unique FROM information_schema.statistics WHERE table_name = '{users}' ORDER BY index_name, seq_in_index"
        ),
    );
    assert_query_parity_unordered(
        &mut mysql_conn,
        &mut whatever_conn,
        &format!(
            "SELECT table_name FROM information_schema.tables WHERE table_name IN ('{users}', '{posts}') ORDER BY table_name"
        ),
    );
    assert_query_parity_unordered(
        &mut mysql_conn,
        &mut whatever_conn,
        &format!(
            "SELECT schema_name FROM information_schema.schemata WHERE schema_name IN ('app', '{MYSQL_DOCKER_DATABASE}')"
        ),
    );
    assert_query_parity_unordered(
        &mut mysql_conn,
        &mut whatever_conn,
        &format!("SHOW INDEX FROM {users}"),
    );
    let show_create_sql = format!("SHOW CREATE TABLE {users}");
    let mysql_show_create =
        fetch_rows(&mut mysql_conn, &show_create_sql).expect("mysql show create");
    let whatever_show_create =
        fetch_rows(&mut whatever_conn, &show_create_sql).expect("whatever show create");
    assert_eq!(
        mysql_show_create.len(),
        1,
        "unexpected mysql show create row count"
    );
    assert_eq!(
        whatever_show_create.len(),
        1,
        "unexpected whatever show create row count"
    );
    for create in [&mysql_show_create[0][1], &whatever_show_create[0][1]] {
        let upper = create.to_ascii_uppercase();
        assert!(
            upper.contains("CREATE TABLE"),
            "missing CREATE TABLE: {create}"
        );
        assert!(
            upper.contains("PRIMARY KEY"),
            "missing PRIMARY KEY: {create}"
        );
        assert!(upper.contains("UNIQUE"), "missing UNIQUE: {create}");
    }

    // Advisory foreign key metadata parity.
    assert_exec_parity(
        &mut mysql_conn,
        &mut whatever_conn,
        &format!("CREATE TABLE {parents} (id BIGINT PRIMARY KEY AUTO_INCREMENT)"),
    );
    assert_exec_parity(
        &mut mysql_conn,
        &mut whatever_conn,
        &format!(
            "CREATE TABLE {children} (id BIGINT PRIMARY KEY AUTO_INCREMENT, parent_id BIGINT, CONSTRAINT fk_children_parent FOREIGN KEY (parent_id) REFERENCES {parents} (id) ON DELETE CASCADE ON UPDATE RESTRICT)"
        ),
    );
    assert_query_parity_unordered(
        &mut mysql_conn,
        &mut whatever_conn,
        &format!(
            "SELECT table_name, constraint_name, constraint_type FROM information_schema.table_constraints WHERE table_name = '{children}' AND constraint_name = 'fk_children_parent'"
        ),
    );
    assert_query_parity_unordered(
        &mut mysql_conn,
        &mut whatever_conn,
        &format!(
            "SELECT constraint_name, column_name, referenced_table_name, referenced_column_name FROM information_schema.key_column_usage WHERE constraint_name = 'fk_children_parent' ORDER BY column_name"
        ),
    );
    assert_query_parity_unordered(
        &mut mysql_conn,
        &mut whatever_conn,
        &format!(
            "SELECT constraint_name, delete_rule, update_rule FROM information_schema.referential_constraints WHERE constraint_name = 'fk_children_parent'"
        ),
    );

    assert_exec_parity(
        &mut mysql_conn,
        &mut whatever_conn,
        &format!("RENAME TABLE {posts} TO {posts_archive}"),
    );
    assert_exec_parity(
        &mut mysql_conn,
        &mut whatever_conn,
        &format!("TRUNCATE TABLE {posts_archive}"),
    );
    assert_query_parity(
        &mut mysql_conn,
        &mut whatever_conn,
        &format!("SELECT COUNT(*) AS n FROM {posts_archive}"),
    );
    assert_exec_parity(
        &mut mysql_conn,
        &mut whatever_conn,
        &format!("DROP INDEX idx_{users}_score ON {users}"),
    );
    assert_query_parity_unordered(
        &mut mysql_conn,
        &mut whatever_conn,
        &format!(
            "SELECT index_name FROM information_schema.statistics WHERE table_name = '{users}' AND index_name = 'idx_{users}_score'"
        ),
    );

    assert_exec_parity(
        &mut mysql_conn,
        &mut whatever_conn,
        &format!("DROP TABLE IF EXISTS {children}"),
    );
    assert_exec_parity(
        &mut mysql_conn,
        &mut whatever_conn,
        &format!("DROP TABLE IF EXISTS {parents}"),
    );
    assert_exec_parity(
        &mut mysql_conn,
        &mut whatever_conn,
        &format!("DROP TABLE IF EXISTS {posts_archive}"),
    );
    assert_exec_parity(
        &mut mysql_conn,
        &mut whatever_conn,
        &format!("DROP TABLE IF EXISTS {users}"),
    );
    assert_exec_parity(
        &mut mysql_conn,
        &mut whatever_conn,
        &format!("DROP DATABASE IF EXISTS {scratch}"),
    );
}

#[test]
fn unsupported_queries_return_mysql_errors() {
    let whatever_url = start_whatever_server();
    let whatever_pool =
        Pool::new(Opts::from_url(&whatever_url).expect("valid MySqweel URL")).expect("connect");
    let mut conn = whatever_pool.get_conn().expect("whatever conn");

    let err = conn
        .query_drop("SELECT * FROM (SELECT 1) AS a JOIN (SELECT 2) AS b ON 1 = 1")
        .expect_err("unsupported derived-table join should return a MySQL error");
    let message = err.to_string();
    assert!(
        message.contains("unsupported") || message.contains("not supported"),
        "unexpected error message: {message}"
    );
}
