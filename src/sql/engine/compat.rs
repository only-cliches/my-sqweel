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
            '\\' if in_single || in_double => {
                current.push(ch);
                if let Some(escaped) = chars.next() {
                    current.push(escaped);
                }
            }
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

pub(super) fn parse_alter_table_drop_index(sql: &str) -> Option<(String, String)> {
    let tokens = normalized_sql_tokens(sql);
    if tokens.len() < 6
        || !tokens[0].eq_ignore_ascii_case("ALTER")
        || !tokens[1].eq_ignore_ascii_case("TABLE")
        || !tokens[3].eq_ignore_ascii_case("DROP")
        || !matches!(tokens[4].to_ascii_uppercase().as_str(), "INDEX" | "KEY")
    {
        return None;
    }

    let index_position = if tokens
        .get(5)
        .is_some_and(|token| token.eq_ignore_ascii_case("IF"))
        && tokens
            .get(6)
            .is_some_and(|token| token.eq_ignore_ascii_case("EXISTS"))
    {
        7
    } else {
        5
    };
    let table = unqualified_sql_identifier(tokens.get(2)?);
    let index = unqualified_sql_identifier(tokens.get(index_position)?);
    (!table.is_empty() && !index.is_empty()).then_some((table, index))
}

fn unqualified_sql_identifier(identifier: &str) -> String {
    identifier
        .rsplit('.')
        .next()
        .unwrap_or(identifier)
        .trim_matches(['`', '"'])
        .to_string()
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

pub(super) fn parse_show_full_columns_table(sql: &str) -> Option<String> {
    let tokens = normalized_sql_tokens(sql);
    let upper = tokens
        .iter()
        .map(|token| token.to_ascii_uppercase())
        .collect::<Vec<_>>();
    if upper.first()? != "SHOW"
        || upper.get(1)? != "FULL"
        || !matches!(upper.get(2).map(String::as_str), Some("COLUMNS" | "FIELDS"))
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

pub(super) fn show_databases_result(sql: &str) -> QueryResult {
    let filtered = sql.to_ascii_uppercase().contains(" LIKE ");
    let column = if filtered {
        "Database (x)".to_string()
    } else {
        "Database".to_string()
    };
    let databases: Vec<&str> = if filtered {
        Vec::new()
    } else {
        vec!["app", "information_schema"]
    };
    let rows = databases
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
        column_metadata: vec![],
        rows,
        warnings: vec![],
    }
}

pub(super) fn show_global_variables_result() -> QueryResult {
    let columns = vec!["Variable_name".to_string(), "Value".to_string()];
    let variables = [
        "version",
        "version_comment",
        "autocommit",
        "sql_mode",
        "time_zone",
        "transaction_isolation",
        "tx_isolation",
        "character_set_client",
        "character_set_connection",
        "character_set_results",
        "collation_connection",
        "max_allowed_packet",
        "max_binlog_stmt_cache_size",
        "have_innodb",
        "have_ssl",
        "performance_schema",
    ];
    let rows = variables
        .into_iter()
        .map(|name| {
            let mut row = Map::new();
            row.insert("Variable_name".to_string(), Value::String(name.to_string()));
            row.insert("Value".to_string(), session_variable_default(name));
            row
        })
        .collect();
    QueryResult {
        rows_affected: 0,
        last_insert_id: 0,
        columns,
        column_metadata: vec![],
        rows,
        warnings: vec![],
    }
}

pub(super) fn show_status_result(sql: &str) -> QueryResult {
    let columns = vec!["Variable_name".to_string(), "Value".to_string()];
    let mut rows = Vec::new();
    if sql.to_ascii_uppercase().contains("THREADS_CONNECTED") {
        let mut row = Map::new();
        row.insert(
            "Variable_name".to_string(),
            Value::String("Threads_connected".to_string()),
        );
        row.insert("Value".to_string(), Value::String("1".to_string()));
        rows.push(row);
    }
    QueryResult {
        rows_affected: 0,
        last_insert_id: 0,
        columns,
        column_metadata: vec![],
        rows,
        warnings: vec![],
    }
}

pub(super) fn select_system_variables(sql: &str) -> Option<QueryResult> {
    if !sql.contains("@@") {
        return None;
    }
    let expression = sql
        .trim()
        .strip_prefix("SELECT")
        .or_else(|| sql.trim().strip_prefix("select"))?
        .trim();
    if !expression.contains(',') {
        let mut row = Map::new();
        row.insert(expression.to_string(), system_variable_fallback(expression));
        return Some(QueryResult {
            rows_affected: 0,
            last_insert_id: 0,
            columns: vec![expression.to_string()],
            column_metadata: vec![],
            rows: vec![row],
            warnings: vec![],
        });
    }
    let Ok(statements) = crate::sql::parse(sql) else {
        let mut row = Map::new();
        row.insert(expression.to_string(), system_variable_fallback(expression));
        return Some(QueryResult {
            rows_affected: 0,
            last_insert_id: 0,
            columns: vec![expression.to_string()],
            column_metadata: vec![],
            rows: vec![row],
            warnings: vec![],
        });
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
        let value = eval::eval_expr(&expr, &Map::new(), 0).unwrap_or(Value::Bool(false));
        let column = alias.unwrap_or_else(|| expr.to_string());
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

fn system_variable_fallback(expression: &str) -> Value {
    let normalized = expression
        .chars()
        .filter(|character| !character.is_ascii_whitespace() && *character != '`')
        .collect::<String>();
    if normalized.starts_with("@@")
        && !normalized
            .chars()
            .any(|character| matches!(character, '=' | '&' | '|'))
    {
        session_variable_default(normalized.trim_start_matches("@@"))
    } else {
        Value::Bool(false)
    }
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
        "max_binlog_stmt_cache_size" => Value::Number(Number::from(4294963200_u64)),
        "log_bin" => Value::Number(Number::from(0)),
        "binlog_format" => Value::String("ROW".to_string()),
        "sql_require_primary_key" => Value::Number(Number::from(0)),
        "log_bin_trust_function_creators" => Value::Number(Number::from(1)),
        _ => Value::String(String::new()),
    }
}
