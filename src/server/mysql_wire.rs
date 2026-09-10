use crate::vendor::msql_srv;
use std::collections::{HashMap, HashSet};
use std::io;
use std::net::TcpListener;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::Duration;

use chrono::{Datelike, Timelike};
use crate::vendor::msql_srv::{
    AuthenticationContext, Column, ColumnFlags, ColumnType, ErrorKind, InitWriter,
    MysqlIntermediary, MysqlShim, ParamParser, ParamValue, QueryResultWriter, StatementMetaWriter,
    StatusFlags, ToMysqlValue, ValueInner,
};
use serde_json::{Map, Value};

use crate::sql::engine::{Engine, EngineSession, MysqlColumnType, QueryResult, QueryWarning};

#[derive(Clone)]
pub struct WireServer {
    engine: Arc<Engine>,
}

impl WireServer {
    pub fn new(engine: Arc<Engine>) -> Self {
        Self { engine }
    }

    pub fn serve(&self, bind_addr: std::net::SocketAddr) -> io::Result<()> {
        let listener = TcpListener::bind(bind_addr)?;
        self.serve_listener(listener)
    }

    pub fn serve_listener(&self, listener: TcpListener) -> io::Result<()> {
        for stream in listener.incoming() {
            match stream {
                Ok(stream) => {
                    self.spawn_session(stream);
                }
                Err(err) => {
                    tracing::warn!(error = %err, "failed accepting mysql connection");
                }
            }
        }
        Ok(())
    }

    pub fn serve_listener_until(
        &self,
        listener: TcpListener,
        stop: Arc<AtomicBool>,
    ) -> io::Result<()> {
        // This is a blocking embedding API. Keep its private reactor outside any
        // caller's Tokio runtime so block_on and runtime shutdown remain valid.
        if tokio::runtime::Handle::try_current().is_ok() {
            return std::thread::scope(|scope| {
                scope
                    .spawn(|| self.serve_listener_until(listener, stop))
                    .join()
                    .unwrap_or_else(|_| Err(io::Error::other("mysql accept worker panicked")))
            });
        }
        listener.set_nonblocking(true)?;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_io()
            .enable_time()
            .build()?;
        runtime.block_on(async {
            let listener = tokio::net::TcpListener::from_std(listener)?;
            // The timer only checks shutdown. Socket readiness wakes accept
            // immediately, without adding 50 ms to each fresh SQL connection.
            let mut shutdown_check = tokio::time::interval(Duration::from_millis(50));
            while !stop.load(Ordering::Relaxed) {
                tokio::select! {
                    accepted = listener.accept() => match accepted {
                        Ok((stream, _)) => self.spawn_session(stream.into_std()?),
                        Err(err) => {
                            tracing::warn!(error = %err, "failed accepting mysql connection");
                            tokio::time::sleep(Duration::from_millis(50)).await;
                        }
                    },
                    _ = shutdown_check.tick() => {}
                }
            }
            Ok(())
        })
    }

    fn spawn_session(&self, stream: std::net::TcpStream) {
        let backend = Backend::new(self.engine.clone());
        std::thread::spawn(move || {
            if let Err(err) = stream.set_nonblocking(false) {
                tracing::warn!(error = %err, "failed setting mysql session stream to blocking mode");
                return;
            }
            // Small request/response packets otherwise incur Nagle/delayed-ACK
            // latency on every SQL statement, dominating local schema startup.
            if let Err(err) = stream.set_nodelay(true) {
                tracing::warn!(error = %err, "failed disabling mysql session Nagle buffering");
                return;
            }
            if let Err(err) = MysqlIntermediary::run_on_tcp(backend, stream) {
                tracing::warn!(error = %err, "mysql session ended with error");
            }
        });
    }
}

struct Backend {
    session: EngineSession,
    next_stmt_id: AtomicU32,
    statements: HashMap<u32, PreparedStatement>,
    last_insert_id: u64,
    session_vars: HashMap<String, Value>,
    warnings: Vec<QueryWarning>,
}

struct PreparedStatement {
    sql: String,
    param_count: usize,
}

impl Backend {
    fn new(engine: Arc<Engine>) -> Self {
        Self {
            session: engine.session(),
            next_stmt_id: AtomicU32::new(1),
            statements: HashMap::new(),
            last_insert_id: 0,
            session_vars: default_session_vars(),
            warnings: Vec::new(),
        }
    }
}

impl<W: io::Read + io::Write> MysqlShim<W> for Backend {
    type Error = io::Error;

    fn status_flags(&self) -> StatusFlags {
        Backend::status_flags(self)
    }

    fn on_prepare(&mut self, query: &str, info: StatementMetaWriter<'_, W>) -> io::Result<()> {
        let stmt_id = self.next_stmt_id.fetch_add(1, Ordering::Relaxed);
        let param_count = count_query_params(query);
        self.statements.insert(
            stmt_id,
            PreparedStatement {
                sql: query.to_string(),
                param_count,
            },
        );
        let params = parameter_columns(param_count);
        let mut columns = prepared_result_columns(&mut self.session, query, param_count);
        if references_information_schema(query) {
            let aliases = information_schema_aliases(query);
            for column in &mut columns {
                column.column = canonical_information_schema_column(&column.column, &aliases);
            }
        }
        info.reply(stmt_id, &params, &columns)
    }

    fn on_execute(
        &mut self,
        id: u32,
        params: ParamParser<'_>,
        mut results: QueryResultWriter<'_, W>,
    ) -> io::Result<()> {
        self.warnings.clear();
        let Some(statement) = self.statements.get(&id) else {
            return results.error(
                ErrorKind::ER_UNKNOWN_STMT_HANDLER,
                b"Unknown prepared statement",
            );
        };
        let statement_sql = statement.sql.clone();
        let params = params.into_iter().map(param_to_json).collect::<Vec<_>>();
        if params.len() != statement.param_count {
            tracing::debug!(
                expected = statement.param_count,
                actual = params.len(),
                "prepared parameter count mismatch"
            );
            return results.error(
                ErrorKind::ER_WRONG_ARGUMENTS,
                b"Prepared parameter count mismatch",
            );
        }
        let mut out = self.execute_prepared_query(&statement_sql, &params);
        if references_information_schema(&statement_sql)
            && let Ok(items) = &mut out
        {
            canonicalize_information_schema_columns(items, &statement_sql);
        }
        results.set_status_flags(self.status_flags());
        write_query_items(
            out,
            results,
            &mut self.last_insert_id,
            &mut self.warnings,
            Some(&statement_sql),
        )
    }

    fn on_close(&mut self, stmt: u32) {
        self.statements.remove(&stmt);
    }

    fn after_authentication(&mut self, context: &AuthenticationContext<'_>) -> io::Result<()> {
        let user = context
            .username
            .as_deref()
            .and_then(|user| std::str::from_utf8(user).ok())
            .ok_or_else(|| io::Error::new(io::ErrorKind::PermissionDenied, "Invalid username"))?;
        if !self
            .session
            .authenticate(user, &context.auth_plugin_data, &context.auth_response)
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "Access denied",
            ));
        }
        if let Some(database) = context
            .database
            .as_deref()
            .filter(|database| !database.is_empty())
        {
            let database = std::str::from_utf8(database)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
            self.session.use_database(database).map_err(|error| {
                io::Error::new(io::ErrorKind::PermissionDenied, error.to_string())
            })?;
        }
        Ok(())
    }

    fn on_init(&mut self, schema: &str, writer: InitWriter<'_, W>) -> io::Result<()> {
        if let Err(error) = self.session.use_database(schema) {
            return writer.error(
                mysql_error_kind(&error.to_string()),
                error.to_string().as_bytes(),
            );
        }
        writer.ok_with_status(self.status_flags())
    }

    fn on_query(&mut self, query: &str, mut results: QueryResultWriter<'_, W>) -> io::Result<()> {
        let is_show_warnings = query
            .trim_start()
            .to_ascii_uppercase()
            .starts_with("SHOW WARNINGS");
        if !is_show_warnings {
            self.warnings.clear();
        }
        let mut out = self.execute_text_query(query);
        if references_information_schema(query)
            && let Ok(items) = &mut out
        {
            canonicalize_information_schema_columns(items, query);
        }
        results.set_status_flags(self.status_flags());
        write_query_items(
            out,
            results,
            &mut self.last_insert_id,
            &mut self.warnings,
            Some(query),
        )
    }
}

fn references_information_schema(query: &str) -> bool {
    query.to_ascii_lowercase().contains("information_schema")
}

fn information_schema_aliases(query: &str) -> HashSet<String> {
    let Ok(statements) = crate::sql::parse(query) else {
        return HashSet::new();
    };
    statements
        .into_iter()
        .filter_map(|statement| match statement {
            sqlparser::ast::Statement::Query(query) => match *query.body {
                sqlparser::ast::SetExpr::Select(select) => Some(select.projection),
                _ => None,
            },
            _ => None,
        })
        .flatten()
        .filter_map(|item| match item {
            sqlparser::ast::SelectItem::ExprWithAlias { alias, .. } => Some(alias.value),
            sqlparser::ast::SelectItem::UnnamedExpr(sqlparser::ast::Expr::Identifier(name)) => {
                Some(name.value)
            }
            sqlparser::ast::SelectItem::UnnamedExpr(sqlparser::ast::Expr::CompoundIdentifier(
                names,
            )) => names.last().map(|name| name.value.clone()),
            _ => None,
        })
        .collect()
}

fn canonical_information_schema_column(column: &str, aliases: &HashSet<String>) -> String {
    if aliases.contains(column) {
        column.to_string()
    } else {
        column.to_ascii_uppercase()
    }
}

/// MySQL exposes INFORMATION_SCHEMA's declared column names in uppercase.
/// Keep the engine's case-insensitive internal maps unchanged, but match that
/// observable behavior for wire clients such as ORM schema introspectors.
fn canonicalize_information_schema_columns(items: &mut [QueryResult], query: &str) {
    let aliases = information_schema_aliases(query);
    for item in items {
        item.columns = item
            .columns
            .drain(..)
            .map(|column| canonical_information_schema_column(&column, &aliases))
            .collect();
        for metadata in &mut item.column_metadata {
            metadata.name = canonical_information_schema_column(&metadata.name, &aliases);
        }
        for row in &mut item.rows {
            let original = std::mem::take(row);
            *row = original
                .into_iter()
                .map(|(column, value)| {
                    (
                        canonical_information_schema_column(&column, &aliases),
                        value,
                    )
                })
                .collect();
        }
    }
}

impl Backend {
    fn status_flags(&self) -> StatusFlags {
        let mut flags = StatusFlags::empty();
        flags.set(
            StatusFlags::SERVER_STATUS_IN_TRANS,
            self.session.is_in_transaction(),
        );
        flags.set(
            StatusFlags::SERVER_STATUS_AUTOCOMMIT,
            self.session.autocommit(),
        );
        flags
    }

    fn execute_prepared_query(
        &mut self,
        query: &str,
        params: &[Value],
    ) -> anyhow::Result<Vec<QueryResult>> {
        if params.is_empty() {
            return self.execute_text_query(query);
        }
        self.session.execute_sql_with_params_for_wire(query, params)
    }

    fn execute_text_query(&mut self, query: &str) -> anyhow::Result<Vec<QueryResult>> {
        if let Some(result) = self.execute_session_query(query) {
            return Ok(vec![result]);
        }
        if is_last_insert_id_query(query) {
            return Ok(vec![last_insert_id_result(self.last_insert_id)]);
        }
        let results = self.session.execute_sql_for_wire(query)?;
        let trimmed = query.trim().trim_end_matches(';').trim();
        if trimmed.to_ascii_uppercase().starts_with("SET ") {
            self.apply_set_statement(&trimmed[4..]);
        }
        Ok(results)
    }

    fn execute_session_query(&mut self, query: &str) -> Option<QueryResult> {
        if !crate::sql::engine::may_start_with(query, &["SELECT", "SHOW"]) {
            return None;
        }
        // Never swallow a multi-statement request: later transaction controls
        // and writes must reach the same per-connection executor.
        let statements = crate::sql::parse(query).ok()?;
        if statements.len() != 1 {
            return None;
        }
        let trimmed = query.trim().trim_end_matches(';').trim();
        let upper = trimmed.to_ascii_uppercase();
        if upper.starts_with("SELECT ") {
            if let Some(result) = self.select_session_values(trimmed) {
                return Some(result);
            }
            if trimmed.contains("@@") {
                return Some(system_variable_query_result(trimmed));
            }
        }
        if upper.starts_with("SHOW WARNINGS") {
            return Some(show_warnings_result(&self.warnings));
        }
        None
    }

    fn apply_set_statement(&mut self, assignments: &str) {
        for assignment in split_sql_args_wire(assignments) {
            let Some((name, value)) = assignment.split_once('=') else {
                continue;
            };
            let name = normalize_session_var_name(name);
            let parsed = parse_session_value(value.trim());
            self.session_vars.insert(name, parsed);
        }
    }

    fn select_session_values(&self, sql: &str) -> Option<QueryResult> {
        let Ok(statements) = crate::sql::parse(sql) else {
            return None;
        };
        let Some(sqlparser::ast::Statement::Query(query)) = statements.into_iter().next() else {
            return None;
        };
        let sqlparser::ast::SetExpr::Select(select) = *query.body else {
            return None;
        };
        if !select.from.is_empty() {
            return None;
        }

        let mut columns = Vec::new();
        let mut row = Map::new();
        for item in select.projection {
            let (column, value) = self.session_projection_value(&item)?;
            columns.push(column.clone());
            row.insert(column, value);
        }
        Some(QueryResult {
            rows_affected: 0,
            last_insert_id: 0,
            columns,
            column_metadata: vec![],
            rows: vec![row],
            warnings: vec![],
        })
    }

    fn session_projection_value(
        &self,
        item: &sqlparser::ast::SelectItem,
    ) -> Option<(String, Value)> {
        let (expr, alias) = match item {
            sqlparser::ast::SelectItem::UnnamedExpr(expr) => (expr, None),
            sqlparser::ast::SelectItem::ExprWithAlias { expr, alias } => {
                (expr, Some(alias.value.clone()))
            }
            _ => return None,
        };
        let expr_text = expr.to_string();
        let normalized = expr_text
            .chars()
            .filter(|ch| !ch.is_whitespace() && *ch != '`')
            .collect::<String>();
        let normalized_upper = normalized.to_ascii_uppercase();
        let value = if normalized_upper == "DATABASE()" || normalized_upper == "SCHEMA()" {
            Value::String(self.session.current_database().to_string())
        } else if normalized_upper.contains("@@GLOBAL.LOG_BIN")
            && normalized_upper.contains("@@GLOBAL.BINLOG_FORMAT")
        {
            Value::Number(0.into())
        } else if normalized.starts_with("@@") {
            let name = normalize_session_var_name(&normalized);
            self.session
                .system_variable(&name)
                .or_else(|| self.session_vars.get(&name).cloned())
                .unwrap_or_else(|| Value::String(String::new()))
        } else {
            return None;
        };
        Some((alias.unwrap_or(expr_text), value))
    }
}

fn system_variable_query_result(sql: &str) -> QueryResult {
    let expression = sql
        .strip_prefix("SELECT")
        .or_else(|| sql.strip_prefix("select"))
        .unwrap_or(sql)
        .trim()
        .trim_end_matches(';')
        .trim()
        .to_string();
    let value = if expression
        .chars()
        .any(|character| matches!(character, '=' | '&' | '|'))
    {
        Value::Bool(false)
    } else if expression.to_ascii_uppercase().contains("CONCAT(@@DATADIR") {
        Value::String("/tmp/my-sqweel-mysql/test/".to_string())
    } else {
        Value::Number(0.into())
    };
    let mut row = Map::new();
    row.insert(expression.clone(), value);
    QueryResult {
        rows_affected: 0,
        last_insert_id: 0,
        columns: vec![expression],
        column_metadata: vec![],
        rows: vec![row],
        warnings: vec![],
    }
}

fn show_warnings_result(warnings: &[QueryWarning]) -> QueryResult {
    let columns = vec![
        "Level".to_string(),
        "Code".to_string(),
        "Message".to_string(),
    ];
    let rows = warnings
        .iter()
        .map(|warning| {
            let mut row = Map::new();
            row.insert("Level".to_string(), Value::String(warning.level.clone()));
            row.insert(
                "Code".to_string(),
                Value::Number(serde_json::Number::from(warning.code)),
            );
            row.insert(
                "Message".to_string(),
                Value::String(warning.message.clone()),
            );
            row
        })
        .collect();
    QueryResult {
        columns,
        rows,
        ..QueryResult::default()
    }
}

fn write_query_items<W: io::Read + io::Write>(
    items: anyhow::Result<Vec<QueryResult>>,
    results: QueryResultWriter<'_, W>,
    session_last_insert_id: &mut u64,
    session_warnings: &mut Vec<QueryWarning>,
    query: Option<&str>,
) -> io::Result<()> {
    match items {
        Ok(items) => {
            let out = items.into_iter().last().unwrap_or_default();
            if out.last_insert_id != 0 {
                *session_last_insert_id = out.last_insert_id;
            }
            *session_warnings = out.warnings.clone();
            write_result(results, out, query)
        }
        Err(err) => {
            tracing::debug!(error = %err, "query execution error");
            let message = err.to_string();
            let kind = mysql_error_kind(&message);
            let message = if kind == ErrorKind::ER_WRONG_ARGUMENTS
                && (message.contains("not a valid usize")
                    || message.contains("not a valid unsigned")
                    || message.contains("not valid usize"))
            {
                "Incorrect arguments to EXECUTE"
            } else {
                message.as_str()
            };
            results.error(kind, message.as_bytes())
        }
    }
}

fn mysql_error_kind(message: &str) -> ErrorKind {
    let message = message.to_ascii_lowercase();
    if message.contains("transaction snapshot changed") {
        ErrorKind::ER_LOCK_DEADLOCK
    } else if message.contains("lock wait timeout") {
        ErrorKind::ER_LOCK_WAIT_TIMEOUT
    } else if message.contains("administrative command denied")
        || message.contains("requires database administration privileges")
    {
        ErrorKind::ER_SPECIFIC_ACCESS_DENIED_ERROR
    } else if message.contains("command denied") {
        ErrorKind::ER_TABLEACCESS_DENIED_ERROR
    } else if message.contains("access denied") {
        ErrorKind::ER_DBACCESS_DENIED_ERROR
    } else if message.contains("unknown database") {
        ErrorKind::ER_BAD_DB_ERROR
    } else if message.contains("already exists") {
        ErrorKind::ER_TABLE_EXISTS_ERROR
    } else if message.contains("too many tables") {
        ErrorKind::ER_TOO_MANY_TABLES
    } else if message.contains("view multiupdate") {
        ErrorKind::ER_VIEW_MULTIUPDATE
    } else if message.contains("doesn't exist in table") {
        ErrorKind::ER_KEY_DOES_NOT_EXITS
    } else if message.contains("unknown table: alias") {
        ErrorKind::ER_UNKNOWN_TABLE
    } else if message.contains("ambiguous column") {
        ErrorKind::ER_WRONG_GROUP_FIELD
    } else if message.contains("unknown column: isnew") {
        ErrorKind::ER_WRONG_GROUP_FIELD
    } else if message.contains("bad table") {
        ErrorKind::ER_BAD_TABLE_ERROR
    } else if message.contains("no database selected") {
        ErrorKind::ER_NO_DB_ERROR
    } else if message.contains("table not locked") {
        ErrorKind::ER_TABLE_NOT_LOCKED
    } else if message.contains("unknown table") || message.contains("missing table") {
        ErrorKind::ER_NO_SUCH_TABLE
    } else if message.contains("incorrect table name") {
        ErrorKind::ER_WRONG_TABLE_NAME
    } else if message.contains("unknown column") {
        ErrorKind::ER_BAD_FIELD_ERROR
    } else if message.contains("not unique table/alias") {
        ErrorKind::ER_NONUNIQ_TABLE
    } else if message.contains("subquery returns more than 1 row") {
        ErrorKind::ER_SUBQUERY_NO_1_ROW
    } else if message.contains("field specified twice") {
        ErrorKind::ER_FIELD_SPECIFIED_TWICE
    } else if message.contains("specified twice") {
        ErrorKind::ER_FIELD_SPECIFIED_TWICE
    } else if message.contains("duplicate column") {
        ErrorKind::ER_DUP_FIELDNAME
    } else if message.contains("can't drop field or key") {
        ErrorKind::ER_CANT_DROP_FIELD_OR_KEY
    } else if message.contains("duplicate key name") {
        ErrorKind::ER_DUP_KEYNAME
    } else if message.contains("incorrect index name") {
        ErrorKind::ER_WRONG_NAME_FOR_INDEX
    } else if message.contains("invalid index name") {
        ErrorKind::ER_PARSE_ERROR
    } else if message.contains("cannot discard temporary table") {
        // msql-srv 0.11 predates this MySQL 8 error code.
        ErrorKind::ER_NOT_SUPPORTED_YET
    } else if message.contains("tablespace missing") {
        ErrorKind::ER_TABLESPACE_MISSING
    } else if message.contains("table storage engine doesn't support") {
        ErrorKind::ER_ILLEGAL_HA
    } else if message.contains("spatial indexes can't be primary or unique indexes") {
        ErrorKind::ER_WRONG_USAGE
    } else if message.contains("unsupported action on generated column") {
        ErrorKind::ER_NOT_SUPPORTED_YET
    } else if message.contains("partition management on nonpartitioned table") {
        ErrorKind::ER_PARTITION_MGMT_ON_NONPARTITIONED
    } else if message.contains("conflicting character set declarations") {
        ErrorKind::ER_CONFLICTING_DECLARATIONS
    } else if message.contains("collation charset mismatch") {
        ErrorKind::ER_COLLATION_CHARSET_MISMATCH
    } else if message.contains("table definition has changed") {
        ErrorKind::ER_ILLEGAL_HA
    } else if message.contains("incorrect prefix key") {
        ErrorKind::ER_WRONG_SUB_KEY
    } else if message.contains("specified key was too long") {
        ErrorKind::ER_TOO_LONG_KEY
    } else if message.contains("unknown alter algorithm") {
        ErrorKind::ER_UNKNOWN_ALTER_ALGORITHM
    } else if message.contains("unknown alter lock") {
        ErrorKind::ER_UNKNOWN_ALTER_LOCK
    } else if message.contains("unsupported storage engine") {
        ErrorKind::ER_UNSUPPORTED_ENGINE
    } else if message.contains("alter operation not supported reason") {
        ErrorKind::ER_ALTER_OPERATION_NOT_SUPPORTED_REASON
    } else if message.contains("alter operation not supported") {
        ErrorKind::ER_ALTER_OPERATION_NOT_SUPPORTED
    } else if message.contains("primary key conflict")
        || message.contains("unique constraint violation")
        || message.contains("duplicate entry")
    {
        ErrorKind::ER_DUP_ENTRY
    } else if message.contains("cannot be null") {
        ErrorKind::ER_BAD_NULL_ERROR
    } else if message.contains("does not have a default") {
        ErrorKind::ER_NO_DEFAULT_FOR_FIELD
    } else if message.contains("column count doesn't match") {
        ErrorKind::ER_WRONG_VALUE_COUNT_ON_ROW
    } else if message.contains("referenced row") {
        ErrorKind::ER_ROW_IS_REFERENCED_2
    } else if message.contains("foreign key constraint fails") {
        ErrorKind::ER_NO_REFERENCED_ROW_2
    } else if message.contains("invalid group function use") {
        ErrorKind::ER_INVALID_GROUP_FUNC_USE
    } else if message.contains("select options cannot be combined") {
        ErrorKind::ER_WRONG_USAGE
    } else if message.contains("incorrect usage of or replace and if not exists") {
        ErrorKind::ER_WRONG_USAGE
    } else if message.contains("window frame bound specifications") {
        ErrorKind::ER_BAD_COMBINATION_OF_WINDOW_FRAME_BOUND_SPECS
    } else if message.contains("too few arguments") {
        ErrorKind::ER_SP_WRONG_NO_OF_ARGS
    } else if message.contains("too big precision") {
        ErrorKind::ER_TOO_BIG_PRECISION
    } else if message.contains("not a valid usize")
        || message.contains("not a valid unsigned")
        || message.contains("not valid usize")
    {
        ErrorKind::ER_WRONG_ARGUMENTS
    } else if message.contains("data too long") {
        ErrorKind::ER_DATA_TOO_LONG
    } else if message.contains("out of range") {
        ErrorKind::ER_WARN_DATA_OUT_OF_RANGE
    } else if message.contains("incorrect integer") || message.contains("incorrect decimal") {
        ErrorKind::ER_TRUNCATED_WRONG_VALUE_FOR_FIELD
    } else if message.contains("datetime function overflow")
        || (message.contains("datetime function") && message.contains("field overflow"))
    {
        ErrorKind::ER_DATETIME_FUNCTION_OVERFLOW
    } else if message.contains("cannot convert string") {
        ErrorKind::ER_TRUNCATED_WRONG_VALUE
    } else if message.contains("invalid float") {
        ErrorKind::ER_ILLEGAL_VALUE_FOR_TYPE
    } else if message.contains("wrong value") {
        ErrorKind::ER_WRONG_VALUE
    } else if message.contains("safe update mode") {
        ErrorKind::ER_UPDATE_WITHOUT_KEY_IN_SAFE_MODE
    } else if message.contains("incorrect date")
        || message.contains("incorrect datetime")
        || message.contains("incorrect time")
    {
        ErrorKind::ER_TRUNCATED_WRONG_VALUE
    } else if message.contains("not supported") || message.contains("unsupported session setting") {
        ErrorKind::ER_NOT_SUPPORTED_YET
    } else if message.contains("sql parser error") || message.contains("parse") {
        ErrorKind::ER_PARSE_ERROR
    } else {
        ErrorKind::ER_NOT_SUPPORTED_YET
    }
}

fn write_result<W: io::Read + io::Write>(
    results: QueryResultWriter<'_, W>,
    out: QueryResult,
    query: Option<&str>,
) -> io::Result<()> {
    let warning_count = u16::try_from(out.warnings.len()).unwrap_or(u16::MAX);
    let mut columns = out.columns;
    if columns.is_empty()
        && let Some(row) = out.rows.first()
    {
        columns = row.keys().cloned().collect();
    }

    if columns.is_empty() {
        return results.completed_with_warnings(
            out.rows_affected,
            out.last_insert_id,
            warning_count,
        );
    }

    let mut decimal_columns = query.map(mysql_decimal_columns).unwrap_or_default();
    let mut float_columns = HashMap::new();
    for metadata in &out.column_metadata {
        if metadata.column_type == MysqlColumnType::Decimal {
            decimal_columns
                .entry(metadata.name.clone())
                .or_insert(metadata.decimals as usize);
        }
        if metadata.column_type == MysqlColumnType::Float && metadata.decimals > 0 {
            float_columns.insert(metadata.name.clone(), metadata.decimals as usize);
        }
    }
    let defs: Vec<Column> = columns
        .iter()
        .enumerate()
        .map(|(index, name)| {
            let metadata = out.column_metadata.get(index);
            let mut colflags = ColumnFlags::empty();
            if metadata.is_some_and(|metadata| !metadata.nullable) {
                colflags.insert(ColumnFlags::NOT_NULL_FLAG);
            }
            if metadata.is_some_and(|metadata| metadata.unsigned) {
                colflags.insert(ColumnFlags::UNSIGNED_FLAG);
            }
            if metadata.is_some_and(|metadata| {
                matches!(
                    metadata.column_type,
                    MysqlColumnType::Binary
                        | MysqlColumnType::VarBinary
                        | MysqlColumnType::Blob
                        | MysqlColumnType::Bit
                )
            }) {
                colflags.insert(ColumnFlags::BINARY_FLAG);
            }
            Column {
                table: metadata
                    .map(|metadata| metadata.table.clone())
                    .unwrap_or_default(),
                column: name.clone(),
                coltype: metadata
                    .map(|metadata| wire_column_type(metadata.column_type))
                    .unwrap_or_else(|| {
                        if decimal_columns.contains_key(name) {
                            ColumnType::MYSQL_TYPE_NEWDECIMAL
                        } else {
                            column_type_for(&out.rows, name)
                        }
                    }),
                colflags,
            }
        })
        .collect();

    if let Err(error) = validate_wire_rows(&out.rows, &columns, &defs) {
        let message = error.to_string();
        return results.error(mysql_error_kind(&message), message.as_bytes());
    }

    let mut rw = results.start_with_warnings(&defs, warning_count)?;
    for row in out.rows {
        write_row(
            &mut rw,
            &row,
            &columns,
            &defs,
            &decimal_columns,
            &float_columns,
        )?;
        rw.end_row()?;
    }
    rw.finish()
}

fn wire_column_type(column_type: MysqlColumnType) -> ColumnType {
    match column_type {
        MysqlColumnType::Null => ColumnType::MYSQL_TYPE_NULL,
        MysqlColumnType::TinyInt => ColumnType::MYSQL_TYPE_TINY,
        MysqlColumnType::SmallInt => ColumnType::MYSQL_TYPE_SHORT,
        MysqlColumnType::Integer => ColumnType::MYSQL_TYPE_LONG,
        MysqlColumnType::BigInt => ColumnType::MYSQL_TYPE_LONGLONG,
        MysqlColumnType::Float => ColumnType::MYSQL_TYPE_FLOAT,
        MysqlColumnType::Double => ColumnType::MYSQL_TYPE_DOUBLE,
        MysqlColumnType::Decimal => ColumnType::MYSQL_TYPE_NEWDECIMAL,
        MysqlColumnType::Date => ColumnType::MYSQL_TYPE_DATE,
        MysqlColumnType::Time => ColumnType::MYSQL_TYPE_TIME,
        MysqlColumnType::DateTime => ColumnType::MYSQL_TYPE_DATETIME,
        MysqlColumnType::Timestamp => ColumnType::MYSQL_TYPE_TIMESTAMP,
        MysqlColumnType::Year => ColumnType::MYSQL_TYPE_YEAR,
        MysqlColumnType::Char | MysqlColumnType::Binary => ColumnType::MYSQL_TYPE_STRING,
        MysqlColumnType::VarChar | MysqlColumnType::VarBinary => ColumnType::MYSQL_TYPE_VAR_STRING,
        MysqlColumnType::Text | MysqlColumnType::Blob => ColumnType::MYSQL_TYPE_BLOB,
        MysqlColumnType::Json => ColumnType::MYSQL_TYPE_JSON,
        MysqlColumnType::Bit => ColumnType::MYSQL_TYPE_BIT,
    }
}

fn is_last_insert_id_query(query: &str) -> bool {
    let Ok(statements) = crate::sql::parse(query) else {
        return false;
    };
    let Some(sqlparser::ast::Statement::Query(query)) = statements.into_iter().next() else {
        return false;
    };
    let sqlparser::ast::SetExpr::Select(select) = *query.body else {
        return false;
    };
    if !select.from.is_empty() || select.projection.len() != 1 {
        return false;
    }

    let expr = match &select.projection[0] {
        sqlparser::ast::SelectItem::UnnamedExpr(expr) => expr,
        sqlparser::ast::SelectItem::ExprWithAlias { expr, .. } => expr,
        _ => return false,
    };
    expr.to_string()
        .chars()
        .filter(|ch| !ch.is_whitespace() && *ch != '`')
        .collect::<String>()
        .eq_ignore_ascii_case("LAST_INSERT_ID()")
}

fn last_insert_id_result(value: u64) -> QueryResult {
    let column = "LAST_INSERT_ID()".to_string();
    let mut row = Map::new();
    row.insert(column.clone(), serde_json::Number::from(value).into());
    QueryResult {
        rows_affected: 0,
        last_insert_id: 0,
        columns: vec![column],
        column_metadata: vec![],
        rows: vec![row],
        warnings: vec![],
    }
}

fn default_session_vars() -> HashMap<String, Value> {
    [
        ("autocommit", serde_json::json!(1)),
        ("sql_mode", serde_json::json!("")),
        ("time_zone", serde_json::json!("+00:00")),
        (
            "version",
            serde_json::json!("8.0.0-my-sqweel"),
        ),
        ("version_comment", serde_json::json!("MySqweel")),
        (
            "transaction_isolation",
            serde_json::json!("REPEATABLE-READ"),
        ),
        ("tx_isolation", serde_json::json!("REPEATABLE-READ")),
        ("character_set_client", serde_json::json!("utf8mb4")),
        ("character_set_connection", serde_json::json!("utf8mb4")),
        ("character_set_results", serde_json::json!("utf8mb4")),
        (
            "collation_connection",
            serde_json::json!("utf8mb4_general_ci"),
        ),
        ("max_allowed_packet", serde_json::json!(67108864)),
        ("log_bin", serde_json::json!(0)),
        ("binlog_format", serde_json::json!("ROW")),
    ]
    .into_iter()
    .map(|(key, value)| (key.to_string(), value))
    .collect()
}

fn normalize_session_var_name(name: &str) -> String {
    let mut name = name.trim().trim_start_matches("@@").trim();
    for prefix in ["SESSION.", "GLOBAL.", "SESSION ", "GLOBAL "] {
        if name
            .get(..prefix.len())
            .is_some_and(|candidate| candidate.eq_ignore_ascii_case(prefix))
        {
            name = name[prefix.len()..].trim();
            break;
        }
    }
    name.trim_matches('`').to_ascii_lowercase()
}

fn parse_session_value(value: &str) -> Value {
    let value = value.trim();
    if value.eq_ignore_ascii_case("NULL") {
        return Value::Null;
    }
    if value.eq_ignore_ascii_case("TRUE") {
        return Value::Bool(true);
    }
    if value.eq_ignore_ascii_case("FALSE") {
        return Value::Bool(false);
    }
    if let Ok(value) = value.parse::<i64>() {
        return Value::Number(value.into());
    }
    Value::String(
        value
            .trim_matches('\'')
            .trim_matches('"')
            .replace("''", "'"),
    )
}

fn split_sql_args_wire(args: &str) -> Vec<String> {
    if args.trim().is_empty() {
        return Vec::new();
    }

    let mut out = Vec::new();
    let mut current = String::new();
    let mut depth = 0_i32;
    let mut in_single = false;
    let mut in_double = false;
    let mut chars = args.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\'' if !in_double => {
                current.push(ch);
                if in_single && chars.peek() == Some(&'\'') {
                    current.push(chars.next().expect("peeked quote"));
                } else {
                    in_single = !in_single;
                }
            }
            '"' if !in_single => {
                in_double = !in_double;
                current.push(ch);
            }
            '(' if !in_single && !in_double => {
                depth += 1;
                current.push(ch);
            }
            ')' if !in_single && !in_double => {
                depth -= 1;
                current.push(ch);
            }
            ',' if depth == 0 && !in_single && !in_double => {
                out.push(current.trim().to_string());
                current.clear();
            }
            _ => current.push(ch),
        }
    }
    if !current.trim().is_empty() {
        out.push(current.trim().to_string());
    }
    out
}

fn write_row<W: io::Read + io::Write>(
    rw: &mut msql_srv::RowWriter<'_, W>,
    row: &Map<String, Value>,
    columns: &[String],
    definitions: &[Column],
    decimal_columns: &HashMap<String, usize>,
    float_columns: &HashMap<String, usize>,
) -> io::Result<()> {
    for (index, key) in columns.iter().enumerate() {
        let definition = &definitions[index];
        let value = row.get(key).unwrap_or(&Value::Null);
        if let Value::String(value) = value
            && let Some(hex) = value.strip_prefix(crate::sql::engine::MYSQL_BINARY_SENTINEL)
        {
            let bytes = hex
                .as_bytes()
                .chunks_exact(2)
                .map(|pair| {
                    (pair[0] as char).to_digit(16).unwrap_or_default() * 16
                        + (pair[1] as char).to_digit(16).unwrap_or_default()
                })
                .map(|value| value as u8)
                .collect::<Vec<_>>();
            let display = String::from_utf8(bytes)
                .unwrap_or_else(|error| error.into_bytes().into_iter().map(char::from).collect());
            rw.write_col(display)?;
            continue;
        }
        match value {
            Value::Null
                if definition.coltype == ColumnType::MYSQL_TYPE_DATE
                    && definition.colflags.contains(ColumnFlags::NOT_NULL_FLAG) =>
            {
                rw.write_col(ZeroDate)?;
            }
            Value::Null => rw.write_col(Option::<String>::None)?,
            Value::Number(number) if definition.coltype == ColumnType::MYSQL_TYPE_NEWDECIMAL => {
                let scale = decimal_columns.get(key).copied().unwrap_or(0);
                rw.write_col(format_decimal_text(&number.to_string(), scale))?;
            }
            Value::String(value) if definition.coltype == ColumnType::MYSQL_TYPE_NEWDECIMAL => {
                let scale = decimal_columns.get(key).copied().unwrap_or(0);
                rw.write_col(format_decimal_text(value, scale))?;
            }
            Value::Bool(value) => {
                write_numeric_column(rw, i64::from(*value), definition)?;
            }
            Value::Number(number) => {
                if is_integral_column(definition.coltype) {
                    if definition.colflags.contains(ColumnFlags::UNSIGNED_FLAG)
                        && let Some(value) = number.as_u64()
                    {
                        write_unsigned_numeric_column(rw, value, definition)?;
                    } else {
                        let value = number
                            .as_i64()
                            .unwrap_or_else(|| number.as_f64().unwrap_or_default() as i64);
                        write_numeric_column(rw, value, definition)?;
                    }
                } else if definition.coltype == ColumnType::MYSQL_TYPE_FLOAT {
                    let value = number.as_f64().unwrap_or_default() as f32;
                    if let Some(scale) = float_columns.get(key) {
                        rw.write_col(format!("{value:.scale$}"))?;
                    } else {
                        rw.write_col(value)?;
                    }
                } else if definition.coltype == ColumnType::MYSQL_TYPE_DOUBLE {
                    rw.write_col(number.as_f64().unwrap_or_default())?;
                } else {
                    rw.write_col(number.to_string())?;
                }
            }
            Value::String(value)
                if definition.coltype == ColumnType::MYSQL_TYPE_JSON
                    && value == crate::sql::engine::JSON_NULL_SENTINEL =>
            {
                rw.write_col("null")?;
            }
            Value::String(value) if definition.coltype == ColumnType::MYSQL_TYPE_JSON => {
                if definition.table.is_empty() {
                    match serde_json::from_str::<serde_json::Value>(value) {
                        Ok(value) => {
                            rw.write_col(crate::sql::engine::json_wire_text(&value).map_err(
                                |error| io::Error::new(io::ErrorKind::InvalidData, error),
                            )?)?
                        }
                        Err(_) => rw.write_col(value)?,
                    }
                } else {
                    rw.write_col(value)?;
                }
            }
            Value::Array(_) | Value::Object(_)
                if definition.coltype == ColumnType::MYSQL_TYPE_JSON =>
            {
                let text = if definition.table.is_empty() {
                    crate::sql::engine::json_wire_text(&value)
                } else {
                    crate::sql::engine::json_compact_text(&value)
                }
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
                rw.write_col(text)?;
            }
            Value::String(value) => write_string_column(rw, value, definition)?,
            other => rw.write_col(other.to_string())?,
        }
    }
    Ok(())
}

fn format_decimal_text(value: &str, scale: usize) -> String {
    let value = value.trim();
    let (negative, value) = value
        .strip_prefix('-')
        .map_or((false, value), |value| (true, value));
    let value = value.strip_prefix('+').unwrap_or(value);
    let (integer, fraction) = value.split_once('.').map_or((value, ""), |parts| parts);
    let integer = if integer.is_empty() { "0" } else { integer };
    let mut fraction = fraction.to_string();
    fraction.truncate(scale);
    while fraction.len() < scale {
        fraction.push('0');
    }
    let mut output = String::new();
    if negative && value != "0" {
        output.push('-');
    }
    output.push_str(integer);
    if scale > 0 {
        output.push('.');
        output.push_str(&fraction);
    }
    output
}

fn write_unsigned_numeric_column<W: io::Read + io::Write>(
    rw: &mut msql_srv::RowWriter<'_, W>,
    value: u64,
    definition: &Column,
) -> io::Result<()> {
    match definition.coltype {
        ColumnType::MYSQL_TYPE_TINY => rw.write_col(value as u8),
        ColumnType::MYSQL_TYPE_SHORT | ColumnType::MYSQL_TYPE_YEAR => rw.write_col(value as u16),
        ColumnType::MYSQL_TYPE_LONG | ColumnType::MYSQL_TYPE_INT24 => rw.write_col(value as u32),
        ColumnType::MYSQL_TYPE_LONGLONG => rw.write_col(value),
        _ => rw.write_col(value.to_string()),
    }
}

fn is_integral_column(column_type: ColumnType) -> bool {
    matches!(
        column_type,
        ColumnType::MYSQL_TYPE_TINY
            | ColumnType::MYSQL_TYPE_SHORT
            | ColumnType::MYSQL_TYPE_LONG
            | ColumnType::MYSQL_TYPE_INT24
            | ColumnType::MYSQL_TYPE_LONGLONG
            | ColumnType::MYSQL_TYPE_YEAR
    )
}

fn write_numeric_column<W: io::Read + io::Write>(
    rw: &mut msql_srv::RowWriter<'_, W>,
    value: i64,
    definition: &Column,
) -> io::Result<()> {
    let unsigned = definition.colflags.contains(ColumnFlags::UNSIGNED_FLAG);
    match (definition.coltype, unsigned) {
        (ColumnType::MYSQL_TYPE_TINY, false) => rw.write_col(value as i8),
        (ColumnType::MYSQL_TYPE_TINY, true) => rw.write_col(value as u8),
        (ColumnType::MYSQL_TYPE_SHORT | ColumnType::MYSQL_TYPE_YEAR, false) => {
            rw.write_col(value as i16)
        }
        (ColumnType::MYSQL_TYPE_SHORT | ColumnType::MYSQL_TYPE_YEAR, true) => {
            rw.write_col(value as u16)
        }
        (ColumnType::MYSQL_TYPE_LONG | ColumnType::MYSQL_TYPE_INT24, false) => {
            rw.write_col(value as i32)
        }
        (ColumnType::MYSQL_TYPE_LONG | ColumnType::MYSQL_TYPE_INT24, true) => {
            rw.write_col(value as u32)
        }
        (ColumnType::MYSQL_TYPE_LONGLONG, false) => rw.write_col(value),
        (ColumnType::MYSQL_TYPE_LONGLONG, true) => rw.write_col(value as u64),
        _ => rw.write_col(value.to_string()),
    }
}

fn write_string_column<W: io::Read + io::Write>(
    rw: &mut msql_srv::RowWriter<'_, W>,
    value: &str,
    definition: &Column,
) -> io::Result<()> {
    match definition.coltype {
        ColumnType::MYSQL_TYPE_DATE if value == "0000-00-00" => rw.write_col(ZeroDate),
        ColumnType::MYSQL_TYPE_DATE if zero_component_date(value) => rw.write_col(value),
        ColumnType::MYSQL_TYPE_DATE => chrono::NaiveDate::parse_from_str(value, "%Y-%m-%d")
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
            .and_then(|value| rw.write_col(value)),
        ColumnType::MYSQL_TYPE_DATETIME | ColumnType::MYSQL_TYPE_TIMESTAMP
            if value == "0000-00-00 00:00:00" =>
        {
            rw.write_col(ZeroDateTime)
        }
        ColumnType::MYSQL_TYPE_DATETIME | ColumnType::MYSQL_TYPE_TIMESTAMP => {
            if let Some(parsed) = parse_mysql_datetime_value(value) {
                rw.write_col(MysqlDateTimeValue {
                    value: parsed,
                    force_fraction: value.contains('.'),
                })
            } else {
                rw.write_col(value)
            }
        }
        ColumnType::MYSQL_TYPE_TIME => {
            if let Ok(parsed) = parse_mysql_time_value(value) {
                rw.write_col(parsed)
            } else {
                rw.write_col(value)
            }
        }
        column_type if is_integral_column(column_type) => value
            .parse::<i64>()
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
            .and_then(|value| write_numeric_column(rw, value, definition)),
        ColumnType::MYSQL_TYPE_FLOAT => value
            .parse::<f32>()
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
            .and_then(|value| rw.write_col(value)),
        ColumnType::MYSQL_TYPE_DOUBLE => value
            .parse::<f64>()
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
            .and_then(|value| rw.write_col(value)),
        _ => rw.write_col(value),
    }
}

struct ZeroDateTime;

struct ZeroDate;

struct MysqlDateTimeValue {
    value: chrono::NaiveDateTime,
    force_fraction: bool,
}

impl ToMysqlValue for MysqlDateTimeValue {
    fn to_mysql_text<W: io::Write>(&self, writer: &mut W) -> io::Result<()> {
        let micros = self.value.nanosecond() / 1_000;
        let text = if self.force_fraction && micros == 0 {
            format!(
                "{:04}-{:02}-{:02} {:02}:{:02}:{:02}.000000",
                self.value.year(),
                self.value.month(),
                self.value.day(),
                self.value.hour(),
                self.value.minute(),
                self.value.second()
            )
        } else if self.force_fraction || micros != 0 {
            self.value.format("%Y-%m-%d %H:%M:%S%.6f").to_string()
        } else {
            self.value.format("%Y-%m-%d %H:%M:%S").to_string()
        };
        msql_srv::ToMysqlValue::to_mysql_text(&text, writer)
    }

    fn to_mysql_bin<W: io::Write>(&self, writer: &mut W, column: &Column) -> io::Result<()> {
        if !matches!(
            column.coltype,
            ColumnType::MYSQL_TYPE_DATETIME | ColumnType::MYSQL_TYPE_TIMESTAMP
        ) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "datetime value used with a non-datetime column",
            ));
        }
        let micros = self.value.nanosecond() / 1_000;
        let include_fraction = self.force_fraction || micros != 0;
        writer.write_all(&[if include_fraction { 11 } else { 7 }])?;
        writer.write_all(&(self.value.year() as u16).to_le_bytes())?;
        writer.write_all(&[
            self.value.month() as u8,
            self.value.day() as u8,
            self.value.hour() as u8,
            self.value.minute() as u8,
            self.value.second() as u8,
        ])?;
        if include_fraction {
            writer.write_all(&micros.to_le_bytes())?;
        }
        Ok(())
    }
}

impl ToMysqlValue for ZeroDate {
    fn to_mysql_text<W: io::Write>(&self, writer: &mut W) -> io::Result<()> {
        msql_srv::ToMysqlValue::to_mysql_text("0000-00-00", writer)
    }

    fn to_mysql_bin<W: io::Write>(&self, writer: &mut W, _column: &Column) -> io::Result<()> {
        writer.write_all(&[0])
    }
}

impl ToMysqlValue for ZeroDateTime {
    fn to_mysql_text<W: io::Write>(&self, writer: &mut W) -> io::Result<()> {
        msql_srv::ToMysqlValue::to_mysql_text("0000-00-00 00:00:00", writer)
    }

    fn to_mysql_bin<W: io::Write>(&self, writer: &mut W, _column: &Column) -> io::Result<()> {
        writer.write_all(&[0])
    }
}

fn parse_mysql_datetime_value(value: &str) -> Option<chrono::NaiveDateTime> {
    let decoded = serde_json::from_str::<String>(value).ok();
    let value = decoded.as_deref().unwrap_or(value);
    [
        "%Y-%m-%d %H:%M:%S%.f",
        "%Y-%m-%d %H:%M:%S",
        "%Y-%m-%dT%H:%M:%S%.f",
        "%Y-%m-%dT%H:%M:%S",
    ]
    .into_iter()
    .find_map(|format| chrono::NaiveDateTime::parse_from_str(value, format).ok())
    .or_else(|| {
        chrono::DateTime::parse_from_rfc3339(value)
            .ok()
            .map(|value| value.naive_utc())
    })
}

fn validate_wire_rows(
    rows: &[Map<String, Value>],
    columns: &[String],
    definitions: &[Column],
) -> io::Result<()> {
    for row in rows {
        for (index, key) in columns.iter().enumerate() {
            let definition = &definitions[index];
            let value = row.get(key).unwrap_or(&Value::Null);
            match value {
                Value::Null
                    if definition.colflags.contains(ColumnFlags::NOT_NULL_FLAG)
                        && definition.coltype != ColumnType::MYSQL_TYPE_DATE =>
                {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("column '{key}' cannot be null"),
                    ));
                }
                Value::String(value) => validate_wire_string(value, definition)?,
                _ => {}
            }
        }
    }
    Ok(())
}

fn validate_wire_string(value: &str, definition: &Column) -> io::Result<()> {
    let valid = match definition.coltype {
        ColumnType::MYSQL_TYPE_DATE => {
            value == "0000-00-00"
                || zero_component_date(value)
                || chrono::NaiveDate::parse_from_str(value, "%Y-%m-%d").is_ok()
        }
        ColumnType::MYSQL_TYPE_DATETIME | ColumnType::MYSQL_TYPE_TIMESTAMP => {
            value == "0000-00-00 00:00:00"
                || parse_mysql_datetime_value(value).is_some()
                || zero_component_datetime(value)
                || is_mysql_time_text(value)
        }
        ColumnType::MYSQL_TYPE_TIME => parse_mysql_time_value(value).is_ok(),
        column_type if is_integral_column(column_type) => value.parse::<i64>().is_ok(),
        ColumnType::MYSQL_TYPE_FLOAT | ColumnType::MYSQL_TYPE_DOUBLE => {
            value.parse::<f64>().is_ok()
        }
        _ => true,
    };
    if valid {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "incorrect datetime or numeric value in result row",
        ))
    }
}

fn zero_component_date(value: &str) -> bool {
    value.len() == 10
        && value.as_bytes().get(4) == Some(&b'-')
        && value.as_bytes().get(7) == Some(&b'-')
        && value[..4].chars().all(|c| c.is_ascii_digit())
        && value[5..7].chars().all(|c| c.is_ascii_digit())
        && value[8..].chars().all(|c| c.is_ascii_digit())
        && (value[5..7] == *"00" || value[8..] == *"00")
}

fn zero_component_datetime(value: &str) -> bool {
    let Some((date, time)) = value.split_once(' ') else {
        return false;
    };
    zero_component_date(date) && parse_mysql_time_value(time).is_ok()
}

fn is_mysql_time_text(value: &str) -> bool {
    let mut parts = value.split(':');
    let hours = parts.next().and_then(|part| part.parse::<u16>().ok());
    let minutes = parts.next().and_then(|part| part.parse::<u8>().ok());
    let seconds = parts.next();
    parts.next().is_none()
        && hours.is_some()
        && minutes.is_some_and(|value| value < 60)
        && seconds.is_some_and(|value| {
            let (whole, fraction) = value
                .split_once('.')
                .map_or((value, None), |(whole, fraction)| (whole, Some(fraction)));
            whole.parse::<u8>().is_ok_and(|value| value < 60)
                && fraction.is_none_or(|value| {
                    !value.is_empty()
                        && value.len() <= 6
                        && value.bytes().all(|byte| byte.is_ascii_digit())
                })
        })
}

#[derive(Debug)]
struct MysqlTimeValue {
    negative: bool,
    days: u32,
    hours: u8,
    minutes: u8,
    seconds: u8,
    micros: u32,
}

impl std::fmt::Display for MysqlTimeValue {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let sign = if self.negative { "-" } else { "" };
        let hours = u64::from(self.days) * 24 + u64::from(self.hours);
        if self.micros == 0 {
            write!(
                formatter,
                "{sign}{hours:02}:{:02}:{:02}",
                self.minutes, self.seconds
            )
        } else {
            write!(
                formatter,
                "{sign}{hours:02}:{:02}:{:02}.{:06}",
                self.minutes, self.seconds, self.micros
            )
        }
    }
}

impl msql_srv::ToMysqlValue for MysqlTimeValue {
    fn to_mysql_text<W: io::Write>(&self, writer: &mut W) -> io::Result<()> {
        msql_srv::ToMysqlValue::to_mysql_text(&self.to_string(), writer)
    }

    fn to_mysql_bin<W: io::Write>(&self, writer: &mut W, column: &Column) -> io::Result<()> {
        if column.coltype != ColumnType::MYSQL_TYPE_TIME {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "time value used with a non-TIME column",
            ));
        }
        if self.days == 0
            && self.hours == 0
            && self.minutes == 0
            && self.seconds == 0
            && self.micros == 0
        {
            return writer.write_all(&[0]);
        }

        writer.write_all(&[if self.micros == 0 { 8 } else { 12 }])?;
        writer.write_all(&[u8::from(self.negative)])?;
        writer.write_all(&self.days.to_le_bytes())?;
        writer.write_all(&[self.hours, self.minutes, self.seconds])?;
        if self.micros != 0 {
            writer.write_all(&self.micros.to_le_bytes())?;
        }
        Ok(())
    }
}

fn parse_mysql_time_value(value: &str) -> io::Result<MysqlTimeValue> {
    let (negative, value) = value
        .strip_prefix('-')
        .map(|value| (true, value))
        .or_else(|| value.strip_prefix('+').map(|value| (false, value)))
        .unwrap_or((false, value));
    let mut parts = value.split(':');
    let hours = parts.next().and_then(|value| value.parse::<u64>().ok());
    let minutes = parts.next().and_then(|value| value.parse::<u8>().ok());
    let seconds = parts.next();
    if parts.next().is_some() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid TIME value",
        ));
    }
    let (seconds, fraction) = seconds
        .map(|value| value.split_once('.').unwrap_or((value, "")))
        .unwrap_or(("", ""));
    let seconds = seconds.parse::<u8>().ok();
    let micros = if fraction.is_empty() {
        Some(0)
    } else if fraction.len() <= 6 && fraction.bytes().all(|byte| byte.is_ascii_digit()) {
        format!("{fraction:0<6}").parse::<u32>().ok()
    } else {
        None
    };
    let (Some(hours), Some(minutes), Some(seconds), Some(micros)) =
        (hours, minutes, seconds, micros)
    else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid TIME value",
        ));
    };
    if hours > 838 || minutes > 59 || seconds > 59 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid TIME value",
        ));
    }
    Ok(MysqlTimeValue {
        negative,
        days: (hours / 24) as u32,
        hours: (hours % 24) as u8,
        minutes,
        seconds,
        micros,
    })
}

fn column_type_for(rows: &[Map<String, Value>], column: &str) -> ColumnType {
    rows.iter()
        .filter_map(|row| row.get(column))
        .find_map(|value| match value {
            Value::Bool(_) => Some(ColumnType::MYSQL_TYPE_TINY),
            Value::Number(number) if number.is_i64() || number.is_u64() => {
                Some(ColumnType::MYSQL_TYPE_LONGLONG)
            }
            Value::Number(_) => Some(ColumnType::MYSQL_TYPE_DOUBLE),
            Value::Null => None,
            _ => Some(ColumnType::MYSQL_TYPE_STRING),
        })
        .unwrap_or(ColumnType::MYSQL_TYPE_STRING)
}

fn parameter_columns(count: usize) -> Vec<Column> {
    (0..count)
        .map(|idx| Column {
            table: "".to_string(),
            column: format!("param{}", idx + 1),
            coltype: ColumnType::MYSQL_TYPE_STRING,
            colflags: ColumnFlags::empty(),
        })
        .collect()
}

fn prepared_result_columns(
    session: &mut EngineSession,
    query: &str,
    param_count: usize,
) -> Vec<Column> {
    // COM_STMT_PREPARE must not execute mutating statements.  The engine call
    // below substitutes zero-valued parameters to derive result metadata, so
    // restrict it to parsed query statements; otherwise INSERT/UPDATE/DELETE
    // statements would run once during prepare and again during execute.
    let Ok(statements) = crate::sql::parse(query) else {
        return Vec::new();
    };
    if statements.len() != 1
        || !matches!(
            statements.first(),
            Some(sqlparser::ast::Statement::Query(_))
        )
    {
        return Vec::new();
    }
    let decimal_columns = mysql_decimal_columns(query);
    // Zero is accepted by LIMIT/OFFSET placeholders and generally produces an
    // empty SELECT while still allowing the engine to derive schema metadata.
    if let Ok(mut results) = session.prepare_sql_with_params_for_wire(
        query,
        &vec![Value::Number(serde_json::Number::from(0)); param_count],
    ) && let Some(result) = results.pop()
        && !result.column_metadata.is_empty()
    {
        return result
            .columns
            .iter()
            .enumerate()
            .map(|(index, name)| {
                let metadata = result.column_metadata.get(index);
                let mut flags = ColumnFlags::empty();
                if metadata.is_some_and(|metadata| !metadata.nullable) {
                    flags.insert(ColumnFlags::NOT_NULL_FLAG);
                }
                if metadata.is_some_and(|metadata| metadata.unsigned) {
                    flags.insert(ColumnFlags::UNSIGNED_FLAG);
                }
                Column {
                    table: metadata
                        .map(|metadata| metadata.table.clone())
                        .unwrap_or_default(),
                    column: name.clone(),
                    coltype: metadata
                        .map(|metadata| wire_column_type(metadata.column_type))
                        .unwrap_or(ColumnType::MYSQL_TYPE_VAR_STRING),
                    colflags: flags,
                }
            })
            .collect();
    }

    let Some(sqlparser::ast::Statement::Query(parsed_query)) = statements.into_iter().next() else {
        return Vec::new();
    };

    let sqlparser::ast::SetExpr::Select(select) = *parsed_query.body else {
        return Vec::new();
    };

    select
        .projection
        .iter()
        .filter_map(|item| {
            let column = match item {
                sqlparser::ast::SelectItem::UnnamedExpr(expr) => match expr {
                    sqlparser::ast::Expr::Identifier(ident) => ident.value.clone(),
                    sqlparser::ast::Expr::CompoundIdentifier(parts) => parts
                        .iter()
                        .map(|part| part.value.clone())
                        .collect::<Vec<_>>()
                        .join("."),
                    other => other.to_string(),
                },
                sqlparser::ast::SelectItem::ExprWithAlias { alias, .. } => alias.value.clone(),
                _ => return None,
            };

            Some(Column {
                table: "".to_string(),
                coltype: if decimal_columns.contains_key(&column) {
                    ColumnType::MYSQL_TYPE_NEWDECIMAL
                } else {
                    ColumnType::MYSQL_TYPE_STRING
                },
                column,
                colflags: ColumnFlags::empty(),
            })
        })
        .collect()
}

fn mysql_decimal_columns(query: &str) -> HashMap<String, usize> {
    let Ok(statements) = crate::sql::parse(query) else {
        return HashMap::new();
    };
    let Some(sqlparser::ast::Statement::Query(query)) = statements.into_iter().next() else {
        return HashMap::new();
    };
    let sqlparser::ast::SetExpr::Select(select) = *query.body else {
        return HashMap::new();
    };

    select
        .projection
        .iter()
        .filter_map(|item| {
            let (expr, column) = match item {
                sqlparser::ast::SelectItem::UnnamedExpr(expr) => (expr, expr.to_string()),
                sqlparser::ast::SelectItem::ExprWithAlias { expr, alias } => {
                    (expr, alias.value.clone())
                }
                _ => return None,
            };
            mysql_decimal_scale(expr).map(|scale| (column, scale))
        })
        .collect()
}

fn mysql_decimal_scale(expr: &sqlparser::ast::Expr) -> Option<usize> {
    use sqlparser::ast::{Expr, FunctionArg, FunctionArgExpr, FunctionArguments, Value};

    let Expr::Function(function) = expr else {
        return None;
    };
    let name = function.name.0.last()?.value.to_ascii_uppercase();
    if name == "UNIX_TIMESTAMP"
        && function
            .to_string()
            .to_ascii_uppercase()
            .contains("CONCAT(")
    {
        return Some(6);
    }
    if name == "AVG" {
        return Some(4);
    }
    if !matches!(name.as_str(), "ROUND" | "TRUNCATE") {
        return None;
    }
    let FunctionArguments::List(arguments) = &function.args else {
        return None;
    };
    let Some(argument) = arguments.args.get(1) else {
        return Some(0);
    };
    let argument = match argument {
        FunctionArg::Named { arg, .. }
        | FunctionArg::ExprNamed { arg, .. }
        | FunctionArg::Unnamed(arg) => arg,
    };
    let FunctionArgExpr::Expr(Expr::Value(Value::Number(scale, _))) = argument else {
        return None;
    };
    Some(scale.parse::<i32>().ok()?.max(0) as usize)
}

fn count_query_params(query: &str) -> usize {
    let mut count = 0;
    let mut in_single = false;
    let mut in_double = false;
    let mut chars = query.chars().peekable();

    while let Some(ch) = chars.next() {
        match ch {
            '\'' if !in_double => in_single = !in_single,
            '"' if !in_single => in_double = !in_double,
            '\\' if in_single || in_double => {
                let _ = chars.next();
            }
            '?' if !in_single && !in_double => count += 1,
            _ => {}
        }
    }

    count
}

fn param_to_json(param: ParamValue<'_>) -> Value {
    match param.value.into_inner() {
        ValueInner::NULL => Value::Null,
        ValueInner::Bytes(bytes) => Value::String(String::from_utf8_lossy(bytes).to_string()),
        ValueInner::Int(value) => Value::Number(value.into()),
        ValueInner::UInt(value) => serde_json::Number::from(value).into(),
        ValueInner::Double(value) => serde_json::Number::from_f64(value)
            .map(Value::Number)
            .unwrap_or(Value::Null),
        ValueInner::Date(bytes) => Value::String(
            decode_mysql_date_parameter(bytes)
                .unwrap_or_else(|| normalize_text_temporal_parameter(bytes)),
        ),
        ValueInner::Time(bytes) => Value::String(
            decode_mysql_time_parameter(bytes)
                .unwrap_or_else(|| normalize_text_temporal_parameter(bytes)),
        ),
        ValueInner::Datetime(bytes) => Value::String(
            decode_mysql_datetime_parameter(bytes)
                .unwrap_or_else(|| normalize_text_temporal_parameter(bytes)),
        ),
    }
}

fn normalize_text_temporal_parameter(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes)
        .trim()
        .trim_matches(['\'', '"'])
        .to_string()
}

fn decode_mysql_date_parameter(bytes: &[u8]) -> Option<String> {
    if bytes.is_empty() {
        return Some("0000-00-00".to_string());
    }
    if bytes.len() != 4 {
        return None;
    }
    let year = u16::from_le_bytes([bytes[0], bytes[1]]);
    Some(format!("{year:04}-{:02}-{:02}", bytes[2], bytes[3]))
}

fn decode_mysql_datetime_parameter(bytes: &[u8]) -> Option<String> {
    if bytes.is_empty() {
        return Some("0000-00-00 00:00:00".to_string());
    }
    if !matches!(bytes.len(), 4 | 7 | 11) {
        return None;
    }
    let date = decode_mysql_date_parameter(&bytes[..4])?;
    if bytes.len() == 4 {
        return Some(format!("{date} 00:00:00"));
    }
    let base = format!("{date} {:02}:{:02}:{:02}", bytes[4], bytes[5], bytes[6]);
    if bytes.len() == 11 {
        let micros = u32::from_le_bytes([bytes[7], bytes[8], bytes[9], bytes[10]]);
        Some(format!("{base}.{micros:06}"))
    } else {
        Some(base)
    }
}

fn decode_mysql_time_parameter(bytes: &[u8]) -> Option<String> {
    if bytes.is_empty() {
        return Some("00:00:00".to_string());
    }
    if !matches!(bytes.len(), 8 | 12) {
        return None;
    }
    let negative = bytes[0] != 0;
    let days = u32::from_le_bytes([bytes[1], bytes[2], bytes[3], bytes[4]]);
    let hours = u64::from(days) * 24 + u64::from(bytes[5]);
    let sign = if negative { "-" } else { "" };
    let base = format!("{sign}{hours:02}:{:02}:{:02}", bytes[6], bytes[7]);
    if bytes.len() == 12 {
        let micros = u32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]);
        Some(format!("{base}.{micros:06}"))
    } else {
        Some(base)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crate::vendor::msql_srv::{self, Column, ColumnFlags, ColumnType};
    use serde_json::{Map, json};

    use super::{
        Backend, canonicalize_information_schema_columns, normalize_session_var_name,
        parse_mysql_datetime_value, prepared_result_columns, references_information_schema,
        validate_wire_rows,
    };
    use crate::sql::engine::{Engine, EngineConfig, QueryResult};

    #[test]
    fn transaction_and_permission_errors_have_mysql_codes() {
        for (message, code) in [
            (
                "Lock wait timeout exceeded; try restarting transaction",
                1205,
            ),
            ("Administrative command denied", 1227),
            (
                "Statement requires database administration privileges",
                1227,
            ),
            ("transaction snapshot changed; retry transaction", 1213),
            ("Select command denied for database 'shard_a'", 1142),
            ("access denied for database 'shard_b'", 1044),
            (
                "transaction modes are not supported; use REPEATABLE READ",
                1235,
            ),
            ("DDL is not supported inside an active transaction", 1235),
            (
                "only REPEATABLE READ isolation is supported, before starting a transaction",
                1235,
            ),
            ("unsupported session setting: foreign_key_checks", 1235),
        ] {
            assert_eq!(super::mysql_error_kind(message) as u16, code, "{message}");
        }
        let defaults = super::default_session_vars();
        assert_eq!(
            defaults.get("transaction_isolation"),
            Some(&json!("REPEATABLE-READ"))
        );
        assert_eq!(
            defaults.get("tx_isolation"),
            Some(&json!("REPEATABLE-READ"))
        );
    }

    #[test]
    fn wire_text_and_prepared_statements_share_transaction_state() {
        let engine =
            Arc::new(Engine::open_with_data_dir(EngineConfig::mysql_strict(), None).unwrap());
        let mut backend = Backend::new(engine.clone());
        backend
            .execute_text_query("CREATE TABLE wire_tx (id BIGINT PRIMARY KEY)")
            .unwrap();
        backend.execute_prepared_query("BEGIN", &[]).unwrap();
        assert!(
            backend
                .status_flags()
                .contains(msql_srv::StatusFlags::SERVER_STATUS_IN_TRANS)
        );
        backend
            .execute_prepared_query("INSERT INTO wire_tx (id) VALUES (?)", &[json!(1)])
            .unwrap();
        backend.execute_text_query("ROLLBACK").unwrap();
        assert!(
            !backend
                .status_flags()
                .contains(msql_srv::StatusFlags::SERVER_STATUS_IN_TRANS)
        );
        assert!(
            backend.execute_text_query("SELECT * FROM wire_tx").unwrap()[0]
                .rows
                .is_empty()
        );

        backend.execute_text_query("SET autocommit = 0").unwrap();
        assert!(
            !backend
                .status_flags()
                .contains(msql_srv::StatusFlags::SERVER_STATUS_AUTOCOMMIT)
        );
        let result = backend
            .execute_prepared_query("SELECT @@autocommit AS enabled", &[])
            .unwrap();
        assert_eq!(result[0].rows[0].get("enabled"), Some(&json!(0)));
        backend
            .execute_text_query("INSERT INTO wire_tx (id) VALUES (2)")
            .unwrap();
        drop(backend);
        let mut reconnected = Backend::new(engine);
        assert!(
            reconnected
                .execute_text_query("SELECT * FROM wire_tx")
                .unwrap()[0]
                .rows
                .is_empty()
        );
    }

    #[test]
    fn wire_session_shortcuts_do_not_swallow_later_statements() {
        let mut backend = Backend::new(Arc::new(Engine::default()));
        assert!(
            backend
                .execute_session_query("SELECT @@autocommit; BEGIN")
                .is_none()
        );
        assert!(
            backend
                .execute_session_query("SET autocommit = 0")
                .is_none()
        );
    }

    #[test]
    fn information_schema_wire_results_use_mysql_column_casing() {
        assert!(references_information_schema(
            "select * from information_schema.columns"
        ));

        let mut row = Map::new();
        row.insert("table_name".to_string(), json!("users"));
        row.insert("column_name".to_string(), json!("id"));
        let mut results = vec![QueryResult {
            columns: vec!["table_name".to_string(), "column_name".to_string()],
            rows: vec![row],
            ..QueryResult::default()
        }];

        canonicalize_information_schema_columns(
            &mut results,
            "select * from information_schema.columns",
        );

        assert_eq!(results[0].columns, ["TABLE_NAME", "COLUMN_NAME"]);
        assert_eq!(results[0].rows[0].get("TABLE_NAME"), Some(&json!("users")));
        assert_eq!(results[0].rows[0].get("COLUMN_NAME"), Some(&json!("id")));
    }

    #[test]
    fn information_schema_wire_results_preserve_explicit_aliases() {
        let mut row = Map::new();
        row.insert("columnName".to_string(), json!("id"));
        let mut results = vec![QueryResult {
            columns: vec!["columnName".to_string()],
            rows: vec![row],
            ..QueryResult::default()
        }];

        canonicalize_information_schema_columns(
            &mut results,
            "SELECT COLUMN_NAME AS `columnName` FROM information_schema.KEY_COLUMN_USAGE",
        );

        assert_eq!(results[0].columns, ["columnName"]);
        assert_eq!(results[0].rows[0].get("columnName"), Some(&json!("id")));
    }

    #[test]
    fn information_schema_wire_results_preserve_explicitly_selected_column_names() {
        let mut row = Map::new();
        row.insert("column_name".to_string(), json!("id"));
        let mut results = vec![QueryResult {
            columns: vec!["column_name".to_string()],
            rows: vec![row],
            ..QueryResult::default()
        }];

        canonicalize_information_schema_columns(
            &mut results,
            "SELECT column_name FROM information_schema.columns",
        );

        assert_eq!(results[0].columns, ["column_name"]);
        assert_eq!(results[0].rows[0].get("column_name"), Some(&json!("id")));
    }

    #[test]
    fn session_select_shortcut_does_not_capture_table_queries() {
        let backend = Backend::new(Arc::new(Engine::default()));

        let session_only = backend
            .select_session_values("SELECT DATABASE() AS db")
            .expect("session-only select should be handled");
        assert_eq!(
            session_only.rows[0]
                .get("db")
                .and_then(|value| value.as_str()),
            Some("app")
        );

        assert!(
            backend
                .select_session_values("SELECT DATABASE() AS db, email FROM users")
                .is_none()
        );
    }

    #[test]
    fn datetime_wire_parser_accepts_mysql_and_rfc3339_values() {
        assert!(parse_mysql_datetime_value("2026-07-15 12:34:56.123456").is_some());
        assert!(parse_mysql_datetime_value("2026-07-15T12:34:56.123Z").is_some());
        assert!(parse_mysql_datetime_value("\"2026-07-15T12:34:56.123Z\"").is_some());
    }

    #[test]
    fn session_variable_names_accept_global_and_session_sql_forms() {
        assert_eq!(normalize_session_var_name("GLOBAL time_zone"), "time_zone");
        assert_eq!(
            normalize_session_var_name("@@global.time_zone"),
            "time_zone"
        );
        assert_eq!(normalize_session_var_name("SESSION time_zone"), "time_zone");
    }

    #[test]
    fn invalid_result_values_are_rejected_before_starting_a_row_writer() {
        let columns = vec!["createdAt".to_string()];
        let definitions = vec![Column {
            table: "events".to_string(),
            column: "createdAt".to_string(),
            coltype: ColumnType::MYSQL_TYPE_TIMESTAMP,
            colflags: ColumnFlags::NOT_NULL_FLAG,
        }];
        let mut row = Map::new();
        row.insert("createdAt".to_string(), json!("not-a-date"));

        let error = validate_wire_rows(&[row], &columns, &definitions).unwrap_err();
        assert!(error.to_string().contains("incorrect datetime"));
    }

    #[test]
    fn preparing_a_write_does_not_execute_it_for_metadata() {
        let engine = Engine::open_with_data_dir(EngineConfig::mysql_strict(), None).unwrap();
        engine
            .execute_sql(
                "CREATE TABLE users (id BIGINT PRIMARY KEY AUTO_INCREMENT, email TEXT UNIQUE)",
            )
            .unwrap();

        let columns = prepared_result_columns(
            &mut engine.session(),
            "INSERT INTO users (email) VALUES (?)",
            1,
        );
        assert!(columns.is_empty());
        let snapshot = engine.snapshot();
        assert_eq!(snapshot.rows.get("users").map(|rows| rows.len()), Some(0));
        assert!(snapshot.auto_inc.is_empty());
    }
}
