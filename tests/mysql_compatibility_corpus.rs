mod common;

use std::collections::BTreeMap;
use std::net::TcpListener;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use my_sqweel::server::WireServer;
use my_sqweel::sql::engine::{Engine, EngineConfig};
use mysql::prelude::Queryable;
use mysql::{Opts, Pool, Row, Value as MyValue};

const MIN_COMPATIBILITY_PERCENT: f64 = 100.0;
const MIN_CORPUS_SIZE: usize = 2_500;
const GENERATED_CASES_PER_FAMILY: i64 = 489;
const CORPUS_WORKERS: usize = 8;

struct Case {
    id: String,
    family: &'static str,
    sql: String,
    ordered: bool,
}

#[derive(Debug, PartialEq, Eq)]
struct ObservedQuery {
    columns: Vec<String>,
    rows: Vec<Vec<String>>,
}

struct CaseOutcome {
    family: &'static str,
    passed: bool,
    failure: Option<String>,
}

#[test]
fn representative_query_corpus_meets_mysql_compatibility_floor() {
    let _guard = common::test_lock();
    let mysql_target = common::mysql_compare_target();

    let mysqweel_url = start_mysqweel_server();
    let mysqweel_pool = Pool::new(Opts::from_url(&mysqweel_url).expect("valid MySqweel URL"))
        .expect("create MySqweel pool");
    let mut mysqweel = connect_with_retry(&mysqweel_pool, "MySqweel");
    let mysql_pool = mysql_target.as_ref().map(|target| {
        Pool::new(Opts::from_url(target.url()).expect("valid MySQL comparison URL"))
            .expect("create MySQL comparison pool")
    });
    let mut mysql = mysql_pool
        .as_ref()
        .map(|pool| connect_with_retry(pool, "MySQL"));

    let suffix = format!("{}_{}", std::process::id(), uuid::Uuid::new_v4().simple());
    let values_table = format!("compat_values_{suffix}");
    let related_table = format!("compat_related_{suffix}");

    setup_fixture(&mut mysqweel, &values_table, &related_table);
    if let Some(mysql) = mysql.as_mut() {
        setup_fixture(mysql, &values_table, &related_table);
    }

    let cases = compatibility_cases(&values_table, &related_table);
    assert!(
        cases.len() >= MIN_CORPUS_SIZE,
        "the compatibility corpus must contain at least {MIN_CORPUS_SIZE} cases"
    );
    let mut failures = Vec::new();
    let mut family_scores = BTreeMap::<&str, (usize, usize)>::new();
    let mut passed = 0_usize;

    let chunk_size = cases.len().div_ceil(CORPUS_WORKERS);
    let outcomes = thread::scope(|scope| {
        let mysqweel_pool = &mysqweel_pool;
        let mysql_pool = mysql_pool.as_ref();
        cases
            .chunks(chunk_size)
            .map(|chunk| scope.spawn(move || evaluate_cases(chunk, mysqweel_pool, mysql_pool)))
            .collect::<Vec<_>>()
            .into_iter()
            .flat_map(|handle| handle.join().expect("compatibility corpus worker panicked"))
            .collect::<Vec<_>>()
    });

    for outcome in outcomes {
        let score = family_scores.entry(outcome.family).or_default();
        score.1 += 1;
        if outcome.passed {
            passed += 1;
            score.0 += 1;
        }
        if let Some(failure) = outcome.failure {
            failures.push(failure);
        }
    }

    cleanup_fixture(&mut mysqweel, &values_table, &related_table);
    if let Some(mysql) = mysql.as_mut() {
        cleanup_fixture(mysql, &values_table, &related_table);
    }

    let percent = passed as f64 * 100.0 / cases.len() as f64;
    eprintln!(
        "MySQL query corpus: {passed}/{} ({percent:.1}%), comparison={}",
        cases.len(),
        if mysql.is_some() {
            "real MySQL"
        } else {
            "local acceptance only"
        }
    );
    for (family, (family_passed, family_total)) in family_scores {
        eprintln!("  {family}: {family_passed}/{family_total}");
    }
    if !failures.is_empty() {
        eprintln!("MySQL compatibility mismatches:\n{}", failures.join("\n\n"));
    }

    if mysql.is_none() {
        eprintln!(
            "Set MYSQL_COMPARE_URL to compare values and result shapes; CI sets MYSQL_PARITY_REQUIRED=1 so differential coverage cannot be skipped."
        );
    }

    assert!(
        percent >= MIN_COMPATIBILITY_PERCENT,
        "query compatibility {percent:.1}% is below the {MIN_COMPATIBILITY_PERCENT:.1}% floor:\n{}",
        failures.join("\n\n")
    );
}

fn evaluate_cases(
    cases: &[Case],
    mysqweel_pool: &Pool,
    mysql_pool: Option<&Pool>,
) -> Vec<CaseOutcome> {
    let mut mysqweel = connect_with_retry(mysqweel_pool, "MySqweel corpus worker");
    let mut mysql = mysql_pool.map(|pool| connect_with_retry(pool, "MySQL corpus worker"));

    cases
        .iter()
        .map(|case| {
            let mysqweel_result = observe_query(&mut mysqweel, &case.sql, case.ordered);
            if let Some(mysql) = mysql.as_mut() {
                let mysql_result = observe_query(mysql, &case.sql, case.ordered);
                let passed = mysqweel_result == mysql_result && mysqweel_result.is_ok();
                CaseOutcome {
                    family: case.family,
                    passed,
                    failure: (!passed).then(|| {
                        format!(
                            "{} [{}]\n  SQL: {}\n  MySQL: {:?}\n  MySqweel: {:?}",
                            case.id, case.family, case.sql, mysql_result, mysqweel_result
                        )
                    }),
                }
            } else {
                match mysqweel_result {
                    Ok(_) => CaseOutcome {
                        family: case.family,
                        passed: true,
                        failure: None,
                    },
                    Err(error) => CaseOutcome {
                        family: case.family,
                        passed: false,
                        failure: Some(format!(
                            "{} [{}]\n  SQL: {}\n  MySqweel: {error}",
                            case.id, case.family, case.sql
                        )),
                    },
                }
            }
        })
        .collect()
}

fn start_mysqweel_server() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind local port");
    let address = listener.local_addr().expect("read local address");
    let engine = Arc::new(Engine::new(EngineConfig::mysql_strict()));
    engine.execute_sql("CREATE DATABASE test").unwrap();
    thread::spawn(move || {
        WireServer::new(engine)
            .serve_listener(listener)
            .expect("MySqweel wire server should run");
    });
    format!("mysql://root@{address}/test")
}

fn connect_with_retry(pool: &Pool, label: &str) -> mysql::PooledConn {
    let mut last_error = None;
    for _ in 0..50 {
        match pool.get_conn() {
            Ok(connection) => return connection,
            Err(error) => last_error = Some(error),
        }
        thread::sleep(Duration::from_millis(100));
    }
    panic!(
        "could not connect to {label}: {}",
        last_error.expect("connection attempt should record an error")
    );
}

fn setup_fixture(conn: &mut mysql::PooledConn, values_table: &str, related_table: &str) {
    let _ = conn.query_drop(format!("DROP TABLE IF EXISTS {related_table}"));
    let _ = conn.query_drop(format!("DROP TABLE IF EXISTS {values_table}"));
    conn.query_drop(format!(
        "CREATE TABLE {values_table} (\
            id BIGINT PRIMARY KEY, \
            category VARCHAR(32), \
            amount BIGINT, \
            label VARCHAR(64)\
        )"
    ))
    .expect("create compatibility values fixture");
    conn.query_drop(format!(
        "CREATE TABLE {related_table} (\
            id BIGINT PRIMARY KEY, \
            compat_id BIGINT, \
            tag VARCHAR(32)\
        )"
    ))
    .expect("create compatibility related fixture");
    conn.query_drop(format!(
        "INSERT INTO {values_table} (id, category, amount, label) VALUES \
            (1, 'alpha', 10, 'One'), \
            (2, 'alpha', NULL, 'Two'), \
            (3, 'beta', 20, 'Three'), \
            (4, 'beta', 0, NULL)"
    ))
    .expect("insert compatibility values fixture");
    conn.query_drop(format!(
        "INSERT INTO {related_table} (id, compat_id, tag) VALUES \
            (10, 1, 'x'), (11, 1, 'y'), (12, 3, 'z'), (13, 99, 'orphan')"
    ))
    .expect("insert compatibility related fixture");
}

fn cleanup_fixture(conn: &mut mysql::PooledConn, values_table: &str, related_table: &str) {
    let _ = conn.query_drop(format!("DROP TABLE IF EXISTS {related_table}"));
    let _ = conn.query_drop(format!("DROP TABLE IF EXISTS {values_table}"));
}

fn compatibility_cases(values: &str, related: &str) -> Vec<Case> {
    let case = |id: &str, family, sql: String| Case {
        id: id.to_string(),
        family,
        sql,
        ordered: true,
    };

    let mut cases = vec![
        case(
            "null-equality",
            "null-logic",
            "SELECT NULL = 1 AS value".into(),
        ),
        case("not-null", "null-logic", "SELECT NOT NULL AS value".into()),
        case(
            "and-unknown",
            "null-logic",
            "SELECT TRUE AND NULL AS value".into(),
        ),
        case(
            "and-false",
            "null-logic",
            "SELECT FALSE AND NULL AS value".into(),
        ),
        case(
            "or-true",
            "null-logic",
            "SELECT TRUE OR NULL AS value".into(),
        ),
        case(
            "or-unknown",
            "null-logic",
            "SELECT FALSE OR NULL AS value".into(),
        ),
        case(
            "not-in-null",
            "null-logic",
            "SELECT 1 NOT IN (2, NULL) AS value".into(),
        ),
        case(
            "between-null",
            "null-logic",
            "SELECT NULL BETWEEN 1 AND 2 AS value".into(),
        ),
        case(
            "null-safe-equality",
            "comparison",
            "SELECT NULL <=> NULL AS value".into(),
        ),
        case(
            "case-insensitive-string-equality",
            "comparison",
            "SELECT 'Alpha' = 'alpha' AS value".into(),
        ),
        case(
            "integer-division",
            "numeric",
            "SELECT 7 DIV 2 AS value".into(),
        ),
        case("bitwise-xor", "numeric", "SELECT 5 ^ 3 AS value".into()),
        case(
            "leading-numeric-coercion",
            "numeric",
            "SELECT '12tail' + 1 AS value".into(),
        ),
        case(
            "null-arithmetic",
            "numeric",
            "SELECT NULL + 1 AS value".into(),
        ),
        case("floor", "numeric", "SELECT FLOOR(-5.3) AS value".into()),
        case("ceil", "numeric", "SELECT CEIL(10.1) AS value".into()),
        case(
            "byte-length",
            "strings",
            "SELECT LENGTH('é') AS value".into(),
        ),
        case(
            "character-length",
            "strings",
            "SELECT CHAR_LENGTH('é') AS value".into(),
        ),
        case(
            "substring",
            "strings",
            "SELECT SUBSTRING('abcdef', 2, 3) AS value".into(),
        ),
        case(
            "concat-coalesce",
            "strings",
            "SELECT CONCAT('a', COALESCE(NULL, 'b')) AS value".into(),
        ),
        case(
            "trim",
            "strings",
            "SELECT TRIM('  value  ') AS value".into(),
        ),
        case(
            "trim-custom",
            "strings",
            "SELECT TRIM(BOTH 'x' FROM 'xxvaluexxx') AS value".into(),
        ),
        case(
            "datediff",
            "datetime",
            "SELECT DATEDIFF('2026-01-03', '2026-01-01') AS value".into(),
        ),
        case(
            "timestampdiff",
            "datetime",
            "SELECT TIMESTAMPDIFF(DAY, '2026-01-01', '2026-01-04') AS value".into(),
        ),
        case(
            "cast-char",
            "conversion",
            "SELECT CAST(12 AS CHAR) AS value".into(),
        ),
        case(
            "where-numeric-coercion",
            "filtering",
            format!("SELECT id FROM {values} WHERE amount = '10tail' ORDER BY id"),
        ),
        case(
            "where-null",
            "filtering",
            format!("SELECT id FROM {values} WHERE amount IS NULL ORDER BY id"),
        ),
        case(
            "where-not",
            "filtering",
            format!("SELECT id FROM {values} WHERE NOT (amount < 20) ORDER BY id"),
        ),
        case(
            "where-like",
            "filtering",
            format!("SELECT id FROM {values} WHERE label LIKE 'T%' ORDER BY id"),
        ),
        case(
            "case-expression",
            "projection",
            format!(
                "SELECT id, CASE WHEN amount IS NULL THEN 'missing' ELSE 'set' END AS state \
                 FROM {values} ORDER BY id"
            ),
        ),
        case(
            "distinct",
            "projection",
            format!("SELECT DISTINCT category FROM {values} ORDER BY category"),
        ),
        case(
            "qualified-case-insensitive-column",
            "projection",
            format!("SELECT c.ID AS id FROM {values} AS c WHERE c.ID = 1"),
        ),
        case(
            "count-and-sum",
            "aggregate",
            format!("SELECT COUNT(*) AS n, SUM(amount) AS total FROM {values}"),
        ),
        case(
            "nested-aggregate",
            "aggregate",
            format!("SELECT SUM(amount) + 1 AS value FROM {values}"),
        ),
        case(
            "empty-aggregate",
            "aggregate",
            format!(
                "SELECT SUM(amount) AS sum_value, COALESCE(SUM(amount), 0) AS fallback \
                 FROM {values} WHERE id < 0"
            ),
        ),
        case(
            "count-distinct",
            "aggregate",
            format!("SELECT COUNT(DISTINCT category) AS value FROM {values}"),
        ),
        case(
            "group-having-alias",
            "aggregate",
            format!(
                "SELECT category, COUNT(*) AS n FROM {values} \
                 GROUP BY category HAVING n = 2 ORDER BY category"
            ),
        ),
        case(
            "order-alias-limit",
            "ordering",
            format!(
                "SELECT id, amount + 5 AS bumped FROM {values} WHERE amount IS NOT NULL \
                 ORDER BY bumped DESC LIMIT 2 OFFSET 1"
            ),
        ),
        case(
            "left-join",
            "joins",
            format!(
                "SELECT c.id AS compat_id, r.id AS related_id FROM {values} AS c \
                 LEFT JOIN {related} AS r ON r.compat_id = c.id \
                 WHERE c.id IN (1, 2) ORDER BY c.id, r.id"
            ),
        ),
        case(
            "cross-join",
            "joins",
            format!("SELECT COUNT(*) AS n FROM {values} AS c CROSS JOIN {related} AS r"),
        ),
        case(
            "scalar-subquery",
            "subqueries",
            format!("SELECT (SELECT COUNT(*) FROM {related}) AS value"),
        ),
        case(
            "exists-subquery",
            "subqueries",
            format!(
                "SELECT id FROM {values} WHERE EXISTS \
                 (SELECT id FROM {related} WHERE tag = 'z') ORDER BY id"
            ),
        ),
        case(
            "in-subquery",
            "subqueries",
            format!(
                "SELECT id FROM {values} WHERE id IN \
                 (SELECT compat_id FROM {related} WHERE tag IN ('x', 'z')) ORDER BY id"
            ),
        ),
        case(
            "derived-table",
            "subqueries",
            format!(
                "SELECT d.id FROM (SELECT id, amount FROM {values} WHERE amount >= 10) AS d \
                 WHERE d.amount = 20 ORDER BY d.id"
            ),
        ),
        case(
            "union-distinct",
            "sets",
            "SELECT 1 AS value UNION SELECT 1 AS value UNION SELECT 2 AS value ORDER BY value"
                .into(),
        ),
        case(
            "union-all",
            "sets",
            "SELECT 1 AS value UNION ALL SELECT 1 AS value ORDER BY value".into(),
        ),
        case(
            "qualified-wildcard",
            "joins",
            format!(
                "SELECT r.* FROM {values} AS c RIGHT JOIN {related} AS r ON r.compat_id = c.id ORDER BY r.id"
            ),
        ),
        case(
            "join-using",
            "joins",
            format!(
                "SELECT c.id, c.category FROM {values} AS c JOIN (SELECT compat_id AS id FROM {related}) AS r USING (id) ORDER BY c.id"
            ),
        ),
        case(
            "derived-join",
            "joins",
            format!(
                "SELECT c.id, r.tag FROM {values} AS c JOIN (SELECT compat_id, tag FROM {related}) AS r ON r.compat_id = c.id ORDER BY c.id, r.tag"
            ),
        ),
        case(
            "nonrecursive-cte",
            "cte",
            format!(
                "WITH selected AS (SELECT id, amount FROM {values} WHERE amount IS NOT NULL) SELECT id FROM selected ORDER BY id"
            ),
        ),
        case(
            "cte-column-aliases",
            "cte",
            format!(
                "WITH selected (value_id, value_amount) AS (SELECT id, amount FROM {values}) SELECT value_id FROM selected WHERE value_amount >= 10 ORDER BY value_id"
            ),
        ),
        case(
            "window-row-number",
            "windows",
            format!(
                "SELECT id, ROW_NUMBER() OVER (PARTITION BY category ORDER BY id) AS rn FROM {values} ORDER BY id"
            ),
        ),
        case(
            "window-running-sum",
            "windows",
            format!(
                "SELECT id, SUM(amount) OVER (ORDER BY id ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW) AS running FROM {values} ORDER BY id"
            ),
        ),
        case(
            "window-lag",
            "windows",
            format!(
                "SELECT id, LAG(amount, 1, 0) OVER (ORDER BY id) AS previous_amount FROM {values} ORDER BY id"
            ),
        ),
        case(
            "window-cume-dist-peers",
            "windows",
            format!(
                "SELECT id, CUME_DIST() OVER (ORDER BY category) AS distribution FROM {values} ORDER BY id"
            ),
        ),
    ];

    // Five balanced deterministic families catch semantic regressions without
    // making CI dependent on a fuzzer seed. Together with the 55 curated cases
    // above, these produce exactly 2,500 differential queries.
    for seed in 0_i64..GENERATED_CASES_PER_FAMILY {
        let left = seed - (GENERATED_CASES_PER_FAMILY / 2);
        let right = (seed % 17) + 1;
        let adjustment = (seed % 11) - (seed % 7);
        let (numeric_operation, numeric_expression) = match seed % 8 {
            0 => ("add", format!("{left} + {right}")),
            1 => ("subtract", format!("{left} - {right}")),
            2 => ("multiply", format!("{left} * {right}")),
            3 => ("integer-divide", format!("{left} DIV {right}")),
            4 => ("modulo", format!("MOD({left}, {right})")),
            5 => ("absolute", format!("ABS({left}) + {adjustment}")),
            6 => (
                "greatest",
                format!("GREATEST({left}, {right}, {adjustment})"),
            ),
            _ => ("least", format!("LEAST({left}, {right}, {adjustment})")),
        };
        cases.push(Case {
            id: format!("generated-numeric-{numeric_operation}-{seed}"),
            family: "generated-numeric",
            sql: format!("SELECT {numeric_expression} AS value"),
            ordered: true,
        });

        let offset = (seed % 9) - 4;
        let candidate = left + offset;
        let lower = left - 2;
        let upper = left + 2;
        let (comparison_operation, comparison_expression) = match seed % 9 {
            0 => ("null-safe", format!("{candidate} <=> {left}")),
            1 => ("equal", format!("{candidate} = {left}")),
            2 => ("not-equal", format!("{candidate} <> {left}")),
            3 => ("less", format!("{candidate} < {left}")),
            4 => ("less-equal", format!("{candidate} <= {left}")),
            5 => ("greater", format!("{candidate} > {left}")),
            6 => ("greater-equal", format!("{candidate} >= {left}")),
            7 => (
                "between",
                format!("{candidate} BETWEEN {lower} AND {upper}"),
            ),
            _ => (
                "in-null-list",
                format!("{candidate} IN ({left}, {right}, NULL)"),
            ),
        };
        cases.push(Case {
            id: format!("generated-comparison-{comparison_operation}-{seed}"),
            family: "generated-comparison",
            sql: format!("SELECT {comparison_expression} AS value"),
            ordered: true,
        });

        let text = format!("compat-{seed}-AbC");
        let substring_start = (seed % 6) + 1;
        let substring_length = (seed % 4) + 1;
        let pad_width = (seed % 18) + 1;
        let (string_operation, string_expression) = match seed % 10 {
            0 => ("concat", format!("CONCAT('prefix-', LOWER('{text}'))")),
            1 => ("length", format!("LENGTH('{text}')")),
            2 => ("char-length", format!("CHAR_LENGTH('{text}')")),
            3 => (
                "substring",
                format!("SUBSTRING('{text}', {substring_start}, {substring_length})"),
            ),
            4 => ("left", format!("LEFT('{text}', {substring_length})")),
            5 => ("right", format!("RIGHT('{text}', {substring_length})")),
            6 => ("left-pad", format!("LPAD('{text}', {pad_width}, '0')")),
            7 => ("right-pad", format!("RPAD('{text}', {pad_width}, 'x')")),
            8 => ("replace", format!("REPLACE('{text}', 'A', 'value')")),
            _ => ("locate", format!("LOCATE('compat', '{text}')")),
        };
        cases.push(Case {
            id: format!("generated-string-{string_operation}-{seed}"),
            family: "generated-strings",
            sql: format!("SELECT {string_expression} AS value"),
            ordered: true,
        });

        let fallback = 1_000 + seed;
        let (null_operation, null_expression) = match seed % 6 {
            0 => ("coalesce", format!("COALESCE(NULL, NULL, {left})")),
            1 => ("if-null", format!("IFNULL(NULL, {left})")),
            2 => ("null-if", format!("NULLIF({left}, {candidate})")),
            3 => (
                "if",
                format!("IF({left} < {candidate}, {left}, {fallback})"),
            ),
            4 => (
                "case",
                format!("CASE WHEN {left} = {candidate} THEN NULL ELSE {fallback} END"),
            ),
            _ => (
                "null-if-fallback",
                format!("COALESCE(NULLIF({left}, {candidate}), {fallback})"),
            ),
        };
        cases.push(Case {
            id: format!("generated-null-control-{null_operation}-{seed}"),
            family: "generated-null-control",
            sql: format!("SELECT {null_expression} AS value"),
            ordered: true,
        });

        let id_limit = (seed % 4) + 1;
        let next_id = (id_limit % 4) + 1;
        let category = if seed % 2 == 0 { "alpha" } else { "beta" };
        let (filter_operation, filter_expression) = match seed % 8 {
            0 => ("range", format!("amount >= {left} AND id <= {id_limit}")),
            1 => (
                "between",
                format!("amount BETWEEN {lower} AND {upper} AND id <= {id_limit}"),
            ),
            2 => (
                "computed-in",
                format!("id + {seed} IN ({}, {})", seed + id_limit, seed + next_id),
            ),
            3 => ("not", format!("NOT (amount < {left}) AND id <= {id_limit}")),
            4 => (
                "like",
                format!("CONCAT(label, '-{seed}') LIKE '%-{seed}' AND id <= {id_limit}"),
            ),
            5 => (
                "category",
                format!("category = '{category}' AND id <= {id_limit}"),
            ),
            6 => (
                "null-or-equal",
                format!("(amount IS NULL OR amount = {right}) AND id <= {id_limit}"),
            ),
            _ => (
                "coalesce",
                format!("COALESCE(amount, -999) >= {left} AND id <= {id_limit}"),
            ),
        };
        cases.push(Case {
            id: format!("generated-filtering-{filter_operation}-{seed}"),
            family: "generated-filtering",
            sql: format!("SELECT id FROM {values} WHERE {filter_expression} ORDER BY id"),
            ordered: true,
        });
    }
    cases
}

fn observe_query(
    conn: &mut mysql::PooledConn,
    sql: &str,
    ordered: bool,
) -> Result<ObservedQuery, String> {
    let mut result = conn.query_iter(sql).map_err(|error| error.to_string())?;
    let columns = result
        .columns()
        .as_ref()
        .iter()
        .map(|column| column.name_str().into_owned())
        .collect::<Vec<_>>();
    let rows = result
        .by_ref()
        .collect::<mysql::Result<Vec<Row>>>()
        .map_err(|error| error.to_string())?;
    let mut rows = rows.into_iter().map(normalize_row).collect::<Vec<_>>();
    if !ordered {
        rows.sort();
    }
    Ok(ObservedQuery { columns, rows })
}

fn normalize_row(row: Row) -> Vec<String> {
    row.unwrap().into_iter().map(normalize_value).collect()
}

fn normalize_value(value: MyValue) -> String {
    match value {
        MyValue::NULL => "NULL".to_string(),
        MyValue::Bytes(value) => String::from_utf8_lossy(&value).into_owned(),
        MyValue::Int(value) => value.to_string(),
        MyValue::UInt(value) => value.to_string(),
        MyValue::Float(value) => normalize_float(f64::from(value)),
        MyValue::Double(value) => normalize_float(value),
        MyValue::Date(year, month, day, hour, minute, second, micros) => {
            format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}:{second:02}.{micros:06}")
        }
        MyValue::Time(negative, days, hours, minutes, seconds, micros) => format!(
            "{}{:02}:{minutes:02}:{seconds:02}.{micros:06}",
            if negative { "-" } else { "" },
            days * 24 + u32::from(hours)
        ),
    }
}

fn normalize_float(value: f64) -> String {
    if value.is_finite() && value.fract().abs() < 1e-9 {
        return format!("{value:.0}");
    }
    let normalized = format!("{value:.12}");
    normalized
        .trim_end_matches('0')
        .trim_end_matches('.')
        .to_string()
}
