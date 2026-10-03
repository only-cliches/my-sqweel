use super::custom::ErasedStorage;
use super::*;
use crate::sql::engine::EngineState;
use anyhow::{Result, anyhow};
const STORAGE_PAGE_SIZE: usize = 512;

pub(crate) async fn load_state(storage: &dyn ErasedStorage) -> Result<Option<EngineState>> {
    let Some(catalog) = storage.load_catalog().await? else {
        return Ok(None);
    };
    if catalog.version != 1 {
        return Err(anyhow!(
            "unsupported async storage catalog version: {}",
            catalog.version
        ));
    }
    let mut databases = std::collections::BTreeMap::new();
    for (database, metadata) in catalog.databases {
        let mut schemas = std::collections::BTreeMap::new();
        let mut rows = std::collections::BTreeMap::new();
        let mut auto_inc = std::collections::BTreeMap::new();
        for table in storage.list_tables(&database).await? {
            let Some(table_state) = storage.load_table(&database, &table).await? else {
                continue;
            };
            schemas.insert(table.clone(), table_state.schema);
            if let Some(value) = table_state.auto_increment {
                auto_inc.insert(table.clone(), value);
            }
            let mut table_rows = std::collections::BTreeMap::new();
            let mut cursor = None;
            loop {
                let page = storage
                    .scan_rows(RowScan {
                        database: database.clone(),
                        table: table.clone(),
                        cursor: cursor.clone(),
                        limit: STORAGE_PAGE_SIZE,
                    })
                    .await?;
                table_rows.extend(page.rows);
                let Some(next_cursor) = page.next_cursor else {
                    break;
                };
                if cursor.as_ref() == Some(&next_cursor) {
                    return Err(anyhow!("async storage returned a non-advancing row cursor"));
                }
                cursor = Some(next_cursor);
            }
            rows.insert(table, table_rows);
        }
        databases.insert(
            database,
            crate::sql::engine::Snapshot {
                version: 1,
                created_at: chrono::Utc::now().to_rfc3339(),
                schemas,
                rows,
                auto_inc,
                views: metadata.views,
                index_comments: metadata.index_comments,
            },
        );
    }
    Ok(Some(EngineState {
        version: 1,
        metadata: catalog.metadata,
        databases,
    }))
}

pub(crate) fn storage_catalog(state: &EngineState) -> StorageCatalog {
    StorageCatalog {
        version: 1,
        metadata: state.metadata.clone(),
        databases: state
            .databases
            .iter()
            .map(|(database, snapshot)| {
                (
                    database.clone(),
                    DatabaseMetadata {
                        views: snapshot.views.clone(),
                        index_comments: snapshot.index_comments.clone(),
                    },
                )
            })
            .collect(),
    }
}

pub(crate) fn storage_batch(before: &EngineState, after: &EngineState) -> StorageBatch {
    let mut batch = StorageBatch::default();
    if storage_catalog(before) != storage_catalog(after) {
        batch
            .metadata
            .push(MetadataMutation::PutCatalog(storage_catalog(after)));
    }
    for (database, previous) in &before.databases {
        if !after.databases.contains_key(database) {
            batch.metadata.push(MetadataMutation::DeleteDatabase {
                database: database.clone(),
            });
            for table in previous.schemas.keys() {
                batch.metadata.push(MetadataMutation::DeleteTable {
                    database: database.clone(),
                    table: table.clone(),
                });
            }
        }
    }
    for (database, current) in &after.databases {
        let previous = before.databases.get(database);
        let mut tables = std::collections::BTreeSet::new();
        if let Some(previous) = previous {
            tables.extend(previous.schemas.keys().cloned());
            tables.extend(previous.rows.keys().cloned());
        }
        tables.extend(current.schemas.keys().cloned());
        tables.extend(current.rows.keys().cloned());
        for table in tables {
            let old_schema = previous.and_then(|snapshot| snapshot.schemas.get(&table));
            let new_schema = current.schemas.get(&table);
            match (old_schema, new_schema) {
                (_, None) => batch.metadata.push(MetadataMutation::DeleteTable {
                    database: database.clone(),
                    table: table.clone(),
                }),
                (old, Some(schema)) => {
                    let old_auto = previous.and_then(|snapshot| snapshot.auto_inc.get(&table));
                    let new_auto = current.auto_inc.get(&table);
                    if old != Some(schema) || old_auto != new_auto {
                        batch.metadata.push(MetadataMutation::PutTable {
                            database: database.clone(),
                            table: table.clone(),
                            state: TableState {
                                schema: schema.clone(),
                                auto_increment: new_auto.copied(),
                            },
                        });
                    }
                }
            }
            let old_rows = previous.and_then(|snapshot| snapshot.rows.get(&table));
            let new_rows = current.rows.get(&table);
            if let Some(old_rows) = old_rows {
                for key in old_rows.keys() {
                    if new_rows.is_none_or(|rows| !rows.contains_key(key)) {
                        batch.rows.push(RowMutation::Delete {
                            database: database.clone(),
                            table: table.clone(),
                            key: key.clone(),
                        });
                    }
                }
            }
            if let Some(new_rows) = new_rows {
                for (key, row) in new_rows {
                    if old_rows.and_then(|rows| rows.get(key)) != Some(row) {
                        batch.rows.push(RowMutation::Put {
                            database: database.clone(),
                            table: table.clone(),
                            key: key.clone(),
                            row: row.clone(),
                        });
                    }
                }
            }
        }
    }
    batch
}
