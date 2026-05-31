use super::*;

impl Engine {
    pub(super) fn select_query(&self, query: Query) -> Result<QueryResult> {
        let order_by = query
            .order_by
            .map(|order_by| order_by.exprs)
            .unwrap_or_default();
        let limit = query.limit;
        let offset = query.offset;
        let select = match *query.body {
            SetExpr::Select(select) => select,
            _ => return Err(anyhow!("only SELECT is supported")),
        };
        self.select_from(select, &order_by, limit.as_ref(), offset.as_ref())
    }

    pub(super) fn select_from(
        &self,
        select: Box<Select>,
        order_by: &[OrderByExpr],
        limit: Option<&Expr>,
        offset: Option<&Offset>,
    ) -> Result<QueryResult> {
        if select.from.is_empty() {
            let last_insert_id = self.last_insert_id.load(AtomicOrdering::Relaxed);
            let mut rows = vec![Map::new()]
                .into_iter()
                .filter(|row| {
                    self.matches_selection_ctx(select.selection.as_ref(), row, last_insert_id)
                        .unwrap_or(false)
                })
                .collect::<Vec<_>>();
            if let Some(result) = aggregate_select_result(
                &select,
                rows.clone(),
                order_by,
                limit,
                offset,
                last_insert_id,
            )? {
                return Ok(result);
            }

            self.materialize_projection_values(&select.projection, &mut rows, last_insert_id)?;
            apply_ordering(&mut rows, order_by)?;
            apply_limit_offset(&mut rows, limit, offset)?;
            let rows = rows
                .into_iter()
                .map(|row| self.project_row_ctx(&select.projection, &row, last_insert_id))
                .collect::<Result<Vec<_>>>()?;
            return Ok(QueryResult {
                rows_affected: 0,
                last_insert_id: 0,
                columns: infer_projection_columns(&select.projection),
                rows,
            });
        }

        if select.from.len() != 1 {
            return Err(anyhow!("only single FROM root is supported"));
        }
        let root = &select.from[0];
        if matches!(root.relation, TableFactor::Derived { .. }) {
            let mut rows = self.select_derived_rows(&select, root)?;
            let last_insert_id = self.last_insert_id.load(AtomicOrdering::Relaxed);
            if let Some(result) = aggregate_select_result(
                &select,
                rows.clone(),
                order_by,
                limit,
                offset,
                last_insert_id,
            )? {
                return Ok(result);
            }
            self.materialize_projection_values(&select.projection, &mut rows, last_insert_id)?;
            apply_ordering(&mut rows, order_by)?;
            apply_limit_offset(&mut rows, limit, offset)?;
            let rows = rows
                .into_iter()
                .map(|row| self.project_row_ctx(&select.projection, &row, last_insert_id))
                .collect::<Result<Vec<_>>>()?;
            return Ok(QueryResult {
                rows_affected: 0,
                last_insert_id: 0,
                columns: infer_projection_columns(&select.projection),
                rows,
            });
        }
        let root_name_full = table_factor_name_full(&root.relation)?;
        if root_name_full.eq_ignore_ascii_case("information_schema.tables") {
            return self.select_information_schema_tables(&select);
        }
        if root_name_full.eq_ignore_ascii_case("information_schema.schemata") {
            return self.select_information_schema_schemata(&select);
        }
        if root_name_full.eq_ignore_ascii_case("information_schema.columns") {
            return self.select_information_schema_columns(&select);
        }
        if root_name_full.eq_ignore_ascii_case("information_schema.table_constraints") {
            return self.select_information_schema_table_constraints(&select);
        }
        if root_name_full.eq_ignore_ascii_case("information_schema.statistics") {
            return self.select_information_schema_statistics(&select);
        }
        if root_name_full.eq_ignore_ascii_case("information_schema.key_column_usage") {
            return self.select_information_schema_key_column_usage(&select);
        }
        if root_name_full.eq_ignore_ascii_case("information_schema.referential_constraints") {
            return self.select_information_schema_referential_constraints(&select);
        }

        let mut rows = if root.joins.is_empty() {
            self.select_single_table(&select, root)?
        } else {
            self.select_with_joins(&select, root)?
        };

        let last_insert_id = self.last_insert_id.load(AtomicOrdering::Relaxed);
        if let Some(result) = aggregate_select_result(
            &select,
            rows.clone(),
            order_by,
            limit,
            offset,
            last_insert_id,
        )? {
            return Ok(result);
        }

        self.materialize_projection_values(&select.projection, &mut rows, last_insert_id)?;
        apply_ordering(&mut rows, order_by)?;
        apply_limit_offset(&mut rows, limit, offset)?;

        let rows = rows
            .into_iter()
            .map(|row| self.project_row_ctx(&select.projection, &row, last_insert_id))
            .collect::<Result<Vec<_>>>()?;
        let columns = infer_projection_columns(&select.projection);
        Ok(QueryResult {
            rows_affected: 0,
            last_insert_id: 0,
            columns,
            rows,
        })
    }

    pub(super) fn select_single_table(
        &self,
        select: &Select,
        root: &TableWithJoins,
    ) -> Result<Vec<Map<String, Value>>> {
        let table = table_factor_name(&root.relation)?;
        let filter = select.selection.as_ref();
        let mut rows = Vec::new();

        if let Some(index_hit) = try_index_lookup(filter, &table)
            && let Some(index_rows) = self
                .indexes
                .get(&table)
                .and_then(|idx| idx.get(&index_hit.0).cloned())
            && let Some(keys) = index_rows.get(&index_hit.1)
            && let Some(table_rows) = self.rows.get(&table)
        {
            for key in keys {
                if let Some(row) = table_rows.get(key) {
                    let view = self.current_schema_row(&table, &row.data);
                    if self.matches_selection_ctx(filter, &view, 0)? {
                        rows.push(view);
                    }
                }
            }
            return Ok(rows);
        }

        if let Some(table_rows) = self.rows.get(&table) {
            for row in table_rows.values() {
                let view = self.current_schema_row(&table, &row.data);
                if !self.matches_selection_ctx(filter, &view, 0)? {
                    continue;
                }
                rows.push(view);
            }
        }
        Ok(rows)
    }

    pub(super) fn select_with_joins(
        &self,
        select: &Select,
        root: &TableWithJoins,
    ) -> Result<Vec<Map<String, Value>>> {
        let (left_table, left_alias) = table_factor_name_and_alias(&root.relation)?;
        let mut joined = Vec::new();
        let left_rows = self
            .rows
            .get(&left_table)
            .map(|r| r.clone())
            .unwrap_or_default();

        for (left_key, left_row) in &left_rows {
            let left_data = self.current_schema_row(&left_table, &left_row.data);
            let mut left_map = left_data.clone();
            add_qualified_columns(&mut left_map, &left_table, &left_data);
            if let Some(alias) = &left_alias {
                add_qualified_columns(&mut left_map, alias, &left_data);
            }

            let mut current = vec![left_map];
            for join in &root.joins {
                let (right_table, right_alias) = table_factor_name_and_alias(&join.relation)?;
                let right_rows = self
                    .rows
                    .get(&right_table)
                    .map(|r| r.clone())
                    .unwrap_or_default();
                let mut next = Vec::new();
                for candidate in &current {
                    let mut matched = false;
                    for right_row in right_rows.values() {
                        let right_data = self.current_schema_row(&right_table, &right_row.data);
                        let mut combined = candidate.clone();
                        add_qualified_columns(&mut combined, &right_table, &right_data);
                        if let Some(alias) = &right_alias {
                            add_qualified_columns(&mut combined, alias, &right_data);
                        }
                        for (k, v) in &right_data {
                            combined.entry(k.clone()).or_insert_with(|| v.clone());
                        }
                        if self.join_matches_ctx(&join.join_operator, &combined)? {
                            matched = true;
                            next.push(combined);
                        }
                    }
                    if !matched && matches!(join.join_operator, JoinOperator::LeftOuter(_)) {
                        let mut combined = candidate.clone();
                        let right_nulls = self.current_schema_null_row(&right_table);
                        add_qualified_columns(&mut combined, &right_table, &right_nulls);
                        if let Some(alias) = &right_alias {
                            add_qualified_columns(&mut combined, alias, &right_nulls);
                        }
                        for (k, v) in &right_nulls {
                            combined.entry(k.clone()).or_insert_with(|| v.clone());
                        }
                        next.push(combined);
                    }
                }
                current = next;
            }

            for c in current {
                if self.matches_selection_ctx(select.selection.as_ref(), &c, 0)? {
                    joined.push(c);
                }
            }
            let _ = left_key;
        }
        Ok(joined)
    }

    pub(super) fn current_schema_row(
        &self,
        table: &str,
        data: &Map<String, Value>,
    ) -> Map<String, Value> {
        let Some(schema) = self.schemas.get(table).map(|schema| schema.clone()) else {
            return data.clone();
        };
        if schema.columns.is_empty() {
            return data.clone();
        }

        let mut out = Map::new();
        for column in ordered_schema_columns(&schema) {
            let Some(hint) = schema.columns.get(&column) else {
                continue;
            };
            let value = data
                .get(&column)
                .cloned()
                .or_else(|| read_default_value(hint))
                .unwrap_or(Value::Null);
            out.insert(column, coerce_value_for_column(value, hint));
        }
        out
    }

    pub(super) fn current_schema_null_row(&self, table: &str) -> Map<String, Value> {
        let Some(schema) = self.schemas.get(table).map(|schema| schema.clone()) else {
            return Map::new();
        };

        ordered_schema_columns(&schema)
            .into_iter()
            .map(|column| (column, Value::Null))
            .collect()
    }

    pub(super) fn materialize_projection_values(
        &self,
        projection: &[SelectItem],
        rows: &mut [Map<String, Value>],
        last_insert_id: u64,
    ) -> Result<()> {
        for row in rows {
            for item in projection {
                match item {
                    SelectItem::UnnamedExpr(expr) => {
                        let column = projection_expr_column_name(expr);
                        if !row.contains_key(&column) {
                            row.insert(column, self.eval_expr_ctx(expr, row, last_insert_id)?);
                        }
                    }
                    SelectItem::ExprWithAlias { expr, alias }
                        if !row.contains_key(&alias.value) =>
                    {
                        row.insert(
                            alias.value.clone(),
                            self.eval_expr_ctx(expr, row, last_insert_id)?,
                        );
                    }
                    _ => {}
                }
            }
        }
        Ok(())
    }

    pub(super) fn select_derived_rows(
        &self,
        select: &Select,
        root: &TableWithJoins,
    ) -> Result<Vec<Map<String, Value>>> {
        let TableFactor::Derived {
            subquery, alias, ..
        } = &root.relation
        else {
            return Err(anyhow!("unsupported derived table factor"));
        };
        if !root.joins.is_empty() {
            return Err(anyhow!(
                "joins against derived tables are not supported yet"
            ));
        }

        let result = self.select_query((**subquery).clone())?;
        let alias_name = alias.as_ref().map(|alias| alias.name.value.clone());
        let mut rows = Vec::new();
        for row in result.rows {
            let mut derived = row.clone();
            if let Some(alias) = &alias_name {
                for (key, value) in row {
                    derived.insert(format!("{alias}.{key}"), value);
                }
            }
            if self.matches_selection_ctx(select.selection.as_ref(), &derived, 0)? {
                rows.push(derived);
            }
        }
        Ok(rows)
    }

    pub(super) fn project_row_ctx(
        &self,
        projection: &[SelectItem],
        data: &Map<String, Value>,
        last_insert_id: u64,
    ) -> Result<Map<String, Value>> {
        project_row_with(projection, data, |expr| {
            self.eval_expr_ctx(expr, data, last_insert_id)
        })
    }

    pub(super) fn eval_expr_ctx(
        &self,
        expr: &Expr,
        data: &Map<String, Value>,
        last_insert_id: u64,
    ) -> Result<Value> {
        if let Some(value) = data.get(&projection_expr_column_name(expr)) {
            return Ok(value.clone());
        }
        if let Some(value) = system_variable_expr_value(expr) {
            return Ok(value);
        }

        match expr {
            Expr::Subquery(query) => self.eval_scalar_subquery(query),
            Expr::Exists { subquery, negated } => {
                let exists = !self.select_query((**subquery).clone())?.rows.is_empty();
                Ok(Value::Bool(if *negated { !exists } else { exists }))
            }
            Expr::InSubquery {
                expr,
                subquery,
                negated,
            } => {
                let value = self.eval_expr_ctx(expr, data, last_insert_id)?;
                let result = self.select_query((**subquery).clone())?;
                let hit = result.rows.iter().any(|row| {
                    first_projected_value(row, &result.columns)
                        .map(|candidate| mysql_eq(&value, &candidate))
                        .unwrap_or(false)
                });
                Ok(Value::Bool(if *negated { !hit } else { hit }))
            }
            Expr::Nested(expr) => self.eval_expr_ctx(expr, data, last_insert_id),
            Expr::UnaryOp { op, expr } if op.to_string() == "-" => {
                let value = self.eval_expr_ctx(expr, data, last_insert_id)?;
                Ok(number_from_f64(-json_to_f64_lossy(&value)?))
            }
            Expr::UnaryOp { op, expr } if op.to_string().eq_ignore_ascii_case("NOT") => {
                Ok(Value::Bool(!value_truthy(&self.eval_expr_ctx(
                    expr,
                    data,
                    last_insert_id,
                )?)))
            }
            Expr::UnaryOp { expr, .. } => self.eval_expr_ctx(expr, data, last_insert_id),
            Expr::BinaryOp { left, op, right } => {
                let left_value = self.eval_expr_ctx(left, data, last_insert_id)?;
                let right_value = self.eval_expr_ctx(right, data, last_insert_id)?;
                eval_binary_values(left_value, op, right_value)
            }
            Expr::IsNull(expr) => Ok(Value::Bool(
                self.eval_expr_ctx(expr, data, last_insert_id)? == Value::Null,
            )),
            Expr::IsNotNull(expr) => Ok(Value::Bool(
                self.eval_expr_ctx(expr, data, last_insert_id)? != Value::Null,
            )),
            Expr::InList {
                expr,
                list,
                negated,
            } => {
                let value = self.eval_expr_ctx(expr, data, last_insert_id)?;
                let hit = list.iter().any(|item| {
                    self.eval_expr_ctx(item, data, last_insert_id)
                        .map(|item| mysql_eq(&value, &item))
                        .unwrap_or(false)
                });
                Ok(Value::Bool(if *negated { !hit } else { hit }))
            }
            Expr::Like {
                expr,
                pattern,
                negated,
                ..
            } => {
                let target = self.eval_expr_ctx(expr, data, last_insert_id)?;
                let pattern = self.eval_expr_ctx(pattern, data, last_insert_id)?;
                Ok(eval_like_values(target, pattern, *negated))
            }
            Expr::Cast {
                expr, data_type, ..
            } => cast_json_value(
                self.eval_expr_ctx(expr, data, last_insert_id)?,
                &data_type.to_string(),
            ),
            _ => eval_expr(expr, data, last_insert_id),
        }
    }

    pub(super) fn eval_scalar_subquery(&self, query: &Query) -> Result<Value> {
        let result = self.select_query(query.clone())?;
        Ok(result
            .rows
            .first()
            .and_then(|row| first_projected_value(row, &result.columns))
            .unwrap_or(Value::Null))
    }

    pub(super) fn matches_selection_ctx(
        &self,
        selection: Option<&Expr>,
        data: &Map<String, Value>,
        last_insert_id: u64,
    ) -> Result<bool> {
        matches_selection_with(selection, |expr| {
            self.eval_expr_ctx(expr, data, last_insert_id)
        })
    }

    pub(super) fn join_matches_ctx(
        &self,
        join: &JoinOperator,
        data: &Map<String, Value>,
    ) -> Result<bool> {
        match join {
            JoinOperator::Inner(constraint) | JoinOperator::LeftOuter(constraint) => {
                match constraint {
                    JoinConstraint::On(expr) => self.matches_selection_ctx(Some(expr), data, 0),
                    _ => Ok(true),
                }
            }
            _ => Err(anyhow!("unsupported join type")),
        }
    }
    pub(super) fn select_information_schema_tables(&self, select: &Select) -> Result<QueryResult> {
        let mut rows = Vec::new();
        for table in self.schemas.iter() {
            let mut row = Map::new();
            row.insert("table_schema".to_string(), Value::String("app".to_string()));
            row.insert("table_name".to_string(), Value::String(table.key().clone()));
            rows.push(row);
        }
        virtual_select_result(select, rows)
    }

    pub(super) fn select_information_schema_schemata(
        &self,
        select: &Select,
    ) -> Result<QueryResult> {
        let mut row = Map::new();
        row.insert("catalog_name".to_string(), Value::String("def".to_string()));
        row.insert("schema_name".to_string(), Value::String("app".to_string()));
        row.insert(
            "default_character_set_name".to_string(),
            Value::String("utf8mb4".to_string()),
        );
        row.insert(
            "default_collation_name".to_string(),
            Value::String("utf8mb4_general_ci".to_string()),
        );
        virtual_select_result(select, vec![row])
    }

    pub(super) fn select_information_schema_columns(&self, select: &Select) -> Result<QueryResult> {
        let mut rows = Vec::new();
        for schema in self.schemas.iter() {
            for (idx, col) in ordered_schema_columns(&schema).into_iter().enumerate() {
                let Some(hint) = schema.columns.get(&col) else {
                    continue;
                };
                let is_pk = schema.primary_key.iter().any(|pk| pk == &col);
                let is_unique = schema
                    .unique
                    .iter()
                    .any(|cols| cols.len() == 1 && cols.first() == Some(&col));
                let data_type = hint.sql_type.clone().unwrap_or_else(|| "text".to_string());
                let mut row = Map::new();
                row.insert("table_schema".to_string(), Value::String("app".to_string()));
                row.insert(
                    "table_name".to_string(),
                    Value::String(schema.table.clone()),
                );
                row.insert("column_name".to_string(), Value::String(col.clone()));
                row.insert(
                    "ordinal_position".to_string(),
                    Value::Number(Number::from(idx + 1)),
                );
                row.insert(
                    "is_nullable".to_string(),
                    Value::String(if hint.nullable == Some(false) {
                        "NO".to_string()
                    } else {
                        "YES".to_string()
                    }),
                );
                row.insert(
                    "column_default".to_string(),
                    hint.default
                        .clone()
                        .map(Value::String)
                        .unwrap_or(Value::Null),
                );
                row.insert("column_type".to_string(), Value::String(data_type.clone()));
                row.insert("data_type".to_string(), Value::String(data_type));
                row.insert(
                    "column_key".to_string(),
                    Value::String(if is_pk {
                        "PRI".to_string()
                    } else if is_unique {
                        "UNI".to_string()
                    } else {
                        String::new()
                    }),
                );
                row.insert(
                    "extra".to_string(),
                    Value::String(if hint.auto_increment {
                        "auto_increment".to_string()
                    } else {
                        String::new()
                    }),
                );
                rows.push(row);
            }
        }
        virtual_select_result(select, rows)
    }

    pub(super) fn select_information_schema_table_constraints(
        &self,
        select: &Select,
    ) -> Result<QueryResult> {
        let mut rows = Vec::new();
        for schema in self.schemas.iter() {
            if !schema.primary_key.is_empty() {
                let mut row = Map::new();
                row.insert(
                    "constraint_schema".to_string(),
                    Value::String("app".to_string()),
                );
                row.insert("table_schema".to_string(), Value::String("app".to_string()));
                row.insert(
                    "table_name".to_string(),
                    Value::String(schema.table.clone()),
                );
                row.insert(
                    "constraint_name".to_string(),
                    Value::String("PRIMARY".to_string()),
                );
                row.insert(
                    "constraint_type".to_string(),
                    Value::String("PRIMARY KEY".to_string()),
                );
                rows.push(row);
            }

            for unique in &schema.unique {
                let mut row = Map::new();
                row.insert(
                    "constraint_schema".to_string(),
                    Value::String("app".to_string()),
                );
                row.insert("table_schema".to_string(), Value::String("app".to_string()));
                row.insert(
                    "table_name".to_string(),
                    Value::String(schema.table.clone()),
                );
                row.insert(
                    "constraint_name".to_string(),
                    Value::String(format!("{}_{}_uniq", schema.table, unique.join("_"))),
                );
                row.insert(
                    "constraint_type".to_string(),
                    Value::String("UNIQUE".to_string()),
                );
                rows.push(row);
            }

            for foreign_key in &schema.foreign_keys {
                let mut row = Map::new();
                row.insert(
                    "constraint_schema".to_string(),
                    Value::String("app".to_string()),
                );
                row.insert("table_schema".to_string(), Value::String("app".to_string()));
                row.insert(
                    "table_name".to_string(),
                    Value::String(schema.table.clone()),
                );
                row.insert(
                    "constraint_name".to_string(),
                    Value::String(foreign_key.name.clone()),
                );
                row.insert(
                    "constraint_type".to_string(),
                    Value::String("FOREIGN KEY".to_string()),
                );
                rows.push(row);
            }
        }
        virtual_select_result(select, rows)
    }

    pub(super) fn select_information_schema_statistics(
        &self,
        select: &Select,
    ) -> Result<QueryResult> {
        let mut rows = Vec::new();
        for schema in self.schemas.iter() {
            for index in &schema.indexes {
                for (idx, col) in index.columns.iter().enumerate() {
                    let mut row = Map::new();
                    row.insert("table_schema".to_string(), Value::String("app".to_string()));
                    row.insert(
                        "table_name".to_string(),
                        Value::String(schema.table.clone()),
                    );
                    row.insert("index_name".to_string(), Value::String(index.name.clone()));
                    row.insert("column_name".to_string(), Value::String(col.clone()));
                    row.insert(
                        "seq_in_index".to_string(),
                        Value::Number(Number::from(idx + 1)),
                    );
                    row.insert(
                        "non_unique".to_string(),
                        Value::Number(Number::from(if index.unique { 0 } else { 1 })),
                    );
                    rows.push(row);
                }
            }
        }
        virtual_select_result(select, rows)
    }

    pub(super) fn select_information_schema_key_column_usage(
        &self,
        select: &Select,
    ) -> Result<QueryResult> {
        let mut rows = Vec::new();
        for schema in self.schemas.iter() {
            for (idx, col) in schema.primary_key.iter().enumerate() {
                let mut row = Map::new();
                row.insert("table_schema".to_string(), Value::String("app".to_string()));
                row.insert(
                    "constraint_schema".to_string(),
                    Value::String("app".to_string()),
                );
                row.insert(
                    "table_name".to_string(),
                    Value::String(schema.table.clone()),
                );
                row.insert("column_name".to_string(), Value::String(col.clone()));
                row.insert(
                    "constraint_name".to_string(),
                    Value::String("PRIMARY".to_string()),
                );
                row.insert(
                    "ordinal_position".to_string(),
                    Value::Number(Number::from(idx + 1)),
                );
                rows.push(row);
            }
            for foreign_key in &schema.foreign_keys {
                for (idx, col) in foreign_key.columns.iter().enumerate() {
                    let mut row = Map::new();
                    row.insert("table_schema".to_string(), Value::String("app".to_string()));
                    row.insert(
                        "constraint_schema".to_string(),
                        Value::String("app".to_string()),
                    );
                    row.insert(
                        "table_name".to_string(),
                        Value::String(schema.table.clone()),
                    );
                    row.insert("column_name".to_string(), Value::String(col.clone()));
                    row.insert(
                        "constraint_name".to_string(),
                        Value::String(foreign_key.name.clone()),
                    );
                    row.insert(
                        "ordinal_position".to_string(),
                        Value::Number(Number::from(idx + 1)),
                    );
                    row.insert(
                        "referenced_table_schema".to_string(),
                        Value::String("app".to_string()),
                    );
                    row.insert(
                        "referenced_table_name".to_string(),
                        Value::String(foreign_key.referenced_table.clone()),
                    );
                    row.insert(
                        "referenced_column_name".to_string(),
                        Value::String(
                            foreign_key
                                .referenced_columns
                                .get(idx)
                                .cloned()
                                .unwrap_or_default(),
                        ),
                    );
                    rows.push(row);
                }
            }
        }
        virtual_select_result(select, rows)
    }

    pub(super) fn select_information_schema_referential_constraints(
        &self,
        select: &Select,
    ) -> Result<QueryResult> {
        let mut rows = Vec::new();
        for schema in self.schemas.iter() {
            for foreign_key in &schema.foreign_keys {
                let mut row = Map::new();
                row.insert(
                    "constraint_catalog".to_string(),
                    Value::String("def".to_string()),
                );
                row.insert(
                    "constraint_schema".to_string(),
                    Value::String("app".to_string()),
                );
                row.insert(
                    "constraint_name".to_string(),
                    Value::String(foreign_key.name.clone()),
                );
                row.insert(
                    "unique_constraint_catalog".to_string(),
                    Value::String("def".to_string()),
                );
                row.insert(
                    "unique_constraint_schema".to_string(),
                    Value::String("app".to_string()),
                );
                row.insert(
                    "unique_constraint_name".to_string(),
                    Value::String("PRIMARY".to_string()),
                );
                row.insert(
                    "match_option".to_string(),
                    Value::String("NONE".to_string()),
                );
                row.insert(
                    "update_rule".to_string(),
                    Value::String(
                        foreign_key
                            .on_update
                            .clone()
                            .unwrap_or_else(|| "NO ACTION".to_string()),
                    ),
                );
                row.insert(
                    "delete_rule".to_string(),
                    Value::String(
                        foreign_key
                            .on_delete
                            .clone()
                            .unwrap_or_else(|| "NO ACTION".to_string()),
                    ),
                );
                row.insert(
                    "table_name".to_string(),
                    Value::String(schema.table.clone()),
                );
                row.insert(
                    "referenced_table_name".to_string(),
                    Value::String(foreign_key.referenced_table.clone()),
                );
                rows.push(row);
            }
        }
        virtual_select_result(select, rows)
    }

    pub(super) fn show_tables(&self) -> QueryResult {
        let columns = vec!["Tables_in_app".to_string()];
        let rows = self
            .schemas
            .iter()
            .map(|schema| {
                let mut row = Map::new();
                row.insert(columns[0].clone(), Value::String(schema.table.clone()));
                row
            })
            .collect();
        QueryResult {
            rows_affected: 0,
            last_insert_id: 0,
            columns,
            rows,
        }
    }

    pub(super) fn show_columns(&self, table: &str) -> QueryResult {
        let columns = ["Field", "Type", "Null", "Key", "Default", "Extra"]
            .into_iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>();
        let rows = self
            .schemas
            .get(table)
            .map(|schema| {
                ordered_schema_columns(&schema)
                    .into_iter()
                    .filter_map(|column| {
                        let hint = schema.columns.get(&column)?;
                        let mut row = Map::new();
                        let key = if schema.primary_key.iter().any(|pk| pk == &column) {
                            "PRI"
                        } else if schema
                            .unique
                            .iter()
                            .any(|cols| cols.len() == 1 && cols.first() == Some(&column))
                        {
                            "UNI"
                        } else if schema.indexes.iter().any(|index| {
                            !index.unique
                                && index.columns.len() == 1
                                && index.columns.first() == Some(&column)
                        }) {
                            "MUL"
                        } else {
                            ""
                        };
                        row.insert("Field".to_string(), Value::String(column.clone()));
                        row.insert(
                            "Type".to_string(),
                            Value::String(hint.sql_type.clone().unwrap_or_else(|| "text".into())),
                        );
                        row.insert(
                            "Null".to_string(),
                            Value::String(if hint.nullable == Some(false) {
                                "NO".to_string()
                            } else {
                                "YES".to_string()
                            }),
                        );
                        row.insert("Key".to_string(), Value::String(key.to_string()));
                        row.insert(
                            "Default".to_string(),
                            hint.default
                                .clone()
                                .map(Value::String)
                                .unwrap_or(Value::Null),
                        );
                        row.insert(
                            "Extra".to_string(),
                            Value::String(if hint.auto_increment {
                                "auto_increment".to_string()
                            } else {
                                String::new()
                            }),
                        );
                        Some(row)
                    })
                    .collect()
            })
            .unwrap_or_default();
        QueryResult {
            rows_affected: 0,
            last_insert_id: 0,
            columns,
            rows,
        }
    }

    pub(super) fn show_index(&self, table: &str) -> QueryResult {
        let columns = [
            "Table",
            "Non_unique",
            "Key_name",
            "Seq_in_index",
            "Column_name",
            "Collation",
            "Cardinality",
            "Sub_part",
            "Packed",
            "Null",
            "Index_type",
            "Comment",
            "Index_comment",
            "Visible",
            "Expression",
        ]
        .into_iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>();
        let mut rows = Vec::new();
        if let Some(schema) = self.schemas.get(table) {
            for index in &schema.indexes {
                for (idx, column) in index.columns.iter().enumerate() {
                    let mut row = Map::new();
                    row.insert("Table".to_string(), Value::String(schema.table.clone()));
                    row.insert(
                        "Non_unique".to_string(),
                        Value::Number(Number::from(if index.unique { 0 } else { 1 })),
                    );
                    row.insert("Key_name".to_string(), Value::String(index.name.clone()));
                    row.insert(
                        "Seq_in_index".to_string(),
                        Value::Number(Number::from(idx + 1)),
                    );
                    row.insert("Column_name".to_string(), Value::String(column.clone()));
                    row.insert("Collation".to_string(), Value::String("A".to_string()));
                    row.insert("Cardinality".to_string(), Value::Null);
                    row.insert("Sub_part".to_string(), Value::Null);
                    row.insert("Packed".to_string(), Value::Null);
                    row.insert(
                        "Null".to_string(),
                        Value::String(
                            schema
                                .columns
                                .get(column)
                                .and_then(|hint| hint.nullable)
                                .map(|nullable| if nullable { "YES" } else { "" })
                                .unwrap_or("YES")
                                .to_string(),
                        ),
                    );
                    row.insert("Index_type".to_string(), Value::String("BTREE".to_string()));
                    row.insert("Comment".to_string(), Value::String(String::new()));
                    row.insert("Index_comment".to_string(), Value::String(String::new()));
                    row.insert("Visible".to_string(), Value::String("YES".to_string()));
                    row.insert("Expression".to_string(), Value::Null);
                    rows.push(row);
                }
            }
        }
        QueryResult {
            rows_affected: 0,
            last_insert_id: 0,
            columns,
            rows,
        }
    }

    pub(super) fn show_create_table(&self, table: &str) -> QueryResult {
        let columns = vec!["Table".to_string(), "Create Table".to_string()];
        let create = self
            .schemas
            .get(table)
            .map(|schema| render_create_table(&schema))
            .unwrap_or_else(|| format!("CREATE TABLE `{table}` ()"));
        let mut row = Map::new();
        row.insert("Table".to_string(), Value::String(table.to_string()));
        row.insert("Create Table".to_string(), Value::String(create));
        QueryResult {
            rows_affected: 0,
            last_insert_id: 0,
            columns,
            rows: vec![row],
        }
    }

    pub(super) fn rename_table(&self, from: &str, to: &str) -> Result<QueryResult> {
        if self.schemas.contains_key(to) {
            return Err(anyhow!("table already exists: {to}"));
        }
        let Some((_, mut schema)) = self.schemas.remove(from) else {
            return Err(anyhow!("unknown table: {from}"));
        };
        schema.table = to.to_string();
        schema.updated_at = Some(Utc::now());
        self.schemas.insert(to.to_string(), schema);
        if let Some((_, mut table_rows)) = self.rows.remove(from) {
            for row in table_rows.values_mut() {
                row.table = to.to_string();
                row.updated_at = Utc::now();
            }
            self.rows.insert(to.to_string(), table_rows);
        }
        self.indexes.remove(from);
        self.rebuild_indexes(to);

        let auto_inc_updates = self
            .auto_inc
            .iter()
            .filter_map(|item| {
                item.key()
                    .strip_prefix(&format!("{from}:"))
                    .map(|suffix| (item.key().clone(), format!("{to}:{suffix}"), *item.value()))
            })
            .collect::<Vec<_>>();
        for (old, new, value) in auto_inc_updates {
            self.auto_inc.remove(&old);
            self.auto_inc.insert(new, value);
        }

        self.delete_table_from_storage(from)?;
        self.persist_schema(to)?;
        self.persist_auto_inc()?;
        if let Some(rows) = self.rows.get(to).map(|rows| rows.clone()) {
            for (pk, row) in rows {
                self.persist_row(to, &pk, &row)?;
            }
        }
        Ok(QueryResult::default())
    }
}
