use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredRow {
    pub id: Value,
    pub table: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub version: u64,
    pub data: serde_json::Map<String, Value>,
}

impl StoredRow {
    pub fn new(table: impl Into<String>, id: Value, data: serde_json::Map<String, Value>) -> Self {
        let now = Utc::now();
        Self {
            id,
            table: table.into(),
            created_at: now,
            updated_at: now,
            version: 1,
            data,
        }
    }
}
