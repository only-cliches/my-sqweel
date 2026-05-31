use super::*;

pub(super) fn is_read_sql(sql: &str) -> bool {
    let trimmed = sql.trim_start();
    let keyword = trimmed
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .trim_matches('`')
        .to_ascii_uppercase();
    matches!(
        keyword.as_str(),
        "SELECT" | "SHOW" | "DESCRIBE" | "DESC" | "EXPLAIN" | "WITH"
    )
}

pub(super) fn split_sql_statements(sql: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut in_single = false;
    let mut in_double = false;
    let mut in_backtick = false;
    let mut chars = sql.chars().peekable();

    while let Some(ch) = chars.next() {
        match ch {
            '\'' if !in_double && !in_backtick => {
                current.push(ch);
                if in_single && chars.peek() == Some(&'\'') {
                    current.push(chars.next().expect("peeked quote"));
                } else {
                    in_single = !in_single;
                }
            }
            '"' if !in_single && !in_backtick => {
                in_double = !in_double;
                current.push(ch);
            }
            '`' if !in_single && !in_double => {
                in_backtick = !in_backtick;
                current.push(ch);
            }
            ';' if !in_single && !in_double && !in_backtick => {
                let statement = current.trim();
                if !statement.is_empty() {
                    out.push(statement.to_string());
                }
                current.clear();
            }
            _ => current.push(ch),
        }
    }
    let statement = current.trim();
    if !statement.is_empty() {
        out.push(statement.to_string());
    }
    out
}

pub(super) fn parse_show_columns_table(sql: &str) -> Option<String> {
    let tokens = normalized_sql_tokens(sql);
    let upper = tokens
        .iter()
        .map(|token| token.to_ascii_uppercase())
        .collect::<Vec<_>>();
    if upper.first()? != "SHOW"
        || !matches!(upper.get(1).map(String::as_str), Some("COLUMNS" | "FIELDS"))
    {
        return None;
    }
    upper
        .iter()
        .position(|token| token == "FROM" || token == "IN")
        .and_then(|idx| tokens.get(idx + 1).cloned())
}

pub(super) fn parse_describe_table(sql: &str) -> Option<String> {
    let tokens = normalized_sql_tokens(sql);
    let first = tokens.first()?.to_ascii_uppercase();
    (first == "DESCRIBE" || first == "DESC")
        .then(|| tokens.get(1).cloned())
        .flatten()
}

pub(super) fn parse_show_index_table(sql: &str) -> Option<String> {
    let tokens = normalized_sql_tokens(sql);
    let upper = tokens
        .iter()
        .map(|token| token.to_ascii_uppercase())
        .collect::<Vec<_>>();
    if upper.first()? != "SHOW"
        || !matches!(
            upper.get(1).map(String::as_str),
            Some("INDEX" | "INDEXES" | "KEYS")
        )
    {
        return None;
    }
    upper
        .iter()
        .position(|token| token == "FROM" || token == "IN")
        .and_then(|idx| tokens.get(idx + 1).cloned())
}

pub(super) fn parse_show_create_table(sql: &str) -> Option<String> {
    let tokens = normalized_sql_tokens(sql);
    if tokens.len() >= 4
        && tokens[0].eq_ignore_ascii_case("SHOW")
        && tokens[1].eq_ignore_ascii_case("CREATE")
        && tokens[2].eq_ignore_ascii_case("TABLE")
    {
        return tokens.get(3).cloned();
    }
    None
}

pub(super) fn parse_rename_table(sql: &str) -> Option<(String, String)> {
    let tokens = normalized_sql_tokens(sql);
    if tokens.len() >= 5
        && tokens[0].eq_ignore_ascii_case("RENAME")
        && tokens[1].eq_ignore_ascii_case("TABLE")
        && tokens[3].eq_ignore_ascii_case("TO")
    {
        return Some((tokens[2].clone(), tokens[4].clone()));
    }
    None
}

pub(super) fn show_databases_result() -> QueryResult {
    let column = "Database".to_string();
    let rows = ["app", "information_schema"]
        .into_iter()
        .map(|db| {
            let mut row = Map::new();
            row.insert(column.clone(), Value::String(db.to_string()));
            row
        })
        .collect();
    QueryResult {
        rows_affected: 0,
        last_insert_id: 0,
        columns: vec![column],
        rows,
    }
}

pub(super) fn select_system_variables(sql: &str) -> Option<QueryResult> {
    let Ok(statements) = crate::sql::parse(sql) else {
        return None;
    };
    let Some(Statement::Query(query)) = statements.into_iter().next() else {
        return None;
    };
    let SetExpr::Select(select) = *query.body else {
        return None;
    };
    if !select.from.is_empty() {
        return None;
    }

    let mut row = Map::new();
    let mut columns = Vec::new();
    for item in select.projection {
        let (expr, alias) = match item {
            SelectItem::UnnamedExpr(expr) => (expr, None),
            SelectItem::ExprWithAlias { expr, alias } => (expr, Some(alias.value)),
            _ => return None,
        };
        let value = system_variable_expr_value(&expr)?;
        let column = alias.unwrap_or_else(|| expr.to_string());
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

pub(super) fn system_variable_expr_value(expr: &Expr) -> Option<Value> {
    let normalized = expr
        .to_string()
        .chars()
        .filter(|ch| !ch.is_whitespace() && *ch != '`')
        .collect::<String>();
    normalized
        .starts_with("@@")
        .then(|| session_variable_default(normalized.trim_start_matches("@@")))
}

pub(super) fn session_variable_default(name: &str) -> Value {
    match name
        .trim_matches('`')
        .split('.')
        .next_back()
        .unwrap_or(name)
        .to_ascii_lowercase()
        .as_str()
    {
        "version" => Value::String("8.0.0-my-sqweel".to_string()),
        "version_comment" => Value::String("MySqweel".to_string()),
        "autocommit" => Value::Number(Number::from(1)),
        "sql_mode" => Value::String(String::new()),
        "time_zone" => Value::String("+00:00".to_string()),
        "transaction_isolation" | "tx_isolation" => Value::String("REPEATABLE-READ".to_string()),
        "character_set_client" | "character_set_connection" | "character_set_results" => {
            Value::String("utf8mb4".to_string())
        }
        "collation_connection" => Value::String("utf8mb4_general_ci".to_string()),
        "max_allowed_packet" => Value::Number(Number::from(67108864)),
        _ => Value::String(String::new()),
    }
}
