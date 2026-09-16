//! Versioned, offline fixtures. External candidates use QUERY_COVERAGE_CASE and
//! QUERY_COVERAGE_REPORT; the controller treats absent reports as infrastructure failures.
mod common;

use std::collections::BTreeMap;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use my_sqweel::server::WireServer;
use my_sqweel::sql::engine::{Engine, EngineConfig};
use mysql::prelude::Queryable;
use mysql::{Conn, Opts, OptsBuilder, Value};
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Case {
    version: u32,
    id: String,
    provenance: serde_json::Value,
    features: Vec<String>,
    fixture_notes: String,
    determinism_notes: String,
    #[serde(default)]
    setup: Vec<String>,
    steps: Vec<Step>,
    #[serde(default)]
    checks: Vec<Step>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Step {
    sql: String,
    #[serde(default = "default_connection")]
    connection: String,
    #[serde(default)]
    ordered: bool,
    #[serde(default)]
    expect_error: bool,
}

fn default_connection() -> String {
    "main".into()
}

#[derive(Debug, Serialize, PartialEq, Eq)]
#[serde(tag = "kind", content = "value")]
enum Cell {
    Null,
    Bytes(Vec<u8>),
    Int(i64),
    UInt(u64),
    Float(u32),
    Double(u64),
    Date(u16, u8, u8, u8, u8, u8, u32),
    Time(bool, u32, u8, u8, u8, u32),
}

impl From<Value> for Cell {
    fn from(value: Value) -> Self {
        match value {
            Value::NULL => Self::Null,
            Value::Bytes(v) => Self::Bytes(v),
            Value::Int(v) => Self::Int(v),
            Value::UInt(v) => Self::UInt(v),
            Value::Float(v) => Self::Float(v.to_bits()),
            Value::Double(v) => Self::Double(v.to_bits()),
            Value::Date(y, m, d, h, min, s, us) => Self::Date(y, m, d, h, min, s, us),
            Value::Time(neg, d, h, m, s, us) => Self::Time(neg, d, h, m, s, us),
        }
    }
}

#[derive(Debug, Serialize, PartialEq, Eq)]
struct ResultSet {
    // Text-protocol cells are often bytes: retain column types to distinguish
    // numeric values from strings. Do not normalize NULL, decimals, or precision.
    columns: Vec<(String, String)>,
    rows: Vec<Vec<Cell>>,
    affected_rows: u64,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
#[serde(tag = "kind")]
enum Observation {
    Success { sets: Vec<ResultSet> },
    Error { code: u16, sqlstate: String },
}

fn database_error(error: mysql::Error) -> Result<Observation, String> {
    match error {
        mysql::Error::MySqlError(error) => Ok(Observation::Error {
            code: error.code,
            sqlstate: error.state,
        }),
        error => Err(format!("connection/protocol failure: {error}")),
    }
}

fn observe(conn: &mut Conn, step: &Step) -> Result<Observation, String> {
    let mut result = match conn.query_iter(&step.sql) {
        Ok(result) => result,
        Err(error) => return database_error(error),
    };
    let mut sets = Vec::new();
    while let Some(mut set) = result.iter() {
        let columns = set
            .columns()
            .as_ref()
            .iter()
            .map(|column| {
                (
                    column.name_str().into_owned(),
                    format!("{:?}", column.column_type()),
                )
            })
            .collect();
        let affected_rows = set.affected_rows();
        let mut rows = Vec::new();
        for row in &mut set {
            match row {
                Ok(row) => rows.push(row.unwrap().into_iter().map(Cell::from).collect::<Vec<_>>()),
                Err(error) => return database_error(error),
            }
        }
        if !step.ordered {
            rows.sort_by_cached_key(|row| serde_json::to_string(row).unwrap());
        }
        sets.push(ResultSet {
            columns,
            rows,
            affected_rows,
        });
    }
    Ok(Observation::Success { sets })
}

fn connect(url: &str) -> Result<Conn, String> {
    let seconds = std::env::var("QUERY_COVERAGE_STATEMENT_TIMEOUT")
        .unwrap_or_else(|_| "10".into())
        .parse::<u64>()
        .map_err(|e| e.to_string())?;
    if seconds == 0 {
        return Err("statement timeout must be positive".into());
    }
    let opts = OptsBuilder::from_opts(Opts::from_url(url).map_err(|e| e.to_string())?)
        .read_timeout(Some(Duration::from_secs(seconds)))
        .write_timeout(Some(Duration::from_secs(seconds)))
        .tcp_connect_timeout(Some(Duration::from_secs(seconds)));
    let mut conn = Conn::new(opts).map_err(|e| e.to_string())?;
    for sql in [
        "SET SESSION sql_mode = 'STRICT_TRANS_TABLES,NO_ENGINE_SUBSTITUTION'",
        "SET SESSION time_zone = '+00:00'",
        "SET NAMES utf8mb4 COLLATE utf8mb4_general_ci",
    ] {
        conn.query_drop(sql)
            .map_err(|e| format!("session setup {sql}: {e}"))?;
    }
    Ok(conn)
}

fn replay(url: &str, case: &Case) -> Result<Vec<Observation>, String> {
    let mut connections = BTreeMap::new();
    let mut main = connect(url)?;
    for sql in &case.setup {
        main.query_drop(sql)
            .map_err(|e| format!("fixture setup {sql}: {e}"))?;
    }
    connections.insert(default_connection(), main);
    let mut observations = Vec::new();
    for step in case.steps.iter().chain(&case.checks) {
        if !connections.contains_key(&step.connection) {
            connections.insert(step.connection.clone(), connect(url)?);
        }
        let conn = connections.get_mut(&step.connection).unwrap();
        observations.push(observe(conn, step)?);
    }
    Ok(observations)
}

fn start_mysqweel() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let engine = Arc::new(Engine::new(EngineConfig::mysql_strict()));
    engine.execute_sql("CREATE DATABASE test").unwrap();
    std::thread::spawn(move || WireServer::new(engine).serve_listener(listener).unwrap());
    format!("mysql://root@{address}/test")
}

fn load_case(path: &Path) -> Result<Case, String> {
    let case: Case = serde_json::from_slice(&std::fs::read(path).map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())?;
    if case.version != 1
        || case.id.is_empty()
        || case.steps.is_empty()
        || case.features.is_empty()
        || !case.provenance.is_object()
        || case.fixture_notes.is_empty()
        || case.determinism_notes.is_empty()
        || case
            .steps
            .iter()
            .chain(&case.checks)
            .any(|s| s.sql.trim().is_empty() || s.connection.is_empty())
    {
        return Err("invalid case version or missing case evidence".into());
    }
    Ok(case)
}

fn run_case(path: &Path, baseline: &str) -> serde_json::Value {
    let case = match load_case(path) {
        Ok(case) => case,
        Err(e) => return serde_json::json!({"status": "invalid", "error": e}),
    };
    // Each case owns a unique database, including every logical connection.
    let database = format!("coverage_{}", uuid::Uuid::new_v4().simple());
    let mut admin = match connect(baseline) {
        Ok(conn) => conn,
        Err(e) => return serde_json::json!({"status": "infrastructure", "error": e}),
    };
    let version: String = match admin.query_first("SELECT VERSION()") {
        Ok(Some(version)) => version,
        other => {
            return serde_json::json!({"status": "infrastructure", "error": format!("version probe: {other:?}")});
        }
    };
    if !version.starts_with("10.11.7-MariaDB") {
        return serde_json::json!({"status": "infrastructure", "error": format!("expected MariaDB 10.11.7, got {version}")});
    }
    if let Err(e) = admin.query_drop(format!(
        "CREATE DATABASE {database} CHARACTER SET utf8mb4 COLLATE utf8mb4_general_ci"
    )) {
        return serde_json::json!({"status": "infrastructure", "error": e.to_string()});
    }
    // mysql::Opts has no URL formatter; replace only the database URL path.
    let baseline_url = database_url(baseline, &database);
    let expected = replay(&baseline_url, &case);
    let result = match expected {
        Err(e) => serde_json::json!({"status": "invalid", "error": e}),
        Ok(expected) => {
            let valid =
                case.steps
                    .iter()
                    .chain(&case.checks)
                    .zip(&expected)
                    .all(|(step, observed)| {
                        step.expect_error == matches!(observed, Observation::Error { .. })
                    });
            if !valid {
                serde_json::json!({"status": "invalid", "error": "unexpected baseline error or success", "baseline": expected})
            } else if std::env::var("QUERY_COVERAGE_MODE").as_deref() == Ok("baseline") {
                serde_json::json!({"status": "pass", "baseline": expected})
            } else {
                match replay(&start_mysqweel(), &case) {
                    Ok(actual) => {
                        serde_json::json!({"status": if expected == actual {"pass"} else {"mismatch"}, "baseline": expected, "mysqweel": actual})
                    }
                    Err(e) => {
                        serde_json::json!({"status": "mismatch", "baseline": expected, "error": e})
                    }
                }
            }
        }
    };
    if let Err(e) = admin.query_drop(format!("DROP DATABASE {database}")) {
        return serde_json::json!({"status": "infrastructure", "error": format!("cleanup: {e}")});
    }
    serde_json::json!({"id": case.id, "reference_version": version, "result": result})
}

fn database_url(url: &str, database: &str) -> String {
    let (prefix, query) = url
        .split_once('?')
        .map_or((url, None), |(p, q)| (p, Some(q)));
    let authority_start = prefix.find("://").expect("validated URL") + 3;
    let path_start = prefix[authority_start..]
        .find('/')
        .map_or(prefix.len(), |i| i + authority_start);
    format!(
        "{}/{database}{}",
        &prefix[..path_start],
        query.map_or(String::new(), |q| format!("?{q}"))
    )
}

#[test]
fn query_coverage_cases() {
    let _guard = common::test_lock();
    let external = std::env::var_os("QUERY_COVERAGE_CASE");
    let paths: Vec<PathBuf> = if let Some(path) = external.as_ref() {
        vec![path.into()]
    } else {
        let mut paths: Vec<_> = std::fs::read_dir("tests/query_cases")
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| p.extension().is_some_and(|e| e == "json"))
            .collect();
        paths.sort();
        paths
    };
    assert!(!paths.is_empty(), "coverage corpus is empty");
    for path in &paths {
        load_case(path).unwrap();
    }
    let Some(target) = common::mysql_compare_target() else {
        assert!(
            external.is_none(),
            "external candidates require MariaDB comparison"
        );
        return;
    };
    let reports: Vec<_> = paths.iter().map(|p| run_case(p, target.url())).collect();
    if let Some(path) = std::env::var_os("QUERY_COVERAGE_REPORT") {
        std::fs::write(path, serde_json::to_vec_pretty(&reports).unwrap()).unwrap();
    }
    assert!(
        reports.iter().all(|r| r["result"]["status"] == "pass"),
        "{}",
        serde_json::to_string_pretty(&reports).unwrap()
    );
}

#[test]
fn comparator_preserves_semantic_differences() {
    assert_ne!(Cell::Null, Cell::Bytes(b"NULL".to_vec()));
    assert_ne!(Cell::Int(1), Cell::Bytes(b"1".to_vec()));
    let error = |code, state: &str| Observation::Error {
        code,
        sqlstate: state.into(),
    };
    assert_ne!(error(1062, "23000"), error(1064, "42000"));
    let set = |rows, affected_rows| ResultSet {
        columns: vec![],
        rows,
        affected_rows,
    };
    assert_ne!(
        set(vec![vec![Cell::Int(1)]], 0),
        set(vec![vec![Cell::Int(1)], vec![Cell::Int(1)]], 0)
    );
    assert_ne!(set(vec![], 1), set(vec![], 2));
    assert_eq!(
        database_url("mysql://root@localhost/test?foo=bar", "fixture"),
        "mysql://root@localhost/fixture?foo=bar"
    );
}

#[test]
fn wire_comparison_detects_order_duplicates_errors_and_rollback() {
    let mut conn = connect(&start_mysqweel()).unwrap();
    conn.query_drop("CREATE TABLE cmp (id INT PRIMARY KEY, value INT)")
        .unwrap();
    conn.query_drop("INSERT INTO cmp VALUES (1, 10), (2, 20)")
        .unwrap();
    let query = |conn: &mut Conn, sql: &str, ordered| {
        observe(
            conn,
            &Step {
                sql: sql.into(),
                connection: default_connection(),
                ordered,
                expect_error: false,
            },
        )
        .unwrap()
    };
    let forward = "SELECT value FROM cmp ORDER BY id ASC";
    let reverse = "SELECT value FROM cmp ORDER BY id DESC";
    assert_eq!(
        query(&mut conn, forward, false),
        query(&mut conn, reverse, false)
    );
    assert_ne!(
        query(&mut conn, forward, true),
        query(&mut conn, reverse, true)
    );
    assert_ne!(
        query(&mut conn, "SELECT value FROM cmp", false),
        query(
            &mut conn,
            "SELECT value FROM cmp UNION ALL SELECT value FROM cmp",
            false
        )
    );
    assert_ne!(
        query(&mut conn, "SELECT NULL AS value", false),
        query(&mut conn, "SELECT 'NULL' AS value", false)
    );
    let before = query(&mut conn, forward, true);
    conn.query_drop("START TRANSACTION").unwrap();
    let update = query(&mut conn, "UPDATE cmp SET value=30 WHERE id=1", false);
    let no_update = query(&mut conn, "UPDATE cmp SET value=30 WHERE id=99", false);
    assert_ne!(update, no_update);
    assert_ne!(before, query(&mut conn, forward, true));
    conn.query_drop("ROLLBACK").unwrap();
    assert_eq!(before, query(&mut conn, forward, true));
    let duplicate = query(&mut conn, "INSERT INTO cmp VALUES (1, 99)", false);
    let syntax = query(&mut conn, "SELECT FROM WHERE", false);
    assert!(matches!(duplicate, Observation::Error { code: 1062, .. }));
    assert!(matches!(syntax, Observation::Error { .. }));
    assert_ne!(duplicate, syntax);
}
