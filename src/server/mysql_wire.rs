use std::collections::HashMap;
use std::io;
use std::net::TcpListener;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::Duration;

use msql_srv::{
    Column, ColumnFlags, ColumnType, ErrorKind, InitWriter, MysqlIntermediary, MysqlShim,
    ParamParser, ParamValue, QueryResultWriter, StatementMetaWriter, ValueInner,
};
use serde_json::{Map, Value};

use crate::sql::engine::{Engine, QueryResult};

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
        listener.set_nonblocking(true)?;
        while !stop.load(Ordering::Relaxed) {
            match listener.accept() {
                Ok((stream, _)) => self.spawn_session(stream),
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(err) => {
                    tracing::warn!(error = %err, "failed accepting mysql connection");
                    std::thread::sleep(Duration::from_millis(50));
                }
            }
        }
        Ok(())
    }

    fn spawn_session(&self, stream: std::net::TcpStream) {
        let backend = Backend::new(self.engine.clone());
        std::thread::spawn(move || {
            if let Err(err) = stream.set_nonblocking(false) {
                tracing::warn!(error = %err, "failed setting mysql session stream to blocking mode");
                return;
            }
            if let Err(err) = MysqlIntermediary::run_on_tcp(backend, stream) {
                tracing::warn!(error = %err, "mysql session ended with error");
            }
        });
    }
}

struct Backend {
    engine: Arc<Engine>,
    next_stmt_id: AtomicU32,
    statements: HashMap<u32, PreparedStatement>,
    last_insert_id: u64,
    current_db: String,
    session_vars: HashMap<String, Value>,
}

struct PreparedStatement {
    sql: String,
    param_count: usize,
}

impl Backend {
    fn new(engine: Arc<Engine>) -> Self {
        Self {
            engine,
            next_stmt_id: AtomicU32::new(1),
            statements: HashMap::new(),
            last_insert_id: 0,
            current_db: "app".to_string(),
            session_vars: default_session_vars(),
        }
    }
}

impl<W: io::Read + io::Write> MysqlShim<W> for Backend {
    type Error = io::Error;

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
        let columns = prepared_result_columns(query);
        info.reply(stmt_id, &params, &columns)
    }

    fn on_execute(
        &mut self,
        id: u32,
        params: ParamParser<'_>,
        results: QueryResultWriter<'_, W>,
    ) -> io::Result<()> {
        let Some(statement) = self.statements.get(&id) else {
            return results.completed(0, 0);
        };
        let params = params.into_iter().map(param_to_json).collect::<Vec<_>>();
        if params.len() != statement.param_count {
            tracing::debug!(
                expected = statement.param_count,
                actual = params.len(),
                "prepared parameter count mismatch"
            );
            return results.completed(0, 0);
        }
        let out = if is_last_insert_id_query(&statement.sql) {
            Ok(vec![last_insert_id_result(self.last_insert_id)])
        } else {
            self.engine.execute_sql_with_params(&statement.sql, &params)
        };
        write_query_items(out, results, &mut self.last_insert_id)
    }

    fn on_close(&mut self, stmt: u32) {
        self.statements.remove(&stmt);
    }

    fn on_init(&mut self, schema: &str, writer: InitWriter<'_, W>) -> io::Result<()> {
        if !schema.is_empty() {
            self.current_db = schema.to_string();
        }
        writer.ok()
    }

    fn on_query(&mut self, query: &str, results: QueryResultWriter<'_, W>) -> io::Result<()> {
        let out = if let Some(result) = self.execute_session_query(query) {
            Ok(vec![result])
        } else if is_last_insert_id_query(query) {
            Ok(vec![last_insert_id_result(self.last_insert_id)])
        } else {
            self.engine.execute_sql(query)
        };
        write_query_items(out, results, &mut self.last_insert_id)
    }
}

impl Backend {
    fn execute_session_query(&mut self, query: &str) -> Option<QueryResult> {
        let trimmed = query.trim().trim_end_matches(';').trim();
        let upper = trimmed.to_ascii_uppercase();
        if upper.starts_with("USE ") {
            self.current_db = trimmed[4..].trim().trim_matches('`').to_string();
            return Some(QueryResult::default());
        }
        if upper.starts_with("SET ") {
            self.apply_set_statement(&trimmed[4..]);
            return Some(QueryResult::default());
        }
        if upper.starts_with("SELECT ") {
            return self.select_session_values(trimmed);
        }
        None
    }

    fn apply_set_statement(&mut self, assignments: &str) {
        for assignment in split_sql_args_wire(assignments) {
            let Some((name, value)) = assignment.split_once('=') else {
                continue;
            };
            let name = normalize_session_var_name(name);
            self.session_vars
                .insert(name, parse_session_value(value.trim()));
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
            rows: vec![row],
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
            Value::String(self.current_db.clone())
        } else if normalized.starts_with("@@") {
            let name = normalize_session_var_name(&normalized);
            self.session_vars
                .get(&name)
                .cloned()
                .unwrap_or_else(|| Value::String(String::new()))
        } else {
            return None;
        };
        Some((alias.unwrap_or(expr_text), value))
    }
}

fn write_query_items<W: io::Read + io::Write>(
    items: anyhow::Result<Vec<QueryResult>>,
    results: QueryResultWriter<'_, W>,
    session_last_insert_id: &mut u64,
) -> io::Result<()> {
    match items {
        Ok(items) => {
            let out = items.into_iter().last().unwrap_or_default();
            if out.last_insert_id != 0 {
                *session_last_insert_id = out.last_insert_id;
            }
            write_result(results, out)
        }
        Err(err) => {
            tracing::debug!(error = %err, "query execution error");
            results.error(ErrorKind::ER_NOT_SUPPORTED_YET, err.to_string().as_bytes())
        }
    }
}

fn write_result<W: io::Read + io::Write>(
    results: QueryResultWriter<'_, W>,
    out: QueryResult,
) -> io::Result<()> {
    let mut columns = out.columns;
    if columns.is_empty()
        && let Some(row) = out.rows.first()
    {
        columns = row.keys().cloned().collect();
    }

    if columns.is_empty() {
        return results.completed(out.rows_affected, out.last_insert_id);
    }

    let defs: Vec<Column> = columns
        .iter()
        .map(|name| Column {
            table: "".to_string(),
            column: name.clone(),
            coltype: column_type_for(&out.rows, name),
            colflags: ColumnFlags::empty(),
        })
        .collect();

    let mut rw = results.start(&defs)?;
    for row in out.rows {
        write_row(&mut rw, &row, &columns)?;
        rw.end_row()?;
    }
    rw.finish()
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
        rows: vec![row],
    }
}

fn default_session_vars() -> HashMap<String, Value> {
    [
        ("autocommit", serde_json::json!(1)),
        ("sql_mode", serde_json::json!("")),
        ("time_zone", serde_json::json!("+00:00")),
        ("version", serde_json::json!("8.0.0-my-sqweel")),
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
    ]
    .into_iter()
    .map(|(key, value)| (key.to_string(), value))
    .collect()
}

fn normalize_session_var_name(name: &str) -> String {
    name.trim()
        .trim_start_matches("@@")
        .trim_start_matches("SESSION.")
        .trim_start_matches("session.")
        .trim_start_matches("GLOBAL.")
        .trim_start_matches("global.")
        .trim_matches('`')
        .to_ascii_lowercase()
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
) -> io::Result<()> {
    for key in columns {
        let value = row.get(key).cloned().unwrap_or(Value::Null);
        match value {
            Value::Null => rw.write_col(Option::<String>::None)?,
            Value::Bool(v) => rw.write_col(if v { 1_i64 } else { 0_i64 })?,
            Value::Number(n) => {
                if let Some(i) = n.as_i64() {
                    rw.write_col(i)?;
                } else if let Some(f) = n.as_f64() {
                    rw.write_col(f)?;
                } else {
                    rw.write_col(n.to_string())?;
                }
            }
            Value::String(s) => rw.write_col(s)?,
            other => rw.write_col(other.to_string())?,
        }
    }
    Ok(())
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

fn prepared_result_columns(query: &str) -> Vec<Column> {
    let Ok(statements) = crate::sql::parse(query) else {
        return Vec::new();
    };

    let Some(sqlparser::ast::Statement::Query(query)) = statements.into_iter().next() else {
        return Vec::new();
    };

    let sqlparser::ast::SetExpr::Select(select) = *query.body else {
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
                column,
                coltype: ColumnType::MYSQL_TYPE_STRING,
                colflags: ColumnFlags::empty(),
            })
        })
        .collect()
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
        ValueInner::Date(bytes) | ValueInner::Time(bytes) | ValueInner::Datetime(bytes) => {
            Value::String(String::from_utf8_lossy(bytes).to_string())
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::Backend;
    use crate::sql::engine::Engine;

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
}
