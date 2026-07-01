use super::*;

pub(super) fn eval_insert_update_value(
    expr: &Expr,
    existing: &Map<String, Value>,
    incoming: &Map<String, Value>,
) -> Result<Value> {
    if let Some(value) = incoming_value_expr(expr, incoming)? {
        return Ok(value);
    }

    match expr {
        Expr::Nested(expr) => eval_insert_update_value(expr, existing, incoming),
        Expr::UnaryOp { op, expr } if op.to_string() == "-" => {
            let value = eval_insert_update_value(expr, existing, incoming)?;
            Ok(number_from_f64(-json_to_f64_lossy(&value)?))
        }
        Expr::UnaryOp { op, expr } if op.to_string().eq_ignore_ascii_case("NOT") => {
            Ok(Value::Bool(!value_truthy(&eval_insert_update_value(
                expr, existing, incoming,
            )?)))
        }
        Expr::UnaryOp { expr, .. } => eval_insert_update_value(expr, existing, incoming),
        Expr::BinaryOp { left, op, right } => {
            let left = eval_insert_update_value(left, existing, incoming)?;
            let right = eval_insert_update_value(right, existing, incoming)?;
            eval_binary_values(left, op, right)
        }
        Expr::IsNull(expr) => Ok(Value::Bool(
            eval_insert_update_value(expr, existing, incoming)? == Value::Null,
        )),
        Expr::IsNotNull(expr) => Ok(Value::Bool(
            eval_insert_update_value(expr, existing, incoming)? != Value::Null,
        )),
        Expr::InList {
            expr,
            list,
            negated,
        } => {
            let value = eval_insert_update_value(expr, existing, incoming)?;
            let hit = list.iter().any(|item| {
                eval_insert_update_value(item, existing, incoming)
                    .map(|item| mysql_eq(&value, &item))
                    .unwrap_or(false)
            });
            Ok(Value::Bool(if *negated { !hit } else { hit }))
        }
        Expr::Between {
            expr,
            negated,
            low,
            high,
        } => {
            let value = eval_insert_update_value(expr, existing, incoming)?;
            let low = eval_insert_update_value(low, existing, incoming)?;
            let high = eval_insert_update_value(high, existing, incoming)?;
            let hit = !compare_predicate_values(value.clone(), low, |a, b| a < b)
                && !compare_predicate_values(value, high, |a, b| a > b);
            Ok(Value::Bool(if *negated { !hit } else { hit }))
        }
        Expr::Like {
            expr,
            pattern,
            negated,
            ..
        } => {
            let target = eval_insert_update_value(expr, existing, incoming)?;
            let pattern = eval_insert_update_value(pattern, existing, incoming)?;
            Ok(eval_like_values(target, pattern, *negated))
        }
        Expr::Case {
            operand,
            conditions,
            results,
            else_result,
        } => {
            for (condition, result) in conditions.iter().zip(results.iter()) {
                let matches = match operand {
                    Some(operand) => mysql_eq(
                        &eval_insert_update_value(operand, existing, incoming)?,
                        &eval_insert_update_value(condition, existing, incoming)?,
                    ),
                    None => value_truthy(&eval_insert_update_value(condition, existing, incoming)?),
                };
                if matches {
                    return eval_insert_update_value(result, existing, incoming);
                }
            }
            match else_result {
                Some(expr) => eval_insert_update_value(expr, existing, incoming),
                None => Ok(Value::Null),
            }
        }
        _ => eval_expr(expr, existing, 0),
    }
}

fn incoming_value_expr(expr: &Expr, incoming: &Map<String, Value>) -> Result<Option<Value>> {
    let Expr::Function(function) = expr else {
        return Ok(None);
    };
    if !object_name(&function.name)?.eq_ignore_ascii_case("VALUES") {
        return Ok(None);
    }
    let FunctionArguments::List(args) = &function.args else {
        return Ok(Some(Value::Null));
    };
    let Some(FunctionArg::Unnamed(FunctionArgExpr::Expr(expr))) = args.args.first() else {
        return Ok(Some(Value::Null));
    };
    let column = projection_expr_column_name(expr);
    Ok(Some(incoming.get(&column).cloned().unwrap_or(Value::Null)))
}

pub(super) fn assignment_target_name(assignment: &Assignment) -> String {
    assignment
        .target
        .to_string()
        .replace('`', "")
        .split('.')
        .next_back()
        .unwrap_or_default()
        .to_string()
}

pub(super) fn unique_key(data: &Map<String, Value>, unique_cols: &[String]) -> Option<String> {
    if unique_cols.is_empty() {
        return None;
    }
    let mut parts = Vec::with_capacity(unique_cols.len());
    for column in unique_cols {
        let value = data.get(column)?;
        if value == &Value::Null {
            return None;
        }
        parts.push(encode_json_value(value));
    }
    Some(parts.join(&UNIQUE_SEPARATOR.to_string()))
}

pub(super) fn unique_duplicate_report(
    schema: &TableSchemaHint,
    rows: &BTreeMap<String, StoredRow>,
) -> Vec<Value> {
    let mut out = Vec::new();
    for cols in &schema.unique {
        let mut seen: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for (pk, row) in rows {
            if let Some(key) = unique_key(&row.data, cols) {
                seen.entry(key).or_default().push(pk.clone());
            }
        }

        for (value, pks) in seen {
            if pks.len() > 1 {
                out.push(json!({
                    "columns": cols,
                    "value": value,
                    "rowIds": pks,
                }));
            }
        }
    }
    out
}

pub(super) fn coerce_value_for_column(value: Value, hint: &ColumnHint) -> Value {
    if value == Value::Null {
        return Value::Null;
    }

    let sql_type = hint
        .sql_type
        .as_deref()
        .unwrap_or_default()
        .to_ascii_lowercase();

    if sql_type.contains("int") || sql_type == "serial" {
        return coerce_number(value.clone()).unwrap_or(value);
    }

    if sql_type.contains("bool") || sql_type == "tinyint(1)" {
        return coerce_bool(value.clone()).unwrap_or(value);
    }

    // Floating-point columns must store their values as f64 even when an
    // integral literal (e.g. `3800`) is inserted. Otherwise the value is kept
    // as a JSON integer and the result-set metadata reports the column as
    // LONGLONG instead of DOUBLE, which makes MySQL clients return it as a
    // string (bigNumberStrings) and breaks numeric consumers downstream.
    if sql_type.contains("double") || sql_type.contains("float") || sql_type.contains("real") {
        return coerce_double(value.clone()).unwrap_or(value);
    }

    if sql_type.contains("char")
        || sql_type.contains("text")
        || sql_type.contains("date")
        || sql_type.contains("time")
        || sql_type.contains("decimal")
    {
        return match value {
            Value::String(_) => value,
            other => Value::String(json_scalar_to_string(&other)),
        };
    }

    if sql_type.contains("json") {
        return match value {
            Value::String(s) => serde_json::from_str(&s).unwrap_or(Value::String(s)),
            other => other,
        };
    }

    value
}

pub(super) fn coerce_number(value: Value) -> Option<Value> {
    match value {
        Value::Number(_) => Some(value),
        Value::Bool(value) => Some(Value::Number(Number::from(i64::from(value)))),
        Value::String(value) => value
            .parse::<i64>()
            .map(|value| Value::Number(Number::from(value)))
            .ok()
            .or_else(|| {
                value
                    .parse::<f64>()
                    .ok()
                    .and_then(Number::from_f64)
                    .map(Value::Number)
            }),
        other => Some(other),
    }
}

// Force a numeric value into an f64-backed JSON number so floating-point
// columns round-trip (and report) as DOUBLE even for integral inputs.
pub(super) fn coerce_double(value: Value) -> Option<Value> {
    match value {
        Value::Number(ref number) => number
            .as_f64()
            .and_then(Number::from_f64)
            .map(Value::Number),
        Value::Bool(value) => Number::from_f64(if value { 1.0 } else { 0.0 }).map(Value::Number),
        Value::String(value) => value
            .parse::<f64>()
            .ok()
            .and_then(Number::from_f64)
            .map(Value::Number),
        other => Some(other),
    }
}

pub(super) fn coerce_bool(value: Value) -> Option<Value> {
    match value {
        Value::Bool(_) => Some(value),
        Value::Number(number) => number.as_i64().map(|value| Value::Bool(value != 0)),
        Value::String(value) => match value.to_ascii_lowercase().as_str() {
            "true" | "1" => Some(Value::Bool(true)),
            "false" | "0" => Some(Value::Bool(false)),
            _ => None,
        },
        other => Some(other),
    }
}

pub(super) fn json_scalar_to_string(value: &Value) -> String {
    match value {
        Value::String(value) => value.clone(),
        Value::Bool(value) => value.to_string(),
        Value::Number(value) => value.to_string(),
        Value::Null => "null".to_string(),
        other => other.to_string(),
    }
}

pub(super) fn sql_value_to_json(v: &SqlValue) -> Result<Value> {
    match v {
        SqlValue::Null => Ok(Value::Null),
        SqlValue::Boolean(b) => Ok(Value::Bool(*b)),
        SqlValue::Number(n, _) => {
            if let Ok(i) = n.parse::<i64>() {
                Ok(Value::Number(Number::from(i)))
            } else if let Ok(f) = n.parse::<f64>() {
                Number::from_f64(f)
                    .map(Value::Number)
                    .ok_or_else(|| anyhow!("invalid float"))
            } else {
                Ok(Value::String(n.clone()))
            }
        }
        SqlValue::SingleQuotedString(s) | SqlValue::DoubleQuotedString(s) => {
            Ok(Value::String(s.clone()))
        }
        _ => Ok(Value::String(v.to_string())),
    }
}

pub(super) fn substitute_params(sql: &str, params: &[Value]) -> Result<String> {
    let mut out = String::with_capacity(sql.len() + params.len() * 8);
    let mut params = params.iter();
    let mut in_single = false;
    let mut in_double = false;
    let mut chars = sql.chars().peekable();

    while let Some(ch) = chars.next() {
        match ch {
            '\'' if !in_double => {
                in_single = !in_single;
                out.push(ch);
            }
            '"' if !in_single => {
                in_double = !in_double;
                out.push(ch);
            }
            '\\' if in_single || in_double => {
                out.push(ch);
                if let Some(next) = chars.next() {
                    out.push(next);
                }
            }
            '?' if !in_single && !in_double => {
                let value = params
                    .next()
                    .ok_or_else(|| anyhow!("not enough parameters for prepared statement"))?;
                out.push_str(&json_to_sql_literal(value));
            }
            _ => out.push(ch),
        }
    }

    if params.next().is_some() {
        return Err(anyhow!("too many parameters for prepared statement"));
    }
    Ok(out)
}

pub(super) fn json_to_sql_literal(value: &Value) -> String {
    match value {
        Value::Null => "NULL".to_string(),
        Value::Bool(v) => {
            if *v {
                "TRUE".to_string()
            } else {
                "FALSE".to_string()
            }
        }
        Value::Number(n) => n.to_string(),
        Value::String(s) => format!("'{}'", s.replace('\\', "\\\\").replace('\'', "''")),
        other => format!(
            "'{}'",
            other.to_string().replace('\\', "\\\\").replace('\'', "''")
        ),
    }
}
