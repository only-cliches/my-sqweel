use super::*;

impl RawEngine {
    pub fn snapshot(&self) -> Snapshot {
        let schemas = self
            .schemas
            .iter()
            .map(|it| (it.key().clone(), (**it.value()).clone()))
            .collect();
        let rows = self
            .rows
            .iter()
            .map(|it| (it.key().clone(), (**it.value()).clone()))
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
            self.schemas.insert(k, v.into());
        }
        for (k, v) in snapshot.rows {
            self.rows.insert(k, v.into());
        }
        for (k, v) in snapshot.auto_inc {
            self.auto_inc.insert(k, v);
        }
        self.rebuild_indexes_all();
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
            self.rows.insert(table.clone(), SharedTable::default());
            self.indexes.remove(&table);
            self.clear_auto_inc(&table);
            self.rebuild_indexes(&table);
        }
        Ok(())
    }

    pub fn reset_table_rows(&self, table: &str) -> Result<()> {
        if !self.schemas.contains_key(table) {
            return Err(anyhow!("unknown table: {table}"));
        }
        self.rows.insert(table.to_string(), SharedTable::default());
        self.indexes.remove(table);
        self.clear_auto_inc(table);
        self.rebuild_indexes(table);

        Ok(())
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

        let schema = self
            .schemas
            .get(table)
            .ok_or_else(|| anyhow!("unknown table: {table}"))?;
        for column in seed_row_columns(&rows) {
            if !schema
                .columns
                .keys()
                .any(|known| known.eq_ignore_ascii_case(&column))
            {
                return Err(anyhow!("unknown column: {column}"));
            }
        }
        drop(schema);

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
