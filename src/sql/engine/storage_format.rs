use super::*;

pub(super) fn storage_tables_key() -> &'static str {
    "my-sqweel:tables"
}

pub(super) fn storage_schema_key(table: &str) -> String {
    format!("{STORAGE_NAMESPACE}:schema:{}", storage_key_part(table))
}

pub(super) fn storage_schema_columns_key(table: &str) -> String {
    format!(
        "{STORAGE_NAMESPACE}:schema_columns:{}",
        storage_key_part(table)
    )
}

pub(super) fn storage_schema_column_pattern(table: &str) -> String {
    format!(
        "{STORAGE_NAMESPACE}:schema_column:{}:*",
        storage_key_part(table)
    )
}

pub(super) fn storage_schema_column_key(table: &str, column: &str) -> String {
    format!(
        "{STORAGE_NAMESPACE}:schema_column:{}:{}",
        storage_key_part(table),
        storage_key_part(column)
    )
}

pub(super) fn storage_schema_pk_key(table: &str) -> String {
    format!("{STORAGE_NAMESPACE}:schema_pk:{}", storage_key_part(table))
}

pub(super) fn storage_schema_uniques_key(table: &str) -> String {
    format!(
        "{STORAGE_NAMESPACE}:schema_unique:{}",
        storage_key_part(table)
    )
}

pub(super) fn storage_schema_indexes_key(table: &str) -> String {
    format!(
        "{STORAGE_NAMESPACE}:schema_index:{}",
        storage_key_part(table)
    )
}

pub(super) fn storage_schema_foreign_keys_key(table: &str) -> String {
    format!("{STORAGE_NAMESPACE}:schema_fk:{}", storage_key_part(table))
}

pub(super) fn storage_table_pks_key(table: &str) -> String {
    format!("{STORAGE_NAMESPACE}:table_pks:{}", storage_key_part(table))
}

pub(super) fn storage_row_pattern(table: &str) -> String {
    format!("{STORAGE_NAMESPACE}:row:{}:*", storage_key_part(table))
}

pub(super) fn storage_row_key(table: &str, pk: &str) -> String {
    format!(
        "{STORAGE_NAMESPACE}:row:{}:{}",
        storage_key_part(table),
        storage_key_part(pk)
    )
}

pub(super) fn storage_key_part(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.') {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

pub(super) fn persist_column_hint(
    store: &dyn RedisStore,
    key: &str,
    hint: &ColumnHint,
) -> Result<()> {
    store.hset(key, "sql_type", hint.sql_type.as_deref().unwrap_or(""))?;
    store.hset(
        key,
        "nullable",
        hint.nullable.map(bool_string).unwrap_or(""),
    )?;
    store.hset(key, "default", hint.default.as_deref().unwrap_or(""))?;
    store.hset(key, "primary_key", bool_string(hint.primary_key))?;
    store.hset(key, "auto_increment", bool_string(hint.auto_increment))?;
    Ok(())
}

pub(super) fn decode_column_hint_from_hash(fields: &BTreeMap<String, String>) -> ColumnHint {
    ColumnHint {
        sql_type: non_empty(fields.get("sql_type")),
        nullable: fields
            .get("nullable")
            .and_then(|value| match value.as_str() {
                "true" => Some(true),
                "false" => Some(false),
                _ => None,
            }),
        default: non_empty(fields.get("default")),
        primary_key: fields
            .get("primary_key")
            .is_some_and(|value| value == "true"),
        auto_increment: fields
            .get("auto_increment")
            .is_some_and(|value| value == "true"),
    }
}

pub(super) fn non_empty(value: Option<&String>) -> Option<String> {
    value.cloned().filter(|value| !value.is_empty())
}

pub(super) fn bool_string(value: bool) -> &'static str {
    if value { "true" } else { "false" }
}

pub(super) fn encode_column_order(schema: &TableSchemaHint) -> String {
    serde_json::to_string(&ordered_schema_columns(schema)).unwrap_or_else(|_| "[]".to_string())
}

pub(super) fn decode_column_order(value: Option<&String>) -> Vec<String> {
    value
        .and_then(|value| serde_json::from_str::<Vec<String>>(value).ok())
        .unwrap_or_default()
}

pub(super) fn persist_stored_row(store: &dyn RedisStore, key: &str, row: &StoredRow) -> Result<()> {
    store.hset(key, "_id", &encode_json_value(&row.id))?;
    store.hset(key, "_table", &row.table)?;
    store.hset(key, "_createdAt", &row.created_at.to_rfc3339())?;
    store.hset(key, "_updatedAt", &row.updated_at.to_rfc3339())?;
    store.hset(key, "_version", &row.version.to_string())?;
    for (column, value) in &row.data {
        store.hset(key, &format!("data:{column}"), &encode_json_value(value))?;
    }
    Ok(())
}

pub(super) fn decode_stored_row(fields: &BTreeMap<String, String>) -> Result<StoredRow> {
    let id = fields
        .get("_id")
        .map(|value| decode_json_value(value))
        .unwrap_or(Value::Null);
    let table = fields.get("_table").cloned().unwrap_or_default();
    let created_at = fields
        .get("_createdAt")
        .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
        .map(|value| value.with_timezone(&Utc))
        .unwrap_or_else(Utc::now);
    let updated_at = fields
        .get("_updatedAt")
        .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
        .map(|value| value.with_timezone(&Utc))
        .unwrap_or(created_at);
    let version = fields
        .get("_version")
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(1);
    let mut data = Map::new();
    for (field, value) in fields {
        if let Some(column) = field.strip_prefix("data:") {
            data.insert(column.to_string(), decode_json_value(value));
        }
    }

    Ok(StoredRow {
        id,
        table,
        created_at,
        updated_at,
        version,
        data,
    })
}

pub(super) fn encode_json_value(value: &Value) -> String {
    match value {
        Value::Null => "n:".to_string(),
        Value::Bool(value) => format!("b:{}", if *value { "1" } else { "0" }),
        Value::Number(value) => {
            if let Some(value) = value.as_i64() {
                format!("i:{value}")
            } else if let Some(value) = value.as_u64() {
                format!("u:{value}")
            } else {
                format!("f:{value}")
            }
        }
        Value::String(value) => format!("s:{value}"),
        Value::Array(_) | Value::Object(_) => format!("j:{value}"),
    }
}

pub(super) fn decode_json_value(value: &str) -> Value {
    let Some((kind, raw)) = value.split_once(':') else {
        return Value::String(value.to_string());
    };
    match kind {
        "n" => Value::Null,
        "b" => Value::Bool(raw == "1" || raw.eq_ignore_ascii_case("true")),
        "i" => raw
            .parse::<i64>()
            .map(|value| Value::Number(Number::from(value)))
            .unwrap_or(Value::Null),
        "u" => raw
            .parse::<u64>()
            .map(|value| Value::Number(Number::from(value)))
            .unwrap_or(Value::Null),
        "f" => raw
            .parse::<f64>()
            .ok()
            .and_then(Number::from_f64)
            .map(Value::Number)
            .unwrap_or(Value::Null),
        "s" => Value::String(raw.to_string()),
        "j" => serde_json::from_str(raw).unwrap_or_else(|_| Value::String(raw.to_string())),
        _ => Value::String(value.to_string()),
    }
}

pub(super) fn encode_unique_columns(columns: &[String]) -> String {
    columns
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join(&UNIQUE_SEPARATOR.to_string())
}

pub(super) fn decode_unique_columns(value: &str) -> Option<Vec<String>> {
    let columns = value
        .split(UNIQUE_SEPARATOR)
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    (!columns.is_empty()).then_some(columns)
}

pub(super) fn encode_index_hint(index: &IndexHint) -> String {
    [
        index.name.clone(),
        bool_string(index.unique).to_string(),
        index.columns.join(&UNIQUE_SEPARATOR.to_string()),
    ]
    .join(&UNIQUE_SEPARATOR.to_string())
}

pub(super) fn decode_index_hint(value: &str) -> Option<IndexHint> {
    let mut parts = value.split(UNIQUE_SEPARATOR);
    let name = parts.next()?.to_string();
    let unique = parts.next()? == "true";
    let columns = parts.map(ToString::to_string).collect::<Vec<_>>();
    (!name.is_empty() && !columns.is_empty()).then_some(IndexHint {
        name,
        columns,
        unique,
    })
}

pub(super) fn encode_foreign_key_hint(foreign_key: &ForeignKeyHint) -> String {
    [
        foreign_key.name.clone(),
        foreign_key.columns.join(&FK_FIELD_SEPARATOR.to_string()),
        foreign_key.referenced_table.clone(),
        foreign_key
            .referenced_columns
            .join(&FK_FIELD_SEPARATOR.to_string()),
        foreign_key.on_delete.clone().unwrap_or_default(),
        foreign_key.on_update.clone().unwrap_or_default(),
    ]
    .join(&UNIQUE_SEPARATOR.to_string())
}

pub(super) fn decode_foreign_key_hint(value: &str) -> Option<ForeignKeyHint> {
    let parts = value.split(UNIQUE_SEPARATOR).collect::<Vec<_>>();
    if parts.len() < 4 {
        return None;
    }
    let name = parts[0].to_string();
    let columns = split_encoded_list(parts[1], FK_FIELD_SEPARATOR);
    let referenced_table = parts[2].to_string();
    let referenced_columns = split_encoded_list(parts[3], FK_FIELD_SEPARATOR);
    (!name.is_empty()
        && !columns.is_empty()
        && !referenced_table.is_empty()
        && !referenced_columns.is_empty())
    .then_some(ForeignKeyHint {
        name,
        columns,
        referenced_table,
        referenced_columns,
        on_delete: parts.get(4).and_then(non_empty_str),
        on_update: parts.get(5).and_then(non_empty_str),
    })
}

pub(super) fn split_encoded_list(value: &str, separator: char) -> Vec<String> {
    value
        .split(separator)
        .map(ToString::to_string)
        .filter(|value| !value.is_empty())
        .collect()
}

pub(super) fn non_empty_str(value: &&str) -> Option<String> {
    (!value.is_empty()).then(|| (*value).to_string())
}
