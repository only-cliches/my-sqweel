use super::*;

impl Engine {
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
        }
    }

    pub fn restore_snapshot(&self, snapshot: Snapshot) {
        self.apply_snapshot(snapshot);
        if let Err(err) = self.persist_all() {
            tracing::warn!(error = %err, "failed to persist restored snapshot to Lux");
        }
    }

    pub(super) fn apply_snapshot(&self, snapshot: Snapshot) {
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

    pub(super) fn persist_all(&self) -> Result<()> {
        let snapshot = self.snapshot();
        for key in self.storage.keys(STORAGE_NAMESPACE_PATTERN)? {
            self.storage.del(&key)?;
        }

        for (table, schema) in &snapshot.schemas {
            self.storage.sadd(storage_tables_key(), table)?;
            self.storage
                .hset(&storage_schema_key(table), "table", &schema.table)?;
            self.storage.hset(
                &storage_schema_key(table),
                "column_order",
                &encode_column_order(schema),
            )?;
            if let Some(updated_at) = &schema.updated_at {
                self.storage.hset(
                    &storage_schema_key(table),
                    "updated_at",
                    &updated_at.to_rfc3339(),
                )?;
            }

            for (column, hint) in &schema.columns {
                self.storage
                    .sadd(&storage_schema_columns_key(table), column)?;
                persist_column_hint(
                    self.storage.as_ref(),
                    &storage_schema_column_key(table, column),
                    hint,
                )?;
            }
            for column in &schema.primary_key {
                self.storage.sadd(&storage_schema_pk_key(table), column)?;
            }
            for unique in &schema.unique {
                self.storage.sadd(
                    &storage_schema_uniques_key(table),
                    &encode_unique_columns(unique),
                )?;
            }
            for index in &schema.indexes {
                self.storage.sadd(
                    &storage_schema_indexes_key(table),
                    &encode_index_hint(index),
                )?;
            }
            for foreign_key in &schema.foreign_keys {
                self.storage.sadd(
                    &storage_schema_foreign_keys_key(table),
                    &encode_foreign_key_hint(foreign_key),
                )?;
            }
        }

        for (key, value) in &snapshot.auto_inc {
            self.storage
                .hset(STORAGE_AUTO_INC_KEY, key, &value.to_string())?;
        }

        for (table, table_rows) in &snapshot.rows {
            self.storage.sadd(storage_tables_key(), table)?;
            for (pk, row) in table_rows {
                self.storage.sadd(&storage_table_pks_key(table), pk)?;
                persist_stored_row(self.storage.as_ref(), &storage_row_key(table, pk), row)?;
            }
        }

        Ok(())
    }

    pub(super) fn persist_schema(&self, table: &str) -> Result<()> {
        let Some(schema) = self.schemas.get(table).map(|schema| schema.clone()) else {
            return Ok(());
        };
        self.storage.sadd(storage_tables_key(), table)?;
        self.delete_schema_from_storage(table)?;
        self.storage
            .hset(&storage_schema_key(table), "table", &schema.table)?;
        self.storage.hset(
            &storage_schema_key(table),
            "column_order",
            &encode_column_order(&schema),
        )?;
        if let Some(updated_at) = &schema.updated_at {
            self.storage.hset(
                &storage_schema_key(table),
                "updated_at",
                &updated_at.to_rfc3339(),
            )?;
        }
        for (column, hint) in &schema.columns {
            self.storage
                .sadd(&storage_schema_columns_key(table), column)?;
            persist_column_hint(
                self.storage.as_ref(),
                &storage_schema_column_key(table, column),
                hint,
            )?;
        }
        for column in &schema.primary_key {
            self.storage.sadd(&storage_schema_pk_key(table), column)?;
        }
        for unique in &schema.unique {
            self.storage.sadd(
                &storage_schema_uniques_key(table),
                &encode_unique_columns(unique),
            )?;
        }
        for index in &schema.indexes {
            self.storage.sadd(
                &storage_schema_indexes_key(table),
                &encode_index_hint(index),
            )?;
        }
        for foreign_key in &schema.foreign_keys {
            self.storage.sadd(
                &storage_schema_foreign_keys_key(table),
                &encode_foreign_key_hint(foreign_key),
            )?;
        }
        Ok(())
    }

    pub(super) fn persist_auto_inc(&self) -> Result<()> {
        let current = self
            .auto_inc
            .iter()
            .map(|item| (item.key().clone(), *item.value()))
            .collect::<BTreeMap<_, _>>();
        let stored = self.storage.hgetall(STORAGE_AUTO_INC_KEY)?;
        for key in stored.keys() {
            if !current.contains_key(key) {
                self.storage.hdel(STORAGE_AUTO_INC_KEY, key)?;
            }
        }
        for (key, value) in current {
            self.storage
                .hset(STORAGE_AUTO_INC_KEY, &key, &value.to_string())?;
        }
        Ok(())
    }

    pub(super) fn persist_row(&self, table: &str, pk: &str, row: &StoredRow) -> Result<()> {
        let key = storage_row_key(table, pk);
        self.storage.sadd(&storage_table_pks_key(table), pk)?;
        self.storage.del(&key)?;
        persist_stored_row(self.storage.as_ref(), &key, row)
    }

    pub(super) fn delete_row_from_storage(&self, table: &str, pk: &str) -> Result<()> {
        self.storage.srem(&storage_table_pks_key(table), pk)?;
        self.storage.del(&storage_row_key(table, pk))
    }

    pub(super) fn delete_table_rows_from_storage(&self, table: &str) -> Result<()> {
        for key in self.storage.keys(&storage_row_pattern(table))? {
            self.storage.del(&key)?;
        }
        self.storage.del(&storage_table_pks_key(table))?;
        Ok(())
    }

    pub(super) fn delete_table_from_storage(&self, table: &str) -> Result<()> {
        self.delete_table_rows_from_storage(table)?;
        self.delete_schema_from_storage(table)?;
        self.storage.srem(storage_tables_key(), table)?;
        self.persist_auto_inc()
    }

    pub(super) fn delete_schema_from_storage(&self, table: &str) -> Result<()> {
        self.storage.del(&storage_schema_key(table))?;
        self.storage.del(&storage_schema_columns_key(table))?;
        self.storage.del(&storage_schema_pk_key(table))?;
        self.storage.del(&storage_schema_uniques_key(table))?;
        self.storage.del(&storage_schema_indexes_key(table))?;
        self.storage.del(&storage_schema_foreign_keys_key(table))?;
        for key in self.storage.keys(&storage_schema_column_pattern(table))? {
            self.storage.del(&key)?;
        }
        Ok(())
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
