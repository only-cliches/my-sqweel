use super::*;

impl Engine {
    pub(super) fn select_query(&self, query: Query) -> Result<QueryResult> {
        let order_by = query
            .order_by
            .map(|order_by| order_by.exprs)
            .unwrap_or_default();
        let limit = query.limit;
        let offset = query.offset;

        let mut rows = match &*query.body {
            SetExpr::Select(select) => {
                return self.select_from(select.clone(), &order_by, limit.as_ref(), offset.as_ref())
            }
            SetExpr::SetOperation { op, left, right, set_quantifier } => {
                let left_query = Query {
                    body: left.clone(),
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
                let right_query = Query {
                    body: right.clone(),
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
                let mut left_result = self.select_query(left_query)?;
                let right_result = self.select_query(right_query)?;

                match op {
                    sqlparser::ast::SetOperator::Union => {
                        let should_dedup = *set_quantifier != sqlparser::ast::SetQuantifier::All;
                        if should_dedup {
                            let mut seen = BTreeSet::new();
                            left_result.rows.retain(|row| {
                                seen.insert(encode_json_row(row))
                            });
                            for row in right_result.rows {
                                let row_key = encode_json_row(&row);
                                if seen.insert(row_key) {
                                    left_result.rows.push(row);
                                }
                            }
                        } else {
                            left_result.rows.extend(right_result.rows);
                        }
                        left_result.rows
                    }
                    _ => return Err(anyhow!("unsupported set operation")),
                }
            }
            _ => return Err(anyhow!("only SELECT and UNION are supported")),
        };

        apply_ordering(&mut rows, &order_by)?;
        apply_limit_offset(&mut rows, limit.as_ref(), offset.as_ref())?;

        Ok(QueryResult {
            rows_affected: 0,
            last_insert_id: 0,
            columns: if let SetExpr::Select(select) = &*query.body {
                infer_projection_columns(&select.projection)
            } else {
                vec![]
            },
            rows,
        })
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

        if select.from.is_empty() {
            // Already handled above
            unreachable!()
        }

        let root = &select.from[0];

        if select.from.len() > 1 {
            // Handle implicit cross-join (comma-separated FROM)
            let mut joined = Vec::new();
            let first_table = table_factor_name(&root.relation)?;
            let first_rows = self
                .rows
                .get(&first_table)
                .map(|r| r.clone())
                .unwrap_or_default();

            let mut current = Vec::new();
            for (_, first_row) in &first_rows {
                let first_data = self.current_schema_row(&first_table, &first_row.data);
                let mut first_map = first_data.clone();
                add_qualified_columns(&mut first_map, &first_table, &first_data);

                current.push(first_map);
            }

            // Cross join each subsequent table
            for from_table in &select.from[1..] {
                let (table_name, alias) = table_factor_name_and_alias(&from_table.relation)?;
                let table_rows = self
                    .rows
                    .get(&table_name)
                    .map(|r| r.clone())
                    .unwrap_or_default();

                let mut next = Vec::new();
                for candidate in &current {
                    for row in table_rows.values() {
                        let table_data = self.current_schema_row(&table_name, &row.data);
                        let mut combined = candidate.clone();
                        add_qualified_columns(&mut combined, &table_name, &table_data);
                        if let Some(ref a) = alias {
                            add_qualified_columns(&mut combined, a, &table_data);
                        }
                        for (k, v) in &table_data {
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

            let last_insert_id = self.last_insert_id.load(AtomicOrdering::Relaxed);
            if let Some(result) = aggregate_select_result(
                &select,
                joined.clone(),
                &order_by,
                limit,
                offset,
                last_insert_id,
            )? {
                return Ok(result);
            }

            self.materialize_projection_values(&select.projection, &mut joined, last_insert_id)?;
            apply_ordering(&mut joined, &order_by)?;
            apply_limit_offset(&mut joined, limit, offset)?;

            let rows = joined
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
        if root_name_full.eq_ignore_ascii_case("information_schema.character_sets") {
            return self.select_information_schema_character_sets(&select);
        }
        if root_name_full.eq_ignore_ascii_case("information_schema.collations") {
            return self.select_information_schema_collations(&select);
        }
        if root_name_full.eq_ignore_ascii_case("information_schema.views") {
            return self.select_information_schema_views(&select);
        }
        if root_name_full.eq_ignore_ascii_case("information_schema.routines") {
            return self.select_information_schema_routines(&select);
        }
        if root_name_full.eq_ignore_ascii_case("information_schema.engines") {
            return self.select_information_schema_engines(&select);
        }
        if root_name_full.eq_ignore_ascii_case("information_schema.processlist") {
            return self.select_information_schema_processlist(&select);
        }
        if root_name_full.eq_ignore_ascii_case("information_schema.session_variables") {
            return self.select_information_schema_session_variables(&select);
        }
        if root_name_full.eq_ignore_ascii_case("information_schema.global_variables") {
            return self.select_information_schema_global_variables(&select);
        }
        if root_name_full.eq_ignore_ascii_case("information_schema.keywords") {
            return self.select_information_schema_keywords(&select);
        }
        if root_name_full.eq_ignore_ascii_case("information_schema.triggers") {
            return self.select_information_schema_triggers(&select);
        }
        if root_name_full.eq_ignore_ascii_case("information_schema.check_constraints") {
            return self.select_information_schema_check_constraints(&select);
        }
        if root_name_full.eq_ignore_ascii_case("information_schema.files") {
            return self.select_information_schema_files(&select);
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
            Expr::Between {
                expr,
                negated,
                low,
                high,
            } => {
                let v = self.eval_expr_ctx(expr, data, last_insert_id)?;
                let lo = self.eval_expr_ctx(low, data, last_insert_id)?;
                let hi = self.eval_expr_ctx(high, data, last_insert_id)?;
                let hit = !compare_predicate_values(v.clone(), lo, |a, b| a < b)
                    && !compare_predicate_values(v, hi, |a, b| a > b);
                Ok(Value::Bool(if *negated { !hit } else { hit }))
            }
            Expr::Case {
                operand,
                conditions,
                results,
                else_result,
            } => {
                for (cond, result) in conditions.iter().zip(results.iter()) {
                    let matches = match operand {
                        Some(op) => mysql_eq(
                            &self.eval_expr_ctx(op, data, last_insert_id)?,
                            &self.eval_expr_ctx(cond, data, last_insert_id)?,
                        ),
                        None => value_truthy(&self.eval_expr_ctx(cond, data, last_insert_id)?),
                    };
                    if matches {
                        return self.eval_expr_ctx(result, data, last_insert_id);
                    }
                }
                match else_result {
                    Some(e) => self.eval_expr_ctx(e, data, last_insert_id),
                    None => Ok(Value::Null),
                }
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
            row.insert("table_type".to_string(), Value::String("BASE TABLE".to_string()));
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
                rows.push(key_column_usage_row(
                    &schema.table,
                    "PRIMARY",
                    col,
                    idx + 1,
                    None,
                    None,
                ));
            }
            for unique in &schema.unique {
                let constraint_name = format!("{}_{}_uniq", schema.table, unique.join("_"));
                for (idx, col) in unique.iter().enumerate() {
                    rows.push(key_column_usage_row(
                        &schema.table,
                        &constraint_name,
                        col,
                        idx + 1,
                        None,
                        None,
                    ));
                }
            }
            for foreign_key in &schema.foreign_keys {
                for (idx, col) in foreign_key.columns.iter().enumerate() {
                    let referenced = foreign_key.referenced_columns.get(idx).cloned();
                    rows.push(key_column_usage_row(
                        &schema.table,
                        &foreign_key.name,
                        col,
                        idx + 1,
                        Some(idx + 1),
                        Some((foreign_key.referenced_table.clone(), referenced)),
                    ));
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

    pub(super) fn select_information_schema_character_sets(
        &self,
        select: &Select,
    ) -> Result<QueryResult> {
        let rows = [
            ("utf8mb4", "utf8mb4_general_ci", "UTF-8 Unicode", 4),
            ("utf8mb3", "utf8mb3_general_ci", "UTF-8 Unicode", 3),
            ("latin1", "latin1_swedish_ci", "cp1252 West European", 1),
            ("ascii", "ascii_general_ci", "US ASCII", 1),
            ("binary", "binary", "Binary pseudo charset", 1),
        ]
        .iter()
        .map(|(name, default_collation, description, maxlen)| {
            let mut row = Map::new();
            row.insert(
                "character_set_name".to_string(),
                Value::String((*name).to_string()),
            );
            row.insert(
                "default_collate_name".to_string(),
                Value::String((*default_collation).to_string()),
            );
            row.insert(
                "description".to_string(),
                Value::String((*description).to_string()),
            );
            row.insert("maxlen".to_string(), Value::Number(Number::from(*maxlen)));
            row
        })
        .collect();
        virtual_select_result(select, rows)
    }

    pub(super) fn select_information_schema_collations(
        &self,
        select: &Select,
    ) -> Result<QueryResult> {
        let rows = [
            ("utf8mb4_general_ci", "utf8mb4", 45, "Yes", "Yes", 1),
            ("utf8mb4_bin", "utf8mb4", 46, "", "Yes", 1),
            ("utf8mb3_general_ci", "utf8mb3", 33, "Yes", "Yes", 1),
            ("latin1_swedish_ci", "latin1", 8, "Yes", "Yes", 1),
            ("ascii_general_ci", "ascii", 11, "Yes", "Yes", 1),
            ("binary", "binary", 63, "Yes", "Yes", 1),
        ]
        .iter()
        .map(
            |(name, charset, id, is_default, is_compiled, sortlen)| {
                let mut row = Map::new();
                row.insert(
                    "collation_name".to_string(),
                    Value::String((*name).to_string()),
                );
                row.insert(
                    "character_set_name".to_string(),
                    Value::String((*charset).to_string()),
                );
                row.insert("id".to_string(), Value::Number(Number::from(*id)));
                row.insert(
                    "is_default".to_string(),
                    Value::String((*is_default).to_string()),
                );
                row.insert(
                    "is_compiled".to_string(),
                    Value::String((*is_compiled).to_string()),
                );
                row.insert("sortlen".to_string(), Value::Number(Number::from(*sortlen)));
                row
            },
        )
        .collect();
        virtual_select_result(select, rows)
    }

    pub(super) fn select_information_schema_views(
        &self,
        select: &Select,
    ) -> Result<QueryResult> {
        virtual_select_result(select, Vec::new())
    }

    pub(super) fn select_information_schema_routines(
        &self,
        select: &Select,
    ) -> Result<QueryResult> {
        virtual_select_result(select, Vec::new())
    }

    pub(super) fn select_information_schema_engines(&self, select: &Select) -> Result<QueryResult> {
        let engines = vec![
            ("InnoDB", "YES", "Supports transactions, row-level locking, and foreign keys"),
            ("MyISAM", "NO", "MyISAM storage engine"),
            ("MEMORY", "NO", "Hash based, stored in memory"),
            ("CSV", "NO", "CSV storage engine"),
            ("ARCHIVE", "NO", "Archive storage engine"),
        ];
        let rows = engines
            .iter()
            .map(|(name, support, comment)| {
                let mut row = Map::new();
                row.insert("engine".to_string(), Value::String((*name).to_string()));
                row.insert(
                    "support".to_string(),
                    Value::String((*support).to_string()),
                );
                row.insert(
                    "comment".to_string(),
                    Value::String((*comment).to_string()),
                );
                row.insert("transactions".to_string(), Value::String("NO".to_string()));
                row.insert("xa".to_string(), Value::String("NO".to_string()));
                row.insert("savepoints".to_string(), Value::String("NO".to_string()));
                row
            })
            .collect();
        virtual_select_result(select, rows)
    }

    pub(super) fn select_information_schema_processlist(
        &self,
        select: &Select,
    ) -> Result<QueryResult> {
        let mut row = Map::new();
        row.insert("id".to_string(), Value::Number(Number::from(1)));
        row.insert("user".to_string(), Value::String("root".to_string()));
        row.insert("host".to_string(), Value::String("localhost".to_string()));
        row.insert("db".to_string(), Value::String("app".to_string()));
        row.insert("command".to_string(), Value::String("Sleep".to_string()));
        row.insert("time".to_string(), Value::Number(Number::from(0)));
        row.insert("state".to_string(), Value::String("".to_string()));
        row.insert("info".to_string(), Value::Null);
        row.insert(
            "time_ms".to_string(),
            Value::Number(Number::from(0)),
        );
        virtual_select_result(select, vec![row])
    }

    pub(super) fn select_information_schema_session_variables(
        &self,
        select: &Select,
    ) -> Result<QueryResult> {
        let variables = vec![
            "version",
            "version_comment",
            "autocommit",
            "sql_mode",
            "time_zone",
            "transaction_isolation",
            "tx_isolation",
            "character_set_client",
            "character_set_connection",
            "character_set_results",
            "collation_connection",
            "max_allowed_packet",
        ];
        let rows = variables
            .iter()
            .map(|name| {
                let value = session_variable_default(name);
                let mut row = Map::new();
                row.insert(
                    "variable_name".to_string(),
                    Value::String(name.to_uppercase()),
                );
                row.insert("variable_value".to_string(), value);
                row
            })
            .collect();
        virtual_select_result(select, rows)
    }

    pub(super) fn select_information_schema_global_variables(
        &self,
        select: &Select,
    ) -> Result<QueryResult> {
        // Global variables are same as session variables for our purposes
        self.select_information_schema_session_variables(select)
    }

    pub(super) fn select_information_schema_keywords(&self, select: &Select) -> Result<QueryResult> {
        let keywords = vec![
            "ACCESSIBLE", "ADD", "ALL", "ALTER", "ANALYZE", "AND", "AS", "ASC", "ASENSITIVE",
            "BEFORE", "BETWEEN", "BIGINT", "BINARY", "BLOB", "BOTH", "BY", "CALL", "CASCADE",
            "CASE", "CHANGE", "CHAR", "CHARACTER", "CHECK", "COLLATE", "COLUMN", "CONDITION",
            "CONSTRAINT", "CONTINUE", "CONVERT", "CREATE", "CROSS", "CURRENT_DATE",
            "CURRENT_TIME", "CURRENT_TIMESTAMP", "CURRENT_USER", "CURSOR", "DATABASE",
            "DATABASES", "DAY_HOUR", "DAY_MICROSECOND", "DAY_MINUTE", "DAY_SECOND", "DEC",
            "DECIMAL", "DECLARE", "DEFAULT", "DELAYED", "DELETE", "DESC", "DESCRIBE",
            "DETERMINISTIC", "DISTINCT", "DISTINCTROW", "DIV", "DOUBLE", "DROP", "DUAL",
            "EACH", "ELSE", "ELSEIF", "ENCLOSED", "ESCAPED", "EXISTS", "EXIT", "EXPLAIN",
            "FALSE", "FETCH", "FLOAT", "FLOAT4", "FLOAT8", "FOR", "FORCE", "FOREIGN", "FROM",
            "FULLTEXT", "GENERAL", "GET", "GRANT", "GROUP", "HAVING", "HIGH_PRIORITY",
            "HOUR_MICROSECOND", "HOUR_MINUTE", "HOUR_SECOND", "IF", "IGNORE", "IN", "INDEX",
            "INFILE", "INNER", "INOUT", "INSENSITIVE", "INSERT", "INT", "INT1", "INT2",
            "INT3", "INT4", "INT8", "INTEGER", "INTERVAL", "INTO", "IO_AFTER_GTIDS",
            "IO_BEFORE_GTIDS", "IS", "ITERATE", "JOIN", "KEY", "KEYS", "KILL", "LEADING",
            "LEAVE", "LEFT", "LIKE", "LIMIT", "LINEAR", "LINES", "LOAD", "LOCALTIME",
            "LOCALTIMESTAMP", "LOCK", "LONG", "LONGBLOB", "LONGTEXT", "LOOP", "LOW_PRIORITY",
            "MASTER_BIND", "MASTER_SSL_VERIFY_SERVER_CERT", "MATCH", "MEDIUMBLOB",
            "MEDIUMINT", "MEDIUMTEXT", "MIDDLEINT", "MINUTE_MICROSECOND", "MINUTE_SECOND",
            "MOD", "MODIFIES", "NATURAL", "NOT", "NO_WRITE_TO_BINLOG", "NULL", "NUMERIC",
            "ON", "ONE_SHOT", "OR", "ORDER", "OUT", "OUTER", "OUTFILE", "PARTITION",
            "PRECISION", "PRIMARY", "PROCEDURE", "PURGE", "RANGE", "READ", "READS",
            "READ_WRITE", "REFERENCES", "REGEXP", "RELEASE", "RENAME", "REPEAT", "REPLACE",
            "REQUIRE", "RESIGNAL", "RESTRICT", "RETURN", "REVOKE", "RIGHT", "RLIKE",
            "SCHEMA", "SCHEMAS", "SECOND_MICROSECOND", "SELECT", "SENSITIVE", "SEPARATOR",
            "SET", "SHOW", "SIGNAL", "SPATIAL", "SPECIFIC", "SQL", "SQLEXCEPTION",
            "SQLSTATE", "SQLWARNING", "SQL_BIG_RESULT", "SQL_CALC_FOUND_ROWS",
            "SQL_SMALL_RESULT", "SSL", "STARTING", "STRAIGHT_JOIN", "TABLE", "TERMINATED",
            "THEN", "TINYBLOB", "TINYINT", "TINYTEXT", "TO", "TRAILING", "TRIGGER", "TRUE",
            "UNDO", "UNION", "UNIQUE", "UNLOCK", "UNSIGNED", "UPDATE", "USAGE", "USE",
            "USING", "UTC_DATE", "UTC_TIME", "UTC_TIMESTAMP", "VALUES", "VARBINARY",
            "VARCHAR", "VARCHARACTER", "VARYING", "WHEN", "WHERE", "WHILE", "WITH", "WRITE",
            "X509", "XOR", "YEAR_MONTH", "ZEROFILL",
        ];
        let rows = keywords
            .iter()
            .map(|keyword| {
                let mut row = Map::new();
                row.insert("keyword".to_string(), Value::String((*keyword).to_string()));
                row.insert("reserved".to_string(), Value::Number(Number::from(1)));
                row
            })
            .collect();
        virtual_select_result(select, rows)
    }

    pub(super) fn select_information_schema_triggers(
        &self,
        select: &Select,
    ) -> Result<QueryResult> {
        // Triggers not supported yet
        virtual_select_result(select, Vec::new())
    }

    pub(super) fn select_information_schema_check_constraints(
        &self,
        select: &Select,
    ) -> Result<QueryResult> {
        // CHECK constraints not validated yet
        virtual_select_result(select, Vec::new())
    }

    pub(super) fn select_information_schema_files(&self, select: &Select) -> Result<QueryResult> {
        // File storage information
        virtual_select_result(select, Vec::new())
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

fn key_column_usage_row(
    table: &str,
    constraint_name: &str,
    column_name: &str,
    ordinal_position: usize,
    position_in_unique_constraint: Option<usize>,
    referenced: Option<(String, Option<String>)>,
) -> Map<String, Value> {
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
        Value::String(constraint_name.to_string()),
    );
    row.insert(
        "table_catalog".to_string(),
        Value::String("def".to_string()),
    );
    row.insert("table_schema".to_string(), Value::String("app".to_string()));
    row.insert("table_name".to_string(), Value::String(table.to_string()));
    row.insert(
        "column_name".to_string(),
        Value::String(column_name.to_string()),
    );
    row.insert(
        "ordinal_position".to_string(),
        Value::Number(Number::from(ordinal_position)),
    );
    row.insert(
        "position_in_unique_constraint".to_string(),
        position_in_unique_constraint
            .map(|pos| Value::Number(Number::from(pos)))
            .unwrap_or(Value::Null),
    );
    let (ref_schema, ref_table, ref_column) = match referenced {
        Some((table, column)) => (
            Value::String("app".to_string()),
            Value::String(table),
            column.map(Value::String).unwrap_or(Value::Null),
        ),
        None => (Value::Null, Value::Null, Value::Null),
    };
    row.insert("referenced_table_schema".to_string(), ref_schema);
    row.insert("referenced_table_name".to_string(), ref_table);
    row.insert("referenced_column_name".to_string(), ref_column);
    row
}
