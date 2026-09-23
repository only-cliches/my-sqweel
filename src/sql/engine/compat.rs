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

pub(super) fn split_sql_statements(sql: &str) -> Result<Vec<String>> {
    use sqlparser::tokenizer::{Token, Whitespace};
    let mut out = Vec::new();
    let mut current = String::new();
    for (token, text) in crate::sql::sql_tokens(sql)? {
        match token {
            Token::SemiColon => {
                if !current.trim().is_empty() {
                    out.push(current.trim().to_owned());
                }
                current.clear();
            }
            Token::Whitespace(
                Whitespace::SingleLineComment { .. } | Whitespace::MultiLineComment(_),
            ) => current.push(' '),
            _ => current.push_str(text),
        }
    }
    if !current.trim().is_empty() {
        out.push(current.trim().to_owned());
    }
    Ok(out)
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

pub(super) fn system_variable_expr_value(expr: &Expr) -> Option<Value> {
    let name = match expr {
        Expr::Identifier(name) => name.value.clone(),
        Expr::Value(SqlValue::Placeholder(name)) => name.clone(),
        Expr::CompoundIdentifier(parts) => parts
            .iter()
            .map(|part| part.value.as_str())
            .collect::<Vec<_>>()
            .join("."),
        _ => return None,
    };
    name.strip_prefix("@@").map(eval_system_variable)
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
        "datadir" => Value::String("/tmp/my-sqweel-mysql/test/".into()),
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
