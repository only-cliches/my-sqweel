//! Deterministic, schema-aware programs for differential testing.
//!
//! This is deliberately not a raw SQL text fuzzer. Every generated program is
//! inside the documented compatibility subset, has a small typed schema, and
//! leaves a reproducible seed in failures. It complements MTR (upstream
//! regression coverage) and the hand-authored fixture corpus (specific bugs).
mod common;

use std::collections::BTreeSet;
use std::net::TcpListener;
use std::sync::Arc;

use my_sqweel::server::WireServer;
use my_sqweel::sql::engine::{Engine, EngineConfig};
use mysql::prelude::Queryable;
use mysql::{Conn, Opts, Value};

const DEFAULT_SEED_COUNT: u64 = 64;

#[derive(Debug, Clone, PartialEq, Eq)]
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
            Value::Bytes(value) => Self::Bytes(value),
            Value::Int(value) => Self::Int(value),
            Value::UInt(value) => Self::UInt(value),
            Value::Float(value) => Self::Float(value.to_bits()),
            Value::Double(value) => Self::Double(value.to_bits()),
            Value::Date(year, month, day, hour, minute, second, micros) => {
                Self::Date(year, month, day, hour, minute, second, micros)
            }
            Value::Time(negative, days, hours, minutes, seconds, micros) => {
                Self::Time(negative, days, hours, minutes, seconds, micros)
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ResultSet {
    columns: Vec<(String, String)>,
    rows: Vec<Vec<Cell>>,
    affected_rows: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Observation {
    Success(Vec<ResultSet>),
    Error { code: u16, sqlstate: String },
}

#[derive(Debug, Clone, Copy)]
enum Mutation {
    Score { id: i64, delta: i64 },
    Savepoint { rolled_back: i64, committed: i64 },
    ChildAmounts { delta: i64 },
    Tags,
}

impl Mutation {
    fn name(self) -> &'static str {
        match self {
            Self::Score { .. } => "transaction-update",
            Self::Savepoint { .. } => "savepoint-rollback",
            Self::ChildAmounts { .. } => "child-update",
            Self::Tags => "expression-update",
        }
    }

    fn statements(self, parent: &str, child: &str) -> Vec<String> {
        match self {
            Self::Score { id, delta } => vec![
                "START TRANSACTION".into(),
                format!("UPDATE {parent} SET score = score + {delta} WHERE id = {id}"),
                "COMMIT".into(),
            ],
            Self::Savepoint {
                rolled_back,
                committed,
            } => vec![
                "START TRANSACTION".into(),
                "SAVEPOINT before_change".into(),
                format!("UPDATE {parent} SET score = score + 100 WHERE id = {rolled_back}"),
                "ROLLBACK TO SAVEPOINT before_change".into(),
                format!("UPDATE {parent} SET score = score - 1 WHERE id = {committed}"),
                "COMMIT".into(),
            ],
            Self::ChildAmounts { delta } => vec![format!(
                "UPDATE {child} SET amount = amount + {delta} WHERE id IN (1, 3, 4)"
            )],
            Self::Tags => vec![format!(
                "UPDATE {parent} SET tag = CONCAT(category, '-', id) WHERE score IS NOT NULL"
            )],
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum Query {
    LeftJoinAggregate { minimum: i64 },
    CorrelatedSubquery { minimum: i64 },
    DerivedCase { boundary: i64 },
    Exists { minimum: i64 },
}

impl Query {
    fn name(self) -> &'static str {
        match self {
            Self::LeftJoinAggregate { .. } => "left-join-aggregate",
            Self::CorrelatedSubquery { .. } => "correlated-subquery",
            Self::DerivedCase { .. } => "derived-case-grouping",
            Self::Exists { .. } => "exists-subquery",
        }
    }

    fn sql(self, parent: &str, child: &str) -> String {
        match self {
            Self::LeftJoinAggregate { minimum } => format!(
                "SELECT p.id, p.category, COUNT(c.id) AS child_count, \
                 COALESCE(SUM(c.amount), 0) AS total_amount \
                 FROM {parent} AS p LEFT JOIN {child} AS c ON c.parent_id = p.id \
                 WHERE p.score IS NULL OR p.score >= {minimum} \
                 GROUP BY p.id, p.category ORDER BY p.id"
            ),
            Self::CorrelatedSubquery { minimum } => format!(
                "SELECT p.id, (SELECT COUNT(*) FROM {child} AS c \
                 WHERE c.parent_id = p.id AND c.amount IS NOT NULL) AS child_count \
                 FROM {parent} AS p WHERE p.id IN \
                 (SELECT c.parent_id FROM {child} AS c WHERE c.amount >= {minimum}) \
                 ORDER BY p.id"
            ),
            Self::DerivedCase { boundary } => format!(
                "SELECT d.bucket, COUNT(*) AS row_count, SUM(d.normalized_score) AS total_score \
                 FROM (SELECT CASE WHEN score IS NULL THEN 'missing' \
                 WHEN score >= {boundary} THEN 'high' ELSE 'low' END AS bucket, \
                 COALESCE(score, 0) AS normalized_score FROM {parent}) AS d \
                 GROUP BY d.bucket ORDER BY d.bucket"
            ),
            Self::Exists { minimum } => format!(
                "SELECT p.id, p.category FROM {parent} AS p WHERE EXISTS \
                 (SELECT 1 FROM {child} AS c WHERE c.parent_id = p.id \
                 AND (c.amount IS NULL OR c.amount >= {minimum})) ORDER BY p.id"
            ),
        }
    }
}

/// A program has an explicit schema, deterministic data, state mutation, and
/// queries. The typed variants above are the generator's compatibility grammar.
#[derive(Debug)]
struct Program {
    seed: u64,
    parent: String,
    child: String,
    setup: Vec<String>,
    steps: Vec<String>,
    checks: Vec<String>,
    mutation: Mutation,
    query: Query,
}

impl Program {
    fn generate(seed: u64) -> Self {
        let mut random = Deterministic::new(seed);
        let nonce = format!("stateful_{seed}_{}", random.next_u32());
        let parent = format!("parents_{nonce}");
        let child = format!("children_{nonce}");
        let score = random.range_i64(-12, 18);
        let mutation = match random.pick(4) {
            0 => Mutation::Score {
                id: random.range_i64(1, 4),
                delta: random.range_i64(-4, 5),
            },
            1 => Mutation::Savepoint {
                rolled_back: random.range_i64(1, 4),
                committed: random.range_i64(1, 4),
            },
            2 => Mutation::ChildAmounts {
                delta: random.range_i64(-3, 4),
            },
            _ => Mutation::Tags,
        };
        let query = match random.pick(4) {
            0 => Query::LeftJoinAggregate {
                minimum: random.range_i64(-3, 15),
            },
            1 => Query::CorrelatedSubquery {
                minimum: random.range_i64(-4, 10),
            },
            2 => Query::DerivedCase {
                boundary: random.range_i64(-2, 16),
            },
            _ => Query::Exists {
                minimum: random.range_i64(-4, 10),
            },
        };
        let setup = vec![
            format!(
                "CREATE TABLE {parent} (id INT PRIMARY KEY, category VARCHAR(16) NOT NULL, \
                 score INT NULL, tag VARCHAR(16) NULL)"
            ),
            format!(
                "CREATE TABLE {child} (id INT PRIMARY KEY, parent_id INT NULL, amount INT NULL, \
                 label VARCHAR(16) NOT NULL)"
            ),
            format!(
                "INSERT INTO {parent} (id, category, score, tag) VALUES \
                 (1, 'alpha', 10, 'x'), (2, 'alpha', NULL, NULL), \
                 (3, 'beta', {score}, 'z'), (4, 'gamma', 0, NULL)"
            ),
            format!(
                "INSERT INTO {child} (id, parent_id, amount, label) VALUES \
                 (1, 1, 5, 'open'), (2, 1, NULL, 'missing'), (3, 2, 9, 'done'), \
                 (4, 3, -3, 'late'), (5, NULL, 7, 'orphan')"
            ),
        ];
        let mut checks = vec![query.sql(&parent, &child)];
        // Canonical table-state checks make every mutation observable, not just
        // the selected query shape.
        checks.push(format!(
            "SELECT id, category, score, tag FROM {parent} ORDER BY id"
        ));
        checks.push(format!(
            "SELECT id, parent_id, amount, label FROM {child} ORDER BY id"
        ));
        let steps = mutation.statements(&parent, &child);
        Self {
            seed,
            parent,
            child,
            setup,
            steps,
            checks,
            mutation,
            query,
        }
    }

    fn features(&self) -> [&'static str; 2] {
        [self.mutation.name(), self.query.name()]
    }

    fn cleanup(&self) -> [String; 2] {
        [
            format!("DROP TABLE {}", self.child),
            format!("DROP TABLE {}", self.parent),
        ]
    }

    fn sql(&self) -> Vec<&str> {
        self.setup
            .iter()
            .chain(&self.steps)
            .chain(&self.checks)
            .map(String::as_str)
            .collect()
    }
}

/// Tiny stable PRNG: adding `rand` behavior or changing its version must not
/// silently change the committed seed corpus.
struct Deterministic(u64);

impl Deterministic {
    fn new(seed: u64) -> Self {
        Self(seed ^ 0x9e37_79b9_7f4a_7c15)
    }

    fn next_u32(&mut self) -> u32 {
        self.0 ^= self.0 << 7;
        self.0 ^= self.0 >> 9;
        self.0 ^= self.0 << 8;
        self.0 as u32
    }

    fn pick(&mut self, exclusive: u32) -> u32 {
        self.next_u32() % exclusive
    }

    fn range_i64(&mut self, start: i64, end: i64) -> i64 {
        start + i64::from(self.pick((end - start) as u32))
    }
}

fn start_mysqweel() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind MySqweel server");
    let address = listener.local_addr().expect("MySqweel listener address");
    let engine = Arc::new(Engine::new(EngineConfig::mysql_strict()));
    engine.execute_sql("CREATE DATABASE test").unwrap();
    std::thread::spawn(move || WireServer::new(engine).serve_listener(listener).unwrap());
    format!("mysql://root@{address}/test")
}

fn connect(url: &str) -> Conn {
    let mut conn = Conn::new(Opts::from_url(url).expect("valid connection URL"))
        .expect("connect to comparison server");
    for sql in [
        "SET SESSION sql_mode = 'STRICT_TRANS_TABLES,NO_ENGINE_SUBSTITUTION'",
        "SET SESSION time_zone = '+00:00'",
        "SET NAMES utf8mb4 COLLATE utf8mb4_general_ci",
    ] {
        conn.query_drop(sql)
            .unwrap_or_else(|error| panic!("session setup {sql:?}: {error}"));
    }
    conn
}

fn observe(conn: &mut Conn, sql: &str) -> Observation {
    let mut result = match conn.query_iter(sql) {
        Ok(result) => result,
        Err(mysql::Error::MySqlError(error)) => {
            return Observation::Error {
                code: error.code,
                sqlstate: error.state,
            };
        }
        Err(error) => panic!("protocol failure for {sql:?}: {error}"),
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
            let row = row.unwrap_or_else(|error| panic!("row failure for {sql:?}: {error}"));
            rows.push(row.unwrap().into_iter().map(Cell::from).collect());
        }
        sets.push(ResultSet {
            columns,
            rows,
            affected_rows,
        });
    }
    Observation::Success(sets)
}

fn run_program(conn: &mut Conn, program: &Program) -> Vec<Observation> {
    for sql in &program.setup {
        match observe(conn, sql) {
            Observation::Success(_) => {}
            error => panic!(
                "setup failed for seed {} with {sql:?}: {error:#?}",
                program.seed
            ),
        }
    }
    program
        .steps
        .iter()
        .chain(&program.checks)
        .map(|sql| observe(conn, sql))
        .collect()
}

fn cleanup(conn: &mut Conn, program: &Program) {
    for sql in program.cleanup() {
        conn.query_drop(&sql)
            .unwrap_or_else(|error| panic!("cleanup {sql:?}: {error}"));
    }
}

fn configured_seed_count() -> u64 {
    std::env::var("STATEFUL_DIFFERENTIAL_SEEDS")
        .map(|value| {
            value
                .parse()
                .expect("STATEFUL_DIFFERENTIAL_SEEDS must be an integer")
        })
        .unwrap_or(DEFAULT_SEED_COUNT)
}

fn configured_seeds() -> (Vec<u64>, bool) {
    if let Ok(value) = std::env::var("STATEFUL_DIFFERENTIAL_SEED") {
        let seed = value
            .parse()
            .expect("STATEFUL_DIFFERENTIAL_SEED must be an integer");
        return (vec![seed], false);
    }
    let count = configured_seed_count();
    assert!(count > 0, "STATEFUL_DIFFERENTIAL_SEEDS must be positive");
    // A short local smoke range is useful, but only the default-or-larger
    // range makes the all-variants coverage claim.
    ((0..count).collect(), count >= DEFAULT_SEED_COUNT)
}

#[test]
fn generated_stateful_programs_match_mariadb() {
    let _guard = common::test_lock();
    let (seeds, require_variant_coverage) = configured_seeds();

    let target = common::mysql_compare_target();
    let mut mysqweel = connect(&start_mysqweel());
    let mut mariadb = target.as_ref().map(|target| connect(target.url()));
    let mut covered = BTreeSet::new();
    let mut interactions = BTreeSet::new();

    for seed in seeds {
        let program = Program::generate(seed);
        let features = program.features();
        covered.extend(features);
        interactions.insert((features[0], features[1]));
        let actual = run_program(&mut mysqweel, &program);
        if let Some(reference) = mariadb.as_mut() {
            let expected = run_program(reference, &program);
            if actual != expected {
                panic!(
                    "stateful differential mismatch for seed {} ({:?})\n\
                     Reproduce with: STATEFUL_DIFFERENTIAL_SEED={} cargo test --locked \
                     --test stateful_differential generated_stateful_programs_match_mariadb -- --exact --nocapture\n\
                     SQL:\n{}\nMariaDB: {expected:#?}\nMySqweel: {actual:#?}",
                    program.seed,
                    program.features(),
                    program.seed,
                    program.sql().join(";\n")
                );
            }
            cleanup(reference, &program);
        }
        cleanup(&mut mysqweel, &program);
    }

    if require_variant_coverage {
        for feature in [
            "transaction-update",
            "savepoint-rollback",
            "child-update",
            "expression-update",
            "left-join-aggregate",
            "correlated-subquery",
            "derived-case-grouping",
            "exists-subquery",
        ] {
            assert!(
                covered.contains(feature),
                "seed range did not cover {feature}"
            );
        }
        assert_eq!(
            interactions.len(),
            16,
            "default seed range must cover every mutation/query interaction"
        );
    }
    if mariadb.is_none() {
        eprintln!(
            "Stateful programs ran against MySqweel only; set MARIADB_COMPARE_URL or enable Docker for differential verification."
        );
    }
}

#[test]
fn generator_is_reproducible_and_covers_every_variant() {
    let first = Program::generate(17);
    let second = Program::generate(17);
    assert_eq!(first.sql(), second.sql());
    assert_eq!(first.features(), second.features());

    let covered: BTreeSet<_> = (0..DEFAULT_SEED_COUNT)
        .flat_map(|seed| Program::generate(seed).features())
        .collect();
    assert_eq!(
        covered.len(),
        8,
        "default seeds must cover every grammar variant"
    );
    let interactions: BTreeSet<_> = (0..DEFAULT_SEED_COUNT)
        .map(|seed| Program::generate(seed).features())
        .map(|features| (features[0], features[1]))
        .collect();
    assert_eq!(interactions.len(), 16);
}
