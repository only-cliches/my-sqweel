mod common;

use std::cell::RefCell;
use std::fmt::Debug;
use std::net::TcpListener;
use std::thread;

use my_sqweel::server::WireServer;
use my_sqweel::sql::engine::{Engine, EngineConfig};
use mysql::prelude::Queryable;
use mysql::{Opts, Pool, Row, Value as MyValue};

use common::{MYSQL_DOCKER_DATABASE, mysql_compare_target};

thread_local! {
    static PARITY_MISMATCHES: RefCell<Option<Vec<String>>> = const { RefCell::new(None) };
}

struct ParityMismatchCollector;

impl ParityMismatchCollector {
    fn new() -> Self {
        PARITY_MISMATCHES.with(|mismatches| {
            assert!(
                mismatches.borrow_mut().replace(Vec::new()).is_none(),
                "parity mismatch collector is already active"
            );
        });
        Self
    }
}

impl Drop for ParityMismatchCollector {
    fn drop(&mut self) {
        let mismatches = PARITY_MISMATCHES.with(|mismatches| {
            mismatches
                .borrow_mut()
                .take()
                .expect("parity mismatch collector disappeared")
        });
        if mismatches.is_empty() {
            return;
        }

        let report = format!(
            "{} MySQL parity mismatch(es):\n\n{}",
            mismatches.len(),
            mismatches.join("\n\n")
        );
        if thread::panicking() {
            eprintln!("{report}");
        } else {
            panic!("{report}");
        }
    }
}

fn assert_parity_eq<T: Debug + PartialEq>(actual: &T, expected: &T, context: &str) {
    if actual == expected {
        return;
    }

    let mismatch = format!("{context}\n  MySqweel: {actual:#?}\n  MySQL:    {expected:#?}");
    let collected = PARITY_MISMATCHES.with(|mismatches| {
        let mut mismatches = mismatches.borrow_mut();
        let Some(mismatches) = mismatches.as_mut() else {
            return false;
        };
        mismatches.push(mismatch.clone());
        true
    });
    if !collected {
        panic!("{mismatch}");
    }
}

fn start_whatever_server() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind local port");
    let addr = listener.local_addr().expect("local addr");

    thread::spawn(move || {
        let engine = std::sync::Arc::new(Engine::new(EngineConfig::mysql_strict()));
        engine.execute_sql("CREATE DATABASE test").unwrap();
        let wire = WireServer::new(engine);
        wire.serve_listener(listener)
            .expect("wire server should run");
    });

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
    assert_parity_eq(
        &whatever_rows,
        &mysql_rows,
        &format!("query mismatch: {sql}"),
    );
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
    assert_parity_eq(
        &whatever_rows,
        &mysql_rows,
        &format!("query mismatch: {sql}"),
    );
}

fn assert_show_index_parity(
    mysql: &mut mysql::PooledConn,
    whatever: &mut mysql::PooledConn,
    sql: &str,
) {
    let mut mysql_rows = fetch_rows(mysql, sql).expect("mysql show index");
    let mut whatever_rows = fetch_rows(whatever, sql).expect("whatever show index");
    // InnoDB Cardinality is an optimizer estimate and can vary between two
    // identical databases, so compare every stable SHOW INDEX field instead.
    for row in mysql_rows.iter_mut().chain(&mut whatever_rows) {
        if let Some(cardinality) = row.get_mut(6) {
            *cardinality = "<estimate>".to_string();
        }
    }
    mysql_rows.sort();
    whatever_rows.sort();
    assert_parity_eq(
        &whatever_rows,
        &mysql_rows,
        &format!("query mismatch: {sql}"),
    );
}

fn assert_exec_parity(mysql: &mut mysql::PooledConn, whatever: &mut mysql::PooledConn, sql: &str) {
    let mysql_stats = exec_drop_with_stats(mysql, sql).expect("mysql exec");
    let whatever_stats = exec_drop_with_stats(whatever, sql).expect("whatever exec");
    assert_parity_eq(
        &whatever_stats.0,
        &mysql_stats.0,
        &format!("rows_affected mismatch for: {sql}"),
    );
    assert_parity_eq(
        &whatever_stats.1,
        &mysql_stats.1,
        &format!("last_insert_id mismatch for: {sql}"),
    );
}

fn assert_exec_succeeds(
    mysql: &mut mysql::PooledConn,
    whatever: &mut mysql::PooledConn,
    sql: &str,
) {
    mysql.query_drop(sql).expect("mysql exec");
    whatever.query_drop(sql).expect("MySqweel exec");
}

fn assert_prepared_query_parity<P: Into<mysql::Params> + Clone>(
    mysql: &mut mysql::PooledConn,
    whatever: &mut mysql::PooledConn,
    sql: &str,
    params: P,
) {
    let mysql_rows = fetch_prepared_rows(mysql, sql, params.clone()).expect("mysql prepared");
    let whatever_rows = fetch_prepared_rows(whatever, sql, params).expect("whatever prepared");
    assert_parity_eq(
        &whatever_rows,
        &mysql_rows,
        &format!("prepared query mismatch: {sql}"),
    );
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
    assert_parity_eq(
        &whatever_stats.0,
        &mysql_stats.0,
        &format!("prepared rows_affected mismatch for: {sql}"),
    );
    assert_parity_eq(
        &whatever_stats.1,
        &mysql_stats.1,
        &format!("prepared last_insert_id mismatch for: {sql}"),
    );
}

#[test]
fn sorting_and_compound_sorting_match_mysql_for_all_primitives() {
    let _guard = common::test_lock();
    let _mismatches = ParityMismatchCollector::new();
    let mysql_target = mysql_compare_target();
    let whatever_url = start_whatever_server();
    let whatever_pool = Pool::new(Opts::from_url(&whatever_url).expect("valid MySqweel URL"))
        .expect("connect to my-sqweel");
    let mysql_pool = mysql_target.as_ref().map(|target| {
        Pool::new(Opts::from_url(target.url()).expect("valid MySQL URL")).expect("connect to mysql")
    });
    let mut mysql_conn = mysql_pool
        .as_ref()
        .map(|pool| pool.get_conn().expect("mysql conn"));
    let mut whatever_conn = whatever_pool.get_conn().expect("whatever conn");

    let table = format!("wdb_sort_primitives_{}", std::process::id());
    if let Some(mysql) = mysql_conn.as_mut() {
        let _ = mysql.query_drop(format!("DROP TABLE IF EXISTS {table}"));
    }
    let _ = whatever_conn.query_drop(format!("DROP TABLE IF EXISTS {table}"));

    let create = format!(
        "CREATE TABLE {table} (
            id BIGINT PRIMARY KEY,
            tiny_value TINYINT,
            small_value SMALLINT,
            medium_value MEDIUMINT,
            int_value INT,
            big_value BIGINT,
            unsigned_value BIGINT UNSIGNED,
            decimal_value DECIMAL(20,6),
            float_value FLOAT,
            double_value DOUBLE,
            bool_value BOOLEAN,
            bit_value BIT(8),
            year_value YEAR,
            date_value DATE,
            time_value TIME(6),
            datetime_value DATETIME(6),
            timestamp_value TIMESTAMP(6),
            char_value CHAR(8),
            varchar_value VARCHAR(8),
            text_value TEXT,
            binary_value BINARY(3),
            varbinary_value VARBINARY(3),
            blob_value BLOB,
            json_value JSON,
            enum_value ENUM('low','Medium','HIGH'),
            set_value SET('red','Green','blue')
        )"
    );
    if let Some(mysql) = mysql_conn.as_mut() {
        assert_exec_succeeds(mysql, &mut whatever_conn, &create);
    } else {
        whatever_conn
            .query_drop(&create)
            .expect("create sort fixture");
    }

    let insert = format!(
        "INSERT INTO {table} VALUES
        (1, -1, 2, 30, -4, 100, 1000, 10.250, -2.5, 8.5, TRUE, 1, 2020,
         '2024-01-02', '02:03:04.000001', '2024-01-02 02:03:04.000001', '2024-01-02 02:03:04.000001',
         'a', '10', 'Alpha', 'a01', 'a01', 'a01', '10', 'HIGH', 'red,blue'),
        (2, 1, -2, -30, 4, -100, 2, 2.500, 10.5, -8.5, FALSE, 2, 1999,
         '2023-12-31', '-02:03:04.000001', '2023-12-31 23:59:59.999999', '2023-12-31 23:59:59.999999',
         'B', '2', 'beta', 'b00', 'b00', 'b00', '\"text\"', 'low', 'Green'),
        (3, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL,
         NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL),
        (4, 1, 1, 1, 1, 1, 1, 10.250, 2.5, 8.5, TRUE, 1, 2020,
         '2024-01-02', '10:00:00', '2024-01-02 10:00:00', '2024-01-02 10:00:00',
         'A', '02', 'ALPHA', 'a02', 'a02', 'a02', 'true', 'Medium', 'red'),
        (5, -128, -32768, -8388608, -2147483648, -9223372036854775808, 9007199254740992, 1.000001, 16777216, -0.0, FALSE, 0, 2000,
         '1000-01-01', '838:59:59', '1000-01-01 00:00:00.000001', '1970-01-01 00:00:01.000001',
         'é', 'é', 'é', 'a', 'a ', 'a', 'null', 'low', ''),
        (6, 127, 32767, 8388607, 2147483647, 9223372036854775807, 9007199254740993, 1.000002, 16777217, 0.0, TRUE, 255, 2001,
         '9999-12-31', '-838:59:59', '9999-12-31 23:59:59.999999', '2038-01-19 03:14:07.999999',
         'E', 'E', 'E', 'b', 'b', 'b', '1', 'HIGH', 'red,blue')"
    );
    if let Some(mysql) = mysql_conn.as_mut() {
        assert_exec_succeeds(mysql, &mut whatever_conn, &insert);
    } else {
        whatever_conn
            .query_drop(&insert)
            .expect("insert sort fixture");
    }

    let columns = [
        "tiny_value",
        "small_value",
        "medium_value",
        "int_value",
        "big_value",
        "unsigned_value",
        "decimal_value",
        "float_value",
        "double_value",
        "bool_value",
        "bit_value",
        "year_value",
        "date_value",
        "time_value",
        "datetime_value",
        "timestamp_value",
        "char_value",
        "varchar_value",
        "text_value",
        "binary_value",
        "varbinary_value",
        "blob_value",
        "json_value",
        "enum_value",
        "set_value",
    ];
    for column in columns {
        for direction in ["ASC", "DESC"] {
            let sql = format!("SELECT id FROM {table} ORDER BY {column} {direction}, id ASC");
            if let Some(mysql) = mysql_conn.as_mut() {
                assert_query_parity(mysql, &mut whatever_conn, &sql);
            } else {
                fetch_rows(&mut whatever_conn, &sql).expect("sort query");
            }
        }
    }

    for order in [
        "decimal_value ASC, text_value DESC, time_value ASC, id ASC",
        "text_value ASC, decimal_value DESC, date_value DESC, id ASC",
        "enum_value ASC, set_value DESC, unsigned_value ASC, id ASC",
        "json_value ASC, varchar_value DESC, bool_value ASC, id ASC",
        "time_value DESC, datetime_value ASC, decimal_value DESC, id ASC",
        "binary_value ASC, varbinary_value DESC, blob_value ASC, id ASC",
    ] {
        let sql = format!("SELECT id FROM {table} ORDER BY {order}");
        if let Some(mysql) = mysql_conn.as_mut() {
            assert_query_parity(mysql, &mut whatever_conn, &sql);
        } else {
            fetch_rows(&mut whatever_conn, &sql).expect("compound sort query");
        }
    }

    if let Some(mysql) = mysql_conn.as_mut() {
        let _ = mysql.query_drop(format!("DROP TABLE IF EXISTS {table}"));
    }
    let _ = whatever_conn.query_drop(format!("DROP TABLE IF EXISTS {table}"));
}

#[test]
fn parity_with_mysql_for_supported_semantics() {
    let _guard = common::test_lock();
    let _mismatches = ParityMismatchCollector::new();
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
    let children_fk = format!("fk_children_parent_{pid}");
    let features = format!("wdb_parity_features_{pid}");
    let scratch = format!("wdb_parity_scratch_{pid}");
    let posts_archive = format!("wdb_parity_posts_archive_{pid}");
    let renamed_posts = format!("wdb_parity_posts_renamed_{pid}");

    for sql in [
        format!("DROP TABLE IF EXISTS {children}"),
        format!("DROP TABLE IF EXISTS {parents}"),
        format!("DROP TABLE IF EXISTS {renamed_posts}"),
        format!("DROP TABLE IF EXISTS {posts_archive}"),
        format!("DROP TABLE IF EXISTS {posts}"),
        format!("DROP TABLE IF EXISTS {users}"),
        format!("DROP TABLE IF EXISTS {features}"),
        format!("DROP DATABASE IF EXISTS {scratch}"),
    ] {
        let _ = mysql_conn.query_drop(&sql);
        let _ = whatever_conn.query_drop(&sql);
    }

    // Database compatibility commands should succeed on both backends.
    assert_exec_succeeds(
        &mut mysql_conn,
        &mut whatever_conn,
        &format!("CREATE DATABASE {scratch}"),
    );
    let mysql_databases = fetch_rows(&mut mysql_conn, "SHOW DATABASES").expect("mysql databases");
    let mysqweel_databases =
        fetch_rows(&mut whatever_conn, "SHOW DATABASES").expect("MySqweel databases");
    assert!(mysql_databases.iter().any(|row| {
        row.first()
            .is_some_and(|database| database.eq_ignore_ascii_case("information_schema"))
    }));
    assert!(mysqweel_databases.iter().any(|row| {
        row.first()
            .is_some_and(|database| database.eq_ignore_ascii_case("information_schema"))
    }));

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
            "CREATE TABLE {posts} (id BIGINT PRIMARY KEY AUTO_INCREMENT, user_id BIGINT, title TEXT, author_name TEXT, repair_note TEXT)"
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
            "CREATE TABLE {features} (id BIGINT PRIMARY KEY, name VARCHAR(64), doubled BIGINT GENERATED ALWAYS AS (id * 2) STORED)"
        ),
    );
    assert_exec_parity(
        &mut mysql_conn,
        &mut whatever_conn,
        &format!("CREATE INDEX idx_{features}_name_prefix ON {features} (name(8))"),
    );
    assert_exec_parity(
        &mut mysql_conn,
        &mut whatever_conn,
        &format!("ALTER TABLE {features} ADD COLUMN note VARCHAR(16) AFTER name"),
    );
    assert_exec_parity(
        &mut mysql_conn,
        &mut whatever_conn,
        &format!("ALTER TABLE {features} CHANGE COLUMN note description VARCHAR(32)"),
    );
    assert_exec_parity(
        &mut mysql_conn,
        &mut whatever_conn,
        &format!(
            "INSERT INTO {features} (id, name, description) VALUES (3, 'compatibility', 'works')"
        ),
    );
    assert_query_parity(
        &mut mysql_conn,
        &mut whatever_conn,
        &format!("SELECT id, name, description, doubled FROM {features}"),
    );
    assert_show_index_parity(
        &mut mysql_conn,
        &mut whatever_conn,
        &format!("SHOW INDEX FROM {features}"),
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
        &format!(
            "INSERT INTO {posts} (user_id, title) VALUES \
             (1, 'p1'), (1, 'p2'), (3, 'p3'), (999, 'orphan'), (NULL, 'draft')"
        ),
    );

    assert_exec_parity(
        &mut mysql_conn,
        &mut whatever_conn,
        &format!("ALTER TABLE {users} ADD COLUMN display_name TEXT"),
    );
    assert_exec_parity(
        &mut mysql_conn,
        &mut whatever_conn,
        &format!("UPDATE {users} SET display_name = CONCAT(name, '!') WHERE id <= 3"),
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
    assert_exec_parity(
        &mut mysql_conn,
        &mut whatever_conn,
        &format!(
            "UPDATE {posts} AS p JOIN {users} AS u ON u.id = p.user_id SET p.author_name = u.name WHERE u.email = 'a@example.com'"
        ),
    );
    assert_exec_parity(
        &mut mysql_conn,
        &mut whatever_conn,
        &format!(
            "UPDATE {posts} AS p LEFT JOIN {users} AS u ON u.id = p.user_id SET p.repair_note = 'missing' WHERE u.id IS NULL"
        ),
    );
    assert_query_parity(
        &mut mysql_conn,
        &mut whatever_conn,
        &format!("SELECT id, user_id, title, author_name, repair_note FROM {posts} ORDER BY id"),
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
            "SELECT id, LEFT(name, 2) AS left_name, RIGHT(email, 11) AS email_domain, LPAD(score, 3, '0') AS padded_score, RPAD(name, 6, '.') AS padded_name, LOCATE('@', email) AS at_pos, INSTR(email, '.') AS dot_pos, POSITION('@' IN email) AS position_pos, REVERSE(name) AS reversed_name, REPEAT(SUBSTRING(name, 1, 1), 2) AS repeated_initial, ASCII(name) AS first_char FROM {users} ORDER BY id"
        ),
    );
    assert_query_parity(
        &mut mysql_conn,
        &mut whatever_conn,
        &format!(
            "SELECT id, GREATEST(score, 15) AS greatest_score, LEAST(score, 15) AS least_score, SIGN(score - 20) AS score_sign, MOD(score, 7) AS score_mod FROM {users} ORDER BY id"
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
            "SELECT id, DATE_FORMAT(created_at, '%Y-%m-%d') AS formatted_day, TIMESTAMPDIFF(DAY, '2026-01-01', created_at) AS days_since, DATEDIFF(created_at, '2026-01-01') AS datediff_days FROM {users} ORDER BY id"
        ),
    );
    assert_query_parity(
        &mut mysql_conn,
        &mut whatever_conn,
        "SELECT JSON_UNQUOTE(JSON_EXTRACT('{\"user\":{\"name\":\"Ada\"}}', '$.user.name')) AS json_name, JSON_CONTAINS('{\"a\":1,\"b\":2}', '{\"a\":1}') AS json_contains",
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
        &format!(
            "SELECT GROUP_CONCAT(name ORDER BY score DESC SEPARATOR '|') AS ordered_names, COUNT(DISTINCT name, score) AS distinct_name_scores FROM {users}"
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
    assert_query_parity(
        &mut mysql_conn,
        &mut whatever_conn,
        &format!(
            "SELECT email FROM {users} WHERE score BETWEEN 10 AND 30 AND id NOT IN (2) ORDER BY score DESC"
        ),
    );
    assert_query_parity(
        &mut mysql_conn,
        &mut whatever_conn,
        &format!(
            "SELECT id, CASE WHEN nickname IS NULL THEN 'missing' ELSE nickname END AS nick_state FROM {users} ORDER BY id"
        ),
    );
    assert_query_parity(
        &mut mysql_conn,
        &mut whatever_conn,
        &format!("SELECT email, score + 5 AS bumped FROM {users} ORDER BY bumped DESC LIMIT 2"),
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
            "SELECT u.id, d.title FROM {users} AS u JOIN (SELECT user_id, title FROM {posts} WHERE title LIKE 'p%') AS d ON d.user_id = u.id ORDER BY u.id, d.title"
        ),
    );
    assert_query_parity(
        &mut mysql_conn,
        &mut whatever_conn,
        &format!(
            "SELECT p.id, u.email FROM {users} AS u RIGHT JOIN {posts} AS p ON p.user_id = u.id ORDER BY p.id"
        ),
    );
    assert_query_parity(
        &mut mysql_conn,
        &mut whatever_conn,
        &format!(
            "WITH selected (user_id, user_email) AS (SELECT id, email FROM {users} WHERE score >= 20) SELECT user_id, user_email FROM selected ORDER BY user_id"
        ),
    );
    assert_query_parity(
        &mut mysql_conn,
        &mut whatever_conn,
        &format!(
            "SELECT id, ROW_NUMBER() OVER (ORDER BY score, id) AS row_num, CUME_DIST() OVER (ORDER BY nickname IS NULL) AS cumulative_distribution, SUM(score) OVER (ORDER BY nickname IS NULL) AS peer_total FROM {users} ORDER BY id"
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
        &format!("CREATE TABLE {posts_archive} (user_id BIGINT, title TEXT, title_len BIGINT)"),
    );
    assert_exec_parity(
        &mut mysql_conn,
        &mut whatever_conn,
        &format!(
            "INSERT INTO {posts_archive} (user_id, title, title_len) SELECT user_id, title, LENGTH(title) FROM {posts} WHERE user_id IN (1, 3)"
        ),
    );
    assert_query_parity(
        &mut mysql_conn,
        &mut whatever_conn,
        &format!("SELECT user_id, title, title_len FROM {posts_archive} ORDER BY user_id, title"),
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
    let mysql_tables = fetch_rows(&mut mysql_conn, "SHOW TABLES").expect("mysql show tables");
    let whatever_tables =
        fetch_rows(&mut whatever_conn, "SHOW TABLES").expect("MySqweel show tables");
    for table in [&users, &posts, &posts_archive] {
        assert!(
            mysql_tables
                .iter()
                .any(|row| row.first().is_some_and(|value| value == table)),
            "MySQL SHOW TABLES omitted {table}"
        );
        assert!(
            whatever_tables
                .iter()
                .any(|row| row.first().is_some_and(|value| value == table)),
            "MySqweel SHOW TABLES omitted {table}"
        );
    }
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
    assert_eq!(
        fetch_rows(
            &mut mysql_conn,
            &format!(
                "SELECT schema_name FROM information_schema.schemata WHERE schema_name = '{MYSQL_DOCKER_DATABASE}'"
            ),
        )
        .expect("mysql schema metadata"),
        vec![vec![MYSQL_DOCKER_DATABASE.to_string()]]
    );
    assert_eq!(
        fetch_rows(
            &mut whatever_conn,
            "SELECT schema_name FROM information_schema.schemata WHERE schema_name = 'app'",
        )
        .expect("MySqweel schema metadata"),
        vec![vec!["app".to_string()]]
    );
    assert_show_index_parity(
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
            "CREATE TABLE {children} (id BIGINT PRIMARY KEY AUTO_INCREMENT, parent_id BIGINT, CONSTRAINT {children_fk} FOREIGN KEY (parent_id) REFERENCES {parents} (id) ON DELETE CASCADE ON UPDATE RESTRICT)"
        ),
    );
    assert_exec_parity(
        &mut mysql_conn,
        &mut whatever_conn,
        &format!("INSERT INTO {parents} (id) VALUES (1)"),
    );
    assert_exec_parity(
        &mut mysql_conn,
        &mut whatever_conn,
        &format!("INSERT INTO {children} (id, parent_id) VALUES (1, 1)"),
    );
    assert_exec_parity(
        &mut mysql_conn,
        &mut whatever_conn,
        &format!("DELETE FROM {parents} WHERE id = 1"),
    );
    assert_query_parity(
        &mut mysql_conn,
        &mut whatever_conn,
        &format!("SELECT COUNT(*) AS child_count FROM {children}"),
    );
    assert_query_parity_unordered(
        &mut mysql_conn,
        &mut whatever_conn,
        &format!(
            "SELECT table_name, constraint_name, constraint_type FROM information_schema.table_constraints WHERE table_name = '{children}' AND constraint_name = '{children_fk}'"
        ),
    );
    assert_query_parity_unordered(
        &mut mysql_conn,
        &mut whatever_conn,
        &format!(
            "SELECT constraint_name, column_name, referenced_table_name, referenced_column_name FROM information_schema.key_column_usage WHERE constraint_name = '{children_fk}' ORDER BY column_name"
        ),
    );
    assert_query_parity_unordered(
        &mut mysql_conn,
        &mut whatever_conn,
        &format!(
            "SELECT constraint_name, delete_rule, update_rule FROM information_schema.referential_constraints WHERE constraint_name = '{children_fk}'"
        ),
    );

    assert_exec_parity(
        &mut mysql_conn,
        &mut whatever_conn,
        &format!("RENAME TABLE {posts} TO {renamed_posts}"),
    );
    assert_exec_parity(
        &mut mysql_conn,
        &mut whatever_conn,
        &format!("TRUNCATE TABLE {renamed_posts}"),
    );
    assert_query_parity(
        &mut mysql_conn,
        &mut whatever_conn,
        &format!("SELECT COUNT(*) AS n FROM {renamed_posts}"),
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
        &format!("DROP TABLE IF EXISTS {renamed_posts}"),
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
        &format!("DROP TABLE IF EXISTS {features}"),
    );
    assert_exec_succeeds(
        &mut mysql_conn,
        &mut whatever_conn,
        &format!("DROP DATABASE IF EXISTS {scratch}"),
    );
}

/// Verify per-column row-by-row parity against real MySQL for every
/// `information_schema` view my-sqweel claims to support. Catches the kind of
/// regression that recently caused `KEY_COLUMN_USAGE` to be unreliable: missing
/// PK rows after ALTER, missing UNIQUE rows, dropped or renamed columns.
///
/// Optional locally if no MySQL is reachable; mandatory when
/// `MYSQL_PARITY_REQUIRED=1` (as it is in CI).
#[test]
fn parity_with_mysql_for_information_schema() {
    let _guard = common::test_lock();
    let _mismatches = ParityMismatchCollector::new();
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
    let parents = format!("wdb_info_parents_{pid}");
    let children = format!("wdb_info_children_{pid}");
    let children_fk = format!("fk_children_parents_{pid}");
    let composite = format!("wdb_info_composite_{pid}");
    let late_pk = format!("wdb_info_late_pk_{pid}");

    for sql in [
        format!("DROP TABLE IF EXISTS {children}"),
        format!("DROP TABLE IF EXISTS {composite}"),
        format!("DROP TABLE IF EXISTS {parents}"),
        format!("DROP TABLE IF EXISTS {late_pk}"),
    ] {
        let _ = mysql_conn.query_drop(&sql);
        let _ = whatever_conn.query_drop(&sql);
    }

    // Fixture: parent table with PK + UNIQUE, child table with composite PK
    // and multi-column FK, a separate composite-PK table, and a table whose PK
    // is added via ALTER (regression coverage for the recent KCU bug).
    assert_exec_parity(
        &mut mysql_conn,
        &mut whatever_conn,
        &format!(
            "CREATE TABLE {parents} (id BIGINT PRIMARY KEY AUTO_INCREMENT, code VARCHAR(32) NOT NULL UNIQUE)"
        ),
    );
    assert_exec_parity(
        &mut mysql_conn,
        &mut whatever_conn,
        &format!(
            "CREATE TABLE {children} (\
                parent_id BIGINT NOT NULL,\
                slot BIGINT NOT NULL,\
                email VARCHAR(255) NOT NULL,\
                note VARCHAR(255),\
                PRIMARY KEY (parent_id, slot),\
                CONSTRAINT {children_fk} FOREIGN KEY (parent_id) REFERENCES {parents} (id) ON DELETE CASCADE ON UPDATE RESTRICT\
            )"
        ),
    );
    assert_exec_parity(
        &mut mysql_conn,
        &mut whatever_conn,
        &format!(
            "CREATE TABLE {composite} (a BIGINT NOT NULL, b BIGINT NOT NULL, c TEXT, PRIMARY KEY (a, b))"
        ),
    );
    assert_exec_parity(
        &mut mysql_conn,
        &mut whatever_conn,
        &format!("CREATE TABLE {late_pk} (email VARCHAR(255))"),
    );
    assert_exec_parity(
        &mut mysql_conn,
        &mut whatever_conn,
        &format!("ALTER TABLE {late_pk} ADD COLUMN id BIGINT PRIMARY KEY AUTO_INCREMENT"),
    );
    assert_exec_parity(
        &mut mysql_conn,
        &mut whatever_conn,
        &format!("CREATE INDEX idx_{children}_note ON {children} (note)"),
    );

    // information_schema.tables — table_name parity. table_schema differs
    // (my-sqweel reports 'app', MySQL reports the connection DB), so we don't
    // project it here; the engine unit tests pin that value.
    assert_query_parity_unordered(
        &mut mysql_conn,
        &mut whatever_conn,
        &format!(
            "SELECT table_name FROM information_schema.tables \
             WHERE table_name IN ('{parents}', '{children}', '{composite}', '{late_pk}') \
             ORDER BY table_name"
        ),
    );

    // information_schema.columns — full populated surface, ordered by
    // ordinal_position so multi-column PK ordering is verified too.
    for table in [&parents, &children, &composite, &late_pk] {
        assert_query_parity_unordered(
            &mut mysql_conn,
            &mut whatever_conn,
            &format!(
                "SELECT table_name, column_name, ordinal_position, is_nullable, column_default, \
                    column_type, data_type, column_key, extra \
                 FROM information_schema.columns WHERE table_name = '{table}' \
                 ORDER BY ordinal_position"
            ),
        );
    }

    // information_schema.statistics — every index column. Use composite PK to
    // exercise seq_in_index ordering and a secondary non-unique index.
    for table in [&children, &composite] {
        assert_query_parity_unordered(
            &mut mysql_conn,
            &mut whatever_conn,
            &format!(
                "SELECT table_name, index_name, column_name, seq_in_index, non_unique \
                 FROM information_schema.statistics WHERE table_name = '{table}' \
                 ORDER BY index_name, seq_in_index"
            ),
        );
    }

    // information_schema.table_constraints — PK + FK rows. UNIQUE rows are
    // omitted from parity because MySQL and my-sqweel auto-name UNIQUE
    // constraints differently; the engine unit tests cover that they exist.
    assert_query_parity_unordered(
        &mut mysql_conn,
        &mut whatever_conn,
        &format!(
            "SELECT table_name, constraint_name, constraint_type \
             FROM information_schema.table_constraints \
             WHERE table_name IN ('{parents}', '{children}', '{composite}', '{late_pk}') \
                AND constraint_type IN ('PRIMARY KEY', 'FOREIGN KEY') \
             ORDER BY table_name, constraint_type, constraint_name"
        ),
    );

    // information_schema.key_column_usage — PK rows (including composite PK
    // and PK added via ALTER) plus FK rows with full referenced_* and
    // position_in_unique_constraint. UNIQUE constraint rows excluded for the
    // same auto-naming reason as above.
    assert_query_parity_unordered(
        &mut mysql_conn,
        &mut whatever_conn,
        &format!(
            "SELECT table_name, constraint_name, column_name, ordinal_position, \
                position_in_unique_constraint, referenced_table_name, referenced_column_name \
             FROM information_schema.key_column_usage \
             WHERE table_name IN ('{parents}', '{children}', '{composite}', '{late_pk}') \
                AND (constraint_name = 'PRIMARY' OR constraint_name = '{children_fk}') \
             ORDER BY table_name, constraint_name, ordinal_position"
        ),
    );

    // Regression pin for the recent KEY_COLUMN_USAGE issue: PRIMARY added via
    // ALTER TABLE must show up.
    assert_query_parity_unordered(
        &mut mysql_conn,
        &mut whatever_conn,
        &format!(
            "SELECT table_name, constraint_name, column_name, ordinal_position \
             FROM information_schema.key_column_usage \
             WHERE table_name = '{late_pk}' AND constraint_name = 'PRIMARY'"
        ),
    );

    // information_schema.referential_constraints — full FK metadata.
    assert_query_parity_unordered(
        &mut mysql_conn,
        &mut whatever_conn,
        &format!(
            "SELECT constraint_name, unique_constraint_name, match_option, update_rule, \
                delete_rule, table_name, referenced_table_name \
             FROM information_schema.referential_constraints \
             WHERE constraint_name = '{children_fk}'"
        ),
    );

    // information_schema.schemata — projecting columns shared between
    // backends. catalog_name = 'def' and utf8mb4 defaults are stable. The
    // schema name itself differs (app vs the connection DB), so we filter
    // by whatever names exist on each side.
    assert_query_parity_unordered(
        &mut mysql_conn,
        &mut whatever_conn,
        "SELECT catalog_name, default_character_set_name FROM information_schema.schemata \
         WHERE default_character_set_name = 'utf8mb4' AND catalog_name = 'def' LIMIT 1",
    );

    // information_schema.character_sets — utf8mb4 row must exist with the
    // same maxlen on both backends. default_collate_name is excluded because
    // MySQL 8.0 changed it to utf8mb4_0900_ai_ci; both still report maxlen 4.
    assert_query_parity_unordered(
        &mut mysql_conn,
        &mut whatever_conn,
        "SELECT character_set_name, maxlen \
         FROM information_schema.character_sets WHERE character_set_name = 'utf8mb4'",
    );

    // information_schema.collations — pin character_set_name and is_compiled
    // for utf8mb4_general_ci. is_default is excluded because MySQL 8.0 made
    // utf8mb4_0900_ai_ci the default collation for utf8mb4.
    assert_query_parity_unordered(
        &mut mysql_conn,
        &mut whatever_conn,
        "SELECT collation_name, character_set_name, is_compiled \
         FROM information_schema.collations WHERE collation_name = 'utf8mb4_general_ci'",
    );

    // information_schema.views / routines — neither test fixture defines any,
    // so both should return zero rows for the test-specific filter.
    assert_query_parity_unordered(
        &mut mysql_conn,
        &mut whatever_conn,
        &format!(
            "SELECT table_name FROM information_schema.views \
             WHERE table_name IN ('{parents}', '{children}', '{composite}', '{late_pk}')"
        ),
    );
    assert_query_parity_unordered(
        &mut mysql_conn,
        &mut whatever_conn,
        &format!(
            "SELECT routine_name FROM information_schema.routines \
             WHERE routine_name IN ('{parents}', '{children}', '{composite}', '{late_pk}')"
        ),
    );

    // Teardown.
    for sql in [
        format!("DROP TABLE IF EXISTS {children}"),
        format!("DROP TABLE IF EXISTS {composite}"),
        format!("DROP TABLE IF EXISTS {parents}"),
        format!("DROP TABLE IF EXISTS {late_pk}"),
    ] {
        let _ = mysql_conn.query_drop(&sql);
        let _ = whatever_conn.query_drop(&sql);
    }
}

#[test]
fn recursive_ctes_return_mysql_rows() {
    let whatever_url = start_whatever_server();
    let whatever_pool =
        Pool::new(Opts::from_url(&whatever_url).expect("valid MySqweel URL")).expect("connect");
    let mut conn = whatever_pool.get_conn().expect("whatever conn");

    let rows: Vec<u32> = conn
        .query(
            "WITH RECURSIVE sequence AS (SELECT 1 AS n UNION ALL SELECT n + 1 FROM sequence WHERE n < 3) SELECT n FROM sequence",
        )
        .expect("recursive CTE should execute");
    assert_eq!(rows, vec![1, 2, 3]);
}

#[test]
fn parity_with_mysql_for_json_expressions() {
    let _guard = common::test_lock();
    let _mismatches = ParityMismatchCollector::new();
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
    let payloads = format!("wdb_json_payload_{pid}");

    let _ = mysql_conn.query_drop(&format!("DROP TABLE IF EXISTS {payloads}"));
    let _ = whatever_conn.query_drop(&format!("DROP TABLE IF EXISTS {payloads}"));

    let ada_payload = r#"{"name":"Ada","tier":"pro","flags":[1,2]}"#;
    let bob_payload = r#"{"name":"Bob","tier":"basic"}"#;
    let has_pro_payload = r#"{"tier":"pro"}"#;

    assert_exec_parity(
        &mut mysql_conn,
        &mut whatever_conn,
        &format!(
            "CREATE TABLE {payloads} \
             (id BIGINT PRIMARY KEY AUTO_INCREMENT, username TEXT, score BIGINT, payload TEXT)"
        ),
    );
    assert_exec_parity(
        &mut mysql_conn,
        &mut whatever_conn,
        &format!(
            "INSERT INTO {payloads} (username, score, payload) VALUES \
            ('Ada', 10, '{ada_payload}'), \
            ('Bob', 20, '{bob_payload}'), \
            ('Eve', 30, NULL)"
        ),
    );
    assert_query_parity(
        &mut mysql_conn,
        &mut whatever_conn,
        "SELECT \
            JSON_TYPE('{\"a\":[1,2]}') AS json_type, \
            JSON_DEPTH('{\"a\":[1,2]}') AS json_depth, \
            JSON_LENGTH('{\"a\":[1,2]}', '$.a') AS json_length, \
            JSON_KEYS('{\"a\":1,\"b\":2}') AS json_keys, \
            JSON_CONTAINS_PATH('{\"a\":1}', 'one', '$.a', '$.missing') AS contains_one, \
            JSON_CONTAINS_PATH('{\"a\":1}', 'all', '$.a', '$.missing') AS contains_all, \
            JSON_OVERLAPS('[1,2]', '[2,3]') AS overlaps, \
            JSON_VALID('{\"a\":1}') AS valid_json, \
            JSON_VALID('{bad json}') AS invalid_json, \
            JSON_QUOTE('Ada') AS quoted",
    );
    assert_query_parity(
        &mut mysql_conn,
        &mut whatever_conn,
        "SELECT \
            JSON_UNQUOTE(JSON_EXTRACT(JSON_ARRAY_APPEND('[1,2]', '$', 3), '$[2]')) AS appended, \
            JSON_EXTRACT(JSON_ARRAY_INSERT('[1,3]', '$[1]', 2), '$') AS inserted, \
            JSON_EXTRACT(JSON_INSERT('{\"a\":1}', '$.a', 2, '$.b', 3), '$') AS inserted_object, \
            JSON_EXTRACT(JSON_REPLACE('{\"a\":1}', '$.a', 2, '$.b', 3), '$') AS replaced_object, \
            JSON_EXTRACT(JSON_MERGE_PATCH('{\"a\":1,\"b\":2}', '{\"a\":9,\"b\":null}'), '$') AS merged_patch, \
            JSON_EXTRACT(JSON_MERGE_PRESERVE('{\"a\":1}', '{\"a\":2}'), '$') AS merged_preserve, \
            JSON_SEARCH('{\"name\":\"Ada\",\"other\":\"Bob\"}', 'one', 'A%') AS found_path",
    );
    assert_query_parity(
        &mut mysql_conn,
        &mut whatever_conn,
        &format!(
            "SELECT \
                JSON_ARRAYAGG(score) AS scores, \
                JSON_OBJECTAGG(username, score) AS score_map \
             FROM {payloads}"
        ),
    );

    assert_query_parity(
        &mut mysql_conn,
        &mut whatever_conn,
        &format!(
            "SELECT \
                id, \
                JSON_EXTRACT(payload, '$.name') AS name_json, \
                JSON_UNQUOTE(JSON_EXTRACT(payload, '$.name')) AS name_plain, \
                JSON_CONTAINS(payload, '{has_pro_payload}', '$') AS has_pro_tier \
             FROM {payloads} \
             ORDER BY id"
        ),
    );
    assert_query_parity(
        &mut mysql_conn,
        &mut whatever_conn,
        &format!(
            "SELECT \
                id, \
                JSON_UNQUOTE(JSON_EXTRACT(JSON_OBJECT('name', username, 'score', score, 'tier', JSON_UNQUOTE(JSON_EXTRACT(payload, '$.tier'))), '$.name')) AS metadata_name, \
                JSON_EXTRACT(JSON_OBJECT('name', username, 'score', score, 'tier', JSON_UNQUOTE(JSON_EXTRACT(payload, '$.tier'))), '$.score') AS metadata_score, \
                JSON_UNQUOTE(JSON_EXTRACT(JSON_OBJECT('name', username, 'score', score, 'tier', JSON_UNQUOTE(JSON_EXTRACT(payload, '$.tier'))), '$.tier')) AS metadata_tier, \
                JSON_UNQUOTE(JSON_EXTRACT(JSON_ARRAY(username, score, JSON_UNQUOTE(JSON_EXTRACT(payload, '$.name'))), '$[0]')) AS array_username, \
                JSON_EXTRACT(JSON_ARRAY(username, score, JSON_UNQUOTE(JSON_EXTRACT(payload, '$.name'))), '$[1]') AS array_score, \
                JSON_UNQUOTE(JSON_EXTRACT(JSON_ARRAY(username, score, JSON_UNQUOTE(JSON_EXTRACT(payload, '$.name'))), '$[2]')) AS array_name \
             FROM {payloads} \
             ORDER BY id"
        ),
    );
    assert_query_parity(
        &mut mysql_conn,
        &mut whatever_conn,
        &format!(
            "SELECT \
                id, \
                JSON_UNQUOTE(JSON_EXTRACT(JSON_SET(payload, '$.tier', 'enterprise'), '$.name')) AS promoted_name, \
                JSON_UNQUOTE(JSON_EXTRACT(JSON_SET(payload, '$.tier', 'enterprise'), '$.tier')) AS promoted_tier, \
                JSON_UNQUOTE(JSON_EXTRACT(JSON_REMOVE(payload, '$.flags'), '$.name')) AS no_flags_name, \
                JSON_EXTRACT(JSON_REMOVE(payload, '$.flags'), '$.tier') AS no_flags_tier, \
                JSON_EXTRACT(JSON_REMOVE(payload, '$.flags'), '$.flags') AS no_flags_flags \
             FROM {payloads} \
             ORDER BY id"
        ),
    );
    assert_query_parity(
        &mut mysql_conn,
        &mut whatever_conn,
        &format!(
            "SELECT \
                TIMESTAMPDIFF(DAY, '2026-01-01 00:00:00', '2026-01-07 12:00:00') AS day_delta, \
                DATE_ADD('2026-01-02 00:00:00', INTERVAL 3 HOUR) AS shifted \
             "
        ),
    );

    assert_prepared_query_parity(
        &mut mysql_conn,
        &mut whatever_conn,
        "SELECT \
            JSON_UNQUOTE(JSON_EXTRACT(JSON_OBJECT('name', ?, 'score', ?), '$.name')) AS object_name, \
            JSON_EXTRACT(JSON_OBJECT('name', ?, 'score', ?), '$.score') AS object_score",
        ("Zoe", 99, "Zoe", 99),
    );
    assert_prepared_query_parity(
        &mut mysql_conn,
        &mut whatever_conn,
        "SELECT \
            JSON_UNQUOTE(JSON_EXTRACT(JSON_OBJECT('name', ?, 'score', ?), '$.name')) AS object_name, \
            JSON_EXTRACT(JSON_OBJECT('name', ?, 'score', ?), '$.score') AS object_score",
        ("Zoe", 99, "same-name", 123),
    );
    assert_prepared_query_parity(
        &mut mysql_conn,
        &mut whatever_conn,
        "SELECT JSON_CONTAINS(?, ?, '$') AS found",
        ("{\"a\":1,\"b\":2}", "{\"a\":1}"),
    );

    assert_exec_parity(
        &mut mysql_conn,
        &mut whatever_conn,
        &format!("DROP TABLE IF EXISTS {payloads}"),
    );
}

#[test]
fn parity_with_mysql_for_json_collection_paths() {
    let _guard = common::test_lock();
    let _mismatches = ParityMismatchCollector::new();
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
    let payloads = format!("wdb_json_collection_{pid}");

    let _ = mysql_conn.query_drop(&format!("DROP TABLE IF EXISTS {payloads}"));
    let _ = whatever_conn.query_drop(&format!("DROP TABLE IF EXISTS {payloads}"));

    let payload_alpha = r#"{"team":{"lead":{"name":"Ada","score":10},"scores":[10,20,30],"labels":["pro","basic"]},"meta":{"active":true}}"#;
    let payload_beta = r#"{"team":{"lead":{"name":"Bob","score":25},"scores":[40,50],"labels":["basic","trial"]},"meta":{"active":false}}"#;

    assert_exec_parity(
        &mut mysql_conn,
        &mut whatever_conn,
        &format!("CREATE TABLE {payloads} (id BIGINT PRIMARY KEY AUTO_INCREMENT, payload TEXT)"),
    );
    assert_exec_parity(
        &mut mysql_conn,
        &mut whatever_conn,
        &format!("INSERT INTO {payloads} (payload) VALUES ('{payload_alpha}'), ('{payload_beta}')"),
    );

    assert_query_parity(
        &mut mysql_conn,
        &mut whatever_conn,
        &format!(
            "SELECT \
                id, \
                JSON_UNQUOTE(JSON_EXTRACT(payload, '$.team.lead.name')) AS lead_name, \
                JSON_EXTRACT(payload, '$.team.scores[1]') AS score_mid, \
                JSON_UNQUOTE(JSON_EXTRACT(JSON_EXTRACT(payload, '$.team.labels[0]', '$.team.labels[1]'), '$[0]')) AS first_label, \
                JSON_UNQUOTE(JSON_EXTRACT(JSON_EXTRACT(payload, '$.team.labels[0]', '$.team.labels[1]'), '$[1]')) AS second_label \
             FROM {payloads} \
             ORDER BY id"
        ),
    );
    assert_query_parity(
        &mut mysql_conn,
        &mut whatever_conn,
        &format!(
            "SELECT \
                id, \
                JSON_UNQUOTE(JSON_EXTRACT(JSON_SET(payload, '$.team.scores[1]', 99, '$.team.labels[2]', 'enterprise'), '$.team.lead.name')) AS updated_lead, \
                JSON_EXTRACT(JSON_SET(payload, '$.team.scores[1]', 99, '$.team.labels[2]', 'enterprise'), '$.team.scores[1]') AS updated_score_mid, \
                JSON_UNQUOTE(JSON_EXTRACT(JSON_SET(payload, '$.team.scores[1]', 99, '$.team.labels[2]', 'enterprise'), '$.team.labels[2]')) AS appended_label, \
                JSON_EXTRACT(JSON_REMOVE(payload, '$.meta.active', '$.team.labels[0]'), '$.team.labels') AS remaining_labels \
             FROM {payloads} \
             ORDER BY id"
        ),
    );
    assert_query_parity(
        &mut mysql_conn,
        &mut whatever_conn,
        &format!(
            "SELECT \
                id, \
                JSON_EXTRACT(payload, '$.team.labels[5]') AS missing_value, \
                JSON_EXTRACT(payload, '$.missing.key') AS missing_path, \
                JSON_CONTAINS(payload, '[\"basic\"]', '$.team.labels') AS has_basic_label \
             FROM {payloads} \
             ORDER BY id"
        ),
    );

    assert_prepared_query_parity(
        &mut mysql_conn,
        &mut whatever_conn,
        "SELECT \
            JSON_EXTRACT(JSON_SET(?, '$.team.scores[0]', ?, '$.team.scores[1]', ?), '$.team.scores[0]') AS score_zero, \
            JSON_EXTRACT(JSON_SET(?, '$.team.scores[0]', ?, '$.team.scores[1]', ?), '$.team.scores[1]') AS score_one, \
            JSON_CONTAINS(?, '\"Alice\"', '$.team.labels') AS has_label",
        (
            r#"{"team":{"lead":{"name":"Zed","score":3},"scores":[1,2],"labels":["alpha","beta"]}}"#,
            99_i64,
            100_i64,
            r#"{"team":{"lead":{"name":"Zed","score":3},"scores":[1,2],"labels":["alpha","beta"]}}"#,
            99_i64,
            100_i64,
            r#"{"team":{"lead":{"name":"Zed","score":3},"scores":[1,2,3],"labels":["alpha","Alice"]}}"#,
        ),
    );

    assert_exec_parity(
        &mut mysql_conn,
        &mut whatever_conn,
        &format!("DROP TABLE IF EXISTS {payloads}"),
    );
}

#[test]
fn parity_with_mysql_for_conditional_expressions() {
    let _guard = common::test_lock();
    let _mismatches = ParityMismatchCollector::new();
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
    let conditional = format!("wdb_conditional_sql_{pid}");

    let _ = mysql_conn.query_drop(&format!("DROP TABLE IF EXISTS {conditional}"));
    let _ = whatever_conn.query_drop(&format!("DROP TABLE IF EXISTS {conditional}"));

    assert_exec_parity(
        &mut mysql_conn,
        &mut whatever_conn,
        &format!(
            "CREATE TABLE {conditional} (id BIGINT PRIMARY KEY AUTO_INCREMENT, username TEXT, score BIGINT, nickname TEXT)"
        ),
    );
    assert_exec_parity(
        &mut mysql_conn,
        &mut whatever_conn,
        &format!(
            "INSERT INTO {conditional} (username, score, nickname) VALUES \
             ('Alice', 10, NULL), \
             (NULL, NULL, NULL), \
             ('Carol', 25, 'carol'), \
             ('Dave', 30, '')"
        ),
    );

    assert_query_parity(
        &mut mysql_conn,
        &mut whatever_conn,
        &format!(
            "SELECT \
                id, \
                IF(username IS NULL, 'missing', username) AS username_or_default, \
                NULLIF(nickname, '') AS trimmed_optional, \
                COALESCE(NULLIF(username, 'Alice'), 'fallback') AS non_alice, \
                CASE WHEN score IS NULL THEN 0 WHEN score >= 20 THEN 1 ELSE -1 END AS score_bucket \
            FROM {conditional} \
            ORDER BY id"
        ),
    );
    assert_query_parity(
        &mut mysql_conn,
        &mut whatever_conn,
        &format!(
            "SELECT \
                id, \
                CASE WHEN score >= 20 THEN 'high' WHEN score IS NULL THEN 'unknown' ELSE 'low' END AS score_label, \
                NULLIF(score, 10) AS not_ten, \
                COALESCE(NULLIF(score, 30), 99) AS null_if_30_or_default \
            FROM {conditional} \
            ORDER BY id"
        ),
    );
    assert_query_parity(
        &mut mysql_conn,
        &mut whatever_conn,
        &format!(
            "SELECT username, IFNULL(nickname, '<missing>') AS nick_value FROM {conditional} ORDER BY id"
        ),
    );
    assert_query_parity(
        &mut mysql_conn,
        &mut whatever_conn,
        &format!(
            "SELECT COUNT(*) AS c, COALESCE(SUM(score), 0) AS score_total, MIN(score) AS min_score \
             FROM {conditional}"
        ),
    );

    assert_prepared_query_parity(
        &mut mysql_conn,
        &mut whatever_conn,
        "SELECT NULLIF(?, ?) AS passthrough",
        ("same", "same"),
    );
    assert_prepared_query_parity(
        &mut mysql_conn,
        &mut whatever_conn,
        "SELECT NULLIF(?, ?) AS passthrough",
        ("left", "right"),
    );
    assert_prepared_query_parity(
        &mut mysql_conn,
        &mut whatever_conn,
        "SELECT IF(? IS NULL, 'missing', 'present') AS marker, COALESCE(?, ?, ?) AS label",
        (Option::<&str>::None, Option::<&str>::None, "alpha", "omega"),
    );
    assert_prepared_query_parity(
        &mut mysql_conn,
        &mut whatever_conn,
        &format!(
            "SELECT id, CASE WHEN username = ? THEN 'match' ELSE 'mismatch' END AS flagged \
             FROM {conditional} \
             WHERE score >= ? \
             ORDER BY id"
        ),
        ("Alice", 20_i64),
    );
    assert_prepared_query_parity(
        &mut mysql_conn,
        &mut whatever_conn,
        &format!("SELECT id, COALESCE(nickname, ?, ?) AS source FROM {conditional} ORDER BY id"),
        ("none", ""),
    );
    assert_prepared_query_parity(
        &mut mysql_conn,
        &mut whatever_conn,
        &format!(
            "SELECT \
                id, \
                CASE WHEN score >= ? THEN 'high' ELSE 'low' END AS prepared_score_band, \
                SUM(CASE WHEN username IS NULL OR username = ? THEN 1 ELSE 0 END) \
                    OVER (ORDER BY id ROWS UNBOUNDED PRECEDING) AS running_flag_count \
             FROM {conditional} \
             ORDER BY id"
        ),
        (20_i64, "Carol"),
    );
    assert_prepared_query_parity(
        &mut mysql_conn,
        &mut whatever_conn,
        &format!(
            "SELECT \
                id, \
                CASE WHEN username = ? THEN 'match' ELSE 'mismatch' END AS match_state, \
                ROW_NUMBER() OVER (ORDER BY (score IS NULL), score, id) AS row_order, \
                RANK() OVER (ORDER BY score >= ?) AS score_rank \
             FROM {conditional} \
             WHERE (username IS NOT NULL) \
             ORDER BY row_order"
        ),
        ("Carol", 20_i64),
    );

    assert_exec_parity(
        &mut mysql_conn,
        &mut whatever_conn,
        &format!("DROP TABLE IF EXISTS {conditional}"),
    );
}
