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
        Expr::Identifier(identifier) if identifier.value.eq_ignore_ascii_case("DEFAULT") => {
            Ok(sql_default_value())
        }
        Expr::Nested(expr) => eval_insert_update_value(expr, existing, incoming),
        Expr::UnaryOp { op, expr } if op.to_string() == "-" => {
            let value = eval_insert_update_value(expr, existing, incoming)?;
            if value == Value::Null {
                return Ok(Value::Null);
            }
            if let Some(integer) = json_to_i128_exact(&value)
                .and_then(|integer| integer.checked_neg())
                .and_then(|integer| i64::try_from(integer).ok())
            {
                return Ok(Value::Number(Number::from(integer)));
            }
            Ok(number_from_f64(-json_to_f64_lossy(&value)?))
        }
        Expr::UnaryOp { op, expr } if op.to_string() == "+" => {
            let value = eval_insert_update_value(expr, existing, incoming)?;
            if value == Value::Null {
                Ok(Value::Null)
            } else if let Some(integer) =
                json_to_i128_exact(&value).and_then(|integer| i64::try_from(integer).ok())
            {
                Ok(Value::Number(Number::from(integer)))
            } else {
                Ok(number_from_f64(json_to_f64_lossy(&value)?))
            }
        }
        Expr::UnaryOp { op, expr }
            if op.to_string().eq_ignore_ascii_case("NOT") || op.to_string() == "!" =>
        {
            Ok(sql_not_value(eval_insert_update_value(
                expr, existing, incoming,
            )?))
        }
        Expr::UnaryOp { op, expr } if op.to_string() == "~" => {
            let value = eval_insert_update_value(expr, existing, incoming)?;
            if value == Value::Null {
                Ok(Value::Null)
            } else {
                Ok(Value::Number(Number::from(
                    !(json_to_f64_lossy(&value)? as i64),
                )))
            }
        }
        Expr::UnaryOp { op, .. } => Err(anyhow!("unsupported unary operator: {op}")),
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
        Expr::IsTrue(expr) => Ok(Value::Bool(matches!(
            sql_truth(&eval_insert_update_value(expr, existing, incoming)?),
            SqlTruth::True
        ))),
        Expr::IsNotTrue(expr) => Ok(Value::Bool(!matches!(
            sql_truth(&eval_insert_update_value(expr, existing, incoming)?),
            SqlTruth::True
        ))),
        Expr::IsFalse(expr) => Ok(Value::Bool(matches!(
            sql_truth(&eval_insert_update_value(expr, existing, incoming)?),
            SqlTruth::False
        ))),
        Expr::IsNotFalse(expr) => Ok(Value::Bool(!matches!(
            sql_truth(&eval_insert_update_value(expr, existing, incoming)?),
            SqlTruth::False
        ))),
        Expr::InList {
            expr,
            list,
            negated,
        } => {
            let value = eval_insert_update_value(expr, existing, incoming)?;
            let candidates = list
                .iter()
                .map(|item| eval_insert_update_value(item, existing, incoming))
                .collect::<Result<Vec<_>>>()?;
            Ok(eval_in_values(value, candidates, *negated))
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
            Ok(eval_between_values(value, low, high, *negated))
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
        Expr::RLike {
            expr,
            pattern,
            negated,
            ..
        } => {
            let target = eval_insert_update_value(expr, existing, incoming)?;
            let pattern = eval_insert_update_value(pattern, existing, incoming)?;
            eval_regexp_match_values(target, pattern, *negated)
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
    unique_key_with_prefixes(data, unique_cols, &[])
}

pub(super) fn unique_key_with_prefixes(
    data: &Map<String, Value>,
    unique_cols: &[String],
    prefix_lengths: &[Option<u32>],
) -> Option<String> {
    if unique_cols.is_empty() {
        return None;
    }
    let mut parts = Vec::with_capacity(unique_cols.len());
    for (index, column) in unique_cols.iter().enumerate() {
        let value = data.get(column)?;
        if value == &Value::Null {
            return None;
        }
        let value = match (value, prefix_lengths.get(index).copied().flatten()) {
            (Value::String(value), Some(length)) => {
                Value::String(value.chars().take(length as usize).collect())
            }
            _ => value.clone(),
        };
        parts.push(encode_json_value(&value));
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

    let sql_type = hint.sql_type.as_deref().unwrap_or_default();

    if ascii_contains_ignore_case(sql_type, "int") || sql_type.eq_ignore_ascii_case("serial") {
        return coerce_number(value.clone()).unwrap_or(value);
    }

    if ascii_contains_ignore_case(sql_type, "bool") || sql_type.eq_ignore_ascii_case("tinyint(1)") {
        return coerce_bool(value.clone()).unwrap_or(value);
    }

    // Floating-point columns must store their values as f64 even when an
    // integral literal (e.g. `3800`) is inserted. Otherwise the value is kept
    // as a JSON integer and the result-set metadata reports the column as
    // LONGLONG instead of DOUBLE, which makes MySQL clients return it as a
    // string (bigNumberStrings) and breaks numeric consumers downstream.
    if ascii_contains_ignore_case(sql_type, "double")
        || ascii_contains_ignore_case(sql_type, "float")
        || ascii_contains_ignore_case(sql_type, "real")
    {
        if ascii_contains_ignore_case(sql_type, "double")
            || ascii_contains_ignore_case(sql_type, "real")
        {
            return coerce_double(value.clone()).unwrap_or(value);
        }
        let coerced = coerce_float(value.clone()).unwrap_or(value);
        if let (Value::Number(number), Some((_, scale))) =
            (&coerced, decimal_precision_scale(&sql_type))
            && let Some(number) = number.as_f64()
            && let Some(number) = Number::from_f64(
                (number * 10_f64.powi(scale as i32)).round() / 10_f64.powi(scale as i32),
            )
        {
            return Value::Number(number);
        }
        return coerced;
    }

    if ascii_starts_with_ignore_case(sql_type, "date")
        && !ascii_starts_with_ignore_case(sql_type, "datetime")
    {
        return match value {
            Value::String(value) if value.len() >= 10 => Value::String(value[..10].to_string()),
            Value::String(value) => compact_mysql_date(&value)
                .map(Value::String)
                .unwrap_or(Value::String(value)),
            Value::Number(value) => compact_mysql_date(&value.to_string())
                .map(Value::String)
                .unwrap_or_else(|| Value::String(value.to_string())),
            other => other,
        };
    }

    if ascii_contains_ignore_case(sql_type, "char")
        || ascii_contains_ignore_case(sql_type, "text")
        || ascii_contains_ignore_case(sql_type, "date")
        || ascii_contains_ignore_case(sql_type, "time")
        || ascii_contains_ignore_case(sql_type, "decimal")
    {
        return match value {
            Value::String(value)
                if (ascii_starts_with_ignore_case(sql_type, "datetime")
                    || ascii_starts_with_ignore_case(sql_type, "timestamp"))
                    && value.len() == 10 =>
            {
                Value::String(format!("{value} 00:00:00"))
            }
            Value::String(value)
                if (ascii_starts_with_ignore_case(sql_type, "datetime")
                    || ascii_starts_with_ignore_case(sql_type, "timestamp"))
                    && value.contains(' ') =>
            {
                let (date, time) = value.split_once(' ').unwrap_or((&value, "00:00:00"));
                let parts = time
                    .split(':')
                    .chain(std::iter::repeat("00"))
                    .take(3)
                    .map(|part| format!("{part:0>2}"))
                    .collect::<Vec<_>>();
                Value::String(format!("{date} {}:{}:{}", parts[0], parts[1], parts[2]))
            }
            Value::String(_) => value,
            Value::Number(value)
                if ascii_contains_ignore_case(sql_type, "datetime")
                    || ascii_contains_ignore_case(sql_type, "timestamp") =>
            {
                compact_mysql_datetime(&value.to_string())
                    .map(Value::String)
                    .unwrap_or_else(|| Value::String(value.to_string()))
            }
            other => Value::String(json_scalar_to_string(&other)),
        };
    }

    if ascii_starts_with_ignore_case(sql_type, "bit") {
        if let Value::String(value) = value {
            let bits = value
                .strip_prefix("B'")
                .or_else(|| value.strip_prefix("b'"))
                .and_then(|bits| bits.strip_suffix('\''));
            if let Some(bits) = bits
                && let Ok(number) = u8::from_str_radix(bits, 2)
            {
                return Value::String(char::from(number).to_string());
            }
            return Value::String(value);
        }
    }

    if ascii_starts_with_ignore_case(sql_type, "binary") {
        let Some(length) = first_type_number(&sql_type) else {
            return value;
        };
        return match value {
            Value::String(mut value) => {
                let byte_len = value.len();
                if byte_len < length {
                    value.extend(std::iter::repeat('\0').take(length - byte_len));
                }
                Value::String(value)
            }
            other => other,
        };
    }

    if ascii_contains_ignore_case(sql_type, "json") {
        return match value {
            Value::String(s) => match serde_json::from_str::<Value>(&s) {
                // Preserve the serialized form of JSON strings. Otherwise a
                // JSON document such as `"text"` becomes the indistinguishable
                // SQL string `text` and fails validation on the next step.
                Ok(Value::String(_)) => Value::String(s),
                Ok(value) => mark_json_nulls(value),
                Err(_) => Value::String(s),
            },
            other => other,
        };
    }

    value
}

fn ascii_contains_ignore_case(value: &str, needle: &str) -> bool {
    value
        .as_bytes()
        .windows(needle.len())
        .any(|window| window.eq_ignore_ascii_case(needle.as_bytes()))
}

fn ascii_starts_with_ignore_case(value: &str, prefix: &str) -> bool {
    value
        .as_bytes()
        .get(..prefix.len())
        .is_some_and(|start| start.eq_ignore_ascii_case(prefix.as_bytes()))
}

pub(super) fn validate_mysql_column_value(
    column: &str,
    value: &Value,
    hint: &ColumnHint,
) -> Result<()> {
    if value == &Value::Null {
        return Ok(());
    }
    let declared = hint
        .sql_type
        .as_deref()
        .unwrap_or_default()
        .to_ascii_uppercase();

    if (declared.contains("CHAR") || declared.contains("BINARY"))
        && let Some(limit) = first_type_number(&declared)
        && let Value::String(value) = value
    {
        let length = if declared.contains("BINARY") {
            value
                .strip_prefix(MYSQL_BINARY_SENTINEL)
                .map_or(value.len(), |hex| hex.len() / 2)
        } else {
            value.chars().count()
        };
        if length > limit {
            return Err(anyhow!("data too long for column '{column}'"));
        }
    }

    if declared.contains("INT") || declared == "SERIAL" {
        let text = match value {
            Value::Number(value) => value.to_string(),
            Value::Bool(value) => i32::from(*value).to_string(),
            Value::String(value) => value.clone(),
            _ => return Err(anyhow!("incorrect integer value for column '{column}'")),
        };
        let number = match value {
            Value::Number(value) => value
                .as_i64()
                .map(i128::from)
                .or_else(|| value.as_u64().map(i128::from))
                .or_else(|| value.as_f64().map(|value| value as i128))
                .ok_or_else(|| anyhow!("incorrect integer value for column '{column}'"))?,
            _ => text
                .parse::<i128>()
                .map_err(|_| anyhow!("incorrect integer value for column '{column}'"))?,
        };
        let unsigned = declared.contains("UNSIGNED") || declared == "SERIAL";
        let (minimum, maximum) = if declared.starts_with("TINYINT") {
            if unsigned { (0, 255) } else { (-128, 127) }
        } else if declared.starts_with("SMALLINT") {
            if unsigned {
                (0, 65_535)
            } else {
                (-32_768, 32_767)
            }
        } else if declared.starts_with("MEDIUMINT") {
            if unsigned {
                (0, 16_777_215)
            } else {
                (-8_388_608, 8_388_607)
            }
        } else if declared.starts_with("BIGINT") || declared == "SERIAL" {
            if unsigned {
                (0, u64::MAX as i128)
            } else {
                (i64::MIN as i128, i64::MAX as i128)
            }
        } else if unsigned {
            (0, u32::MAX as i128)
        } else {
            (i32::MIN as i128, i32::MAX as i128)
        };
        if number < minimum || number > maximum {
            return Err(anyhow!("out of range value for column '{column}'"));
        }
    }

    if declared.starts_with("DECIMAL") || declared.starts_with("NUMERIC") {
        let text = match value {
            Value::String(value) => value.clone(),
            Value::Number(value) => value.to_string(),
            _ => return Err(anyhow!("incorrect decimal value for column '{column}'")),
        };
        let normalized = text.trim().trim_start_matches(['+', '-']);
        if normalized.parse::<f64>().is_err() {
            return Err(anyhow!("incorrect decimal value for column '{column}'"));
        }
        if let Some((precision, scale)) = decimal_precision_scale(&declared) {
            let integer_digits = normalized
                .split_once('.')
                .map(|(integer, _)| integer)
                .unwrap_or(normalized)
                .trim_start_matches('0')
                .len();
            if integer_digits > precision.saturating_sub(scale) {
                return Err(anyhow!("out of range value for column '{column}'"));
            }
        }
    }

    if declared.starts_with("DATE") && !declared.starts_with("DATETIME") {
        let text = value
            .as_str()
            .map(ToOwned::to_owned)
            .or_else(|| value.as_i64().map(|value| value.to_string()))
            .ok_or_else(|| anyhow!("incorrect date value for column '{column}'"))?;
        if text == "0000-00-00" {
            return Ok(());
        }
        if text.len() == 10
            && text.as_bytes()[4] == b'-'
            && text.as_bytes()[7] == b'-'
            && text[..4].chars().all(|c| c.is_ascii_digit())
            && text[5..7].chars().all(|c| c.is_ascii_digit())
            && text[8..].chars().all(|c| c.is_ascii_digit())
        {
            return Ok(());
        }
        let valid = NaiveDate::parse_from_str(&text, "%Y-%m-%d").is_ok()
            || NaiveDateTime::parse_from_str(&text, "%Y-%m-%d %H:%M:%S").is_ok()
            || NaiveDateTime::parse_from_str(&text, "%Y-%m-%d %H:%M:%S%.f").is_ok()
            || compact_mysql_date(&text).is_some();
        if !valid {
            return Err(anyhow!("incorrect date value for column '{column}'"));
        }
    }
    if declared.starts_with("DATETIME") || declared.starts_with("TIMESTAMP") {
        let text = value
            .as_str()
            .map(ToOwned::to_owned)
            .or_else(|| value.as_i64().map(|value| value.to_string()))
            .ok_or_else(|| anyhow!("incorrect datetime value for column '{column}'"))?;
        if text == "0000-00-00 00:00:00" {
            return Ok(());
        }
        if text.len() >= 19
            && text.as_bytes()[4] == b'-'
            && text.as_bytes()[7] == b'-'
            && text.as_bytes()[10] == b' '
        {
            return Ok(());
        }
        let valid = NaiveDateTime::parse_from_str(&text, "%Y-%m-%d %H:%M:%S")
            .or_else(|_| NaiveDateTime::parse_from_str(&text, "%Y-%m-%d %H:%M:%S%.f"))
            .is_ok()
            || compact_mysql_datetime(&text).is_some();
        if !valid {
            return Err(anyhow!("incorrect datetime value for column '{column}'"));
        }
    }
    if declared.starts_with("TIME") && !declared.starts_with("TIMESTAMP") {
        let text = value
            .as_str()
            .ok_or_else(|| anyhow!("incorrect time value for column '{column}'"))?;
        if !is_valid_mysql_time(text) {
            return Err(anyhow!("incorrect time value for column '{column}'"));
        }
    }
    if declared.starts_with("JSON")
        && let Value::String(value) = value
        && !is_json_null(value)
        && serde_json::from_str::<Value>(value).is_err()
    {
        return Err(anyhow!("invalid JSON text for column '{column}'"));
    }
    Ok(())
}

fn compact_mysql_date(value: &str) -> Option<String> {
    NaiveDate::parse_from_str(value, "%Y%m%d")
        .or_else(|_| NaiveDate::parse_from_str(value, "%y%m%d"))
        .ok()
        .map(|value| value.format("%Y-%m-%d").to_string())
}

fn compact_mysql_datetime(value: &str) -> Option<String> {
    NaiveDateTime::parse_from_str(value, "%Y%m%d%H%M%S")
        .ok()
        .map(|value| value.format("%Y-%m-%d %H:%M:%S").to_string())
}

fn is_valid_mysql_time(value: &str) -> bool {
    let value = value.strip_prefix(['+', '-']).unwrap_or(value);
    let mut parts = value.split(':');
    let hours = parts.next().and_then(|part| part.parse::<u16>().ok());
    let minutes = parts.next().and_then(|part| part.parse::<u8>().ok());
    let seconds = parts.next();
    if parts.next().is_some() {
        return false;
    }
    let Some((seconds, fraction)) = seconds.map(|seconds| {
        seconds
            .split_once('.')
            .map(|(seconds, fraction)| (seconds, Some(fraction)))
            .unwrap_or((seconds, None))
    }) else {
        return false;
    };
    let seconds = seconds.parse::<u8>().ok();
    hours.is_some_and(|hours| hours <= 838)
        && minutes.is_some_and(|minutes| minutes <= 59)
        && seconds.is_some_and(|seconds| seconds <= 59)
        && fraction.is_none_or(|fraction| {
            !fraction.is_empty()
                && fraction.len() <= 6
                && fraction.bytes().all(|byte| byte.is_ascii_digit())
        })
}

fn first_type_number(declared: &str) -> Option<usize> {
    let (_, tail) = declared.split_once('(')?;
    tail.split(|character: char| !character.is_ascii_digit())
        .find(|part| !part.is_empty())?
        .parse()
        .ok()
}

fn decimal_precision_scale(declared: &str) -> Option<(usize, usize)> {
    let (_, tail) = declared.split_once('(')?;
    let body = tail.split_once(')').map(|(body, _)| body).unwrap_or(tail);
    let mut parts = body.split(',').map(str::trim);
    let precision = parts.next()?.parse().ok()?;
    let scale = parts
        .next()
        .and_then(|scale| scale.parse().ok())
        .unwrap_or(0);
    Some((precision, scale))
}

pub(super) fn coerce_number(value: Value) -> Option<Value> {
    match value {
        Value::Number(number) => number
            .as_i64()
            .map(|value| Value::Number(Number::from(value)))
            .or_else(|| {
                number
                    .as_f64()
                    .map(|value| Value::Number(Number::from(value.round() as i64)))
            }),
        Value::Bool(value) => Some(Value::Number(Number::from(i64::from(value)))),
        Value::String(value) => value
            .parse::<i64>()
            .map(|value| Value::Number(Number::from(value)))
            .ok()
            .or_else(|| {
                value
                    .parse::<u64>()
                    .ok()
                    .map(|value| Value::Number(Number::from(value)))
            })
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

pub(super) fn coerce_float(value: Value) -> Option<Value> {
    match value {
        Value::Number(number) => number
            .as_f64()
            .and_then(|value| Number::from_f64(value as f32 as f64))
            .map(Value::Number),
        Value::Bool(value) => Number::from_f64(if value { 1.0 } else { 0.0 }).map(Value::Number),
        Value::String(value) => value.parse::<f32>().ok().map(|value| {
            Value::Number(
                Number::from_f64(value as f64)
                    .expect("finite f32 values always convert to JSON numbers"),
            )
        }),
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
        Value::String(value) if is_json_null(value) => "null".to_string(),
        Value::String(value) if value.starts_with(JSON_EXTRACT_TEXT_SENTINEL) => value
            .strip_prefix(JSON_EXTRACT_TEXT_SENTINEL)
            .unwrap_or_default()
            .to_string(),
        Value::String(value) => value.clone(),
        Value::Bool(value) => value.to_string(),
        Value::Number(value) => value.to_string(),
        Value::Null => "null".to_string(),
        Value::Array(_) | Value::Object(_) => {
            serde_json::to_string(&public_json_value(value)).unwrap_or_else(|_| value.to_string())
        }
    }
}

pub(super) fn sql_value_to_json(v: &SqlValue) -> Result<Value> {
    match v {
        SqlValue::Null => Ok(Value::Null),
        SqlValue::Boolean(b) => Ok(Value::Bool(*b)),
        SqlValue::Number(n, _) => {
            if let Ok(i) = n.parse::<i64>() {
                Ok(Value::Number(Number::from(i)))
            } else if let Ok(u) = n.parse::<u64>() {
                Ok(Value::Number(Number::from(u)))
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
        SqlValue::HexStringLiteral(value) => {
            let digits = value.trim_start_matches("0x").trim_start_matches("0X");
            if let Ok(number) = u64::from_str_radix(digits, 16) {
                Ok(Value::Number(Number::from(number)))
            } else {
                Ok(Value::String(value.clone()))
            }
        }
        _ => Ok(Value::String(v.to_string())),
    }
}

pub(crate) fn substitute_params(sql: &str, params: &[Value]) -> Result<String> {
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
