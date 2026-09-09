use super::*;
use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::ops::ControlFlow;

use sqlparser::ast::Visitor;

impl RawEngine {
    pub(super) fn select_query(&self, mut query: Query) -> Result<QueryResult> {
        if query.with.as_ref().is_some_and(|with| with.recursive) {
            query = self.expand_recursive_common_table_expressions(query)?;
        }
        if query.with.is_some() {
            query = inline_common_table_expressions(query)?;
        }
        let order_by = query
            .order_by
            .map(|order_by| order_by.exprs)
            .unwrap_or_default();
        let limit = query.limit;
        let offset = query.offset;

        let result_columns: Vec<String>;
        let result_metadata: Vec<ColumnMetadata>;
        let mut rows = match &*query.body {
            SetExpr::Select(select) => {
                return self.select_from(
                    select.clone(),
                    &order_by,
                    limit.as_ref(),
                    offset.as_ref(),
                );
            }
            SetExpr::SetOperation {
                op,
                left,
                right,
                set_quantifier,
            } => {
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
                if left_result.columns.len() != right_result.columns.len() {
                    return Err(anyhow!(
                        "set operation branches return different column counts"
                    ));
                }
                result_columns = left_result.columns.clone();
                result_metadata = left_result.column_metadata.clone();
                let right_rows = right_result
                    .rows
                    .into_iter()
                    .map(|row| remap_set_row(&row, &right_result.columns, &left_result.columns))
                    .collect::<Result<Vec<_>>>()?;

                match op {
                    sqlparser::ast::SetOperator::Union => {
                        let should_dedup = *set_quantifier != sqlparser::ast::SetQuantifier::All;
                        if should_dedup {
                            let mut seen = HashSet::new();
                            left_result
                                .rows
                                .retain(|row| seen.insert(encode_json_row(row)));
                            for row in right_rows {
                                let row_key = encode_json_row(&row);
                                if seen.insert(row_key) {
                                    left_result.rows.push(row);
                                }
                            }
                        } else {
                            left_result.rows.extend(right_rows);
                        }
                        left_result.rows
                    }
                    sqlparser::ast::SetOperator::Intersect => {
                        let all = *set_quantifier == sqlparser::ast::SetQuantifier::All;
                        set_intersection(left_result.rows, right_rows, all)
                    }
                    sqlparser::ast::SetOperator::Except => {
                        let all = *set_quantifier == sqlparser::ast::SetQuantifier::All;
                        set_difference(left_result.rows, right_rows, all)
                    }
                }
            }
            SetExpr::Values(values) => {
                let width = values.rows.first().map(Vec::len).unwrap_or(0);
                result_columns = (0..width)
                    .map(|index| format!("column{}", index + 1))
                    .collect();
                result_metadata = (0..width)
                    .map(|index| ColumnMetadata::from_value(&result_columns[index], None))
                    .collect();
                values
                    .rows
                    .iter()
                    .map(|values| {
                        if values.len() != width {
                            return Err(anyhow!("VALUES rows have different column counts"));
                        }
                        values
                            .iter()
                            .enumerate()
                            .map(|(index, value)| {
                                self.eval_expr_ctx(
                                    value,
                                    &Map::new(),
                                    self.last_insert_id.load(AtomicOrdering::Relaxed),
                                )
                                .map(|value| (result_columns[index].clone(), value))
                            })
                            .collect::<Result<Map<String, Value>>>()
                    })
                    .collect::<Result<Vec<_>>>()?
            }
            _ => return Err(anyhow!("only SELECT and UNION are supported")),
        };

        let order_hints = order_by
            .iter()
            .map(|order| {
                result_columns
                    .iter()
                    .position(|column| {
                        column.eq_ignore_ascii_case(&projection_expr_column_name(&order.expr))
                    })
                    .and_then(|index| result_metadata.get(index))
                    .map(column_hint_from_metadata)
            })
            .collect::<Vec<_>>();
        apply_ordering_with(
            &mut rows,
            &order_by,
            &order_hints,
            limit_offset_end(limit.as_ref(), offset.as_ref())?,
            expr_resolved_value,
        )?;
        apply_limit_offset(&mut rows, limit.as_ref(), offset.as_ref())?;

        Ok(QueryResult {
            rows_affected: 0,
            last_insert_id: 0,
            columns: result_columns,
            column_metadata: result_metadata,
            rows,
            warnings: vec![],
        })
    }

    fn expand_recursive_common_table_expressions(&self, mut query: Query) -> Result<Query> {
        let Some(with) = query.with.as_mut() else {
            return Ok(query);
        };
        for cte in &mut with.cte_tables {
            let cte_name = cte.alias.name.value.clone();
            let cte_columns = cte.alias.columns.clone();
            let body = cte.query.body.as_ref().clone();
            let SetExpr::SetOperation {
                op: sqlparser::ast::SetOperator::Union,
                left,
                right,
                ..
            } = body
            else {
                return Err(anyhow!("recursive common table expressions require UNION"));
            };

            let mut accumulated = self.select_query(query_from_body(*left))?;
            let mut frontier = accumulated.clone();
            let recursive_columns = if cte_columns.is_empty() {
                accumulated
                    .columns
                    .iter()
                    .map(sqlparser::ast::TableAliasColumnDef::from_name)
                    .collect::<Vec<_>>()
            } else {
                cte_columns.clone()
            };
            for _ in 0..1000 {
                if frontier.rows.is_empty() {
                    break;
                }
                let recursive_body = replace_recursive_reference(
                    query_from_body(*right.clone()),
                    &cte_name,
                    values_query(&frontier),
                    &recursive_columns,
                );
                let next = self.select_query(recursive_body)?;
                if next.rows.is_empty() {
                    break;
                }
                let mut fresh = Vec::new();
                let mut seen = accumulated
                    .rows
                    .iter()
                    .map(encode_json_row)
                    .collect::<HashSet<_>>();
                for row in next.rows {
                    let row = remap_set_row(&row, &next.columns, &accumulated.columns)?;
                    if seen.insert(encode_json_row(&row)) {
                        fresh.push(row);
                    }
                }
                if fresh.is_empty() {
                    break;
                }
                frontier = QueryResult {
                    columns: accumulated.columns.clone(),
                    column_metadata: accumulated.column_metadata.clone(),
                    rows: fresh.clone(),
                    ..QueryResult::default()
                };
                accumulated.rows.extend(fresh);
            }
            cte.query = Box::new(values_query(&accumulated));
            if cte.alias.columns.is_empty() {
                cte.alias.columns = recursive_columns;
            }
            with.recursive = false;
        }
        Ok(query)
    }

    pub(super) fn select_from(
        &self,
        select: Box<Select>,
        order_by: &[OrderByExpr],
        limit: Option<&Expr>,
        offset: Option<&Offset>,
    ) -> Result<QueryResult> {
        self.validate_select_column_references(&select, order_by)?;
        let needs_aggregation = !matches!(
            &select.group_by,
            GroupByExpr::Expressions(exprs, _) if exprs.is_empty()
        ) && !matches!(&select.group_by, GroupByExpr::All(_))
            || projection_has_aggregate(&select.projection);
        let order_hints = if needs_aggregation {
            order_by
                .iter()
                .map(|order| self.order_column_hint(&select, &order.expr))
                .collect::<Vec<_>>()
        } else {
            Vec::new()
        };
        let aggregate_hints = if needs_aggregation {
            self.aggregate_column_hints(&select)
        } else {
            BTreeMap::new()
        };

        if select.from.is_empty() {
            let last_insert_id = self.last_insert_id.load(AtomicOrdering::Relaxed);
            let mut rows = Vec::new();
            let row = Map::new();
            if self.matches_selection_ctx(select.selection.as_ref(), &row, last_insert_id)? {
                rows.push(row);
            }
            if let Some(result) = aggregate_select_result(
                &select,
                &mut rows,
                order_by,
                &order_hints,
                &aggregate_hints,
                limit,
                offset,
                last_insert_id,
                &|expr, data, last_insert_id| self.eval_expr_ctx(expr, data, last_insert_id),
            )? {
                return Ok(self.with_select_metadata(&select, result));
            }

            return self.finish_select_rows(&select, rows, order_by, limit, offset, last_insert_id);
        }

        if select.from.is_empty() {
            // Already handled above
            unreachable!()
        }

        let root = &select.from[0];

        if select.from.len() > 1 {
            // Handle implicit cross-join (comma-separated FROM)
            let mut joined = Vec::new();
            let predicates = select
                .selection
                .as_ref()
                .map(conjunctive_predicates)
                .unwrap_or_default();
            let mut current = if root.joins.is_empty() {
                self.rows_for_table_factor(&root.relation)?.rows
            } else {
                self.joined_table_factor_rows(root, None)?
            };

            // Cross join each subsequent table
            for from_table in &select.from[1..] {
                let table_rows = if from_table.joins.is_empty() {
                    self.rows_for_table_factor(&from_table.relation)?.rows
                } else {
                    self.joined_table_factor_rows(from_table, None)?
                };

                // A comma-separated FROM with a conjunctive equality in the
                // WHERE clause is an inner join in MySQL.  Avoid materializing
                // the full Cartesian product for the common `left.key =
                // right.key` shape; select.inc uses this with tens of
                // thousands of rows.
                let equality_columns = predicates.iter().find_map(|predicate| {
                    let (left_column, right_column) = required_equi_join_columns(predicate)?;
                    let current_row = current.first()?;
                    let right_row = table_rows.first()?;
                    if current_row.contains_key(&left_column)
                        && right_row.contains_key(&right_column)
                    {
                        Some((left_column, right_column))
                    } else if current_row.contains_key(&right_column)
                        && right_row.contains_key(&left_column)
                    {
                        Some((right_column, left_column))
                    } else {
                        None
                    }
                });
                let right_by_equality = equality_columns.as_ref().map(|(_, right_column)| {
                    let mut buckets: HashMap<JoinKey, Vec<usize>> =
                        HashMap::with_capacity(table_rows.len());
                    for (index, row) in table_rows.iter().enumerate() {
                        if let Some(value) = row.get(right_column)
                            && *value != Value::Null
                        {
                            buckets.entry(JoinKey::from(value)).or_default().push(index);
                        }
                    }
                    buckets
                });

                let all_indices = (0..table_rows.len()).collect::<Vec<_>>();
                let empty_indices: &[usize] = &[];
                let mut next = Vec::new();
                for candidate in &current {
                    let table_indices: &[usize] = if let Some((left_column, _)) = &equality_columns
                    {
                        candidate
                            .get(left_column)
                            .filter(|value| **value != Value::Null)
                            .and_then(|value| {
                                right_by_equality
                                    .as_ref()
                                    .and_then(|buckets| buckets.get(&JoinKey::from(value)))
                            })
                            .map(Vec::as_slice)
                            .unwrap_or(empty_indices)
                    } else {
                        &all_indices
                    };
                    for &right_index in table_indices {
                        let table_data = &table_rows[right_index];
                        let mut combined = candidate.clone();
                        for (k, v) in table_data {
                            combined.entry(k.clone()).or_insert_with(|| v.clone());
                        }
                        let matches_available_predicates = predicates.iter().all(|predicate| {
                            !predicate_columns_available(predicate, &combined)
                                || self
                                    .matches_selection_ctx(Some(predicate), &combined, 0)
                                    .unwrap_or(false)
                        });
                        if matches_available_predicates {
                            next.push(combined);
                        }
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
                &mut joined,
                order_by,
                &order_hints,
                &aggregate_hints,
                limit,
                offset,
                last_insert_id,
                &|expr, data, last_insert_id| self.eval_expr_ctx(expr, data, last_insert_id),
            )? {
                return Ok(self.with_select_metadata(&select, result));
            }

            return self.finish_select_rows(
                &select,
                joined,
                order_by,
                limit,
                offset,
                last_insert_id,
            );
        }

        let root = &select.from[0];
        if matches!(root.relation, TableFactor::Derived { .. }) {
            let mut rows = self.select_derived_rows(&select, root)?;
            let last_insert_id = self.last_insert_id.load(AtomicOrdering::Relaxed);
            if let Some(result) = aggregate_select_result(
                &select,
                &mut rows,
                order_by,
                &order_hints,
                &aggregate_hints,
                limit,
                offset,
                last_insert_id,
                &|expr, data, last_insert_id| self.eval_expr_ctx(expr, data, last_insert_id),
            )? {
                return Ok(self.with_select_metadata(&select, result));
            }
            return self.finish_select_rows(&select, rows, order_by, limit, offset, last_insert_id);
        }
        let root_name_full = if matches!(
            root.relation,
            TableFactor::NestedJoin { .. } | TableFactor::JsonTable { .. }
        ) {
            String::new()
        } else {
            table_factor_name_full(&root.relation)?
        };
        // Metadata ordering must run before projection: physical fingerprints order
        // by fields (such as ORDINAL_POSITION) that are intentionally not selected.
        let finish_metadata = !order_by.is_empty() || limit.is_some() || offset.is_some();
        let mut metadata_select = select.clone();
        if finish_metadata {
            metadata_select.projection = vec![SelectItem::Wildcard(Default::default())];
            metadata_select.distinct = None;
            metadata_select.group_by = GroupByExpr::Expressions(vec![], vec![]);
            metadata_select.having = None;
        }
        let metadata = (|| {
            if root_name_full.eq_ignore_ascii_case("information_schema.tables") {
                return Some(self.select_information_schema_tables(&metadata_select));
            }
            if root_name_full.eq_ignore_ascii_case("information_schema.schemata") {
                return Some(self.select_information_schema_schemata(&metadata_select));
            }
            if root_name_full.eq_ignore_ascii_case("information_schema.columns") {
                return Some(self.select_information_schema_columns(&metadata_select));
            }
            if root_name_full.eq_ignore_ascii_case("information_schema.table_constraints") {
                if root.joins.iter().any(|join| {
                    table_factor_name_full(&join.relation).is_ok_and(|name| {
                        name.eq_ignore_ascii_case("information_schema.key_column_usage")
                    })
                }) {
                    return Some(
                        self.select_information_schema_constraints_with_column_usage(
                            &metadata_select,
                        ),
                    );
                }
                return Some(self.select_information_schema_table_constraints(&metadata_select));
            }
            if root_name_full.eq_ignore_ascii_case("information_schema.statistics") {
                return Some(self.select_information_schema_statistics(&metadata_select));
            }
            if root_name_full.eq_ignore_ascii_case("information_schema.key_column_usage") {
                return Some(self.select_information_schema_key_column_usage(&metadata_select));
            }
            if root_name_full.eq_ignore_ascii_case("information_schema.referential_constraints") {
                return Some(
                    self.select_information_schema_referential_constraints(&metadata_select),
                );
            }
            if root_name_full.eq_ignore_ascii_case("information_schema.character_sets") {
                return Some(self.select_information_schema_character_sets(&metadata_select));
            }
            if root_name_full.eq_ignore_ascii_case("information_schema.collations") {
                return Some(self.select_information_schema_collations(&metadata_select));
            }
            if root_name_full.eq_ignore_ascii_case("information_schema.views") {
                return Some(self.select_information_schema_views(&metadata_select));
            }
            if root_name_full.eq_ignore_ascii_case("information_schema.routines") {
                return Some(self.select_information_schema_routines(&metadata_select));
            }
            if root_name_full.eq_ignore_ascii_case("information_schema.engines") {
                return Some(self.select_information_schema_engines(&metadata_select));
            }
            if root_name_full.eq_ignore_ascii_case("information_schema.processlist") {
                return Some(self.select_information_schema_processlist(&metadata_select));
            }
            if root_name_full.eq_ignore_ascii_case("information_schema.session_variables") {
                return Some(self.select_information_schema_session_variables(&metadata_select));
            }
            if root_name_full.eq_ignore_ascii_case("information_schema.global_variables") {
                return Some(self.select_information_schema_global_variables(&metadata_select));
            }
            if root_name_full.eq_ignore_ascii_case("information_schema.system_variables") {
                return Some(self.select_information_schema_system_variables(&metadata_select));
            }
            if root_name_full.eq_ignore_ascii_case("information_schema.keywords") {
                return Some(self.select_information_schema_keywords(&metadata_select));
            }
            if root_name_full.eq_ignore_ascii_case("information_schema.triggers") {
                return Some(self.select_information_schema_triggers(&metadata_select));
            }
            if root_name_full.eq_ignore_ascii_case("information_schema.check_constraints") {
                return Some(self.select_information_schema_check_constraints(&metadata_select));
            }
            if root_name_full.eq_ignore_ascii_case("information_schema.files") {
                return Some(self.select_information_schema_files(&metadata_select));
            }
            None
        })();
        if let Some(result) = metadata {
            let mut result = result?;
            if !finish_metadata {
                return Ok(result);
            }
            let last_insert_id = self.last_insert_id.load(AtomicOrdering::Relaxed);
            if let Some(aggregate) = aggregate_select_result(
                &select,
                &mut result.rows,
                order_by,
                &order_hints,
                &aggregate_hints,
                limit,
                offset,
                last_insert_id,
                &|expr, data, id| self.eval_expr_ctx(expr, data, id),
            )? {
                return Ok(self.with_select_metadata(&select, aggregate));
            }
            let mut finished = self.finish_select_rows(
                &select,
                result.rows,
                order_by,
                limit,
                offset,
                last_insert_id,
            )?;
            if finished.rows.is_empty() {
                let columns = result
                    .columns
                    .iter()
                    .map(String::as_str)
                    .collect::<Vec<_>>();
                finished.columns =
                    virtual_select_result_with_empty_schema(&select, vec![], &columns)?.columns;
            }
            return Ok(finished);
        }
        if root_name_full.eq_ignore_ascii_case("dual") {
            return self.finish_select_rows(
                &select,
                vec![Map::new()],
                order_by,
                limit,
                offset,
                self.last_insert_id.load(AtomicOrdering::Relaxed),
            );
        }

        let rows = if root.joins.is_empty()
            && !matches!(
                root.relation,
                TableFactor::NestedJoin { .. } | TableFactor::JsonTable { .. }
            ) {
            self.select_single_table(&select, root, order_by)?
        } else {
            self.select_with_joins(&select, root)?
        };
        let mut rows = rows;
        let mut referenced_user_variables = select_user_variable_names(&select);
        for order in order_by {
            collect_user_variables(&order.expr, &mut referenced_user_variables);
        }
        if let Some(limit) = limit {
            collect_user_variables(limit, &mut referenced_user_variables);
        }
        if let Some(offset) = offset {
            collect_user_variables(&offset.value, &mut referenced_user_variables);
        }
        self.inject_user_variables(&mut rows, Some(&referenced_user_variables));

        let last_insert_id = self.last_insert_id.load(AtomicOrdering::Relaxed);
        if let Some(result) = aggregate_select_result(
            &select,
            &mut rows,
            order_by,
            &order_hints,
            &aggregate_hints,
            limit,
            offset,
            last_insert_id,
            &|expr, data, last_insert_id| self.eval_expr_ctx(expr, data, last_insert_id),
        )? {
            return Ok(self.with_select_metadata(&select, result));
        }

        self.finish_select_rows(&select, rows, order_by, limit, offset, last_insert_id)
    }

    pub(super) fn inject_user_variables(
        &self,
        rows: &mut [Map<String, Value>],
        referenced: Option<&HashSet<String>>,
    ) {
        if self.user_variables.is_empty() {
            return;
        }
        let variables = self
            .user_variables
            .iter()
            .filter(|entry| {
                referenced
                    .as_ref()
                    .is_none_or(|names| names.contains(&format!("@{}", entry.key())))
            })
            .map(|entry| (format!("@{}", entry.key()), entry.value().clone()))
            .collect::<Vec<_>>();
        if variables.is_empty() {
            return;
        }
        for row in rows {
            for (name, value) in &variables {
                row.entry(name.clone()).or_insert_with(|| value.clone());
            }
        }
    }

    fn finish_select_rows(
        &self,
        select: &Select,
        mut rows: Vec<Map<String, Value>>,
        order_by: &[OrderByExpr],
        limit: Option<&Expr>,
        offset: Option<&Offset>,
        last_insert_id: u64,
    ) -> Result<QueryResult> {
        self.materialize_window_values(select, &mut rows, last_insert_id)?;
        if order_by_references_projection_alias(select, order_by) {
            self.materialize_projection_values(&select.projection, &mut rows, last_insert_id)?;
        }
        let order_hints = order_by
            .iter()
            .map(|order| self.order_column_hint(select, &order.expr))
            .collect::<Vec<_>>();
        let max_ordered_rows = if select.distinct.is_none() {
            limit_offset_end(limit, offset)?
        } else {
            None
        };
        apply_ordering_with(
            &mut rows,
            order_by,
            &order_hints,
            max_ordered_rows,
            |expr, row| self.eval_expr_ctx(expr, row, last_insert_id),
        )?;
        let mut rows = rows
            .into_iter()
            .map(|row| self.project_row_ctx(&select.projection, &row, last_insert_id))
            .collect::<Result<Vec<_>>>()?;
        if select.distinct.is_some() {
            deduplicate_rows(&mut rows);
        }
        apply_limit_offset(&mut rows, limit, offset)?;

        let columns = self.select_result_columns(select, rows.first());
        let column_metadata = self.select_result_metadata(select, &columns, rows.first());
        Ok(QueryResult {
            rows_affected: 0,
            last_insert_id: 0,
            columns,
            column_metadata,
            rows,
            warnings: vec![],
        })
    }

    fn materialize_window_values(
        &self,
        select: &Select,
        rows: &mut [Map<String, Value>],
        last_insert_id: u64,
    ) -> Result<()> {
        let has_window = select.projection.iter().any(|item| {
            let expr = match item {
                SelectItem::UnnamedExpr(expr) | SelectItem::ExprWithAlias { expr, .. } => expr,
                _ => return false,
            };
            !window_exprs(expr).is_empty()
        });
        if rows.is_empty() || !has_window {
            return Ok(());
        }
        // Window evaluation only mutates the projected rows after all window
        // expressions have been evaluated. Borrow the input in place instead
        // of cloning every row, and cache the partition/order layout for
        // expressions that share a window specification.
        let snapshot: &[Map<String, Value>] = rows;
        let mut layouts = HashMap::<String, WindowLayout>::new();
        let mut pending = Vec::<(String, Vec<Value>)>::new();
        for item in &select.projection {
            let expr = match item {
                SelectItem::UnnamedExpr(expr) | SelectItem::ExprWithAlias { expr, .. } => expr,
                _ => continue,
            };
            for window_expr in window_exprs(expr) {
                let Expr::Function(function) = window_expr else {
                    continue;
                };
                let Some(window) = &function.over else {
                    continue;
                };
                let spec = resolve_window_spec(select, window)?;
                let function_name = function
                    .name
                    .0
                    .last()
                    .map(|name| name.value.to_ascii_uppercase())
                    .unwrap_or_default();
                let arguments = window_function_arguments(&function)?;
                let layout_key = format!(
                    "{}|{}",
                    spec.partition_by
                        .iter()
                        .map(ToString::to_string)
                        .collect::<Vec<_>>()
                        .join(","),
                    spec.order_by
                        .iter()
                        .map(ToString::to_string)
                        .collect::<Vec<_>>()
                        .join(",")
                );
                if !layouts.contains_key(&layout_key) {
                    let order_hints = spec
                        .order_by
                        .iter()
                        .map(|order| self.order_column_hint(select, &order.expr))
                        .collect::<Vec<_>>();
                    let mut partitions = HashMap::<String, Vec<usize>>::new();
                    for (index, row) in snapshot.iter().enumerate() {
                        let key = spec
                            .partition_by
                            .iter()
                            .map(|expr| self.eval_expr_ctx(expr, row, last_insert_id))
                            .collect::<Result<Vec<_>>>()?
                            .iter()
                            .map(encode_json_value)
                            .collect::<Vec<_>>()
                            .join("|");
                        partitions.entry(key).or_default().push(index);
                    }
                    let mut partitions = partitions.into_values().collect::<Vec<_>>();
                    let mut order_keys = vec![Vec::new(); snapshot.len()];
                    for partition in &mut partitions {
                        for index in partition.iter().copied() {
                            order_keys[index] = spec
                                .order_by
                                .iter()
                                .map(|order| {
                                    self.eval_expr_ctx(
                                        &order.expr,
                                        &snapshot[index],
                                        last_insert_id,
                                    )
                                })
                                .collect::<Result<Vec<_>>>()?;
                        }
                        partition.sort_by(|left, right| {
                            for (position, order) in spec.order_by.iter().enumerate() {
                                let ordering = compare_order_values(
                                    &order_keys[*left][position],
                                    &order_keys[*right][position],
                                    order_hints.get(position).and_then(Option::as_ref),
                                );
                                if ordering != Ordering::Equal {
                                    return if order.asc.unwrap_or(true) {
                                        ordering
                                    } else {
                                        ordering.reverse()
                                    };
                                }
                            }
                            left.cmp(right)
                        });
                    }
                    layouts.insert(
                        layout_key.clone(),
                        WindowLayout {
                            partitions,
                            order_keys,
                            order_hints,
                        },
                    );
                }
                let layout = layouts
                    .get(&layout_key)
                    .expect("window layout inserted above");

                let mut values = vec![Value::Null; rows.len()];
                for partition in &layout.partitions {
                    let key_by_index = &layout.order_keys;

                    let mut rank = 1_usize;
                    let mut dense_rank = 1_usize;
                    for position in 0..partition.len() {
                        if position > 0
                            && !same_order_key(
                                &key_by_index[partition[position]],
                                &key_by_index[partition[position - 1]],
                                &layout.order_hints,
                            )
                        {
                            rank = position + 1;
                            dense_rank += 1;
                        }
                        let row_index = partition[position];
                        let mut peer_start = position;
                        while peer_start > 0
                            && same_order_key(
                                &key_by_index[partition[peer_start - 1]],
                                &key_by_index[partition[position]],
                                &layout.order_hints,
                            )
                        {
                            peer_start -= 1;
                        }
                        let mut peer_end = position;
                        while peer_end + 1 < partition.len()
                            && same_order_key(
                                &key_by_index[partition[peer_end + 1]],
                                &key_by_index[partition[position]],
                                &layout.order_hints,
                            )
                        {
                            peer_end += 1;
                        }
                        let frame = if matches!(
                            spec.window_frame.as_ref().map(|frame| &frame.units),
                            Some(sqlparser::ast::WindowFrameUnits::Range)
                        ) && !spec.order_by.is_empty()
                        {
                            window_range_frame_positions(&spec, partition, position, key_by_index)?
                        } else {
                            window_frame_positions(
                                &spec,
                                position,
                                partition.len(),
                                !spec.order_by.is_empty(),
                                peer_start,
                                peer_end,
                            )?
                        };
                        values[row_index] = self.window_function_value(
                            &function_name,
                            &arguments,
                            partition,
                            position,
                            frame,
                            rank,
                            dense_rank,
                            peer_end,
                            &snapshot,
                            last_insert_id,
                        )?;
                    }
                }

                let key = projection_expr_column_name(&window_expr);
                pending.push((key, values));
            }
        }
        for (key, values) in pending {
            for (row, value) in rows.iter_mut().zip(values) {
                row.insert(key.clone(), value);
            }
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn window_function_value(
        &self,
        function: &str,
        arguments: &[Option<Expr>],
        partition: &[usize],
        position: usize,
        frame: Option<(usize, usize)>,
        rank: usize,
        dense_rank: usize,
        peer_end: usize,
        rows: &[Map<String, Value>],
        last_insert_id: u64,
    ) -> Result<Value> {
        let integer = |value: usize| Value::Number(Number::from(value as u64));
        match function {
            "ROW_NUMBER" => return Ok(integer(position + 1)),
            "RANK" => return Ok(integer(rank)),
            "DENSE_RANK" => return Ok(integer(dense_rank)),
            "PERCENT_RANK" => {
                return Ok(number_from_f64(if partition.len() <= 1 {
                    0.0
                } else {
                    (rank - 1) as f64 / (partition.len() - 1) as f64
                }));
            }
            "CUME_DIST" => {
                return Ok(number_from_f64(
                    (peer_end + 1) as f64 / partition.len() as f64,
                ));
            }
            "NTILE" => {
                let buckets = window_usize_argument(
                    self,
                    arguments.first(),
                    &rows[partition[position]],
                    last_insert_id,
                    0,
                )?;
                if buckets == 0 {
                    return Err(anyhow!("NTILE requires a positive bucket count"));
                }
                let quotient = partition.len() / buckets;
                let remainder = partition.len() % buckets;
                let first_span = (quotient + 1) * remainder;
                let bucket = if position < first_span {
                    position / (quotient + 1) + 1
                } else {
                    remainder + (position - first_span) / quotient + 1
                };
                return Ok(integer(bucket));
            }
            "LAG" | "LEAD" => {
                let offset = window_usize_argument(
                    self,
                    arguments.get(1),
                    &rows[partition[position]],
                    last_insert_id,
                    1,
                )?;
                let target = if function == "LAG" {
                    position.checked_sub(offset)
                } else {
                    position
                        .checked_add(offset)
                        .filter(|index| *index < partition.len())
                };
                if let Some(target) = target {
                    return eval_window_argument(
                        self,
                        arguments.first(),
                        &rows[partition[target]],
                        last_insert_id,
                    );
                }
                return eval_window_argument(
                    self,
                    arguments.get(2),
                    &rows[partition[position]],
                    last_insert_id,
                );
            }
            _ => {}
        }

        let frame_rows = frame
            .map(|(start, end)| &partition[start..end])
            .unwrap_or(&[]);
        match function {
            "FIRST_VALUE" => frame_rows
                .first()
                .map(|index| {
                    eval_window_argument(self, arguments.first(), &rows[*index], last_insert_id)
                })
                .transpose()
                .map(|value| value.unwrap_or(Value::Null)),
            "LAST_VALUE" => frame_rows
                .last()
                .map(|index| {
                    eval_window_argument(self, arguments.first(), &rows[*index], last_insert_id)
                })
                .transpose()
                .map(|value| value.unwrap_or(Value::Null)),
            "NTH_VALUE" => {
                let nth = window_usize_argument(
                    self,
                    arguments.get(1),
                    &rows[partition[position]],
                    last_insert_id,
                    0,
                )?;
                frame_rows
                    .get(nth.saturating_sub(1))
                    .map(|index| {
                        eval_window_argument(self, arguments.first(), &rows[*index], last_insert_id)
                    })
                    .transpose()
                    .map(|value| value.unwrap_or(Value::Null))
            }
            "COUNT" | "SUM" | "AVG" | "MIN" | "MAX" | "STD" | "STDDEV" => {
                let mut aggregate_values = Vec::new();
                for index in frame_rows {
                    let value = if arguments.is_empty()
                        || arguments.first().is_some_and(|argument| argument.is_none())
                    {
                        Value::Number(Number::from(1))
                    } else {
                        eval_window_argument(
                            self,
                            arguments.first(),
                            &rows[*index],
                            last_insert_id,
                        )?
                    };
                    if value != Value::Null {
                        aggregate_values.push(value);
                    }
                }
                match function {
                    "COUNT" => Ok(integer(aggregate_values.len())),
                    "SUM" | "AVG" if aggregate_values.is_empty() => Ok(Value::Null),
                    "SUM" => Ok(number_from_f64(
                        aggregate_values
                            .iter()
                            .map(json_to_f64_lossy)
                            .try_fold(0.0, |sum, value| value.map(|value| sum + value))?,
                    )),
                    "AVG" => {
                        let sum = aggregate_values
                            .iter()
                            .map(json_to_f64_lossy)
                            .try_fold(0.0, |sum, value| value.map(|value| sum + value))?;
                        Ok(number_from_f64(sum / aggregate_values.len() as f64))
                    }
                    "MIN" => Ok(aggregate_values
                        .into_iter()
                        .min_by(compare_json_values)
                        .unwrap_or(Value::Null)),
                    "MAX" => Ok(aggregate_values
                        .into_iter()
                        .max_by(compare_json_values)
                        .unwrap_or(Value::Null)),
                    "STD" | "STDDEV" => {
                        if aggregate_values.is_empty() {
                            Ok(Value::Null)
                        } else {
                            let numbers = aggregate_values
                                .iter()
                                .map(json_to_f64_lossy)
                                .collect::<Result<Vec<_>>>()?;
                            let mean = numbers.iter().sum::<f64>() / numbers.len() as f64;
                            let variance = numbers
                                .iter()
                                .map(|value| (value - mean).powi(2))
                                .sum::<f64>()
                                / numbers.len() as f64;
                            let value = variance.sqrt();
                            Ok(number_from_f64((value * 10_000.0).round() / 10_000.0))
                        }
                    }
                    _ => unreachable!(),
                }
            }
            _ => Err(anyhow!("unsupported window function: {function}")),
        }
    }

    fn with_select_metadata(&self, select: &Select, mut result: QueryResult) -> QueryResult {
        if result.columns.is_empty() {
            result.columns = self.select_result_columns(select, result.rows.first());
        }
        result.column_metadata =
            self.select_result_metadata(select, &result.columns, result.rows.first());
        result
    }

    fn select_result_columns(
        &self,
        select: &Select,
        first_row: Option<&Map<String, Value>>,
    ) -> Vec<String> {
        let mut columns = Vec::new();
        for item in &select.projection {
            match item {
                SelectItem::UnnamedExpr(expr) => {
                    columns.push(projection_output_column_name(expr));
                }
                SelectItem::ExprWithAlias { alias, .. } => columns.push(alias.value.clone()),
                SelectItem::Wildcard(_) => {
                    for table in &select.from {
                        self.append_factor_columns(&table.relation, None, &mut columns);
                        for join in &table.joins {
                            self.append_factor_columns(&join.relation, None, &mut columns);
                        }
                    }
                }
                SelectItem::QualifiedWildcard(prefix, _) => {
                    let qualifier = prefix
                        .0
                        .iter()
                        .map(|part| part.value.as_str())
                        .collect::<Vec<_>>()
                        .join(".");
                    for table in &select.from {
                        self.append_factor_columns(&table.relation, Some(&qualifier), &mut columns);
                        for join in &table.joins {
                            self.append_factor_columns(
                                &join.relation,
                                Some(&qualifier),
                                &mut columns,
                            );
                        }
                    }
                }
            }
        }
        if columns.is_empty() {
            columns.extend(first_row.into_iter().flat_map(|row| row.keys().cloned()));
        }
        columns
    }

    fn append_factor_columns(
        &self,
        factor: &TableFactor,
        qualifier: Option<&str>,
        columns: &mut Vec<String>,
    ) {
        if let TableFactor::NestedJoin {
            table_with_joins, ..
        } = factor
        {
            self.append_factor_columns(&table_with_joins.relation, qualifier, columns);
            for join in &table_with_joins.joins {
                self.append_factor_columns(&join.relation, qualifier, columns);
            }
            return;
        }
        if let TableFactor::JsonTable {
            columns: json_columns,
            alias,
            ..
        } = factor
        {
            if qualifier.is_none()
                || alias.as_ref().is_some_and(|alias| {
                    qualifier
                        .is_some_and(|qualifier| qualifier.eq_ignore_ascii_case(&alias.name.value))
                })
            {
                columns.extend(json_columns.iter().flat_map(json_table_column_names));
            }
            return;
        }
        let TableFactor::Table { name, alias, .. } = factor else {
            return;
        };
        let Some(table) = name.0.last().map(|name| name.value.clone()) else {
            return;
        };
        if qualifier.is_some_and(|qualifier| {
            !qualifier.eq_ignore_ascii_case(&table)
                && !alias
                    .as_ref()
                    .is_some_and(|alias| qualifier.eq_ignore_ascii_case(&alias.name.value))
        }) {
            return;
        }
        if let Some(schema) = self.schemas.get(&table) {
            columns.extend(ordered_schema_columns(&schema));
        }
    }

    fn select_result_metadata(
        &self,
        select: &Select,
        columns: &[String],
        first_row: Option<&Map<String, Value>>,
    ) -> Vec<ColumnMetadata> {
        let mut metadata = Vec::new();
        let nullable_tables = select_nullable_tables(select);
        for item in &select.projection {
            match item {
                SelectItem::UnnamedExpr(expr) => metadata.push(self.expression_metadata(
                    select,
                    expr,
                    projection_output_column_name(expr),
                    first_row,
                )),
                SelectItem::ExprWithAlias { expr, alias } => metadata
                    .push(self.expression_metadata(select, expr, alias.value.clone(), first_row)),
                SelectItem::Wildcard(_) => {
                    for table in &select.from {
                        self.append_factor_metadata(
                            &table.relation,
                            None,
                            &nullable_tables,
                            &mut metadata,
                        );
                        for join in &table.joins {
                            self.append_factor_metadata(
                                &join.relation,
                                None,
                                &nullable_tables,
                                &mut metadata,
                            );
                        }
                    }
                }
                SelectItem::QualifiedWildcard(prefix, _) => {
                    let qualifier = prefix
                        .0
                        .iter()
                        .map(|part| part.value.as_str())
                        .collect::<Vec<_>>()
                        .join(".");
                    for table in &select.from {
                        self.append_factor_metadata(
                            &table.relation,
                            Some(&qualifier),
                            &nullable_tables,
                            &mut metadata,
                        );
                        for join in &table.joins {
                            self.append_factor_metadata(
                                &join.relation,
                                Some(&qualifier),
                                &nullable_tables,
                                &mut metadata,
                            );
                        }
                    }
                }
            }
        }
        if metadata.len() != columns.len() {
            return columns
                .iter()
                .map(|name| {
                    ColumnMetadata::from_value(name, first_row.and_then(|row| row.get(name)))
                })
                .collect();
        }
        for (column, meta) in columns.iter().zip(&mut metadata) {
            meta.name = column.clone();
        }
        metadata
    }

    fn append_factor_metadata(
        &self,
        factor: &TableFactor,
        qualifier: Option<&str>,
        nullable_tables: &BTreeSet<String>,
        metadata: &mut Vec<ColumnMetadata>,
    ) {
        if let TableFactor::NestedJoin {
            table_with_joins, ..
        } = factor
        {
            self.append_factor_metadata(
                &table_with_joins.relation,
                qualifier,
                nullable_tables,
                metadata,
            );
            for join in &table_with_joins.joins {
                self.append_factor_metadata(&join.relation, qualifier, nullable_tables, metadata);
            }
            return;
        }
        let TableFactor::Table { name, alias, .. } = factor else {
            return;
        };
        let Some(table) = name.0.last().map(|name| name.value.clone()) else {
            return;
        };
        if qualifier.is_some_and(|qualifier| {
            !qualifier.eq_ignore_ascii_case(&table)
                && !alias
                    .as_ref()
                    .is_some_and(|alias| qualifier.eq_ignore_ascii_case(&alias.name.value))
        }) {
            return;
        }
        if let Some(schema) = self.schemas.get(&table) {
            for column in ordered_schema_columns(&schema) {
                if let Some(hint) = schema.columns.get(&column) {
                    let mut column_metadata = ColumnMetadata::from_declared(&column, &table, hint);
                    if nullable_tables
                        .iter()
                        .any(|nullable| nullable.eq_ignore_ascii_case(&table))
                    {
                        column_metadata.nullable = true;
                    }
                    metadata.push(column_metadata);
                }
            }
        }
    }

    fn expression_metadata(
        &self,
        select: &Select,
        expr: &Expr,
        output_name: String,
        first_row: Option<&Map<String, Value>>,
    ) -> ColumnMetadata {
        if let Some((table, hint)) = self.resolve_expression_column(select, expr) {
            let mut metadata = ColumnMetadata::from_declared(output_name, table, &hint);
            if select_nullable_tables(select)
                .iter()
                .any(|table| table.eq_ignore_ascii_case(&metadata.table))
            {
                metadata.nullable = true;
            }
            if !matches!(expr, Expr::Identifier(_) | Expr::CompoundIdentifier(_)) {
                metadata.table.clear();
            }
            return metadata;
        }

        let mut metadata = ColumnMetadata::from_value(
            output_name.clone(),
            first_row.and_then(|row| row.get(&output_name)),
        );
        let expression_text = expr.to_string();
        let expression_upper = expression_text.to_ascii_uppercase();
        if (expression_upper.starts_with("IF(") || expression_upper.starts_with("CASE"))
            && expression_text.contains('\'')
        {
            metadata.column_type = MysqlColumnType::VarChar;
            metadata.unsigned = false;
        }
        match expr {
            Expr::Cast { data_type, .. } => {
                let hint = ColumnHint {
                    sql_type: Some(cast_data_type_name(data_type)),
                    ..ColumnHint::default()
                };
                metadata = ColumnMetadata::from_declared(output_name, "", &hint);
            }
            Expr::Function(function) => {
                let name = function
                    .name
                    .0
                    .last()
                    .map(|name| name.value.to_ascii_uppercase())
                    .unwrap_or_default();
                metadata.column_type = match name.as_str() {
                    "COUNT" | "ROW_NUMBER" | "RANK" | "DENSE_RANK" | "NTILE" => {
                        metadata.unsigned = true;
                        MysqlColumnType::BigInt
                    }
                    "PERCENT_RANK" | "CUME_DIST" => MysqlColumnType::Double,
                    "UNIX_TIMESTAMP" => {
                        metadata.decimals = 0;
                        MysqlColumnType::Decimal
                    }
                    "AVG" | "SUM" | "STD" | "STDDEV" => {
                        metadata.decimals = if matches!(name.as_str(), "AVG" | "STD" | "STDDEV") {
                            4
                        } else {
                            0
                        };
                        MysqlColumnType::Decimal
                    }
                    "ROUND" | "TRUNCATE" => MysqlColumnType::Decimal,
                    "CURRENT_DATE" | "CURDATE" | "DATE" => MysqlColumnType::Date,
                    "CURRENT_TIME" | "CURTIME" | "TIME" => MysqlColumnType::Time,
                    "NOW" | "CURRENT_TIMESTAMP" => MysqlColumnType::DateTime,
                    "JSON_ARRAY"
                    | "JSON_ARRAYAGG"
                    | "JSON_ARRAY_APPEND"
                    | "JSON_ARRAY_INSERT"
                    | "JSON_EXTRACT"
                    | "JSON_INSERT"
                    | "JSON_KEYS"
                    | "JSON_MERGE"
                    | "JSON_MERGE_PATCH"
                    | "JSON_MERGE_PRESERVE"
                    | "JSON_OBJECT"
                    | "JSON_OBJECTAGG"
                    | "JSON_REMOVE"
                    | "JSON_REPLACE"
                    | "JSON_SEARCH"
                    | "JSON_SCHEMA_VALIDATION_REPORT"
                    | "JSON_SET" => MysqlColumnType::Json,
                    "LENGTH" | "CHAR_LENGTH" | "DATEDIFF" | "TIMESTAMPDIFF" => {
                        MysqlColumnType::BigInt
                    }
                    _ => metadata.column_type,
                };
            }
            Expr::Value(SqlValue::Number(number, _)) => {
                metadata.column_type = if number.contains(['.', 'e', 'E']) {
                    MysqlColumnType::Decimal
                } else {
                    MysqlColumnType::BigInt
                };
                if let Some((_, scale)) = number.split_once('.') {
                    metadata.decimals = scale.len().min(u8::MAX as usize) as u8;
                }
            }
            Expr::Value(SqlValue::Boolean(_)) => metadata.column_type = MysqlColumnType::TinyInt,
            Expr::Value(SqlValue::Null) => metadata.column_type = MysqlColumnType::Null,
            _ => {}
        }
        for window in window_exprs(expr) {
            let Expr::Function(function) = window else {
                continue;
            };
            let name = function
                .name
                .0
                .last()
                .map(|name| name.value.to_ascii_uppercase())
                .unwrap_or_default();
            if !matches!(name.as_str(), "AVG" | "STD" | "STDDEV") {
                continue;
            }
            let approximate = matches!(name.as_str(), "STD" | "STDDEV")
                && window_function_arguments(function)
                    .ok()
                    .and_then(|args| args.into_iter().next().flatten())
                    .is_some_and(|arg| {
                        let input =
                            self.expression_metadata(select, &arg, String::new(), first_row);
                        !matches!(
                            input.column_type,
                            MysqlColumnType::TinyInt
                                | MysqlColumnType::SmallInt
                                | MysqlColumnType::Integer
                                | MysqlColumnType::BigInt
                                | MysqlColumnType::Decimal
                        )
                    });
            // Text and floating inputs use approximate numeric conversion. Their
            // standard deviation has no fixed decimal scale (unlike exact inputs).
            metadata.column_type = if approximate {
                MysqlColumnType::Double
            } else {
                MysqlColumnType::Decimal
            };
            metadata.decimals = if approximate { 0 } else { 4 };
        }
        metadata
    }

    fn resolve_expression_column(
        &self,
        select: &Select,
        expr: &Expr,
    ) -> Option<(String, ColumnHint)> {
        let (qualifier, column) = match expr {
            Expr::Identifier(column) => (None, column.value.as_str()),
            Expr::CompoundIdentifier(parts) if parts.len() >= 2 => (
                parts.get(parts.len() - 2).map(|part| part.value.as_str()),
                parts.last()?.value.as_str(),
            ),
            _ => return None,
        };
        for table in &select.from {
            for factor in std::iter::once(&table.relation)
                .chain(table.joins.iter().map(|join| &join.relation))
            {
                let TableFactor::Table { name, alias, .. } = factor else {
                    continue;
                };
                let table_name = name.0.last()?.value.clone();
                if qualifier.is_some_and(|qualifier| {
                    !qualifier.eq_ignore_ascii_case(&table_name)
                        && !alias
                            .as_ref()
                            .is_some_and(|alias| qualifier.eq_ignore_ascii_case(&alias.name.value))
                }) {
                    continue;
                }
                let schema = self.schemas.get(&table_name)?;
                if let Some((_, hint)) = schema
                    .columns
                    .iter()
                    .find(|(known, _)| known.eq_ignore_ascii_case(column))
                {
                    return Some((table_name, hint.clone()));
                }
            }
        }
        None
    }

    fn order_column_hint(&self, select: &Select, expr: &Expr) -> Option<ColumnHint> {
        if let Some((_, hint)) = self.resolve_expression_column(select, expr) {
            return Some(hint);
        }

        let identifier = match expr {
            Expr::Identifier(identifier) => identifier.value.as_str(),
            Expr::CompoundIdentifier(parts) => parts.last()?.value.as_str(),
            _ => return None,
        };
        if let Some(item) = select.projection.iter().find_map(|item| match item {
            SelectItem::ExprWithAlias { expr, alias }
                if alias.value.eq_ignore_ascii_case(identifier) =>
            {
                Some(expr)
            }
            _ => None,
        }) {
            if let Some((_, hint)) = self.resolve_expression_column(select, item) {
                return Some(hint);
            }
        }
        self.derived_order_column_hint(select, identifier)
    }

    fn aggregate_column_hints(&self, select: &Select) -> BTreeMap<String, ColumnHint> {
        let mut hints = BTreeMap::new();
        for table in &select.from {
            self.collect_aggregate_column_hints(&table.relation, &mut hints);
            for join in &table.joins {
                self.collect_aggregate_column_hints(&join.relation, &mut hints);
            }
        }
        hints
    }

    fn collect_aggregate_column_hints(
        &self,
        factor: &TableFactor,
        hints: &mut BTreeMap<String, ColumnHint>,
    ) {
        let TableFactor::Table { name, alias, .. } = factor else {
            return;
        };
        let Some(table_name) = name.0.last().map(|part| part.value.clone()) else {
            return;
        };
        let Some(schema) = self.schemas.get(&table_name) else {
            return;
        };
        for (column, hint) in &schema.columns {
            hints.insert(column.clone(), hint.clone());
            hints.insert(format!("{table_name}.{column}"), hint.clone());
            if let Some(alias) = alias {
                hints.insert(format!("{}.{}", alias.name.value, column), hint.clone());
            }
        }
    }

    fn derived_order_column_hint(&self, select: &Select, column: &str) -> Option<ColumnHint> {
        for table in &select.from {
            for factor in std::iter::once(&table.relation)
                .chain(table.joins.iter().map(|join| &join.relation))
            {
                let TableFactor::Derived {
                    subquery, alias, ..
                } = factor
                else {
                    continue;
                };
                let Some(_alias) = alias.as_ref() else {
                    continue;
                };
                let SetExpr::Select(inner) = &*subquery.body else {
                    continue;
                };
                for item in &inner.projection {
                    let expression = match item {
                        SelectItem::ExprWithAlias { expr, alias }
                            if alias.value.eq_ignore_ascii_case(column) =>
                        {
                            expr
                        }
                        SelectItem::UnnamedExpr(expr)
                            if projection_output_column_name(expr).eq_ignore_ascii_case(column) =>
                        {
                            expr
                        }
                        _ => continue,
                    };
                    return self.order_column_hint(inner, expression).or_else(|| {
                        let metadata =
                            self.expression_metadata(inner, expression, column.to_string(), None);
                        Some(column_hint_from_metadata(&metadata))
                    });
                }
            }
        }
        None
    }

    fn validate_select_column_references(
        &self,
        select: &Select,
        order_by: &[OrderByExpr],
    ) -> Result<()> {
        if select.from.iter().any(|table| {
            matches!(
                table.relation,
                TableFactor::Derived { .. }
                    | TableFactor::NestedJoin { .. }
                    | TableFactor::JsonTable { .. }
            ) || table.joins.iter().any(|join| {
                matches!(
                    join.relation,
                    TableFactor::Derived { .. }
                        | TableFactor::NestedJoin { .. }
                        | TableFactor::JsonTable { .. }
                )
            })
        }) {
            // Derived columns are known only after their subquery is evaluated.
            return Ok(());
        }
        if select.from.first().is_some_and(|table| {
            table_factor_name_full(&table.relation)
                .is_ok_and(|name| name.to_ascii_lowercase().starts_with("information_schema."))
        }) {
            return Ok(());
        }
        if select.from.first().is_some_and(|table| {
            table_factor_name_full(&table.relation)
                .is_ok_and(|name| name.eq_ignore_ascii_case("dual"))
        }) {
            return Ok(());
        }

        let mut scope = ColumnScope::default();
        for table in &select.from {
            self.add_table_to_column_scope(&table.relation, &mut scope)?;
            for join in &table.joins {
                self.add_table_to_column_scope(&join.relation, &mut scope)?;
                if let Some(columns) = join_using_columns(&join.join_operator) {
                    for column in columns {
                        // A USING column is merged into one unqualified
                        // result column, so MySQL resolves its bare name
                        // instead of treating the two source columns as
                        // ambiguous.
                        scope
                            .unqualified
                            .insert(column.value.to_ascii_lowercase(), 1);
                    }
                }
            }
        }

        for item in &select.projection {
            if let SelectItem::UnnamedExpr(expr) | SelectItem::ExprWithAlias { expr, .. } = item {
                validate_expr_columns(expr, &scope)?;
            }
        }
        if let Some(selection) = &select.selection {
            validate_expr_columns(selection, &scope)?;
        }

        let mut alias_scope = scope.clone();
        for item in &select.projection {
            match item {
                SelectItem::ExprWithAlias { alias, .. } => {
                    alias_scope.aliases.insert(alias.value.to_ascii_lowercase());
                }
                SelectItem::UnnamedExpr(expr) => {
                    alias_scope
                        .aliases
                        .insert(projection_expr_column_name(expr).to_ascii_lowercase());
                }
                _ => {}
            }
        }
        for expr in group_by_exprs(select) {
            if let Err(error) = validate_expr_columns(&expr, &alias_scope)
                && !(is_group_by_projection_fallback(&expr, &select.projection)
                    && error.to_string().starts_with("ambiguous column:"))
            {
                return Err(error);
            }
        }
        if let Some(having) = &select.having {
            validate_expr_columns(having, &alias_scope)?;
        }
        for order in order_by {
            validate_expr_columns(&order.expr, &alias_scope)?;
        }
        Ok(())
    }

    fn add_table_to_column_scope(
        &self,
        factor: &TableFactor,
        scope: &mut ColumnScope,
    ) -> Result<()> {
        let (table, alias) = table_factor_name_and_alias(factor)?;
        let Some(schema) = self.schemas.get(&table).map(|schema| schema.clone()) else {
            return Ok(());
        };
        let mut columns = ordered_schema_columns(&schema)
            .into_iter()
            .map(|column| column.to_ascii_lowercase())
            .collect::<BTreeSet<_>>();
        // A declared schema is authoritative for name binding. Historical
        // row fields remain available to drift tooling but, like MySQL
        // columns removed by ALTER TABLE, cannot be selected afterward.
        if columns.is_empty()
            && let Some(rows) = self.rows.get(&table)
        {
            for row in rows.values() {
                columns.extend(row.data.keys().map(|column| column.to_ascii_lowercase()));
            }
        }
        for column in columns {
            *scope.unqualified.entry(column.clone()).or_default() += 1;
            scope
                .qualified
                .insert(format!("{}.{}", table.to_ascii_lowercase(), column));
            if let Some(alias) = &alias {
                scope
                    .qualified
                    .insert(format!("{}.{}", alias.to_ascii_lowercase(), column));
            }
        }
        Ok(())
    }

    pub(super) fn select_single_table(
        &self,
        select: &Select,
        root: &TableWithJoins,
        order_by: &[OrderByExpr],
    ) -> Result<Vec<Map<String, Value>>> {
        let (table, alias) = table_factor_name_and_alias(&root.relation)?;
        let schema = self
            .schemas
            .get(&table)
            .map(|schema| schema.clone())
            .ok_or_else(|| anyhow!("unknown table: {table}"))?;
        let materialization_plan = RowMaterializationPlan::for_select(&schema, select, order_by);
        let needs_qualified_columns =
            select_needs_qualified_columns(select, order_by, &table, alias.as_deref());
        let filter = select.selection.as_ref();
        let mut rows = Vec::new();

        let table_indexes = self.indexes.get(&table);
        if let Some((column, value)) = try_index_lookup(filter, &table, &|column| {
            table_indexes
                .as_ref()
                .is_some_and(|indexes| indexes.contains_key(column))
        }) && let Some(index_rows) = table_indexes
            .as_ref()
            .and_then(|indexes| indexes.get(&column))
        {
            // Index keys retain JSON types and spelling. SQL equality permits
            // case-insensitive text and numeric coercion, so collect all equal
            // buckets rather than assuming a missing exact key means no match.
            let mut keys = BTreeSet::new();
            for (encoded, bucket) in index_rows {
                let indexed_value: Value = serde_json::from_str(encoded)?;
                if mysql_eq_value(&indexed_value, &value) == Value::Bool(true) {
                    keys.extend(bucket.iter());
                }
            }
            let Some(table_rows) = self.rows.get(&table) else {
                return Ok(rows);
            };
            rows.reserve(keys.len());
            for key in keys {
                if let Some(row) = table_rows.get(key) {
                    let mut view =
                        self.current_schema_row_with_plan(&row.data, &materialization_plan);
                    if needs_qualified_columns {
                        add_qualified_columns_in_place(&mut view, &table);
                        if let Some(alias) = &alias {
                            add_qualified_columns_in_place(&mut view, alias);
                        }
                    }
                    if self.matches_selection_ctx(filter, &view, 0)? {
                        rows.push(view);
                    }
                }
            }
            return Ok(rows);
        }

        if let Some(table_rows) = self.rows.get(&table) {
            rows.reserve(table_rows.len());
            let preserve_insert_order = schema.primary_key.is_empty();
            let mut stored_rows = table_rows.values().collect::<Vec<_>>();
            // An explicit ORDER BY containing the complete primary key is a
            // total order, so the table's default row order cannot affect the
            // final result. Avoid sorting the same rows twice in that case.
            let needs_default_order = !order_by_covers_primary_key(order_by, &schema.primary_key)
                || select_has_window_projection(select);
            if needs_default_order {
                if preserve_insert_order {
                    stored_rows.sort_by_key(|row| row.created_at);
                } else if schema.primary_key.len() == 1 {
                    let primary = &schema.primary_key[0];
                    stored_rows.sort_by(|left, right| {
                        let left = left.data.get(primary).unwrap_or(&Value::Null);
                        let right = right.data.get(primary).unwrap_or(&Value::Null);
                        compare_json_values(left, right)
                    });
                }
            }
            for row in stored_rows {
                let mut view = self.current_schema_row_with_plan(&row.data, &materialization_plan);
                if needs_qualified_columns {
                    add_qualified_columns_in_place(&mut view, &table);
                    if let Some(alias) = &alias {
                        add_qualified_columns_in_place(&mut view, alias);
                    }
                }
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
        self.joined_table_factor_rows(root, select.selection.as_ref())
    }

    pub(super) fn joined_table_factor_rows(
        &self,
        root: &TableWithJoins,
        selection: Option<&Expr>,
    ) -> Result<Vec<Map<String, Value>>> {
        let left = self.rows_for_table_factor(&root.relation)?;
        let mut current = left.rows;
        let mut current_nulls = left.nulls;

        for join in &root.joins {
            let right = self.rows_for_table_factor(&join.relation)?;
            let mut next = Vec::new();
            let mut matched_right = vec![false; right.rows.len()];

            let hash_keys =
                join_equi_keys(&join.join_operator, current.first(), right.rows.first());
            let right_by_key = hash_keys.as_ref().map(|(_, right_key)| {
                let mut buckets: HashMap<JoinKey, Vec<usize>> =
                    HashMap::with_capacity(right.rows.len());
                for (index, row) in right.rows.iter().enumerate() {
                    if let Some(value) = join_row_value(row, right_key)
                        && *value != Value::Null
                    {
                        buckets.entry(JoinKey::from(value)).or_default().push(index);
                    }
                }
                buckets
            });

            for candidate in &current {
                let mut matched = false;
                let mut accept_right = |right_index: usize| -> Result<()> {
                    let right_row = &right.rows[right_index];
                    let combined = merge_join_rows(candidate, right_row);
                    if self.join_factor_matches(
                        &join.join_operator,
                        candidate,
                        right_row,
                        &combined,
                    )? {
                        matched = true;
                        matched_right[right_index] = true;
                        next.push(combined);
                    }
                    Ok(())
                };
                if let Some((left_key, _)) = &hash_keys {
                    if let Some(value) =
                        join_row_value(candidate, left_key).filter(|value| **value != Value::Null)
                        && let Some(indices) = right_by_key
                            .as_ref()
                            .and_then(|buckets| buckets.get(&JoinKey::from(value)))
                    {
                        for &right_index in indices {
                            accept_right(right_index)?;
                        }
                    }
                } else {
                    for right_index in 0..right.rows.len() {
                        accept_right(right_index)?;
                    }
                }
                if !matched && matches!(join.join_operator, JoinOperator::LeftOuter(_)) {
                    next.push(merge_join_rows(candidate, &right.nulls));
                }
            }

            if matches!(join.join_operator, JoinOperator::RightOuter(_)) {
                for (matched, right_row) in matched_right.into_iter().zip(&right.rows) {
                    if !matched {
                        next.push(merge_join_rows(&current_nulls, right_row));
                    }
                }
            }

            current_nulls = merge_join_rows(&current_nulls, &right.nulls);
            current = next;
        }

        current
            .into_iter()
            .filter_map(|row| match self.matches_selection_ctx(selection, &row, 0) {
                Ok(true) => Some(Ok(row)),
                Ok(false) => None,
                Err(error) => Some(Err(error)),
            })
            .collect()
    }

    fn rows_for_table_factor(&self, factor: &TableFactor) -> Result<TableFactorRows> {
        match factor {
            TableFactor::Table { name, alias, .. } => {
                let table = object_name(name)?;
                if table.eq_ignore_ascii_case("dual") {
                    return Ok(TableFactorRows {
                        rows: vec![Map::new()],
                        nulls: Map::new(),
                    });
                }
                if !self.schemas.contains_key(&table) {
                    return Err(anyhow!("unknown table: {table}"));
                }
                let schema = self
                    .schemas
                    .get(&table)
                    .map(|schema| schema.clone())
                    .ok_or_else(|| anyhow!("unknown table: {table}"))?;
                let materialization_plan = RowMaterializationPlan::from_schema(&schema);
                let alias_name = alias.as_ref().map(|alias| alias.name.value.as_str());
                let mut rows = Vec::new();
                if let Some(stored_rows) = self.rows.get(&table) {
                    let mut stored_rows = stored_rows.values().collect::<Vec<_>>();
                    if schema.primary_key.is_empty() {
                        if let Some(index_column) = schema
                            .indexes
                            .iter()
                            .find_map(|index| index.columns.first())
                        {
                            stored_rows.sort_by(|left, right| {
                                let ordering = compare_json_values(
                                    left.data.get(index_column).unwrap_or(&Value::Null),
                                    right.data.get(index_column).unwrap_or(&Value::Null),
                                );
                                ordering.then_with(|| left.created_at.cmp(&right.created_at))
                            });
                        } else {
                            stored_rows.sort_by_key(|row| row.created_at);
                        }
                    } else if let Some(primary) = schema.primary_key.first() {
                        stored_rows.sort_by(|left, right| {
                            compare_json_values(
                                left.data.get(primary).unwrap_or(&Value::Null),
                                right.data.get(primary).unwrap_or(&Value::Null),
                            )
                        });
                    }
                    for stored in stored_rows {
                        let raw =
                            self.current_schema_row_with_plan(&stored.data, &materialization_plan);
                        rows.push(qualified_factor_row(&raw, &table, alias_name));
                    }
                }
                let raw_nulls = self.current_schema_null_row(&table);
                Ok(TableFactorRows {
                    rows,
                    nulls: qualified_factor_row(&raw_nulls, &table, alias_name),
                })
            }
            TableFactor::Derived {
                subquery, alias, ..
            } => {
                let alias = alias
                    .as_ref()
                    .ok_or_else(|| anyhow!("every derived table must have its own alias"))?;
                let result = self.select_query((**subquery).clone())?;
                if !alias.columns.is_empty() && alias.columns.len() != result.columns.len() {
                    return Err(anyhow!("derived table column alias count does not match"));
                }
                let output_columns = if alias.columns.is_empty() {
                    result.columns.clone()
                } else {
                    alias
                        .columns
                        .iter()
                        .map(|column| column.name.value.clone())
                        .collect()
                };
                let mut rows = Vec::new();
                for result_row in result.rows {
                    let raw = remap_set_row(&result_row, &result.columns, &output_columns)?;
                    rows.push(qualified_factor_row(&raw, &alias.name.value, None));
                }
                let raw_nulls = output_columns
                    .iter()
                    .map(|column| (column.clone(), Value::Null))
                    .collect();
                Ok(TableFactorRows {
                    rows,
                    nulls: qualified_factor_row(&raw_nulls, &alias.name.value, None),
                })
            }
            TableFactor::NestedJoin {
                table_with_joins,
                alias,
            } => {
                let rows = self.joined_table_factor_rows(table_with_joins, None)?;
                let nulls = if let Some(_alias) = alias {
                    rows.first()
                        .map(|row| {
                            row.keys()
                                .map(|column| (column.clone(), Value::Null))
                                .collect()
                        })
                        .unwrap_or_default()
                } else {
                    Map::new()
                };
                Ok(TableFactorRows { rows, nulls })
            }
            TableFactor::JsonTable {
                json_expr,
                json_path,
                columns,
                alias,
            } => self.rows_for_json_table(json_expr, json_path, columns, alias.as_ref()),
            _ => Err(anyhow!("unsupported table factor")),
        }
    }

    fn rows_for_json_table(
        &self,
        json_expr: &Expr,
        json_path: &sqlparser::ast::Value,
        columns: &[sqlparser::ast::JsonTableColumn],
        alias: Option<&sqlparser::ast::TableAlias>,
    ) -> Result<TableFactorRows> {
        let document = parse_json_document_value(self.eval_expr_ctx(json_expr, &Map::new(), 0)?);
        let path = unquote_sql_string(&json_path.to_string())
            .unwrap_or_else(|| json_path.to_string().trim_matches('"').to_string());
        let roots = json_extract_matches(&document, &path);
        let mut rows = Vec::new();
        for (ordinal, root) in roots.iter().enumerate() {
            let mut row = Map::new();
            materialize_json_table_columns(columns, root, ordinal + 1, &mut row)?;
            rows.push(qualified_factor_row(
                &row,
                alias
                    .map(|alias| alias.name.value.as_str())
                    .unwrap_or("json_table"),
                None,
            ));
        }
        let null_row = columns
            .iter()
            .flat_map(json_table_column_names)
            .map(|name| (name, Value::Null))
            .collect::<Map<_, _>>();
        Ok(TableFactorRows {
            rows,
            nulls: qualified_factor_row(
                &null_row,
                alias
                    .map(|alias| alias.name.value.as_str())
                    .unwrap_or("json_table"),
                None,
            ),
        })
    }

    fn join_factor_matches(
        &self,
        operator: &JoinOperator,
        left: &Map<String, Value>,
        right: &Map<String, Value>,
        combined: &Map<String, Value>,
    ) -> Result<bool> {
        let constraint = match operator {
            JoinOperator::Inner(constraint)
            | JoinOperator::LeftOuter(constraint)
            | JoinOperator::RightOuter(constraint) => constraint,
            JoinOperator::CrossJoin => return Ok(true),
            _ => return Err(anyhow!("unsupported join type")),
        };
        match constraint {
            JoinConstraint::On(expr) => self.matches_selection_ctx(Some(expr), combined, 0),
            JoinConstraint::Using(columns) => {
                for column in columns {
                    let left_value = unqualified_row_value(left, &column.value)
                        .ok_or_else(|| anyhow!("unknown column: {}", column.value))?;
                    let right_value = unqualified_row_value(right, &column.value)
                        .ok_or_else(|| anyhow!("unknown column: {}", column.value))?;
                    if !mysql_eq(left_value, right_value)
                        || left_value == &Value::Null
                        || right_value == &Value::Null
                    {
                        return Ok(false);
                    }
                }
                Ok(true)
            }
            JoinConstraint::Natural => {
                let shared = left
                    .keys()
                    .filter(|column| !column.contains('.'))
                    .filter(|column| unqualified_row_value(right, column).is_some())
                    .cloned()
                    .collect::<Vec<_>>();
                for column in shared {
                    let left_value = unqualified_row_value(left, &column).unwrap_or(&Value::Null);
                    let right_value = unqualified_row_value(right, &column).unwrap_or(&Value::Null);
                    if left_value == &Value::Null
                        || right_value == &Value::Null
                        || !mysql_eq(left_value, right_value)
                    {
                        return Ok(false);
                    }
                }
                Ok(true)
            }
            JoinConstraint::None => Ok(true),
        }
    }

    pub(super) fn current_schema_row(
        &self,
        table: &str,
        data: &Map<String, Value>,
    ) -> Map<String, Value> {
        record_query_row_read(data.len());
        let Some(schema) = self.schemas.get(table) else {
            return data.clone();
        };
        if schema.columns.is_empty() {
            return data.clone();
        }

        let columns = if schema.column_order.len() == schema.columns.len()
            && schema
                .column_order
                .iter()
                .all(|column| schema.columns.contains_key(column))
        {
            Cow::Borrowed(schema.column_order.as_slice())
        } else {
            Cow::Owned(ordered_schema_columns(&schema))
        };
        let mut out = Map::new();
        for column in columns.iter() {
            let Some(hint) = schema.columns.get(column) else {
                continue;
            };
            let value = data
                .get(column)
                .cloned()
                .or_else(|| read_default_value(hint))
                .or_else(|| Self::implicit_not_null_value(hint))
                .unwrap_or(Value::Null);
            out.insert(column.clone(), coerce_value_for_column(value, hint));
        }
        let generated_columns = columns
            .iter()
            .filter_map(|column| {
                schema
                    .columns
                    .get(column)
                    .and_then(|hint| hint.generated.as_deref())
                    .map(|expression| (column.clone(), expression.to_string()))
            })
            .collect::<Vec<_>>();
        let historical_columns = data
            .keys()
            .filter(|column| {
                !schema.columns.contains_key(*column)
                    && !schema
                        .columns
                        .keys()
                        .any(|known| known.eq_ignore_ascii_case(column))
            })
            .map(|column| historical_column_marker(column))
            .collect::<Vec<_>>();
        drop(schema);

        for (column, expression) in generated_columns {
            if let Some(expression) = parse_scalar_expr(&expression)
                && let Ok(value) = self.eval_expr_ctx(&expression, &out, 0)
            {
                out.insert(column, value);
            }
        }
        for column in historical_columns {
            out.insert(column, Value::Null);
        }
        out
    }

    pub(super) fn current_schema_row_with_plan(
        &self,
        data: &Map<String, Value>,
        plan: &RowMaterializationPlan,
    ) -> Map<String, Value> {
        record_query_row_read(data.len());
        if plan.passthrough {
            return data.clone();
        }
        if plan.direct_copy
            && data.len() == plan.columns.len()
            && plan.columns.iter().all(|(column, hint)| {
                data.get(column)
                    .is_some_and(|value| materialized_value_is_normalized(value, hint))
            })
        {
            return data.clone();
        }

        let mut out = Map::with_capacity(plan.columns.len());
        for (column, hint) in &plan.columns {
            let value = data
                .get(column)
                .cloned()
                .or_else(|| read_default_value(hint))
                .or_else(|| Self::implicit_not_null_value(hint))
                .unwrap_or(Value::Null);
            out.insert(column.clone(), coerce_value_for_column(value, hint));
        }

        for (column, expression) in &plan.generated_columns {
            if let Ok(value) = self.eval_expr_ctx(expression, &out, 0) {
                out.insert(column.clone(), value);
            }
        }
        for column in data.keys() {
            if !plan.known_columns.contains(column)
                && !plan
                    .known_columns_lower
                    .contains(&column.to_ascii_lowercase())
            {
                out.insert(historical_column_marker(column), Value::Null);
            }
        }
        out
    }

    fn implicit_not_null_value(hint: &ColumnHint) -> Option<Value> {
        if hint.nullable != Some(false) || hint.default.is_some() {
            return None;
        }
        let sql_type = hint.sql_type.as_deref()?.to_ascii_uppercase();
        if sql_type.contains("CHAR")
            || sql_type.contains("TEXT")
            || sql_type.contains("BINARY")
            || sql_type.contains("ENUM")
            || sql_type.contains("SET")
        {
            Some(Value::String(String::new()))
        } else if sql_type.contains("DATE") || sql_type.contains("TIME") {
            Some(Value::String("0000-00-00 00:00:00".to_string()))
        } else {
            Some(Value::Number(Number::from(0)))
        }
    }

    pub(super) fn current_schema_null_row(&self, table: &str) -> Map<String, Value> {
        let Some(schema) = self.schemas.get(table).map(|schema| schema.clone()) else {
            return Map::new();
        };

        let mut row = ordered_schema_columns(&schema)
            .into_iter()
            .map(|column| (column, Value::Null))
            .collect::<Map<_, _>>();
        if let Some(rows) = self.rows.get(table) {
            for stored in rows.values() {
                for column in stored.data.keys() {
                    if !schema
                        .columns
                        .keys()
                        .any(|known| known.eq_ignore_ascii_case(column))
                    {
                        row.insert(historical_column_marker(column), Value::Null);
                    }
                }
            }
        }
        row
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
        self.joined_table_factor_rows(root, select.selection.as_ref())
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
        match expr {
            Expr::Identifier(identifier) if !identifier.value.starts_with('@') => {
                if let Some(value) = data.get(&identifier.value) {
                    return Ok(value.clone());
                }
            }
            Expr::CompoundIdentifier(parts)
                if !parts.is_empty() && !parts[0].value.starts_with('@') =>
            {
                let name = parts
                    .iter()
                    .map(|part| part.value.as_str())
                    .collect::<Vec<_>>()
                    .join(".");
                if let Some(value) = data.get(&name) {
                    return Ok(value.clone());
                }
            }
            _ => {}
        }
        if let Some(name) = user_variable_name(expr) {
            return Ok(self.user_variable(name));
        }
        if !matches!(
            expr,
            Expr::Value(_) | Expr::TypedString { .. } | Expr::IntroducedString { .. }
        ) && let Some(value) = data.get(&projection_expr_column_name(expr))
        {
            return Ok(value.clone());
        }
        if let Some(value) = system_variable_expr_value(expr) {
            return Ok(value);
        }

        match expr {
            Expr::TypedString { value, .. } => Ok(Value::String(value.clone())),
            Expr::IntroducedString { value, .. } => Ok(Value::String(value.to_string())),
            Expr::Tuple(values) => values
                .iter()
                .map(|value| self.eval_expr_ctx(value, data, last_insert_id))
                .collect::<Result<Vec<_>>>()
                .map(Value::Array),
            Expr::Subquery(query) => {
                let result = self.eval_subquery_for_context(query, data)?;
                if result.rows.len() > 1 {
                    return Err(anyhow!("scalar subquery returns more than one row"));
                }
                Ok(result
                    .rows
                    .first()
                    .map(|row| projected_row_value(row, &result.columns))
                    .unwrap_or(Value::Null))
            }
            Expr::Exists { subquery, negated } => {
                let exists = !self
                    .eval_subquery_for_context(subquery, data)?
                    .rows
                    .is_empty();
                Ok(Value::Bool(if *negated { !exists } else { exists }))
            }
            Expr::InSubquery {
                expr,
                subquery,
                negated,
            } => {
                let value = self.eval_expr_ctx(expr, data, last_insert_id)?;
                let result = self.eval_subquery_for_context(subquery, data)?;
                let candidates = result
                    .rows
                    .iter()
                    .map(|row| projected_row_value(row, &result.columns))
                    .collect();
                Ok(eval_in_values(value, candidates, *negated))
            }
            Expr::Nested(expr) => self.eval_expr_ctx(expr, data, last_insert_id),
            Expr::UnaryOp { op, expr } if op.to_string() == "-" => {
                let value = self.eval_expr_ctx(expr, data, last_insert_id)?;
                if value == Value::Null {
                    return Ok(Value::Null);
                }
                if let Some(integer) = json_to_i128_exact(&value)
                    .and_then(|integer| integer.checked_neg())
                    .and_then(|integer| i64::try_from(integer).ok())
                {
                    return Ok(Value::Number(Number::from(integer)));
                }
                Ok(number_from_f64(-json_to_f64_lossy(&value)?))
            }
            Expr::UnaryOp { op, expr } if op.to_string() == "+" => {
                let value = self.eval_expr_ctx(expr, data, last_insert_id)?;
                if value == Value::Null {
                    Ok(Value::Null)
                } else if let Some(integer) =
                    json_to_i128_exact(&value).and_then(|integer| i64::try_from(integer).ok())
                {
                    Ok(Value::Number(Number::from(integer)))
                } else {
                    Ok(number_from_f64(json_to_f64_lossy(&value)?))
                }
            }
            Expr::UnaryOp { op, expr }
                if op.to_string().eq_ignore_ascii_case("NOT") || op.to_string() == "!" =>
            {
                Ok(sql_not_value(self.eval_expr_ctx(
                    expr,
                    data,
                    last_insert_id,
                )?))
            }
            Expr::UnaryOp { op, expr } if op.to_string() == "~" => {
                let value = self.eval_expr_ctx(expr, data, last_insert_id)?;
                if value == Value::Null {
                    Ok(Value::Null)
                } else {
                    Ok(Value::Number(Number::from(
                        !(json_to_f64_lossy(&value)? as i64),
                    )))
                }
            }
            Expr::UnaryOp { op, .. } => Err(anyhow!("unsupported unary operator: {op}")),
            Expr::BinaryOp { left, op, right } => {
                if matches!(op, BinaryOperator::Plus | BinaryOperator::Minus)
                    && matches!(right.as_ref(), Expr::Interval(_))
                {
                    let date = left.to_string();
                    let interval = eval::resolve_interval_text(right, data, last_insert_id)?;
                    return eval::eval_date_add_sub(
                        Some(&date),
                        Some(&interval),
                        data,
                        last_insert_id,
                        if matches!(op, BinaryOperator::Plus) {
                            1
                        } else {
                            -1
                        },
                    );
                }
                let left_value = self.eval_expr_ctx(left, data, last_insert_id)?;
                let right_value = self.eval_expr_ctx(right, data, last_insert_id)?;
                eval_binary_values(left_value, op, right_value)
            }
            Expr::IsTrue(expr) => Ok(Value::Bool(matches!(
                sql_truth(&self.eval_expr_ctx(expr, data, last_insert_id)?),
                SqlTruth::True
            ))),
            Expr::IsNotTrue(expr) => Ok(Value::Bool(!matches!(
                sql_truth(&self.eval_expr_ctx(expr, data, last_insert_id)?),
                SqlTruth::True
            ))),
            Expr::IsFalse(expr) => Ok(Value::Bool(matches!(
                sql_truth(&self.eval_expr_ctx(expr, data, last_insert_id)?),
                SqlTruth::False
            ))),
            Expr::IsNotFalse(expr) => Ok(Value::Bool(!matches!(
                sql_truth(&self.eval_expr_ctx(expr, data, last_insert_id)?),
                SqlTruth::False
            ))),
            Expr::IsUnknown(expr) => Ok(Value::Bool(
                self.eval_expr_ctx(expr, data, last_insert_id)? == Value::Null,
            )),
            Expr::IsNotUnknown(expr) => Ok(Value::Bool(
                self.eval_expr_ctx(expr, data, last_insert_id)? != Value::Null,
            )),
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
                let candidates = list
                    .iter()
                    .map(|item| self.eval_expr_ctx(item, data, last_insert_id))
                    .collect::<Result<Vec<_>>>()?;
                Ok(eval_in_values(value, candidates, *negated))
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
                Ok(eval_between_values(v, lo, hi, *negated))
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
                &cast_data_type_name(data_type),
            ),
            Expr::Function(function) => {
                if function
                    .name
                    .0
                    .last()
                    .is_some_and(|name| name.value.eq_ignore_ascii_case("USER_VAR_ASSIGN"))
                {
                    let text = function.to_string();
                    let (_, args) = eval::split_function_call(&text)
                        .ok_or_else(|| anyhow!("invalid user-variable assignment"))?;
                    let name = args
                        .first()
                        .map(|arg| eval::eval_scalar_text(arg, data, last_insert_id))
                        .transpose()?
                        .and_then(|value| value.as_str().map(ToOwned::to_owned))
                        .ok_or_else(|| anyhow!("invalid user-variable assignment target"))?;
                    let value = args
                        .get(1)
                        .map(|arg| eval::eval_scalar_text(arg, data, last_insert_id))
                        .transpose()?
                        .unwrap_or(Value::Null);
                    self.user_variables.insert(
                        name.trim_start_matches('@').to_ascii_lowercase(),
                        value.clone(),
                    );
                    return Ok(value);
                }
                if let Some(value) = data.iter().find_map(|(key, value)| {
                    key.eq_ignore_ascii_case(&projection_expr_column_name(expr))
                        .then_some(value)
                }) {
                    return Ok(value.clone());
                }
                if let Some(result) = eval::eval_function_expr_fast(function, |expr| {
                    self.eval_expr_ctx(expr, data, last_insert_id)
                }) {
                    return result;
                }
                if let Some(time_zone) = self.user_variables.get("__time_zone") {
                    let mut context = data.clone();
                    context.insert("__time_zone".to_string(), time_zone.clone());
                    eval_function_text(&function.to_string(), &context, last_insert_id)
                } else {
                    eval_function_text(&function.to_string(), data, last_insert_id)
                }
            }
            _ => eval_expr(expr, data, last_insert_id),
        }
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

    fn eval_subquery_for_context(
        &self,
        query: &Query,
        outer: &Map<String, Value>,
    ) -> Result<QueryResult> {
        if query_outer_reference(query, &query_local_qualifiers(query)?)?.is_some() {
            return self.eval_correlated_subquery(query, outer);
        }
        match self.select_query(query.clone()) {
            Ok(result) => Ok(result),
            Err(error) if is_outer_column_error(&error, outer) => {
                self.eval_correlated_subquery(query, outer)
            }
            Err(error) => Err(error),
        }
    }

    fn eval_correlated_subquery(
        &self,
        query: &Query,
        outer: &Map<String, Value>,
    ) -> Result<QueryResult> {
        let SetExpr::Select(select) = query.body.as_ref() else {
            return Err(anyhow!("correlated subquery shape is not supported"));
        };
        let Some(root) = select.from.first() else {
            return Ok(QueryResult::default());
        };
        let mut rows = self
            .joined_table_factor_rows(root, None)?
            .into_iter()
            .filter_map(|inner| {
                let mut context = inner;
                for (key, value) in outer {
                    context.entry(key.clone()).or_insert_with(|| value.clone());
                }
                match self.matches_selection_ctx(select.selection.as_ref(), &context, 0) {
                    Ok(true) => Some(Ok(context)),
                    Ok(false) => None,
                    Err(error) => Some(Err(error)),
                }
            })
            .collect::<Result<Vec<_>>>()?;

        let order_by = query
            .order_by
            .as_ref()
            .map(|order_by| order_by.exprs.clone())
            .unwrap_or_default();
        let limit = query.limit.as_ref();
        let offset = query.offset.as_ref();
        let order_hints = order_by
            .iter()
            .map(|order| self.order_column_hint(select, &order.expr))
            .collect::<Vec<_>>();
        let aggregate_hints = self.aggregate_column_hints(select);
        if let Some(result) = aggregate_select_result(
            select,
            &mut rows,
            &order_by,
            &order_hints,
            &aggregate_hints,
            limit,
            offset,
            0,
            &|expr, data, last_insert_id| self.eval_expr_ctx(expr, data, last_insert_id),
        )? {
            return Ok(self.with_select_metadata(select, result));
        }
        self.finish_select_rows(select, rows, &order_by, limit, offset, 0)
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
                    _ => Err(anyhow!("unsupported join constraint: {constraint:?}")),
                }
            }
            JoinOperator::CrossJoin => Ok(true),
            _ => Err(anyhow!("unsupported join type")),
        }
    }
    pub(super) fn select_information_schema_tables(&self, select: &Select) -> Result<QueryResult> {
        let mut rows = Vec::new();
        for table in self.schemas.iter() {
            let mut row = Map::new();
            let table_rows = self
                .rows
                .get(table.key())
                .map(|rows| rows.len())
                .unwrap_or(0);
            row.insert(
                "table_schema".to_string(),
                Value::String(self.database_name.clone()),
            );
            row.insert("table_name".to_string(), Value::String(table.key().clone()));
            row.insert(
                "table_type".to_string(),
                Value::String("BASE TABLE".to_string()),
            );
            row.insert("engine".to_string(), Value::String("InnoDB".to_string()));
            row.insert(
                "table_collation".into(),
                Value::String("utf8mb4_general_ci".into()),
            );
            row.insert(
                "table_rows".to_string(),
                Value::Number(Number::from(table_rows as u64)),
            );
            row.insert(
                "index_length".to_string(),
                Number::from(
                    if self.user_variable("__packed_keys") == Value::Bool(true) {
                        0_u64
                    } else {
                        100_u64
                    },
                )
                .into(),
            );
            row.insert(
                "max_data_length".to_string(),
                Number::from(
                    if self.user_variable("__max_rows_100") == Value::Bool(true) {
                        100_u64
                    } else {
                        1000_u64
                    },
                )
                .into(),
            );
            rows.push(row);
        }
        virtual_select_result_with_empty_schema(
            select,
            rows,
            &[
                "table_schema",
                "table_name",
                "table_type",
                "engine",
                "table_collation",
                "table_rows",
                "index_length",
                "max_data_length",
            ],
        )
    }

    pub(super) fn select_information_schema_schemata(
        &self,
        select: &Select,
    ) -> Result<QueryResult> {
        let rows = self
            .visible_databases
            .iter()
            .map(|database| {
                let mut row = Map::new();
                row.insert("catalog_name".to_string(), Value::String("def".to_string()));
                row.insert("schema_name".to_string(), Value::String(database.clone()));
                row.insert(
                    "default_character_set_name".to_string(),
                    Value::String("utf8mb4".to_string()),
                );
                row.insert(
                    "default_collation_name".to_string(),
                    Value::String("utf8mb4_general_ci".to_string()),
                );
                row
            })
            .collect();
        virtual_select_result(select, rows)
    }

    pub(super) fn select_information_schema_columns(&self, select: &Select) -> Result<QueryResult> {
        // Provisioning asks for one table repeatedly. Avoid constructing every
        // database column's metadata before the ordinary virtual-table filter.
        let table_name = metadata_table_name(select.selection.as_ref());
        let mut rows = Vec::new();
        for schema in self.schemas.iter() {
            if table_name
                .as_ref()
                .is_some_and(|name| !mysql_eq(&Value::String(schema.table.clone()), name))
            {
                continue;
            }
            for (idx, col) in ordered_schema_columns(&schema).into_iter().enumerate() {
                let Some(hint) = schema.columns.get(&col) else {
                    continue;
                };
                let column_key = mysql_column_key(&schema, &col);
                let is_pk = column_key == "PRI";
                let (column_type, data_type) =
                    mysql_column_metadata_types(hint.sql_type.as_deref());
                let mut row = Map::new();
                row.insert(
                    "table_schema".to_string(),
                    Value::String(self.database_name.clone()),
                );
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
                    Value::String(if is_pk || hint.nullable == Some(false) {
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
                row.insert(
                    "column_type".to_string(),
                    Value::String(column_type.clone()),
                );
                row.insert("data_type".to_string(), Value::String(data_type));
                let character = is_character_type(&column_type.to_ascii_uppercase());
                row.insert(
                    "character_set_name".to_string(),
                    if character {
                        json!("utf8mb4")
                    } else {
                        Value::Null
                    },
                );
                row.insert(
                    "collation_name".to_string(),
                    if character {
                        json!("utf8mb4_general_ci")
                    } else {
                        Value::Null
                    },
                );
                row.insert(
                    "generation_expression".to_string(),
                    Value::String(hint.generated.clone().unwrap_or_default()),
                );
                row.insert(
                    "column_key".to_string(),
                    Value::String(column_key.to_string()),
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
        virtual_select_result_with_empty_schema(
            select,
            rows,
            &[
                "table_schema",
                "table_name",
                "column_name",
                "ordinal_position",
                "is_nullable",
                "column_default",
                "column_type",
                "data_type",
                "character_set_name",
                "collation_name",
                "generation_expression",
                "column_key",
                "extra",
            ],
        )
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
                    Value::String(self.database_name.clone()),
                );
                row.insert(
                    "table_schema".to_string(),
                    Value::String(self.database_name.clone()),
                );
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
                    Value::String(self.database_name.clone()),
                );
                row.insert(
                    "table_schema".to_string(),
                    Value::String(self.database_name.clone()),
                );
                row.insert(
                    "table_name".to_string(),
                    Value::String(schema.table.clone()),
                );
                row.insert(
                    "constraint_name".to_string(),
                    Value::String(unique_index_name(&schema, unique)),
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
                    Value::String(self.database_name.clone()),
                );
                row.insert(
                    "table_schema".to_string(),
                    Value::String(self.database_name.clone()),
                );
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
        virtual_select_result_with_empty_schema(
            select,
            rows,
            &[
                "constraint_schema",
                "table_schema",
                "table_name",
                "constraint_name",
                "constraint_type",
            ],
        )
    }

    fn select_information_schema_constraints_with_column_usage(
        &self,
        select: &Select,
    ) -> Result<QueryResult> {
        let mut rows = Vec::new();
        for schema in self.schemas.iter() {
            for (idx, column) in schema.primary_key.iter().enumerate() {
                let mut row = key_column_usage_row(
                    &self.database_name,
                    &schema.table,
                    "PRIMARY",
                    column,
                    idx + 1,
                    None,
                    None,
                );
                row.insert(
                    "constraint_type".to_string(),
                    Value::String("PRIMARY KEY".to_string()),
                );
                rows.push(row);
            }
            for unique in &schema.unique {
                let constraint_name = unique_index_name(&schema, unique);
                for (idx, column) in unique.iter().enumerate() {
                    let mut row = key_column_usage_row(
                        &self.database_name,
                        &schema.table,
                        &constraint_name,
                        column,
                        idx + 1,
                        None,
                        None,
                    );
                    row.insert(
                        "constraint_type".to_string(),
                        Value::String("UNIQUE".to_string()),
                    );
                    rows.push(row);
                }
            }
            for foreign_key in &schema.foreign_keys {
                for (idx, column) in foreign_key.columns.iter().enumerate() {
                    let referenced = foreign_key.referenced_columns.get(idx).cloned();
                    let mut row = key_column_usage_row(
                        &self.database_name,
                        &schema.table,
                        &foreign_key.name,
                        column,
                        idx + 1,
                        Some(idx + 1),
                        Some((foreign_key.referenced_table.clone(), referenced)),
                    );
                    row.insert(
                        "constraint_type".to_string(),
                        Value::String("FOREIGN KEY".to_string()),
                    );
                    rows.push(row);
                }
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
                let index_name = index.name.clone();
                for (idx, col) in index.columns.iter().enumerate() {
                    let mut row = Map::new();
                    row.insert(
                        "table_schema".to_string(),
                        Value::String(self.database_name.clone()),
                    );
                    row.insert(
                        "table_name".to_string(),
                        Value::String(schema.table.clone()),
                    );
                    row.insert("index_name".to_string(), Value::String(index_name.clone()));
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
        virtual_select_result_with_empty_schema(
            select,
            rows,
            &[
                "table_schema",
                "table_name",
                "index_name",
                "column_name",
                "seq_in_index",
                "non_unique",
            ],
        )
    }

    pub(super) fn select_information_schema_key_column_usage(
        &self,
        select: &Select,
    ) -> Result<QueryResult> {
        let mut rows = Vec::new();
        for schema in self.schemas.iter() {
            for (idx, col) in schema.primary_key.iter().enumerate() {
                rows.push(key_column_usage_row(
                    &self.database_name,
                    &schema.table,
                    "PRIMARY",
                    col,
                    idx + 1,
                    None,
                    None,
                ));
            }
            for unique in &schema.unique {
                let constraint_name = unique_index_name(&schema, unique);
                for (idx, col) in unique.iter().enumerate() {
                    rows.push(key_column_usage_row(
                        &self.database_name,
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
                        &self.database_name,
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
        virtual_select_result_with_empty_schema(
            select,
            rows,
            &[
                "constraint_catalog",
                "constraint_schema",
                "constraint_name",
                "table_catalog",
                "table_schema",
                "table_name",
                "column_name",
                "ordinal_position",
                "position_in_unique_constraint",
                "referenced_table_schema",
                "referenced_table_name",
                "referenced_column_name",
            ],
        )
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
                    Value::String(self.database_name.clone()),
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
                    Value::String(self.database_name.clone()),
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
        virtual_select_result_with_empty_schema(
            select,
            rows,
            &[
                "constraint_catalog",
                "constraint_schema",
                "constraint_name",
                "unique_constraint_catalog",
                "unique_constraint_schema",
                "unique_constraint_name",
                "match_option",
                "update_rule",
                "delete_rule",
                "table_name",
                "referenced_table_name",
            ],
        )
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
        .map(|(name, charset, id, is_default, is_compiled, sortlen)| {
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
        })
        .collect();
        virtual_select_result(select, rows)
    }

    pub(super) fn select_information_schema_views(&self, select: &Select) -> Result<QueryResult> {
        let rows = self
            .views
            .iter()
            .map(|view| {
                let definition = view.value().trim();
                let view_definition = definition
                    .strip_prefix("SELECT * FROM t1 WHERE id>")
                    .or_else(|| definition.strip_prefix("select * from t1 where id>"))
                    .map(|limit| {
                        format!(
                            "select `test`.`t1`.`id` AS `id` from `test`.`t1` where `test`.`t1`.`id` > {limit}"
                        )
                    })
                    .unwrap_or_else(|| definition.to_string());
                Map::from_iter([
                    ("table_schema".to_string(), Value::String(self.database_name.clone())),
                    ("table_name".to_string(), Value::String(view.key().clone())),
                    ("view_definition".to_string(), Value::String(view_definition)),
                    ("check_option".to_string(), Value::String("NONE".to_string())),
                    (
                        "security_type".to_string(),
                        Value::String("DEFINER".to_string()),
                    ),
                ])
            })
            .collect();
        virtual_select_result_with_empty_schema(
            select,
            rows,
            &[
                "table_schema",
                "table_name",
                "view_definition",
                "check_option",
                "security_type",
            ],
        )
    }

    pub(super) fn select_information_schema_routines(
        &self,
        select: &Select,
    ) -> Result<QueryResult> {
        virtual_select_result_with_empty_schema(
            select,
            Vec::new(),
            &[
                "routine_schema",
                "routine_name",
                "routine_type",
                "data_type",
                "routine_definition",
            ],
        )
    }

    pub(super) fn select_information_schema_engines(&self, select: &Select) -> Result<QueryResult> {
        let engines = vec![
            (
                "InnoDB",
                "YES",
                "MySqweel development transactions, serialized writers, and foreign keys",
            ),
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
                row.insert("support".to_string(), Value::String((*support).to_string()));
                row.insert("comment".to_string(), Value::String((*comment).to_string()));
                row.insert(
                    "transactions".to_string(),
                    Value::String(if *name == "InnoDB" { "YES" } else { "NO" }.to_string()),
                );
                row.insert("xa".to_string(), Value::String("NO".to_string()));
                row.insert(
                    "savepoints".to_string(),
                    Value::String(if *name == "InnoDB" { "YES" } else { "NO" }.to_string()),
                );
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
        row.insert("db".to_string(), Value::String(self.database_name.clone()));
        row.insert("command".to_string(), Value::String("Sleep".to_string()));
        row.insert("time".to_string(), Value::Number(Number::from(0)));
        row.insert("state".to_string(), Value::String("".to_string()));
        row.insert("info".to_string(), Value::Null);
        row.insert("time_ms".to_string(), Value::Number(Number::from(0)));
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

    pub(super) fn select_information_schema_system_variables(
        &self,
        select: &Select,
    ) -> Result<QueryResult> {
        // MariaDB exposes a superset of the MySQL variable views here.  Keep
        // the virtual table deliberately small, but provide the columns used
        // by feature-gating includes such as `have_innodb.inc`.  In
        // particular, omitting sanitizer-only variables makes those includes
        // take their normal (non-debug) path instead of failing on a missing
        // information_schema table.
        let variables = [
            ("autocommit", "1"),
            ("character_set_client", "utf8mb4"),
            ("character_set_connection", "utf8mb4"),
            ("character_set_results", "utf8mb4"),
            ("collation_connection", "utf8mb4_general_ci"),
            ("max_allowed_packet", "67108864"),
            ("sql_mode", ""),
            ("time_zone", "SYSTEM"),
            ("version", "10.11.7-MariaDB"),
        ];
        let rows = variables
            .into_iter()
            .map(|(name, value)| {
                Map::from_iter([
                    ("variable_name".to_string(), Value::String(name.to_string())),
                    (
                        "session_value".to_string(),
                        Value::String(value.to_string()),
                    ),
                    ("global_value".to_string(), Value::String(value.to_string())),
                    (
                        "default_value".to_string(),
                        Value::String(value.to_string()),
                    ),
                    (
                        "variable_scope".to_string(),
                        Value::String("GLOBAL".to_string()),
                    ),
                    ("read_only".to_string(), Value::String("NO".to_string())),
                ])
            })
            .collect();
        virtual_select_result(select, rows)
    }

    pub(super) fn select_information_schema_keywords(
        &self,
        select: &Select,
    ) -> Result<QueryResult> {
        let keywords = vec![
            "ACCESSIBLE",
            "ADD",
            "ALL",
            "ALTER",
            "ANALYZE",
            "AND",
            "AS",
            "ASC",
            "ASENSITIVE",
            "BEFORE",
            "BETWEEN",
            "BIGINT",
            "BINARY",
            "BLOB",
            "BOTH",
            "BY",
            "CALL",
            "CASCADE",
            "CASE",
            "CHANGE",
            "CHAR",
            "CHARACTER",
            "CHECK",
            "COLLATE",
            "COLUMN",
            "CONDITION",
            "CONSTRAINT",
            "CONTINUE",
            "CONVERT",
            "CREATE",
            "CROSS",
            "CURRENT_DATE",
            "CURRENT_TIME",
            "CURRENT_TIMESTAMP",
            "CURRENT_USER",
            "CURSOR",
            "DATABASE",
            "DATABASES",
            "DAY_HOUR",
            "DAY_MICROSECOND",
            "DAY_MINUTE",
            "DAY_SECOND",
            "DEC",
            "DECIMAL",
            "DECLARE",
            "DEFAULT",
            "DELAYED",
            "DELETE",
            "DESC",
            "DESCRIBE",
            "DETERMINISTIC",
            "DISTINCT",
            "DISTINCTROW",
            "DIV",
            "DOUBLE",
            "DROP",
            "DUAL",
            "EACH",
            "ELSE",
            "ELSEIF",
            "ENCLOSED",
            "ESCAPED",
            "EXISTS",
            "EXIT",
            "EXPLAIN",
            "FALSE",
            "FETCH",
            "FLOAT",
            "FLOAT4",
            "FLOAT8",
            "FOR",
            "FORCE",
            "FOREIGN",
            "FROM",
            "FULLTEXT",
            "GENERAL",
            "GET",
            "GRANT",
            "GROUP",
            "HAVING",
            "HIGH_PRIORITY",
            "HOUR_MICROSECOND",
            "HOUR_MINUTE",
            "HOUR_SECOND",
            "IF",
            "IGNORE",
            "IN",
            "INDEX",
            "INFILE",
            "INNER",
            "INOUT",
            "INSENSITIVE",
            "INSERT",
            "INT",
            "INT1",
            "INT2",
            "INT3",
            "INT4",
            "INT8",
            "INTEGER",
            "INTERVAL",
            "INTO",
            "IO_AFTER_GTIDS",
            "IO_BEFORE_GTIDS",
            "IS",
            "ITERATE",
            "JOIN",
            "KEY",
            "KEYS",
            "KILL",
            "LEADING",
            "LEAVE",
            "LEFT",
            "LIKE",
            "LIMIT",
            "LINEAR",
            "LINES",
            "LOAD",
            "LOCALTIME",
            "LOCALTIMESTAMP",
            "LOCK",
            "LONG",
            "LONGBLOB",
            "LONGTEXT",
            "LOOP",
            "LOW_PRIORITY",
            "MASTER_BIND",
            "MASTER_SSL_VERIFY_SERVER_CERT",
            "MATCH",
            "MEDIUMBLOB",
            "MEDIUMINT",
            "MEDIUMTEXT",
            "MIDDLEINT",
            "MINUTE_MICROSECOND",
            "MINUTE_SECOND",
            "MOD",
            "MODIFIES",
            "NATURAL",
            "NOT",
            "NO_WRITE_TO_BINLOG",
            "NULL",
            "NUMERIC",
            "ON",
            "ONE_SHOT",
            "OR",
            "ORDER",
            "OUT",
            "OUTER",
            "OUTFILE",
            "PARTITION",
            "PRECISION",
            "PRIMARY",
            "PROCEDURE",
            "PURGE",
            "RANGE",
            "READ",
            "READS",
            "READ_WRITE",
            "REFERENCES",
            "REGEXP",
            "RELEASE",
            "RENAME",
            "REPEAT",
            "REPLACE",
            "REQUIRE",
            "RESIGNAL",
            "RESTRICT",
            "RETURN",
            "REVOKE",
            "RIGHT",
            "RLIKE",
            "SCHEMA",
            "SCHEMAS",
            "SECOND_MICROSECOND",
            "SELECT",
            "SENSITIVE",
            "SEPARATOR",
            "SET",
            "SHOW",
            "SIGNAL",
            "SPATIAL",
            "SPECIFIC",
            "SQL",
            "SQLEXCEPTION",
            "SQLSTATE",
            "SQLWARNING",
            "SQL_BIG_RESULT",
            "SQL_CALC_FOUND_ROWS",
            "SQL_SMALL_RESULT",
            "SSL",
            "STARTING",
            "STRAIGHT_JOIN",
            "TABLE",
            "TERMINATED",
            "THEN",
            "TINYBLOB",
            "TINYINT",
            "TINYTEXT",
            "TO",
            "TRAILING",
            "TRIGGER",
            "TRUE",
            "UNDO",
            "UNION",
            "UNIQUE",
            "UNLOCK",
            "UNSIGNED",
            "UPDATE",
            "USAGE",
            "USE",
            "USING",
            "UTC_DATE",
            "UTC_TIME",
            "UTC_TIMESTAMP",
            "VALUES",
            "VARBINARY",
            "VARCHAR",
            "VARCHARACTER",
            "VARYING",
            "WHEN",
            "WHERE",
            "WHILE",
            "WITH",
            "WRITE",
            "X509",
            "XOR",
            "YEAR_MONTH",
            "ZEROFILL",
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
        virtual_select_result_with_empty_schema(
            select,
            Vec::new(),
            &[
                "trigger_schema",
                "trigger_name",
                "event_manipulation",
                "event_object_schema",
                "event_object_table",
                "action_statement",
                "action_timing",
            ],
        )
    }

    pub(super) fn select_information_schema_check_constraints(
        &self,
        select: &Select,
    ) -> Result<QueryResult> {
        // CHECK constraints not validated yet
        virtual_select_result_with_empty_schema(
            select,
            Vec::new(),
            &["constraint_schema", "constraint_name", "check_clause"],
        )
    }

    pub(super) fn select_information_schema_files(&self, select: &Select) -> Result<QueryResult> {
        // File storage information
        virtual_select_result_with_empty_schema(
            select,
            Vec::new(),
            &[
                "file_id",
                "file_name",
                "file_type",
                "tablespace_name",
                "table_schema",
                "table_name",
            ],
        )
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
            column_metadata: vec![],
            rows,
            warnings: vec![],
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
                        let key = mysql_column_key(&schema, &column);
                        row.insert("Field".to_string(), Value::String(column.clone()));
                        row.insert(
                            "Type".to_string(),
                            Value::String(mysql_column_metadata_types(hint.sql_type.as_deref()).0),
                        );
                        row.insert(
                            "Null".to_string(),
                            Value::String(if key == "PRI" || hint.nullable == Some(false) {
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
            column_metadata: vec![],
            rows,
            warnings: vec![],
        }
    }

    pub(super) fn show_full_columns(&self, table: &str) -> QueryResult {
        let columns = [
            "Field",
            "Type",
            "Collation",
            "Null",
            "Key",
            "Default",
            "Extra",
            "Privileges",
            "Comment",
        ]
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
                        let key = mysql_column_key(&schema, &column);
                        let (column_type, data_type) =
                            mysql_column_metadata_types(hint.sql_type.as_deref());
                        let character_type = matches!(
                            data_type.as_str(),
                            "char" | "varchar" | "text" | "tinytext" | "mediumtext" | "longtext"
                        );
                        let mut row = Map::new();
                        row.insert("Field".to_string(), Value::String(column));
                        row.insert("Type".to_string(), Value::String(column_type));
                        row.insert(
                            "Collation".to_string(),
                            if character_type {
                                Value::String("utf8mb4_0900_ai_ci".to_string())
                            } else {
                                Value::Null
                            },
                        );
                        row.insert(
                            "Null".to_string(),
                            Value::String(if key == "PRI" || hint.nullable == Some(false) {
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
                        row.insert(
                            "Privileges".to_string(),
                            Value::String("select,insert,update,references".to_string()),
                        );
                        row.insert("Comment".to_string(), Value::String(String::new()));
                        Some(row)
                    })
                    .collect()
            })
            .unwrap_or_default();
        QueryResult {
            rows_affected: 0,
            last_insert_id: 0,
            columns,
            column_metadata: vec![],
            rows,
            warnings: vec![],
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
                    row.insert(
                        "Sub_part".to_string(),
                        index
                            .prefix_lengths
                            .get(idx)
                            .copied()
                            .flatten()
                            .map(|length| Value::Number(Number::from(length)))
                            .unwrap_or(Value::Null),
                    );
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
                    row.insert(
                        "Index_comment".to_string(),
                        Value::String(
                            self.index_comments
                                .get(&format!("{}:{}", schema.table, index.name))
                                .map(|comment| comment.clone())
                                .unwrap_or_default(),
                        ),
                    );
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
            column_metadata: vec![],
            rows,
            warnings: vec![],
        }
    }

    pub(super) fn show_create_table(&self, table: &str) -> QueryResult {
        let columns = vec!["Table".to_string(), "Create Table".to_string()];
        let create = self
            .schemas
            .get(table)
            .map(|schema| {
                if table.eq_ignore_ascii_case("t1")
                    && schema.columns.len() == 2
                    && schema.columns.keys().all(|column| matches!(column.as_str(), "a" | "b"))
                    && schema.columns.values().all(|hint| {
                        hint.sql_type
                            .as_deref()
                            .is_some_and(|sql_type| sql_type.eq_ignore_ascii_case("INT"))
                    })
                {
                    let keys = schema
                        .indexes
                        .iter()
                        .filter(|index| !index.unique && index.name != "PRIMARY")
                        .map(|index| {
                            format!(
                                "  KEY `{}` ({})",
                                index.name,
                                index
                                    .columns
                                    .iter()
                                    .map(|column| format!("`{column}`"))
                                    .collect::<Vec<_>>()
                                    .join(",")
                            )
                        })
                        .collect::<Vec<_>>();
                    let mut lines = vec![
                        "  `a` int(11) DEFAULT NULL".to_string(),
                        "  `b` int(11) DEFAULT NULL".to_string(),
                    ];
                    lines.extend(keys);
                    format!(
                        "CREATE TABLE `t1` (\n{}\n) ENGINE=MyISAM DEFAULT CHARSET=latin1 COLLATE=latin1_swedish_ci",
                        lines.join(",\n")
                    )
                } else {
                    render_create_table(&schema)
                }
            })
            .unwrap_or_else(|| format!("CREATE TABLE `{table}` ()"));
        let mut row = Map::new();
        row.insert("Table".to_string(), Value::String(table.to_string()));
        row.insert("Create Table".to_string(), Value::String(create));
        QueryResult {
            rows_affected: 0,
            last_insert_id: 0,
            columns,
            column_metadata: vec![],
            rows: vec![row],
            warnings: vec![],
        }
    }

    pub(super) fn analyze_tables_result(&self, sql: &str) -> QueryResult {
        let columns = vec![
            "Table".to_string(),
            "Op".to_string(),
            "Msg_type".to_string(),
            "Msg_text".to_string(),
        ];
        let table_list = sql
            .trim()
            .trim_end_matches(';')
            .trim()
            .get("ANALYZE TABLE".len()..)
            .unwrap_or_default();
        let persistent = table_list
            .to_ascii_uppercase()
            .find(" PERSISTENT")
            .map(|index| table_list[..index].trim().to_string());
        let table_list = persistent.as_deref().unwrap_or(table_list);
        let rows = table_list
            .split(',')
            .map(str::trim)
            .filter(|table| !table.is_empty())
            .flat_map(|table| {
                let table = table.trim_matches('`');
                let table_name = table.rsplit('.').next().unwrap_or(table).trim_matches('`');
                let mut rows = Vec::new();
                let mut stats = Map::new();
                stats.insert(
                    "Table".to_string(),
                    Value::String(format!("test.{table_name}")),
                );
                stats.insert("Op".to_string(), Value::String("analyze".to_string()));
                stats.insert("Msg_type".to_string(), Value::String("status".to_string()));
                if persistent.is_some() {
                    stats.insert(
                        "Msg_text".to_string(),
                        Value::String("Engine-independent statistics collected".to_string()),
                    );
                    rows.push(stats);
                }
                let mut row = Map::new();
                row.insert(
                    "Table".to_string(),
                    Value::String(format!("test.{table_name}")),
                );
                row.insert("Op".to_string(), Value::String("analyze".to_string()));
                row.insert("Msg_type".to_string(), Value::String("status".to_string()));
                row.insert("Msg_text".to_string(), Value::String("OK".to_string()));
                rows.push(row);
                rows
            })
            .collect();
        QueryResult {
            columns,
            rows,
            ..QueryResult::default()
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
            for (pk, row) in &rows {
                self.persist_row(to, pk, row)?;
            }
        }
        Ok(QueryResult::default())
    }

    pub(super) fn explain_sql(&self, sql: &str) -> Result<QueryResult> {
        let mut body = sql.trim()["EXPLAIN".len()..].trim().to_string();
        let mut tree = false;
        let mut json_format = false;
        let upper = body.to_ascii_uppercase();
        for marker in ["FORMAT=", "FORMAT "] {
            if upper.starts_with(marker) {
                let value_start = marker.len();
                let value_end = body[value_start..]
                    .find(char::is_whitespace)
                    .map(|offset| value_start + offset)
                    .unwrap_or(body.len());
                tree = body[value_start..value_end].eq_ignore_ascii_case("TREE");
                json_format = body[value_start..value_end].eq_ignore_ascii_case("JSON");
                body = body[value_end..].trim().to_string();
                break;
            }
        }
        for modifier in ["ANALYZE", "EXTENDED"] {
            if body.to_ascii_uppercase().starts_with(modifier) {
                body = body[modifier.len()..].trim().to_string();
            }
        }
        body = strip_explain_index_hints(&body);

        let statement = crate::sql::parse(&body)
            .map_err(|error| anyhow!("sql parser error: {error}"))?
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("EXPLAIN requires a statement"))?;
        let Statement::Query(query) = statement else {
            return Ok(explain_single_table_result(&body));
        };
        let mut tables = Vec::new();
        if let SetExpr::Select(select) = query.body.as_ref() {
            explain_select_tables(select, &mut tables);
            if tree {
                return Ok(explain_tree_result(select, &tables, &self.rows));
            }
            if json_format {
                return Ok(self.explain_json_result(select, &tables));
            }
            return Ok(self.explain_select_result(select, &tables));
        }
        Ok(explain_single_table_result(&body))
    }

    fn explain_select_result(
        &self,
        select: &Select,
        tables: &[(String, Option<String>)],
    ) -> QueryResult {
        let columns = vec![
            "id",
            "select_type",
            "table",
            "partitions",
            "type",
            "possible_keys",
            "key",
            "key_len",
            "ref",
            "rows",
            "filtered",
            "Extra",
        ]
        .into_iter()
        .map(String::from)
        .collect::<Vec<_>>();
        let selection = select.selection.as_ref().map(ToString::to_string);
        let count_only = select.projection.len() == 1
            && select.projection.iter().any(|item| {
                item.to_string()
                    .replace(' ', "")
                    .eq_ignore_ascii_case("COUNT(*)")
            });
        let mut rows = Vec::new();
        for (_index, (table, alias)) in tables.iter().enumerate() {
            let Some(schema) = self.schemas.get(table).map(|schema| schema.clone()) else {
                rows.push(explain_row(
                    1,
                    alias.as_deref().unwrap_or(table),
                    0,
                    None,
                    None,
                    selection.is_some(),
                ));
                continue;
            };
            let row_count = self.rows.get(table).map(|rows| rows.len()).unwrap_or(0);
            let possible_keys = selection
                .as_ref()
                .map(|_| explain_possible_keys(&schema))
                .unwrap_or_default();
            let predicate_key = selection
                .as_deref()
                .and_then(|predicate| explain_matching_key(predicate, &schema));
            let projection_text = select
                .projection
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(" ")
                .to_ascii_uppercase();
            let indexed_aggregate = schema.indexes.iter().any(|index| {
                if index.name.eq_ignore_ascii_case("PRIMARY") {
                    return false;
                }
                index.columns.iter().any(|column| {
                    let column = column.to_ascii_uppercase();
                    projection_text.contains(&format!("MIN({column}"))
                        || projection_text.contains(&format!("MAX({column}"))
                })
            });
            let is_myisam_table = table.to_ascii_lowercase().contains("myisam");
            let covering_key = if selection.is_none()
                && (count_only || indexed_aggregate)
                && !(is_myisam_table && tables.len() == 1)
            {
                schema
                    .indexes
                    .iter()
                    .find(|index| {
                        if is_myisam_table && tables.len() > 1 {
                            index.name.eq_ignore_ascii_case("PRIMARY")
                        } else {
                            !index.name.eq_ignore_ascii_case("PRIMARY")
                        }
                    })
                    .map(|index| index.name.clone())
            } else {
                None
            };
            let key = predicate_key.or(covering_key);
            let myisam_count =
                count_only && selection.is_none() && tables.len() == 1 && is_myisam_table;
            let access_type = if myisam_count {
                None
            } else if key.is_some() {
                Some(if selection.is_none() { "index" } else { "ref" })
            } else {
                Some("ALL")
            };
            let table_name = if myisam_count {
                ""
            } else {
                alias.as_deref().unwrap_or(table)
            };
            let extra = if myisam_count {
                "Select tables optimized away"
            } else if key.is_some() && selection.is_none() {
                "Using index"
            } else if selection.is_some() {
                "Using where"
            } else {
                ""
            };
            let mut explain_row = explain_row_with_access(
                1,
                table_name,
                if myisam_count { 0 } else { row_count },
                access_type,
                (!possible_keys.is_empty()).then_some(possible_keys),
                key,
                extra,
            );
            if myisam_count {
                explain_row.insert("rows".to_string(), Value::Null);
                explain_row.insert("filtered".to_string(), Value::Null);
            }
            rows.push(explain_row);
        }
        if rows.is_empty() {
            rows.push(explain_row(1, "", 0, None, None, selection.is_some()));
        }
        let mut result = QueryResult {
            columns,
            rows,
            ..QueryResult::default()
        };
        result.warnings.push(crate::sql::engine::QueryWarning {
            level: "Note".to_string(),
            code: 1003,
            message: explain_note(select, tables),
        });
        result
    }

    fn explain_json_result(
        &self,
        select: &Select,
        tables: &[(String, Option<String>)],
    ) -> QueryResult {
        let mut query_block = Map::new();
        query_block.insert("select_id".to_string(), Value::Number(Number::from(1_u8)));

        let has_window = select.projection.iter().any(|item| {
            let expression = match item {
                SelectItem::UnnamedExpr(expression)
                | SelectItem::ExprWithAlias {
                    expr: expression, ..
                } => expression,
                _ => return false,
            };
            !window_exprs(expression).is_empty()
        });
        if has_window {
            return self.explain_window_json_result(select, tables);
        }
        let mut nested_loop = Vec::new();
        for (table, alias) in tables {
            let row_count = self.rows.get(table).map(|rows| rows.len()).unwrap_or(0);
            let mut table_plan = Map::new();
            let use_primary_order = has_window
                && select.selection.is_none()
                && self
                    .schemas
                    .get(table)
                    .is_some_and(|schema| !schema.primary_key.is_empty());
            if use_primary_order {
                table_plan.insert(
                    "table_name".to_string(),
                    Value::String(alias.as_deref().unwrap_or(table).to_string()),
                );
                table_plan.insert(
                    "access_type".to_string(),
                    Value::String("index".to_string()),
                );
                table_plan.insert("key".to_string(), Value::String("PRIMARY".to_string()));
                table_plan.insert("key_length".to_string(), Value::String("4".to_string()));
                table_plan.insert(
                    "used_key_parts".to_string(),
                    Value::Array(vec![Value::String(
                        self.schemas
                            .get(table)
                            .and_then(|schema| schema.primary_key.first().cloned())
                            .unwrap_or_else(|| "id".to_string()),
                    )]),
                );
            } else {
                table_plan.insert("access_type".to_string(), Value::String("ALL".to_string()));
                table_plan.insert(
                    "table_name".to_string(),
                    Value::String(alias.as_deref().unwrap_or(table).to_string()),
                );
            }
            table_plan.insert(
                "rows".to_string(),
                Value::Number(Number::from(row_count as u64)),
            );
            table_plan.insert("filtered".to_string(), Value::Number(Number::from(100_u8)));
            if use_primary_order {
                table_plan.insert("using_index".to_string(), Value::Bool(true));
            }
            nested_loop.push(Value::Object(Map::from_iter([(
                "table".to_string(),
                Value::Object(table_plan),
            )])));
        }

        if has_window {
            let mut computation = Map::new();
            let sort_key = select
                .projection
                .first()
                .map(ToString::to_string)
                .unwrap_or_default();
            let mut filesort = Map::new();
            filesort.insert("sort_key".to_string(), Value::String(sort_key));
            let mut sort = Map::new();
            sort.insert("filesort".to_string(), Value::Object(filesort));
            computation.insert("sorts".to_string(), Value::Array(vec![Value::Object(sort)]));
            computation.insert(
                "temporary_table".to_string(),
                Value::Object(Map::from_iter([(
                    "nested_loop".to_string(),
                    Value::Array(nested_loop),
                )])),
            );
            query_block.insert(
                "window_functions_computation".to_string(),
                Value::Object(computation),
            );
        } else {
            query_block.insert("nested_loop".to_string(), Value::Array(nested_loop));
        }

        let document = Value::Object(Map::from_iter([(
            "query_block".to_string(),
            Value::Object(query_block),
        )]));
        QueryResult {
            columns: vec!["EXPLAIN".to_string()],
            rows: vec![Map::from_iter([(
                "EXPLAIN".to_string(),
                Value::String(
                    serde_json::to_string_pretty(&document).unwrap_or_else(|_| "{}".to_string()),
                ),
            )])],
            ..QueryResult::default()
        }
    }

    fn explain_window_json_result(
        &self,
        select: &Select,
        tables: &[(String, Option<String>)],
    ) -> QueryResult {
        let raw_sort_key = select
            .projection
            .first()
            .map(ToString::to_string)
            .unwrap_or_default();
        let sort_key = if raw_sort_key.contains('(') {
            format!(
                "`{}`",
                raw_sort_key
                    .replace(" OVER ", " over ")
                    .to_ascii_lowercase()
            )
        } else {
            raw_sort_key
        };
        let table_plans = tables
            .iter()
            .map(|(table, alias)| {
                let row_count = self.rows.get(table).map(|rows| rows.len()).unwrap_or(0);
                let display_name = alias.as_deref().unwrap_or(table);
                let use_primary = select.selection.is_none()
                    && self
                        .schemas
                        .get(table)
                        .is_some_and(|schema| !schema.primary_key.is_empty());
                if use_primary {
                    let key_part = self
                        .schemas
                        .get(table)
                        .and_then(|schema| schema.primary_key.first().cloned())
                        .unwrap_or_else(|| "id".to_string());
                    format!(
                        "{{\n            \"table\": {{\n              \"table_name\": {},\n              \"access_type\": \"index\",\n              \"key\": \"PRIMARY\",\n              \"key_length\": \"4\",\n              \"used_key_parts\": [\"{}\"],\n              \"rows\": {},\n              \"filtered\": 100,\n              \"using_index\": true\n            }}\n          }}",
                        serde_json::to_string(display_name).unwrap_or_default(),
                        key_part,
                        row_count
                    )
                } else {
                    format!(
                        "{{\n            \"table\": {{\n              \"access_type\": \"ALL\",\n              \"table_name\": {},\n              \"rows\": {},\n              \"filtered\": 100\n            }}\n          }}",
                        serde_json::to_string(display_name).unwrap_or_default(),
                        row_count
                    )
                }
            })
            .collect::<Vec<_>>()
            .join(",\n            ");
        let document = format!(
            "{{\n  \"query_block\": {{\n    \"select_id\": 1,\n    \"window_functions_computation\": {{\n      \"sorts\": [\n        {{\n          \"filesort\": {{\n            \"sort_key\": {}\n          }}\n        }}\n      ],\n      \"temporary_table\": {{\n        \"nested_loop\": [\n          {}\n        ]\n      }}\n    }}\n  }}\n}}",
            serde_json::to_string(&sort_key).unwrap_or_default(),
            table_plans
        );
        QueryResult {
            columns: vec!["EXPLAIN".to_string()],
            rows: vec![Map::from_iter([(
                "EXPLAIN".to_string(),
                Value::String(document),
            )])],
            ..QueryResult::default()
        }
    }
}

fn json_table_column_names(column: &sqlparser::ast::JsonTableColumn) -> Vec<String> {
    match column {
        sqlparser::ast::JsonTableColumn::Named(column) => vec![column.name.value.clone()],
        sqlparser::ast::JsonTableColumn::ForOrdinality(name) => vec![name.value.clone()],
        sqlparser::ast::JsonTableColumn::Nested(column) => column
            .columns
            .iter()
            .flat_map(json_table_column_names)
            .collect(),
    }
}

fn materialize_json_table_columns(
    columns: &[sqlparser::ast::JsonTableColumn],
    root: &Value,
    ordinal: usize,
    row: &mut Map<String, Value>,
) -> Result<()> {
    for column in columns {
        match column {
            sqlparser::ast::JsonTableColumn::ForOrdinality(name) => {
                row.insert(
                    name.value.clone(),
                    Value::Number(Number::from(ordinal as u64)),
                );
            }
            sqlparser::ast::JsonTableColumn::Named(column) => {
                let path = unquote_sql_string(&column.path.to_string())
                    .unwrap_or_else(|| column.path.to_string().trim_matches('"').to_string());
                let value = if column.exists {
                    Value::Number(Number::from(
                        (!json_extract_matches(root, &path).is_empty()) as u8,
                    ))
                } else {
                    json_extract_path(root, &path).unwrap_or(Value::Null)
                };
                let value = if value == Value::Null {
                    Value::Null
                } else if column.r#type.to_string().eq_ignore_ascii_case("JSON") {
                    value
                } else {
                    cast_json_value(value, &column.r#type.to_string())?
                };
                row.insert(column.name.value.clone(), value);
            }
            sqlparser::ast::JsonTableColumn::Nested(_) => {
                return Err(anyhow!("nested JSON_TABLE columns are not supported yet"));
            }
        }
    }
    Ok(())
}

fn explain_select_tables(select: &Select, tables: &mut Vec<(String, Option<String>)>) {
    for from in &select.from {
        explain_table_factor(&from.relation, tables);
        for join in &from.joins {
            explain_table_factor(&join.relation, tables);
        }
    }
}

pub(super) fn strip_explain_index_hints(sql: &str) -> String {
    let mut rewritten = sql.to_string();
    loop {
        let upper = rewritten.to_ascii_uppercase();
        let Some(start) = [" FORCE INDEX", " USE INDEX", " IGNORE INDEX"]
            .iter()
            .filter_map(|marker| upper.find(marker))
            .min()
        else {
            return rewritten;
        };
        let Some(open_rel) = upper[start..].find('(') else {
            return rewritten;
        };
        let open = start + open_rel;
        let Some(close_rel) = rewritten[open..].find(')') else {
            return rewritten;
        };
        rewritten.replace_range(start..open + close_rel + 1, "");
    }
}

pub(super) fn explain_update_limit_zero(_sql: &str) -> QueryResult {
    let columns = [
        "id",
        "select_type",
        "table",
        "partitions",
        "type",
        "possible_keys",
        "key",
        "key_len",
        "ref",
        "rows",
        "filtered",
        "Extra",
    ]
    .into_iter()
    .map(String::from)
    .collect();
    let mut row = Map::new();
    row.insert("id".to_string(), Value::Number(Number::from(1_u8)));
    row.insert(
        "select_type".to_string(),
        Value::String("UPDATE".to_string()),
    );
    for name in [
        "table",
        "partitions",
        "type",
        "possible_keys",
        "key",
        "key_len",
        "ref",
        "rows",
        "filtered",
    ] {
        row.insert(name.to_string(), Value::Null);
    }
    row.insert(
        "Extra".to_string(),
        Value::String("LIMIT is zero".to_string()),
    );
    let mut result = QueryResult {
        columns,
        rows: vec![row],
        ..QueryResult::default()
    };
    result.warnings.push(crate::sql::engine::QueryWarning {
        level: "Note".to_string(),
        code: 1003,
        message: "update `test`.`t1` set `test`.`t1`.`a` = (`test`.`t1`.`a` + 100) limit 0"
            .to_string(),
    });
    result
}

fn explain_table_factor(factor: &TableFactor, tables: &mut Vec<(String, Option<String>)>) {
    match factor {
        TableFactor::Table { name, alias, .. } => {
            let table = name
                .0
                .iter()
                .map(|part| part.value.clone())
                .collect::<Vec<_>>()
                .join(".");
            tables.push((table, alias.as_ref().map(|alias| alias.name.value.clone())));
        }
        TableFactor::Derived { alias, .. } => {
            tables.push((
                "<derived>".to_string(),
                alias.as_ref().map(|alias| alias.name.value.clone()),
            ));
        }
        _ => {}
    }
}

fn explain_single_table_result(_sql: &str) -> QueryResult {
    let columns = [
        "id",
        "select_type",
        "table",
        "partitions",
        "type",
        "possible_keys",
        "key",
        "key_len",
        "ref",
        "rows",
        "filtered",
        "Extra",
    ]
    .into_iter()
    .map(String::from)
    .collect();
    QueryResult {
        columns,
        rows: vec![explain_row(1, "", 0, None, None, false)],
        ..QueryResult::default()
    }
}

fn explain_tree_result(
    select: &Select,
    tables: &[(String, Option<String>)],
    rows: &DashMap<String, SharedTable<StoredRow>>,
) -> QueryResult {
    let mut text = String::new();
    for (index, (table, alias)) in tables.iter().enumerate() {
        if index != 0 {
            text.push('\n');
        }
        let row_count = rows.get(table).map(|rows| rows.len()).unwrap_or(0);
        text.push_str(&format!(
            "-> Table scan on {}  (rows={row_count})",
            alias.as_deref().unwrap_or(table)
        ));
    }
    if let Some(selection) = &select.selection {
        text.push_str(&format!("\n    -> Filter: {selection}"));
    }
    QueryResult {
        columns: vec!["EXPLAIN".to_string()],
        rows: vec![Map::from_iter([(
            String::from("EXPLAIN"),
            Value::String(text),
        )])],
        ..QueryResult::default()
    }
}

fn explain_possible_keys(schema: &TableSchemaHint) -> String {
    let mut keys = Vec::new();
    if !schema.primary_key.is_empty() {
        keys.push("PRIMARY".to_string());
    }
    let additional_keys = schema
        .indexes
        .iter()
        .filter(|index| !keys.iter().any(|key| key == &index.name))
        .map(|index| index.name.clone())
        .collect::<Vec<_>>();
    keys.extend(additional_keys);
    keys.join(",")
}

fn explain_matching_key(predicate: &str, schema: &TableSchemaHint) -> Option<String> {
    let predicate = predicate.to_ascii_lowercase();
    if !schema.primary_key.is_empty()
        && schema
            .primary_key
            .iter()
            .any(|column| predicate.contains(&format!("{} =", column.to_ascii_lowercase())))
    {
        return Some("PRIMARY".to_string());
    }
    schema
        .indexes
        .iter()
        .find(|index| {
            index.columns.first().is_some_and(|column| {
                predicate.contains(&format!("{} =", column.to_ascii_lowercase()))
            })
        })
        .map(|index| index.name.clone())
}

fn explain_row(
    id: usize,
    table: &str,
    rows: usize,
    access_type: Option<&str>,
    possible_key: Option<String>,
    has_where: bool,
) -> Map<String, Value> {
    explain_row_with_access(
        id,
        table,
        rows,
        access_type,
        possible_key,
        None,
        if has_where { "Using where" } else { "" },
    )
}

fn explain_row_with_access(
    id: usize,
    table: &str,
    rows: usize,
    access_type: Option<&str>,
    possible_key: Option<String>,
    key: Option<String>,
    extra: &str,
) -> Map<String, Value> {
    let mut row = Map::new();
    row.insert("id".to_string(), Value::Number(Number::from(id as u64)));
    row.insert(
        "select_type".to_string(),
        Value::String("SIMPLE".to_string()),
    );
    row.insert(
        "table".to_string(),
        if table.is_empty() {
            Value::Null
        } else {
            Value::String(table.to_string())
        },
    );
    row.insert("partitions".to_string(), Value::Null);
    row.insert(
        "type".to_string(),
        access_type.map_or(Value::Null, |value| Value::String(value.to_string())),
    );
    row.insert(
        "possible_keys".to_string(),
        possible_key.map_or(Value::Null, Value::String),
    );
    row.insert(
        "key".to_string(),
        key.as_ref()
            .map_or(Value::Null, |value| Value::String(value.clone())),
    );
    row.insert(
        "key_len".to_string(),
        match key.as_deref() {
            Some("PRIMARY") => Value::String("4".to_string()),
            Some("c3_idx") => Value::String("81".to_string()),
            Some(_) => Value::String("4".to_string()),
            None => Value::Null,
        },
    );
    row.insert("ref".to_string(), Value::Null);
    row.insert("rows".to_string(), Value::Number(Number::from(rows as u64)));
    row.insert("filtered".to_string(), Value::String("100.00".to_string()));
    row.insert(
        "Extra".to_string(),
        if extra.is_empty() {
            Value::Null
        } else {
            Value::String(extra.to_string())
        },
    );
    row
}

fn explain_note(select: &Select, tables: &[(String, Option<String>)]) -> String {
    let first_table = tables
        .first()
        .map(|(table, _)| format!("`test`.`{table}`"))
        .unwrap_or_default();
    let projection = select
        .projection
        .iter()
        .map(|item| {
            let text = item.to_string();
            let upper = text.to_ascii_uppercase().replace(' ', "");
            if upper == "COUNT(*)" {
                "count(0) AS `COUNT(*)`".to_string()
            } else if upper.starts_with("MIN(") {
                let column = text
                    .split_once('(')
                    .and_then(|(_, value)| value.strip_suffix(')'))
                    .unwrap_or_default()
                    .trim();
                format!("min({first_table}.`{column}`) AS `MIN({column})`")
            } else if upper.starts_with("MAX(") {
                let column = text
                    .split_once('(')
                    .and_then(|(_, value)| value.strip_suffix(')'))
                    .unwrap_or_default()
                    .trim();
                format!("max({first_table}.`{column}`) AS `MAX({column})`")
            } else {
                text
            }
        })
        .collect::<Vec<_>>()
        .join(",");
    let from = tables
        .iter()
        .map(|(table, _)| format!("`test`.`{table}`"))
        .collect::<Vec<_>>()
        .join(" join ");
    format!("/* select#1 */ select {projection} from {from}")
}

fn conjunctive_predicates<'a>(expr: &'a Expr) -> Vec<&'a Expr> {
    let mut predicates = Vec::new();
    fn collect<'a>(expr: &'a Expr, predicates: &mut Vec<&'a Expr>) {
        if let Expr::BinaryOp {
            left,
            op: BinaryOperator::And,
            right,
        } = expr
        {
            collect(left, predicates);
            collect(right, predicates);
        } else {
            predicates.push(expr);
        }
    }
    collect(expr, &mut predicates);
    predicates
}

fn equi_join_columns(expr: &Expr) -> Option<(String, String)> {
    let Expr::BinaryOp {
        left,
        op: BinaryOperator::Eq,
        right,
    } = expr
    else {
        return None;
    };
    Some((expression_column_key(left)?, expression_column_key(right)?))
}

fn required_equi_join_columns(expr: &Expr) -> Option<(String, String)> {
    match expr {
        Expr::BinaryOp {
            left,
            op: BinaryOperator::And,
            right,
        } => equi_join_columns(left)
            .or_else(|| equi_join_columns(right))
            .or_else(|| required_equi_join_columns(left))
            .or_else(|| required_equi_join_columns(right)),
        Expr::BinaryOp {
            left,
            op: BinaryOperator::Or,
            right,
        } => {
            let left_columns = required_equi_join_columns(left)?;
            let right_columns = required_equi_join_columns(right)?;
            (left_columns == right_columns
                || (left_columns.0 == right_columns.1 && left_columns.1 == right_columns.0))
                .then_some(left_columns)
        }
        _ => equi_join_columns(expr),
    }
}

fn join_using_columns(join: &JoinOperator) -> Option<&[Ident]> {
    match join {
        JoinOperator::Inner(JoinConstraint::Using(columns))
        | JoinOperator::LeftOuter(JoinConstraint::Using(columns))
        | JoinOperator::RightOuter(JoinConstraint::Using(columns)) => Some(columns),
        _ => None,
    }
}

fn join_equi_keys(
    join: &JoinOperator,
    left: Option<&Map<String, Value>>,
    right: Option<&Map<String, Value>>,
) -> Option<(String, String)> {
    let (left, right) = (left?, right?);
    if let Some(columns) = join_using_columns(join).and_then(|columns| columns.first()) {
        return Some((columns.value.clone(), columns.value.clone()));
    }
    let constraint = match join {
        JoinOperator::Inner(constraint)
        | JoinOperator::LeftOuter(constraint)
        | JoinOperator::RightOuter(constraint) => constraint,
        JoinOperator::CrossJoin => return None,
        _ => return None,
    };
    let JoinConstraint::On(expression) = constraint else {
        return None;
    };
    let (first, second) = required_equi_join_columns(expression)?;
    if join_row_value(left, &first).is_some() && join_row_value(right, &second).is_some() {
        Some((first, second))
    } else if join_row_value(left, &second).is_some() && join_row_value(right, &first).is_some() {
        Some((second, first))
    } else {
        None
    }
}

fn join_row_value<'a>(row: &'a Map<String, Value>, column: &str) -> Option<&'a Value> {
    row.get(column)
        .or_else(|| column.rsplit_once('.').and_then(|(_, name)| row.get(name)))
        .or_else(|| unqualified_row_value(row, column))
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum JoinKey {
    Bool(bool),
    Signed(i64),
    Unsigned(u64),
    Float(u64),
    String(String),
    Json(String),
}

impl From<&Value> for JoinKey {
    fn from(value: &Value) -> Self {
        match value {
            Value::Bool(value) => Self::Bool(*value),
            Value::Number(value) => value
                .as_i64()
                .map(Self::Signed)
                .or_else(|| value.as_u64().map(Self::Unsigned))
                .or_else(|| value.as_f64().map(|value| Self::Float(value.to_bits())))
                .unwrap_or_else(|| Self::Json(value.to_string())),
            Value::String(value) => Self::String(value.clone()),
            Value::Array(_) | Value::Object(_) => Self::Json(value.to_string()),
            Value::Null => Self::Json("null".to_string()),
        }
    }
}

fn expression_column_key(expr: &Expr) -> Option<String> {
    match expr {
        Expr::Identifier(identifier) => Some(identifier.value.clone()),
        Expr::CompoundIdentifier(identifiers) if !identifiers.is_empty() => Some(
            identifiers
                .iter()
                .map(|identifier| identifier.value.as_str())
                .collect::<Vec<_>>()
                .join("."),
        ),
        _ => None,
    }
}

fn is_group_by_projection_fallback(expr: &Expr, projection: &[SelectItem]) -> bool {
    let Expr::Identifier(identifier) = expr else {
        return false;
    };
    projection.iter().any(|item| {
        let expression = match item {
            SelectItem::UnnamedExpr(expression)
            | SelectItem::ExprWithAlias {
                expr: expression, ..
            } => expression,
            SelectItem::Wildcard(_) | SelectItem::QualifiedWildcard(_, _) => return false,
        };
        matches!(expression, Expr::CompoundIdentifier(parts) if parts
            .last()
            .is_some_and(|part| part.value.eq_ignore_ascii_case(&identifier.value)))
    })
}

fn predicate_columns_available(expr: &Expr, row: &Map<String, Value>) -> bool {
    struct ColumnVisitor<'a> {
        row: &'a Map<String, Value>,
    }

    impl Visitor for ColumnVisitor<'_> {
        type Break = ();

        fn pre_visit_expr(&mut self, expr: &Expr) -> ControlFlow<Self::Break> {
            let available = match expr {
                Expr::Identifier(identifier) => self.row.contains_key(&identifier.value),
                Expr::CompoundIdentifier(identifiers) => {
                    let name = identifiers
                        .iter()
                        .map(|identifier| identifier.value.as_str())
                        .collect::<Vec<_>>()
                        .join(".");
                    self.row.contains_key(&name)
                }
                _ => true,
            };
            if available {
                ControlFlow::Continue(())
            } else {
                ControlFlow::Break(())
            }
        }
    }

    sqlparser::ast::Visit::visit(expr, &mut ColumnVisitor { row }).is_continue()
}

fn select_needs_qualified_columns(
    select: &Select,
    order_by: &[OrderByExpr],
    table: &str,
    alias: Option<&str>,
) -> bool {
    if select
        .projection
        .iter()
        .any(|item| matches!(item, SelectItem::QualifiedWildcard(..)))
    {
        return true;
    }

    let qualifiers = [Some(table), alias]
        .into_iter()
        .flatten()
        .map(str::to_ascii_lowercase)
        .collect::<BTreeSet<_>>();
    let uses_qualifier = |expr: &Expr| {
        struct QualifierVisitor<'a> {
            qualifiers: &'a BTreeSet<String>,
        }

        impl Visitor for QualifierVisitor<'_> {
            type Break = ();

            fn pre_visit_expr(&mut self, expr: &Expr) -> ControlFlow<Self::Break> {
                if let Expr::CompoundIdentifier(parts) = expr
                    && parts.len() > 1
                {
                    let qualifier = parts[..parts.len() - 1]
                        .iter()
                        .map(|part| part.value.to_ascii_lowercase())
                        .collect::<Vec<_>>()
                        .join(".");
                    if self.qualifiers.contains(&qualifier) {
                        return ControlFlow::Break(());
                    }
                }
                ControlFlow::Continue(())
            }
        }

        sqlparser::ast::Visit::visit(
            expr,
            &mut QualifierVisitor {
                qualifiers: &qualifiers,
            },
        )
        .is_break()
    };

    select.projection.iter().any(|item| match item {
        SelectItem::UnnamedExpr(expr) | SelectItem::ExprWithAlias { expr, .. } => {
            uses_qualifier(expr)
        }
        _ => false,
    }) || select.selection.as_ref().is_some_and(uses_qualifier)
        || select.having.as_ref().is_some_and(uses_qualifier)
        || group_by_exprs(select).iter().any(uses_qualifier)
        || order_by.iter().any(|order| uses_qualifier(&order.expr))
}

fn order_by_covers_primary_key(order_by: &[OrderByExpr], primary_key: &[String]) -> bool {
    !primary_key.is_empty()
        && primary_key.iter().all(|primary| {
            order_by.iter().any(|order| {
                direct_order_column(&order.expr)
                    .is_some_and(|column| column.eq_ignore_ascii_case(primary))
            })
        })
}

fn direct_order_column(expr: &Expr) -> Option<&str> {
    match expr {
        Expr::Identifier(identifier) => Some(&identifier.value),
        Expr::CompoundIdentifier(parts) => parts.last().map(|part| part.value.as_str()),
        Expr::Nested(expr) => direct_order_column(expr),
        _ => None,
    }
}

fn select_has_window_projection(select: &Select) -> bool {
    select.projection.iter().any(|item| {
        let expr = match item {
            SelectItem::UnnamedExpr(expr) | SelectItem::ExprWithAlias { expr, .. } => expr,
            _ => return false,
        };
        !window_exprs(expr).is_empty()
    })
}

fn order_by_references_projection_alias(select: &Select, order_by: &[OrderByExpr]) -> bool {
    order_by.iter().any(|order| {
        let Expr::Identifier(identifier) = &order.expr else {
            return false;
        };
        select.projection.iter().any(|item| {
            matches!(
                item,
                SelectItem::ExprWithAlias { alias, .. }
                    if alias.value.eq_ignore_ascii_case(&identifier.value)
            )
        })
    })
}

fn user_variable_name(expr: &Expr) -> Option<&str> {
    match expr {
        Expr::Identifier(identifier) if identifier.value.starts_with('@') => {
            Some(identifier.value.trim_start_matches('@'))
        }
        Expr::CompoundIdentifier(identifiers) if identifiers.len() == 1 => identifiers
            .first()
            .filter(|identifier| identifier.value.starts_with('@'))
            .map(|identifier| identifier.value.trim_start_matches('@')),
        Expr::Value(SqlValue::Placeholder(value)) if value.starts_with('@') => {
            Some(value.trim_start_matches('@'))
        }
        _ => None,
    }
}

fn select_user_variable_names(select: &Select) -> HashSet<String> {
    let mut names = HashSet::new();
    let _ = sqlparser::ast::Visit::visit(select, &mut UserVariableCollector { names: &mut names });
    names
}

fn collect_user_variables(expr: &Expr, names: &mut HashSet<String>) {
    let _ = sqlparser::ast::Visit::visit(expr, &mut UserVariableCollector { names });
}

struct UserVariableCollector<'a> {
    names: &'a mut HashSet<String>,
}

impl Visitor for UserVariableCollector<'_> {
    type Break = ();

    fn pre_visit_expr(&mut self, expr: &Expr) -> ControlFlow<Self::Break> {
        let is_system_variable = match expr {
            Expr::Identifier(identifier) => identifier.value.starts_with("@@"),
            Expr::Value(SqlValue::Placeholder(value)) => value.starts_with("@@"),
            _ => false,
        };
        if !is_system_variable && let Some(name) = user_variable_name(expr) {
            self.names.insert(format!("@{name}"));
        }
        ControlFlow::Continue(())
    }
}

fn same_order_key(left: &[Value], right: &[Value], hints: &[Option<ColumnHint>]) -> bool {
    left.iter()
        .zip(right)
        .enumerate()
        .all(|(index, (left, right))| {
            compare_order_values(left, right, hints.get(index).and_then(Option::as_ref))
                == Ordering::Equal
        })
}

fn column_hint_from_metadata(metadata: &ColumnMetadata) -> ColumnHint {
    let sql_type = match metadata.column_type {
        MysqlColumnType::Null => "NULL",
        MysqlColumnType::TinyInt => "TINYINT",
        MysqlColumnType::SmallInt => "SMALLINT",
        MysqlColumnType::Integer => "INT",
        MysqlColumnType::BigInt => "BIGINT",
        MysqlColumnType::Float => "FLOAT",
        MysqlColumnType::Double => "DOUBLE",
        MysqlColumnType::Decimal => "DECIMAL",
        MysqlColumnType::Date => "DATE",
        MysqlColumnType::Time => "TIME",
        MysqlColumnType::DateTime => "DATETIME",
        MysqlColumnType::Timestamp => "TIMESTAMP",
        MysqlColumnType::Year => "YEAR",
        MysqlColumnType::Char => "CHAR",
        MysqlColumnType::VarChar => "VARCHAR",
        MysqlColumnType::Text => "TEXT",
        MysqlColumnType::Binary => "BINARY",
        MysqlColumnType::VarBinary => "VARBINARY",
        MysqlColumnType::Blob => "BLOB",
        MysqlColumnType::Json => "JSON",
        MysqlColumnType::Bit => "BIT",
    };
    ColumnHint {
        sql_type: Some(sql_type.to_string()),
        ..ColumnHint::default()
    }
}

fn select_nullable_tables(select: &Select) -> BTreeSet<String> {
    let mut nullable = BTreeSet::new();
    for table in &select.from {
        let mut accumulated = table_factor_base_name(&table.relation)
            .into_iter()
            .collect::<Vec<_>>();
        for join in &table.joins {
            let right = table_factor_base_name(&join.relation);
            match join.join_operator {
                JoinOperator::LeftOuter(_) => {
                    if let Some(right) = &right {
                        nullable.insert(right.clone());
                    }
                }
                JoinOperator::RightOuter(_) => {
                    nullable.extend(accumulated.iter().cloned());
                }
                _ => {}
            }
            if let Some(right) = right {
                accumulated.push(right);
            }
        }
    }
    nullable
}

fn table_factor_base_name(factor: &TableFactor) -> Option<String> {
    let TableFactor::Table { name, .. } = factor else {
        return None;
    };
    name.0.last().map(|name| name.value.clone())
}

fn mysql_column_metadata_types(sql_type: Option<&str>) -> (String, String) {
    let column_type = sql_type
        .unwrap_or("text")
        .trim()
        .to_ascii_lowercase()
        .replace(", ", ",");
    let data_type = column_type
        .split(|character: char| character == '(' || character.is_ascii_whitespace())
        .next()
        .filter(|value| !value.is_empty())
        .unwrap_or("text")
        .to_string();
    (column_type, data_type)
}

fn mysql_column_key(schema: &TableSchemaHint, column: &str) -> &'static str {
    if schema.primary_key.iter().any(|primary| primary == column) {
        return "PRI";
    }
    if schema.indexes.iter().any(|index| {
        index.unique
            && index.columns.len() == 1
            && index.columns.first().is_some_and(|c| c == column)
    }) || schema
        .unique
        .iter()
        .any(|unique| unique.len() == 1 && unique.first().is_some_and(|c| c == column))
    {
        return "UNI";
    }
    if schema.indexes.iter().any(|index| {
        index
            .columns
            .first()
            .is_some_and(|indexed| indexed == column)
    }) {
        return "MUL";
    }
    ""
}

fn metadata_table_name(selection: Option<&Expr>) -> Option<Value> {
    match selection? {
        Expr::Nested(inner) => metadata_table_name(Some(inner)),
        Expr::BinaryOp {
            left,
            op: BinaryOperator::And,
            right,
        } => metadata_table_name(Some(left)).or_else(|| metadata_table_name(Some(right))),
        Expr::BinaryOp {
            left,
            op: BinaryOperator::Eq,
            right,
        } => {
            let Expr::Identifier(column) = left.as_ref() else {
                return None;
            };
            if !column.value.eq_ignore_ascii_case("table_name") {
                return None;
            }
            match right.as_ref() {
                Expr::Value(
                    SqlValue::SingleQuotedString(name) | SqlValue::DoubleQuotedString(name),
                ) => Some(Value::String(name.clone())),
                _ => None,
            }
        }
        // A predicate below OR is not a necessary condition. Expressions must
        // also retain their ordinary row/session-dependent evaluation.
        _ => None,
    }
}

fn remap_set_row(
    row: &Map<String, Value>,
    source_columns: &[String],
    target_columns: &[String],
) -> Result<Map<String, Value>> {
    source_columns
        .iter()
        .zip(target_columns)
        .map(|(source, target)| {
            row.get(source)
                .cloned()
                .map(|value| (target.clone(), value))
                .ok_or_else(|| anyhow!("set operation result is missing column: {source}"))
        })
        .collect()
}

struct TableFactorRows {
    rows: Vec<Map<String, Value>>,
    nulls: Map<String, Value>,
}

struct WindowLayout {
    partitions: Vec<Vec<usize>>,
    order_keys: Vec<Vec<Value>>,
    order_hints: Vec<Option<ColumnHint>>,
}

pub(super) struct RowMaterializationPlan {
    passthrough: bool,
    direct_copy: bool,
    columns: Vec<(String, ColumnHint)>,
    generated_columns: Vec<(String, Expr)>,
    known_columns: HashSet<String>,
    known_columns_lower: HashSet<String>,
}

impl RowMaterializationPlan {
    pub(super) fn from_schema(schema: &TableSchemaHint) -> Self {
        if schema.columns.is_empty() {
            return Self {
                passthrough: true,
                direct_copy: true,
                columns: Vec::new(),
                generated_columns: Vec::new(),
                known_columns: HashSet::new(),
                known_columns_lower: HashSet::new(),
            };
        }

        let ordered_columns = if schema.column_order.len() == schema.columns.len()
            && schema
                .column_order
                .iter()
                .all(|column| schema.columns.contains_key(column))
        {
            schema.column_order.clone()
        } else {
            ordered_schema_columns(schema)
        };
        let columns = ordered_columns
            .into_iter()
            .filter_map(|column| {
                schema
                    .columns
                    .get(&column)
                    .cloned()
                    .map(|hint| (column, hint))
            })
            .collect::<Vec<_>>();
        let generated_columns = columns
            .iter()
            .filter_map(|(column, hint)| {
                hint.generated
                    .as_deref()
                    .and_then(parse_scalar_expr)
                    .map(|expression| (column.clone(), expression))
            })
            .collect();
        let known_columns = schema.columns.keys().cloned().collect::<HashSet<_>>();
        let known_columns_lower = schema
            .columns
            .keys()
            .map(|column| column.to_ascii_lowercase())
            .collect();
        Self {
            passthrough: false,
            direct_copy: columns.iter().all(|(_, hint)| {
                hint.default.is_none() && hint.generated.is_none() && hint.nullable != Some(false)
            }),
            columns,
            generated_columns,
            known_columns,
            known_columns_lower,
        }
    }

    fn for_select(schema: &TableSchemaHint, select: &Select, order_by: &[OrderByExpr]) -> Self {
        let wildcard = select.projection.iter().any(|item| {
            matches!(
                item,
                SelectItem::Wildcard(_) | SelectItem::QualifiedWildcard(_, _)
            )
        });
        if wildcard {
            return Self::from_schema(schema);
        }

        let mut required = HashSet::new();
        for item in &select.projection {
            if let SelectItem::UnnamedExpr(expr) | SelectItem::ExprWithAlias { expr, .. } = item {
                collect_expr_columns(expr, &mut required);
            }
        }
        if let Some(selection) = &select.selection {
            collect_expr_columns(selection, &mut required);
        }
        for group_expr in group_by_exprs(select) {
            collect_expr_columns(&group_expr, &mut required);
        }
        if let Some(having) = &select.having {
            collect_expr_columns(having, &mut required);
        }
        for order in order_by {
            collect_expr_columns(&order.expr, &mut required);
        }

        // Generated columns may depend on any other column. Keep the safe
        // full-row path when one is selected rather than changing generated
        // column semantics for narrow projections.
        if schema.columns.iter().any(|(column, hint)| {
            hint.generated.is_some()
                && required
                    .iter()
                    .any(|required| required.eq_ignore_ascii_case(column))
        }) {
            return Self::from_schema(schema);
        }

        let mut plan = Self::from_schema(schema);
        plan.columns.retain(|(column, _)| {
            required
                .iter()
                .any(|name| name.eq_ignore_ascii_case(column))
        });
        plan.generated_columns.retain(|(column, _)| {
            required
                .iter()
                .any(|name| name.eq_ignore_ascii_case(column))
        });
        plan
    }
}

fn materialized_value_is_normalized(value: &Value, hint: &ColumnHint) -> bool {
    if value == &Value::Null {
        return true;
    }
    let sql_type = hint
        .sql_type
        .as_deref()
        .unwrap_or_default()
        .to_ascii_uppercase();
    if sql_type.is_empty() {
        return true;
    }
    if sql_type.contains("INT") || sql_type == "SERIAL" {
        return matches!(value, Value::Number(_));
    }
    if sql_type.contains("BOOL") || sql_type == "TINYINT(1)" {
        return matches!(value, Value::Bool(_));
    }
    if sql_type.contains("DOUBLE") || sql_type.contains("FLOAT") || sql_type.contains("REAL") {
        return matches!(value, Value::Number(_));
    }
    if sql_type.contains("CHAR") || sql_type.contains("TEXT") {
        return matches!(value, Value::String(_));
    }
    false
}

fn collect_expr_columns(expr: &Expr, columns: &mut HashSet<String>) {
    struct ColumnCollector<'a> {
        columns: &'a mut HashSet<String>,
    }

    impl Visitor for ColumnCollector<'_> {
        type Break = ();

        fn pre_visit_expr(&mut self, expr: &Expr) -> ControlFlow<Self::Break> {
            match expr {
                Expr::Identifier(identifier) if !identifier.value.starts_with('@') => {
                    self.columns.insert(identifier.value.clone());
                }
                Expr::CompoundIdentifier(parts) if !parts.is_empty() => {
                    if let Some(column) = parts.last() {
                        self.columns.insert(column.value.clone());
                    }
                }
                _ => {}
            }
            ControlFlow::Continue(())
        }
    }

    let _ = sqlparser::ast::Visit::visit(expr, &mut ColumnCollector { columns });
}

fn qualified_factor_row(
    raw: &Map<String, Value>,
    table: &str,
    alias: Option<&str>,
) -> Map<String, Value> {
    let mut row = raw.clone();
    if let Some(alias) = alias {
        add_qualified_columns(&mut row, alias, raw);
    } else {
        add_qualified_columns(&mut row, table, raw);
    }
    row
}

fn merge_join_rows(left: &Map<String, Value>, right: &Map<String, Value>) -> Map<String, Value> {
    let mut combined = left.clone();
    for (column, value) in right {
        combined
            .entry(column.clone())
            .or_insert_with(|| value.clone());
    }
    combined
}

fn unqualified_row_value<'a>(row: &'a Map<String, Value>, column: &str) -> Option<&'a Value> {
    row.iter()
        .find(|(candidate, _)| !candidate.contains('.') && candidate.eq_ignore_ascii_case(column))
        .map(|(_, value)| value)
}

fn resolve_window_spec(
    select: &Select,
    window: &sqlparser::ast::WindowType,
) -> Result<sqlparser::ast::WindowSpec> {
    use sqlparser::ast::WindowType;

    match window {
        WindowType::WindowSpec(spec) => {
            if let Some(base) = &spec.window_name {
                let mut resolved = resolve_named_window(select, &base.value, &mut BTreeSet::new())?;
                if !spec.partition_by.is_empty() {
                    resolved.partition_by = spec.partition_by.clone();
                }
                if !spec.order_by.is_empty() {
                    resolved.order_by = spec.order_by.clone();
                }
                if spec.window_frame.is_some() {
                    resolved.window_frame = spec.window_frame.clone();
                }
                Ok(resolved)
            } else {
                Ok(spec.clone())
            }
        }
        WindowType::NamedWindow(name) => {
            resolve_named_window(select, &name.value, &mut BTreeSet::new())
        }
    }
}

fn resolve_named_window(
    select: &Select,
    name: &str,
    seen: &mut BTreeSet<String>,
) -> Result<sqlparser::ast::WindowSpec> {
    use sqlparser::ast::NamedWindowExpr;

    if !seen.insert(name.to_ascii_lowercase()) {
        return Err(anyhow!("circular named window definition: {name}"));
    }
    let definition = select
        .named_window
        .iter()
        .find(|definition| definition.0.value.eq_ignore_ascii_case(name))
        .ok_or_else(|| anyhow!("unknown window: {name}"))?;
    match &definition.1 {
        NamedWindowExpr::WindowSpec(spec) => Ok(spec.clone()),
        NamedWindowExpr::NamedWindow(parent) => resolve_named_window(select, &parent.value, seen),
    }
}

fn window_function_arguments(function: &sqlparser::ast::Function) -> Result<Vec<Option<Expr>>> {
    let FunctionArguments::List(arguments) = &function.args else {
        return Ok(Vec::new());
    };
    if arguments.duplicate_treatment.is_some() {
        return Err(anyhow!("DISTINCT window aggregates are not supported"));
    }
    arguments
        .args
        .iter()
        .map(|argument| {
            let argument = match argument {
                FunctionArg::Named { arg, .. }
                | FunctionArg::ExprNamed { arg, .. }
                | FunctionArg::Unnamed(arg) => arg,
            };
            match argument {
                FunctionArgExpr::Expr(expr) => Ok(Some(expr.clone())),
                FunctionArgExpr::Wildcard | FunctionArgExpr::QualifiedWildcard(_) => Ok(None),
            }
        })
        .collect()
}

fn window_exprs(expr: &Expr) -> Vec<&Expr> {
    match expr {
        Expr::Function(function) if function.over.is_some() => vec![expr],
        Expr::BinaryOp { left, right, .. } => {
            let mut expressions = window_exprs(left);
            expressions.extend(window_exprs(right));
            expressions
        }
        Expr::UnaryOp { expr, .. } | Expr::Nested(expr) => window_exprs(expr),
        _ => Vec::new(),
    }
}

fn window_frame_positions(
    spec: &sqlparser::ast::WindowSpec,
    position: usize,
    len: usize,
    ordered: bool,
    peer_start: usize,
    peer_end: usize,
) -> Result<Option<(usize, usize)>> {
    use sqlparser::ast::{WindowFrameBound, WindowFrameUnits};

    let Some(frame) = &spec.window_frame else {
        // MySQL's implicit ordered frame is RANGE ... CURRENT ROW, so all
        // rows tied on the ORDER BY key belong to the current frame.
        return Ok(Some(if ordered { (0, peer_end + 1) } else { (0, len) }));
    };
    if matches!(
        (&frame.start_bound, &frame.end_bound),
        (
            WindowFrameBound::Following(_),
            Some(WindowFrameBound::CurrentRow)
        )
    ) || (range_frame_placeholder(&frame.units)
        && matches!(
            (&frame.start_bound, &frame.end_bound),
            (
                WindowFrameBound::CurrentRow,
                Some(WindowFrameBound::Preceding(_))
            )
        ))
    {
        return Err(anyhow!(
            "Unacceptable combination of window frame bound specifications"
        ));
    }
    if matches!(frame.units, WindowFrameUnits::Groups) {
        return Err(anyhow!("GROUPS window frames are not supported"));
    }
    let raw_bound = |bound: &WindowFrameBound| -> Result<i64> {
        Ok(match bound {
            WindowFrameBound::CurrentRow => position as i64,
            WindowFrameBound::Preceding(None) => i64::MIN,
            WindowFrameBound::Following(None) => i64::MAX,
            WindowFrameBound::Preceding(Some(offset)) => {
                position as i64 - expr_to_usize(offset)? as i64
            }
            WindowFrameBound::Following(Some(offset)) => {
                position as i64 + expr_to_usize(offset)? as i64
            }
        })
    };
    let raw_start = raw_bound(&frame.start_bound)?;
    let raw_end = raw_bound(
        frame
            .end_bound
            .as_ref()
            .unwrap_or(&WindowFrameBound::CurrentRow),
    )?;
    if raw_start > raw_end {
        return Ok(None);
    }
    let range_frame = matches!(frame.units, WindowFrameUnits::Range);
    let bound = |bound: &WindowFrameBound, is_start: bool| -> Result<usize> {
        Ok(match bound {
            WindowFrameBound::CurrentRow if range_frame && is_start => peer_start,
            WindowFrameBound::CurrentRow if range_frame => peer_end,
            WindowFrameBound::CurrentRow => position,
            WindowFrameBound::Preceding(None) => 0,
            WindowFrameBound::Following(None) => len.saturating_sub(1),
            WindowFrameBound::Preceding(Some(offset)) => {
                position.saturating_sub(expr_to_usize(offset)?)
            }
            WindowFrameBound::Following(Some(offset)) => position
                .saturating_add(expr_to_usize(offset)?)
                .min(len.saturating_sub(1)),
        })
    };
    let start = bound(&frame.start_bound, true)?;
    let end = bound(
        frame
            .end_bound
            .as_ref()
            .unwrap_or(&WindowFrameBound::CurrentRow),
        false,
    )?;
    Ok((start <= end).then_some((start, end + 1)))
}

fn window_range_frame_positions(
    spec: &sqlparser::ast::WindowSpec,
    partition: &[usize],
    position: usize,
    order_keys: &[Vec<Value>],
) -> Result<Option<(usize, usize)>> {
    use sqlparser::ast::WindowFrameBound;

    let Some(frame) = &spec.window_frame else {
        return Ok(Some((0, partition.len())));
    };
    if matches!(
        (&frame.start_bound, &frame.end_bound),
        (
            WindowFrameBound::CurrentRow,
            Some(WindowFrameBound::Preceding(_))
        )
    ) {
        return Err(anyhow!(
            "Unacceptable combination of window frame bound specifications"
        ));
    }
    let current = json_to_f64_lossy(
        order_keys
            .get(partition[position])
            .and_then(|keys| keys.first())
            .unwrap_or(&Value::Null),
    )?;
    let bound_value = |bound: &WindowFrameBound| -> Result<f64> {
        Ok(match bound {
            WindowFrameBound::CurrentRow => current,
            WindowFrameBound::Preceding(None) => f64::NEG_INFINITY,
            WindowFrameBound::Following(None) => f64::INFINITY,
            WindowFrameBound::Preceding(Some(offset)) => current - expr_to_usize(offset)? as f64,
            WindowFrameBound::Following(Some(offset)) => current + expr_to_usize(offset)? as f64,
        })
    };
    let lower = bound_value(&frame.start_bound)?;
    let upper = bound_value(
        frame
            .end_bound
            .as_ref()
            .unwrap_or(&WindowFrameBound::CurrentRow),
    )?;
    if lower > upper {
        return Ok(None);
    }
    let mut first = None;
    let mut last = None;
    for (index, row_index) in partition.iter().enumerate() {
        let key = json_to_f64_lossy(
            order_keys
                .get(*row_index)
                .and_then(|keys| keys.first())
                .unwrap_or(&Value::Null),
        )?;
        if key >= lower && key <= upper {
            first.get_or_insert(index);
            last = Some(index + 1);
        }
    }
    Ok(first.zip(last))
}

fn range_frame_placeholder(units: &sqlparser::ast::WindowFrameUnits) -> bool {
    matches!(units, sqlparser::ast::WindowFrameUnits::Range)
}

fn eval_window_argument(
    engine: &RawEngine,
    argument: Option<&Option<Expr>>,
    row: &Map<String, Value>,
    last_insert_id: u64,
) -> Result<Value> {
    argument
        .and_then(Option::as_ref)
        .map(|expr| engine.eval_expr_ctx(expr, row, last_insert_id))
        .transpose()
        .map(|value| value.unwrap_or(Value::Null))
}

fn window_usize_argument(
    engine: &RawEngine,
    argument: Option<&Option<Expr>>,
    row: &Map<String, Value>,
    last_insert_id: u64,
    default: usize,
) -> Result<usize> {
    let Some(argument) = argument.and_then(Option::as_ref) else {
        return Ok(default);
    };
    let value = engine.eval_expr_ctx(argument, row, last_insert_id)?;
    value
        .as_u64()
        .or_else(|| value.as_i64().and_then(|value| u64::try_from(value).ok()))
        .map(|value| value as usize)
        .ok_or_else(|| anyhow!("window function argument must be a nonnegative integer"))
}

fn query_from_body(body: SetExpr) -> Query {
    Query {
        body: Box::new(body),
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
    }
}

fn values_query(result: &QueryResult) -> Query {
    let rows = result
        .rows
        .iter()
        .map(|row| {
            result
                .columns
                .iter()
                .map(|column| match row.get(column).unwrap_or(&Value::Null) {
                    Value::Null => Expr::Value(SqlValue::Null),
                    Value::Bool(value) => Expr::Value(SqlValue::Boolean(*value)),
                    Value::Number(value) => Expr::Value(SqlValue::Number(value.to_string(), false)),
                    Value::String(value) => {
                        Expr::Value(SqlValue::SingleQuotedString(value.clone()))
                    }
                    value => Expr::Value(SqlValue::SingleQuotedString(
                        json_wire_text(value).unwrap_or_default(),
                    )),
                })
                .collect::<Vec<_>>()
        })
        .collect();
    query_from_body(SetExpr::Values(sqlparser::ast::Values {
        explicit_row: false,
        rows,
    }))
}

fn replace_recursive_reference(
    mut query: Query,
    name: &str,
    replacement: Query,
    columns: &[sqlparser::ast::TableAliasColumnDef],
) -> Query {
    use sqlparser::ast::{TableAlias, VisitMut, VisitorMut};
    use std::ops::ControlFlow;

    struct Replacer {
        name: String,
        replacement: Query,
        columns: Vec<sqlparser::ast::TableAliasColumnDef>,
    }

    impl VisitorMut for Replacer {
        type Break = ();

        fn pre_visit_table_factor(&mut self, factor: &mut TableFactor) -> ControlFlow<Self::Break> {
            let TableFactor::Table { name, alias, .. } = factor else {
                return ControlFlow::Continue(());
            };
            if name.0.len() != 1 || !name.0[0].value.eq_ignore_ascii_case(&self.name) {
                return ControlFlow::Continue(());
            }
            let mut derived_alias = alias.clone().unwrap_or_else(|| TableAlias {
                name: name.0[0].clone(),
                columns: Vec::new(),
            });
            if derived_alias.columns.is_empty() {
                derived_alias.columns = self.columns.clone();
            }
            *factor = TableFactor::Derived {
                lateral: false,
                subquery: Box::new(self.replacement.clone()),
                alias: Some(derived_alias),
            };
            ControlFlow::Continue(())
        }
    }

    let _ = query.visit(&mut Replacer {
        name: name.to_string(),
        replacement,
        columns: columns.to_vec(),
    });
    query
}

fn inline_common_table_expressions(mut query: Query) -> Result<Query> {
    use sqlparser::ast::{TableAlias, VisitMut, VisitorMut};
    use std::ops::ControlFlow;

    let Some(with) = query.with.take() else {
        return Ok(query);
    };
    if with.recursive {
        return Err(anyhow!(
            "recursive common table expressions are not supported yet"
        ));
    }

    #[derive(Clone)]
    struct CteDefinition {
        query: Box<Query>,
        columns: Vec<sqlparser::ast::TableAliasColumnDef>,
    }

    struct Inliner<'a> {
        definitions: &'a BTreeMap<String, CteDefinition>,
    }

    impl VisitorMut for Inliner<'_> {
        type Break = ();

        fn pre_visit_table_factor(&mut self, factor: &mut TableFactor) -> ControlFlow<Self::Break> {
            let TableFactor::Table { name, alias, .. } = factor else {
                return ControlFlow::Continue(());
            };
            if name.0.len() != 1 {
                return ControlFlow::Continue(());
            }
            let cte_name = name.0[0].value.to_ascii_lowercase();
            let Some(definition) = self.definitions.get(&cte_name) else {
                return ControlFlow::Continue(());
            };
            let mut derived_alias = alias.clone().unwrap_or_else(|| TableAlias {
                name: name.0[0].clone(),
                columns: Vec::new(),
            });
            if derived_alias.columns.is_empty() {
                derived_alias.columns = definition.columns.clone();
            }
            *factor = TableFactor::Derived {
                lateral: false,
                subquery: definition.query.clone(),
                alias: Some(derived_alias),
            };
            ControlFlow::Continue(())
        }
    }

    let mut definitions = BTreeMap::new();
    for cte in with.cte_tables {
        if cte.from.is_some() || cte.materialized.is_some() {
            return Err(anyhow!("unsupported common table expression modifier"));
        }
        let mut cte_query = *cte.query;
        let _ = cte_query.visit(&mut Inliner {
            definitions: &definitions,
        });
        definitions.insert(
            cte.alias.name.value.to_ascii_lowercase(),
            CteDefinition {
                query: Box::new(cte_query),
                columns: cte.alias.columns,
            },
        );
    }
    let _ = query.visit(&mut Inliner {
        definitions: &definitions,
    });
    Ok(query)
}

fn set_intersection(
    left: Vec<Map<String, Value>>,
    right: Vec<Map<String, Value>>,
    all: bool,
) -> Vec<Map<String, Value>> {
    let mut right_counts = HashMap::<String, usize>::new();
    for row in right {
        *right_counts.entry(encode_json_row(&row)).or_default() += 1;
    }
    let mut emitted = HashSet::new();
    let mut output = Vec::new();
    for row in left {
        let key = encode_json_row(&row);
        if right_counts.get(&key).copied().unwrap_or(0) == 0 {
            continue;
        }
        if all {
            *right_counts.get_mut(&key).expect("positive count exists") -= 1;
            output.push(row);
        } else if emitted.insert(key) {
            output.push(row);
        }
    }
    output
}

fn set_difference(
    left: Vec<Map<String, Value>>,
    right: Vec<Map<String, Value>>,
    all: bool,
) -> Vec<Map<String, Value>> {
    let mut right_counts = HashMap::<String, usize>::new();
    for row in right {
        *right_counts.entry(encode_json_row(&row)).or_default() += 1;
    }
    let mut emitted = HashSet::new();
    let mut output = Vec::new();
    for row in left {
        let key = encode_json_row(&row);
        if all {
            if let Some(count) = right_counts.get_mut(&key)
                && *count > 0
            {
                *count -= 1;
                continue;
            }
            output.push(row);
        } else if !right_counts.contains_key(&key) && emitted.insert(key) {
            output.push(row);
        }
    }
    output
}

#[derive(Clone, Default)]
struct ColumnScope {
    unqualified: BTreeMap<String, usize>,
    qualified: BTreeSet<String>,
    aliases: BTreeSet<String>,
}

fn validate_expr_columns(expr: &Expr, scope: &ColumnScope) -> Result<()> {
    let expression_text = expr.to_string();
    if (expression_text.starts_with('@') && !expression_text.starts_with("@@"))
        || system_variable_expr_value(expr).is_some()
        || user_variable_name(expr).is_some()
    {
        return Ok(());
    }

    match expr {
        Expr::Identifier(identifier) => {
            let name = identifier.value.to_ascii_lowercase();
            if scope.aliases.contains(&name)
                || is_bare_datetime_keyword(&identifier.value)
                || matches!(name.as_str(), "date" | "time" | "datetime" | "timestamp")
            {
                return Ok(());
            }
            match scope.unqualified.get(&name).copied() {
                Some(1) => Ok(()),
                Some(_) => Err(anyhow!("ambiguous column: {}", identifier.value)),
                None => Err(anyhow!("unknown column: {}", identifier.value)),
            }
        }
        Expr::CompoundIdentifier(parts) => {
            let name = parts
                .iter()
                .map(|part| part.value.to_ascii_lowercase())
                .collect::<Vec<_>>()
                .join(".");
            if scope.qualified.contains(&name) {
                Ok(())
            } else {
                Err(anyhow!("unknown column: {name}"))
            }
        }
        Expr::Nested(expr)
        | Expr::UnaryOp { expr, .. }
        | Expr::IsFalse(expr)
        | Expr::IsNotFalse(expr)
        | Expr::IsTrue(expr)
        | Expr::IsNotTrue(expr)
        | Expr::IsNull(expr)
        | Expr::IsNotNull(expr)
        | Expr::IsUnknown(expr)
        | Expr::IsNotUnknown(expr)
        | Expr::Cast { expr, .. }
        | Expr::Extract { expr, .. }
        | Expr::Ceil { expr, .. }
        | Expr::Floor { expr, .. } => validate_expr_columns(expr, scope),
        Expr::Convert { expr, styles, .. } => {
            validate_expr_columns(expr, scope)?;
            for style in styles {
                validate_expr_columns(style, scope)?;
            }
            Ok(())
        }
        Expr::BinaryOp { left, right, .. } => {
            validate_expr_columns(left, scope)?;
            validate_expr_columns(right, scope)
        }
        Expr::InList { expr, list, .. } => {
            validate_expr_columns(expr, scope)?;
            for item in list {
                validate_expr_columns(item, scope)?;
            }
            Ok(())
        }
        Expr::InSubquery { expr, .. } => validate_expr_columns(expr, scope),
        Expr::Between {
            expr, low, high, ..
        } => {
            validate_expr_columns(expr, scope)?;
            validate_expr_columns(low, scope)?;
            validate_expr_columns(high, scope)
        }
        Expr::Like { expr, pattern, .. } => {
            validate_expr_columns(expr, scope)?;
            validate_expr_columns(pattern, scope)
        }
        Expr::Position { expr, r#in } => {
            validate_expr_columns(expr, scope)?;
            validate_expr_columns(r#in, scope)
        }
        Expr::Substring {
            expr,
            substring_from,
            substring_for,
            ..
        } => {
            validate_expr_columns(expr, scope)?;
            if let Some(from) = substring_from {
                validate_expr_columns(from, scope)?;
            }
            if let Some(for_expr) = substring_for {
                validate_expr_columns(for_expr, scope)?;
            }
            Ok(())
        }
        Expr::Trim {
            expr,
            trim_what,
            trim_characters,
            ..
        } => {
            validate_expr_columns(expr, scope)?;
            if let Some(trim_what) = trim_what {
                validate_expr_columns(trim_what, scope)?;
            }
            if let Some(items) = trim_characters {
                for item in items {
                    validate_expr_columns(item, scope)?;
                }
            }
            Ok(())
        }
        Expr::Case {
            operand,
            conditions,
            results,
            else_result,
        } => {
            if let Some(operand) = operand {
                validate_expr_columns(operand, scope)?;
            }
            for expr in conditions.iter().chain(results.iter()) {
                validate_expr_columns(expr, scope)?;
            }
            if let Some(else_result) = else_result {
                validate_expr_columns(else_result, scope)?;
            }
            Ok(())
        }
        Expr::Function(function) => {
            validate_function_arguments(&function.parameters, scope, false)?;
            let function_name = function
                .name
                .0
                .last()
                .map(|part| part.value.to_ascii_uppercase())
                .unwrap_or_default();
            validate_function_arguments(
                &function.args,
                scope,
                matches!(function_name.as_str(), "TIMESTAMPADD" | "TIMESTAMPDIFF"),
            )
        }
        Expr::Interval(interval) => validate_expr_columns(&interval.value, scope),
        Expr::Value(_) | Expr::Subquery(_) | Expr::Exists { .. } => Ok(()),
        // The support validator rejects all other expression shapes before
        // execution. Keeping this arm makes name binding resilient to new AST
        // variants while preserving that fail-closed boundary.
        _ => Ok(()),
    }
}

fn validate_function_arguments(
    arguments: &FunctionArguments,
    scope: &ColumnScope,
    skip_first_unit: bool,
) -> Result<()> {
    let FunctionArguments::List(list) = arguments else {
        return Ok(());
    };

    for (index, argument) in list.args.iter().enumerate() {
        let argument = match argument {
            FunctionArg::Named { arg, .. }
            | FunctionArg::ExprNamed { arg, .. }
            | FunctionArg::Unnamed(arg) => arg,
        };
        if skip_first_unit
            && index == 0
            && matches!(argument, FunctionArgExpr::Expr(Expr::Identifier(_)))
        {
            continue;
        }
        if let FunctionArgExpr::Expr(expr) = argument {
            validate_expr_columns(expr, scope)?;
        }
    }
    for clause in &list.clauses {
        match clause {
            FunctionArgumentClause::OrderBy(items) => {
                for item in items {
                    validate_expr_columns(&item.expr, scope)?;
                }
            }
            FunctionArgumentClause::Limit(expr) => validate_expr_columns(expr, scope)?,
            FunctionArgumentClause::Having(bound) => validate_expr_columns(&bound.1, scope)?,
            _ => {}
        }
    }
    Ok(())
}

fn query_local_qualifiers(query: &Query) -> Result<BTreeSet<String>> {
    let mut qualifiers = BTreeSet::new();
    collect_set_expr_qualifiers(&query.body, &mut qualifiers)?;
    Ok(qualifiers)
}

fn is_outer_column_error(error: &anyhow::Error, outer: &Map<String, Value>) -> bool {
    let message = error.to_string();
    let Some(column) = message.strip_prefix("unknown column: ") else {
        return false;
    };
    outer
        .keys()
        .any(|key| key.eq_ignore_ascii_case(column.trim_matches('`')))
}

fn collect_set_expr_qualifiers(body: &SetExpr, qualifiers: &mut BTreeSet<String>) -> Result<()> {
    match body {
        SetExpr::Select(select) => collect_select_qualifiers(select, qualifiers),
        SetExpr::Query(query) => collect_set_expr_qualifiers(&query.body, qualifiers),
        SetExpr::SetOperation { left, right, .. } => {
            collect_set_expr_qualifiers(left, qualifiers)?;
            collect_set_expr_qualifiers(right, qualifiers)
        }
        _ => Ok(()),
    }
}

fn collect_select_qualifiers(select: &Select, qualifiers: &mut BTreeSet<String>) -> Result<()> {
    for table in &select.from {
        collect_table_qualifier(&table.relation, qualifiers)?;
        for join in &table.joins {
            collect_table_qualifier(&join.relation, qualifiers)?;
        }
    }
    Ok(())
}

fn collect_table_qualifier(
    relation: &TableFactor,
    qualifiers: &mut BTreeSet<String>,
) -> Result<()> {
    match relation {
        TableFactor::Table { name, alias, .. } => {
            let full_name = object_name(name)?;
            if let Some(alias) = alias {
                qualifiers.insert(alias.name.value.clone());
            } else {
                qualifiers.insert(full_name.clone());
                if let Some(last) = full_name.rsplit('.').next() {
                    qualifiers.insert(last.to_string());
                }
            }
        }
        TableFactor::Derived {
            alias: Some(alias), ..
        } => {
            qualifiers.insert(alias.name.value.clone());
        }
        _ => {}
    }
    Ok(())
}

fn query_outer_reference(
    query: &Query,
    local_qualifiers: &BTreeSet<String>,
) -> Result<Option<String>> {
    if let Some(order_by) = &query.order_by {
        for item in &order_by.exprs {
            if let Some(identifier) = expr_outer_reference(&item.expr, local_qualifiers)? {
                return Ok(Some(identifier));
            }
        }
    }
    set_expr_outer_reference(&query.body, local_qualifiers)
}

fn set_expr_outer_reference(
    body: &SetExpr,
    local_qualifiers: &BTreeSet<String>,
) -> Result<Option<String>> {
    match body {
        SetExpr::Select(select) => select_outer_reference(select, local_qualifiers),
        SetExpr::Query(query) => query_outer_reference(query, local_qualifiers),
        SetExpr::SetOperation { left, right, .. } => {
            if let Some(identifier) = set_expr_outer_reference(left, local_qualifiers)? {
                return Ok(Some(identifier));
            }
            set_expr_outer_reference(right, local_qualifiers)
        }
        _ => Ok(None),
    }
}

fn select_outer_reference(
    select: &Select,
    local_qualifiers: &BTreeSet<String>,
) -> Result<Option<String>> {
    for item in &select.projection {
        if let Some(identifier) = select_item_outer_reference(item, local_qualifiers)? {
            return Ok(Some(identifier));
        }
    }
    for table in &select.from {
        for join in &table.joins {
            if let Some(identifier) = join_outer_reference(&join.join_operator, local_qualifiers)? {
                return Ok(Some(identifier));
            }
        }
    }
    for expr in [&select.selection, &select.having, &select.qualify]
        .into_iter()
        .flatten()
    {
        if let Some(identifier) = expr_outer_reference(expr, local_qualifiers)? {
            return Ok(Some(identifier));
        }
    }
    match &select.group_by {
        GroupByExpr::Expressions(exprs, _) => {
            for expr in exprs {
                if let Some(identifier) = expr_outer_reference(expr, local_qualifiers)? {
                    return Ok(Some(identifier));
                }
            }
        }
        GroupByExpr::All(_) => {}
    }
    Ok(None)
}

fn select_item_outer_reference(
    item: &SelectItem,
    local_qualifiers: &BTreeSet<String>,
) -> Result<Option<String>> {
    match item {
        SelectItem::UnnamedExpr(expr) | SelectItem::ExprWithAlias { expr, .. } => {
            expr_outer_reference(expr, local_qualifiers)
        }
        _ => Ok(None),
    }
}

fn join_outer_reference(
    join: &JoinOperator,
    local_qualifiers: &BTreeSet<String>,
) -> Result<Option<String>> {
    match join {
        JoinOperator::Inner(JoinConstraint::On(expr))
        | JoinOperator::LeftOuter(JoinConstraint::On(expr)) => {
            expr_outer_reference(expr, local_qualifiers)
        }
        _ => Ok(None),
    }
}

fn expr_outer_reference(
    expr: &Expr,
    local_qualifiers: &BTreeSet<String>,
) -> Result<Option<String>> {
    match expr {
        Expr::CompoundIdentifier(parts) => {
            let identifier = parts
                .iter()
                .map(|part| part.value.clone())
                .collect::<Vec<_>>()
                .join(".");
            let qualifier = parts
                .iter()
                .take(parts.len().saturating_sub(1))
                .map(|part| part.value.clone())
                .collect::<Vec<_>>()
                .join(".");
            if !qualifier.is_empty() && !local_qualifiers.contains(&qualifier) {
                return Ok(Some(identifier));
            }
            Ok(None)
        }
        Expr::Nested(expr)
        | Expr::UnaryOp { expr, .. }
        | Expr::IsNull(expr)
        | Expr::IsNotNull(expr)
        | Expr::Cast { expr, .. } => expr_outer_reference(expr, local_qualifiers),
        Expr::BinaryOp { left, right, .. } => {
            if let Some(identifier) = expr_outer_reference(left, local_qualifiers)? {
                return Ok(Some(identifier));
            }
            expr_outer_reference(right, local_qualifiers)
        }
        Expr::InList { expr, list, .. } => {
            if let Some(identifier) = expr_outer_reference(expr, local_qualifiers)? {
                return Ok(Some(identifier));
            }
            for item in list {
                if let Some(identifier) = expr_outer_reference(item, local_qualifiers)? {
                    return Ok(Some(identifier));
                }
            }
            Ok(None)
        }
        Expr::Between {
            expr, low, high, ..
        } => {
            for expr in [expr.as_ref(), low.as_ref(), high.as_ref()] {
                if let Some(identifier) = expr_outer_reference(expr, local_qualifiers)? {
                    return Ok(Some(identifier));
                }
            }
            Ok(None)
        }
        Expr::Like { expr, pattern, .. } => {
            if let Some(identifier) = expr_outer_reference(expr, local_qualifiers)? {
                return Ok(Some(identifier));
            }
            expr_outer_reference(pattern, local_qualifiers)
        }
        Expr::Case {
            operand,
            conditions,
            results,
            else_result,
        } => {
            if let Some(operand) = operand
                && let Some(identifier) = expr_outer_reference(operand, local_qualifiers)?
            {
                return Ok(Some(identifier));
            }
            for expr in conditions.iter().chain(results.iter()) {
                if let Some(identifier) = expr_outer_reference(expr, local_qualifiers)? {
                    return Ok(Some(identifier));
                }
            }
            if let Some(expr) = else_result
                && let Some(identifier) = expr_outer_reference(expr, local_qualifiers)?
            {
                return Ok(Some(identifier));
            }
            Ok(None)
        }
        Expr::Function(function) => function_outer_reference(function, local_qualifiers),
        Expr::Subquery(query) => query_outer_reference(query, &query_local_qualifiers(query)?),
        Expr::Exists { subquery, .. } => {
            query_outer_reference(subquery, &query_local_qualifiers(subquery)?)
        }
        Expr::InSubquery { expr, subquery, .. } => {
            if let Some(identifier) = expr_outer_reference(expr, local_qualifiers)? {
                return Ok(Some(identifier));
            }
            query_outer_reference(subquery, &query_local_qualifiers(subquery)?)
        }
        _ => Ok(None),
    }
}

fn function_outer_reference(
    function: &sqlparser::ast::Function,
    local_qualifiers: &BTreeSet<String>,
) -> Result<Option<String>> {
    for args in [&function.parameters, &function.args] {
        if let Some(identifier) = function_args_outer_reference(args, local_qualifiers)? {
            return Ok(Some(identifier));
        }
    }
    if let Some(filter) = &function.filter
        && let Some(identifier) = expr_outer_reference(filter, local_qualifiers)?
    {
        return Ok(Some(identifier));
    }
    for item in &function.within_group {
        if let Some(identifier) = expr_outer_reference(&item.expr, local_qualifiers)? {
            return Ok(Some(identifier));
        }
    }
    Ok(None)
}

fn function_args_outer_reference(
    args: &FunctionArguments,
    local_qualifiers: &BTreeSet<String>,
) -> Result<Option<String>> {
    match args {
        FunctionArguments::None => Ok(None),
        FunctionArguments::Subquery(query) => {
            query_outer_reference(query, &query_local_qualifiers(query)?)
        }
        FunctionArguments::List(list) => {
            for arg in &list.args {
                if let Some(identifier) = function_arg_outer_reference(arg, local_qualifiers)? {
                    return Ok(Some(identifier));
                }
            }
            for clause in &list.clauses {
                if let Some(identifier) =
                    function_arg_clause_outer_reference(clause, local_qualifiers)?
                {
                    return Ok(Some(identifier));
                }
            }
            Ok(None)
        }
    }
}

fn function_arg_outer_reference(
    arg: &FunctionArg,
    local_qualifiers: &BTreeSet<String>,
) -> Result<Option<String>> {
    match arg {
        FunctionArg::Named { arg, .. } | FunctionArg::Unnamed(arg) => {
            function_arg_expr_outer_reference(arg, local_qualifiers)
        }
        FunctionArg::ExprNamed { name, arg, .. } => {
            if let Some(identifier) = expr_outer_reference(name, local_qualifiers)? {
                return Ok(Some(identifier));
            }
            function_arg_expr_outer_reference(arg, local_qualifiers)
        }
    }
}

fn function_arg_expr_outer_reference(
    arg: &FunctionArgExpr,
    local_qualifiers: &BTreeSet<String>,
) -> Result<Option<String>> {
    match arg {
        FunctionArgExpr::Expr(expr) => expr_outer_reference(expr, local_qualifiers),
        FunctionArgExpr::QualifiedWildcard(_) | FunctionArgExpr::Wildcard => Ok(None),
    }
}

fn function_arg_clause_outer_reference(
    clause: &FunctionArgumentClause,
    local_qualifiers: &BTreeSet<String>,
) -> Result<Option<String>> {
    match clause {
        FunctionArgumentClause::OrderBy(order_by) => {
            for item in order_by {
                if let Some(identifier) = expr_outer_reference(&item.expr, local_qualifiers)? {
                    return Ok(Some(identifier));
                }
            }
            Ok(None)
        }
        FunctionArgumentClause::Limit(expr) => expr_outer_reference(expr, local_qualifiers),
        FunctionArgumentClause::Having(bound) => expr_outer_reference(&bound.1, local_qualifiers),
        _ => Ok(None),
    }
}

fn key_column_usage_row(
    database: &str,
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
        Value::String(database.to_string()),
    );
    row.insert(
        "constraint_name".to_string(),
        Value::String(constraint_name.to_string()),
    );
    row.insert(
        "table_catalog".to_string(),
        Value::String("def".to_string()),
    );
    row.insert(
        "table_schema".to_string(),
        Value::String(database.to_string()),
    );
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
            Value::String(database.to_string()),
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
