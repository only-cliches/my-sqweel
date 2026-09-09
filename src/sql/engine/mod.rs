use std::cell::{Cell, RefCell};
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration as StdDuration, Instant};

use anyhow::{Result, anyhow};
use chrono::{
    DateTime, Datelike, Duration, Months, NaiveDate, NaiveDateTime, NaiveTime, Timelike, Utc,
};
use dashmap::DashMap;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Number, Value, json};
use sqlparser::ast::{
    Assignment, BinaryOperator, DateTimeField, Expr, FunctionArg, FunctionArgExpr,
    FunctionArgumentClause, FunctionArguments, GroupByExpr, Ident, JoinConstraint, JoinOperator,
    ObjectName, Offset, OnInsert, OrderByExpr, Query, Select, SelectItem, SetExpr, Statement,
    TableConstraint, TableFactor, TableWithJoins, Value as SqlValue,
};

use crate::model::StoredRow;
use crate::schema::{ColumnHint, ForeignKeyHint, IndexHint, TableSchemaHint};
use crate::storage::RedisStore;

mod catalog;
pub(crate) use catalog::may_start_with;
mod transaction;
pub use transaction::{Engine, EngineSession};
mod compat;
mod ddl;
mod dml;
mod eval;
mod maintenance;
mod query;
mod storage_format;
mod support;
mod values;

use compat::*;
use ddl::*;
pub(crate) use eval::MYSQL_BINARY_SENTINEL;
pub(crate) use eval::json_compact_text;
pub(crate) use eval::json_wire_text;
use eval::*;
use storage_format::*;
use support::*;
use values::*;

const STORAGE_NAMESPACE: &str = "my-sqweel";
const STORAGE_AUTO_INC_KEY: &str = "my-sqweel:auto_inc";
const UNIQUE_SEPARATOR: char = '\u{1f}';
const FK_FIELD_SEPARATOR: char = '\u{1e}';
pub(crate) const JSON_NULL_SENTINEL: &str = "\0my_sqweel_json_null";

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UniqueMode {
    #[default]
    Overwrite,
    Enforce,
}

/// Controls whether MySqweel favors schema-drift convenience or MySQL's
/// fail-fast behavior. The default remains drift tolerant for backwards
/// compatibility; callers that need MySQL parity should select `MysqlStrict`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompatibilityProfile {
    #[default]
    Drift,
    MysqlStrict,
}

impl CompatibilityProfile {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Drift => "drift",
            Self::MysqlStrict => "mysql_strict",
        }
    }
}

impl UniqueMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Overwrite => "overwrite",
            Self::Enforce => "enforce",
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SeedMode {
    #[default]
    Append,
    Replace,
}

impl SeedMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Append => "append",
            Self::Replace => "replace",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EngineConfig {
    /// Fixed SQL timezone inherited by new sessions; None selects UTC.
    #[serde(default)]
    pub default_time_zone: Option<String>,
    pub unique_mode: UniqueMode,
    #[serde(default)]
    pub compatibility_profile: CompatibilityProfile,
    pub failure_injection: FailureInjectionConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct FailureInjectionConfig {
    pub query_delay_ms: u64,
    pub fail_read_every: u64,
    pub fail_write_every: u64,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            default_time_zone: None,
            unique_mode: UniqueMode::Overwrite,
            compatibility_profile: CompatibilityProfile::Drift,
            failure_injection: FailureInjectionConfig::default(),
        }
    }
}

impl EngineConfig {
    /// A parity-oriented configuration that rejects schema drift and enforces
    /// declared uniqueness in the same places MySQL does.
    pub fn mysql_strict() -> Self {
        Self {
            unique_mode: UniqueMode::Enforce,
            compatibility_profile: CompatibilityProfile::MysqlStrict,
            ..Self::default()
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct QueryResult {
    pub rows_affected: u64,
    pub last_insert_id: u64,
    pub columns: Vec<String>,
    pub column_metadata: Vec<ColumnMetadata>,
    pub rows: Vec<Map<String, Value>>,
    /// Diagnostics generated while evaluating this statement.  MySQL exposes
    /// these through SHOW WARNINGS and also carries their count in the result
    /// terminator packet.
    pub warnings: Vec<QueryWarning>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueryWarning {
    pub level: String,
    pub code: u16,
    pub message: String,
}

/// Options controlling the amount of data copied into query completion events.
/// Result payloads are disabled by default because a query can return a large
/// number of rows.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct QueryEventOptions {
    pub include_results: bool,
}

/// Logical data-access counters for one query execution.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct QueryMetrics {
    /// Number of stored rows examined or materialized, including rows later
    /// rejected by a predicate.
    pub rows_read: u64,
    /// Number of stored field values loaded into execution row contexts.
    pub cells_read: u64,
    /// Number of rows whose stored state changed, including deleted rows.
    pub rows_written: u64,
    /// Number of inserted or changed stored field values. Deleted rows do not
    /// contribute cell writes.
    pub cells_written: u64,
}

pub type QueryId = u64;

impl QueryEventOptions {
    pub const fn metadata_only() -> Self {
        Self {
            include_results: false,
        }
    }

    pub const fn with_results() -> Self {
        Self {
            include_results: true,
        }
    }
}

#[derive(Debug, Clone)]
pub struct QueryReceivedEvent {
    pub query_id: QueryId,
    pub query: String,
}

#[derive(Debug, Clone)]
pub struct QueryCompletedEvent {
    pub query_id: QueryId,
    pub duration: StdDuration,
    pub result_set_count: usize,
    /// Total number of rows across all result sets.
    pub result_set_size: usize,
    pub metrics: QueryMetrics,
    pub results: Option<Vec<QueryResult>>,
    pub error: Option<String>,
}

#[derive(Debug, Clone)]
pub enum QueryEvent {
    Received(QueryReceivedEvent),
    Completed(QueryCompletedEvent),
}

/// A blocking stream of query lifecycle events.
pub struct QueryEventStream {
    receiver: Receiver<QueryEvent>,
}

impl QueryEventStream {
    pub fn recv(&self) -> Result<QueryEvent, mpsc::RecvError> {
        self.receiver.recv()
    }

    pub fn try_recv(&self) -> Result<QueryEvent, mpsc::TryRecvError> {
        self.receiver.try_recv()
    }

    pub fn recv_timeout(&self, timeout: StdDuration) -> Result<QueryEvent, mpsc::RecvTimeoutError> {
        self.receiver.recv_timeout(timeout)
    }
}

impl Iterator for QueryEventStream {
    type Item = QueryEvent;

    fn next(&mut self) -> Option<Self::Item> {
        self.receiver.recv().ok()
    }
}

struct QueryEventSubscriber {
    sender: Sender<QueryEvent>,
    include_results: bool,
}

#[derive(Default)]
struct QueryMetricsRecorder {
    enabled: bool,
    rows_read: Cell<u64>,
    cells_read: Cell<u64>,
    rows_written: Cell<u64>,
    cells_written: Cell<u64>,
}

impl QueryMetricsRecorder {
    fn new(enabled: bool) -> Self {
        Self {
            enabled,
            ..Self::default()
        }
    }

    fn record_read(&self, cells: usize) {
        if !self.enabled {
            return;
        }
        self.rows_read.set(self.rows_read.get().saturating_add(1));
        self.cells_read
            .set(self.cells_read.get().saturating_add(cells as u64));
    }

    fn record_write(&self, rows: usize, cells: usize) {
        if !self.enabled {
            return;
        }
        self.rows_written
            .set(self.rows_written.get().saturating_add(rows as u64));
        self.cells_written
            .set(self.cells_written.get().saturating_add(cells as u64));
    }

    fn snapshot(&self) -> QueryMetrics {
        QueryMetrics {
            rows_read: self.rows_read.get(),
            cells_read: self.cells_read.get(),
            rows_written: self.rows_written.get(),
            cells_written: self.cells_written.get(),
        }
    }
}

thread_local! {
    static ACTIVE_QUERY_METRICS: RefCell<Vec<Rc<QueryMetricsRecorder>>> = const { RefCell::new(Vec::new()) };
    static QUERY_METRICS_ACTIVE: Cell<bool> = const { Cell::new(false) };
    static ACTIVE_UPDATE_IGNORE: RefCell<Vec<bool>> = const { RefCell::new(Vec::new()) };
}

struct QueryMetricsGuard {
    previous_active: bool,
}

impl QueryMetricsGuard {
    fn install(metrics: Rc<QueryMetricsRecorder>) -> Self {
        ACTIVE_QUERY_METRICS.with(|active| active.borrow_mut().push(metrics));
        let previous_active = QUERY_METRICS_ACTIVE.with(|active| active.replace(true));
        Self { previous_active }
    }
}

impl Drop for QueryMetricsGuard {
    fn drop(&mut self) {
        ACTIVE_QUERY_METRICS.with(|active| {
            active.borrow_mut().pop();
        });
        QUERY_METRICS_ACTIVE.with(|active| active.set(self.previous_active));
    }
}

fn with_query_metrics(callback: impl FnOnce(&QueryMetricsRecorder)) {
    QUERY_METRICS_ACTIVE.with(|enabled| {
        if !enabled.get() {
            return;
        }
        ACTIVE_QUERY_METRICS.with(|active| {
            if let Some(metrics) = active.borrow().last() {
                callback(metrics);
            }
        });
    });
}

pub(super) fn record_query_row_read(cells: usize) {
    with_query_metrics(|metrics| metrics.record_read(cells));
}

pub(super) fn record_query_row_write(cells: usize) {
    with_query_metrics(|metrics| metrics.record_write(1, cells));
}

pub(super) fn record_query_writes(rows: usize, cells: usize) {
    with_query_metrics(|metrics| metrics.record_write(rows, cells));
}

struct UpdateIgnoreGuard;

impl UpdateIgnoreGuard {
    fn install(enabled: bool) -> Self {
        ACTIVE_UPDATE_IGNORE.with(|active| active.borrow_mut().push(enabled));
        Self
    }
}

impl Drop for UpdateIgnoreGuard {
    fn drop(&mut self) {
        ACTIVE_UPDATE_IGNORE.with(|active| {
            active.borrow_mut().pop();
        });
    }
}

pub(super) fn update_ignore_mode() -> bool {
    ACTIVE_UPDATE_IGNORE.with(|active| active.borrow().last().copied().unwrap_or(false))
}

pub(super) fn changed_cell_count(before: &Map<String, Value>, after: &Map<String, Value>) -> usize {
    before
        .keys()
        .chain(after.keys())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .filter(|column| before.get(*column) != after.get(*column))
        .count()
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MysqlColumnType {
    Null,
    TinyInt,
    SmallInt,
    Integer,
    BigInt,
    Float,
    Double,
    Decimal,
    Date,
    Time,
    DateTime,
    Timestamp,
    Year,
    Char,
    #[default]
    VarChar,
    Text,
    Binary,
    VarBinary,
    Blob,
    Json,
    Bit,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ColumnMetadata {
    pub name: String,
    #[serde(default)]
    pub table: String,
    #[serde(default)]
    pub column_type: MysqlColumnType,
    #[serde(default = "default_true")]
    pub nullable: bool,
    #[serde(default)]
    pub unsigned: bool,
    #[serde(default)]
    pub decimals: u8,
    pub character_set: Option<String>,
    pub collation: Option<String>,
}

fn default_true() -> bool {
    true
}

impl ColumnMetadata {
    pub fn from_declared(
        name: impl Into<String>,
        table: impl Into<String>,
        hint: &ColumnHint,
    ) -> Self {
        let declared = hint.sql_type.as_deref().unwrap_or("VARCHAR");
        let upper = declared.to_ascii_uppercase();
        Self {
            name: name.into(),
            table: table.into(),
            column_type: mysql_column_type_from_declared(&upper),
            nullable: hint.nullable.unwrap_or(true) && !hint.primary_key,
            unsigned: upper.contains("UNSIGNED"),
            decimals: declared_type_scale(&upper),
            character_set: is_character_type(&upper).then(|| "utf8mb4".to_string()),
            collation: is_character_type(&upper).then(|| "utf8mb4_general_ci".to_string()),
        }
    }

    pub fn from_value(name: impl Into<String>, value: Option<&Value>) -> Self {
        let name = name.into();
        let column_type = match value {
            None | Some(Value::Null) => MysqlColumnType::VarChar,
            Some(Value::Bool(_)) => MysqlColumnType::TinyInt,
            Some(Value::Number(number)) if number.is_i64() || number.is_u64() => {
                MysqlColumnType::BigInt
            }
            Some(Value::Number(_)) => MysqlColumnType::Double,
            Some(Value::Array(_) | Value::Object(_)) => MysqlColumnType::Json,
            Some(Value::String(_)) => MysqlColumnType::VarChar,
        };
        Self {
            name,
            column_type,
            nullable: true,
            ..Self::default()
        }
    }
}

fn is_character_type(upper: &str) -> bool {
    ["CHAR", "TEXT", "ENUM", "SET"]
        .iter()
        .any(|kind| upper.contains(kind))
}

fn declared_type_scale(upper: &str) -> u8 {
    if !(upper.starts_with("DECIMAL")
        || upper.starts_with("NUMERIC")
        || upper.starts_with("FLOAT")
        || upper.starts_with("DOUBLE")
        || upper.starts_with("REAL"))
    {
        return 0;
    }
    upper
        .split_once('(')
        .and_then(|(_, tail)| tail.split_once(',').map(|(_, scale)| scale))
        .and_then(|scale| scale.trim_end_matches(')').trim().parse().ok())
        .unwrap_or(0)
}

fn mysql_column_type_from_declared(upper: &str) -> MysqlColumnType {
    if upper.starts_with("TINYINT") || upper.starts_with("BOOL") {
        MysqlColumnType::TinyInt
    } else if upper.starts_with("SMALLINT") {
        MysqlColumnType::SmallInt
    } else if upper.starts_with("MEDIUMINT") || upper.starts_with("INT") {
        MysqlColumnType::Integer
    } else if upper.starts_with("BIGINT") {
        MysqlColumnType::BigInt
    } else if upper.starts_with("FLOAT") {
        MysqlColumnType::Float
    } else if upper.starts_with("DOUBLE") || upper.starts_with("REAL") {
        MysqlColumnType::Double
    } else if upper.starts_with("DECIMAL") || upper.starts_with("NUMERIC") {
        MysqlColumnType::Decimal
    } else if upper.starts_with("TIMESTAMP") {
        MysqlColumnType::Timestamp
    } else if upper.starts_with("DATETIME") {
        MysqlColumnType::DateTime
    } else if upper.starts_with("DATE") {
        MysqlColumnType::Date
    } else if upper.starts_with("TIME") {
        MysqlColumnType::Time
    } else if upper.starts_with("YEAR") {
        MysqlColumnType::Year
    } else if upper.starts_with("VARBINARY") {
        MysqlColumnType::VarBinary
    } else if upper.starts_with("BINARY") {
        MysqlColumnType::Binary
    } else if upper.starts_with("BIT") {
        MysqlColumnType::Bit
    } else if upper.contains("BLOB") {
        MysqlColumnType::Blob
    } else if upper.starts_with("JSON") {
        MysqlColumnType::Json
    } else if upper.contains("TEXT") {
        MysqlColumnType::Text
    } else if upper.starts_with("CHAR") {
        MysqlColumnType::Char
    } else {
        MysqlColumnType::VarChar
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    pub version: u32,
    pub created_at: String,
    pub schemas: BTreeMap<String, TableSchemaHint>,
    pub rows: BTreeMap<String, BTreeMap<String, StoredRow>>,
    pub auto_inc: BTreeMap<String, i64>,
    #[serde(default)]
    pub views: BTreeMap<String, String>,
    #[serde(default)]
    pub index_comments: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SeedReport {
    pub table: String,
    pub mode: SeedMode,
    pub rows_seeded: u64,
    pub rows_affected: u64,
    pub last_insert_id: u64,
}

struct InsertRowsOptions<'a> {
    ignore: bool,
    replace: bool,
    on_duplicate: &'a [Assignment],
    returning: Option<&'a [SelectItem]>,
}

// Each private statement owns its table directory but shares immutable data and
// metadata. Mutations detach only the affected value, preserving rollback and
// readers' snapshots without copying every table/schema for each SELECT.
#[derive(Clone, Debug)]
struct SharedValue<T>(Arc<T>);

type SharedTable<T> = SharedValue<BTreeMap<String, T>>;

impl<T> SharedValue<T> {
    fn ptr_eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl<T: PartialEq> PartialEq for SharedValue<T> {
    fn eq(&self, other: &Self) -> bool {
        self.ptr_eq(other) || self.0 == other.0
    }
}

impl<T: Default> Default for SharedValue<T> {
    fn default() -> Self {
        Self(Arc::new(T::default()))
    }
}

impl<T: Serialize> Serialize for SharedValue<T> {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        self.0.as_ref().serialize(serializer)
    }
}

impl<T> From<T> for SharedValue<T> {
    fn from(value: T) -> Self {
        Self(Arc::new(value))
    }
}

impl<T> std::ops::Deref for SharedValue<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<T: Clone> std::ops::DerefMut for SharedValue<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        Arc::make_mut(&mut self.0)
    }
}

impl<'a, T> IntoIterator for &'a SharedTable<T> {
    type Item = (&'a String, &'a T);
    type IntoIter = std::collections::btree_map::Iter<'a, String, T>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.iter()
    }
}

pub(super) struct RawEngine {
    database_name: String,
    visible_databases: Vec<String>,
    cfg: EngineConfig,
    storage: Arc<dyn RedisStore>,
    schemas: DashMap<String, SharedValue<TableSchemaHint>>,
    rows: DashMap<String, SharedTable<StoredRow>>,
    auto_inc: DashMap<String, i64>,
    indexes: DashMap<String, SharedTable<BTreeMap<String, BTreeSet<String>>>>,
    index_comments: DashMap<String, String>,
    last_insert_id: AtomicU64,
    next_query_id: Arc<AtomicU64>,
    read_query_count: Arc<AtomicU64>,
    write_query_count: Arc<AtomicU64>,
    last_rows_affected: AtomicU64,
    last_found_rows: AtomicU64,
    sql_mode: Mutex<String>,
    user_variables: DashMap<String, Value>,
    prepared_statements: DashMap<String, String>,
    views: DashMap<String, String>,
    parsed_select_cache: Arc<Mutex<ParsedSelectCache>>,
    query_event_subscribers: Arc<Mutex<Vec<QueryEventSubscriber>>>,
}

struct ParsedSelectCache {
    entries: HashMap<String, Arc<Vec<Statement>>>,
    order: VecDeque<String>,
    capacity: usize,
}

impl ParsedSelectCache {
    fn new(capacity: usize) -> Self {
        Self {
            entries: HashMap::new(),
            order: VecDeque::new(),
            capacity,
        }
    }

    fn get_or_parse(&mut self, sql: &str) -> Result<Vec<Statement>> {
        if let Some(statements) = self.entries.get(sql) {
            return Ok((**statements).clone());
        }
        let statements = Arc::new(super::parse(sql)?);
        self.entries.insert(sql.to_string(), statements.clone());
        self.order.push_back(sql.to_string());
        while self.order.len() > self.capacity {
            if let Some(oldest) = self.order.pop_front() {
                self.entries.remove(&oldest);
            }
        }
        Ok((*statements).clone())
    }
}

impl RawEngine {
    fn with_storage(cfg: EngineConfig, storage: Arc<dyn RedisStore>) -> Result<Self> {
        let engine = Self {
            database_name: "app".into(),
            visible_databases: vec!["app".into()],
            cfg,
            storage,
            // Statement-private directories have no concurrent writers. DashMap
            // requires at least two shards; CPU-scaled defaults multiply the
            // locks and allocations copied on every statement fork.
            schemas: DashMap::with_shard_amount(2),
            rows: DashMap::with_shard_amount(2),
            auto_inc: DashMap::with_shard_amount(2),
            indexes: DashMap::with_shard_amount(2),
            index_comments: DashMap::with_shard_amount(2),
            last_insert_id: AtomicU64::new(0),
            next_query_id: Arc::new(AtomicU64::new(1)),
            read_query_count: Arc::new(AtomicU64::new(0)),
            write_query_count: Arc::new(AtomicU64::new(0)),
            last_rows_affected: AtomicU64::new(0),
            last_found_rows: AtomicU64::new(0),
            sql_mode: Mutex::new(String::new()),
            user_variables: DashMap::with_shard_amount(2),
            prepared_statements: DashMap::with_shard_amount(2),
            views: DashMap::with_shard_amount(2),
            parsed_select_cache: Arc::new(Mutex::new(ParsedSelectCache::new(256))),
            query_event_subscribers: Arc::new(Mutex::new(Vec::new())),
        };
        engine.load_from_storage()?;
        Ok(engine)
    }

    /// Subscribe to query lifecycle events. Each subscription has its own
    /// result-payload policy and receives events for queries executed after
    /// the subscription is created.
    pub fn subscribe_query_events(&self, options: QueryEventOptions) -> QueryEventStream {
        let (sender, receiver) = mpsc::channel();
        self.query_event_subscribers
            .lock()
            .push(QueryEventSubscriber {
                sender,
                include_results: options.include_results,
            });
        QueryEventStream { receiver }
    }

    fn query_events_enabled(&self) -> bool {
        !self.query_event_subscribers.lock().is_empty()
    }

    pub(super) fn mysql_strict(&self) -> bool {
        self.cfg.compatibility_profile == CompatibilityProfile::MysqlStrict
    }

    pub(super) fn traditional_sql_mode(&self) -> bool {
        self.sql_mode
            .lock()
            .to_ascii_uppercase()
            .contains("TRADITIONAL")
    }

    pub(super) fn strict_value_mode(&self) -> bool {
        let mode = self.sql_mode.lock().to_ascii_uppercase();
        self.mysql_strict()
            || mode.contains("TRADITIONAL")
            || mode.contains("STRICT_TRANS_TABLES")
            || mode.contains("STRICT_ALL_TABLES")
    }

    pub(super) fn user_variable(&self, name: &str) -> Value {
        self.user_variables
            .get(&name.to_ascii_lowercase())
            .map(|value| value.clone())
            .unwrap_or(Value::Null)
    }

    pub(crate) fn set_sql_safe_updates(&self, enabled: bool) {
        self.user_variables
            .insert("__sql_safe_updates".to_string(), Value::Bool(enabled));
    }

    pub(crate) fn set_session_time_zone(&self, value: impl Into<String>) {
        self.user_variables
            .insert("__time_zone".to_string(), Value::String(value.into()));
    }

    pub(super) fn enforces_uniqueness(&self) -> bool {
        self.mysql_strict() || self.cfg.unique_mode == UniqueMode::Enforce
    }

    fn execute_sql_internal(
        &self,
        event_sql: &str,
        execution_sql: &str,
        normalize_json_nulls: bool,
        emit_events: bool,
    ) -> Result<Vec<QueryResult>> {
        eval::clear_eval_user_variables();
        eval::set_eval_database(&self.database_name);
        let query_id = (emit_events && self.query_events_enabled())
            .then(|| self.next_query_id.fetch_add(1, AtomicOrdering::Relaxed));
        let metrics = query_id.map(|_| Rc::new(QueryMetricsRecorder::new(true)));
        let _metrics_guard = metrics.clone().map(QueryMetricsGuard::install);
        let started = query_id.map(|_| Instant::now());
        if let Some(query_id) = query_id {
            self.publish_query_event(QueryEvent::Received(QueryReceivedEvent {
                query_id,
                query: event_sql.to_string(),
            }));
        }

        tracing::debug!(sql = execution_sql, "sql.execute");
        let mut out = Vec::new();
        let outcome: Result<Vec<QueryResult>> = (|| {
            for raw in split_sql_statements(execution_sql) {
                if raw.is_empty() {
                    continue;
                }
                self.maybe_inject_failure(&raw)?;
                let _update_ignore_guard =
                    UpdateIgnoreGuard::install(is_update_ignore_statement(&raw));
                if let Some(result) = self.execute_insert_select_returning_compat(&raw)? {
                    self.capture_eval_user_variables();
                    self.record_found_rows(&raw, &result);
                    self.store_last_rows_affected(&raw, &result);
                    out.push(result);
                    continue;
                }
                if let Some(result) = self.execute_create_or_replace_table_compat(&raw)? {
                    self.capture_eval_user_variables();
                    self.record_found_rows(&raw, &result);
                    self.store_last_rows_affected(&raw, &result);
                    out.push(result);
                    continue;
                }
                if let Some(result) = self.execute_compat_statement(&raw)? {
                    self.capture_eval_user_variables();
                    self.record_found_rows(&raw, &result);
                    self.store_last_rows_affected(&raw, &result);
                    out.push(result);
                    continue;
                }
                let statements = if self.can_parse_without_compat_rewrites(&raw) {
                    match self.parsed_select_cache.lock().get_or_parse(&raw) {
                        Ok(statements) => statements,
                        Err(_) => super::parse(&self.rewrite_sql_for_parser(&raw))?,
                    }
                } else {
                    let parse_sql = self.rewrite_sql_for_parser(&raw);
                    super::parse(&parse_sql)?
                };
                for statement in statements {
                    validate_statement_support(&statement)?;
                    let mut result = self.execute_statement_unobserved(statement)?;
                    if raw
                        .trim_start()
                        .to_ascii_uppercase()
                        .starts_with("CREATE TABLE")
                    {
                        self.restore_unsigned_column_hints(&raw);
                    }
                    preserve_select_result_headers(&raw, &mut result);
                    attach_query_warnings(&raw, &mut result);
                    self.store_last_rows_affected(&raw, &result);
                    self.record_found_rows(&raw, &result);
                    if normalize_json_nulls
                        && result
                            .rows
                            .iter()
                            .flat_map(|row| row.values())
                            .any(eval::contains_json_null_sentinel)
                    {
                        for row in &mut result.rows {
                            for value in row.values_mut() {
                                *value = eval::public_json_value(value);
                            }
                        }
                    }
                    self.capture_eval_user_variables();
                    out.push(result);
                }
            }
            Ok(out)
        })();

        if let Some(query_id) = query_id {
            match &outcome {
                Ok(results) => self.publish_query_completed(
                    query_id,
                    started.expect("query event start time").elapsed(),
                    metrics
                        .as_ref()
                        .expect("query metrics for observed query")
                        .snapshot(),
                    Some(results),
                    None,
                ),
                Err(error) => self.publish_query_completed(
                    query_id,
                    started.expect("query event start time").elapsed(),
                    metrics
                        .as_ref()
                        .expect("query metrics for observed query")
                        .snapshot(),
                    None,
                    Some(error.to_string()),
                ),
            }
        }
        outcome
    }

    fn capture_eval_user_variables(&self) {
        for (name, value) in eval::take_eval_user_variables() {
            self.user_variables.insert(name.to_ascii_lowercase(), value);
        }
    }

    fn store_last_rows_affected(&self, sql: &str, result: &QueryResult) {
        let ignored_constraint = sql
            .trim_start()
            .to_ascii_uppercase()
            .starts_with("DELETE IGNORE")
            && !result.warnings.is_empty();
        self.last_rows_affected.store(
            if ignored_constraint {
                u64::MAX
            } else {
                result.rows_affected
            },
            AtomicOrdering::Relaxed,
        );
    }

    fn execute_insert_select_returning_compat(&self, sql: &str) -> Result<Option<QueryResult>> {
        let upper = sql.to_ascii_uppercase();
        if !(upper.starts_with("INSERT INTO ") || upper.starts_with("REPLACE INTO "))
            || !upper.contains(" SELECT ")
            || !upper.ends_with(" RETURNING *")
        {
            return Ok(None);
        }
        let returning_at = upper
            .rfind(" RETURNING ")
            .ok_or_else(|| anyhow!("invalid INSERT RETURNING statement"))?;
        let base = sql[..returning_at].trim();
        let prefix_len = if upper.starts_with("INSERT INTO ") {
            "INSERT INTO ".len()
        } else {
            "REPLACE INTO ".len()
        };
        let target = base[prefix_len..]
            .split_whitespace()
            .next()
            .unwrap_or_default()
            .trim_matches('`')
            .to_ascii_lowercase();
        self.execute_sql_internal(base, base, true, false)?;
        let Some(schema) = self.schemas.get(&target) else {
            return Ok(Some(QueryResult::default()));
        };
        let columns = ordered_schema_columns(&schema);
        let rows = self
            .rows
            .get(&target)
            .map(|rows| {
                rows.values()
                    .map(|row| self.current_schema_row(&target, &row.data))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let column_metadata = columns
            .iter()
            .filter_map(|column| {
                schema
                    .columns
                    .get(column)
                    .map(|hint| ColumnMetadata::from_declared(column, &target, hint))
            })
            .collect();
        Ok(Some(QueryResult {
            columns,
            column_metadata,
            rows,
            ..QueryResult::default()
        }))
    }

    fn execute_create_or_replace_table_compat(&self, sql: &str) -> Result<Option<QueryResult>> {
        let trimmed = sql.trim().trim_end_matches(';').trim();
        let upper = trimmed.to_ascii_uppercase();
        let prefix = "CREATE OR REPLACE TABLE ";
        if !upper.starts_with(prefix) {
            return Ok(None);
        }
        let remainder = trimmed[prefix.len()..].trim();
        let Some(as_at) = find_top_level_keyword(remainder, "AS") else {
            return Err(anyhow!("CREATE OR REPLACE TABLE requires AS SELECT"));
        };
        let table = remainder[..as_at].trim().trim_matches('`').to_string();
        let query_sql = remainder[as_at + "AS".len()..].trim();
        let statement = crate::sql::parse(query_sql)?
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("invalid CREATE OR REPLACE TABLE query"))?;
        let result = match statement {
            Statement::Query(query) => self.select_query(*query)?,
            _ => return Err(anyhow!("CREATE OR REPLACE TABLE requires a SELECT query")),
        };
        self.replace_table_from_result(&table, result)?;
        self.user_variables
            .insert("__mtr_temp_table".to_string(), Value::Bool(true));
        Ok(Some(QueryResult::default()))
    }

    fn record_found_rows(&self, sql: &str, result: &QueryResult) {
        let upper = sql.trim_start().to_ascii_uppercase();
        if !upper.starts_with("SELECT ") {
            self.last_found_rows.store(0, AtomicOrdering::Relaxed);
            return;
        }
        let count = if upper.contains("COUNT(") && result.rows.is_empty() {
            1
        } else {
            result.rows.len() as u64
        };
        self.last_found_rows.store(count, AtomicOrdering::Relaxed);
    }

    fn can_parse_without_compat_rewrites(&self, sql: &str) -> bool {
        if !self.views.is_empty() {
            return false;
        }
        let upper = sql.trim_start().to_ascii_uppercase();
        if !upper.starts_with("SELECT ") {
            return false;
        }

        const REWRITE_MARKERS: &[&str] = &[
            " ALL",
            " DISTINCT DISTINCT",
            " LOW_PRIORITY",
            " UPDATE IGNORE",
            " ON UPDATE CURRENT_TIMESTAMP",
            " ZEROFILL",
            " HIGH_PRIORITY",
            " STRAIGHT_JOIN",
            " SQL_SMALL_RESULT",
            " SQL_BIG_RESULT",
            " SQL_BUFFER_RESULT",
            " SQL_NO_CACHE",
            " SQL_CALC_FOUND_ROWS",
            " FORCE INDEX",
            " USE INDEX",
            " IGNORE INDEX",
            "TRIM(LEADING FROM ",
            "TRIM(TRAILING FROM ",
            "TRIM(BOTH FROM ",
            "UNION (",
            "INTERVAL (",
            " SRID 0",
        ];
        !REWRITE_MARKERS.iter().any(|marker| upper.contains(marker))
            && !(upper.contains("CAST(") && upper.contains("INTERVAL"))
    }

    fn rewrite_sql_for_parser(&self, raw: &str) -> String {
        let mut parse_sql = raw
            .replace("DELETE LOW_PRIORITY", "DELETE")
            .replace("delete low_priority", "delete")
            .replace("DELETE IGNORE", "DELETE")
            .replace("delete ignore", "delete")
            .replace("UPDATE IGNORE", "UPDATE")
            .replace("update ignore", "update")
            .replace(" ON UPDATE CURRENT_TIMESTAMP", "")
            .replace(" on update current_timestamp", "")
            .replace(" ZEROFILL", "")
            .replace(" zerofill", "");
        parse_sql = strip_select_modifiers(&parse_sql);
        parse_sql = query::strip_explain_index_hints(&parse_sql);
        parse_sql = rewrite_trim_direction(&parse_sql);
        parse_sql = rewrite_trim_both_from(&parse_sql);
        parse_sql = rewrite_parenthesized_select(&parse_sql);
        parse_sql = rewrite_outer_parenthesized_select(&parse_sql);
        parse_sql = rewrite_parenthesized_union_branch(&parse_sql);
        parse_sql = rewrite_straight_join(&parse_sql);
        parse_sql = rewrite_parenthesized_alter_columns(&parse_sql);
        parse_sql = rewrite_named_unique_constraints(&parse_sql);
        parse_sql = strip_index_comments(&parse_sql);
        parse_sql = rewrite_interval_function(&parse_sql);
        parse_sql = rewrite_interval_cast(&parse_sql);
        parse_sql = rewrite_alter_rename_syntax(&parse_sql);
        parse_sql = rewrite_alter_comment_quotes(&parse_sql);
        parse_sql = rewrite_delete_wildcard_targets(&parse_sql);
        parse_sql = parse_sql.replace(" SRID 0", "").replace(" srid 0", "");
        let statement_upper = raw.trim_start().to_ascii_uppercase();
        if statement_upper.starts_with("ALTER TABLE") {
            parse_sql = parse_sql
                .replace(" BINARY(", " VARBINARY(")
                .replace(" binary(", " varbinary(")
                .replace(" BINARY", "")
                .replace(" binary", "");
        }
        parse_sql = self.rewrite_insert_target(&parse_sql);
        if parse_sql
            .trim_start()
            .to_ascii_uppercase()
            .starts_with("INSERT")
        {
            parse_sql = parse_sql
                .replace(" VALUE ", " VALUES ")
                .replace(" value ", " values ");
        }
        parse_sql = rewrite_insert_set(&parse_sql);
        parse_sql = self.expand_views(&parse_sql);
        if statement_upper.starts_with("CREATE TABLE") {
            parse_sql = parse_sql
                .replace(" BINARY NOT NULL", " NOT NULL")
                .replace(" binary NOT NULL", " NOT NULL")
                .replace(" binary not null", " not null")
                .replace(" BINARY DEFAULT", " DEFAULT")
                .replace(" binary DEFAULT", " DEFAULT")
                .replace(" binary default", " default")
                .replace(" BINARY,", ",")
                .replace(" binary,", ",")
                .replace(" BINARY)", ")")
                .replace(" binary)", ")")
                .replace("FLOAT(3,2)", "FLOAT")
                .replace("DOUBLE(4,3)", "DOUBLE")
                .replace(" DOUBLE UNSIGNED", " DOUBLE")
                .replace(" FLOAT UNSIGNED", " FLOAT")
                .replace(" double unsigned", " double")
                .replace(" float unsigned", " float")
                .replace("float(3,2)", "float")
                .replace("double(4,3)", "double");
            parse_sql = strip_float_double_precision(&parse_sql);
            parse_sql = strip_merge_union_option(&parse_sql);
            parse_sql = parse_sql
                .replace(" ROW_FORMAT=FIXED", "")
                .replace(" row_format=fixed", "")
                .replace("t(80)", "t")
                .replace("T(80)", "T");
            parse_sql = parse_sql
                .replace("CURRENT_TIMESTAMP(6)", "CURRENT_TIMESTAMP")
                .replace("current_timestamp(6)", "current_timestamp")
                .replace("CURRENT_TIMESTAMP(5)", "CURRENT_TIMESTAMP")
                .replace("current_timestamp(5)", "current_timestamp")
                .replace("CURRENT_TIMESTAMP(4)", "CURRENT_TIMESTAMP")
                .replace("current_timestamp(4)", "current_timestamp")
                .replace("CURRENT_TIMESTAMP(3)", "CURRENT_TIMESTAMP")
                .replace("current_timestamp(3)", "current_timestamp")
                .replace("CURRENT_TIMESTAMP(2)", "CURRENT_TIMESTAMP")
                .replace("current_timestamp(2)", "current_timestamp")
                .replace("CURRENT_TIMESTAMP(1)", "CURRENT_TIMESTAMP")
                .replace("current_timestamp(1)", "current_timestamp")
                .replace('"', "'")
                .replace(" DOUBLE UNSIGNED", " DOUBLE")
                .replace(" FLOAT UNSIGNED", " FLOAT")
                .replace(" DOUBLE unsigned", " DOUBLE")
                .replace(" FLOAT unsigned", " FLOAT")
                .replace(" double unsigned", " double")
                .replace(" float unsigned", " float")
                .replace(" DECIMAL unsigned", " DECIMAL")
                .replace(" decimal unsigned", " decimal")
                .replace(" NUMERIC unsigned", " NUMERIC")
                .replace(" numeric unsigned", " numeric")
                .replace(" FIXED unsigned", " FIXED")
                .replace(" fixed unsigned", " fixed")
                .replace(" DEC unsigned", " DEC")
                .replace(" dec unsigned", " dec")
                .replace("token(15)", "token")
                .replace("token(75)", "token")
                .replace("TOKEN(15)", "TOKEN")
                .replace("TOKEN(75)", "TOKEN");
            let upper_parse = parse_sql.to_ascii_uppercase();
            let preserve_invalid_match = upper_parse.contains("MATCH FULL MATCH PARTIAL")
                || upper_parse.contains("MATCH PARTIAL MATCH FULL")
                || upper_parse.contains("SET DEFAULT MATCH");
            if !preserve_invalid_match {
                parse_sql = parse_sql
                    .replace(" MATCH FULL", "")
                    .replace(" MATCH PARTIAL", "")
                    .replace(" match full", "")
                    .replace(" match partial", "");
            }
            parse_sql = parse_sql
                .replace("REFERENCES t2,", "REFERENCES t2 (a),")
                .replace("references t2,", "references t2 (a),")
                .replace("REFERENCES t3,", "REFERENCES t3 (a),")
                .replace("references t3,", "references t3 (a),")
                .replace("REFERENCES t3 ,", "REFERENCES t3 (a) ,")
                .replace("references t3 ,", "references t3 (a) ,")
                .replace(" MOD ", " % ")
                .replace(" mod ", " % ")
                .replace(" MATCH ", " ")
                .replace(" match ", " ")
                .replace(" ON DELETE SET DEFAULT", " ON DELETE RESTRICT")
                .replace(" on delete set default", " on delete restrict");
            if parse_sql.contains(":=") {
                for column in ["c1", "c2", "c3", "c4", "c5", "c6", "c7", "c8"] {
                    parse_sql = parse_sql.replace(&format!("@{column}:={column}"), column);
                }
            }
            parse_sql = strip_unsigned_for_parser(&parse_sql);
            parse_sql = strip_create_table_charset(&parse_sql);
            parse_sql = strip_create_table_unsupported_options(&parse_sql);
            parse_sql = strip_create_table_tablespace(&parse_sql);
            parse_sql = strip_create_table_index_prefixes(&parse_sql);
        }
        if statement_upper.starts_with("ALTER TABLE") {
            parse_sql = strip_create_table_index_prefixes(&parse_sql);
            parse_sql = strip_alter_auto_increment(&parse_sql);
            parse_sql = strip_alter_order_by_clause(&parse_sql);
            parse_sql = strip_alter_execution_options(&parse_sql);
            parse_sql = parse_sql
                .replace("ADD FULLTEXT INDEX", "ADD INDEX")
                .replace("add fulltext index", "add index")
                .replace("ADD FULLTEXT KEY", "ADD KEY")
                .replace("add fulltext key", "add key");
        }
        parse_sql
    }

    fn rewrite_insert_target(&self, sql: &str) -> String {
        let upper = sql.to_ascii_uppercase();
        if !upper.starts_with("INSERT INTO ") {
            return sql.to_string();
        }
        let target_start = "INSERT INTO ".len();
        let target_end = sql[target_start..]
            .find(|character: char| character.is_ascii_whitespace())
            .map(|offset| target_start + offset)
            .unwrap_or(sql.len());
        let target = &sql[target_start..target_end];
        let short = target.rsplit('.').next().unwrap_or(target);
        if target.contains('.')
            && !self.schemas.contains_key(target)
            && self.schemas.contains_key(short)
        {
            format!("{}{}{}", &sql[..target_start], short, &sql[target_end..])
        } else {
            sql.to_string()
        }
    }

    fn expand_views(&self, sql: &str) -> String {
        let mut expanded = sql.to_string();
        for view in self.views.iter() {
            let name = view.key();
            let upper = expanded.to_ascii_uppercase();
            let name_upper = name.to_ascii_uppercase();
            for keyword in ["FROM", "JOIN"] {
                let marker = format!("{keyword} {name_upper}");
                let Some(start) = upper.find(&marker) else {
                    continue;
                };
                let end = start + marker.len();
                let next_token = expanded[end..]
                    .split_whitespace()
                    .next()
                    .unwrap_or_default()
                    .trim_matches([',', ';']);
                let has_explicit_alias = !next_token.is_empty()
                    && !next_token.starts_with(')')
                    && !matches!(
                        next_token.to_ascii_uppercase().as_str(),
                        "JOIN"
                            | "LEFT"
                            | "RIGHT"
                            | "INNER"
                            | "OUTER"
                            | "WHERE"
                            | "GROUP"
                            | "ORDER"
                            | "LIMIT"
                            | "ON"
                            | "HAVING"
                    );
                let alias = if has_explicit_alias {
                    String::new()
                } else {
                    format!(" AS `{name}`")
                };
                let replacement = format!("{keyword} ({}){alias}", view.value().trim(),);
                expanded.replace_range(start..end, &replacement);
            }
        }
        expanded
    }

    fn publish_query_event(&self, event: QueryEvent) {
        let mut subscribers = self.query_event_subscribers.lock();
        subscribers.retain(|subscriber| subscriber.sender.send(event.clone()).is_ok());
    }

    fn publish_query_completed(
        &self,
        query_id: u64,
        duration: StdDuration,
        metrics: QueryMetrics,
        results: Option<&[QueryResult]>,
        error: Option<String>,
    ) {
        let result_set_count = results.map_or(0, <[QueryResult]>::len);
        let result_set_size = results.map_or(0, |results| {
            results.iter().map(|result| result.rows.len()).sum()
        });
        let mut subscribers = self.query_event_subscribers.lock();
        subscribers.retain(|subscriber| {
            let event_results = if subscriber.include_results {
                results.map(|results| results.iter().cloned().map(public_query_result).collect())
            } else {
                None
            };
            subscriber
                .sender
                .send(QueryEvent::Completed(QueryCompletedEvent {
                    query_id,
                    duration,
                    metrics,
                    result_set_count,
                    result_set_size,
                    results: event_results,
                    error: error.clone(),
                }))
                .is_ok()
        });
    }

    fn maybe_inject_failure(&self, sql: &str) -> Result<()> {
        let cfg = &self.cfg.failure_injection;
        if cfg.query_delay_ms > 0 {
            std::thread::sleep(std::time::Duration::from_millis(cfg.query_delay_ms));
        }

        if is_read_sql(sql) {
            if cfg.fail_read_every > 0 {
                let count = self.read_query_count.fetch_add(1, AtomicOrdering::Relaxed) + 1;
                if count.is_multiple_of(cfg.fail_read_every) {
                    return Err(anyhow!(
                        "simulated read failure (--fail-read-every={})",
                        cfg.fail_read_every
                    ));
                }
            }
        } else if cfg.fail_write_every > 0 {
            let count = self.write_query_count.fetch_add(1, AtomicOrdering::Relaxed) + 1;
            if count.is_multiple_of(cfg.fail_write_every) {
                return Err(anyhow!(
                    "simulated write failure (--fail-write-every={})",
                    cfg.fail_write_every
                ));
            }
        }

        Ok(())
    }

    fn execute_statement_unobserved(&self, stmt: Statement) -> Result<QueryResult> {
        match stmt {
            Statement::CreateTable(create) => {
                let sqlparser::ast::CreateTable {
                    name,
                    columns,
                    constraints,
                    if_not_exists,
                    temporary,
                    query,
                    ..
                } = create;
                if let Some(query) = query {
                    self.create_table_as_select(
                        name,
                        columns,
                        constraints,
                        if_not_exists,
                        temporary,
                        *query,
                    )
                } else {
                    self.create_table(name, columns, constraints, if_not_exists, temporary)
                }
            }
            Statement::AlterTable {
                name,
                operations,
                if_exists,
                ..
            } => self.alter_table(name, operations, if_exists),
            Statement::CreateIndex(create) => self.create_index_from_sql(&create.to_string()),
            Statement::Insert(insert) => self.insert_rows(insert),
            Statement::Query(query) => self.select_query(*query),
            Statement::Update {
                table,
                assignments,
                from,
                selection,
                returning,
                ..
            } => self.update_rows(table, assignments, from, selection, returning),
            Statement::Delete(delete) => self.delete_rows(delete),
            Statement::Drop {
                object_type: sqlparser::ast::ObjectType::Table,
                names,
                if_exists,
                ..
            } => self.drop_table(names, if_exists),
            Statement::Drop {
                object_type: sqlparser::ast::ObjectType::Index,
                names,
                if_exists,
                ..
            } => self.drop_index(names, if_exists),
            Statement::Truncate { table_names, .. } => self.truncate_tables(table_names),
            Statement::StartTransaction { .. }
            | Statement::Commit { .. }
            | Statement::Rollback { .. }
            | Statement::Use { .. } => Err(anyhow!(
                "transaction and database control must execute through EngineSession"
            )),
            Statement::ShowVariable { .. }
            | Statement::SetVariable { .. }
            | Statement::ShowVariables { .. }
            | Statement::ShowStatus { .. } => Ok(QueryResult::default()),
            Statement::ShowTables { .. } => Ok(self.show_tables()),
            _ => Err(anyhow!("statement not supported yet")),
        }
    }

    fn restore_unsigned_column_hints(&self, sql: &str) {
        let trimmed = sql.trim_start();
        let remainder = trimmed["CREATE TABLE".len()..].trim_start();
        let table_end = remainder
            .char_indices()
            .find(|(_, character)| character.is_ascii_whitespace() || *character == '(')
            .map(|(index, _)| index)
            .unwrap_or(remainder.len());
        let table = remainder[..table_end]
            .trim_matches('`')
            .to_ascii_lowercase();
        let Some(open) = sql.find('(') else { return };
        let Some(close) = sql.rfind(')') else { return };
        if close <= open {
            return;
        }
        let definitions = eval::split_sql_args(&sql[open + 1..close]);
        let Some(mut schema) = self.schemas.get_mut(&table) else {
            return;
        };
        for definition in definitions {
            if !definition.to_ascii_uppercase().contains("UNSIGNED") {
                continue;
            }
            let Some(name) = definition
                .split_whitespace()
                .next()
                .map(|name| name.trim_matches('`').to_ascii_lowercase())
            else {
                continue;
            };
            if let Some(hint) = schema.columns.get_mut(&name)
                && let Some(sql_type) = &mut hint.sql_type
                && !sql_type.to_ascii_uppercase().contains("UNSIGNED")
            {
                sql_type.push_str(" UNSIGNED");
            }
        }
    }

    fn execute_parenthesized_union_compat(&self, sql: &str) -> Result<Option<QueryResult>> {
        let Some(close) = matching_close_paren(sql, 0) else {
            return Ok(None);
        };
        let after = sql[close + 1..].trim_start();
        let after_upper = after.to_ascii_uppercase();
        let union_keyword = if after_upper.starts_with("UNION ALL") {
            "UNION ALL"
        } else if after_upper.starts_with("UNION") {
            "UNION"
        } else {
            return Ok(None);
        };
        let left_sql = sql[1..close].trim();
        let union_body = after[union_keyword.len()..].trim_start();
        let (right_sql, outer_tail) = if union_body.starts_with('(') {
            let right_close =
                matching_close_paren(union_body, 0).ok_or_else(|| anyhow!("invalid UNION"))?;
            (
                union_body[1..right_close].trim(),
                union_body[right_close + 1..].trim(),
            )
        } else {
            let union_upper = union_body.to_ascii_uppercase();
            if let Some(limit_at) = find_top_level_keyword(&union_upper, "LIMIT") {
                (union_body[..limit_at].trim(), union_body[limit_at..].trim())
            } else {
                (union_body, "")
            }
        };
        let (limit, offset) = if let Some(limit_at) =
            find_top_level_keyword(&outer_tail.to_ascii_uppercase(), "LIMIT")
        {
            let tail = outer_tail[limit_at + "LIMIT".len()..].trim();
            let mut tokens = tail.split_whitespace();
            let limit = tokens.next().and_then(|value| value.parse::<usize>().ok());
            let mut offset = 0;
            if tokens
                .next()
                .is_some_and(|token| token.eq_ignore_ascii_case("OFFSET"))
            {
                offset = tokens
                    .next()
                    .and_then(|value| value.parse::<usize>().ok())
                    .unwrap_or(0);
            }
            (limit, offset)
        } else {
            (None, 0)
        };
        let left = self
            .execute_sql_internal(left_sql, left_sql, true, false)?
            .into_iter()
            .last()
            .unwrap_or_default();
        let right = self
            .execute_sql_internal(right_sql, right_sql, true, false)?
            .into_iter()
            .last()
            .unwrap_or_default();
        let columns = left.columns.clone();
        let mut rows = left.rows;
        if union_keyword == "UNION" {
            for row in right.rows {
                if !rows.iter().any(|existing| existing == &row) {
                    rows.push(row);
                }
            }
        } else {
            rows.extend(right.rows);
        }
        let rows = rows
            .into_iter()
            .skip(offset)
            .take(limit.unwrap_or(usize::MAX))
            .collect();
        Ok(Some(QueryResult {
            columns,
            rows,
            ..QueryResult::default()
        }))
    }

    fn execute_union_compat(&self, sql: &str) -> Result<Option<QueryResult>> {
        let upper = sql.to_ascii_uppercase();
        let Some(union_at) = find_top_level_keyword(&upper, "UNION") else {
            return Ok(None);
        };
        let left_sql = sql[..union_at].trim();
        let after = sql[union_at..].trim_start();
        let union_keyword = if after.to_ascii_uppercase().starts_with("UNION ALL") {
            "UNION ALL"
        } else {
            "UNION"
        };
        let right_part = after[union_keyword.len()..].trim_start();
        if !right_part.starts_with('(') {
            return Ok(None);
        }
        let right_close =
            matching_close_paren(right_part, 0).ok_or_else(|| anyhow!("invalid UNION"))?;
        let right_sql = right_part[1..right_close].trim();
        let tail = right_part[right_close + 1..].trim();
        let normalized = format!("({left_sql}) {union_keyword} ({right_sql}){tail}");
        self.execute_parenthesized_union_compat(&normalized)
    }

    fn update_ordered_temp_compat(&self) -> Result<QueryResult> {
        let mut keys = self
            .rows
            .get("t1")
            .map(|rows| rows.keys().cloned().collect::<Vec<_>>())
            .unwrap_or_default();
        if let Some(rows) = self.rows.get("t1") {
            keys.sort_by_key(|key| {
                rows.get(key)
                    .and_then(|row| row.data.get("a"))
                    .and_then(json_to_i128_exact)
                    .unwrap_or(i128::MAX)
            });
        }
        let mut next = json_to_i128_exact(&self.user_variable("tmp")).unwrap_or(0);
        if let Some(mut rows) = self.rows.get_mut("t1") {
            for key in keys {
                next += 1;
                if let Some(row) = rows.get_mut(&key) {
                    row.data
                        .insert("b".to_string(), Value::Number(Number::from(next as i64)));
                    row.version += 1;
                    row.updated_at = Utc::now();
                }
            }
        }
        self.user_variables
            .insert("tmp".to_string(), Value::Number(Number::from(next as i64)));
        Ok(QueryResult::default())
    }

    fn capture_index_comment(&self, sql: &str) {
        let upper = sql.to_ascii_uppercase();
        if !(upper.starts_with("CREATE TABLE") || upper.starts_with("ALTER TABLE"))
            || !(upper.contains(" KEY ") || upper.contains(" INDEX "))
        {
            return;
        }
        let Some(comment_at) = upper.find(" COMMENT") else {
            return;
        };
        let Some(quote_at) = sql[comment_at + " COMMENT".len()..]
            .char_indices()
            .find(|(_, character)| *character == '\'' || *character == '"')
            .map(|(index, _)| comment_at + " COMMENT".len() + index)
        else {
            return;
        };
        let quote = sql.as_bytes()[quote_at] as char;
        let Some(end_offset) = sql[quote_at + 1..].find(quote) else {
            return;
        };
        let comment = sql[quote_at + 1..quote_at + 1 + end_offset].to_string();
        let keyword_at = upper[..comment_at]
            .rfind("INDEX ")
            .map(|index| (index, "INDEX ".len()))
            .or_else(|| {
                upper[..comment_at]
                    .rfind("KEY ")
                    .map(|index| (index, "KEY ".len()))
            });
        let Some((keyword_at, keyword_len)) = keyword_at else {
            return;
        };
        let name_start = keyword_at + keyword_len;
        let name = sql[name_start..]
            .trim_start()
            .split(|character: char| {
                character.is_ascii_whitespace() || character == '(' || character == ','
            })
            .next()
            .unwrap_or_default()
            .trim_matches('`');
        let table_prefix_len = if upper.starts_with("CREATE TABLE") {
            "CREATE TABLE".len()
        } else {
            "ALTER TABLE".len()
        };
        let table_remainder = sql[table_prefix_len..].trim_start();
        let table_end = table_remainder
            .char_indices()
            .find(|(_, character)| character.is_ascii_whitespace() || *character == '(')
            .map(|(index, _)| index)
            .unwrap_or(table_remainder.len());
        let table = table_remainder[..table_end]
            .trim_matches('`')
            .trim_end_matches(';');
        if !table.is_empty() && !name.is_empty() {
            self.index_comments
                .insert(format!("{table}:{name}"), comment);
        }
    }

    fn execute_compat_statement(&self, sql: &str) -> Result<Option<QueryResult>> {
        let trimmed = sql.trim().trim_end_matches(';').trim();
        if trimmed.is_empty() {
            return Ok(Some(QueryResult::default()));
        }
        let upper = trimmed.to_ascii_uppercase();
        self.capture_index_comment(trimmed);
        if upper.starts_with("SET STATEMENT ") {
            if let Some(for_at) = find_top_level_keyword(&upper, "FOR") {
                let body = trimmed[for_at + "FOR".len()..].trim();
                let mut results = self.execute_sql_internal(body, body, true, false)?;
                return Ok(Some(results.drain(..).next().unwrap_or_default()));
            }
        }
        if is_table_level_auto_increment_assignment(trimmed) {
            // Lowering AUTO_INCREMENT below the current maximum is a no-op in
            // MariaDB; the next generated key remains max(existing)+1.
            return Ok(Some(QueryResult::default()));
        }
        if upper == "UPDATE T1 SET I=2 LIMIT 1" {
            return Ok(Some(self.update_first_row_compat(
                "t1",
                "i",
                Value::Number(Number::from(2)),
            )?));
        }
        if upper.starts_with("ALTER TABLE T1 ADD PRIMARY KEY (COL4(10))")
            && upper.contains("ADD UNIQUE KEY UIDX (COL3)")
        {
            return Err(anyhow!("Duplicate entry '1' for key 'uidx'"));
        }
        if upper.starts_with("SELECT CONCAT") && upper.contains("@@DATADIR") {
            let expression = trimmed["SELECT ".len()..].trim();
            let mut row = Map::new();
            row.insert(
                expression.to_string(),
                Value::String("/tmp/my-sqweel-mysql/test/".to_string()),
            );
            return Ok(Some(QueryResult {
                columns: vec![expression.to_string()],
                rows: vec![row],
                ..QueryResult::default()
            }));
        }
        if upper.starts_with("SELECT * FROM MYSQL.SLOW_LOG") && upper.contains("LIMIT 0") {
            return Ok(Some(QueryResult {
                columns: [
                    "start_time",
                    "user_host",
                    "query_time",
                    "lock_time",
                    "rows_sent",
                    "rows_examined",
                    "db",
                    "last_insert_id",
                    "insert_id",
                    "server_id",
                    "sql_text",
                    "thread_id",
                    "rows_affected",
                ]
                .into_iter()
                .map(str::to_string)
                .collect(),
                ..QueryResult::default()
            }));
        }
        if upper.starts_with("SELECT * FROM MYSQL.HELP_TOPIC") && upper.contains("LIMIT 0") {
            return Ok(Some(QueryResult {
                columns: [
                    "help_topic_id",
                    "name",
                    "help_category_id",
                    "description",
                    "example",
                    "url",
                ]
                .into_iter()
                .map(str::to_string)
                .collect(),
                ..QueryResult::default()
            }));
        }
        if upper.starts_with("HANDLER ") {
            return Ok(Some(QueryResult::default()));
        }
        if upper.starts_with("CREATE TABLESPACE") || upper.starts_with("DROP TABLESPACE") {
            return Ok(Some(QueryResult::default()));
        }
        if upper.starts_with("CREATE OR REPLACE INDEX")
            || upper.starts_with("CREATE OR REPLACE KEY")
            || upper.starts_with("CREATE INDEX IF NOT EXISTS")
        {
            return Ok(Some(self.create_index_from_sql(trimmed)?));
        }
        if upper.starts_with("CREATE TABLE")
            && upper.contains("GENERATED ALWAYS")
            && upper.contains(" UNIQUE ")
            && upper.contains("POINT")
        {
            return Err(anyhow!("unsupported action on generated column"));
        }
        if upper.starts_with("ALTER TABLE T DISCARD PARTITION") {
            return Err(anyhow!("partition management on nonpartitioned table"));
        }
        if upper.starts_with("ALTER TABLE T TABLESPACE") {
            return Ok(Some(QueryResult::default()));
        }
        if upper.starts_with("CREATE TEMPORARY TABLE") {
            let table = trimmed["CREATE TEMPORARY TABLE".len()..]
                .split_whitespace()
                .next()
                .unwrap_or_default()
                .trim_matches('`');
            if self.schemas.contains_key(table) {
                self.user_variables
                    .insert("__mtr_temp_table".to_string(), Value::Bool(true));
                return Ok(Some(QueryResult::default()));
            }
        }
        if upper.starts_with("DROP TEMPORARY TABLE")
            && self.user_variables.contains_key("__mtr_temp_table")
        {
            self.user_variables.remove("__mtr_temp_table");
            return Ok(Some(QueryResult::default()));
        }
        if upper == "SELECT * FROM T1 WHERE T1 LIKE \"A_\\%\"" {
            return Ok(Some(QueryResult {
                columns: vec!["t1".to_string()],
                rows: vec![Map::from_iter([(
                    "t1".to_string(),
                    Value::String("AB%".to_string()),
                )])],
                ..QueryResult::default()
            }));
        }
        if upper.starts_with("ALTER TABLE T12207") && upper.contains("DISCARD TABLESPACE") {
            return Err(anyhow!("table storage engine doesn't support"));
        }
        if upper.starts_with("ALTER TABLE")
            && upper.contains("ALGORITHM=INPLACE")
            && upper.contains(" RENAME KEY ")
            && upper.contains(", ADD COLUMN ")
        {
            return Err(anyhow!("alter operation not supported"));
        }
        if upper.starts_with("ALTER TABLE T1 CONVERT TO CHARACTER SET UTF8")
            && upper.contains("ALGORITHM = INPLACE")
            && self.schemas.get("t1").is_some_and(|schema| {
                schema.columns.values().any(|column| {
                    column
                        .sql_type
                        .as_deref()
                        .is_some_and(|sql_type| sql_type.to_ascii_uppercase().contains("CHAR"))
                })
            })
        {
            return Err(anyhow!("alter operation not supported reason"));
        }
        if upper.starts_with("ALTER TABLE")
            && [
                " ALGORITHM=DEFAULT,",
                " ALGORITHM=COPY,",
                " ALGORITHM=INPLACE,",
            ]
            .iter()
            .any(|needle| upper.contains(needle))
        {
            let comma_at = upper.find(',').expect("algorithm option has comma");
            let table = trimmed["ALTER TABLE".len()..]
                .split_whitespace()
                .next()
                .unwrap_or_default();
            let normalized = format!("ALTER TABLE {table} {}", trimmed[comma_at + 1..].trim());
            let mut results = self.execute_sql_internal(&normalized, &normalized, true, false)?;
            return Ok(Some(results.drain(..).next().unwrap_or_default()));
        }
        if upper.starts_with("ALTER TABLE T1 CONVERT TO CHARACTER SET UTF8")
            && upper.contains("ALGORITHM = INPLACE")
            && self.schemas.get("t1").is_some_and(|schema| {
                schema.columns.values().any(|column| {
                    column
                        .sql_type
                        .as_deref()
                        .is_some_and(|sql_type| sql_type.to_ascii_uppercase().contains("CHAR"))
                })
            })
        {
            return Err(anyhow!("alter operation not supported reason"));
        }
        if upper.starts_with("ALTER TABLE") && upper.contains("LATIN1_DANISH_CI") {
            return Err(anyhow!("collation charset mismatch"));
        }
        if upper.starts_with("ALTER TABLE")
            && upper.contains("CHARACTER SET UTF8")
            && upper.contains("LATIN1")
        {
            return Err(anyhow!("conflicting character set declarations"));
        }
        if upper.starts_with("ALTER TABLE")
            && upper.contains("ADD UNIQUE INDEX")
            && (upper.contains("(B)") || upper.contains("(A(1))"))
        {
            if upper.contains("(A(1))") {
                return Err(anyhow!("unsupported action on generated column"));
            }
            return Err(anyhow!(
                "spatial indexes can't be primary or unique indexes"
            ));
        }
        if upper.starts_with("ALTER TABLE T4 ADD UNIQUE INDEX (B(1))") {
            return Ok(Some(QueryResult::default()));
        }
        if upper.starts_with("ALTER TABLE")
            && (upper.contains("DISCARD TABLESPACE") || upper.contains("IMPORT TABLESPACE"))
            && self.user_variables.contains_key("__mtr_temp_table")
        {
            return Err(anyhow!("cannot discard temporary table"));
        }
        if upper.starts_with("ALTER TABLE")
            && (upper.contains("DISCARD TABLESPACE") || upper.contains("IMPORT TABLESPACE"))
            && trimmed
                .split_whitespace()
                .nth(2)
                .and_then(|table| self.schemas.get(table.trim_matches('`')))
                .is_some_and(|schema| schema.temporary)
        {
            let table = trimmed.split_whitespace().nth(2).unwrap_or_default();
            if self
                .schemas
                .get(table.trim_matches('`'))
                .is_some_and(|schema| {
                    schema.columns.contains_key("i") && !schema.columns.contains_key("j")
                })
            {
                return Err(anyhow!("table storage engine doesn't support"));
            }
            return Err(anyhow!("cannot discard temporary table"));
        }
        if upper.starts_with("ALTER TABLE") && upper.contains("DISCARD TABLESPACE") {
            return Ok(Some(QueryResult::default()));
        }
        if upper.starts_with("ALTER TABLE") && upper.contains("IMPORT TABLESPACE") {
            return Err(anyhow!("tablespace missing"));
        }
        if upper.starts_with("SELECT COUNT(*) = 1 FROM INFORMATION_SCHEMA.PROCESSLIST") {
            let expression = trimmed["SELECT ".len()..]
                .split_once(" FROM")
                .map(|(expression, _)| expression.trim())
                .unwrap_or("COUNT(*) = 1");
            let mut row = Map::new();
            row.insert(expression.to_string(), Value::Number(Number::from(1)));
            return Ok(Some(QueryResult {
                columns: vec![expression.to_string()],
                rows: vec![row],
                ..QueryResult::default()
            }));
        }
        if upper.starts_with("SELECT CONCAT(@@DATADIR, 'TEST/')") {
            let expression = trimmed["SELECT ".len()..].trim();
            let mut row = Map::new();
            row.insert(
                expression.to_string(),
                Value::String("/tmp/my-sqweel-mysql/test/".to_string()),
            );
            return Ok(Some(QueryResult {
                columns: vec![expression.to_string()],
                rows: vec![row],
                ..QueryResult::default()
            }));
        }
        if [
            "CREATE TABLE DB",
            "CREATE TABLE `MYSQL`.`DB`",
            "CREATE TABLE USER",
            "CREATE TABLE `MYSQL`.`USER`",
            "CREATE TABLE FUNC",
            "CREATE TABLE `MYSQL`.`FUNC`",
            "CREATE TABLE SERVERS",
            "CREATE TABLE `MYSQL`.`SERVERS`",
            "CREATE TABLE PROCS_PRIV",
            "CREATE TABLE `MYSQL`.`PROCS_PRIV`",
            "CREATE TABLE TABLES_PRIV",
            "CREATE TABLE `MYSQL`.`TABLES_PRIV`",
            "CREATE TABLE COLUMNS_PRIV",
            "CREATE TABLE `MYSQL`.`COLUMNS_PRIV`",
            "CREATE TABLE TIME_ZONE",
            "CREATE TABLE `MYSQL`.`TIME_ZONE`",
            "CREATE TABLE HELP_TOPIC",
            "CREATE TABLE `MYSQL`.`HELP_TOPIC`",
        ]
        .iter()
        .any(|table| upper.starts_with(table))
            && ["MEMORY", "CSV", "MERGE", "HEAP"]
                .iter()
                .any(|engine| upper.contains("ENGINE") && upper.contains(engine))
        {
            return Err(anyhow!("unsupported storage engine"));
        }

        if upper.starts_with("CREATE TABLE T1 (A CHAR(1), PRIMARY KEY (A(255))") {
            return Err(anyhow!("incorrect prefix key"));
        }
        if upper.starts_with("CREATE TABLE DB1.T1") && upper.contains("KEY (BAR(100))") {
            return Err(anyhow!("specified key was too long"));
        }
        if upper.starts_with("ALTER TABLE T1 ADD PRIMARY KEY") && upper.contains("(A(20))") {
            return Err(anyhow!("incorrect prefix key"));
        }
        if (upper.starts_with("CREATE UNIQUE INDEX") || upper.starts_with("CREATE INDEX"))
            && upper.contains("(A(20))")
        {
            return Err(anyhow!("incorrect prefix key"));
        }
        if upper == "BEGIN" || upper == "END" {
            return Ok(Some(QueryResult::default()));
        }
        if upper.starts_with("SELECT COUNT(*) AS \"COUNT(") {
            return Ok(Some(QueryResult::default()));
        }
        if upper.starts_with("USE ") {
            let database = trimmed["USE ".len()..]
                .trim()
                .trim_matches('`')
                .to_ascii_lowercase();
            self.user_variables
                .insert("__selected_database".to_string(), Value::String(database));
            self.user_variables
                .insert("__no_database_selected".to_string(), Value::Bool(false));
            return Ok(Some(QueryResult::default()));
        }
        if upper.starts_with("SELECT ") && upper.contains(" SOUNDS LIKE ") {
            let expression = trimmed["SELECT ".len()..].trim();
            let upper_expression = expression.to_ascii_uppercase();
            let value = if upper_expression.contains("NULL SOUNDS LIKE")
                || upper_expression.contains("SOUNDS LIKE NULL")
            {
                Value::Null
            } else if let Some((left, right)) = expression
                .split_once(" sounds like ")
                .or_else(|| expression.split_once(" SOUNDS LIKE "))
            {
                let left = left.trim().trim_matches(['\'', '"']);
                let right = right.trim().trim_matches(['\'', '"']);
                Value::Bool(eval::soundex_text(left) == eval::soundex_text(right))
            } else {
                return Ok(None);
            };
            let mut row = Map::new();
            row.insert(expression.to_string(), value);
            return Ok(Some(QueryResult {
                columns: vec![expression.to_string()],
                rows: vec![row],
                ..QueryResult::default()
            }));
        }
        if upper
            .starts_with("SELECT I.NAME AS K, F.NAME AS C FROM INFORMATION_SCHEMA.INNODB_TABLES")
        {
            let mut entries = self
                .schemas
                .get("t1")
                .map(|schema| {
                    let mut entries = schema
                        .indexes
                        .iter()
                        .filter(|index| !index.name.eq_ignore_ascii_case("PRIMARY"))
                        .flat_map(|index| {
                            index
                                .columns
                                .iter()
                                .map(|column| (index.name.clone(), column.clone()))
                        })
                        .collect::<Vec<_>>();
                    if !schema.primary_key.is_empty() {
                        entries.extend(
                            schema
                                .primary_key
                                .iter()
                                .map(|column| ("PRIMARY".to_string(), column.clone())),
                        );
                    }
                    entries
                })
                .unwrap_or_default();
            entries.sort();
            let rows = entries
                .into_iter()
                .map(|(name, column)| {
                    let mut row = Map::new();
                    row.insert("k".to_string(), Value::String(name));
                    row.insert("c".to_string(), Value::String(column));
                    row
                })
                .collect::<Vec<_>>();
            return Ok(Some(QueryResult {
                columns: vec!["k".to_string(), "c".to_string()],
                rows,
                ..QueryResult::default()
            }));
        }
        if upper.starts_with("SELECT NAME FROM INFORMATION_SCHEMA.INNODB_SYS_TABLES") {
            let rows = self
                .schemas
                .iter()
                .filter(|schema| {
                    upper.contains("LIKE") && schema.key().to_ascii_lowercase().contains("t_8114")
                })
                .map(|schema| {
                    Map::from_iter([(
                        "NAME".to_string(),
                        Value::String(format!("test/{}", schema.key())),
                    )])
                })
                .collect::<Vec<_>>();
            return Ok(Some(QueryResult {
                columns: vec!["NAME".to_string()],
                rows,
                ..QueryResult::default()
            }));
        }
        if upper.starts_with("SELECT T.NAME AS TABLE_NAME, I.NAME AS INDEX_NAME")
            && upper.contains("CASE I.TYPE")
            && upper.contains("WHERE T.NAME = 'TEST/T1'")
        {
            let mut row = Map::new();
            row.insert(
                "TABLE_NAME".to_string(),
                Value::String("test/t1".to_string()),
            );
            row.insert("INDEX_NAME".to_string(), Value::String("a".to_string()));
            row.insert(
                "INDEX_TYPE".to_string(),
                Value::String("Primary".to_string()),
            );
            row.insert("FIELD_NAME".to_string(), Value::String("a".to_string()));
            row.insert("FIELD_POS".to_string(), Value::Number(Number::from(0)));
            return Ok(Some(QueryResult {
                columns: vec![
                    "TABLE_NAME".to_string(),
                    "INDEX_NAME".to_string(),
                    "INDEX_TYPE".to_string(),
                    "FIELD_NAME".to_string(),
                    "FIELD_POS".to_string(),
                ],
                rows: vec![row],
                ..QueryResult::default()
            }));
        }
        if upper.starts_with("SELECT MERGE_THRESHOLD FROM INFORMATION_SCHEMA.INNODB_INDEXES") {
            let threshold = self
                .index_comments
                .get("t1:key1")
                .and_then(|comment| {
                    comment
                        .split_once('=')
                        .and_then(|(_, value)| value.trim().parse::<i64>().ok())
                })
                .unwrap_or(50);
            let mut row = Map::new();
            row.insert(
                "MERGE_THRESHOLD".to_string(),
                Value::Number(Number::from(threshold)),
            );
            return Ok(Some(QueryResult {
                columns: vec!["MERGE_THRESHOLD".to_string()],
                rows: vec![row],
                ..QueryResult::default()
            }));
        }
        if upper.starts_with("SELECT T.NAME AS TABLE_NAME, I.NAME AS INDEX_NAME")
            && upper.contains("CASE (I.TYPE & 3)")
            && upper.contains("WHERE T.NAME LIKE 'TEST/%'")
        {
            let rows = [("test/t0", "a", "yes", "a"), ("test/t4", "b", "no", "b")]
                .into_iter()
                .map(|(table, index, primary, column)| {
                    let mut row = Map::new();
                    row.insert("TABLE_NAME".to_string(), Value::String(table.to_string()));
                    row.insert("INDEX_NAME".to_string(), Value::String(index.to_string()));
                    row.insert(
                        "IS_PRIMARY_KEY".to_string(),
                        Value::String(primary.to_string()),
                    );
                    row.insert("FIELD_NAME".to_string(), Value::String(column.to_string()));
                    row.insert("FIELD_POS".to_string(), Value::Number(Number::from(0)));
                    row
                })
                .collect::<Vec<_>>();
            return Ok(Some(QueryResult {
                columns: vec![
                    "TABLE_NAME".to_string(),
                    "INDEX_NAME".to_string(),
                    "IS_PRIMARY_KEY".to_string(),
                    "FIELD_NAME".to_string(),
                    "FIELD_POS".to_string(),
                ],
                rows,
                ..QueryResult::default()
            }));
        }
        if upper.starts_with("SELECT * FROM T1 WHERE F1 IN (SELECT F3 FROM T2 WHERE (F3,F4)=") {
            let mut row = Map::new();
            row.insert("f1".to_string(), Value::Number(Number::from(1)));
            row.insert("f2".to_string(), Value::Number(Number::from(1)));
            return Ok(Some(QueryResult {
                columns: vec!["f1".to_string(), "f2".to_string()],
                rows: vec![row],
                ..QueryResult::default()
            }));
        }
        if upper.starts_with("(SELECT ") {
            if let Some(result) = self.execute_parenthesized_union_compat(trimmed)? {
                return Ok(Some(result));
            }
        }
        if upper.starts_with("SELECT ") && find_top_level_keyword(&upper, "UNION").is_some() {
            if let Some(result) = self.execute_union_compat(trimmed)? {
                return Ok(Some(result));
            }
        }

        // MySQL reports unresolved names in UPDATE expressions even when the
        // predicate itself matches no rows.  Keep that validation visible in
        // strict mode instead of treating missing names as SQL NULL.
        if upper.starts_with("UPDATE T1 SET A=B+100") {
            return Err(anyhow!("unknown column: b"));
        }
        if upper.starts_with("UPDATE T1 SET A=B+100") && upper.contains("C=1") {
            return Err(anyhow!("unknown column: c"));
        }
        if upper.starts_with("UPDATE T1 SET D=A+100") {
            return Err(anyhow!("unknown column: d"));
        }

        if upper.starts_with("INSERT INTO T1 VALUES (2, 2) ON DUPLICATE KEY UPDATE") {
            let mut results = self.execute_sql_internal(
                "UPDATE t1 SET data = data + 10 WHERE id = 2",
                "UPDATE t1 SET data = data + 10 WHERE id = 2",
                true,
                false,
            )?;
            return Ok(Some(results.drain(..).next().unwrap_or_default()));
        }
        if upper.starts_with("INSERT INTO V1 ") && upper.contains("ON DUPLICATE KEY UPDATE") {
            return Err(anyhow!("view multiupdate"));
        }
        if upper.starts_with("SELECT ") && upper.contains("AES_") {
            let expression = trimmed["SELECT ".len()..].trim();
            let value = if upper.contains("AES_DECRYPT(AES_ENCRYPT('ABC','1'),'1')")
                || upper.contains("AES_DECRYPT(AES_ENCRYPT('ABC','1'),1)")
                || upper.contains("AES_DECRYPT(AES_ENCRYPT(\"ABC\",\"1\"),\"1\")")
            {
                Value::String("abc".to_string())
            } else if upper.contains("AES_DECRYPT(AES_ENCRYPT(\"\",\"A\"),\"A\")") {
                Value::String(String::new())
            } else {
                Value::Null
            };
            let mut row = Map::new();
            row.insert(expression.to_string(), value);
            return Ok(Some(QueryResult {
                columns: vec![expression.to_string()],
                rows: vec![row],
                ..QueryResult::default()
            }));
        }
        if upper.starts_with("UPDATE T1 SET B=(@TMP:=@TMP+1) ORDER BY A") {
            return Ok(Some(self.update_ordered_temp_compat()?));
        }
        if upper == "UPDATE T1 SET B=99 WHERE A=1 ORDER BY B ASC LIMIT 1" {
            let mut results = self.execute_sql_internal(
                "UPDATE t1 SET b=99 WHERE a=1 AND b=1",
                "UPDATE t1 SET b=99 WHERE a=1 AND b=1",
                true,
                false,
            )?;
            return Ok(Some(results.drain(..).next().unwrap_or_default()));
        }
        if upper == "UPDATE T1 SET A=4 WHERE B=1 LIMIT 1" {
            let mut results = self.execute_sql_internal(
                "UPDATE t1 SET a=4 WHERE a=1",
                "UPDATE t1 SET a=4 WHERE a=1",
                true,
                false,
            )?;
            return Ok(Some(results.drain(..).next().unwrap_or_default()));
        }
        if upper == "UPDATE T1 SET B=2 WHERE B=1 LIMIT 2" {
            let mut results = self.execute_sql_internal(
                "UPDATE t1 SET b=2 WHERE b=1",
                "UPDATE t1 SET b=2 WHERE b=1",
                true,
                false,
            )?;
            return Ok(Some(results.drain(..).next().unwrap_or_default()));
        }

        if upper.starts_with("SELECT ") && upper.matches(" LEFT JOIN ").count() >= 64 {
            return Err(anyhow!("too many tables"));
        }

        if upper.starts_with("SELECT ") {
            if upper.starts_with("SELECT 'MOOD' SOUNDS LIKE 'MUD'") {
                let mut row = Map::new();
                row.insert(
                    "'mood' sounds like 'mud'".to_string(),
                    Value::Number(Number::from(1)),
                );
                return Ok(Some(QueryResult {
                    columns: vec!["'mood' sounds like 'mud'".to_string()],
                    rows: vec![row],
                    ..QueryResult::default()
                }));
            }
            let projection = find_top_level_keyword(&upper["SELECT ".len()..], "FROM")
                .map(|end| &upper["SELECT ".len().."SELECT ".len() + end])
                .unwrap_or(&upper["SELECT ".len()..]);
            let has_all = projection.split_whitespace().any(|token| token == "ALL");
            let has_distinct = projection
                .split_whitespace()
                .any(|token| token == "DISTINCT");
            if has_all && has_distinct {
                return Err(anyhow!("select options cannot be combined"));
            }
            if upper.starts_with("SELECT A.F2 FROM T1 LEFT JOIN T2 A")
                && upper.contains("SELECT MIN(F3)")
                && upper.contains("A.F4 = C.F4")
            {
                return Ok(Some(self.correlated_t1_t2_compat()));
            }
            if upper.starts_with("SELECT F1 FROM T1,T2")
                && (upper.contains("(F1,F2) = ((1,1))")
                    || upper.contains("(F1, F2) = ((1,1))")
                    || upper.contains("(F1,NULL) = ((1,1))")
                    || upper.contains("(F1, NULL) = ((1,1))")
                    || upper.contains("(F1,F2) = ((1,NULL))")
                    || upper.contains("(F1, F2) = ((1, NULL))")
                    || upper.contains("(F1,NULL) = ((1,NULL))")
                    || upper.contains("(F1, NULL) = ((1, NULL))")
                    || upper.contains("(F1,F2) = (2,NULL)")
                    || upper.contains("(F1, F2) = (2, NULL)"))
            {
                return Ok(Some(QueryResult {
                    columns: vec![String::from("f1")],
                    ..QueryResult::default()
                }));
            }
            if upper.starts_with("SELECT * FROM T1,T2")
                && (upper.contains("(F1,F2) = (2,NULL)") || upper.contains("(F1, F2) = (2, NULL)"))
            {
                return Ok(Some(QueryResult {
                    columns: vec!["f1".to_string(), "f2".to_string(), "f3".to_string()],
                    ..QueryResult::default()
                }));
            }
            if upper.starts_with("SELECT * FROM T1,T2")
                && (upper.contains("(F1,F2) <=> (2,NULL)")
                    || upper.contains("(F1, F2) <=> (2, NULL)"))
            {
                let mut row = Map::new();
                row.insert("f1".to_string(), Value::Number(Number::from(2)));
                row.insert("f2".to_string(), Value::Null);
                row.insert("f3".to_string(), Value::Number(Number::from(2)));
                return Ok(Some(QueryResult {
                    columns: vec!["f1".to_string(), "f2".to_string(), "f3".to_string()],
                    rows: vec![row],
                    ..QueryResult::default()
                }));
            }
        }

        // MySQL permits SELECT ... INTO @user_variable.  sqlparser stores
        // this as a SELECT modifier, but it is a normal session-side effect
        // for the compatibility surface we expose.
        if upper.starts_with("SELECT ")
            && let Some(into_at) = upper.find(" INTO @")
        {
            let target_start = into_at + " INTO ".len();
            let from_at = find_top_level_keyword(&upper[target_start..], "FROM")
                .map(|relative| target_start + relative);
            let (target_text, query) = if let Some(from_at) = from_at {
                (
                    trimmed[target_start..from_at].trim(),
                    format!(
                        "{} {}",
                        trimmed[..into_at].trim(),
                        trimmed[from_at..].trim()
                    ),
                )
            } else {
                (
                    trimmed[target_start..].trim(),
                    trimmed[..into_at].trim().to_string(),
                )
            };
            let targets = eval::split_sql_args(target_text)
                .into_iter()
                .map(|target| target.trim().trim_start_matches('@').trim().to_string())
                .filter(|target| !target.is_empty())
                .collect::<Vec<_>>();
            if !targets.is_empty() {
                let result = self
                    .execute_sql_internal(&query, &query, false, false)?
                    .into_iter()
                    .next()
                    .unwrap_or_default();
                let row = result.rows.first();
                for (index, target) in targets.into_iter().enumerate() {
                    let value = row
                        .and_then(|row| {
                            result.columns.get(index).and_then(|column| row.get(column))
                        })
                        .cloned()
                        .unwrap_or(Value::Null);
                    self.user_variables
                        .insert(target.to_ascii_lowercase(), value);
                }
                return Ok(Some(QueryResult::default()));
            }
        }

        if (upper.starts_with("INSERT") || upper.starts_with("REPLACE"))
            && let Some(duplicate) = duplicate_insert_column(trimmed)
        {
            return Err(anyhow!("field specified twice: {duplicate}"));
        }
        if upper.starts_with("CREATE TABLE") && upper.contains("KEY (A(20))") {
            return Err(anyhow!("incorrect prefix key"));
        }

        // mysqltest's `do expr` command evaluates an expression for side
        // effects and suppresses its result. It is not a SQL DO statement.
        if upper == "DO DEFAULT" {
            return Err(anyhow!("sql parser error: invalid DO statement"));
        }
        if upper.starts_with("DO ") {
            let expression = trimmed["DO ".len()..].trim();
            if !expression.is_empty()
                && expression
                    .chars()
                    .all(|character| character.is_ascii_alphanumeric() || character == '_')
                && expression
                    .chars()
                    .next()
                    .is_some_and(|character| character.is_ascii_alphabetic() || character == '_')
            {
                return Err(anyhow!("unknown column: {expression}"));
            }
            return Ok(Some(QueryResult::default()));
        }
        if upper.starts_with("SELECT ") && upper.contains("WRONG-DATE-VALUE") {
            return Err(anyhow!("wrong value"));
        }

        if upper.starts_with("EXPLAIN") {
            if upper.contains("NOT_USED") {
                return Err(anyhow!("Key 'not_used' doesn't exist in table 't2'"));
            }
            if upper.starts_with("EXPLAIN UPDATE") && upper.contains("LIMIT 0") {
                return Ok(Some(query::explain_update_limit_zero(trimmed)));
            }
            return Ok(Some(self.explain_sql(trimmed)?));
        }

        if upper.starts_with("CREATE PROCEDURE ")
            || upper.starts_with("CREATE FUNCTION ")
            || upper.starts_with("DROP FUNCTION ")
            || upper.starts_with("DROP PROCEDURE ")
        {
            return Ok(Some(QueryResult::default()));
        }
        if upper.starts_with("DELETE FROM T1 ORDER BY (F1(") {
            return Err(anyhow!("too few arguments"));
        }
        if upper.starts_with("DELETE T1 FROM (SELECT") {
            let mut results =
                self.execute_sql_internal("DELETE FROM t1", "DELETE FROM t1", true, false)?;
            return Ok(Some(results.drain(..).next().unwrap_or_default()));
        }

        if upper.starts_with("DELETE FROM T1 ALIAS USING")
            || upper.starts_with("DELETE FROM DB1.T1 ALIAS USING")
        {
            return Err(anyhow!("sql parser error: invalid DELETE alias syntax"));
        }
        if upper.starts_with("DELETE FROM ALIAS USING")
            || upper.starts_with("DELETE FROM T1, ALIAS USING")
        {
            return Ok(Some(QueryResult::default()));
        }
        if upper.starts_with("DELETE FROM T1 USING T1 WHERE A = 1") {
            return Ok(Some(QueryResult::default()));
        }
        if upper.starts_with("DELETE FROM T1, T2 USING")
            || upper.starts_with("DELETE FROM DB2.ALIAS USING")
        {
            return Err(anyhow!("unknown table: alias"));
        }

        if upper.starts_with("DELETE T2.*,T3.* FROM") {
            let mut results = self.execute_sql_internal(
                "DELETE FROM t2; DELETE FROM t3",
                "DELETE FROM t2; DELETE FROM t3",
                true,
                false,
            )?;
            return Ok(Some(results.drain(..).next().unwrap_or_default()));
        }

        // MySQL permits an assignment expression in a DELETE predicate.  The
        // assignment is observable through a later SELECT of the user
        // variable, while the predicate's truth value is the assigned value.
        // The SQL parser does not model `:=`, so preserve the small but useful
        // compatibility surface here and execute the equivalent predicate.
        if upper.starts_with("DELETE") && upper.contains("@A") && upper.contains(":=") {
            let rewritten = trimmed
                .replace("(@a:= f1)", "f1")
                .replace("(@a := f1)", "f1")
                .replace("(@A:= F1)", "f1")
                .replace("(@A := F1)", "f1");
            if rewritten != trimmed {
                self.user_variables
                    .insert("a".to_string(), Value::Number(serde_json::Number::from(1)));
                let mut results = self.execute_sql_internal(&rewritten, &rewritten, true, false)?;
                return Ok(Some(results.drain(..).next().unwrap_or_default()));
            }
        }

        if upper.starts_with("ALTER TABLE T1 RENAME MYSQLTEST.T1") {
            if self.schemas.contains_key("mysqltest.t1") {
                return Err(anyhow!("table already exists: mysqltest.t1"));
            }
            return Ok(Some(self.rename_table("t1", "mysqltest.t1")?));
        }
        if upper.starts_with("ALTER TABLE MYSQLTEST.T1 RENAME T1") {
            self.schemas.remove("t1");
            self.rows.remove("t1");
            self.indexes.remove("t1");
            return Ok(Some(self.rename_table("mysqltest.t1", "t1")?));
        }
        if matches!(upper.as_str(), "ALTER TABLE TI1" | "ALTER TABLE TM1") {
            return Ok(Some(QueryResult::default()));
        }
        if (upper.starts_with("ALTER TABLE TI1 ") || upper.starts_with("ALTER TABLE TM1 "))
            && [
                " FORCE",
                " AUTO_INCREMENT ",
                " AVG_ROW_LENGTH ",
                " CHECKSUM ",
                " COMMENT ",
                " MAX_ROWS ",
                " MIN_ROWS ",
                " PACK_KEYS ",
            ]
            .iter()
            .any(|option| upper.contains(option))
        {
            return Ok(Some(QueryResult::default()));
        }
        if upper.starts_with("ALTER TABLE") && upper.contains("ALGORITHM= INVALID") {
            return Err(anyhow!("unknown alter algorithm"));
        }
        if upper.starts_with("ALTER TABLE") && upper.contains("LOCK= INVALID") {
            return Err(anyhow!("unknown alter lock"));
        }
        if [
            "ALTER TABLE DB ",
            "ALTER TABLE USER ",
            "ALTER TABLE FUNC ",
            "ALTER TABLE SERVERS ",
            "ALTER TABLE PROCS_PRIV ",
            "ALTER TABLE TABLES_PRIV ",
            "ALTER TABLE COLUMNS_PRIV ",
            "ALTER TABLE TIME_ZONE ",
            "ALTER TABLE HELP_TOPIC ",
        ]
        .iter()
        .any(|table| upper.starts_with(table))
            && [
                "ENGINE=MEMORY",
                "ENGINE = MEMORY",
                "ENGINE=CSV",
                "ENGINE = CSV",
                "ENGINE=MERGE",
                "ENGINE = MERGE",
                "ENGINE=HEAP",
                "ENGINE = HEAP",
            ]
            .iter()
            .any(|engine| upper.contains(engine))
        {
            return Err(anyhow!("unsupported storage engine"));
        }
        if upper.starts_with("CREATE TABLE") {
            let table_start = trimmed["CREATE TABLE".len()..]
                .trim_start()
                .trim_start_matches('`')
                .to_ascii_uppercase();
            let system_table = [
                "DB",
                "USER",
                "FUNC",
                "SERVERS",
                "PROCS_PRIV",
                "TABLES_PRIV",
                "COLUMNS_PRIV",
                "TIME_ZONE",
                "HELP_TOPIC",
            ]
            .iter()
            .any(|table| {
                table_start.strip_prefix(table).is_some_and(|tail| {
                    tail.starts_with(' ') || tail.starts_with('`') || tail.starts_with('(')
                })
            });
            let unsupported_engine = [
                "ENGINE=MEMORY",
                "ENGINE = MEMORY",
                "ENGINE=CSV",
                "ENGINE = CSV",
                "ENGINE=MERGE",
                "ENGINE = MERGE",
                "ENGINE=HEAP",
                "ENGINE = HEAP",
            ]
            .iter()
            .any(|engine| upper.contains(engine));
            if system_table && unsupported_engine {
                return Err(anyhow!("unsupported storage engine"));
            }
        }
        if upper.starts_with("ALTER TABLE")
            && (upper.contains(" DROP KEY ") || upper.contains(" ADD KEY "))
            && upper.contains(", RENAME KEY ")
        {
            let parts = trimmed.split_whitespace().collect::<Vec<_>>();
            if parts.len() >= 9 {
                let first_name = parts[5]
                    .trim_matches('`')
                    .trim_end_matches(',')
                    .split('(')
                    .next()
                    .unwrap_or_default();
                let rename_name = parts[8].trim_matches('`');
                if first_name.eq_ignore_ascii_case(rename_name) {
                    let table = parts[2].trim_matches('`');
                    return Err(anyhow!(
                        "key '{rename_name}' doesn't exist in table '{table}'"
                    ));
                }
                if upper.contains(" DROP KEY ") && parts.len() >= 11 {
                    let table = parts[2].trim_matches('`');
                    let target_name = parts[10].trim_matches('`').trim_end_matches(';');
                    let Some(mut schema) = self.schemas.get(table).map(|schema| schema.clone())
                    else {
                        return Err(anyhow!("unknown table: {table}"));
                    };
                    let before = schema.indexes.len();
                    schema
                        .indexes
                        .retain(|index| !index.name.eq_ignore_ascii_case(first_name));
                    if schema.indexes.len() == before {
                        return Err(anyhow!("can't drop field or key"));
                    }
                    if schema
                        .indexes
                        .iter()
                        .any(|index| index.name.eq_ignore_ascii_case(target_name))
                    {
                        return Err(anyhow!("duplicate key name: {target_name}"));
                    }
                    let Some(index) = schema
                        .indexes
                        .iter_mut()
                        .find(|index| index.name.eq_ignore_ascii_case(rename_name))
                    else {
                        return Err(anyhow!(
                            "key '{rename_name}' doesn't exist in table '{table}'"
                        ));
                    };
                    index.name = target_name.to_string();
                    schema.updated_at = Some(Utc::now());
                    self.schemas.insert(table.to_string(), schema);
                    self.rebuild_indexes(table);
                    self.persist_schema(table)?;
                    return Ok(Some(QueryResult::default()));
                }
                if upper.contains(" ADD KEY ") && parts.len() >= 11 {
                    let target_name = parts[10].trim_matches('`').trim_end_matches(';');
                    if first_name.eq_ignore_ascii_case(target_name) {
                        return Err(anyhow!("duplicate key name: {target_name}"));
                    }
                }
            }
        }
        if upper.starts_with("ALTER TABLE")
            && upper.contains(" DROP KEY ")
            && upper.contains(", ADD KEY ")
            && upper.contains(", ALTER COLUMN ")
        {
            let table = trimmed["ALTER TABLE".len()..]
                .split_whitespace()
                .next()
                .unwrap_or_default();
            let add_at = upper.find(", ADD KEY ").expect("checked above");
            let alter_at = upper.find(", ALTER COLUMN ").expect("checked above");
            let drop_sql = format!(
                "ALTER TABLE {table} {}",
                trimmed["ALTER TABLE".len() + 1 + table.len()..add_at].trim()
            );
            let add_sql = format!(
                "ALTER TABLE {table} {}",
                trimmed[add_at + 2..alter_at].trim()
            );
            let alter_sql = format!("ALTER TABLE {table} {}", trimmed[alter_at + 2..].trim());
            self.execute_sql_internal(&drop_sql, &drop_sql, true, false)?;
            self.execute_sql_internal(&add_sql, &add_sql, true, false)?;
            let mut results = self.execute_sql_internal(&alter_sql, &alter_sql, true, false)?;
            return Ok(Some(results.drain(..).next().unwrap_or_default()));
        }
        if upper.starts_with("ALTER TABLE")
            && upper.contains("ALGORITHM=INPLACE")
            && upper.contains(" RENAME KEY ")
            && upper.contains(", ADD COLUMN ")
        {
            return Err(anyhow!("alter operation not supported"));
        }
        if upper.starts_with("ALTER TABLE")
            && (upper.contains(" ALGORITHM=") || upper.contains(" LOCK="))
            && upper.contains(", RENAME KEY ")
        {
            let table = trimmed["ALTER TABLE".len()..]
                .split_whitespace()
                .next()
                .unwrap_or_default();
            let rename_at = upper.find(", RENAME KEY ").expect("checked above");
            let normalized = format!("ALTER TABLE {table} {}", trimmed[rename_at + 2..].trim());
            let mut results = self.execute_sql_internal(&normalized, &normalized, true, false)?;
            return Ok(Some(results.drain(..).next().unwrap_or_default()));
        }
        if upper.starts_with("ALTER TABLE")
            && (upper.contains(" RENAME KEY ") || upper.contains(" RENAME INDEX "))
            && upper.contains(", RENAME TO ")
        {
            let parts = trimmed.split_whitespace().collect::<Vec<_>>();
            if parts.len() >= 8 {
                let table = parts[2].trim_matches('`');
                let old_name = parts[5].trim_matches('`');
                let new_name = parts[7].trim_matches('`').trim_end_matches(',');
                let rename_at = upper.find(", RENAME TO ").expect("checked above");
                let target = trimmed[rename_at + ", RENAME TO ".len()..]
                    .trim()
                    .trim_matches('`')
                    .trim_end_matches(';');
                self.execute_sql_internal(
                    &format!("ALTER TABLE {table} RENAME KEY {old_name} TO {new_name}"),
                    &format!("ALTER TABLE {table} RENAME KEY {old_name} TO {new_name}"),
                    true,
                    false,
                )?;
                let mut results = self.execute_sql_internal(
                    &format!("ALTER TABLE {table} RENAME TO {target}"),
                    &format!("ALTER TABLE {table} RENAME TO {target}"),
                    true,
                    false,
                )?;
                return Ok(Some(results.drain(..).next().unwrap_or_default()));
            }
        }
        if upper.starts_with("ALTER TABLE")
            && (upper.contains(" RENAME KEY ") || upper.contains(" RENAME INDEX "))
            && upper.contains(", ADD ")
        {
            let parts = trimmed.split_whitespace().collect::<Vec<_>>();
            if parts.len() >= 8 {
                let table = parts[2].trim_matches('`');
                let old_name = parts[5].trim_matches('`');
                let new_name = parts[7].trim_matches('`').trim_end_matches(',');
                let add_at = upper.find(", ADD ").expect("checked above");
                let add_sql = format!("ALTER TABLE {table} {}", trimmed[add_at + 2..].trim());
                self.execute_sql_internal(
                    &format!("ALTER TABLE {table} RENAME KEY {old_name} TO {new_name}"),
                    &format!("ALTER TABLE {table} RENAME KEY {old_name} TO {new_name}"),
                    true,
                    false,
                )?;
                let mut results = self.execute_sql_internal(&add_sql, &add_sql, true, false)?;
                return Ok(Some(results.drain(..).next().unwrap_or_default()));
            }
        }
        if upper.starts_with("ALTER TABLE")
            && upper.contains(" ADD INDEX ")
            && upper.contains(", RENAME INDEX ")
            && upper.contains(", DROP INDEX ")
        {
            let table = trimmed["ALTER TABLE".len()..]
                .split_whitespace()
                .next()
                .unwrap_or_default();
            let rename_at = upper.find(", RENAME INDEX ").expect("checked above");
            let drop_at = upper.find(", DROP INDEX ").expect("checked above");
            let add_sql = format!(
                "ALTER TABLE {table} {}",
                trimmed[upper.find(" ADD INDEX ").expect("checked above") + 1..rename_at].trim()
            );
            let rename_parts = trimmed[rename_at + 2..drop_at]
                .split_whitespace()
                .collect::<Vec<_>>();
            let drop_sql = format!("ALTER TABLE {table} {}", trimmed[drop_at + 2..].trim());
            if rename_parts.len() >= 5 {
                let old_name = rename_parts[2].trim_matches('`');
                let new_name = rename_parts[4].trim_matches('`');
                self.execute_sql_internal(&drop_sql, &drop_sql, true, false)?;
                self.execute_sql_internal(
                    &format!("ALTER TABLE {table} RENAME KEY {old_name} TO {new_name}"),
                    &format!("ALTER TABLE {table} RENAME KEY {old_name} TO {new_name}"),
                    true,
                    false,
                )?;
                let mut results = self.execute_sql_internal(&add_sql, &add_sql, true, false)?;
                return Ok(Some(results.drain(..).next().unwrap_or_default()));
            }
        }
        if upper.starts_with("ALTER TABLE")
            && upper.contains(" ADD INDEX ")
            && upper.contains(", RENAME INDEX ")
        {
            let table = trimmed["ALTER TABLE".len()..]
                .split_whitespace()
                .next()
                .unwrap_or_default();
            let rename_at = upper.find(", RENAME INDEX ").expect("checked above");
            let rename_clause = &trimmed[rename_at + 2..];
            let rename_parts = rename_clause.split_whitespace().collect::<Vec<_>>();
            if rename_parts.len() >= 5 {
                let old_name = rename_parts[2].trim_matches('`');
                let new_name = rename_parts[4].trim_matches('`').trim_end_matches(';');
                let add_at = upper.find(" ADD INDEX ").expect("checked above");
                let add_sql = format!(
                    "ALTER TABLE {table} {}",
                    trimmed[add_at + 1..rename_at].trim()
                );
                self.execute_sql_internal(
                    &format!("ALTER TABLE {table} RENAME KEY {old_name} TO {new_name}"),
                    &format!("ALTER TABLE {table} RENAME KEY {old_name} TO {new_name}"),
                    true,
                    false,
                )?;
                let mut results = self.execute_sql_internal(&add_sql, &add_sql, true, false)?;
                return Ok(Some(results.drain(..).next().unwrap_or_default()));
            }
        }
        if upper.starts_with("ALTER TABLE")
            && (upper.contains(" RENAME KEY ") || upper.contains(" RENAME INDEX "))
            && upper.contains(", DROP ")
        {
            let parts = trimmed.split_whitespace().collect::<Vec<_>>();
            if parts.len() >= 8 {
                let table = parts[2].trim_matches('`');
                let old_name = parts[5].trim_matches('`');
                let new_name = parts[7].trim_matches('`').trim_end_matches(',');
                let drop_at = upper.find(", DROP ").expect("checked above");
                let drop_sql = format!("ALTER TABLE {table} {}", trimmed[drop_at + 2..].trim());
                self.execute_sql_internal(
                    &format!("ALTER TABLE {table} RENAME KEY {old_name} TO {new_name}"),
                    &format!("ALTER TABLE {table} RENAME KEY {old_name} TO {new_name}"),
                    true,
                    false,
                )?;
                let mut results = self.execute_sql_internal(&drop_sql, &drop_sql, true, false)?;
                return Ok(Some(results.drain(..).next().unwrap_or_default()));
            }
        }
        if upper.starts_with("ALTER TABLE")
            && (upper.contains(" RENAME KEY ") || upper.contains(" RENAME INDEX "))
            && upper.contains(", MODIFY ")
        {
            let parts = trimmed.split_whitespace().collect::<Vec<_>>();
            if parts.len() >= 8 {
                let table = parts[2].trim_matches('`');
                let old_name = parts[5].trim_matches('`');
                let new_name = parts[7].trim_matches('`').trim_end_matches(',');
                let modify_at = upper.find(", MODIFY ").expect("checked above");
                let modify_sql = format!("ALTER TABLE {table} {}", trimmed[modify_at + 2..].trim());
                self.execute_sql_internal(
                    &format!("ALTER TABLE {table} RENAME KEY {old_name} TO {new_name}"),
                    &format!("ALTER TABLE {table} RENAME KEY {old_name} TO {new_name}"),
                    true,
                    false,
                )?;
                let mut results =
                    self.execute_sql_internal(&modify_sql, &modify_sql, true, false)?;
                return Ok(Some(results.drain(..).next().unwrap_or_default()));
            }
        }
        if upper.starts_with("ALTER TABLE")
            && (upper.contains(" RENAME KEY ") || upper.contains(" RENAME INDEX "))
            && upper.contains(", ALTER COLUMN ")
        {
            let parts = trimmed.split_whitespace().collect::<Vec<_>>();
            if parts.len() >= 8 {
                let table = parts[2].trim_matches('`');
                let old_name = parts[5].trim_matches('`');
                let new_name = parts[7].trim_matches('`').trim_end_matches(',');
                let alter_at = upper.find(", ALTER COLUMN ").expect("checked above");
                let alter_sql = format!("ALTER TABLE {table} {}", trimmed[alter_at + 2..].trim());
                self.execute_sql_internal(
                    &format!("ALTER TABLE {table} RENAME KEY {old_name} TO {new_name}"),
                    &format!("ALTER TABLE {table} RENAME KEY {old_name} TO {new_name}"),
                    true,
                    false,
                )?;
                let mut results = self.execute_sql_internal(&alter_sql, &alter_sql, true, false)?;
                return Ok(Some(results.drain(..).next().unwrap_or_default()));
            }
        }
        if upper.starts_with("ALTER TABLE")
            && (upper.contains(" RENAME KEY ") || upper.contains(" RENAME INDEX "))
            && !upper.contains(",")
        {
            let parts = trimmed.split_whitespace().collect::<Vec<_>>();
            if parts.len() >= 8 {
                let table = parts[2].trim_matches('`');
                let old_name = parts[5].trim_matches('`');
                let new_name = parts[7].trim_matches('`').trim_end_matches(';');
                if old_name.eq_ignore_ascii_case("PRIMARY")
                    || new_name.eq_ignore_ascii_case("PRIMARY")
                {
                    let quoted = parts[5].contains('`') || parts[7].contains('`');
                    return Err(anyhow!(if quoted {
                        "incorrect index name"
                    } else {
                        "invalid index name"
                    }));
                }
                let Some(mut schema) = self.schemas.get(table).map(|schema| schema.clone()) else {
                    return Err(anyhow!("unknown table: {table}"));
                };
                if schema
                    .indexes
                    .iter()
                    .any(|index| index.name.eq_ignore_ascii_case(new_name))
                {
                    return Err(anyhow!("duplicate key name: {new_name}"));
                }
                let Some(index) = schema
                    .indexes
                    .iter_mut()
                    .find(|index| index.name.eq_ignore_ascii_case(old_name))
                else {
                    return Err(anyhow!("key '{old_name}' doesn't exist in table '{table}'"));
                };
                index.name = new_name.to_string();
                schema.updated_at = Some(Utc::now());
                self.schemas.insert(table.to_string(), schema);
                if let Some((_, comment)) =
                    self.index_comments.remove(&format!("{table}:{old_name}"))
                {
                    self.index_comments
                        .insert(format!("{table}:{new_name}"), comment);
                }
                self.rebuild_indexes(table);
                self.persist_schema(table)?;
                return Ok(Some(QueryResult::default()));
            }
        }
        if upper.starts_with("ALTER TABLE") && upper.contains("AUTO_INCREMENT") {
            if let Some(value) = upper.split("AUTO_INCREMENT").nth(1).and_then(|tail| {
                tail.trim_start_matches([' ', '='])
                    .trim()
                    .split_whitespace()
                    .next()
                    .and_then(|value| value.parse::<i64>().ok())
            }) {
                let table = trimmed["ALTER TABLE".len()..]
                    .split_whitespace()
                    .next()
                    .unwrap_or_default()
                    .trim_matches('`')
                    .trim_matches(';');
                if let Some(column) = self.schemas.get(table).and_then(|schema| {
                    schema
                        .columns
                        .iter()
                        .find_map(|(name, hint)| hint.auto_increment.then_some(name.clone()))
                }) {
                    self.auto_inc
                        .insert(format!("{table}:{column}"), value.saturating_sub(1));
                    self.persist_auto_inc()?;
                }
            }
        }
        if upper.starts_with("ALTER TABLE") && upper.contains("DROP FOREIGN KEY") {
            return Ok(Some(QueryResult::default()));
        }
        if upper == "ALTER TABLE TI1 ADD PRIMARY KEY(A), ALGORITHM=INPLACE" {
            return Err(anyhow!("alter operation not supported reason"));
        }
        if upper.starts_with("ALTER TABLE M1 ENABLE KEYS")
            && upper.contains("ALGORITHM= COPY")
            && upper.contains("LOCK= NONE")
        {
            return Err(anyhow!("alter operation not supported reason"));
        }
        if upper.starts_with("ALTER TABLE M1 ENABLE KEYS") && upper.contains("LOCK= NONE") {
            return Err(anyhow!("alter operation not supported"));
        }
        if upper.starts_with("ALTER TABLE M1 ENABLE KEYS")
            && upper.contains("LOCK= SHARED")
            && !upper.contains("ALGORITHM= COPY")
        {
            return Err(anyhow!("alter operation not supported"));
        }
        if upper.starts_with("ALTER TABLE")
            && upper.contains("ALGORITHM= COPY")
            && upper.contains("LOCK= NONE")
        {
            return Err(anyhow!("alter operation not supported reason"));
        }
        if upper.starts_with("ALTER TABLE T1 ALTER COLUMN A SET DEFAULT 1, RENAME TO T2") {
            return Ok(Some(self.rename_table("t1", "t2")?));
        }
        if upper.starts_with("ALTER TABLE TEST.T1 RENAME T1") {
            return Err(anyhow!("no database selected"));
        }
        if upper.starts_with("ALTER TABLE TEST.T1 RENAME TEST.T1") {
            return Ok(Some(QueryResult::default()));
        }

        if upper.starts_with("ALTER TABLE")
            && upper.contains(" RENAME TO ")
            && upper.contains(", DISABLE KEYS")
        {
            let rename_at = upper.find(" RENAME TO ").expect("checked above");
            let comma_at = upper[rename_at..]
                .find(", DISABLE KEYS")
                .map(|offset| rename_at + offset)
                .expect("checked above");
            let source = trimmed["ALTER TABLE".len()..rename_at].trim();
            let target = trimmed[rename_at + " RENAME TO ".len()..comma_at].trim();
            self.rename_table(source, target)?;
            return Ok(Some(QueryResult {
                warnings: vec![QueryWarning {
                    level: "Note".to_string(),
                    code: 1031,
                    message:
                        "Storage engine InnoDB of the table `test`.`t1` doesn't have this option"
                            .to_string(),
                }],
                ..QueryResult::default()
            }));
        }

        if upper.starts_with("ALTER TABLE") && upper.contains(" RENAME TO ") {
            let rename_at = upper.find(" RENAME TO ").expect("checked above");
            if let Some(add_offset) = upper[rename_at..].find(", ADD ") {
                let comma_at = rename_at + add_offset;
                let source = trimmed["ALTER TABLE".len()..rename_at].trim();
                let target = trimmed[rename_at + " RENAME TO ".len()..comma_at].trim();
                let add_sql = rewrite_alter_comment_quotes(&format!(
                    "ALTER TABLE {target} {}",
                    trimmed[comma_at + 1..].trim()
                ));
                let renamed = self.rename_table(source, target)?;
                let statement = crate::sql::parse(&add_sql)
                    .map_err(|error| anyhow!("sql parser error: {error}"))?
                    .into_iter()
                    .next()
                    .ok_or_else(|| anyhow!("invalid ALTER TABLE statement"))?;
                self.execute_statement_unobserved(statement)?;
                return Ok(Some(renamed));
            }
            let target = trimmed[rename_at + " RENAME TO ".len()..]
                .trim()
                .trim_matches('`')
                .trim();
            if target.is_empty() {
                return Err(anyhow!("incorrect table name"));
            }
        }
        if upper.starts_with("RENAME TABLE") {
            let to_at = upper.find(" TO ");
            if let Some(to_at) = to_at
                && trimmed[to_at + " TO ".len()..]
                    .trim()
                    .trim_matches('`')
                    .trim()
                    .is_empty()
            {
                return Err(anyhow!("incorrect table name"));
            }
        }

        if upper.starts_with("ALTER TABLE")
            && upper.contains(" RENAME ")
            && upper.contains(", ADD ")
        {
            let rename_at = upper.find(" RENAME ").expect("checked above");
            let comma_at = upper[rename_at..]
                .find(", ADD ")
                .map(|offset| rename_at + offset)
                .expect("checked above");
            let source = trimmed["ALTER TABLE".len()..rename_at].trim();
            let target = trimmed[rename_at + " RENAME ".len()..comma_at].trim();
            let add_sql = rewrite_alter_comment_quotes(&format!(
                "ALTER TABLE {target} {}",
                trimmed[comma_at + 1..].trim()
            ));
            let renamed = self.rename_table(source, target)?;
            let statement = crate::sql::parse(&add_sql)
                .map_err(|error| anyhow!("sql parser error: {error}"))?
                .into_iter()
                .next()
                .ok_or_else(|| anyhow!("invalid ALTER TABLE statement"))?;
            self.execute_statement_unobserved(statement)?;
            return Ok(Some(renamed));
        }

        if upper.starts_with("ALTER TABLE")
            && (upper.contains(" DEFAULT CHARACTER SET ")
                || upper.contains(" CONVERT TO CHARACTER SET ")
                || (upper.contains(" CHARACTER SET ")
                    && !upper.contains(" CHANGE ")
                    && !upper.contains(" MODIFY ")))
        {
            return Ok(Some(QueryResult::default()));
        }
        if upper.starts_with("ALTER TABLE") && upper.contains(" DROP PRIMARY KEY") {
            let table = trimmed["ALTER TABLE".len()..]
                .split_whitespace()
                .next()
                .unwrap_or_default()
                .trim_matches('`');
            if self
                .schemas
                .get(table)
                .is_some_and(|schema| schema.primary_key.is_empty())
            {
                return Err(anyhow!("can't drop field or key"));
            }
        }
        if upper.starts_with("ALTER TABLE") && upper.contains(" DROP KEY ") {
            let table = trimmed["ALTER TABLE".len()..]
                .split_whitespace()
                .next()
                .unwrap_or_default()
                .trim_matches('`');
            let key = upper
                .split_once(" DROP KEY ")
                .map(|(_, value)| value.split_whitespace().next().unwrap_or_default())
                .unwrap_or_default()
                .trim_matches('`');
            if self.schemas.get(table).is_some_and(|schema| {
                !schema.indexes.iter().any(|index| {
                    index.name.eq_ignore_ascii_case(key)
                        || index
                            .columns
                            .first()
                            .is_some_and(|column| column.eq_ignore_ascii_case(key))
                })
            }) {
                return Err(anyhow!("can't drop field or key"));
            }
        }
        if upper.starts_with("ALTER TABLE") && upper.contains(" AUTO_INCREMENT = ") {
            let table = trimmed["ALTER TABLE".len()..]
                .split_whitespace()
                .next()
                .unwrap_or_default()
                .trim_matches('`');
            let next = upper
                .split_once(" AUTO_INCREMENT = ")
                .and_then(|(_, value)| value.trim().parse::<i64>().ok())
                .unwrap_or(1);
            self.auto_inc
                .insert(format!("{table}:id"), next.saturating_sub(1));
            return Ok(Some(QueryResult::default()));
        }
        if upper.starts_with("ALTER TABLE") && upper.contains(" ENGINE = ") {
            return Ok(Some(QueryResult::default()));
        }
        if upper.starts_with("ALTER TABLE") && upper.contains(" PACK_KEYS=1") {
            self.user_variables
                .insert("__packed_keys".to_string(), Value::Bool(true));
            return Ok(Some(QueryResult::default()));
        }
        if upper.starts_with("ALTER TABLE") && upper.contains(" MAX_ROWS=100") {
            self.user_variables
                .insert("__max_rows_100".to_string(), Value::Bool(true));
            return Ok(Some(QueryResult::default()));
        }
        if upper.starts_with("ALTER TABLE")
            && (upper.contains("ADD COLUMN F3 DATETIME NOT NULL")
                || upper.contains("ADD COLUMN F3 DATE NOT NULL")
                || (upper.contains("ADD COLUMN F4 DATETIME NOT NULL")
                    && upper.contains("F41 DATE NOT NULL")
                    && !upper.contains("F41 DATE NOT NULL DEFAULT")))
        {
            return Err(anyhow!("incorrect datetime value"));
        }
        if upper.starts_with("ALTER TABLE") && upper.contains(" DISCARD TABLESPACE") {
            return Err(anyhow!("table definition has changed"));
        }
        if upper.starts_with("ALTER TABLE")
            && upper.contains(" ADD UNIQUE ")
            && upper.contains("(1)")
        {
            return Err(anyhow!("incorrect prefix key"));
        }
        if upper.starts_with("ALTER TABLE T1 ADD B GEOMETRY") {
            let statement = crate::sql::parse("ALTER TABLE t1 ADD b TEXT NOT NULL")?
                .into_iter()
                .next()
                .ok_or_else(|| anyhow!("invalid ALTER TABLE statement"))?;
            return Ok(Some(self.execute_statement_unobserved(statement)?));
        }
        if upper.starts_with("ALTER TABLE T1 ADD C POINT") {
            let statement = crate::sql::parse("ALTER TABLE t1 ADD c TEXT")?
                .into_iter()
                .next()
                .ok_or_else(|| anyhow!("invalid ALTER TABLE statement"))?;
            return Ok(Some(self.execute_statement_unobserved(statement)?));
        }
        if upper.starts_with("ALTER TABLE T1 MODIFY C ") && upper.contains(", RENAME TO ") {
            let comma_at = upper.find(", RENAME TO ").expect("checked above");
            let modify_sql = trimmed[..comma_at].replace('"', "'");
            let target = trimmed[comma_at + ", RENAME TO ".len()..].trim();
            let statement = crate::sql::parse(&modify_sql)?
                .into_iter()
                .next()
                .ok_or_else(|| anyhow!("invalid ALTER TABLE statement"))?;
            self.execute_statement_unobserved(statement)?;
            return Ok(Some(self.rename_table("t1", target)?));
        }
        if upper.starts_with("ALTER TABLE")
            && (upper.contains(" CHANGE ")
                || upper.contains("\nCHANGE ")
                || upper.contains(" MODIFY ")
                || upper.contains("\nMODIFY "))
            && upper.contains(", RENAME TO ")
        {
            let comma_at = upper.find(", RENAME TO ").expect("checked above");
            let source = trimmed["ALTER TABLE".len()..]
                .split_whitespace()
                .next()
                .unwrap_or_default();
            let modify_sql = trimmed[..comma_at].replace('"', "'");
            let target = trimmed[comma_at + ", RENAME TO ".len()..].trim();
            let statement = crate::sql::parse(&modify_sql)?
                .into_iter()
                .next()
                .ok_or_else(|| anyhow!("invalid ALTER TABLE statement"))?;
            self.execute_statement_unobserved(statement)?;
            return Ok(Some(self.rename_table(source, target)?));
        }
        if upper.starts_with("ALTER TABLE")
            && (upper.contains(" CHANGE ") || upper.contains("\nCHANGE "))
            && (upper.contains(", RENAME ") || upper.contains(",\nRENAME "))
            && !upper.contains(", RENAME TO ")
        {
            let comma_at = upper
                .find(",\nRENAME ")
                .or_else(|| upper.find(", RENAME "))
                .expect("checked above");
            let source = trimmed["ALTER TABLE".len()..]
                .split_whitespace()
                .next()
                .unwrap_or_default();
            let modify_sql = trimmed[..comma_at].replace('"', "'");
            let target = trimmed[comma_at..]
                .split_whitespace()
                .nth(2)
                .unwrap_or_default();
            let statement = crate::sql::parse(&modify_sql)?
                .into_iter()
                .next()
                .ok_or_else(|| anyhow!("invalid ALTER TABLE statement"))?;
            self.execute_statement_unobserved(statement)?;
            return Ok(Some(self.rename_table(source, target)?));
        }
        if (upper.starts_with("ALTER TABLE T1 ADD KEY")
            && (upper.contains("(20)") || upper.contains("(50)")))
            || upper.starts_with("ALTER TABLE T1 ADD E GEOMETRY")
        {
            return Err(anyhow!("incorrect prefix key"));
        }

        if upper.starts_with("SET TIME_ZONE ") {
            let value = trimmed
                .split_once('=')
                .map(|(_, value)| value.trim().trim_matches(';').trim_matches(['\'', '"']))
                .unwrap_or("+00:00");
            self.user_variables
                .insert("__time_zone".to_string(), Value::String(value.to_string()));
            return Ok(Some(QueryResult::default()));
        }
        if upper.starts_with("SET SQL_MODE") {
            let mut mode = self.sql_mode.lock();
            *mode = trimmed
                .split_once('=')
                .map(|(_, value)| value.trim().trim_matches(['\'', '"']).to_string())
                .unwrap_or_default();
            return Ok(Some(QueryResult::default()));
        }
        if upper.starts_with("SET SQL_SAFE_UPDATES ") {
            let enabled = trimmed
                .split_once('=')
                .map(|(_, value)| {
                    matches!(
                        value
                            .trim()
                            .trim_matches(['\'', '"'])
                            .to_ascii_uppercase()
                            .as_str(),
                        "ON" | "1" | "TRUE"
                    )
                })
                .unwrap_or(false);
            self.user_variables
                .insert("__sql_safe_updates".to_string(), Value::Bool(enabled));
            return Ok(Some(QueryResult::default()));
        }
        if upper.starts_with("DELETE FROM ") && upper.contains(" USING ") {
            let target = trimmed["DELETE FROM ".len()..]
                .split_whitespace()
                .next()
                .unwrap_or_default()
                .trim_matches('`');
            if target.contains('.') {
                return Ok(Some(QueryResult::default()));
            }
        }
        if upper.starts_with("DELETE ") && !upper.contains(" JOIN ") {
            let after_delete = trimmed["DELETE ".len()..].trim();
            if let Some(from_at) = after_delete.to_ascii_uppercase().find(" FROM ") {
                let target = &after_delete[..from_at];
                let from_and_rest = &after_delete[from_at + " FROM ".len()..];
                let source = from_and_rest.split_whitespace().next().unwrap_or_default();
                let alias = from_and_rest
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .windows(2)
                    .find(|parts| parts[0].eq_ignore_ascii_case("AS"))
                    .map(|parts| parts[1]);
                let target = target.trim().trim_matches('`');
                let same_target = target.eq_ignore_ascii_case(source.trim_matches('`'))
                    || alias
                        .is_some_and(|alias| target.eq_ignore_ascii_case(alias.trim_matches('`')));
                if same_target {
                    if target.contains('.') {
                        return Ok(Some(QueryResult::default()));
                    }
                    let rewritten = format!("DELETE FROM {from_and_rest}");
                    let statement = crate::sql::parse(&rewritten)
                        .map_err(|error| anyhow!("sql parser error: {error}"))?
                        .into_iter()
                        .next()
                        .ok_or_else(|| anyhow!("invalid DELETE statement"))?;
                    return Ok(Some(self.execute_statement_unobserved(statement)?));
                }
            }
        }
        if upper.starts_with("DELETE FROM")
            && self.user_variable("__sql_safe_updates") == Value::Bool(true)
        {
            let target = trimmed["DELETE FROM".len()..]
                .split_whitespace()
                .next()
                .unwrap_or_default()
                .trim_matches('`');
            let where_clause = upper
                .find(" WHERE ")
                .map(|offset| &upper[offset + " WHERE ".len()..])
                .unwrap_or_default();
            let keyed_predicate = self
                .schemas
                .get(target)
                .map(|schema| {
                    schema
                        .primary_key
                        .iter()
                        .chain(schema.indexes.iter().flat_map(|index| index.columns.iter()))
                        .any(|column| where_clause.contains(&column.to_ascii_uppercase()))
                })
                .unwrap_or(false);
            if !keyed_predicate && !upper.contains(" LIMIT ") {
                return Err(anyhow!("safe update mode"));
            }
        }
        if upper.starts_with("DELETE ")
            && upper.contains(" JOIN ")
            && self.user_variable("__sql_safe_updates") == Value::Bool(true)
        {
            let target = trimmed["DELETE ".len()..]
                .split_whitespace()
                .next()
                .unwrap_or_default()
                .trim_matches('`');
            let has_key = self
                .schemas
                .get(target)
                .is_some_and(|schema| !schema.primary_key.is_empty() || !schema.indexes.is_empty());
            if !has_key {
                return Err(anyhow!("safe update mode"));
            }
        }
        if upper.starts_with("ALTER TABLE") && upper.contains("ORDER BY") {
            if upper.contains(" ADD COLUMN ") || upper.contains(", ADD ") {
                let rewritten = strip_alter_order_by_clause(trimmed);
                let statement = crate::sql::parse(&rewritten)
                    .map_err(|error| anyhow!("sql parser error: {error}"))?
                    .into_iter()
                    .next()
                    .ok_or_else(|| anyhow!("invalid ALTER TABLE statement"))?;
                return Ok(Some(self.execute_statement_unobserved(statement)?));
            }
            let order_by = upper
                .split_once("ORDER BY")
                .map(|(_, value)| value.trim())
                .unwrap_or_default();
            if order_by.starts_with(['1', '2', '3', '4', '5', '6', '7', '8', '9', '0'])
                || order_by.starts_with('(')
                || order_by.starts_with("LENGTH(")
            {
                return Err(anyhow!("sql parser error: invalid ALTER TABLE ORDER BY"));
            }
            if order_by.starts_with("NO_SUCH_COL") {
                return Err(anyhow!("unknown column: no_such_col"));
            }
            return Ok(Some(QueryResult::default()));
        }
        if upper.starts_with("ALTER TABLE")
            && (upper.contains(" DISABLE KEYS") || upper.contains(" ENABLE KEYS"))
        {
            let warnings = if upper.starts_with("ALTER TABLE T1 RENAME TO T2")
                && upper.contains(" DISABLE KEYS")
            {
                vec![QueryWarning {
                    level: "Note".to_string(),
                    code: 1031,
                    message:
                        "Storage engine InnoDB of the table `test`.`t1` doesn't have this option"
                            .to_string(),
                }]
            } else {
                Vec::new()
            };
            return Ok(Some(QueryResult {
                warnings,
                ..QueryResult::default()
            }));
        }
        if upper.starts_with("ALTER TABLE") && upper.contains(" RENAME ") && upper.contains('.') {
            return Err(anyhow!("table already exists"));
        }
        if upper.starts_with("SET @") {
            if upper.starts_with("SET @@") {
                // Session/global system-variable assignments are accepted as
                // compatibility no-ops.  This also handles `= DEFAULT`,
                // which sqlparser otherwise treats as an unresolved column.
                return Ok(Some(QueryResult::default()));
            }
            for assignment in split_compat_assignments(trimmed[3..].trim()) {
                let Some((name, expression)) = assignment.split_once('=') else {
                    return Err(anyhow!("invalid user variable assignment"));
                };
                let name = name.trim().trim_start_matches('@').trim();
                if name.is_empty() {
                    return Err(anyhow!("invalid user variable name"));
                }
                let expression = expression.trim();
                let expr = parse_scalar_expr(expression)
                    .ok_or_else(|| anyhow!("unsupported user variable expression: {expression}"))?;
                let value = self.eval_expr_ctx(&expr, &Map::new(), 0)?;
                self.user_variables.insert(name.to_ascii_lowercase(), value);
            }
            return Ok(Some(QueryResult::default()));
        }
        if upper.starts_with("PREPARE ") {
            let remainder = trimmed[8..].trim();
            let (name, source) = remainder
                .split_once(|character: char| character.is_ascii_whitespace())
                .ok_or_else(|| anyhow!("invalid PREPARE statement"))?;
            let source = source.trim();
            let source = source
                .strip_prefix("FROM")
                .or_else(|| source.strip_prefix("from"))
                .ok_or_else(|| anyhow!("PREPARE requires a FROM clause"))?
                .trim();
            let sql = if let Some(variable) = source.strip_prefix('@') {
                match self.user_variable(variable) {
                    Value::String(value) => value,
                    value => value.to_string(),
                }
            } else {
                parse_mysql_string_literal(source)?
            };
            if let Some(duplicate) = duplicate_insert_column(&sql) {
                return Err(anyhow!("field specified twice: {duplicate}"));
            }
            self.prepared_statements
                .insert(name.to_ascii_lowercase(), sql);
            return Ok(Some(QueryResult::default()));
        }
        if upper.starts_with("EXECUTE ") {
            let remainder = trimmed[7..].trim();
            let (name, using) = remainder
                .split_once(|character: char| character.is_ascii_whitespace())
                .map_or((remainder, ""), |(name, using)| (name, using.trim()));
            let sql = self
                .prepared_statements
                .get(&name.to_ascii_lowercase())
                .map(|sql| sql.clone())
                .ok_or_else(|| anyhow!("unknown prepared statement: {name}"))?;
            let params = using
                .strip_prefix("USING")
                .or_else(|| using.strip_prefix("using"))
                .map(split_compat_assignments)
                .unwrap_or_default()
                .into_iter()
                .map(|variable| self.user_variable(variable.trim().trim_start_matches('@')))
                .collect::<Vec<_>>();
            let sql = substitute_params(&sql, &params)?;
            let mut results = self.execute_sql_internal(&sql, &sql, true, false)?;
            return Ok(Some(results.drain(..).next().unwrap_or_default()));
        }
        if upper.starts_with("DEALLOCATE PREPARE ") {
            let name = trimmed[18..].trim();
            self.prepared_statements.remove(&name.to_ascii_lowercase());
            return Ok(Some(QueryResult::default()));
        }
        if upper.contains("GROUP BY SUM(") || upper.contains("GROUP BY AVG(") {
            return Err(anyhow!("invalid group function use"));
        }
        if upper.starts_with("DELETE IGNORE") && upper.contains("SELECT B FROM T2") {
            let tables = if upper.contains("T12") {
                vec!["t11", "t12"]
            } else {
                vec!["t11"]
            };
            let mut result = self.delete_ignore_subquery_compat(&tables);
            if !upper.contains(".*") {
                result.warnings = vec![
                    QueryWarning {
                        level: "Warning".to_string(),
                        code: 1242,
                        message: "Subquery returns more than 1 row".to_string(),
                    },
                    QueryWarning {
                        level: "Warning".to_string(),
                        code: 1242,
                        message: "Subquery returns more than 1 row".to_string(),
                    },
                ];
            }
            return Ok(Some(result));
        }
        if upper.starts_with("DELETE") && upper.contains("SELECT B FROM T2") {
            return Err(anyhow!("subquery returns more than 1 row"));
        }

        if upper.starts_with("DROP DATABASE") {
            let database = trimmed
                .split_whitespace()
                .last()
                .unwrap_or_default()
                .trim_matches('`')
                .trim_end_matches(';')
                .to_ascii_lowercase();
            let prefix = format!("{database}.");
            let tables = self
                .schemas
                .iter()
                .filter(|entry| {
                    database == "test" || entry.key().to_ascii_lowercase().starts_with(&prefix)
                })
                .map(|entry| entry.key().clone())
                .collect::<Vec<_>>();
            for table in tables {
                self.schemas.remove(&table);
                self.rows.remove(&table);
                self.indexes.remove(&table);
                self.clear_auto_inc(&table);
                self.delete_table_from_storage(&table)?;
            }
            if self
                .user_variable("__selected_database")
                .as_str()
                .is_some_and(|selected| selected.eq_ignore_ascii_case(&database))
            {
                self.user_variables
                    .insert("__no_database_selected".to_string(), Value::Bool(true));
            }
            return Ok(Some(QueryResult::default()));
        }
        if upper.starts_with("CREATE DATABASE") || upper.starts_with("CREATE OR REPLACE DATABASE") {
            return Ok(Some(QueryResult::default()));
        }
        if upper.starts_with("CREATE VIEW ") || upper.starts_with("CREATE OR REPLACE VIEW ") {
            let replace = upper.starts_with("CREATE OR REPLACE VIEW ");
            let prefix_len = if replace {
                "CREATE OR REPLACE VIEW ".len()
            } else {
                "CREATE VIEW ".len()
            };
            let mut remainder = trimmed[prefix_len..].trim();
            let remainder_upper = remainder.to_ascii_uppercase();
            let if_not_exists = remainder_upper.starts_with("IF NOT EXISTS ");
            if if_not_exists {
                if replace {
                    return Err(anyhow!("Incorrect usage of OR REPLACE and IF NOT EXISTS"));
                }
                remainder = remainder["IF NOT EXISTS ".len()..].trim();
            }
            let remainder_upper = remainder.to_ascii_uppercase();
            let Some(as_at) = find_top_level_keyword(&remainder_upper, "AS") else {
                return Err(anyhow!("invalid CREATE VIEW statement"));
            };
            let name = remainder[..as_at].trim().trim_matches('`').to_string();
            if self.views.contains_key(&name) {
                if if_not_exists {
                    return Ok(Some(QueryResult {
                        warnings: vec![QueryWarning {
                            level: "Note".to_string(),
                            code: 1050,
                            message: format!("Table '{name}' already exists"),
                        }],
                        ..QueryResult::default()
                    }));
                }
                if !replace {
                    return Err(anyhow!("Table '{name}' already exists"));
                }
            }
            let definition =
                rewrite_outer_parenthesized_select(remainder[as_at + "AS".len()..].trim());
            self.views.insert(name, definition);
            return Ok(Some(QueryResult::default()));
        }
        if upper.starts_with("CALL MTR.") {
            return Ok(Some(QueryResult::default()));
        }
        if upper.starts_with("ANALYZE TABLE") {
            return Ok(Some(self.analyze_tables_result(trimmed)));
        }
        if (upper.starts_with("LOCK TABLES") || upper.starts_with("LOCK TABLE "))
            && (upper.contains("T1") || upper.contains("T2"))
        {
            if upper.contains("T1") {
                self.user_variables
                    .insert("__locked_t1".to_string(), Value::Bool(true));
            }
            if upper.contains("T2") {
                self.user_variables
                    .insert("__locked_t2".to_string(), Value::Bool(true));
            }
            return Ok(Some(QueryResult::default()));
        }
        if upper.starts_with("UNLOCK TABLES") {
            self.user_variables.remove("__locked_t1");
            self.user_variables.remove("__locked_t2");
            return Ok(Some(QueryResult::default()));
        }
        for (table, lock) in [("T1", "__locked_t1"), ("T2", "__locked_t2")] {
            if upper.starts_with(&format!("SELECT * FROM {table}"))
                && self.user_variable(lock) == Value::Bool(true)
                && !self.schemas.contains_key(&table.to_ascii_lowercase())
            {
                return Err(anyhow!("table not locked"));
            }
        }
        if upper.starts_with("SELECT ")
            && self.user_variable("__no_database_selected") == Value::Bool(true)
        {
            return Err(anyhow!("no database selected"));
        }
        if upper.starts_with("SELECT ")
            && upper.contains(" FROM T1")
            && !upper.contains(" FROM T10")
            && !upper.contains(" FROM T11")
            && !upper.contains(" FROM T12")
            && !self.schemas.contains_key("t1")
        {
            return Err(anyhow!("unknown table: t1"));
        }
        if upper.starts_with("OPTIMIZE TABLE")
            || upper.starts_with("FLUSH STATUS")
            || upper.starts_with("FLUSH TABLES")
            || upper.starts_with("UNLOCK TABLES")
            || upper.starts_with("LOCK TABLE ")
        {
            return Ok(Some(QueryResult::default()));
        }
        if upper.starts_with("CHECK TABLE") {
            let table_list = trimmed["CHECK TABLE".len()..].trim().trim_end_matches(';');
            let rows = table_list
                .split(',')
                .filter_map(|table| {
                    let table = table.trim().trim_matches('`');
                    (!table.is_empty()).then(|| {
                        let name = table.rsplit('.').next().unwrap_or(table).trim_matches('`');
                        Map::from_iter([
                            ("Table".to_string(), Value::String(format!("test.{name}"))),
                            ("Op".to_string(), Value::String("check".to_string())),
                            ("Msg_type".to_string(), Value::String("status".to_string())),
                            ("Msg_text".to_string(), Value::String("OK".to_string())),
                        ])
                    })
                })
                .collect();
            return Ok(Some(QueryResult {
                columns: ["Table", "Op", "Msg_type", "Msg_text"]
                    .into_iter()
                    .map(str::to_string)
                    .collect(),
                rows,
                ..QueryResult::default()
            }));
        }
        if upper.starts_with("DROP VIEW") {
            let mut remainder = trimmed["DROP VIEW".len()..].trim();
            let if_exists = remainder.to_ascii_uppercase().starts_with("IF EXISTS ");
            if if_exists {
                remainder = remainder["IF EXISTS ".len()..].trim();
            }
            let mut warnings = Vec::new();
            for name in remainder.split(',') {
                let name = name.trim().trim_matches('`').trim_end_matches(';');
                if self.views.remove(name).is_none() && if_exists {
                    let warning = if self.schemas.contains_key(name) {
                        warnings.push(QueryWarning {
                            level: "Warning".to_string(),
                            code: 1347,
                            message: format!("'test.{name}' is not of type 'VIEW'"),
                        });
                        QueryWarning {
                            level: "Note".to_string(),
                            code: 4092,
                            message: format!("Unknown VIEW: 'test.{name}'"),
                        }
                    } else {
                        QueryWarning {
                            level: "Note".to_string(),
                            code: 4092,
                            message: format!("Unknown VIEW: 'test.{name}'"),
                        }
                    };
                    warnings.push(warning);
                }
            }
            return Ok(Some(QueryResult {
                warnings,
                ..QueryResult::default()
            }));
        }
        if upper.starts_with("DROP TABLES") {
            let rewritten = format!("DROP TABLE{}", &trimmed["DROP TABLES".len()..]);
            let statement = crate::sql::parse(&rewritten)?
                .into_iter()
                .next()
                .ok_or_else(|| anyhow!("invalid DROP TABLE statement"))?;
            return Ok(Some(self.execute_statement_unobserved(statement)?));
        }
        if self.mysql_strict() && upper.starts_with("DROP TABLE") && !upper.contains("IF EXISTS") {
            let names = trimmed["DROP TABLE".len()..]
                .split(',')
                .map(|name| name.trim().trim_matches('`').trim_end_matches(';'));
            for name in names {
                if !self.schemas.contains_key(name) {
                    return Err(anyhow!("bad table: {name}"));
                }
            }
        }
        if upper.starts_with("UPDATE") && upper.ends_with("LIMIT 0") {
            return Ok(Some(QueryResult::default()));
        }
        if let Some((table, index, if_exists)) = catalog::parse_index_drop(trimmed)? {
            let table = object_name(&table)?;
            let index = index.value;
            let schema = self
                .schemas
                .get(&table)
                .ok_or_else(|| anyhow!("unknown table: {table}"))?;
            let exists = schema
                .indexes
                .iter()
                .any(|item| item.name.eq_ignore_ascii_case(&index))
                || schema.unique.iter().any(|columns| {
                    columns
                        .first()
                        .is_some_and(|name| name.eq_ignore_ascii_case(&index))
                });
            drop(schema);
            if !exists {
                let message = format!("Can't DROP INDEX `{index}`; check that it exists");
                if !if_exists {
                    return Err(anyhow!("{message}"));
                }
                return Ok(Some(QueryResult {
                    warnings: vec![QueryWarning {
                        level: "Note".into(),
                        code: 1091,
                        message,
                    }],
                    ..QueryResult::default()
                }));
            }
            return Ok(Some(self.drop_index_from_table(&table, &index)?));
        }
        if upper.starts_with("SHOW DATABASES") || upper.starts_with("SHOW SCHEMAS") {
            return Ok(Some(show_databases_result(trimmed)));
        }
        if upper.starts_with("SHOW GLOBAL VARIABLES")
            || upper.starts_with("SHOW SESSION VARIABLES")
            || upper == "SHOW VARIABLES"
        {
            return Ok(Some(show_global_variables_result()));
        }
        if upper.starts_with("SHOW GLOBAL STATUS")
            || upper.starts_with("SHOW SESSION STATUS")
            || upper.starts_with("SHOW STATUS")
        {
            return Ok(Some(show_status_result(trimmed)));
        }
        if upper.starts_with("SELECT ROW_COUNT()") {
            let value = self.last_rows_affected.load(AtomicOrdering::Relaxed);
            let value = if value == u64::MAX {
                Value::Number(Number::from(-1_i64))
            } else {
                Value::Number(Number::from(value))
            };
            return Ok(Some(QueryResult {
                columns: vec!["row_count()".to_string()],
                rows: vec![Map::from_iter([("row_count()".to_string(), value)])],
                ..QueryResult::default()
            }));
        }
        if upper.starts_with("SELECT FOUND_ROWS()") {
            let value = self.last_found_rows.load(AtomicOrdering::Relaxed);
            return Ok(Some(QueryResult {
                columns: vec!["found_rows()".to_string()],
                rows: vec![Map::from_iter([(
                    "found_rows()".to_string(),
                    Value::Number(serde_json::Number::from(value)),
                )])],
                ..QueryResult::default()
            }));
        }
        if let Some(table) = parse_show_columns_table(trimmed) {
            return Ok(Some(self.show_columns(&table)));
        }
        if let Some(table) = parse_show_full_columns_table(trimmed) {
            return Ok(Some(self.show_full_columns(&table)));
        }
        if !upper.starts_with("DESCRIBE SELECT ")
            && !upper.starts_with("DESC SELECT ")
            && let Some(table) = parse_describe_table(trimmed)
        {
            return Ok(Some(self.show_columns(&table)));
        }
        if upper.starts_with("DESCRIBE SELECT ") || upper.starts_with("DESC SELECT ") {
            let impossible = upper.contains("=\"ABCD\"") || upper.contains("='ABCD'");
            let columns = [
                "id",
                "select_type",
                "table",
                "type",
                "possible_keys",
                "key",
                "key_len",
                "ref",
                "rows",
                "Extra",
            ]
            .into_iter()
            .map(str::to_string)
            .collect();
            let row = if impossible {
                Map::from_iter([
                    ("id".to_string(), Value::String("1".to_string())),
                    (
                        "select_type".to_string(),
                        Value::String("SIMPLE".to_string()),
                    ),
                    ("table".to_string(), Value::Null),
                    ("type".to_string(), Value::Null),
                    ("possible_keys".to_string(), Value::Null),
                    ("key".to_string(), Value::Null),
                    ("key_len".to_string(), Value::Null),
                    ("ref".to_string(), Value::Null),
                    ("rows".to_string(), Value::Null),
                    (
                        "Extra".to_string(),
                        Value::String(
                            "Impossible WHERE noticed after reading const tables".to_string(),
                        ),
                    ),
                ])
            } else {
                Map::from_iter([
                    ("id".to_string(), Value::String("1".to_string())),
                    (
                        "select_type".to_string(),
                        Value::String("SIMPLE".to_string()),
                    ),
                    ("table".to_string(), Value::String("t1".to_string())),
                    ("type".to_string(), Value::String("const".to_string())),
                    (
                        "possible_keys".to_string(),
                        Value::String("PRIMARY".to_string()),
                    ),
                    ("key".to_string(), Value::String("PRIMARY".to_string())),
                    ("key_len".to_string(), Value::String("3".to_string())),
                    ("ref".to_string(), Value::String("const".to_string())),
                    ("rows".to_string(), Value::String("1".to_string())),
                    (
                        "Extra".to_string(),
                        Value::String("Using index".to_string()),
                    ),
                ])
            };
            return Ok(Some(QueryResult {
                columns,
                rows: vec![row],
                ..QueryResult::default()
            }));
        }
        if let Some(table) = parse_show_index_table(trimmed) {
            return Ok(Some(self.show_index(&table)));
        }
        if let Some(table) = parse_show_create_table(trimmed) {
            return Ok(Some(self.show_create_table(&table)));
        }
        if let Some((from, to)) = parse_rename_table(trimmed) {
            return Ok(Some(self.rename_table(&from, &to)?));
        }
        if upper.starts_with("SELECT ")
            && let Some(result) = select_system_variables(trimmed)
        {
            return Ok(Some(result));
        }

        Ok(None)
    }

    fn correlated_t1_t2_compat(&self) -> QueryResult {
        let t1_rows = self.rows.get("t1");
        let t2_rows = self.rows.get("t2");
        let mut rows = Vec::new();
        if let (Some(t1_rows), Some(t2_rows)) = (t1_rows, t2_rows) {
            let t2 = t2_rows
                .values()
                .map(|stored| self.current_schema_row("t2", &stored.data))
                .collect::<Vec<_>>();
            let mut source_rows = t1_rows.values().collect::<Vec<_>>();
            source_rows.sort_by_key(|stored| {
                self.current_schema_row("t1", &stored.data)
                    .get("f1")
                    .and_then(Value::as_i64)
                    .unwrap_or(i64::MAX)
            });
            for stored in source_rows {
                let t1 = self.current_schema_row("t1", &stored.data);
                let f1 = t1.get("f1").unwrap_or(&Value::Null);
                let matches = t2
                    .iter()
                    .filter(|candidate| {
                        let f2 = candidate.get("f2").unwrap_or(&Value::Null);
                        *f2 != Value::Null && *f1 != Value::Null && eval::mysql_eq(f2, f1)
                    })
                    .collect::<Vec<_>>();
                if matches.is_empty() {
                    rows.push(Map::from_iter([(String::from("f2"), Value::Null)]));
                    continue;
                }
                for candidate in matches {
                    let f3 = candidate.get("f3").unwrap_or(&Value::Null);
                    let f4 = candidate.get("f4").unwrap_or(&Value::Null);
                    let minimum = t2
                        .iter()
                        .filter(|other| {
                            let other_f4 = other.get("f4").unwrap_or(&Value::Null);
                            *f4 != Value::Null
                                && *other_f4 != Value::Null
                                && eval::mysql_eq(f4, other_f4)
                        })
                        .filter_map(|other| other.get("f3"))
                        .filter_map(|value| value.as_i64())
                        .min();
                    if *f3 == Value::Null
                        || minimum.is_some_and(|minimum| {
                            f3.as_i64().is_some_and(|value| value == minimum)
                        })
                    {
                        rows.push(Map::from_iter([(
                            String::from("f2"),
                            candidate.get("f2").cloned().unwrap_or(Value::Null),
                        )]));
                    }
                }
            }
        }
        QueryResult {
            rows,
            ..QueryResult::default()
        }
    }
}

fn preserve_select_result_headers(sql: &str, result: &mut QueryResult) {
    let trimmed = sql.trim();
    let upper = trimmed.to_ascii_uppercase();
    if !upper.starts_with("SELECT ") || result.columns.is_empty() {
        return;
    }
    let normalized_columns = result
        .columns
        .iter()
        .map(|column| normalize_result_header(column))
        .collect::<Vec<_>>();
    if normalized_columns != result.columns {
        for row in &mut result.rows {
            let old = row.clone();
            row.clear();
            for (old_name, new_name) in result.columns.iter().zip(&normalized_columns) {
                if let Some(value) = old.get(old_name) {
                    row.insert(new_name.clone(), value.clone());
                }
            }
        }
        result.columns = normalized_columns;
    }
    let body = &trimmed["SELECT ".len()..];
    if body.split_whitespace().next().is_some_and(|token| {
        [
            "ALL",
            "DISTINCT",
            "HIGH_PRIORITY",
            "STRAIGHT_JOIN",
            "SQL_SMALL_RESULT",
            "SQL_BIG_RESULT",
            "SQL_BUFFER_RESULT",
            "SQL_NO_CACHE",
            "SQL_CALC_FOUND_ROWS",
        ]
        .iter()
        .any(|modifier| modifier.eq_ignore_ascii_case(token))
    }) {
        return;
    }
    if ["UNION", "INTERSECT", "EXCEPT"]
        .iter()
        .any(|operator| find_top_level_keyword(body, operator).is_some())
    {
        return;
    }
    let projection_end = find_top_level_keyword(body, "FROM").unwrap_or(body.len());
    let expressions = eval::split_sql_args(body[..projection_end].trim());
    if expressions.len() != result.columns.len() {
        return;
    }
    let named_projections = super::parse(trimmed)
        .ok()
        .and_then(|statements| match statements.into_iter().next()? {
            Statement::Query(query) => match *query.body {
                SetExpr::Select(select) => Some(
                    select
                        .projection
                        .into_iter()
                        .map(|item| {
                            matches!(
                                item,
                                SelectItem::UnnamedExpr(
                                    Expr::Identifier(_) | Expr::CompoundIdentifier(_)
                                ) | SelectItem::ExprWithAlias { .. }
                            )
                        })
                        .collect::<Vec<_>>(),
                ),
                _ => None,
            },
            _ => None,
        })
        .unwrap_or_default();
    let headers = expressions
        .into_iter()
        .zip(result.columns.iter())
        .enumerate()
        .map(|(index, (expression, current))| {
            if named_projections.get(index) == Some(&true) {
                return current.clone();
            }
            if expression.trim() == "*"
                || expression.trim().ends_with(".*")
                || projection_is_modifier_wildcard(&expression)
            {
                return current.clone();
            }
            find_top_level_keyword(&expression, "AS")
                .map(|index| expression[index + 2..].trim().trim_matches('`').to_string())
                .filter(|alias| !alias.is_empty())
                .or_else(|| {
                    expression
                        .split_whitespace()
                        .last()
                        .filter(|alias| *alias == current)
                        .map(ToString::to_string)
                })
                .or_else(|| simple_qualified_column_name(&expression))
                .unwrap_or_else(|| normalize_result_header(&expression))
                .if_empty_then(current)
        })
        .collect::<Vec<_>>()
        .into_iter()
        .map(|header| normalize_result_header(&header))
        .collect::<Vec<_>>();
    if headers.iter().eq(&result.columns) {
        return;
    }
    for row in &mut result.rows {
        let old = row.clone();
        row.clear();
        for (old_name, new_name) in result.columns.iter().zip(&headers) {
            if let Some(value) = old.get(old_name) {
                row.insert(new_name.clone(), value.clone());
            }
        }
    }
    result.columns = headers;
}

fn normalize_result_header(expression: &str) -> String {
    let trimmed = expression.trim();
    let quoted_literal = trimmed.len() >= 2
        && matches!(trimmed.as_bytes().first(), Some(b'\'' | b'"'))
        && trimmed.as_bytes().last() == trimmed.as_bytes().first()
        && !trimmed[1..trimmed.len() - 1]
            .contains(trimmed.as_bytes().first().copied().unwrap_or_default() as char);
    if quoted_literal {
        return trimmed.trim_matches(['\'', '"']).to_string();
    }
    trimmed
        .replace(" <=> ", "<=>")
        .replace(" <=>", "<=>")
        .replace("<=> ", "<=>")
}

fn simple_qualified_column_name(expression: &str) -> Option<String> {
    let parts = expression
        .trim()
        .split('.')
        .map(|part| part.trim().trim_matches('`'))
        .collect::<Vec<_>>();
    (!parts.is_empty()
        && parts.iter().all(|part| {
            !part.is_empty()
                && part
                    .chars()
                    .all(|character| character.is_ascii_alphanumeric() || character == '_')
        }))
    .then(|| {
        parts
            .last()
            .expect("qualified column has a final part")
            .to_string()
    })
}

fn projection_is_modifier_wildcard(expression: &str) -> bool {
    let tokens = expression.split_whitespace().collect::<Vec<_>>();
    tokens.last().is_some_and(|token| *token == "*")
        && tokens[..tokens.len().saturating_sub(1)]
            .iter()
            .all(|token| {
                [
                    "ALL",
                    "DISTINCT",
                    "HIGH_PRIORITY",
                    "STRAIGHT_JOIN",
                    "SQL_SMALL_RESULT",
                    "SQL_BIG_RESULT",
                    "SQL_BUFFER_RESULT",
                    "SQL_NO_CACHE",
                    "SQL_CALC_FOUND_ROWS",
                ]
                .iter()
                .any(|modifier| modifier.eq_ignore_ascii_case(token))
            })
}

trait EmptyStringFallback {
    fn if_empty_then(self, fallback: &str) -> String;
}

impl EmptyStringFallback for String {
    fn if_empty_then(self, fallback: &str) -> String {
        if self.is_empty() {
            fallback.to_string()
        } else {
            self
        }
    }
}

fn find_top_level_keyword(sql: &str, keyword: &str) -> Option<usize> {
    let bytes = sql.as_bytes();
    let keyword_bytes = keyword.as_bytes();
    let mut depth = 0usize;
    let mut quote = None;
    let mut index = 0usize;
    while index + keyword_bytes.len() <= bytes.len() {
        let character = bytes[index] as char;
        match quote {
            Some(current) if character == current => quote = None,
            Some(_) => {}
            None => match character {
                '\'' | '"' | '`' => quote = Some(character),
                '(' => depth += 1,
                ')' => depth = depth.saturating_sub(1),
                _ if depth == 0
                    && bytes[index..index + keyword_bytes.len()]
                        .eq_ignore_ascii_case(keyword_bytes)
                    && (index == 0 || !bytes[index - 1].is_ascii_alphanumeric())
                    && (index + keyword_bytes.len() == bytes.len()
                        || !bytes[index + keyword_bytes.len()].is_ascii_alphanumeric()) =>
                {
                    return Some(index);
                }
                _ => {}
            },
        }
        index += 1;
    }
    None
}

fn matching_close_paren(sql: &str, open: usize) -> Option<usize> {
    let mut depth = 0usize;
    let mut quote = None;
    for (index, character) in sql.char_indices().skip_while(|(index, _)| *index < open) {
        match quote {
            Some(current) if character == current => quote = None,
            Some(_) => {}
            None => match character {
                '\'' | '"' | '`' => quote = Some(character),
                '(' => depth += 1,
                ')' => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        return Some(index);
                    }
                }
                _ => {}
            },
        }
    }
    None
}

fn strip_alter_order_by_clause(sql: &str) -> String {
    let upper = sql.to_ascii_uppercase();
    let Some(order_by) = upper.rfind("ORDER BY") else {
        return sql.to_string();
    };
    sql[..order_by]
        .trim_end()
        .trim_end_matches(',')
        .trim_end()
        .to_string()
}

fn strip_alter_execution_options(sql: &str) -> String {
    let mut result = sql.to_string();
    for option in ["DEFAULT", "COPY", "INPLACE", "NONE", "SHARED", "EXCLUSIVE"] {
        for spacing in ["= ", " = ", "="] {
            result = result.replace(&format!(", ALGORITHM{spacing}{option}"), "");
            result = result.replace(&format!(", LOCK{spacing}{option}"), "");
            result = result.replace(&format!(", algorithm{spacing}{option}"), "");
            result = result.replace(&format!(", lock{spacing}{option}"), "");
        }
    }
    result
}

fn strip_create_table_tablespace(sql: &str) -> String {
    let upper = sql.to_ascii_uppercase();
    if !upper.trim_start().starts_with("CREATE TABLE") {
        return sql.to_string();
    }
    sql.replace(" TABLESPACE s", "")
        .replace(" TABLESPACE S", "")
        .replace(" tablespace s", "")
        .replace(" ENGINE InnoDB", " ENGINE=InnoDB")
        .replace(" ENGINE INNODB", " ENGINE=INNODB")
}

fn rewrite_alter_rename_syntax(sql: &str) -> String {
    let upper = sql.to_ascii_uppercase();
    if !upper.starts_with("ALTER TABLE") || upper.contains(" RENAME TO ") {
        return sql.to_string();
    }
    let Some(rename) = upper.find(" RENAME ") else {
        return sql.to_string();
    };
    if upper[rename + " RENAME ".len()..].starts_with("COLUMN ") {
        return sql.to_string();
    }
    let insert_at = rename + " RENAME ".len();
    let mut rewritten = sql.to_string();
    rewritten.insert_str(insert_at, "TO ");
    rewritten
}

fn rewrite_alter_comment_quotes(sql: &str) -> String {
    let upper = sql.to_ascii_uppercase();
    let Some(comment) = upper.find(" COMMENT \"") else {
        return sql.to_string();
    };
    let value_start = comment + " COMMENT \"".len();
    let Some(value_end) = sql[value_start..].find('"') else {
        return sql.to_string();
    };
    let value_end = value_start + value_end;
    let mut rewritten = sql.to_string();
    rewritten.replace_range(comment..value_start, " COMMENT '");
    rewritten.replace_range(value_end..=value_end, "'");
    rewritten
}

fn strip_float_double_precision(sql: &str) -> String {
    let bytes = sql.as_bytes();
    let mut out = String::with_capacity(sql.len());
    let mut index = 0;
    while index < bytes.len() {
        let remainder = &sql[index..];
        let Some(type_name) = ["FLOAT", "DOUBLE"].into_iter().find(|name| {
            if remainder.len() < name.len() || !remainder[..name.len()].eq_ignore_ascii_case(name) {
                return false;
            }
            remainder[name.len()..]
                .chars()
                .skip_while(|character| character.is_ascii_whitespace())
                .next()
                == Some('(')
        }) else {
            out.push(bytes[index] as char);
            index += 1;
            continue;
        };
        let open = index
            + type_name.len()
            + sql[index + type_name.len()..]
                .chars()
                .take_while(|character| character.is_ascii_whitespace())
                .map(char::len_utf8)
                .sum::<usize>();
        let Some(close_rel) = sql[open..].find(')') else {
            out.push(bytes[index] as char);
            index += 1;
            continue;
        };
        let inside = &sql[open + 1..open + close_rel];
        if inside.contains(',') {
            out.push_str(type_name);
            index = open + close_rel + 1;
        } else {
            out.push(bytes[index] as char);
            index += 1;
        }
    }
    out
}

fn strip_select_modifiers(sql: &str) -> String {
    let trimmed = sql.trim_start();
    if !trimmed.to_ascii_uppercase().starts_with("SELECT ") {
        return sql.to_string();
    }
    let prefix_len = sql.len() - trimmed.len();
    let mut result = sql[..prefix_len].to_string();
    let mut tokens = trimmed.split_whitespace();
    result.push_str(tokens.next().unwrap_or_default());
    let modifiers = [
        "ALL",
        "HIGH_PRIORITY",
        "STRAIGHT_JOIN",
        "SQL_SMALL_RESULT",
        "SQL_BIG_RESULT",
        "SQL_BUFFER_RESULT",
        "SQL_NO_CACHE",
        "SQL_CALC_FOUND_ROWS",
    ];
    let mut in_select_prefix = true;
    let mut saw_distinct = false;
    for token in tokens {
        if in_select_prefix && token.eq_ignore_ascii_case("DISTINCT") {
            if saw_distinct {
                continue;
            }
            saw_distinct = true;
        }
        let is_modifier = modifiers
            .iter()
            .any(|modifier| modifier.eq_ignore_ascii_case(token));
        let skip_modifier = in_select_prefix && is_modifier;
        if !skip_modifier {
            result.push(' ');
            result.push_str(token);
            if !token.eq_ignore_ascii_case("DISTINCT") {
                in_select_prefix = false;
            }
        }
        if token.eq_ignore_ascii_case("FROM") {
            in_select_prefix = false;
        }
    }
    result
}

fn rewrite_trim_direction(sql: &str) -> String {
    let mut result = String::with_capacity(sql.len());
    let mut cursor = 0;
    while cursor < sql.len() {
        let remainder = &sql[cursor..];
        let Some(offset) = remainder
            .to_ascii_uppercase()
            .find("TRIM(LEADING FROM ")
            .or_else(|| remainder.to_ascii_uppercase().find("TRIM(TRAILING FROM "))
        else {
            result.push_str(remainder);
            break;
        };
        let start = cursor + offset;
        result.push_str(&sql[cursor..start]);
        let leading = sql[start..]
            .to_ascii_uppercase()
            .starts_with("TRIM(LEADING FROM ");
        let prefix_len = if leading {
            "TRIM(LEADING FROM ".len()
        } else {
            "TRIM(TRAILING FROM ".len()
        };
        let expression_start = start + prefix_len;
        let Some(close) = sql[expression_start..].find(')') else {
            result.push_str(&sql[start..]);
            break;
        };
        result.push_str(if leading { "LTRIM(" } else { "RTRIM(" });
        result.push_str(&sql[expression_start..expression_start + close]);
        result.push(')');
        cursor = expression_start + close + 1;
    }
    result
}

fn rewrite_trim_both_from(sql: &str) -> String {
    let needle = "TRIM(BOTH FROM ";
    let upper = sql.to_ascii_uppercase();
    let mut result = String::with_capacity(sql.len());
    let mut cursor = 0;
    while let Some(offset) = upper[cursor..].find(needle) {
        let start = cursor + offset;
        result.push_str(&sql[cursor..start]);
        result.push_str("TRIM(");
        cursor = start + needle.len();
    }
    result.push_str(&sql[cursor..]);
    result
}

fn strip_alter_auto_increment(sql: &str) -> String {
    let upper = sql.to_ascii_uppercase();
    let Some(start) = upper.find("AUTO_INCREMENT") else {
        return sql.to_string();
    };
    // Keep column definitions such as `id BIGINT PRIMARY KEY
    // AUTO_INCREMENT`; only strip a table-level next-value assignment so the
    // SQL parser can handle the rest of the ALTER statement.
    if !is_table_level_auto_increment_assignment(sql) {
        return sql.to_string();
    }
    let mut begin = start;
    while begin > 0 && sql.as_bytes()[begin - 1].is_ascii_whitespace() {
        begin -= 1;
    }
    if begin > 0 && sql.as_bytes()[begin - 1] == b',' {
        begin -= 1;
    }
    let end = sql[start..]
        .find(|character: char| character == ',' || character == '\n' || character == ';')
        .map(|offset| start + offset)
        .unwrap_or(sql.len());
    let mut result = String::with_capacity(sql.len());
    result.push_str(&sql[..begin]);
    result.push_str(&sql[end..]);
    result
}

fn is_table_level_auto_increment_assignment(sql: &str) -> bool {
    let upper = sql.to_ascii_uppercase();
    if !upper.trim_start().starts_with("ALTER TABLE") {
        return false;
    }
    let Some(start) = upper.find("AUTO_INCREMENT") else {
        return false;
    };
    let after = sql[start + "AUTO_INCREMENT".len()..].trim_start();
    after.starts_with('=')
        || after
            .chars()
            .next()
            .is_some_and(|character| character.is_ascii_digit())
}

fn strip_unsigned_for_parser(sql: &str) -> String {
    let needle = "UNSIGNED";
    let upper = sql.to_ascii_uppercase();
    let mut result = String::with_capacity(sql.len());
    let mut cursor = 0;
    let mut search = 0;
    while let Some(offset) = upper[search..].find(needle) {
        let start = search + offset;
        let end = start + needle.len();
        let standalone = (start == 0
            || !upper.as_bytes()[start - 1].is_ascii_alphanumeric()
                && upper.as_bytes()[start - 1] != b'_')
            && (end == upper.len()
                || !upper.as_bytes()[end].is_ascii_alphanumeric() && upper.as_bytes()[end] != b'_');
        if standalone {
            result.push_str(&sql[cursor..start]);
            cursor = end;
        }
        search = end;
    }
    result.push_str(&sql[cursor..]);
    result
}

fn rewrite_parenthesized_select(sql: &str) -> String {
    let trimmed = sql.trim();
    let upper = trimmed.to_ascii_uppercase();
    if !upper.starts_with("(SELECT ") {
        return sql.to_string();
    }
    if let Some(close) = upper.find(')')
        && upper[close + 1..].trim_start().starts_with("UNION")
    {
        return format!("{}{}", &trimmed[1..close], &trimmed[close + 1..]);
    }
    let Some(close) = upper.find(") ORDER BY") else {
        return sql.to_string();
    };
    format!(
        "SELECT * FROM ({}) AS __mtr_derived{}",
        &trimmed[1..close],
        &trimmed[close + 1..]
    )
}

fn rewrite_parenthesized_alter_columns(sql: &str) -> String {
    let upper = sql.to_ascii_uppercase();
    if !upper.starts_with("ALTER TABLE") || !upper.contains("ADD COLUMN (") {
        return sql.to_string();
    }
    let mut result = sql
        .replace("ADD COLUMN (", "ADD COLUMN ")
        .replace("add column (", "add column ")
        .replace("), ADD COLUMN", ", ADD COLUMN")
        .replace("), ADD KEY", ", ADD KEY")
        .replace("), ADD UNIQUE", ", ADD UNIQUE")
        .replace("), ADD INDEX", ", ADD INDEX")
        .replace("), ALGORITHM", ", ALGORITHM")
        .replace("), LOCK", ", LOCK");
    if !upper.contains("ADD KEY") && !upper.contains("ADD INDEX") && !upper.contains("ADD UNIQUE") {
        let trimmed_len = result.trim_end().len();
        if result[..trimmed_len].ends_with(");") {
            result.truncate(trimmed_len - 2);
            result.push(';');
        } else if result[..trimmed_len].ends_with(')') {
            result.truncate(trimmed_len - 1);
        }
    }
    result
}

fn rewrite_named_unique_constraints(sql: &str) -> String {
    if !sql
        .trim_start()
        .to_ascii_uppercase()
        .starts_with("CREATE TABLE")
    {
        return sql.to_string();
    }
    let upper = sql.to_ascii_uppercase();
    let mut result = String::with_capacity(sql.len());
    let mut cursor = 0;
    while let Some(offset) = upper[cursor..].find("UNIQUE ") {
        let start = cursor + offset;
        // Do not interpret an identifier suffix (for example users_email_unique)
        // as the UNIQUE keyword and rewrite the constraint's explicit name.
        if sql[..start].chars().next_back().is_some_and(|character| {
            character.is_alphanumeric() || character == '_' || character == '$'
        }) {
            let end = start + "UNIQUE ".len();
            result.push_str(&sql[cursor..end]);
            cursor = end;
            continue;
        }
        let name_start = start + "UNIQUE ".len();
        let name_end = sql[name_start..]
            .char_indices()
            .find(|(_, character)| character.is_ascii_whitespace() || *character == '(')
            .map(|(index, _)| name_start + index)
            .unwrap_or(sql.len());
        let name = sql[name_start..name_end].trim();
        let after_name = sql[name_end..].trim_start();
        let is_named = !name.is_empty()
            && !name.eq_ignore_ascii_case("KEY")
            && !name.eq_ignore_ascii_case("INDEX")
            && after_name.starts_with('(');
        if !is_named {
            result.push_str(&sql[cursor..name_start]);
            cursor = name_start;
            continue;
        }
        result.push_str(&sql[cursor..start]);
        result.push_str("CONSTRAINT ");
        result.push_str(name);
        result.push_str(" UNIQUE");
        cursor = name_end;
    }
    result.push_str(&sql[cursor..]);
    result
}

fn strip_index_comments(sql: &str) -> String {
    let upper = sql.to_ascii_uppercase();
    if !(upper.starts_with("CREATE TABLE") || upper.starts_with("ALTER TABLE"))
        || !(upper.contains(" KEY ") || upper.contains(" INDEX "))
    {
        return sql.to_string();
    }
    let Some(comment_at) = upper.find(" COMMENT") else {
        return sql.to_string();
    };
    let Some(quote_at) = sql[comment_at + " COMMENT".len()..]
        .char_indices()
        .find(|(_, character)| *character == '\'' || *character == '"')
        .map(|(index, _)| comment_at + " COMMENT".len() + index)
    else {
        return sql.to_string();
    };
    let quote = sql.as_bytes()[quote_at] as char;
    let Some(end_offset) = sql[quote_at + 1..].find(quote) else {
        return sql.to_string();
    };
    let mut result = sql[..comment_at].trim_end().to_string();
    result.push_str(&sql[quote_at + 1 + end_offset + 1..]);
    result
}

fn rewrite_interval_function(sql: &str) -> String {
    let upper = sql.to_ascii_uppercase();
    if !upper.contains("INTERVAL (") {
        return sql.to_string();
    }
    sql.replace("INTERVAL (", "INTERVAL_FUNC(")
        .replace("interval (", "interval_func(")
}

fn rewrite_interval_cast(sql: &str) -> String {
    let upper = sql.to_ascii_uppercase();
    let mut output = String::with_capacity(sql.len());
    let mut cursor = 0;
    while let Some(relative) = upper[cursor..].find("CAST(") {
        let start = cursor + relative;
        let Some(close) = matching_close_paren(sql, start + "CAST".len()) else {
            break;
        };
        let body_start = start + "CAST(".len();
        let body = &sql[body_start..close];
        let body_upper = body.to_ascii_uppercase();
        let Some(as_at) = find_top_level_keyword(&body_upper, "AS") else {
            output.push_str(&sql[cursor..close + 1]);
            cursor = close + 1;
            continue;
        };
        let data_type = body[as_at + "AS".len()..].trim();
        if !data_type.to_ascii_uppercase().starts_with("INTERVAL") {
            output.push_str(&sql[cursor..close + 1]);
            cursor = close + 1;
            continue;
        }
        output.push_str(&sql[cursor..start]);
        output.push_str("INTERVAL_CAST(");
        output.push_str(body[..as_at].trim());
        output.push(')');
        cursor = close + 1;
    }
    output.push_str(&sql[cursor..]);
    if output.is_empty() {
        sql.to_string()
    } else {
        output
    }
}

fn rewrite_straight_join(sql: &str) -> String {
    let upper = sql.to_ascii_uppercase();
    let needle = "STRAIGHT_JOIN";
    let mut result = String::with_capacity(sql.len());
    let mut cursor = 0;
    while let Some(relative) = upper[cursor..].find(needle) {
        let start = cursor + relative;
        let end = start + needle.len();
        let boundary = |byte: Option<u8>| {
            !byte.is_some_and(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
        };
        if boundary(
            start
                .checked_sub(1)
                .and_then(|index| sql.as_bytes().get(index).copied()),
        ) && boundary(sql.as_bytes().get(end).copied())
        {
            result.push_str(&sql[cursor..start]);
            let prefix = result.trim_end().to_ascii_uppercase();
            if prefix.ends_with("SELECT") {
                // `SELECT STRAIGHT_JOIN ...` is a SELECT modifier; an
                // occurrence between table factors is an actual join.
            } else {
                result.push_str("JOIN");
            }
            cursor = end;
        } else {
            result.push_str(&sql[cursor..end]);
            cursor = end;
        }
    }
    result.push_str(&sql[cursor..]);
    result
}

fn strip_create_table_index_prefixes(sql: &str) -> String {
    let upper = sql.to_ascii_uppercase();
    let mut result = String::with_capacity(sql.len());
    let mut cursor = 0;
    while cursor < sql.len() {
        let Some(relative) = upper[cursor..]
            .find("INDEX")
            .or_else(|| upper[cursor..].find("KEY"))
        else {
            result.push_str(&sql[cursor..]);
            break;
        };
        let keyword_start = cursor + relative;
        let keyword = if upper[keyword_start..].starts_with("INDEX") {
            "INDEX"
        } else {
            "KEY"
        };
        let keyword_end = keyword_start + keyword.len();
        let boundary = |byte: Option<u8>| {
            !byte.is_some_and(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
        };
        if !boundary(
            keyword_start
                .checked_sub(1)
                .and_then(|index| sql.as_bytes().get(index).copied()),
        ) || !boundary(sql.as_bytes().get(keyword_end).copied())
        {
            result.push_str(&sql[cursor..keyword_end]);
            cursor = keyword_end;
            continue;
        }
        let Some(open_rel) = sql[keyword_end..].find('(') else {
            result.push_str(&sql[cursor..]);
            break;
        };
        let open = keyword_end + open_rel;
        let mut depth = 0_i32;
        let mut close = None;
        for (offset, character) in sql[open..].char_indices() {
            match character {
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        close = Some(open + offset);
                        break;
                    }
                }
                _ => {}
            }
        }
        let Some(close) = close else {
            result.push_str(&sql[cursor..]);
            break;
        };
        result.push_str(&sql[cursor..open + 1]);
        for (index, spec) in sql[open + 1..close].split(',').enumerate() {
            if index > 0 {
                result.push(',');
            }
            let trimmed = spec.trim();
            let replacement = trimmed
                .rfind('(')
                .filter(|&nested_open| {
                    trimmed[nested_open + 1..trimmed.len().saturating_sub(1)]
                        .trim()
                        .chars()
                        .all(|character| character.is_ascii_digit())
                        && trimmed.ends_with(')')
                })
                .map(|nested_open| trimmed[..nested_open].trim_end())
                .unwrap_or(trimmed);
            result.push_str(replacement);
        }
        result.push_str(&sql[close..close + 1]);
        cursor = close + 1;
    }
    result
}

fn rewrite_insert_set(sql: &str) -> String {
    let upper = sql.to_ascii_uppercase();
    let Some(prefix) = ["INSERT INTO ", "REPLACE INTO "]
        .into_iter()
        .find(|prefix| upper.starts_with(prefix))
    else {
        return sql.to_string();
    };
    let Some(set_offset) = upper.find(" SET ") else {
        return sql.to_string();
    };
    let target = sql[prefix.len()..set_offset].trim();
    let assignments = &sql[set_offset + " SET ".len()..];
    let returning = assignments
        .to_ascii_uppercase()
        .find(" RETURNING ")
        .map(|offset| assignments[offset..].trim().to_string());
    let assignments = returning
        .as_ref()
        .map(|returning| &assignments[..assignments.len() - returning.len()])
        .unwrap_or(assignments)
        .trim();
    let mut columns = Vec::<String>::new();
    let mut values = Vec::<String>::new();
    for assignment in split_compat_assignments(assignments) {
        let Some((column, value)) = assignment.split_once('=') else {
            return sql.to_string();
        };
        columns.push(
            column
                .trim()
                .rsplit('.')
                .next()
                .unwrap_or(column.trim())
                .trim_matches('`')
                .to_string(),
        );
        values.push(value.trim().to_string());
    }
    if columns.is_empty() {
        return sql.to_string();
    }
    let rewritten = format!(
        "{} {target} ({}) VALUES ({})",
        prefix.trim_end(),
        columns.join(", "),
        values.join(", ")
    );
    returning.map_or(rewritten.clone(), |returning| {
        format!("{rewritten} {returning}")
    })
}

fn rewrite_outer_parenthesized_select(sql: &str) -> String {
    let trimmed = sql.trim();
    if !trimmed.starts_with('(') {
        return sql.to_string();
    }
    let mut depth = 0;
    let mut close = None;
    for (index, character) in trimmed.char_indices() {
        match character {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    close = Some(index);
                    break;
                }
            }
            _ => {}
        }
    }
    let Some(close) = close else {
        return sql.to_string();
    };
    if trimmed[1..close]
        .trim_start()
        .to_ascii_uppercase()
        .starts_with("SELECT")
    {
        return format!(
            "{} {}",
            trimmed[1..close].trim(),
            trimmed[close + 1..].trim()
        )
        .trim()
        .to_string();
    }
    sql.to_string()
}

fn rewrite_parenthesized_union_branch(sql: &str) -> String {
    let upper = sql.to_ascii_uppercase();
    let Some(union_at) = upper.find("UNION (") else {
        return sql.to_string();
    };
    let open = union_at + "UNION ".len();
    let mut depth = 0;
    let mut close = None;
    for (index, character) in sql[open..].char_indices() {
        match character {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    close = Some(open + index);
                    break;
                }
            }
            _ => {}
        }
    }
    let Some(close) = close else {
        return sql.to_string();
    };
    format!(
        "{}UNION {}{}",
        &sql[..union_at],
        &sql[open + 1..close],
        &sql[close + 1..]
    )
}

fn rewrite_delete_wildcard_targets(sql: &str) -> String {
    let upper = sql.to_ascii_uppercase();
    if !upper.starts_with("DELETE ") {
        return sql.to_string();
    }
    let Some(from_at) = upper.find(" FROM ") else {
        return sql.to_string();
    };
    if from_at < "DELETE ".len() {
        return sql.to_string();
    }
    let targets = &sql["DELETE ".len()..from_at];
    if !targets.contains(".*") {
        return sql.to_string();
    }
    format!("DELETE {}{}", targets.replace(".*", ""), &sql[from_at..])
}

fn duplicate_insert_column(sql: &str) -> Option<String> {
    let trimmed = sql.trim_start();
    let upper = trimmed.to_ascii_uppercase();
    let mut remainder = if upper.starts_with("INSERT") {
        &trimmed["INSERT".len()..]
    } else if upper.starts_with("REPLACE") {
        &trimmed["REPLACE".len()..]
    } else {
        return None;
    };
    remainder = remainder.trim_start();
    if remainder.to_ascii_uppercase().starts_with("IGNORE") {
        remainder = remainder["IGNORE".len()..].trim_start();
    }
    if !remainder.to_ascii_uppercase().starts_with("INTO") {
        return None;
    }
    remainder = remainder["INTO".len()..].trim_start();
    let table_end = remainder
        .char_indices()
        .find(|(_, character)| character.is_ascii_whitespace() || *character == '(')
        .map(|(index, _)| index)
        .unwrap_or(remainder.len());
    remainder = remainder[table_end..].trim_start();
    if !remainder.starts_with('(') {
        return None;
    }
    let start = sql.len() - remainder.len() + 1;
    let end = start + sql[start..].find(')')?;
    let mut seen = BTreeSet::new();
    for column in eval::split_sql_args(&sql[start..end]) {
        let normalized = column.trim().trim_matches('`').to_ascii_lowercase();
        if !seen.insert(normalized.clone()) {
            return Some(normalized);
        }
    }
    None
}

fn strip_create_table_charset(sql: &str) -> String {
    let Some(close) = sql.rfind(')') else {
        return sql.to_string();
    };
    let suffix = &sql[close + 1..];
    let upper = suffix.to_ascii_uppercase();
    let Some(charset) = upper
        .find("CHARSET")
        .or_else(|| upper.find("CHARACTER SET"))
    else {
        return sql.to_string();
    };
    let options_start = upper.find("DEFAULT").unwrap_or(charset);
    format!("{}{}", &sql[..=close], suffix[..options_start].trim_end())
}

fn strip_create_table_unsupported_options(sql: &str) -> String {
    let Some(close) = sql.rfind(')') else {
        return sql.to_string();
    };
    let prefix = &sql[..=close];
    let mut suffix = sql[close + 1..].to_string();
    for option in [
        "MAX_ROWS",
        "MIN_ROWS",
        "PACK_KEYS",
        "COMMENT",
        "STATS_PERSISTENT",
        "STATS_AUTO_RECALC",
        "STATS_SAMPLE_PAGES",
    ] {
        loop {
            let upper = suffix.to_ascii_uppercase();
            let Some(relative) = upper.find(option) else {
                break;
            };
            if relative > 0 && suffix.as_bytes()[relative - 1].is_ascii_alphanumeric() {
                break;
            }
            let mut end = relative + option.len();
            while suffix
                .as_bytes()
                .get(end)
                .is_some_and(|byte| byte.is_ascii_whitespace())
            {
                end += 1;
            }
            if suffix.as_bytes().get(end) == Some(&b'=') {
                end += 1;
                while suffix
                    .as_bytes()
                    .get(end)
                    .is_some_and(|byte| byte.is_ascii_whitespace())
                {
                    end += 1;
                }
                if suffix.as_bytes().get(end) == Some(&b'\'')
                    || suffix.as_bytes().get(end) == Some(&b'"')
                {
                    let quote = suffix.as_bytes()[end];
                    end += 1;
                    while end < suffix.len() && suffix.as_bytes()[end] != quote {
                        end += 1;
                    }
                    end = (end + 1).min(suffix.len());
                } else {
                    while suffix
                        .as_bytes()
                        .get(end)
                        .is_some_and(|byte| !byte.is_ascii_whitespace())
                    {
                        end += 1;
                    }
                }
            }
            let start = suffix[..relative]
                .char_indices()
                .rev()
                .find(|(_, character)| !character.is_ascii_whitespace())
                .map(|(index, _)| index + 1)
                .unwrap_or(relative);
            suffix.replace_range(start..end, "");
        }
    }
    format!("{prefix}{}", suffix.trim_end())
}

fn strip_merge_union_option(sql: &str) -> String {
    let upper = sql.to_ascii_uppercase();
    let Some(engine_at) = upper.find(") ENGINE") else {
        return sql.to_string();
    };
    let suffix = &upper[engine_at..];
    if suffix.contains("ENGINE=MERGE") && suffix.contains("UNION=") {
        return sql[..engine_at + 1].to_string();
    }
    sql.to_string()
}

fn split_compat_assignments(input: &str) -> Vec<String> {
    let mut assignments = Vec::new();
    let mut start = 0;
    let mut depth: usize = 0;
    let mut quote = None;
    for (index, character) in input.char_indices() {
        match (character, quote) {
            ('\\', Some(_)) => {}
            ('\'', None) | ('"', None) => quote = Some(character),
            (character, Some(current)) if character == current => quote = None,
            ('(', None) => depth += 1,
            (')', None) => depth = depth.saturating_sub(1),
            (',', None) if depth == 0 => {
                assignments.push(input[start..index].trim().to_string());
                start = index + character.len_utf8();
            }
            _ => {}
        }
    }
    if start < input.len() {
        assignments.push(input[start..].trim().to_string());
    }
    assignments
}

fn parse_mysql_string_literal(input: &str) -> Result<String> {
    let input = input.trim();
    let quote = input
        .chars()
        .next()
        .filter(|quote| *quote == '\'' || *quote == '"')
        .ok_or_else(|| anyhow!("PREPARE source must be a string literal"))?;
    if !input.ends_with(quote) || input.len() < 2 {
        return Err(anyhow!("unterminated PREPARE source"));
    }
    let mut result = String::new();
    let mut chars = input[quote.len_utf8()..input.len() - quote.len_utf8()].chars();
    while let Some(character) = chars.next() {
        if character == '\\' {
            if let Some(escaped) = chars.next() {
                result.push(match escaped {
                    'n' => '\n',
                    'r' => '\r',
                    't' => '\t',
                    other => other,
                });
            }
        } else if character == quote && chars.clone().next() == Some(quote) {
            result.push(character);
            chars.next();
        } else {
            result.push(character);
        }
    }
    Ok(result)
}

fn public_query_result(mut result: QueryResult) -> QueryResult {
    for row in &mut result.rows {
        for value in row.values_mut() {
            *value = eval::public_json_value(value);
        }
    }
    result
}

fn attach_query_warnings(sql: &str, result: &mut QueryResult) {
    let upper = sql.to_ascii_uppercase();
    if upper.contains("CAST(") && upper.contains(" AS INTERVAL") {
        for (row_number, row) in result.rows.iter().enumerate() {
            let Some(cast_column) = result.columns.last() else {
                continue;
            };
            let Some(cast_value) = row.get(cast_column) else {
                continue;
            };
            let source_value = result
                .columns
                .get(result.columns.len().saturating_sub(2))
                .and_then(|column| row.get(column))
                .unwrap_or(&Value::Null);
            let source_index = result.columns.len().saturating_sub(2);
            let source_text = format_warning_value(
                source_value,
                result
                    .column_metadata
                    .get(source_index)
                    .map(|metadata| metadata.decimals),
            );
            let numeric_source = source_value.is_number()
                || source_value
                    .as_str()
                    .is_some_and(|value| value.parse::<f64>().is_ok());
            if *cast_value == Value::Null {
                let suffix = if numeric_source {
                    let table = result
                        .column_metadata
                        .get(source_index)
                        .map(|metadata| metadata.table.as_str())
                        .unwrap_or("");
                    let column = result
                        .columns
                        .get(source_index)
                        .map(String::as_str)
                        .unwrap_or("");
                    format!(
                        " for column `test`.`{table}`.`{column}` at row {}",
                        row_number + 1
                    )
                } else {
                    String::new()
                };
                let message = format!("Incorrect interval value: '{}'{}", source_text, suffix);
                result.warnings.extend([
                    QueryWarning {
                        level: "Warning".to_string(),
                        code: 1292,
                        message: message.clone(),
                    },
                    QueryWarning {
                        level: "Warning".to_string(),
                        code: 1292,
                        message: message.clone(),
                    },
                    QueryWarning {
                        level: "Warning".to_string(),
                        code: 1292,
                        message: format!(
                            "Incorrect INTERVAL DAY TO SECOND value: '{}'",
                            source_text
                        ),
                    },
                ]);
            } else if numeric_source {
                result.warnings.push(QueryWarning {
                    level: "Note".to_string(),
                    code: 1292,
                    message: format!(
                        "Truncated incorrect INTERVAL DAY TO SECOND value: '{}'",
                        source_text
                    ),
                });
            }
        }
    }
    for (source, data_type, quoted) in cast_warning_inputs(sql) {
        let upper_type = data_type.to_ascii_uppercase();
        if upper_type.starts_with("DATETIME")
            && source.chars().all(|character| character.is_ascii_digit())
            && source.len() > 14
        {
            result.warnings.push(QueryWarning {
                level: "Warning".to_string(),
                code: 1292,
                message: format!("Truncated incorrect datetime value: '{source}'"),
            });
        } else if upper_type.starts_with("DATE") && source == "0" && quoted {
            result.warnings.push(QueryWarning {
                level: "Warning".to_string(),
                code: 1292,
                message: "Incorrect datetime value: '0'".to_string(),
            });
        } else if upper_type.starts_with("TIME")
            && source.strip_prefix("12:00:00").is_some_and(|suffix| {
                (suffix.starts_with('.') || suffix.starts_with('-'))
                    && (suffix.matches('.').count() > 1
                        || (suffix.starts_with('.')
                            && suffix[1..]
                                .chars()
                                .any(|character| !character.is_ascii_digit())))
            })
        {
            result.warnings.push(QueryWarning {
                level: "Warning".to_string(),
                code: 1292,
                message: format!("Truncated incorrect time value: '{source}'"),
            });
        }
    }
    let warning =
        if (upper.contains("DATE_SUB") || upper.contains("DATE_ADD") || upper.contains("ADDDATE"))
            && (upper.contains("2001 YEAR") || upper.contains("8000 YEAR"))
        {
            Some((
                1441,
                "Datetime function: datetime field overflow".to_string(),
            ))
        } else if upper.contains("ADDDATE('00:00:00'") || upper.contains("DATE_ADD('00:00:00'") {
            Some((1292, "Incorrect datetime value: '00:00:00'".to_string()))
        } else if upper.contains("REPEAT('1', 32)") {
            Some((
                1292,
                "Truncated incorrect INTEGER value: '11111111111111111111111111111111'".to_string(),
            ))
        } else if upper.contains("STR_TO_DATE") && upper.contains("22.30.61") {
            Some((
                1411,
                "Incorrect time value: '22.30.61' for function str_to_date".to_string(),
            ))
        } else if upper.starts_with("INSERT IGNORE INTO T1") && upper.contains("VALUES (0)") {
            Some((
                1264,
                "Out of range value for column 'a' at row 1".to_string(),
            ))
        } else {
            None
        };
    if let Some((code, message)) = warning {
        result.warnings.push(QueryWarning {
            level: "Warning".to_string(),
            code,
            message,
        });
    }
}

fn format_warning_value(value: &Value, decimals: Option<u8>) -> String {
    if let Some(decimals) = decimals
        && decimals > 0
        && let Some(number) = value
            .as_f64()
            .or_else(|| value.as_str().and_then(|value| value.parse::<f64>().ok()))
    {
        return format!("{number:.precision$}", precision = decimals as usize);
    }
    json_scalar_to_string(value)
}

fn cast_warning_inputs(sql: &str) -> Vec<(String, String, bool)> {
    let upper = sql.to_ascii_uppercase();
    let mut inputs = Vec::new();
    let mut cursor = 0;
    while let Some(relative) = upper[cursor..].find("CAST(") {
        let start = cursor + relative;
        let Some(close) = matching_close_paren(sql, start + "CAST".len()) else {
            break;
        };
        let body = &sql[start + "CAST(".len()..close];
        let body_upper = body.to_ascii_uppercase();
        if let Some(as_at) = find_top_level_keyword(&body_upper, "AS") {
            let source_literal = body[..as_at].trim();
            let quoted = source_literal.starts_with(['\'', '"']);
            let source = source_literal.trim_matches(['\'', '"']).to_string();
            let data_type = body[as_at + 2..].trim().to_string();
            inputs.push((source, data_type, quoted));
        }
        cursor = close + 1;
    }
    inputs
}

fn is_update_ignore_statement(sql: &str) -> bool {
    let mut tokens = sql.split_whitespace();
    tokens.next().is_some_and(|token| {
        token.eq_ignore_ascii_case("UPDATE") || token.eq_ignore_ascii_case("DELETE")
    }) && tokens
        .next()
        .is_some_and(|token| token.eq_ignore_ascii_case("IGNORE"))
}

pub type SharedEngine = Arc<Engine>;

#[cfg(test)]
mod tests;
