use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ColumnHint {
    pub sql_type: Option<String>,
    pub nullable: Option<bool>,
    pub default: Option<String>,
    pub primary_key: bool,
    pub auto_increment: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TableSchemaHint {
    pub table: String,
    pub columns: BTreeMap<String, ColumnHint>,
    #[serde(default)]
    pub column_order: Vec<String>,
    pub primary_key: Vec<String>,
    pub unique: Vec<Vec<String>>,
    #[serde(default)]
    pub indexes: Vec<IndexHint>,
    #[serde(default)]
    pub foreign_keys: Vec<ForeignKeyHint>,
    pub updated_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct IndexHint {
    pub name: String,
    pub columns: Vec<String>,
    pub unique: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct ForeignKeyHint {
    pub name: String,
    pub columns: Vec<String>,
    pub referenced_table: String,
    pub referenced_columns: Vec<String>,
    pub on_delete: Option<String>,
    pub on_update: Option<String>,
}
