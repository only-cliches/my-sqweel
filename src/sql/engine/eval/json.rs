use super::*;

pub(super) fn eval_json_extract(
    args: &[String],
    data: &Map<String, Value>,
    last_insert_id: u64,
) -> Result<Value> {
    let Some(first) = args.first() else {
        return Ok(Value::Null);
    };
    let document = eval_json_document(first, data, last_insert_id)?;
    if document == Value::Null {
        return Ok(Value::Null);
    }
    let mut matches = Vec::new();
    for path_arg in args.iter().skip(1) {
        let path = eval_scalar_text(path_arg, data, last_insert_id)?;
        let path = json_scalar_to_string(&path);
        let Some(value) = json_extract_path(&document, &path) else {
            return Ok(Value::Null);
        };
        matches.push(value);
    }
    match matches.len() {
        0 => Ok(Value::Null),
        1 => {
            let value = matches.pop().unwrap_or(Value::Null);
            json_extract_text_value(value)
        }
        _ => json_extract_text_value(Value::Array(matches)),
    }
}

pub(super) fn eval_json_query(
    args: &[String],
    data: &Map<String, Value>,
    last_insert_id: u64,
) -> Result<Value> {
    let Some(first) = args.first() else {
        return Ok(Value::Null);
    };
    let document = eval_json_document(first, data, last_insert_id)?;
    let Some(path_arg) = args.get(1) else {
        return match document {
            Value::Array(_) | Value::Object(_) => json_extract_text_value(document),
            _ => Ok(Value::Null),
        };
    };
    let path = json_scalar_to_string(&eval_scalar_text(path_arg, data, last_insert_id)?);
    let Some(value) = json_extract_path(&document, &path) else {
        return Ok(Value::Null);
    };
    match value {
        Value::Array(_) | Value::Object(_) => json_extract_text_value(value),
        _ => Ok(Value::Null),
    }
}

pub(super) fn json_text_value(value: Value) -> Result<Value> {
    if matches!(&value, Value::String(value) if is_json_null(value)) {
        return Ok(json_null_value());
    }
    Ok(Value::String(serde_json::to_string(&public_json_value(
        &value,
    ))?))
}

pub(crate) fn json_compact_text(value: &Value) -> Result<String> {
    Ok(serde_json::to_string(&public_json_value(value))?)
}

pub(crate) fn json_wire_text(value: &Value) -> Result<String> {
    mysql_json_text(&public_json_value(value))
}

fn mysql_json_text(value: &Value) -> Result<String> {
    match value {
        Value::Null => Ok("null".to_string()),
        Value::Bool(value) => Ok(value.to_string()),
        Value::Number(value) => Ok(value.to_string()),
        Value::String(value) if is_json_null(value) => Ok("null".to_string()),
        Value::String(value) => {
            serde_json::to_string(value).map_err(|error| anyhow!("invalid JSON string: {error}"))
        }
        Value::Array(values) => Ok(format!(
            "[{}]",
            values
                .iter()
                .map(mysql_json_text)
                .collect::<Result<Vec<_>>>()?
                .join(", ")
        )),
        Value::Object(values) => Ok(format!(
            "{{{}}}",
            values
                .iter()
                .map(|(key, value)| {
                    Ok(format!(
                        "{}: {}",
                        serde_json::to_string(key).unwrap_or_default(),
                        mysql_json_text(value)?
                    ))
                })
                .collect::<Result<Vec<_>>>()?
                .join(", ")
        )),
    }
}

/// MariaDB renders JSON_ARRAYAGG/JSON_OBJECTAGG results with aggregate-style
/// separators: arrays join with "," and objects join "key:value" pairs with
/// ", " (unlike JSON_ARRAY/JSON_OBJECT, which add spaces after colons too).
pub(crate) fn mysql_json_agg_text(value: &Value) -> Result<String> {
    match value {
        Value::Null => Ok("null".to_string()),
        Value::Bool(value) => Ok(value.to_string()),
        Value::Number(value) => Ok(value.to_string()),
        Value::String(value) if is_json_null(value) => Ok("null".to_string()),
        Value::String(value) => {
            serde_json::to_string(value).map_err(|error| anyhow!("invalid JSON string: {error}"))
        }
        Value::Array(values) => Ok(format!(
            "[{}]",
            values
                .iter()
                .map(mysql_json_agg_text)
                .collect::<Result<Vec<_>>>()?
                .join(",")
        )),
        Value::Object(values) => Ok(format!(
            "{{{}}}",
            values
                .iter()
                .map(|(key, value)| {
                    Ok(format!(
                        "{}:{}",
                        serde_json::to_string(key).unwrap_or_default(),
                        mysql_json_agg_text(value)?
                    ))
                })
                .collect::<Result<Vec<_>>>()?
                .join(", ")
        )),
    }
}

pub(super) fn eval_json_unquote(
    arg: Option<&String>,
    data: &Map<String, Value>,
    last_insert_id: u64,
) -> Result<Value> {
    let value = arg
        .map(|arg| eval_scalar_text(arg, data, last_insert_id))
        .transpose()?
        .unwrap_or(Value::Null);
    let value = json_extract_value(&value).unwrap_or(value);
    if value == Value::Null {
        return Ok(Value::Null);
    }
    if matches!(&value, Value::String(value) if is_json_null(value)) {
        return Ok(Value::String("null".to_string()));
    }
    match value {
        Value::String(value) => {
            if let Ok(Value::String(unquoted)) = serde_json::from_str::<Value>(&value) {
                Ok(Value::String(unquoted))
            } else {
                Ok(Value::String(value))
            }
        }
        other => Ok(Value::String(json_scalar_to_string(&other))),
    }
}

pub(super) fn json_extract_text_value(value: Value) -> Result<Value> {
    if matches!(&value, Value::String(value) if is_json_null(value)) {
        return Ok(json_null_value());
    }
    Ok(Value::String(format!(
        "{JSON_EXTRACT_TEXT_SENTINEL}{}",
        serde_json::to_string(&public_json_value(&value))?
    )))
}

pub(super) fn eval_json_object(
    args: &[String],
    data: &Map<String, Value>,
    last_insert_id: u64,
) -> Result<Value> {
    let mut object = Map::new();
    for pair in args.chunks(2) {
        let key = eval_scalar_text(&pair[0], data, last_insert_id)?;
        if key == Value::Null {
            return Ok(Value::Null);
        }
        let value = if let Some(value_arg) = pair.get(1) {
            eval_scalar_text(value_arg, data, last_insert_id)?
        } else {
            Value::Null
        };
        object.insert(
            json_scalar_to_string(&key),
            if value == Value::Null {
                json_null_value()
            } else {
                mark_json_nulls(value)
            },
        );
    }
    Ok(Value::Object(object))
}

pub(super) fn eval_json_array(
    args: &[String],
    data: &Map<String, Value>,
    last_insert_id: u64,
) -> Result<Value> {
    args.iter()
        .map(|arg| {
            eval_scalar_text(arg, data, last_insert_id).map(|value| {
                if value == Value::Null {
                    json_null_value()
                } else {
                    mark_json_nulls(value)
                }
            })
        })
        .collect::<Result<Vec<_>>>()
        .map(Value::Array)
}

pub(super) fn eval_json_contains(
    target_arg: Option<&String>,
    candidate_arg: Option<&String>,
    path_arg: Option<&String>,
    data: &Map<String, Value>,
    last_insert_id: u64,
) -> Result<Value> {
    let target = target_arg
        .map(|arg| eval_json_document(arg, data, last_insert_id))
        .transpose()?
        .unwrap_or(Value::Null);
    let candidate = candidate_arg
        .map(|arg| eval_json_document(arg, data, last_insert_id))
        .transpose()?
        .unwrap_or(Value::Null);
    if target == Value::Null || candidate == Value::Null {
        return Ok(Value::Null);
    }
    let target = if let Some(path_arg) = path_arg {
        let path = eval_scalar_text(path_arg, data, last_insert_id)?;
        json_extract_path(&target, &json_scalar_to_string(&path)).unwrap_or(Value::Null)
    } else {
        target
    };
    if target == Value::Null {
        return Ok(Value::Null);
    }
    Ok(Value::Number(Number::from(
        if json_contains_value(&target, &candidate) {
            1
        } else {
            0
        },
    )))
}

pub(super) fn eval_json_valid(
    arg: Option<&String>,
    data: &Map<String, Value>,
    last_insert_id: u64,
) -> Result<Value> {
    let Some(arg) = arg else {
        return Ok(Value::Null);
    };
    let value = eval_scalar_text(arg, data, last_insert_id)?;
    if value == Value::Null {
        return Ok(Value::Null);
    }
    let valid = match value {
        Value::String(value) if value.starts_with(MYSQL_BINARY_SENTINEL) => {
            let hex = value.trim_start_matches(MYSQL_BINARY_SENTINEL);
            let bytes = hex
                .as_bytes()
                .chunks_exact(2)
                .map(|pair| {
                    (pair[0] as char).to_digit(16).unwrap_or_default() * 16
                        + (pair[1] as char).to_digit(16).unwrap_or_default()
                })
                .map(|value| value as u8)
                .collect::<Vec<_>>();
            let text = String::from_utf8(bytes.clone())
                .unwrap_or_else(|_| bytes.into_iter().map(char::from).collect());
            serde_json::from_str::<Value>(&text).is_ok()
        }
        Value::String(value) => serde_json::from_str::<Value>(&value).is_ok(),
        _ => true,
    };
    Ok(Value::Number(Number::from(valid as u8)))
}

pub(super) fn eval_json_equals(
    left_arg: Option<&String>,
    right_arg: Option<&String>,
    data: &Map<String, Value>,
    last_insert_id: u64,
) -> Result<Value> {
    let Some(left_arg) = left_arg else {
        return Ok(Value::Null);
    };
    let Some(right_arg) = right_arg else {
        return Ok(Value::Null);
    };
    let left = eval_json_value_argument(left_arg, data, last_insert_id)?;
    let right = eval_json_value_argument(right_arg, data, last_insert_id)?;
    let (Some(left), Some(right)) = (left, right) else {
        return Ok(Value::Null);
    };
    if json_depth(&left) > 32 || json_depth(&right) > 32 {
        return Ok(Value::Null);
    }
    Ok(Value::Number(Number::from(
        (canonical_json(&left) == canonical_json(&right)) as u8,
    )))
}

pub(super) fn eval_json_normalize(
    arg: Option<&String>,
    data: &Map<String, Value>,
    last_insert_id: u64,
) -> Result<Value> {
    let Some(arg) = arg else {
        return Ok(Value::Null);
    };
    let Some(value) = eval_json_value_argument(arg, data, last_insert_id)? else {
        return Ok(Value::Null);
    };
    Ok(Value::String(canonical_json(&value)))
}

fn eval_json_value_argument(
    arg: &str,
    data: &Map<String, Value>,
    last_insert_id: u64,
) -> Result<Option<Value>> {
    let value = eval_scalar_text(arg, data, last_insert_id)?;
    if value == Value::Null {
        return Ok(None);
    }
    Ok(Some(parse_json_document_value(value)))
}

fn canonical_json(value: &Value) -> String {
    match value {
        Value::Null => "null".to_string(),
        Value::Bool(value) => value.to_string(),
        Value::Number(value) => value
            .as_f64()
            .map(|value| format!("{value:.1E}"))
            .unwrap_or_else(|| value.to_string()),
        Value::String(value) if is_json_null(value) => "null".to_string(),
        Value::String(value) => serde_json::to_string(value).unwrap_or_default(),
        Value::Array(values) => format!(
            "[{}]",
            values
                .iter()
                .map(canonical_json)
                .collect::<Vec<_>>()
                .join(",")
        ),
        Value::Object(values) => {
            let mut entries = values.iter().collect::<Vec<_>>();
            entries.sort_by(|left, right| left.0.cmp(right.0));
            format!(
                "{{{}}}",
                entries
                    .into_iter()
                    .map(|(key, value)| format!(
                        "{}:{}",
                        serde_json::to_string(key).unwrap_or_default(),
                        canonical_json(value)
                    ))
                    .collect::<Vec<_>>()
                    .join(",")
            )
        }
    }
}

pub(super) fn eval_json_type(
    arg: Option<&String>,
    data: &Map<String, Value>,
    last_insert_id: u64,
) -> Result<Value> {
    let Some(arg) = arg else {
        return Ok(Value::Null);
    };
    let value = eval_json_document(arg, data, last_insert_id)?;
    if value == Value::Null {
        return Ok(Value::Null);
    }
    Ok(Value::String(json_type_name(&value).to_string()))
}

fn json_type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "NULL",
        Value::Bool(_) => "BOOLEAN",
        Value::Number(number) if number.is_i64() => "INTEGER",
        Value::Number(number) if number.is_u64() => "UNSIGNED INTEGER",
        Value::Number(_) => "DOUBLE",
        Value::String(value) if is_json_null(value) => "NULL",
        Value::String(_) => "STRING",
        Value::Array(_) => "ARRAY",
        Value::Object(_) => "OBJECT",
    }
}

pub(super) fn eval_json_depth(
    arg: Option<&String>,
    data: &Map<String, Value>,
    last_insert_id: u64,
) -> Result<Value> {
    let Some(arg) = arg else {
        return Ok(Value::Null);
    };
    let value = eval_json_document(arg, data, last_insert_id)?;
    if value == Value::Null {
        return Ok(Value::Null);
    }
    Ok(Value::Number(Number::from(json_depth(&value))))
}

fn json_depth(value: &Value) -> u64 {
    match value {
        Value::Array(values) => 1 + values.iter().map(json_depth).max().unwrap_or(0),
        Value::Object(values) => 1 + values.values().map(json_depth).max().unwrap_or(0),
        _ => 1,
    }
}

pub(super) fn eval_json_length(
    args: &[String],
    data: &Map<String, Value>,
    last_insert_id: u64,
) -> Result<Value> {
    let Some(document_arg) = args.first() else {
        return Ok(Value::Null);
    };
    let document = eval_json_document(document_arg, data, last_insert_id)?;
    if document == Value::Null {
        return Ok(Value::Null);
    }
    let value = if let Some(path_arg) = args.get(1) {
        let path = eval_scalar_text(path_arg, data, last_insert_id)?;
        json_extract_path(&document, &json_scalar_to_string(&path)).unwrap_or(Value::Null)
    } else {
        document
    };
    if value == Value::Null {
        return Ok(Value::Null);
    }
    let length = match &value {
        Value::Array(values) => values.len(),
        Value::Object(values) => values.len(),
        _ => 1,
    };
    Ok(Value::Number(Number::from(length as u64)))
}

pub(super) fn eval_json_keys(
    args: &[String],
    data: &Map<String, Value>,
    last_insert_id: u64,
) -> Result<Value> {
    let Some(document_arg) = args.first() else {
        return Ok(Value::Null);
    };
    let document = eval_json_document(document_arg, data, last_insert_id)?;
    if document == Value::Null {
        return Ok(Value::Null);
    }
    let value = if let Some(path_arg) = args.get(1) {
        let path = eval_scalar_text(path_arg, data, last_insert_id)?;
        json_extract_path(&document, &json_scalar_to_string(&path)).unwrap_or(Value::Null)
    } else {
        document
    };
    let Some(object) = value.as_object() else {
        return Ok(Value::Null);
    };
    let keys = object
        .keys()
        .map(|key| serde_json::to_string(key).unwrap_or_default())
        .collect::<Vec<_>>()
        .join(", ");
    Ok(Value::String(format!("[{keys}]")))
}

pub(super) fn eval_json_contains_path(
    args: &[String],
    data: &Map<String, Value>,
    last_insert_id: u64,
) -> Result<Value> {
    let Some(document_arg) = args.first() else {
        return Ok(Value::Null);
    };
    let Some(mode_arg) = args.get(1) else {
        return Ok(Value::Null);
    };
    let document = eval_json_document(document_arg, data, last_insert_id)?;
    let mode = json_scalar_to_string(&eval_scalar_text(mode_arg, data, last_insert_id)?);
    if document == Value::Null {
        return Ok(Value::Null);
    }
    let found = args.iter().skip(2).map(|path_arg| {
        eval_scalar_text(path_arg, data, last_insert_id)
            .map(|path| !json_extract_matches(&document, &json_scalar_to_string(&path)).is_empty())
    });
    let found = found.collect::<Result<Vec<_>>>()?;
    let result = if mode.eq_ignore_ascii_case("ALL") {
        found.iter().all(|found| *found)
    } else {
        found.iter().any(|found| *found)
    };
    Ok(Value::Number(Number::from(result as u8)))
}

pub(super) fn eval_json_exists(
    document_arg: Option<&String>,
    path_arg: Option<&String>,
    data: &Map<String, Value>,
    last_insert_id: u64,
) -> Result<Value> {
    let Some(document_arg) = document_arg else {
        return Ok(Value::Null);
    };
    let Some(path_arg) = path_arg else {
        return Ok(Value::Null);
    };
    let document = eval_json_document(document_arg, data, last_insert_id)?;
    if document == Value::Null {
        return Ok(Value::Null);
    }
    let path = eval_scalar_text(path_arg, data, last_insert_id)?;
    let exists = !json_extract_matches(&document, &json_scalar_to_string(&path)).is_empty();
    Ok(Value::Number(Number::from(exists as u8)))
}

pub(super) fn eval_json_overlaps(
    left_arg: Option<&String>,
    right_arg: Option<&String>,
    data: &Map<String, Value>,
    last_insert_id: u64,
) -> Result<Value> {
    let left = left_arg
        .map(|arg| eval_json_document(arg, data, last_insert_id))
        .transpose()?
        .unwrap_or(Value::Null);
    let right = right_arg
        .map(|arg| eval_json_document(arg, data, last_insert_id))
        .transpose()?
        .unwrap_or(Value::Null);
    if left == Value::Null || right == Value::Null {
        return Ok(Value::Null);
    }
    let overlaps = match (&left, &right) {
        (Value::Array(left), Value::Array(right)) => left
            .iter()
            .any(|left| right.iter().any(|right| json_equal(left, right))),
        (Value::Object(left), Value::Object(right)) => left
            .iter()
            .any(|(key, value)| right.get(key).is_some_and(|other| json_equal(value, other))),
        (Value::Array(left), right) => left.iter().any(|value| json_equal(value, right)),
        (left, Value::Array(right)) => right.iter().any(|value| json_equal(left, value)),
        (left, right) => json_equal(left, right),
    };
    Ok(Value::Number(Number::from(overlaps as u8)))
}

fn json_equal(left: &Value, right: &Value) -> bool {
    public_json_value(left) == public_json_value(right)
}

pub(super) fn eval_json_quote(
    arg: Option<&String>,
    data: &Map<String, Value>,
    last_insert_id: u64,
) -> Result<Value> {
    let value = arg
        .map(|arg| eval_scalar_text(arg, data, last_insert_id))
        .transpose()?
        .unwrap_or(Value::Null);
    if value == Value::Null {
        return Ok(Value::Null);
    }
    Ok(Value::String(serde_json::to_string(
        &json_scalar_to_string(&value),
    )?))
}

pub(super) fn eval_json_pretty(
    arg: Option<&String>,
    data: &Map<String, Value>,
    last_insert_id: u64,
) -> Result<Value> {
    let Some(arg) = arg else {
        return Ok(Value::Null);
    };
    let value = eval_json_document(arg, data, last_insert_id)?;
    if value == Value::Null {
        return Ok(Value::Null);
    }
    let pretty = serde_json::to_string_pretty(&public_json_value(&value))?;
    let mut formatted = String::with_capacity(pretty.len());
    for (line_number, line) in pretty.split('\n').enumerate() {
        if line_number > 0 {
            formatted.push('\n');
        }
        let indent = line.len() - line.trim_start_matches(' ').len();
        let content = &line[indent..];
        formatted.extend(std::iter::repeat_n(' ', indent * 2));
        if content.len() > 1
            && (content.ends_with('{') || content.ends_with('['))
            && content[..content.len() - 1].ends_with(": ")
        {
            formatted.push_str(&content[..content.len() - 1]);
            formatted.push('\n');
            formatted.extend(std::iter::repeat_n(' ', indent * 2));
            formatted.push_str(&content[content.len() - 1..]);
        } else {
            formatted.push_str(content);
        }
    }
    Ok(Value::String(formatted))
}

pub(super) fn eval_json_merge(
    args: &[String],
    data: &Map<String, Value>,
    last_insert_id: u64,
    patch: bool,
) -> Result<Value> {
    let mut values = Vec::new();
    for arg in args {
        let value = eval_json_document(arg, data, last_insert_id)?;
        if value == Value::Null {
            return Ok(Value::Null);
        }
        values.push(value);
    }
    let Some(mut merged) = values.first().cloned() else {
        return Ok(Value::Null);
    };
    for value in values.into_iter().skip(1) {
        merged = if patch {
            json_merge_patch_value(merged, value)
        } else {
            json_merge_preserve_value(merged, value)
        };
    }
    Ok(merged)
}

fn json_merge_patch_value(target: Value, patch: Value) -> Value {
    let Value::Object(patch) = patch else {
        return patch;
    };
    let mut target = match target {
        Value::Object(target) => target,
        _ => Map::new(),
    };
    for (key, value) in patch {
        if value == Value::Null || is_json_null_value(&value) {
            target.remove(&key);
        } else {
            let previous = target.remove(&key).unwrap_or(Value::Null);
            target.insert(key, json_merge_patch_value(previous, value));
        }
    }
    Value::Object(target)
}

fn json_merge_preserve_value(left: Value, right: Value) -> Value {
    match (left, right) {
        (Value::Object(mut left), Value::Object(right)) => {
            for (key, right_value) in right {
                if let Some(left_value) = left.remove(&key) {
                    left.insert(key, json_merge_preserve_value(left_value, right_value));
                } else {
                    left.insert(key, right_value);
                }
            }
            Value::Object(left)
        }
        (Value::Array(mut left), Value::Array(right)) => {
            left.extend(right);
            Value::Array(left)
        }
        (Value::Array(mut left), right) => {
            left.push(right);
            Value::Array(left)
        }
        (left, right) => Value::Array(vec![left, right]),
    }
}

pub(super) fn eval_json_search(
    args: &[String],
    data: &Map<String, Value>,
    last_insert_id: u64,
) -> Result<Value> {
    if args.len() < 3 {
        return Ok(Value::Null);
    }
    let document = eval_json_document(&args[0], data, last_insert_id)?;
    let mode = json_scalar_to_string(&eval_scalar_text(&args[1], data, last_insert_id)?);
    let pattern = json_scalar_to_string(&eval_scalar_text(&args[2], data, last_insert_id)?);
    if document == Value::Null {
        return Ok(Value::Null);
    }
    let (escape, path_start) = if args.len() >= 4 {
        (
            json_scalar_to_string(&eval_scalar_text(&args[3], data, last_insert_id)?),
            4,
        )
    } else {
        ("\\".to_string(), 3)
    };
    let paths = if path_start < args.len() {
        args[path_start..]
            .iter()
            .map(|arg| eval_scalar_text(arg, data, last_insert_id))
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .map(|path| json_scalar_to_string(&path))
            .flat_map(|path| json_extract_matches_with_paths(&document, &path))
            .collect::<Vec<_>>()
    } else {
        json_extract_matches_with_paths(&document, "$.**")
    };
    let mut matches = Vec::new();
    for (path, value) in paths {
        if let Value::String(value) = value
            && !is_json_null(&value)
            && json_like_match(&value, &pattern, &escape)
        {
            matches.push(Value::String(path));
        }
    }
    if matches.is_empty() {
        return Ok(Value::Null);
    }
    if mode.eq_ignore_ascii_case("ONE") {
        json_text_value(matches.remove(0))
    } else {
        json_text_value(Value::Array(matches))
    }
}

pub(super) fn eval_json_value(
    args: &[String],
    data: &Map<String, Value>,
    last_insert_id: u64,
) -> Result<Value> {
    let Some(document_arg) = args.first() else {
        return Ok(Value::Null);
    };
    let Some(path_arg) = args.get(1) else {
        return Ok(Value::Null);
    };
    let (path_text, returning) = split_json_value_path(path_arg);
    let path = json_scalar_to_string(&eval_scalar_text(&path_text, data, last_insert_id)?);
    let document = eval_json_document(document_arg, data, last_insert_id)?;
    let Some(value) = json_extract_path(&document, &path) else {
        return Ok(Value::Null);
    };
    if matches!(value, Value::Array(_) | Value::Object(_)) {
        return Ok(Value::Null);
    }
    let value = if is_json_null_value(&value) {
        Value::Null
    } else {
        value
    };
    if value == Value::Null {
        return Ok(Value::Null);
    }
    if let Some(returning) = returning {
        let returning = returning.trim().to_ascii_uppercase();
        if returning == "JSON" {
            return json_text_value(value);
        }
        if returning.contains("CHAR") || returning.contains("TEXT") {
            return Ok(Value::String(json_value_scalar_to_string(&value)));
        }
        return cast_json_value(value, &returning);
    }
    Ok(Value::String(json_value_scalar_to_string(&value)))
}
fn json_value_scalar_to_string(value: &Value) -> String {
    match value {
        Value::Bool(value) => if *value { "1" } else { "0" }.to_string(),
        _ => json_scalar_to_string(value),
    }
}

fn split_json_value_path(arg: &str) -> (String, Option<String>) {
    let mut quote = None;
    let mut escaped = false;
    for (index, character) in arg.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        if character == '\\' && quote.is_some() {
            escaped = true;
            continue;
        }
        if character == '\'' || character == '"' {
            if quote == Some(character) {
                quote = None;
            } else if quote.is_none() {
                quote = Some(character);
            }
            continue;
        }
        if quote.is_none() && character.is_ascii_whitespace() {
            let path = arg[..index].trim().to_string();
            let tail = arg[index..].trim();
            let returning = tail
                .strip_prefix("RETURNING")
                .or_else(|| tail.strip_prefix("returning"))
                .map(str::trim)
                .map(|value| value.split_whitespace().next().unwrap_or(value).to_string());
            return (path, returning);
        }
    }
    (arg.trim().to_string(), None)
}

pub(super) fn eval_json_schema_valid(
    args: &[String],
    data: &Map<String, Value>,
    last_insert_id: u64,
) -> Result<Value> {
    if args.len() < 2 {
        return Ok(Value::Null);
    }
    let schema = eval_json_document(&args[0], data, last_insert_id)?;
    let document = eval_json_document(&args[1], data, last_insert_id)?;
    if schema == Value::Null || document == Value::Null {
        return Ok(Value::Null);
    }
    Ok(Value::Number(Number::from(
        json_schema_matches(&schema, &document) as u8,
    )))
}

fn json_schema_matches(schema: &Value, value: &Value) -> bool {
    match schema {
        Value::Bool(valid) => *valid,
        Value::Object(schema) => {
            if let Some(expected) = schema.get("const")
                && !json_equal(expected, value)
            {
                return false;
            }
            if let Some(enum_values) = schema.get("enum").and_then(Value::as_array)
                && !enum_values
                    .iter()
                    .any(|candidate| json_equal(candidate, value))
            {
                return false;
            }
            if let Some(expected_type) = schema.get("type").and_then(Value::as_str)
                && !json_schema_type_matches(expected_type, value)
            {
                return false;
            }
            if let Some(required) = schema.get("required").and_then(Value::as_array)
                && let Some(object) = value.as_object()
                && required
                    .iter()
                    .any(|key| key.as_str().is_some_and(|key| !object.contains_key(key)))
            {
                return false;
            }
            if let Some(properties) = schema.get("properties").and_then(Value::as_object)
                && let Some(object) = value.as_object()
                && properties.iter().any(|(key, schema)| {
                    object
                        .get(key)
                        .is_some_and(|value| !json_schema_matches(schema, value))
                })
            {
                return false;
            }
            if let Some(items) = schema.get("items")
                && let Some(values) = value.as_array()
                && values
                    .iter()
                    .any(|value| !json_schema_matches(items, value))
            {
                return false;
            }
            if let Some(minimum) = schema.get("minimum").and_then(Value::as_f64)
                && value.as_f64().is_some_and(|value| value < minimum)
            {
                return false;
            }
            if let Some(maximum) = schema.get("maximum").and_then(Value::as_f64)
                && value.as_f64().is_some_and(|value| value > maximum)
            {
                return false;
            }
            if let Some(minimum) = schema.get("minLength").and_then(Value::as_u64)
                && value
                    .as_str()
                    .is_some_and(|value| value.chars().count() < minimum as usize)
            {
                return false;
            }
            if let Some(minimum) = schema.get("minItems").and_then(Value::as_u64)
                && value
                    .as_array()
                    .is_some_and(|value| value.len() < minimum as usize)
            {
                return false;
            }
            true
        }
        _ => false,
    }
}

fn json_schema_type_matches(expected: &str, value: &Value) -> bool {
    match expected {
        "null" => is_json_null_value(value) || value == &Value::Null,
        "boolean" => value.is_boolean(),
        "object" => value.is_object(),
        "array" => value.is_array(),
        "number" => value.is_number(),
        "integer" => value.as_i64().is_some() || value.as_u64().is_some(),
        "string" => value.is_string() && !is_json_null_value(value),
        _ => true,
    }
}

pub(super) fn eval_json_schema_report(
    args: &[String],
    data: &Map<String, Value>,
    last_insert_id: u64,
) -> Result<Value> {
    let valid = eval_json_schema_valid(args, data, last_insert_id)?;
    if valid == Value::Null {
        return Ok(Value::Null);
    }
    let is_valid = valid == Value::Number(Number::from(1_u8));
    let report = serde_json::json!({
        "valid": is_valid,
        "keywordLocation": "",
        "instanceLocation": "",
        "errors": if is_valid { serde_json::json!([]) } else { serde_json::json!([{"error": "JSON document does not match schema"}]) },
    });
    Ok(Value::String(report.to_string()))
}

pub(super) fn eval_json_storage(
    arg: Option<&String>,
    data: &Map<String, Value>,
    last_insert_id: u64,
    free: bool,
) -> Result<Value> {
    let Some(arg) = arg else {
        return Ok(Value::Null);
    };
    let value = eval_json_document(arg, data, last_insert_id)?;
    if value == Value::Null {
        return Ok(Value::Null);
    }
    if free {
        return Ok(Value::Number(Number::from(0_u8)));
    }
    let bytes = serde_json::to_vec(&public_json_value(&value))?;
    Ok(Value::Number(Number::from(bytes.len() as u64)))
}

fn is_json_null_value(value: &Value) -> bool {
    matches!(value, Value::String(value) if is_json_null(value))
}

fn json_like_match(value: &str, pattern: &str, escape: &str) -> bool {
    let escape = escape.chars().next().unwrap_or('\\');
    let mut tokens = Vec::new();
    let mut chars = pattern.chars();
    while let Some(ch) = chars.next() {
        if ch == escape {
            if let Some(next) = chars.next() {
                tokens.push((false, next));
            }
        } else if ch == '%' {
            tokens.push((true, ch));
        } else if ch == '_' {
            tokens.push((true, ch));
        } else {
            tokens.push((false, ch));
        }
    }
    fn matches(value: &[char], tokens: &[(bool, char)], vi: usize, ti: usize) -> bool {
        if ti == tokens.len() {
            return vi == value.len();
        }
        if tokens[ti].0 && tokens[ti].1 == '%' {
            (vi..=value.len()).any(|next| matches(value, tokens, next, ti + 1))
        } else if vi < value.len() && (tokens[ti].0 || tokens[ti].1 == value[vi]) {
            matches(value, tokens, vi + 1, ti + 1)
        } else {
            false
        }
    }
    matches(&value.chars().collect::<Vec<_>>(), &tokens, 0, 0)
}

#[derive(Debug, Clone, Copy)]
pub(super) enum JsonMutation {
    Set,
    Insert,
    Replace,
    Remove,
    ArrayAppend,
    ArrayInsert,
}

pub(super) fn eval_json_mutation(
    args: &[String],
    data: &Map<String, Value>,
    last_insert_id: u64,
    mutation: JsonMutation,
) -> Result<Value> {
    let Some(first) = args.first() else {
        return Ok(Value::Null);
    };
    let mut document = eval_json_document(first, data, last_insert_id)?;
    if document == Value::Null {
        return Ok(Value::Null);
    }

    match mutation {
        JsonMutation::Set | JsonMutation::Insert | JsonMutation::Replace => {
            for pair in args.iter().skip(1).collect::<Vec<_>>().chunks(2) {
                let Some(path_arg) = pair.first() else {
                    break;
                };
                let Some(value_arg) = pair.get(1) else {
                    break;
                };
                let path = eval_scalar_text(path_arg, data, last_insert_id)?;
                let value = eval_json_mutation_value(value_arg, data, last_insert_id)?;
                let value = if value == Value::Null {
                    json_null_value()
                } else {
                    mark_json_nulls(value)
                };
                let path = json_scalar_to_string(&path);
                let exists = json_extract_path(&document, &path).is_some();
                if matches!(mutation, JsonMutation::Set)
                    || (matches!(mutation, JsonMutation::Insert) && !exists)
                    || (matches!(mutation, JsonMutation::Replace) && exists)
                {
                    json_set_path(&mut document, &path, value);
                }
            }
        }
        JsonMutation::Remove => {
            for path_arg in args.iter().skip(1) {
                let path = eval_scalar_text(path_arg, data, last_insert_id)?;
                json_remove_path(&mut document, &json_scalar_to_string(&path));
            }
        }
        JsonMutation::ArrayAppend | JsonMutation::ArrayInsert => {
            for pair in args.iter().skip(1).collect::<Vec<_>>().chunks(2) {
                let Some(path_arg) = pair.first() else {
                    break;
                };
                let Some(value_arg) = pair.get(1) else {
                    break;
                };
                let path =
                    json_scalar_to_string(&eval_scalar_text(path_arg, data, last_insert_id)?);
                let value = eval_json_mutation_value(value_arg, data, last_insert_id)?;
                let value = if value == Value::Null {
                    json_null_value()
                } else {
                    mark_json_nulls(value)
                };
                if matches!(mutation, JsonMutation::ArrayAppend) {
                    json_array_append_path(&mut document, &path, value);
                } else {
                    json_array_insert_path(&mut document, &path, value);
                }
            }
        }
    }
    Ok(document)
}

fn json_array_append_path(document: &mut Value, path: &str, value: Value) -> bool {
    let Some(existing) = json_extract_path(document, path) else {
        return false;
    };
    let Some(tokens) = parse_json_path(path) else {
        return false;
    };
    if tokens
        .iter()
        .any(|token| matches!(token, JsonPathToken::Wildcard | JsonPathToken::Recursive))
    {
        return false;
    }
    let replacement = match existing {
        Value::Array(mut values) => {
            values.push(value);
            Value::Array(values)
        }
        existing => Value::Array(vec![existing, value]),
    };
    json_set_path(document, path, replacement)
}

fn json_array_insert_path(document: &mut Value, path: &str, value: Value) -> bool {
    let Some(tokens) = parse_json_path(path) else {
        return false;
    };
    let Some(JsonPathToken::Index(index)) = tokens.last() else {
        return false;
    };
    if tokens.len() == 1 {
        let Some(array) = document.as_array_mut() else {
            return false;
        };
        if *index <= array.len() {
            array.insert(*index, value);
            return true;
        }
        return false;
    }
    let parent_path = json_path_to_string(&tokens[..tokens.len() - 1]);
    let Some(parent) = json_extract_path(document, &parent_path) else {
        return false;
    };
    let Value::Array(mut array) = parent else {
        return false;
    };
    if *index > array.len() {
        return false;
    }
    array.insert(*index, value);
    json_set_path(document, &parent_path, Value::Array(array))
}

fn json_path_to_string(tokens: &[JsonPathToken]) -> String {
    let mut path = "$".to_string();
    for token in tokens {
        match token {
            JsonPathToken::Key(key) => path.push_str(&format!(".{key}")),
            JsonPathToken::Index(index) => path.push_str(&format!("[{index}]")),
            JsonPathToken::Wildcard => path.push_str("[*]"),
            JsonPathToken::Recursive => path.push_str(".**"),
        }
    }
    path
}

fn eval_json_document(arg: &str, data: &Map<String, Value>, last_insert_id: u64) -> Result<Value> {
    let value = eval_scalar_text(arg, data, last_insert_id)?;
    Ok(parse_json_document_value(value))
}

fn eval_json_mutation_value(
    arg: &str,
    data: &Map<String, Value>,
    last_insert_id: u64,
) -> Result<Value> {
    let value = eval_scalar_text(arg, data, last_insert_id)?;
    if value == Value::Null {
        return Ok(Value::Null);
    }
    if arg
        .trim_start()
        .to_ascii_uppercase()
        .starts_with("JSON_EXTRACT(")
    {
        return Ok(parse_json_document_value(value));
    }
    Ok(value)
}

pub(crate) fn parse_json_document_value(value: Value) -> Value {
    match value {
        Value::String(value) if value.starts_with(MYSQL_BINARY_SENTINEL) => {
            let hex = value.trim_start_matches(MYSQL_BINARY_SENTINEL);
            let bytes = hex
                .as_bytes()
                .chunks_exact(2)
                .map(|pair| {
                    (pair[0] as char).to_digit(16).unwrap_or_default() * 16
                        + (pair[1] as char).to_digit(16).unwrap_or_default()
                })
                .map(|value| value as u8)
                .collect::<Vec<_>>();
            serde_json::from_slice::<Value>(&bytes)
                .map(mark_json_nulls)
                .unwrap_or_else(|_| {
                    let text = String::from_utf8(bytes.clone())
                        .unwrap_or_else(|_| bytes.into_iter().map(char::from).collect());
                    serde_json::from_str::<Value>(&text)
                        .map(mark_json_nulls)
                        .unwrap_or(Value::String(text))
                })
        }
        Value::String(value) if value.starts_with(JSON_EXTRACT_TEXT_SENTINEL) => {
            let text = value
                .strip_prefix(JSON_EXTRACT_TEXT_SENTINEL)
                .unwrap_or_default();
            serde_json::from_str::<Value>(text)
                .map(mark_json_nulls)
                .unwrap_or(Value::String(text.to_string()))
        }
        Value::String(value) if value.starts_with(JSON_AGGREGATE_TEXT_SENTINEL) => {
            let text = value.trim_start_matches(JSON_AGGREGATE_TEXT_SENTINEL);
            serde_json::from_str::<Value>(text)
                .map(mark_json_nulls)
                .unwrap_or(Value::String(text.to_string()))
        }
        Value::String(value) => serde_json::from_str::<Value>(&value)
            .map(mark_json_nulls)
            .unwrap_or(Value::String(value)),
        other => other,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum JsonPathToken {
    Key(String),
    Index(usize),
    Wildcard,
    Recursive,
}

pub(crate) fn json_extract_path(document: &Value, path: &str) -> Option<Value> {
    let tokens = parse_json_path(path)?;
    if tokens
        .iter()
        .any(|token| matches!(token, JsonPathToken::Wildcard | JsonPathToken::Recursive))
    {
        let values = json_extract_matches(document, path);
        return (!values.is_empty()).then_some(Value::Array(values));
    }
    let mut current = document;
    for token in tokens {
        match token {
            JsonPathToken::Key(key) => current = current.as_object()?.get(&key)?,
            JsonPathToken::Index(index) => current = current.as_array()?.get(index)?,
            JsonPathToken::Wildcard | JsonPathToken::Recursive => return None,
        }
    }
    Some(current.clone())
}

pub(crate) fn json_extract_matches(document: &Value, path: &str) -> Vec<Value> {
    json_extract_matches_with_paths(document, path)
        .into_iter()
        .map(|(_, value)| value)
        .collect()
}

fn json_extract_matches_with_paths(document: &Value, path: &str) -> Vec<(String, Value)> {
    let Some(tokens) = parse_json_path(path) else {
        return Vec::new();
    };
    let mut matches = Vec::new();
    collect_json_path_matches(document, &tokens, 0, "$".to_string(), &mut matches);
    matches
}

fn collect_json_path_matches(
    current: &Value,
    tokens: &[JsonPathToken],
    token_index: usize,
    path: String,
    matches: &mut Vec<(String, Value)>,
) {
    if token_index == tokens.len() {
        matches.push((path, current.clone()));
        return;
    }
    match &tokens[token_index] {
        JsonPathToken::Key(key) => {
            if let Some(value) = current.as_object().and_then(|object| object.get(key)) {
                collect_json_path_matches(
                    value,
                    tokens,
                    token_index + 1,
                    format!("{path}.{key}"),
                    matches,
                );
            }
        }
        JsonPathToken::Index(index) => {
            if let Some(value) = current.as_array().and_then(|array| array.get(*index)) {
                collect_json_path_matches(
                    value,
                    tokens,
                    token_index + 1,
                    format!("{path}[{index}]"),
                    matches,
                );
            }
        }
        JsonPathToken::Wildcard => match current {
            Value::Array(values) => {
                for (index, value) in values.iter().enumerate() {
                    collect_json_path_matches(
                        value,
                        tokens,
                        token_index + 1,
                        format!("{path}[{index}]"),
                        matches,
                    );
                }
            }
            Value::Object(values) => {
                for (key, value) in values {
                    collect_json_path_matches(
                        value,
                        tokens,
                        token_index + 1,
                        format!("{path}.{key}"),
                        matches,
                    );
                }
            }
            _ => {}
        },
        JsonPathToken::Recursive => {
            collect_json_path_matches(current, tokens, token_index + 1, path.clone(), matches);
            match current {
                Value::Array(values) => {
                    for (index, value) in values.iter().enumerate() {
                        collect_json_path_matches(
                            value,
                            tokens,
                            token_index,
                            format!("{path}[{index}]"),
                            matches,
                        );
                    }
                }
                Value::Object(values) => {
                    for (key, value) in values {
                        collect_json_path_matches(
                            value,
                            tokens,
                            token_index,
                            format!("{path}.{key}"),
                            matches,
                        );
                    }
                }
                _ => {}
            }
        }
    }
}

fn json_set_path(document: &mut Value, path: &str, value: Value) -> bool {
    let Some(tokens) = parse_json_path(path) else {
        return false;
    };
    if tokens.is_empty() {
        *document = value;
        return true;
    }
    let mut current = document;
    for token in &tokens[..tokens.len() - 1] {
        match token {
            JsonPathToken::Key(key) => {
                if !current.is_object() {
                    return false;
                }
                current = current
                    .as_object_mut()
                    .expect("object already verified")
                    .entry(key.clone())
                    .or_insert_with(|| Value::Object(Map::new()));
            }
            JsonPathToken::Index(index) => {
                if !current.is_array() {
                    *current = Value::Array(Vec::new());
                }
                let array = current.as_array_mut().expect("array just inserted");
                while array.len() <= *index {
                    array.push(json_null_value());
                }
                current = &mut array[*index];
            }
            JsonPathToken::Wildcard | JsonPathToken::Recursive => return false,
        }
    }
    match tokens.last().expect("non-empty path") {
        JsonPathToken::Key(key) => {
            if !current.is_object() {
                return false;
            }
            current
                .as_object_mut()
                .expect("object just inserted")
                .insert(key.clone(), value);
            true
        }
        JsonPathToken::Index(index) => {
            if !current.is_array() {
                *current = Value::Array(Vec::new());
            }
            let array = current.as_array_mut().expect("array just inserted");
            while array.len() <= *index {
                array.push(Value::Null);
            }
            array[*index] = value;
            true
        }
        JsonPathToken::Wildcard | JsonPathToken::Recursive => false,
    }
}

fn json_remove_path(document: &mut Value, path: &str) -> bool {
    let Some(tokens) = parse_json_path(path) else {
        return false;
    };
    if tokens.is_empty() {
        *document = json_null_value();
        return true;
    }
    let mut current = document;
    for token in &tokens[..tokens.len() - 1] {
        match token {
            JsonPathToken::Key(key) => {
                let Some(next) = current
                    .as_object_mut()
                    .and_then(|object| object.get_mut(key))
                else {
                    return false;
                };
                current = next;
            }
            JsonPathToken::Index(index) => {
                let Some(next) = current
                    .as_array_mut()
                    .and_then(|array| array.get_mut(*index))
                else {
                    return false;
                };
                current = next;
            }
            JsonPathToken::Wildcard | JsonPathToken::Recursive => return false,
        }
    }
    match tokens.last().expect("non-empty path") {
        JsonPathToken::Key(key) => current
            .as_object_mut()
            .map(|object| object.remove(key).is_some())
            .unwrap_or(false),
        JsonPathToken::Index(index) => current
            .as_array_mut()
            .map(|array| {
                if *index < array.len() {
                    array.remove(*index);
                    true
                } else {
                    false
                }
            })
            .unwrap_or(false),
        JsonPathToken::Wildcard | JsonPathToken::Recursive => false,
    }
}

fn parse_json_path(path: &str) -> Option<Vec<JsonPathToken>> {
    let mut chars = path.trim().chars().peekable();
    if chars.next()? != '$' {
        return None;
    }
    let mut tokens = Vec::new();
    while let Some(ch) = chars.next() {
        match ch {
            '.' => {
                if chars.peek() == Some(&'*') {
                    chars.next();
                    if chars.peek() == Some(&'*') {
                        chars.next();
                        tokens.push(JsonPathToken::Recursive);
                    } else {
                        tokens.push(JsonPathToken::Wildcard);
                    }
                    continue;
                }
                if matches!(chars.peek(), Some('"') | Some('\'')) {
                    tokens.push(JsonPathToken::Key(parse_quoted_json_path_part(&mut chars)?));
                    continue;
                }
                let mut key = String::new();
                while let Some(&next) = chars.peek() {
                    if next == '.' || next == '[' {
                        break;
                    }
                    key.push(next);
                    chars.next();
                }
                if key.is_empty() {
                    return None;
                }
                tokens.push(JsonPathToken::Key(key));
            }
            '[' => {
                if chars.peek() == Some(&'*') {
                    chars.next();
                    if chars.peek() == Some(&'*') {
                        chars.next();
                        tokens.push(JsonPathToken::Recursive);
                    } else {
                        tokens.push(JsonPathToken::Wildcard);
                    }
                    if chars.next()? != ']' {
                        return None;
                    }
                    continue;
                }
                if matches!(chars.peek(), Some('"') | Some('\'')) {
                    let key = parse_quoted_json_path_part(&mut chars)?;
                    if chars.next()? != ']' {
                        return None;
                    }
                    tokens.push(JsonPathToken::Key(key));
                } else {
                    let mut index = String::new();
                    while let Some(&next) = chars.peek() {
                        if next == ']' {
                            break;
                        }
                        index.push(next);
                        chars.next();
                    }
                    if chars.next()? != ']' {
                        return None;
                    }
                    tokens.push(JsonPathToken::Index(index.trim().parse().ok()?));
                }
            }
            _ => return None,
        }
    }
    Some(tokens)
}

fn parse_quoted_json_path_part<I>(chars: &mut std::iter::Peekable<I>) -> Option<String>
where
    I: Iterator<Item = char>,
{
    let quote = chars.next()?;
    let mut out = String::new();
    while let Some(ch) = chars.next() {
        if ch == quote {
            return Some(out);
        }
        if ch == '\\' {
            out.push(chars.next().unwrap_or('\\'));
        } else {
            out.push(ch);
        }
    }
    None
}

fn json_contains_value(target: &Value, candidate: &Value) -> bool {
    match (target, candidate) {
        (Value::Object(target), Value::Object(candidate)) => {
            candidate.iter().all(|(key, value)| {
                target
                    .get(key)
                    .map(|target_value| json_contains_value(target_value, value))
                    .unwrap_or(false)
            })
        }
        (Value::Array(target), Value::Array(candidate)) => candidate.iter().all(|candidate| {
            target
                .iter()
                .any(|target| json_contains_value(target, candidate))
        }),
        (Value::Array(target), candidate) => target
            .iter()
            .any(|target| json_contains_value(target, candidate)),
        _ => target == candidate,
    }
}
