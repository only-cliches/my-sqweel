use std::net::SocketAddr;

use anyhow::{Result, anyhow};
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{Map, Value, json};

use crate::sql::engine::{SeedMode, SharedEngine, Snapshot};

pub fn spawn(addr: SocketAddr, engine: SharedEngine) {
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build();
        let Ok(rt) = rt else {
            tracing::warn!("failed to build debug http runtime");
            return;
        };
        rt.block_on(async move {
            let app = Router::new()
                .route("/_drift/health", get(health))
                .route("/_drift/report", get(report))
                .route("/_drift/tables", get(tables))
                .route("/_drift/tables/{table}/rows", get(table_rows))
                .route("/_drift/tables/{table}/seed", post(seed_table))
                .route("/_drift/snapshot", post(snapshot))
                .route("/_drift/restore", post(restore))
                .with_state(engine);

            let listener = match tokio::net::TcpListener::bind(addr).await {
                Ok(l) => l,
                Err(err) => {
                    tracing::warn!(error = %err, "debug http bind failed");
                    return;
                }
            };

            if let Err(err) = axum::serve(listener, app).await {
                tracing::warn!(error = %err, "debug http serve failed");
            }
        });
    });
}

async fn health() -> Json<Value> {
    Json(json!({ "ok": true }))
}

async fn tables(State(engine): State<SharedEngine>) -> Json<Value> {
    let snap = engine.snapshot();
    let tables: Vec<String> = snap.schemas.keys().cloned().collect();
    Json(json!({ "tables": tables }))
}

async fn report(State(engine): State<SharedEngine>) -> Json<Value> {
    Json(engine.drift_report())
}

async fn table_rows(Path(table): Path<String>, State(engine): State<SharedEngine>) -> Json<Value> {
    let snap = engine.snapshot();
    let rows = snap.rows.get(&table).cloned().unwrap_or_default();
    Json(json!({ "table": table, "rows": rows }))
}

async fn seed_table(
    Path(table): Path<String>,
    State(engine): State<SharedEngine>,
    Json(payload): Json<Value>,
) -> (StatusCode, Json<Value>) {
    let result = parse_seed_payload(payload)
        .and_then(|request| engine.seed_json_rows(&table, request.rows, request.mode));

    match result {
        Ok(report) => (
            StatusCode::OK,
            Json(json!({
                "table": report.table,
                "mode": report.mode.as_str(),
                "rowsSeeded": report.rows_seeded,
                "rowsAffected": report.rows_affected,
                "lastInsertId": report.last_insert_id,
            })),
        ),
        Err(err) => (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": err.to_string(),
            })),
        ),
    }
}

async fn snapshot(State(engine): State<SharedEngine>) -> Json<Snapshot> {
    Json(engine.snapshot())
}

async fn restore(
    State(engine): State<SharedEngine>,
    Json(snapshot): Json<Snapshot>,
) -> Json<Value> {
    engine.restore_snapshot(snapshot);
    Json(json!({ "restored": true }))
}

struct SeedRequest {
    mode: SeedMode,
    rows: Vec<Map<String, Value>>,
}

fn parse_seed_payload(payload: Value) -> Result<SeedRequest> {
    match payload {
        Value::Array(rows) => Ok(SeedRequest {
            mode: SeedMode::Append,
            rows: parse_seed_rows(Value::Array(rows))?,
        }),
        Value::Object(mut object) if is_seed_envelope(&object) => {
            let mode = object
                .remove("mode")
                .map(parse_seed_mode)
                .transpose()?
                .unwrap_or_default();
            let rows = object
                .remove("rows")
                .ok_or_else(|| anyhow!("seed request must include rows"))?;
            Ok(SeedRequest {
                mode,
                rows: parse_seed_rows(rows)?,
            })
        }
        Value::Object(row) => Ok(SeedRequest {
            mode: SeedMode::Append,
            rows: vec![row],
        }),
        _ => Err(anyhow!(
            "seed request must be a row object, row array, or object with rows"
        )),
    }
}

fn is_seed_envelope(object: &Map<String, Value>) -> bool {
    object.contains_key("mode")
        || object
            .get("rows")
            .is_some_and(|rows| rows.is_array() || rows.is_object())
}

fn parse_seed_rows(value: Value) -> Result<Vec<Map<String, Value>>> {
    match value {
        Value::Array(values) => values
            .into_iter()
            .enumerate()
            .map(|(idx, value)| match value {
                Value::Object(row) => Ok(row),
                _ => Err(anyhow!("seed row at index {idx} must be an object")),
            })
            .collect(),
        Value::Object(row) => Ok(vec![row]),
        _ => Err(anyhow!(
            "seed rows must be an object or an array of objects"
        )),
    }
}

fn parse_seed_mode(value: Value) -> Result<SeedMode> {
    let Some(mode) = value.as_str() else {
        return Err(anyhow!("seed mode must be a string"));
    };
    match mode.to_ascii_lowercase().as_str() {
        "append" => Ok(SeedMode::Append),
        "replace" => Ok(SeedMode::Replace),
        _ => Err(anyhow!("seed mode must be append or replace")),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use serde_json::json;

    use super::*;
    use crate::sql::engine::{Engine, EngineConfig};

    #[test]
    fn parses_seed_payload_shapes() {
        let request = parse_seed_payload(json!([
            { "email": "a@example.com" },
            { "email": "b@example.com" }
        ]))
        .unwrap();
        assert_eq!(request.mode, SeedMode::Append);
        assert_eq!(request.rows.len(), 2);

        let request = parse_seed_payload(json!({
            "mode": "replace",
            "rows": { "email": "c@example.com" }
        }))
        .unwrap();
        assert_eq!(request.mode, SeedMode::Replace);
        assert_eq!(request.rows.len(), 1);

        let request = parse_seed_payload(json!({
            "rows": { "email": "d@example.com" }
        }))
        .unwrap();
        assert_eq!(request.mode, SeedMode::Append);
        assert_eq!(request.rows.len(), 1);
    }

    #[test]
    fn seed_table_extends_schema_and_replaces_rows() {
        let engine = test_engine("seed-table");
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        let (status, _) = rt.block_on(seed_table(
            Path("seeded_users".to_string()),
            State(engine.clone()),
            Json(json!({
                "rows": [
                    { "email": "a@example.com" },
                    { "email": "b@example.com", "score": 20 }
                ]
            })),
        ));
        assert_eq!(status, StatusCode::OK);

        let rows = engine
            .execute_sql("SELECT email, score FROM seeded_users ORDER BY email")
            .unwrap();
        assert_eq!(rows[0].rows.len(), 2);
        assert_eq!(
            rows[0].rows[1].get("score").and_then(Value::as_i64),
            Some(20)
        );

        let (status, _) = rt.block_on(seed_table(
            Path("seeded_users".to_string()),
            State(engine.clone()),
            Json(json!({
                "mode": "replace",
                "rows": [
                    { "email": "c@example.com", "score": 30 }
                ]
            })),
        ));
        assert_eq!(status, StatusCode::OK);

        let rows = engine
            .execute_sql("SELECT email, score FROM seeded_users")
            .unwrap();
        assert_eq!(rows[0].rows.len(), 1);
        assert_eq!(
            rows[0].rows[0].get("email").and_then(Value::as_str),
            Some("c@example.com")
        );
    }

    fn test_engine(name: &str) -> Arc<Engine> {
        let dir = std::env::temp_dir().join(format!(
            "my-sqweel-debug-http-{name}-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        Arc::new(
            Engine::open_with_data_dir(EngineConfig::default(), Some(&dir.to_string_lossy()))
                .unwrap(),
        )
    }
}
