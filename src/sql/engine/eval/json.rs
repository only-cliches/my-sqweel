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
        1 => Ok(matches.pop().unwrap_or(Value::Null)),
        _ => Ok(Value::Array(matches)),
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
    if value == Value::Null {
        return Ok(Value::Null);
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
        object.insert(json_scalar_to_string(&key), value);
    }
    Ok(Value::Object(object))
}

pub(super) fn eval_json_array(
    args: &[String],
    data: &Map<String, Value>,
    last_insert_id: u64,
) -> Result<Value> {
    args.iter()
        .map(|arg| eval_scalar_text(arg, data, last_insert_id))
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

#[derive(Debug, Clone, Copy)]
pub(super) enum JsonMutation {
    Set,
    Remove,
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
        JsonMutation::Set => {
            for pair in args.iter().skip(1).collect::<Vec<_>>().chunks(2) {
                let Some(path_arg) = pair.first() else {
                    break;
                };
                let Some(value_arg) = pair.get(1) else {
                    break;
                };
                let path = eval_scalar_text(path_arg, data, last_insert_id)?;
                let value = eval_scalar_text(value_arg, data, last_insert_id)?;
                json_set_path(&mut document, &json_scalar_to_string(&path), value);
            }
        }
        JsonMutation::Remove => {
            for path_arg in args.iter().skip(1) {
                let path = eval_scalar_text(path_arg, data, last_insert_id)?;
                json_remove_path(&mut document, &json_scalar_to_string(&path));
            }
        }
    }
    Ok(document)
}

fn eval_json_document(arg: &str, data: &Map<String, Value>, last_insert_id: u64) -> Result<Value> {
    let value = eval_scalar_text(arg, data, last_insert_id)?;
    Ok(parse_json_document_value(value))
}

pub(super) fn parse_json_document_value(value: Value) -> Value {
    match value {
        Value::String(value) => serde_json::from_str(&value).unwrap_or(Value::String(value)),
        other => other,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum JsonPathToken {
    Key(String),
    Index(usize),
}

fn json_extract_path(document: &Value, path: &str) -> Option<Value> {
    let tokens = parse_json_path(path)?;
    let mut current = document;
    for token in tokens {
        match token {
            JsonPathToken::Key(key) => current = current.as_object()?.get(&key)?,
            JsonPathToken::Index(index) => current = current.as_array()?.get(index)?,
        }
    }
    Some(current.clone())
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
                    *current = Value::Object(Map::new());
                }
                current = current
                    .as_object_mut()
                    .expect("object just inserted")
                    .entry(key.clone())
                    .or_insert_with(|| Value::Object(Map::new()));
            }
            JsonPathToken::Index(index) => {
                if !current.is_array() {
                    *current = Value::Array(Vec::new());
                }
                let array = current.as_array_mut().expect("array just inserted");
                while array.len() <= *index {
                    array.push(Value::Null);
                }
                current = &mut array[*index];
            }
        }
    }
    match tokens.last().expect("non-empty path") {
        JsonPathToken::Key(key) => {
            if !current.is_object() {
                *current = Value::Object(Map::new());
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
    }
}

fn json_remove_path(document: &mut Value, path: &str) -> bool {
    let Some(tokens) = parse_json_path(path) else {
        return false;
    };
    if tokens.is_empty() {
        *document = Value::Null;
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
