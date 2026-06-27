use super::*;

impl Engine {
    pub(super) fn insert_rows(&self, insert: sqlparser::ast::Insert) -> Result<QueryResult> {
        let table = object_name(&insert.table_name)?;
        let explicit_columns: Vec<String> = insert.columns.into_iter().map(|i| i.value).collect();
        let query = insert
            .source
            .ok_or_else(|| anyhow!("missing INSERT source"))?;
        let ignore = insert.ignore;
        let replace = insert.replace_into;
        let returning = insert.returning;
        let on_duplicate = match insert.on {
            Some(OnInsert::DuplicateKeyUpdate(assignments)) => assignments,
            Some(_) | None => Vec::new(),
        };

        let mut prepared_rows = Vec::new();

        match *query.body {
            SetExpr::Values(v) => {
                let values = v.rows;
                let columns = self.resolve_insert_columns(&table, explicit_columns, &values)?;
                for row in values {
                    let mut data = Map::new();
                    for (idx, expr) in row.into_iter().enumerate() {
                        if let Some(col) = columns.get(idx) {
                            data.insert(col.clone(), self.eval_expr_ctx(&expr, &Map::new(), 0)?);
                        }
                    }
                    prepared_rows.push(data);
                }
            }
            SetExpr::Select(_) => {
                let select_query = Query {
                    body: query.body.clone(),
                    order_by: None,
                    limit: None,
                    offset: None,
                    fetch: None,
                    locks: vec![],
                    with: None,
                    for_clause: None,
                    format_clause: None,
                    limit_by: vec![],
                    settings: None,
                };
                let select_result = self.select_query(select_query)?;
                let columns = if explicit_columns.is_empty() {
                    select_result.columns.clone()
                } else {
                    explicit_columns
                };
                for row in select_result.rows {
                    let mut data = Map::new();
                    for (idx, col) in columns.iter().enumerate() {
                        let value = select_result
                            .columns
                            .get(idx)
                            .and_then(|src_col| row.get(src_col).cloned())
                            .unwrap_or(Value::Null);
                        data.insert(col.clone(), value);
                    }
                    prepared_rows.push(data);
                }
            }
            _ => return Err(anyhow!("only VALUES and SELECT insert are supported")),
        }

        self.insert_prepared_rows(
            &table,
            prepared_rows,
            InsertRowsOptions {
                ignore,
                replace,
                on_duplicate: &on_duplicate,
                returning: returning.as_deref(),
            },
        )
    }

    pub(super) fn insert_prepared_rows(
        &self,
        table: &str,
        rows: Vec<Map<String, Value>>,
        options: InsertRowsOptions<'_>,
    ) -> Result<QueryResult> {
        let mut affected = 0_u64;
        let mut first_insert_id = 0_u64;
        let mut rows_to_persist: BTreeMap<String, StoredRow> = BTreeMap::new();
        let mut rows_to_delete: BTreeSet<String> = BTreeSet::new();
        let mut returned_rows = Vec::new();
        {
            let mut table_rows = self.rows.entry(table.to_string()).or_default();
            for mut data in rows {
                self.apply_defaults(table, &mut data)?;
                self.apply_schema_types(table, &mut data)?;

                let (row_id, generated_id) = self.resolve_row_id(table, &data)?;
                let generated_insert_id = generated_id.then(|| value_to_u64(&row_id)).flatten();
                if !data.contains_key("id") || data.get("id").is_some_and(is_defaultish) {
                    data.insert("id".to_string(), row_id.clone());
                }

                let key = row_id.to_string();
                let conflict_keys = self.find_conflict_keys(table, &key, &data, &table_rows);

                if !conflict_keys.is_empty() {
                    if options.ignore {
                        continue;
                    }

                    if !options.on_duplicate.is_empty() {
                        let conflict_key = conflict_keys
                            .iter()
                            .next()
                            .ok_or_else(|| anyhow!("conflict row disappeared"))?;
                        let existing = table_rows
                            .get_mut(conflict_key)
                            .ok_or_else(|| anyhow!("conflict row disappeared"))?;
                        for assignment in options.on_duplicate {
                            let col = assignment_target_name(assignment);
                            let value =
                                eval_insert_update_value(&assignment.value, &existing.data, &data)?;
                            existing.data.insert(col, value);
                        }
                        existing.version += 1;
                        existing.updated_at = Utc::now();
                        returned_rows.push(existing.data.clone());
                        rows_to_persist.insert(conflict_key.clone(), existing.clone());
                        affected += 1;
                        continue;
                    }

                    if options.replace || self.cfg.unique_mode == UniqueMode::Overwrite {
                        for conflict_key in conflict_keys {
                            if table_rows.remove(&conflict_key).is_some() {
                                rows_to_delete.insert(conflict_key.clone());
                                rows_to_persist.remove(&conflict_key);
                            }
                        }
                    } else if conflict_keys.contains(&key) {
                        return Err(anyhow!("primary key conflict on {table}: {key}"));
                    } else {
                        self.enforce_unique_if_needed(table, &row_id, &data, &table_rows)?;
                    }
                } else {
                    self.enforce_unique_if_needed(table, &row_id, &data, &table_rows)?;
                }

                let stored = StoredRow::new(table.to_string(), row_id, data);
                table_rows.insert(key.clone(), stored.clone());
                if first_insert_id == 0 {
                    first_insert_id = generated_insert_id.unwrap_or(0);
                }
                returned_rows.push(stored.data.clone());
                rows_to_persist.insert(key, stored);
                affected += 1;
            }
        }
        self.rebuild_indexes(table);
        self.persist_auto_inc()?;
        for key in rows_to_delete {
            self.delete_row_from_storage(table, &key)?;
        }
        for (key, row) in rows_to_persist {
            self.persist_row(table, &key, &row)?;
        }
        if first_insert_id != 0 {
            self.last_insert_id
                .store(first_insert_id, AtomicOrdering::Relaxed);
        }

        self.returning_result(
            table,
            options.returning,
            returned_rows,
            affected,
            first_insert_id,
        )
    }

    pub(super) fn resolve_insert_columns(
        &self,
        table: &str,
        explicit_columns: Vec<String>,
        values: &[Vec<Expr>],
    ) -> Result<Vec<String>> {
        if !explicit_columns.is_empty() {
            self.ensure_schema_for_insert(table, &explicit_columns)?;
            return Ok(explicit_columns);
        }

        let width = values.iter().map(Vec::len).max().unwrap_or(0);
        if let Some(schema) = self.schemas.get(table).map(|schema| schema.clone()) {
            let mut columns = ordered_schema_columns(&schema);
            if width > columns.len() {
                let mut schema = schema;
                for idx in columns.len() + 1..=width {
                    let column = generated_position_column(idx);
                    add_schema_column(&mut schema, column.clone(), ColumnHint::default());
                    columns.push(column);
                }
                schema.updated_at = Some(Utc::now());
                self.schemas.insert(table.to_string(), schema);
                self.persist_schema(table)?;
            }
            return Ok(columns);
        }

        let columns = (1..=width)
            .map(generated_position_column)
            .collect::<Vec<_>>();
        self.ensure_schema_for_insert(table, &columns)?;
        Ok(columns)
    }

    pub(super) fn ensure_schema_for_insert(&self, table: &str, columns: &[String]) -> Result<()> {
        if self.schemas.contains_key(table) {
            return Ok(());
        }
        if columns.is_empty() {
            return Err(anyhow!(
                "cannot infer schema for {table}: INSERT must provide at least one value or named column"
            ));
        }

        let mut schema = TableSchemaHint {
            table: table.to_string(),
            updated_at: Some(Utc::now()),
            ..TableSchemaHint::default()
        };
        for column in columns {
            add_schema_column(&mut schema, column.clone(), ColumnHint::default());
        }
        self.schemas.insert(table.to_string(), schema);
        self.rows.entry(table.to_string()).or_default();
        self.persist_schema(table)
    }

    pub(super) fn ensure_schema_for_seed(
        &self,
        table: &str,
        rows: &[Map<String, Value>],
    ) -> Result<()> {
        let columns = seed_row_columns(rows);
        if columns.is_empty() {
            if self.schemas.contains_key(table) {
                return Ok(());
            }
            return Err(anyhow!(
                "cannot infer schema for {table}: seed rows must include at least one column"
            ));
        }

        let existed = self.schemas.contains_key(table);
        let mut schema = self
            .schemas
            .get(table)
            .map(|schema| schema.clone())
            .unwrap_or_else(|| TableSchemaHint {
                table: table.to_string(),
                ..TableSchemaHint::default()
            });

        let mut changed = !existed;
        for column in columns {
            if !schema.columns.contains_key(&column) {
                add_schema_column(&mut schema, column, ColumnHint::default());
                changed = true;
            }
        }

        if changed {
            schema.updated_at = Some(Utc::now());
            self.schemas.insert(table.to_string(), schema);
            self.rows.entry(table.to_string()).or_default();
            self.persist_schema(table)?;
        }

        Ok(())
    }
    pub(super) fn update_rows(
        &self,
        table: TableWithJoins,
        assignments: Vec<Assignment>,
        selection: Option<Expr>,
        returning: Option<Vec<SelectItem>>,
    ) -> Result<QueryResult> {
        let table_name = table_factor_name(&table.relation)?;
        if !self.schemas.contains_key(&table_name) {
            return Err(anyhow!("unknown table: {table_name}"));
        }
        let mut updated = 0_u64;
        let current_rows = self
            .rows
            .get(&table_name)
            .map(|rows| rows.clone())
            .unwrap_or_default();
        let mut next_rows = current_rows.clone();
        let mut changed_rows: BTreeMap<String, StoredRow> = BTreeMap::new();
        let mut deleted_keys: BTreeSet<String> = BTreeSet::new();
        let mut returned_rows = Vec::new();

        for (old_key, current_row) in &current_rows {
            if self.cfg.unique_mode == UniqueMode::Overwrite && !next_rows.contains_key(old_key) {
                continue;
            }
            if !self.matches_selection_ctx(selection.as_ref(), &current_row.data, 0)? {
                continue;
            }

            let mut updated_data = current_row.data.clone();
            for assignment in &assignments {
                let col = assignment_target_name(assignment);
                let value = self.eval_expr_ctx(&assignment.value, &updated_data, 0)?;
                updated_data.insert(col, value);
            }
            self.apply_defaults(&table_name, &mut updated_data)?;
            self.apply_schema_types(&table_name, &mut updated_data)?;

            let (row_id, new_key) =
                self.updated_row_identity(&table_name, current_row, &updated_data);

            let mut updated_row = current_row.clone();
            updated_row.id = row_id;
            updated_row.data = updated_data;
            updated_row.version += 1;
            updated_row.updated_at = Utc::now();

            next_rows.remove(old_key);
            if new_key != *old_key {
                deleted_keys.insert(old_key.clone());
                changed_rows.remove(old_key);
            }

            if self.cfg.unique_mode == UniqueMode::Overwrite {
                let conflict_keys =
                    self.find_conflict_keys(&table_name, &new_key, &updated_row.data, &next_rows);
                for conflict_key in conflict_keys {
                    if next_rows.remove(&conflict_key).is_some() {
                        deleted_keys.insert(conflict_key.clone());
                        changed_rows.remove(&conflict_key);
                    }
                }
            } else if next_rows.contains_key(&new_key) {
                return Err(anyhow!("primary key conflict on {table_name}: {new_key}"));
            }

            next_rows.insert(new_key.clone(), updated_row.clone());
            returned_rows.push(updated_row.data.clone());
            changed_rows.insert(new_key, updated_row);
            updated += 1;
        }

        self.validate_unique_constraints(&table_name, &next_rows)?;
        self.rows.insert(table_name.clone(), next_rows);
        self.rebuild_indexes(&table_name);
        for key in deleted_keys {
            self.delete_row_from_storage(&table_name, &key)?;
        }
        for (pk, row) in changed_rows {
            self.persist_row(&table_name, &pk, &row)?;
        }

        self.returning_result(&table_name, returning.as_deref(), returned_rows, updated, 0)
    }

    pub(super) fn delete_rows(&self, delete: sqlparser::ast::Delete) -> Result<QueryResult> {
        let returning = delete.returning;
        let table_name = match delete.from {
            sqlparser::ast::FromTable::WithFromKeyword(v) => v
                .first()
                .map(|t| table_factor_name(&t.relation))
                .transpose()?
                .ok_or_else(|| anyhow!("missing DELETE target table"))?,
            sqlparser::ast::FromTable::WithoutKeyword(v) => v
                .first()
                .map(|t| table_factor_name(&t.relation))
                .transpose()?
                .ok_or_else(|| anyhow!("missing DELETE target table"))?,
        };

        let mut deleted = 0_u64;
        let mut deleted_keys = Vec::new();
        let mut returned_rows = Vec::new();
        let current_rows = self
            .rows
            .get(&table_name)
            .map(|rows| rows.clone())
            .unwrap_or_default();
        let mut keys = Vec::new();
        for (k, row) in &current_rows {
            if self.matches_selection_ctx(delete.selection.as_ref(), &row.data, 0)? {
                keys.push(k.clone());
            }
        }

        if !keys.is_empty() {
            let mut next_rows = current_rows;
            for k in keys {
                if let Some(row) = next_rows.remove(&k) {
                    returned_rows.push(row.data);
                    deleted_keys.push(k);
                    deleted += 1;
                }
            }
            self.rows.insert(table_name.clone(), next_rows);
        }
        self.rebuild_indexes(&table_name);
        for key in deleted_keys {
            self.delete_row_from_storage(&table_name, &key)?;
        }

        self.returning_result(&table_name, returning.as_deref(), returned_rows, deleted, 0)
    }

    fn returning_result(
        &self,
        table: &str,
        returning: Option<&[SelectItem]>,
        rows: Vec<Map<String, Value>>,
        rows_affected: u64,
        last_insert_id: u64,
    ) -> Result<QueryResult> {
        let Some(projection) = returning else {
            return Ok(QueryResult {
                rows_affected,
                last_insert_id,
                columns: vec![],
                rows: vec![],
            });
        };

        let rows = rows
            .into_iter()
            .map(|row| {
                let row = self.current_schema_row(table, &row);
                self.project_row_ctx(projection, &row, last_insert_id)
            })
            .collect::<Result<Vec<_>>>()?;

        Ok(QueryResult {
            rows_affected,
            last_insert_id,
            columns: self.returning_columns(table, projection, rows.first()),
            rows,
        })
    }

    fn returning_columns(
        &self,
        table: &str,
        projection: &[SelectItem],
        first_row: Option<&Map<String, Value>>,
    ) -> Vec<String> {
        if projection
            .iter()
            .any(|item| matches!(item, SelectItem::Wildcard(_)))
        {
            if let Some(schema) = self.schemas.get(table).map(|schema| schema.clone()) {
                return ordered_schema_columns(&schema);
            }
            return first_row
                .map(|row| row.keys().cloned().collect())
                .unwrap_or_default();
        }

        infer_projection_columns(projection)
    }
    pub(super) fn apply_defaults(&self, table: &str, data: &mut Map<String, Value>) -> Result<()> {
        let Some(schema) = self.schemas.get(table).map(|schema| schema.clone()) else {
            return Ok(());
        };
        for (column, hint) in &schema.columns {
            if data.contains_key(column) && !data.get(column).is_some_and(is_defaultish) {
                continue;
            }
            if let Some(default) = &hint.default {
                data.insert(column.clone(), eval_default_value(default)?);
            }
        }
        Ok(())
    }

    pub(super) fn apply_schema_types(
        &self,
        table: &str,
        data: &mut Map<String, Value>,
    ) -> Result<()> {
        let Some(schema) = self.schemas.get(table).map(|schema| schema.clone()) else {
            return Ok(());
        };
        for (column, hint) in &schema.columns {
            if let Some(value) = data.get(column).cloned() {
                if value == Value::Null && hint.nullable == Some(false) && !hint.auto_increment {
                    return Err(anyhow!("column '{column}' cannot be null"));
                }
                data.insert(column.clone(), coerce_value_for_column(value, hint));
            } else if hint.nullable == Some(false) && hint.default.is_none() && !hint.auto_increment
            {
                return Err(anyhow!("column '{column}' does not have a default value"));
            }
        }
        Ok(())
    }

    pub(super) fn resolve_row_id(
        &self,
        table: &str,
        data: &Map<String, Value>,
    ) -> Result<(Value, bool)> {
        let pk_col = self
            .schemas
            .get(table)
            .and_then(|schema| schema.primary_key.first().cloned())
            .or_else(|| {
                if data.contains_key("id") {
                    Some("id".to_string())
                } else {
                    None
                }
            });

        if let Some(pk_col) = pk_col {
            let maybe_auto_inc = self
                .schemas
                .get(table)
                .and_then(|schema| schema.columns.get(&pk_col).cloned())
                .map(|c| c.auto_increment)
                .unwrap_or(false);

            if let Some(v) = data.get(&pk_col) {
                if maybe_auto_inc && is_defaultish(v) {
                    let next = self.next_auto_inc(table, &pk_col);
                    return Ok((Value::Number(Number::from(next)), true));
                }
                return Ok((v.clone(), false));
            }

            if maybe_auto_inc || pk_col == "id" {
                let next = self.next_auto_inc(table, &pk_col);
                return Ok((Value::Number(Number::from(next)), true));
            }
        }

        Ok((Value::String(uuid::Uuid::new_v4().to_string()), false))
    }

    pub(super) fn updated_row_identity(
        &self,
        table: &str,
        current_row: &StoredRow,
        data: &Map<String, Value>,
    ) -> (Value, String) {
        let pk_col = self
            .schemas
            .get(table)
            .and_then(|schema| schema.primary_key.first().cloned())
            .or_else(|| data.contains_key("id").then(|| "id".to_string()));

        let id = pk_col
            .as_deref()
            .and_then(|column| data.get(column).cloned())
            .unwrap_or_else(|| current_row.id.clone());
        let key = id.to_string();
        (id, key)
    }

    pub(super) fn enforce_unique_if_needed(
        &self,
        table: &str,
        row_id: &Value,
        data: &Map<String, Value>,
        table_rows: &BTreeMap<String, StoredRow>,
    ) -> Result<()> {
        if self.cfg.unique_mode != UniqueMode::Enforce {
            return Ok(());
        }
        let Some(schema) = self.schemas.get(table).map(|s| s.clone()) else {
            return Ok(());
        };
        if schema.unique.is_empty() {
            return Ok(());
        }

        for unique_cols in &schema.unique {
            let Some(key) = unique_key(data, unique_cols) else {
                continue;
            };
            for existing in table_rows.values() {
                if &existing.id == row_id {
                    continue;
                }
                if unique_key(&existing.data, unique_cols).as_ref() == Some(&key) {
                    return Err(anyhow!(
                        "unique constraint violation on {table}({})",
                        unique_cols.join(",")
                    ));
                }
            }
        }
        Ok(())
    }

    pub(super) fn validate_unique_constraints(
        &self,
        table: &str,
        table_rows: &BTreeMap<String, StoredRow>,
    ) -> Result<()> {
        if self.cfg.unique_mode != UniqueMode::Enforce {
            return Ok(());
        }
        let Some(schema) = self.schemas.get(table).map(|s| s.clone()) else {
            return Ok(());
        };

        for unique_cols in &schema.unique {
            let mut seen: BTreeMap<String, String> = BTreeMap::new();
            for (pk, row) in table_rows {
                let Some(key) = unique_key(&row.data, unique_cols) else {
                    continue;
                };
                if seen.insert(key, pk.clone()).is_some() {
                    return Err(anyhow!(
                        "unique constraint violation on {table}({})",
                        unique_cols.join(",")
                    ));
                }
            }
        }
        Ok(())
    }

    pub(super) fn find_conflict_keys(
        &self,
        table: &str,
        row_key: &str,
        data: &Map<String, Value>,
        table_rows: &BTreeMap<String, StoredRow>,
    ) -> BTreeSet<String> {
        let mut conflicts = BTreeSet::new();
        if table_rows.contains_key(row_key) {
            conflicts.insert(row_key.to_string());
        }

        if let Some(schema) = self.schemas.get(table).map(|s| s.clone()) {
            for unique_cols in &schema.unique {
                let Some(incoming) = unique_key(data, unique_cols) else {
                    continue;
                };
                for (existing_key, existing) in table_rows {
                    if unique_key(&existing.data, unique_cols).as_ref() == Some(&incoming) {
                        conflicts.insert(existing_key.clone());
                    }
                }
            }
        }

        conflicts
    }

    pub(super) fn next_auto_inc(&self, table: &str, column: &str) -> i64 {
        let key = format!("{table}:{column}");
        let mut slot = self.auto_inc.entry(key).or_insert(0);
        *slot += 1;
        *slot
    }

    pub(super) fn clear_auto_inc(&self, table: &str) {
        let prefix = format!("{table}:");
        let keys: Vec<String> = self
            .auto_inc
            .iter()
            .filter_map(|it| it.key().starts_with(&prefix).then(|| it.key().clone()))
            .collect();
        for key in keys {
            self.auto_inc.remove(&key);
        }
    }

    pub(super) fn rebuild_indexes_all(&self) {
        let tables: Vec<String> = self.rows.iter().map(|r| r.key().clone()).collect();
        for t in tables {
            self.rebuild_indexes(&t);
        }
    }

    pub(super) fn rebuild_indexes(&self, table: &str) {
        let Some(schema) = self.schemas.get(table).map(|s| s.clone()) else {
            return;
        };
        let Some(rows) = self.rows.get(table).map(|r| r.clone()) else {
            return;
        };
        let mut table_index: BTreeMap<String, BTreeMap<String, BTreeSet<String>>> = BTreeMap::new();

        for index in &schema.indexes {
            if index.columns.len() != 1 {
                continue;
            }
            let col = index.columns[0].clone();
            let mut map: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
            for (pk, row) in &rows {
                let view = self.current_schema_row(table, &row.data);
                let key = view.get(&col).cloned().unwrap_or(Value::Null).to_string();
                map.entry(key).or_default().insert(pk.clone());
            }
            table_index.insert(col, map);
        }
        self.indexes.insert(table.to_string(), table_index);
    }
}
