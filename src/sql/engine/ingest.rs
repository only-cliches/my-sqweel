//! CSV and JSON imports through the same SQL path as embedded queries.

use std::collections::BTreeSet;
use std::io::Read;

use anyhow::{Context, Result, anyhow, ensure};
use chrono::{NaiveDate, NaiveDateTime};
use serde_json::{Map, Number, Value};

use super::{Engine, EngineSession};

/// How an import handles a row whose primary or unique key already exists.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ImportMode {
    #[default]
    Insert,
    /// Update only columns supplied by the imported row.
    UpsertMerge,
    /// Replace the existing row, applying defaults to omitted columns.
    UpsertReplace,
}

/// Whether the import may create its destination table.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ImportTable {
    #[default]
    CreateIfMissing,
    Existing,
}

/// A column definition used when the import creates a table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportColumn {
    pub name: String,
    pub sql_type: String,
    pub nullable: bool,
    pub primary_key: bool,
}

impl ImportColumn {
    pub fn new(name: impl Into<String>, sql_type: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            sql_type: sql_type.into(),
            nullable: true,
            primary_key: false,
        }
    }

    pub fn primary_key(mut self) -> Self {
        self.primary_key = true;
        self.nullable = false;
        self
    }
}

/// Return `Skip` from a visitor to leave a source row out of the import.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImportDecision {
    Include,
    Skip,
}

pub type ImportVisitor<'a> =
    dyn FnMut(usize, &mut Map<String, Value>) -> Result<ImportDecision> + 'a;

/// Options shared by CSV and JSON imports. A visitor may edit each row before
/// type inference and writing. Row numbers start at one.
pub struct ImportOptions<'a> {
    pub database: Option<String>,
    pub table: ImportTable,
    pub mode: ImportMode,
    /// Explicit definitions for a table created by this import. When absent,
    /// column types are inferred from all included rows.
    pub columns: Option<Vec<ImportColumn>>,
    pub visitor: Option<&'a mut ImportVisitor<'a>>,
}

impl Default for ImportOptions<'_> {
    fn default() -> Self {
        Self {
            database: None,
            table: ImportTable::CreateIfMissing,
            mode: ImportMode::Insert,
            columns: None,
            visitor: None,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ImportReport {
    pub rows_read: usize,
    pub rows_imported: usize,
    pub rows_skipped: usize,
    pub table_created: bool,
}

struct PreparedRows {
    rows: Vec<Map<String, Value>>,
    read: usize,
    skipped: usize,
}

impl Engine {
    /// Import a CSV file with a header row. Empty fields become SQL NULL.
    pub fn import_csv(
        &self,
        table: &str,
        input: impl Read,
        mut options: ImportOptions<'_>,
    ) -> Result<ImportReport> {
        let mut reader = csv::ReaderBuilder::new().from_reader(input);
        let headers = reader
            .headers()?
            .iter()
            .map(str::to_owned)
            .collect::<Vec<_>>();
        validate_columns(&headers)?;
        let mut prepared = PreparedRows {
            rows: Vec::new(),
            read: 0,
            skipped: 0,
        };
        for record in reader.records() {
            let record = record?;
            let mut row = Map::new();
            for (name, value) in headers.iter().zip(record.iter()) {
                row.insert(name.clone(), csv_value(value));
            }
            prepared.push(row, &mut options)?;
        }
        self.import_prepared(table, prepared, options)
    }

    /// Import a JSON array, one JSON object, or newline-delimited JSON objects.
    pub fn import_json(
        &self,
        table: &str,
        input: impl Read,
        mut options: ImportOptions<'_>,
    ) -> Result<ImportReport> {
        let mut prepared = PreparedRows {
            rows: Vec::new(),
            read: 0,
            skipped: 0,
        };
        for value in serde_json::Deserializer::from_reader(input).into_iter::<Value>() {
            match value? {
                Value::Object(row) => prepared.push(row, &mut options)?,
                Value::Array(rows) => {
                    for row in rows {
                        let Value::Object(row) = row else {
                            return Err(anyhow!("JSON import rows must be objects"));
                        };
                        prepared.push(row, &mut options)?;
                    }
                }
                _ => {
                    return Err(anyhow!(
                        "JSON import must contain objects or an array of objects"
                    ));
                }
            }
        }
        self.import_prepared(table, prepared, options)
    }

    fn import_prepared(
        &self,
        table: &str,
        prepared: PreparedRows,
        options: ImportOptions<'_>,
    ) -> Result<ImportReport> {
        let table_name = quote_identifier(table)?;
        let mut session = self.session();
        if let Some(database) = &options.database {
            session.use_database(database)?;
        }
        let exists = table_exists(&mut session, table)?;
        ensure!(
            options.table != ImportTable::Existing || exists,
            "import table does not exist: {table}"
        );
        let created = !exists;
        if created {
            let columns = match options.columns {
                Some(columns) => columns,
                None => infer_columns(&prepared.rows)?,
            };
            let definition = create_table_definition(&columns)?;
            session.execute_sql(&format!("CREATE TABLE {table_name} ({definition})"))?;
        }
        session.execute_sql("BEGIN")?;
        let result = (|| {
            write_batches(&mut session, &table_name, &prepared.rows, options.mode)?;
            session.execute_sql("COMMIT")?;
            Ok(ImportReport {
                rows_read: prepared.read,
                rows_imported: prepared.rows.len(),
                rows_skipped: prepared.skipped,
                table_created: created,
            })
        })();
        if result.is_err() {
            let _ = session.execute_sql("ROLLBACK");
        }
        result
    }
}

impl PreparedRows {
    fn push(&mut self, mut row: Map<String, Value>, options: &mut ImportOptions<'_>) -> Result<()> {
        self.read += 1;
        if let Some(visitor) = options.visitor.as_mut()
            && visitor(self.read, &mut row)? == ImportDecision::Skip
        {
            self.skipped += 1;
            return Ok(());
        }
        self.rows.push(row);
        Ok(())
    }
}

fn csv_value(text: &str) -> Value {
    let value = text.trim();
    if value.is_empty() {
        return Value::Null;
    }
    if value.eq_ignore_ascii_case("true") {
        return Value::Bool(true);
    }
    if value.eq_ignore_ascii_case("false") {
        return Value::Bool(false);
    }
    let leading_zero = (value.starts_with('0') && value.len() > 1 && !value.starts_with("0."))
        || (value.starts_with("-0") && value.len() > 2 && !value.starts_with("-0."));
    if !leading_zero {
        if let Ok(number) = value.parse::<i64>() {
            return Value::Number(number.into());
        }
        if value.contains(['.', 'e', 'E'])
            && let Ok(number) = value.parse::<f64>()
            && let Some(number) = Number::from_f64(number)
        {
            return Value::Number(number);
        }
    }
    if (value.starts_with('{') || value.starts_with('['))
        && let Ok(nested @ (Value::Object(_) | Value::Array(_))) = serde_json::from_str(value)
    {
        return nested;
    }
    Value::String(text.to_owned())
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Guess {
    Unknown,
    Boolean,
    Integer,
    Float,
    Date,
    DateTime,
    Json,
    Text,
}

fn guess(value: &Value) -> Guess {
    match value {
        Value::Null => Guess::Unknown,
        Value::Bool(_) => Guess::Boolean,
        Value::Number(value) if value.is_i64() || value.is_u64() => Guess::Integer,
        Value::Number(_) => Guess::Float,
        Value::Array(_) | Value::Object(_) => Guess::Json,
        Value::String(value) if NaiveDate::parse_from_str(value, "%Y-%m-%d").is_ok() => Guess::Date,
        Value::String(value)
            if NaiveDateTime::parse_from_str(value, "%Y-%m-%d %H:%M:%S").is_ok()
                || NaiveDateTime::parse_from_str(value, "%Y-%m-%dT%H:%M:%S").is_ok() =>
        {
            Guess::DateTime
        }
        Value::String(_) => Guess::Text,
    }
}

fn merge_guess(left: Guess, right: Guess) -> Guess {
    match (left, right) {
        (Guess::Unknown, kind) | (kind, Guess::Unknown) => kind,
        (Guess::Integer, Guess::Float) | (Guess::Float, Guess::Integer) => Guess::Float,
        (left, right) if left == right => left,
        _ => Guess::Text,
    }
}

fn infer_columns(rows: &[Map<String, Value>]) -> Result<Vec<ImportColumn>> {
    ensure!(
        !rows.is_empty(),
        "cannot infer columns from an empty import"
    );
    let mut names = Vec::new();
    let mut kinds = Vec::new();
    for row in rows {
        for (name, value) in row {
            match names
                .iter()
                .position(|known: &String| known.eq_ignore_ascii_case(name))
            {
                Some(index) if names[index] != *name => {
                    return Err(anyhow!("import columns differ only by case: {name}"));
                }
                Some(index) => kinds[index] = merge_guess(kinds[index], guess(value)),
                None => {
                    names.push(name.clone());
                    kinds.push(guess(value));
                }
            }
        }
    }
    validate_columns(&names)?;
    Ok(names
        .into_iter()
        .zip(kinds)
        .map(|(name, kind)| {
            let primary_key = name.eq_ignore_ascii_case("id")
                && rows
                    .iter()
                    .all(|row| row.get(&name).is_some_and(|v| !v.is_null()))
                && matches!(kind, Guess::Integer | Guess::Text)
                && (kind != Guess::Text
                    || rows.iter().all(|row| {
                        row.get(&name)
                            .and_then(Value::as_str)
                            .is_some_and(|value| value.chars().count() <= 255)
                    }));
            let sql_type = match (kind, primary_key) {
                (Guess::Boolean, _) => "BOOLEAN",
                (Guess::Integer, _) => "BIGINT",
                (Guess::Float, _) => "DOUBLE",
                (Guess::Date, _) => "DATE",
                (Guess::DateTime, _) => "DATETIME",
                (Guess::Json, _) => "JSON",
                (Guess::Text, true) => "VARCHAR(255)",
                _ => "TEXT",
            };
            ImportColumn {
                name,
                sql_type: sql_type.into(),
                nullable: !primary_key,
                primary_key,
            }
        })
        .collect())
}

fn validate_columns(names: &[String]) -> Result<()> {
    ensure!(!names.is_empty(), "import has no columns");
    let mut seen = BTreeSet::new();
    for name in names {
        quote_identifier(name)?;
        ensure!(
            seen.insert(name.to_ascii_lowercase()),
            "duplicate import column: {name}"
        );
    }
    Ok(())
}

fn quote_identifier(name: &str) -> Result<String> {
    ensure!(
        !name.is_empty() && !name.contains(['\0', '.']),
        "invalid import identifier: {name:?}"
    );
    Ok(format!("`{}`", name.replace('`', "``")))
}

fn create_table_definition(columns: &[ImportColumn]) -> Result<String> {
    validate_columns(
        &columns
            .iter()
            .map(|column| column.name.clone())
            .collect::<Vec<_>>(),
    )?;
    let mut definitions = Vec::new();
    let mut primary = Vec::new();
    for column in columns {
        ensure!(
            !column.sql_type.trim().is_empty() && !column.sql_type.contains(';'),
            "invalid SQL type for import column {}",
            column.name
        );
        let name = quote_identifier(&column.name)?;
        definitions.push(format!(
            "{name} {}{}",
            column.sql_type,
            if column.primary_key || !column.nullable {
                " NOT NULL"
            } else {
                ""
            }
        ));
        if column.primary_key {
            primary.push(name);
        }
    }
    if !primary.is_empty() {
        definitions.push(format!("PRIMARY KEY ({})", primary.join(", ")));
    }
    Ok(definitions.join(", "))
}

fn table_exists(session: &mut EngineSession, table: &str) -> Result<bool> {
    Ok(session
        .execute_sql("SHOW TABLES")?
        .into_iter()
        .flat_map(|result| result.rows)
        .flat_map(|row| row.into_values())
        .any(|value| {
            value
                .as_str()
                .is_some_and(|name| name.eq_ignore_ascii_case(table))
        }))
}

fn write_batches(
    session: &mut EngineSession,
    table: &str,
    rows: &[Map<String, Value>],
    mode: ImportMode,
) -> Result<()> {
    let mut offset = 0;
    while offset < rows.len() {
        let mut columns = rows[offset].keys().cloned().collect::<Vec<_>>();
        columns.sort();
        validate_columns(&columns)?;
        let mut end = offset + 1;
        while end < rows.len() && end - offset < 128 {
            let mut next = rows[end].keys().cloned().collect::<Vec<_>>();
            next.sort();
            if next != columns {
                break;
            }
            end += 1;
        }
        let names = columns
            .iter()
            .map(|name| quote_identifier(name))
            .collect::<Result<Vec<_>>>()?;
        let mut params = Vec::with_capacity((end - offset) * columns.len());
        for row in &rows[offset..end] {
            params.extend(columns.iter().map(|name| row[name].clone()));
        }
        let values = (offset..end)
            .map(|_| format!("({})", vec!["?"; columns.len()].join(", ")))
            .collect::<Vec<_>>()
            .join(", ");
        let verb = if mode == ImportMode::UpsertReplace {
            "REPLACE"
        } else {
            "INSERT"
        };
        let mut sql = format!("{verb} INTO {table} ({}) VALUES {values}", names.join(", "));
        if mode == ImportMode::UpsertMerge {
            sql.push_str(" ON DUPLICATE KEY UPDATE ");
            sql.push_str(
                &names
                    .iter()
                    .map(|name| format!("{name} = VALUES({name})"))
                    .collect::<Vec<_>>()
                    .join(", "),
            );
        }
        session
            .execute_sql_with_params(&sql, &params)
            .with_context(|| format!("import rows {} through {}", offset + 1, end))?;
        offset = end;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{EngineConfig, Storage};
    use serde_json::json;

    #[test]
    fn csv_infers_columns_and_visits_every_row() {
        assert_eq!(csv_value("0012"), json!("0012"));
        let engine = Engine::default();
        let mut visitor = |number: usize, row: &mut Map<String, Value>| {
            if number == 2 {
                return Ok(ImportDecision::Skip);
            }
            row.insert("name".into(), json!("edited"));
            Ok(ImportDecision::Include)
        };
        let report = engine
            .import_csv(
                "people",
                "id,name,score,active,born\n1,\"Ada, A\",10,true,2026-01-01\n2,Bob,20,false,2026-01-02\n".as_bytes(),
                ImportOptions {
                    visitor: Some(&mut visitor),
                    ..ImportOptions::default()
                },
            )
            .unwrap();
        assert_eq!(report.rows_read, 2);
        assert_eq!(report.rows_imported, 1);
        assert_eq!(report.rows_skipped, 1);
        assert!(report.table_created);
        let rows = engine
            .execute_sql("SELECT id, name, score FROM people")
            .unwrap();
        assert_eq!(
            rows[0].rows,
            vec![
                json!({"id": 1, "name": "edited", "score": 10})
                    .as_object()
                    .unwrap()
                    .clone()
            ]
        );
        let create = engine.execute_sql("SHOW CREATE TABLE people").unwrap();
        assert!(
            create[0].rows[0]
                .values()
                .filter_map(Value::as_str)
                .any(|value| value.to_ascii_uppercase().contains("BIGINT"))
        );
    }

    #[test]
    fn json_import_merges_or_replaces_existing_rows() {
        let engine = Engine::default();
        engine
            .execute_sql("CREATE TABLE people (id BIGINT PRIMARY KEY, name TEXT, city TEXT DEFAULT 'unknown'); INSERT INTO people VALUES (1, 'old', 'Paris')")
            .unwrap();
        engine
            .import_json(
                "people",
                r#"[{"id":1,"name":"merged"},{"id":2,"name":"new"}]"#.as_bytes(),
                ImportOptions {
                    table: ImportTable::Existing,
                    mode: ImportMode::UpsertMerge,
                    ..ImportOptions::default()
                },
            )
            .unwrap();
        let merged = engine
            .execute_sql("SELECT name, city FROM people WHERE id = 1")
            .unwrap();
        assert_eq!(merged[0].rows[0]["city"], json!("Paris"));
        engine
            .import_json(
                "people",
                r#"{"id":1,"name":"replaced"}"#.as_bytes(),
                ImportOptions {
                    table: ImportTable::Existing,
                    mode: ImportMode::UpsertReplace,
                    ..ImportOptions::default()
                },
            )
            .unwrap();
        let replaced = engine
            .execute_sql("SELECT name, city FROM people WHERE id = 1")
            .unwrap();
        assert_eq!(replaced[0].rows[0]["name"], json!("replaced"));
        assert_eq!(replaced[0].rows[0]["city"], json!("unknown"));
    }

    #[test]
    fn explicit_columns_and_batches_survive_rocksdb_reopen() {
        let directory = tempfile::tempdir().unwrap();
        let rows = (0..130)
            .map(|id| json!({"id": id, "note": format!("row {id}")}))
            .collect::<Vec<_>>();
        {
            let engine = Engine::open(
                EngineConfig::default(),
                Storage::RocksDb(directory.path().join("db")),
            )
            .unwrap();
            let id = ImportColumn::new("id", "BIGINT").primary_key();
            let report = engine
                .import_json(
                    "items",
                    serde_json::to_vec(&rows).unwrap().as_slice(),
                    ImportOptions {
                        columns: Some(vec![id, ImportColumn::new("note", "TEXT")]),
                        ..ImportOptions::default()
                    },
                )
                .unwrap();
            assert_eq!(report.rows_imported, 130);
        }
        let reopened = Engine::open(
            EngineConfig::default(),
            Storage::RocksDb(directory.path().join("db")),
        )
        .unwrap();
        let result = reopened
            .execute_sql("SELECT COUNT(*) AS n FROM items")
            .unwrap();
        assert_eq!(result[0].rows[0]["n"], json!(130));
    }

    #[test]
    fn failed_ndjson_batch_rolls_back_all_rows() {
        let engine = Engine::default();
        let mut lines = (0..129)
            .map(|id| json!({"id": id}).to_string())
            .collect::<Vec<_>>();
        lines.push(json!({"id": 0}).to_string());
        let error = engine
            .import_json(
                "items",
                lines.join("\n").as_bytes(),
                ImportOptions::default(),
            )
            .unwrap_err();
        assert!(error.to_string().contains("import rows"));
        let result = engine
            .execute_sql("SELECT COUNT(*) AS n FROM items")
            .unwrap();
        assert_eq!(result[0].rows[0]["n"], json!(0));
    }

    #[test]
    fn import_targets_selected_database() {
        let engine = Engine::default();
        engine.execute_sql("CREATE DATABASE analytics").unwrap();
        engine
            .import_json(
                "metrics",
                r#"{"id":1,"value":7}"#.as_bytes(),
                ImportOptions {
                    database: Some("analytics".into()),
                    ..ImportOptions::default()
                },
            )
            .unwrap();
        let mut session = engine.session();
        session.use_database("analytics").unwrap();
        let result = session
            .execute_sql("SELECT value FROM metrics WHERE id = 1")
            .unwrap();
        assert_eq!(result[0].rows[0]["value"], json!(7));
        assert!(engine.execute_sql("SELECT * FROM metrics").is_err());
    }
}
