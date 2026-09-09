use super::*;
use crate::storage::StorageWrite;

impl RawEngine {
    pub fn snapshot(&self) -> Snapshot {
        let schemas = self
            .schemas
            .iter()
            .map(|it| (it.key().clone(), it.value().clone()))
            .collect();
        let rows = self
            .rows
            .iter()
            .map(|it| (it.key().clone(), it.value().clone()))
            .collect();
        let auto_inc = self
            .auto_inc
            .iter()
            .map(|it| (it.key().clone(), *it.value()))
            .collect();
        Snapshot {
            version: 1,
            created_at: Utc::now().to_rfc3339(),
            schemas,
            rows,
            auto_inc,
            views: self
                .views
                .iter()
                .map(|it| (it.key().clone(), it.value().clone()))
                .collect(),
            index_comments: self
                .index_comments
                .iter()
                .map(|it| (it.key().clone(), it.value().clone()))
                .collect(),
        }
    }

    pub(super) fn apply_snapshot(&self, snapshot: Snapshot) {
        self.views.clear();
        self.index_comments.clear();
        for (k, v) in snapshot.views {
            self.views.insert(k, v);
        }
        for (k, v) in snapshot.index_comments {
            self.index_comments.insert(k, v);
        }
        self.schemas.clear();
        self.rows.clear();
        self.auto_inc.clear();
        self.indexes.clear();
        for (k, v) in snapshot.schemas {
            self.schemas.insert(k, v);
        }
        for (k, v) in snapshot.rows {
            self.rows.insert(k, v);
        }
        for (k, v) in snapshot.auto_inc {
            self.auto_inc.insert(k, v);
        }
        self.rebuild_indexes_all();
    }

    pub(super) fn load_from_storage(&self) -> Result<()> {
        let tables = self.storage.smembers(storage_tables_key())?;
        if tables.is_empty() {
            return Ok(());
        }

        let mut schemas = BTreeMap::new();
        let mut rows = BTreeMap::new();

        for table in tables {
            let schema = self.load_schema_from_storage(&table)?;
            schemas.insert(table.clone(), schema);

            let mut table_rows = BTreeMap::new();
            for pk in self.storage.smembers(&storage_table_pks_key(&table))? {
                let row_hash = self.storage.hgetall(&storage_row_key(&table, &pk))?;
                if row_hash.is_empty() {
                    continue;
                }
                table_rows.insert(pk, decode_stored_row(&row_hash)?);
            }
            rows.insert(table, table_rows);
        }

        let auto_inc = self
            .storage
            .hgetall(STORAGE_AUTO_INC_KEY)?
            .into_iter()
            .filter_map(|(key, value)| value.parse::<i64>().ok().map(|value| (key, value)))
            .collect();

        self.apply_snapshot(Snapshot {
            version: 1,
            created_at: Utc::now().to_rfc3339(),
            schemas,
            rows,
            auto_inc,
            views: BTreeMap::new(),
            index_comments: BTreeMap::new(),
        });
        Ok(())
    }

    pub(super) fn load_schema_from_storage(&self, table: &str) -> Result<TableSchemaHint> {
        let root = self.storage.hgetall(&storage_schema_key(table))?;
        let mut schema = TableSchemaHint {
            table: root
                .get("table")
                .cloned()
                .unwrap_or_else(|| table.to_string()),
            updated_at: root
                .get("updated_at")
                .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
                .map(|value| value.with_timezone(&Utc)),
            column_order: decode_column_order(root.get("column_order")),
            ..TableSchemaHint::default()
        };

        for column in self.storage.smembers(&storage_schema_columns_key(table))? {
            let fields = self
                .storage
                .hgetall(&storage_schema_column_key(table, &column))?;
            schema
                .columns
                .insert(column, decode_column_hint_from_hash(&fields));
        }

        schema.primary_key = self
            .storage
            .smembers(&storage_schema_pk_key(table))?
            .into_iter()
            .collect();
        schema.unique = self
            .storage
            .smembers(&storage_schema_uniques_key(table))?
            .into_iter()
            .filter_map(|value| decode_unique_columns(&value))
            .collect();
        schema.indexes = self
            .storage
            .smembers(&storage_schema_indexes_key(table))?
            .into_iter()
            .filter_map(|value| decode_index_hint(&value))
            .collect();
        schema.foreign_keys = self
            .storage
            .smembers(&storage_schema_foreign_keys_key(table))?
            .into_iter()
            .filter_map(|value| decode_foreign_key_hint(&value))
            .collect();
        repair_column_order(&mut schema);

        Ok(schema)
    }

    pub(super) fn persist_schema(&self, table: &str) -> Result<()> {
        let Some(schema) = self.schemas.get(table).map(|schema| schema.clone()) else {
            return Ok(());
        };
        self.delete_schema_from_storage(table)?;
        self.storage
            .write_batch(schema_storage_writes(table, &schema))
    }

    pub(super) fn persist_auto_inc(&self) -> Result<()> {
        let current = self
            .auto_inc
            .iter()
            .map(|item| (item.key().clone(), *item.value()))
            .collect::<BTreeMap<_, _>>();
        let stored = self.storage.hgetall(STORAGE_AUTO_INC_KEY)?;
        let mut writes = Vec::new();
        for key in stored.keys() {
            if !current.contains_key(key) {
                writes.push(StorageWrite::HDel {
                    key: STORAGE_AUTO_INC_KEY.to_string(),
                    field: key.clone(),
                });
            }
        }
        for (key, value) in current {
            writes.push(StorageWrite::HSet {
                key: STORAGE_AUTO_INC_KEY.to_string(),
                field: key,
                value: value.to_string(),
            });
        }
        self.storage.write_batch(writes)
    }

    pub(super) fn persist_row(&self, table: &str, pk: &str, row: &StoredRow) -> Result<()> {
        let key = storage_row_key(table, pk);
        self.storage.sadd(&storage_table_pks_key(table), pk)?;
        self.storage.del(&key)?;
        persist_stored_row(self.storage.as_ref(), &key, row)
    }

    pub(super) fn persist_row_batch(
        &self,
        table: &str,
        deleted: &BTreeSet<String>,
        rows: &BTreeMap<String, StoredRow>,
    ) -> Result<()> {
        let mut writes = Vec::new();
        let table_pks = storage_table_pks_key(table);
        for pk in deleted {
            writes.push(StorageWrite::SRem {
                key: table_pks.clone(),
                member: pk.clone(),
            });
            writes.push(StorageWrite::Del {
                key: storage_row_key(table, pk),
            });
        }
        for (pk, row) in rows {
            let key = storage_row_key(table, pk);
            writes.push(StorageWrite::SAdd {
                key: table_pks.clone(),
                member: pk.clone(),
            });
            writes.push(StorageWrite::Del { key: key.clone() });
            writes.push(StorageWrite::HSet {
                key: key.clone(),
                field: "_id".to_string(),
                value: encode_json_value(&row.id),
            });
            writes.push(StorageWrite::HSet {
                key: key.clone(),
                field: "_table".to_string(),
                value: row.table.clone(),
            });
            writes.push(StorageWrite::HSet {
                key: key.clone(),
                field: "_createdAt".to_string(),
                value: row.created_at.to_rfc3339(),
            });
            writes.push(StorageWrite::HSet {
                key: key.clone(),
                field: "_updatedAt".to_string(),
                value: row.updated_at.to_rfc3339(),
            });
            writes.push(StorageWrite::HSet {
                key: key.clone(),
                field: "_version".to_string(),
                value: row.version.to_string(),
            });
            for (column, value) in &row.data {
                writes.push(StorageWrite::HSet {
                    key: key.clone(),
                    field: format!("data:{column}"),
                    value: encode_json_value(value),
                });
            }
        }
        self.storage.write_batch(writes)
    }

    pub(super) fn delete_row_from_storage(&self, table: &str, pk: &str) -> Result<()> {
        self.storage.srem(&storage_table_pks_key(table), pk)?;
        self.storage.del(&storage_row_key(table, pk))
    }

    pub(super) fn delete_table_rows_from_storage(&self, table: &str) -> Result<()> {
        let mut writes = self
            .storage
            .keys(&storage_row_pattern(table))?
            .into_iter()
            .map(|key| StorageWrite::Del { key })
            .collect::<Vec<_>>();
        writes.push(StorageWrite::Del {
            key: storage_table_pks_key(table),
        });
        self.storage.write_batch(writes)
    }

    pub(super) fn delete_table_from_storage(&self, table: &str) -> Result<()> {
        self.delete_table_rows_from_storage(table)?;
        self.delete_schema_from_storage(table)?;
        self.storage.srem(storage_tables_key(), table)?;
        self.persist_auto_inc()
    }

    pub(super) fn delete_schema_from_storage(&self, table: &str) -> Result<()> {
        let mut writes = [
            storage_schema_key(table),
            storage_schema_columns_key(table),
            storage_schema_pk_key(table),
            storage_schema_uniques_key(table),
            storage_schema_indexes_key(table),
            storage_schema_foreign_keys_key(table),
        ]
        .into_iter()
        .map(|key| StorageWrite::Del { key })
        .collect::<Vec<_>>();
        writes.extend(
            self.storage
                .keys(&storage_schema_column_pattern(table))?
                .into_iter()
                .map(|key| StorageWrite::Del { key }),
        );
        self.storage.write_batch(writes)
    }

    pub fn drift_report(&self) -> Value {
        let mut tables = Map::new();
        for schema in self.schemas.iter() {
            let table = schema.table.clone();
            let schema_columns = schema.columns.keys().cloned().collect::<BTreeSet<_>>();
            let table_rows = self
                .rows
                .get(&table)
                .map(|rows| rows.clone())
                .unwrap_or_default();
            let mut missing_columns: BTreeMap<String, usize> = BTreeMap::new();
            let mut extra_columns: BTreeMap<String, usize> = BTreeMap::new();

            for row in table_rows.values() {
                let row_columns = row.data.keys().cloned().collect::<BTreeSet<_>>();
                for col in schema_columns.difference(&row_columns) {
                    *missing_columns.entry(col.clone()).or_default() += 1;
                }
                for col in row_columns.difference(&schema_columns) {
                    *extra_columns.entry(col.clone()).or_default() += 1;
                }
            }

            let unique_duplicates = unique_duplicate_report(&schema, &table_rows);
            tables.insert(
                table.clone(),
                json!({
                    "table": table,
                    "rowCount": table_rows.len(),
                    "schemaColumns": schema_columns,
                    "missingColumns": missing_columns,
                    "extraColumns": extra_columns,
                    "uniqueDuplicates": unique_duplicates,
                }),
            );
        }

        json!({
            "version": 1,
            "createdAt": Utc::now().to_rfc3339(),
            "tables": tables,
        })
    }

    pub fn rebuild_indexes_for_table(&self, table: &str) -> Result<()> {
        if !self.schemas.contains_key(table) {
            return Err(anyhow!("unknown table: {table}"));
        }
        self.rebuild_indexes(table);
        Ok(())
    }

    pub fn rebuild_indexes_for_all_tables(&self) {
        self.rebuild_indexes_all();
    }

    pub fn reset_all_rows(&self) -> Result<()> {
        let tables = self
            .schemas
            .iter()
            .map(|schema| schema.key().clone())
            .collect::<Vec<_>>();
        for table in tables {
            self.rows.insert(table.clone(), BTreeMap::new());
            self.indexes.remove(&table);
            self.clear_auto_inc(&table);
            self.rebuild_indexes(&table);
            self.delete_table_rows_from_storage(&table)?;
        }
        self.persist_auto_inc()
    }

    pub fn reset_table_rows(&self, table: &str) -> Result<()> {
        if !self.schemas.contains_key(table) {
            return Err(anyhow!("unknown table: {table}"));
        }
        self.rows.insert(table.to_string(), BTreeMap::new());
        self.indexes.remove(table);
        self.clear_auto_inc(table);
        self.rebuild_indexes(table);
        self.delete_table_rows_from_storage(table)?;
        self.persist_auto_inc()
    }

    /// Search document updates are explicit upserts, independent of SQL INSERT policy.
    /// The coordinator publishes the complete batch only after every row succeeds.
    pub fn upsert_json_documents(
        &self,
        table: &str,
        rows: Vec<Map<String, Value>>,
        merge: bool,
    ) -> Result<u64> {
        let primary_key = self
            .schemas
            .get(table)
            .ok_or_else(|| anyhow!("unknown search index {table}"))?
            .primary_key
            .clone();
        let count = rows.len() as u64;
        for mut row in rows {
            if merge {
                let existing = self.rows.get(table).and_then(|stored| {
                    stored
                        .values()
                        .find(|existing| {
                            !primary_key.is_empty()
                                && primary_key.iter().all(|key| {
                                    row.get(key).is_some() && row.get(key) == existing.data.get(key)
                                })
                        })
                        .map(|existing| existing.data.clone())
                });
                if let Some(mut existing) = existing {
                    existing.extend(row);
                    row = existing;
                }
            }
            self.ensure_schema_for_seed(table, std::slice::from_ref(&row))?;
            self.insert_prepared_rows(
                table,
                vec![row],
                InsertRowsOptions {
                    ignore: false,
                    replace: true,
                    on_duplicate: &[],
                    returning: None,
                },
            )?;
        }
        Ok(count)
    }

    pub fn seed_json_rows(
        &self,
        table: &str,
        rows: Vec<Map<String, Value>>,
        mode: SeedMode,
    ) -> Result<SeedReport> {
        if table.trim().is_empty() {
            return Err(anyhow!("seed table name must not be empty"));
        }

        let rows_seeded = rows.len() as u64;
        if mode == SeedMode::Replace && self.schemas.contains_key(table) {
            self.reset_table_rows(table)?;
        }

        if rows.is_empty() {
            return Ok(SeedReport {
                table: table.to_string(),
                mode,
                rows_seeded,
                rows_affected: 0,
                last_insert_id: 0,
            });
        }

        self.ensure_schema_for_seed(table, &rows)?;
        let result = self.insert_prepared_rows(
            table,
            rows,
            InsertRowsOptions {
                ignore: false,
                replace: false,
                on_duplicate: &[],
                returning: None,
            },
        )?;

        Ok(SeedReport {
            table: table.to_string(),
            mode,
            rows_seeded,
            rows_affected: result.rows_affected,
            last_insert_id: result.last_insert_id,
        })
    }
}

fn schema_storage_writes(table: &str, schema: &TableSchemaHint) -> Vec<StorageWrite> {
    let mut writes = Vec::new();
    writes.push(StorageWrite::SAdd {
        key: storage_tables_key().to_string(),
        member: table.to_string(),
    });
    let schema_key = storage_schema_key(table);
    writes.push(StorageWrite::HSet {
        key: schema_key.clone(),
        field: "table".to_string(),
        value: schema.table.clone(),
    });
    writes.push(StorageWrite::HSet {
        key: schema_key,
        field: "column_order".to_string(),
        value: encode_column_order(schema),
    });
    if let Some(updated_at) = &schema.updated_at {
        writes.push(StorageWrite::HSet {
            key: storage_schema_key(table),
            field: "updated_at".to_string(),
            value: updated_at.to_rfc3339(),
        });
    }
    for (column, hint) in &schema.columns {
        writes.push(StorageWrite::SAdd {
            key: storage_schema_columns_key(table),
            member: column.clone(),
        });
        append_column_hint_writes(&mut writes, &storage_schema_column_key(table, column), hint);
    }
    for column in &schema.primary_key {
        writes.push(StorageWrite::SAdd {
            key: storage_schema_pk_key(table),
            member: column.clone(),
        });
    }
    for unique in &schema.unique {
        writes.push(StorageWrite::SAdd {
            key: storage_schema_uniques_key(table),
            member: encode_unique_columns(unique),
        });
    }
    for index in &schema.indexes {
        writes.push(StorageWrite::SAdd {
            key: storage_schema_indexes_key(table),
            member: encode_index_hint(index),
        });
    }
    for foreign_key in &schema.foreign_keys {
        writes.push(StorageWrite::SAdd {
            key: storage_schema_foreign_keys_key(table),
            member: encode_foreign_key_hint(foreign_key),
        });
    }
    writes
}
