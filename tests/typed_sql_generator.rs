//! Deterministic, typed SELECT generation for the supported query subset.
mod common;

use std::collections::BTreeSet;

use my_sqweel::sql::engine::{Engine, EngineConfig};
use mysql::prelude::Queryable;
use mysql::{Conn, Opts, Value};

const SEEDS: u64 = 64;

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
    fn from(v: Value) -> Self {
        match v {
            Value::NULL => Self::Null,
            Value::Bytes(v) => Self::Bytes(v),
            Value::Int(v) => Self::Int(v),
            Value::UInt(v) => Self::UInt(v),
            Value::Float(v) => Self::Float(v.to_bits()),
            Value::Double(v) => Self::Double(v.to_bits()),
            Value::Date(y, m, d, h, n, s, us) => Self::Date(y, m, d, h, n, s, us),
            Value::Time(neg, days, h, m, s, us) => Self::Time(neg, days, h, m, s, us),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ResultSet(Vec<(String, String)>, Vec<Vec<Cell>>, u64);

#[derive(Debug, Clone, PartialEq, Eq)]
enum Observation {
    Success(Vec<ResultSet>),
    Error(u16, String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Ty {
    Int,
    Text,
}

#[derive(Clone, Copy)]
struct Column {
    name: &'static str,
    ty: Ty,
}

const PARENT: &[Column] = &[
    Column {
        name: "id",
        ty: Ty::Int,
    },
    Column {
        name: "category",
        ty: Ty::Text,
    },
    Column {
        name: "score",
        ty: Ty::Int,
    },
];

#[derive(Clone)]
struct Expr {
    sql: String,
    ty: Ty,
}

impl Expr {
    fn column(alias: &str, column: Column) -> Self {
        Self {
            sql: format!("{alias}.{}", column.name),
            ty: column.ty,
        }
    }
    fn int(value: i64) -> Self {
        Self {
            sql: value.to_string(),
            ty: Ty::Int,
        }
    }
}

#[derive(Clone)]
enum Pred {
    Compare(Expr, &'static str, Expr),
    IsNull(Expr, bool),
    And(Box<Pred>, Box<Pred>),
    Or(Box<Pred>, Box<Pred>),
    Not(Box<Pred>),
}

impl Pred {
    fn sql(&self) -> String {
        match self {
            Self::Compare(a, op, b) => {
                assert_eq!(a.ty, b.ty);
                format!("{} {op} {}", a.sql, b.sql)
            }
            Self::IsNull(e, negated) => {
                format!("{} IS {}NULL", e.sql, if *negated { "NOT " } else { "" })
            }
            Self::And(a, b) => format!("({} AND {})", a.sql(), b.sql()),
            Self::Or(a, b) => format!("({} OR {})", a.sql(), b.sql()),
            Self::Not(p) => format!("NOT ({})", p.sql()),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Shape {
    Simple,
    LeftAggregate,
    Correlated,
    Exists,
    Derived,
    NullPredicate,
}

impl Shape {
    fn name(self) -> &'static str {
        match self {
            Self::Simple => "simple",
            Self::LeftAggregate => "left_join_aggregate",
            Self::Correlated => "correlated_subquery",
            Self::Exists => "exists_subquery",
            Self::Derived => "derived_aggregate",
            Self::NullPredicate => "null_predicate",
        }
    }
}

struct Query {
    sql: String,
    shape: Shape,
}

fn query(seed: u64, p: &str, c: &str, rng: &mut Rng) -> Query {
    let threshold = rng.range(-3, 12);
    let score = Expr::column("p", PARENT[2]);
    let category = Expr::column("p", PARENT[1]);
    let shape = match seed % 6 {
        0 => Shape::LeftAggregate,
        1 => Shape::Exists,
        2 => Shape::Correlated,
        3 => Shape::Derived,
        4 => Shape::NullPredicate,
        _ => Shape::Simple,
    };
    let sql = match shape {
        Shape::Simple => {
            let pred = Pred::Or(
                Box::new(Pred::IsNull(score.clone(), false)),
                Box::new(Pred::And(
                    Box::new(Pred::Compare(score.clone(), ">=", Expr::int(threshold))),
                    Box::new(Pred::Not(Box::new(Pred::Compare(
                        score.clone(),
                        "=",
                        Expr::int(threshold + 1),
                    )))),
                )),
            );
            format!(
                "SELECT p.id, p.category, p.score FROM {p} p WHERE {} ORDER BY p.id",
                pred.sql()
            )
        }
        Shape::LeftAggregate => format!(
            "SELECT p.category, COUNT(c.id), COALESCE(SUM(c.amount), 0) FROM {p} p LEFT JOIN {c} c ON c.parent_id = p.id GROUP BY p.category HAVING COUNT(c.id) >= 0 ORDER BY p.category"
        ),
        Shape::Correlated => format!(
            "SELECT p.id, (SELECT COUNT(*) FROM {c} c WHERE c.parent_id = p.id AND c.amount IS NOT NULL) FROM {p} p WHERE p.id IN (SELECT c.parent_id FROM {c} c WHERE c.amount >= {threshold}) ORDER BY p.id"
        ),
        Shape::Exists => format!(
            "SELECT p.id, p.category FROM {p} p WHERE EXISTS (SELECT 1 FROM {c} c WHERE c.parent_id = p.id AND (c.amount IS NULL OR c.amount >= {threshold})) ORDER BY p.id"
        ),
        Shape::Derived => format!(
            "SELECT d.bucket, COUNT(*), SUM(d.value) FROM (SELECT CASE WHEN score IS NULL THEN 'null' WHEN score >= {threshold} THEN 'high' ELSE 'low' END AS bucket, COALESCE(score, 0) AS value FROM {p}) d GROUP BY d.bucket ORDER BY d.bucket"
        ),
        Shape::NullPredicate => {
            let pred = Pred::Or(
                Box::new(Pred::IsNull(score, false)),
                Box::new(Pred::Compare(
                    category,
                    "=",
                    Expr {
                        sql: "'alpha'".into(),
                        ty: Ty::Text,
                    },
                )),
            );
            format!("SELECT p.id FROM {p} p WHERE {} ORDER BY p.id", pred.sql())
        }
    };
    Query { sql, shape }
}

struct Program {
    seed: u64,
    parent: String,
    child: String,
    setup: Vec<String>,
    queries: Vec<String>,
    features: BTreeSet<&'static str>,
    interaction: Option<&'static str>,
    data: Vec<&'static str>,
    target: Option<&'static str>,
    shape: Shape,
}

struct CoveragePlan {
    targets: Vec<(&'static str, u64)>,
}

impl CoveragePlan {
    fn from_spec(selected: Option<&str>) -> Self {
        let gaps: Vec<_> = include_str!("sql-coverage-spec.toml")
            .split("[[obligations]]")
            .filter_map(|block| {
                let field = |name: &str| {
                    block.lines().find_map(|line| {
                        line.trim()
                            .strip_prefix(&format!("{name} = "))
                            .map(|value| value.trim().trim_matches('"'))
                    })
                };
                let id = field("id")?;
                (field("status") == Some("supported")
                    && field("dimension") == Some("data_boundary")
                    && field("case").is_none()
                    && matches!(id, "null_join_key" | "empty_join_inputs"))
                .then_some(id)
            })
            .collect();
        let gaps = if let Some(selected) = selected {
            assert!(
                gaps.contains(&selected),
                "TYPED_SQL_GAP must name an uncovered supported data_boundary obligation"
            );
            vec![*gaps.iter().find(|id| **id == selected).unwrap()]
        } else {
            gaps
        };
        Self {
            targets: gaps
                .into_iter()
                .map(|id| (id, if id == "null_join_key" { 0 } else { 6 }))
                .collect(),
        }
    }

    fn target(&self, seed: u64) -> Option<&'static str> {
        self.targets
            .iter()
            .find_map(|(id, target_seed)| (*target_seed == seed).then_some(*id))
    }
}

impl Program {
    fn generate(seed: u64, plan: &CoveragePlan) -> Self {
        let mut rng = Rng::new(seed);
        let parent = format!("tg_p_{seed}");
        let child = format!("tg_c_{seed}");
        let q = query(seed, &parent, &child, &mut rng);
        let shape = q.shape;
        let target = plan
            .target(seed)
            .filter(|_| matches!(shape, Shape::LeftAggregate));
        let mut features = BTreeSet::from(["select", "predicates"]);
        match shape {
            Shape::LeftAggregate => {
                features.extend(["left_join", "aggregate", "group_having"]);
            }
            Shape::Correlated => {
                features.extend(["correlated_subquery", "in_subquery", "aggregate"]);
            }
            Shape::Exists => {
                features.extend(["exists", "null_predicate"]);
            }
            Shape::Derived => {
                features.extend(["derived_table", "aggregate", "null_predicate"]);
            }
            Shape::NullPredicate => {
                features.insert("null_predicate");
            }
            Shape::Simple => {
                features.insert("boolean_predicate");
            }
        }
        let setup = vec![
            format!(
                "CREATE TABLE {parent} (id INT PRIMARY KEY, category VARCHAR(16) NOT NULL, score INT NULL)"
            ),
            format!(
                "CREATE TABLE {child} (id INT PRIMARY KEY, parent_id INT NULL, amount INT NULL, label VARCHAR(16) NOT NULL)"
            ),
            format!(
                "INSERT INTO {parent} VALUES (1, 'alpha', 10), (2, 'alpha', NULL), (3, 'beta', {}), (4, 'gamma', 0)",
                rng.range(-5, 15)
            ),
            if target == Some("empty_join_inputs") {
                format!("DELETE FROM {child}")
            } else if target == Some("null_join_key") {
                format!(
                    "INSERT INTO {child} VALUES (1, 1, 5, 'open'), (2, 1, NULL, 'missing'), (3, 2, 9, 'done'), (4, 3, -3, 'late'), (5, NULL, 7, 'orphan')"
                )
            } else {
                format!(
                    "INSERT INTO {child} VALUES (1, 1, 5, 'open'), (2, 1, NULL, 'missing'), (3, 2, 9, 'done'), (4, 3, -3, 'late')"
                )
            },
        ];
        let queries = vec![
            q.sql,
            format!("SELECT id, parent_id, amount FROM {child} ORDER BY id"),
        ];
        let interaction = match shape {
            Shape::LeftAggregate => Some("left_join×aggregate"),
            Shape::Correlated => Some("correlated_subquery×aggregate"),
            Shape::Exists => Some("exists×null_predicate"),
            Shape::Derived => Some("derived_table×aggregate"),
            _ => None,
        };
        let mut data = vec!["duplicate_join_keys", "unmatched_parent", "nullable_score"];
        if let Some(target) = target {
            data.push(target);
        }
        Self {
            seed,
            parent,
            child,
            setup,
            queries,
            features,
            interaction,
            data,
            target,
            shape,
        }
    }
    fn cleanup(&self, conn: &mut Conn) {
        for table in [&self.child, &self.parent] {
            conn.query_drop(format!("DROP TABLE {table}")).unwrap();
        }
    }
}

struct Rng(u64);
impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed ^ 0x9e37_79b9_7f4a_7c15)
    }
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 7;
        self.0 ^= self.0 >> 9;
        self.0 ^= self.0 << 8;
        self.0
    }
    fn range(&mut self, lo: i64, hi: i64) -> i64 {
        lo + (self.next() % (hi - lo) as u64) as i64
    }
}

fn write_report(
    seeds: &[u64],
    features: &BTreeSet<&str>,
    interactions: &BTreeSet<&str>,
    data: &BTreeSet<&str>,
    failures: &[u64],
    differential_checked: bool,
    plan: &CoveragePlan,
) {
    let Some(path) = std::env::var_os("TYPED_SQL_COVERAGE_REPORT") else {
        return;
    };
    let cases: Vec<_> = seeds
        .iter()
        .map(|seed| {
            let program = Program::generate(*seed, plan);
            serde_json::json!({"seed": seed, "shape": program.shape.name(), "features": program.features, "target": program.target})
        })
        .collect();
    let targeted_obligations: Vec<_> = seeds
        .iter()
        .filter_map(|seed| {
            let program = Program::generate(*seed, plan);
            program
                .target
                .map(|id| serde_json::json!({"id": id, "seed": seed}))
        })
        .collect();
    let json = serde_json::to_vec_pretty(&serde_json::json!({
        "seeds": seeds,
        "features": features,
        "interactions": interactions,
        "data_conditions": data,
        "failures": failures,
        "differential_checked": differential_checked,
        "targeted_obligations": targeted_obligations,
        "cases": cases,
    }))
    .expect("serialize typed SQL coverage report");
    std::fs::write(path, json).expect("write typed SQL coverage report");
}

fn start_mysqweel() -> (String, my_sqweel::SqlEndpoint) {
    let engine = Engine::new(EngineConfig::mysql_strict());
    engine.execute_sql("CREATE DATABASE test").unwrap();
    let endpoint = engine
        .spawn_sql(my_sqweel::SqlEndpointConfig::new(
            "127.0.0.1:0".parse().unwrap(),
            my_sqweel::server::Authentication::AllowAll,
        ))
        .unwrap();
    (
        format!("mysql://root@{}/test", endpoint.local_addr()),
        endpoint,
    )
}

fn connect(url: &str) -> Conn {
    let mut conn = Conn::new(Opts::from_url(url).unwrap()).expect("connect to SQL server");
    for sql in [
        "SET SESSION sql_mode = 'STRICT_TRANS_TABLES,NO_ENGINE_SUBSTITUTION'",
        "SET SESSION time_zone = '+00:00'",
        "SET NAMES utf8mb4 COLLATE utf8mb4_general_ci",
    ] {
        conn.query_drop(sql).unwrap();
    }
    conn
}

fn observe(conn: &mut Conn, sql: &str) -> Observation {
    let mut result = match conn.query_iter(sql) {
        Ok(result) => result,
        Err(mysql::Error::MySqlError(e)) => return Observation::Error(e.code, e.state),
        Err(e) => panic!("connection error for {sql:?}: {e}"),
    };
    let mut sets = Vec::new();
    while let Some(mut set) = result.iter() {
        let cols = set
            .columns()
            .as_ref()
            .iter()
            .map(|c| (c.name_str().into_owned(), format!("{:?}", c.column_type())))
            .collect();
        let affected = set.affected_rows();
        let mut rows = Vec::new();
        for row in &mut set {
            rows.push(row.unwrap().unwrap().into_iter().map(Cell::from).collect());
        }
        sets.push(ResultSet(cols, rows, affected));
    }
    Observation::Success(sets)
}

fn run(conn: &mut Conn, program: &Program) -> Vec<Observation> {
    for sql in &program.setup {
        assert!(
            matches!(observe(conn, sql), Observation::Success(_)),
            "setup failed for seed {}: {sql}",
            program.seed
        );
    }
    program
        .queries
        .iter()
        .map(|sql| observe(conn, sql))
        .collect()
}

#[test]
fn generated_typed_selects_match_mariadb() {
    let _guard = common::test_lock();
    let target = common::mysql_compare_target();
    let (url, _endpoint) = start_mysqweel();
    let mut actual_conn = connect(&url);
    let mut reference = target.as_ref().map(|t| connect(t.url()));
    let selected_gap = std::env::var("TYPED_SQL_GAP").ok();
    let plan = CoveragePlan::from_spec(selected_gap.as_deref());
    let seeds: Vec<u64> = if let Ok(seed) = std::env::var("TYPED_SQL_SEED") {
        vec![seed.parse().expect("TYPED_SQL_SEED integer")]
    } else {
        (0..SEEDS).collect()
    };
    let mut covered = BTreeSet::new();
    let mut interactions: BTreeSet<&str> = BTreeSet::new();
    let mut data = BTreeSet::new();
    let mut failures = Vec::new();
    for &seed in &seeds {
        let program = Program::generate(seed, &plan);
        covered.extend(program.features.iter().copied());
        interactions.extend(program.interaction);
        data.extend(program.data.iter().copied());
        let actual = run(&mut actual_conn, &program);
        if let Some(reference) = reference.as_mut() {
            let expected = run(reference, &program);
            if expected != actual {
                failures.push(seed);
                write_report(
                    &seeds,
                    &covered,
                    &interactions,
                    &data,
                    &failures,
                    target.is_some(),
                    &plan,
                );
                let gap = program
                    .target
                    .map(|id| format!(" TYPED_SQL_GAP={id}"))
                    .unwrap_or_default();
                panic!(
                    "typed SQL mismatch seed={} features={:?}\nReproduce: TYPED_SQL_SEED={seed}{gap} cargo test --test typed_sql_generator generated_typed_selects_match_mariadb -- --exact --nocapture\nSQL: {}\nMariaDB: {expected:#?}\nMySqweel: {actual:#?}",
                    seed,
                    program.features,
                    program.queries.join(";\n")
                );
            }
            program.cleanup(reference);
        }
        program.cleanup(&mut actual_conn);
    }
    if std::env::var_os("TYPED_SQL_SEED").is_none() {
        assert!(
            covered.contains("left_join")
                && covered.contains("correlated_subquery")
                && covered.contains("exists")
                && covered.contains("null_predicate")
        );
    }
    write_report(
        &seeds,
        &covered,
        &interactions,
        &data,
        &failures,
        target.is_some(),
        &plan,
    );
    if reference.is_none() {
        eprintln!("MariaDB comparison skipped; set MARIADB_COMPARE_URL or enable Docker.");
    }
}

#[test]
fn generator_is_reproducible_and_targets_missing_spec_obligations() {
    let plan = CoveragePlan::from_spec(None);
    let a = Program::generate(17, &plan);
    let b = Program::generate(17, &plan);
    assert_eq!(a.queries, b.queries);
    assert!(a.setup.iter().any(|sql| sql.contains("parent_id INT NULL")));
    for (id, seed) in &plan.targets {
        let target = Program::generate(*seed, &plan);
        assert_eq!(target.target, Some(*id));
        assert_eq!(target.shape, Shape::LeftAggregate);
        assert!(target.data.contains(id));
        match *id {
            "null_join_key" => assert!(target.setup[3].contains("NULL, 7, 'orphan'")),
            "empty_join_inputs" => assert!(target.setup[3].starts_with("DELETE FROM")),
            _ => unreachable!("plan admitted an unsupported target"),
        }
    }
    let shapes: BTreeSet<_> = (0..6)
        .map(|seed| Program::generate(seed, &plan).shape)
        .collect();
    assert_eq!(
        shapes.len(),
        6,
        "seed smoke range must retain every query shape"
    );
}
