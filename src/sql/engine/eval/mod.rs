use super::*;
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};

use sqlparser::ast::Function;

thread_local! {
    static EVAL_DATABASE: RefCell<String> = RefCell::new("app".into());
    static EVAL_USER_VARIABLES: RefCell<std::collections::HashMap<String, Value>> =
        RefCell::new(std::collections::HashMap::new());
}

pub(super) fn set_eval_database(name: &str) {
    EVAL_DATABASE.with(|database| *database.borrow_mut() = name.into());
}

pub(super) fn clear_eval_user_variables() {
    EVAL_USER_VARIABLES.with(|variables| variables.borrow_mut().clear());
}

pub(super) fn take_eval_user_variables() -> HashMap<String, Value> {
    EVAL_USER_VARIABLES.with(|variables| std::mem::take(&mut *variables.borrow_mut()))
}

fn eval_user_variable(name: &str) -> Option<Value> {
    EVAL_USER_VARIABLES.with(|variables| {
        variables
            .borrow()
            .get(name.trim_start_matches('@'))
            .cloned()
    })
}

fn set_eval_user_variable(name: &str, value: Value) {
    EVAL_USER_VARIABLES.with(|variables| {
        variables.borrow_mut().insert(name.to_string(), value);
    });
}

mod common;
mod datetime;
mod json;
mod scalar;

pub(crate) fn soundex_text(value: &str) -> String {
    scalar::mysql_soundex(value)
}

use common::*;
pub(super) use datetime::*;
use json::*;
use scalar::*;

pub(crate) const MYSQL_BINARY_SENTINEL: &str = "\0my_sqweel_binary:";

pub(crate) use common::unquote_sql_string;
pub(crate) use json::{
    json_compact_text, json_extract_matches, json_extract_path, json_wire_text,
    parse_json_document_value,
};

const HIDDEN_HISTORICAL_COLUMN_PREFIX: &str = "\0my_sqweel_historical:";
const SQL_DEFAULT_VALUE_SENTINEL: &str = "\0my_sqweel_sql_default";

pub(super) fn is_json_null(value: &str) -> bool {
    value == super::JSON_NULL_SENTINEL
}

pub(super) fn json_null_value() -> Value {
    Value::String(super::JSON_NULL_SENTINEL.to_string())
}

pub(super) fn mark_json_nulls(value: Value) -> Value {
    match value {
        Value::Null => json_null_value(),
        Value::Array(values) => Value::Array(values.into_iter().map(mark_json_nulls).collect()),
        Value::Object(values) => Value::Object(
            values
                .into_iter()
                .map(|(key, value)| (key, mark_json_nulls(value)))
                .collect(),
        ),
        other => other,
    }
}

pub(super) fn public_json_value(value: &Value) -> Value {
    match value {
        Value::String(value) if is_json_null(value) => Value::Null,
        Value::Array(values) => Value::Array(values.iter().map(public_json_value).collect()),
        Value::Object(values) => Value::Object(
            values
                .iter()
                .map(|(key, value)| (key.clone(), public_json_value(value)))
                .collect(),
        ),
        other => other.clone(),
    }
}

pub(super) fn contains_json_null_sentinel(value: &Value) -> bool {
    match value {
        Value::String(value) => is_json_null(value),
        Value::Array(values) => values.iter().any(contains_json_null_sentinel),
        Value::Object(values) => values.values().any(contains_json_null_sentinel),
        _ => false,
    }
}

pub(super) fn sql_default_value() -> Value {
    Value::String(SQL_DEFAULT_VALUE_SENTINEL.to_string())
}

pub(super) fn is_default_keyword(value: &Value) -> bool {
    value.as_str() == Some(SQL_DEFAULT_VALUE_SENTINEL)
}

pub(super) fn is_bare_datetime_keyword(name: &str) -> bool {
    eval_bare_datetime_keyword(name).is_some()
}

pub(super) fn historical_column_marker(column: &str) -> String {
    format!("{HIDDEN_HISTORICAL_COLUMN_PREFIX}{column}")
}

fn is_historical_column_marker(column: &str) -> bool {
    column.starts_with(HIDDEN_HISTORICAL_COLUMN_PREFIX)
}

pub(super) fn table_factor_name(factor: &TableFactor) -> Result<String> {
    table_factor_name_and_alias(factor).map(|(name, _)| name)
}

pub(super) fn table_factor_name_and_alias(
    factor: &TableFactor,
) -> Result<(String, Option<String>)> {
    match factor {
        TableFactor::Table { name, alias, .. } => Ok((
            object_name(name)?,
            alias.as_ref().map(|alias| alias.name.value.clone()),
        )),
        TableFactor::JsonTable { alias, .. } => Ok((
            alias
                .as_ref()
                .map(|alias| alias.name.value.clone())
                .unwrap_or_else(|| "json_table".to_string()),
            alias.as_ref().map(|alias| alias.name.value.clone()),
        )),
        _ => Err(anyhow!("unsupported table factor")),
    }
}

pub(super) fn add_qualified_columns(
    target: &mut Map<String, Value>,
    qualifier: &str,
    data: &Map<String, Value>,
) {
    for (key, value) in data {
        target.insert(format!("{qualifier}.{key}"), value.clone());
    }
}

pub(super) fn add_qualified_columns_in_place(target: &mut Map<String, Value>, qualifier: &str) {
    let qualified = target
        .iter()
        .filter(|(key, _)| !key.contains('.'))
        .map(|(key, value)| (format!("{qualifier}.{key}"), value.clone()))
        .collect::<Vec<_>>();
    target.extend(qualified);
}

pub(super) fn table_factor_name_full(factor: &TableFactor) -> Result<String> {
    match factor {
        TableFactor::Table { name, .. } => Ok(name
            .0
            .iter()
            .map(|i| i.value.clone())
            .collect::<Vec<_>>()
            .join(".")),
        _ => Err(anyhow!("unsupported table factor")),
    }
}

pub(super) fn project_row(
    projection: &[SelectItem],
    data: &Map<String, Value>,
    last_insert_id: u64,
) -> Result<Map<String, Value>> {
    project_row_with(projection, data, |expr| {
        eval_expr(expr, data, last_insert_id)
    })
}

pub(super) fn project_row_with<F>(
    projection: &[SelectItem],
    data: &Map<String, Value>,
    mut eval: F,
) -> Result<Map<String, Value>>
where
    F: FnMut(&Expr) -> Result<Value>,
{
    let mut out = Map::new();
    for item in projection {
        match item {
            SelectItem::Wildcard(_) => {
                for (column, value) in data {
                    if !column.contains('.') && !is_historical_column_marker(column) {
                        out.insert(column.clone(), value.clone());
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
                let qualified_prefix = format!("{qualifier}.");
                for (column, value) in data {
                    if let Some(output) = strip_prefix_case_insensitive(column, &qualified_prefix)
                        && !is_historical_column_marker(output)
                    {
                        out.insert(output.to_string(), value.clone());
                    }
                }
            }
            SelectItem::UnnamedExpr(expr) => {
                let key = projection_output_column_name(expr);
                let value = eval(expr)?;
                out.insert(key, value);
            }
            SelectItem::ExprWithAlias { expr, alias } => {
                let value = eval(expr)?;
                out.insert(alias.value.clone(), value);
            }
        }
    }

    Ok(out)
}

fn strip_prefix_case_insensitive<'a>(value: &'a str, prefix: &str) -> Option<&'a str> {
    (value.len() >= prefix.len() && value[..prefix.len()].eq_ignore_ascii_case(prefix))
        .then(|| &value[prefix.len()..])
}

pub(super) fn virtual_select_result(
    select: &Select,
    rows: Vec<Map<String, Value>>,
) -> Result<QueryResult> {
    let qualifiers = select.from.first().and_then(|table| match &table.relation {
        TableFactor::Table { name, alias, .. } => {
            let full = name
                .0
                .iter()
                .map(|part| part.value.clone())
                .collect::<Vec<_>>()
                .join(".");
            let short = name.0.last().map(|part| part.value.clone());
            Some((
                full,
                short,
                alias.as_ref().map(|alias| alias.name.value.clone()),
            ))
        }
        _ => None,
    });
    let rows = rows
        .into_iter()
        .filter_map(|row| {
            let mut view = row.clone();
            if let Some((full, short, alias)) = &qualifiers {
                add_qualified_columns(&mut view, full, &row);
                if let Some(short) = short {
                    add_qualified_columns(&mut view, short, &row);
                }
                if let Some(alias) = alias {
                    add_qualified_columns(&mut view, alias, &row);
                }
            }
            match matches_selection(select.selection.as_ref(), &view) {
                Ok(true) => Some(project_row(&select.projection, &view, 0)),
                Ok(false) => None,
                Err(err) => Some(Err(err)),
            }
        })
        .collect::<Result<Vec<_>>>()?;

    Ok(QueryResult {
        rows_affected: 0,
        last_insert_id: 0,
        columns: infer_projection_columns(&select.projection),
        column_metadata: vec![],
        rows,
        warnings: vec![],
    })
}

/// Preserve the result-set shape of a virtual table when a wildcard query
/// matches no rows. MySQL still sends column definitions for an empty SELECT;
/// without them the wire layer must encode the response as an OK packet.
pub(super) fn virtual_select_result_with_empty_schema(
    select: &Select,
    rows: Vec<Map<String, Value>>,
    wildcard_columns: &[&str],
) -> Result<QueryResult> {
    let mut result = virtual_select_result(select, rows)?;
    if result.rows.is_empty() {
        result.columns = select
            .projection
            .iter()
            .flat_map(|item| match item {
                SelectItem::UnnamedExpr(expr) => vec![projection_output_column_name(expr)],
                SelectItem::ExprWithAlias { alias, .. } => vec![alias.value.clone()],
                SelectItem::Wildcard(_) | SelectItem::QualifiedWildcard(_, _) => wildcard_columns
                    .iter()
                    .map(|column| (*column).to_string())
                    .collect(),
            })
            .collect();
    }
    Ok(result)
}

pub(super) fn aggregate_select_result(
    select: &Select,
    rows: &mut Vec<Map<String, Value>>,
    order_by: &[OrderByExpr],
    order_hints: &[Option<ColumnHint>],
    column_hints: &BTreeMap<String, ColumnHint>,
    limit: Option<&Expr>,
    offset: Option<&Offset>,
    last_insert_id: u64,
    eval: &dyn Fn(&Expr, &Map<String, Value>, u64) -> Result<Value>,
) -> Result<Option<QueryResult>> {
    let group_by = group_by_exprs(select);
    if group_by.is_empty() && !projection_has_aggregate(&select.projection) {
        return Ok(None);
    }

    let mut rows = std::mem::take(rows);
    for item in &select.projection {
        let SelectItem::ExprWithAlias { expr, alias } = item else {
            continue;
        };
        if expr_has_aggregate(expr) {
            continue;
        }
        for row in &mut rows {
            if !row.contains_key(&alias.value) {
                let value = eval(expr, row, last_insert_id)?;
                row.insert(alias.value.clone(), value);
            }
        }
    }
    let grouped = group_rows(rows, &group_by, last_insert_id, eval)?;
    let mut order_hint_map = column_hints.clone();
    order_hint_map.extend(
        order_by
            .iter()
            .zip(order_hints)
            .filter_map(|(order, hint)| hint.clone().map(|hint| (order.expr.to_string(), hint))),
    );
    let mut output = Vec::new();
    for group in grouped {
        let base = group.first().cloned().unwrap_or_default();
        let mut row = Map::new();

        for item in &select.projection {
            project_aggregate_item(
                item,
                &group,
                &base,
                last_insert_id,
                &order_hint_map,
                eval,
                &mut row,
            )?;
        }
        if let Some(having) = &select.having {
            let mut context = base.clone();
            context.extend(row.clone());
            materialize_aggregate_exprs(
                having,
                &group,
                &base,
                last_insert_id,
                &order_hint_map,
                eval,
                &mut context,
            )?;
            let having_value = eval(having, &context, last_insert_id)?;
            if !matches!(sql_truth(&having_value), SqlTruth::True) {
                continue;
            }
        }
        for order in order_by {
            materialize_aggregate_exprs(
                &order.expr,
                &group,
                &base,
                last_insert_id,
                &order_hint_map,
                eval,
                &mut row,
            )?;
        }
        output.push(row);
    }

    if select.distinct.is_some() {
        deduplicate_rows(&mut output);
    }
    apply_ordering_with(
        &mut output,
        order_by,
        order_hints,
        limit_offset_end(limit, offset)?,
        expr_resolved_value,
    )?;
    apply_limit_offset(&mut output, limit, offset)?;

    Ok(Some(QueryResult {
        rows_affected: 0,
        last_insert_id: 0,
        columns: infer_projection_columns(&select.projection),
        column_metadata: vec![],
        rows: output,
        warnings: vec![],
    }))
}

pub(super) fn deduplicate_rows(rows: &mut Vec<Map<String, Value>>) {
    let mut seen = HashSet::new();
    rows.retain(|row| seen.insert(encode_json_row(row)));
}

pub(super) fn group_by_exprs(select: &Select) -> Vec<Expr> {
    match &select.group_by {
        sqlparser::ast::GroupByExpr::Expressions(exprs, _) => exprs.clone(),
        sqlparser::ast::GroupByExpr::All(_) => Vec::new(),
    }
}

pub(super) fn projection_has_aggregate(projection: &[SelectItem]) -> bool {
    projection.iter().any(|item| match item {
        SelectItem::UnnamedExpr(expr) | SelectItem::ExprWithAlias { expr, .. } => {
            expr_has_aggregate(expr)
        }
        _ => false,
    })
}

fn expr_has_aggregate(expr: &Expr) -> bool {
    if aggregate_call(expr).is_some() {
        return true;
    }
    match expr {
        Expr::BinaryOp { left, right, .. } => expr_has_aggregate(left) || expr_has_aggregate(right),
        Expr::UnaryOp { expr, .. }
        | Expr::Nested(expr)
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
        | Expr::Floor { expr, .. } => expr_has_aggregate(expr),
        Expr::Convert { expr, styles, .. } => {
            expr_has_aggregate(expr) || styles.iter().any(expr_has_aggregate)
        }
        Expr::InList { expr, list, .. } => {
            expr_has_aggregate(expr) || list.iter().any(expr_has_aggregate)
        }
        Expr::InSubquery { expr, .. } => expr_has_aggregate(expr),
        Expr::Like { expr, pattern, .. } => expr_has_aggregate(expr) || expr_has_aggregate(pattern),
        Expr::Between {
            expr, low, high, ..
        } => expr_has_aggregate(expr) || expr_has_aggregate(low) || expr_has_aggregate(high),
        Expr::Position { expr, r#in } => expr_has_aggregate(expr) || expr_has_aggregate(r#in),
        Expr::Substring {
            expr,
            substring_from,
            substring_for,
            ..
        } => {
            expr_has_aggregate(expr)
                || substring_from.as_deref().is_some_and(expr_has_aggregate)
                || substring_for.as_deref().is_some_and(expr_has_aggregate)
        }
        Expr::Trim {
            expr,
            trim_what,
            trim_characters,
            ..
        } => {
            expr_has_aggregate(expr)
                || trim_what.as_deref().is_some_and(expr_has_aggregate)
                || trim_characters
                    .as_ref()
                    .is_some_and(|items| items.iter().any(expr_has_aggregate))
        }
        Expr::Case {
            operand,
            conditions,
            results,
            else_result,
        } => {
            operand.as_deref().is_some_and(expr_has_aggregate)
                || conditions.iter().any(expr_has_aggregate)
                || results.iter().any(expr_has_aggregate)
                || else_result.as_deref().is_some_and(expr_has_aggregate)
        }
        Expr::Function(function) => function_arguments_have_aggregate(&function.args),
        // Aggregates inside subqueries belong to the subquery's scope.
        Expr::Subquery(_) | Expr::Exists { .. } => false,
        _ => false,
    }
}

fn function_arguments_have_aggregate(arguments: &FunctionArguments) -> bool {
    let FunctionArguments::List(arguments) = arguments else {
        return false;
    };
    arguments.args.iter().any(|argument| {
        let arg = match argument {
            FunctionArg::Named { arg, .. }
            | FunctionArg::ExprNamed { arg, .. }
            | FunctionArg::Unnamed(arg) => arg,
        };
        matches!(arg, FunctionArgExpr::Expr(expr) if expr_has_aggregate(expr))
    })
}

pub(super) fn group_rows(
    rows: Vec<Map<String, Value>>,
    group_by: &[Expr],
    last_insert_id: u64,
    eval: &dyn Fn(&Expr, &Map<String, Value>, u64) -> Result<Value>,
) -> Result<Vec<Vec<Map<String, Value>>>> {
    if group_by.is_empty() {
        return Ok(vec![rows]);
    }

    let mut positions = HashMap::<String, usize>::new();
    let mut group_keys = Vec::<Vec<Value>>::new();
    let mut grouped = Vec::<Vec<Map<String, Value>>>::new();
    for row in rows {
        let mut key_parts = Vec::new();
        let mut key_values = Vec::new();
        for expr in group_by {
            let value = eval(expr, &row, last_insert_id)?;
            key_parts.push(encode_json_value(&value));
            key_values.push(value);
        }
        let key = key_parts.join("|");
        let position = *positions.entry(key).or_insert_with(|| {
            grouped.push(Vec::new());
            group_keys.push(key_values);
            grouped.len() - 1
        });
        grouped[position].push(row);
    }
    let mut grouped = group_keys.into_iter().zip(grouped).collect::<Vec<_>>();
    grouped.sort_by(|(left, _), (right, _)| {
        left.iter()
            .zip(right)
            .map(|(left, right)| compare_json_values(left, right))
            .find(|ordering| *ordering != Ordering::Equal)
            .unwrap_or_else(|| left.len().cmp(&right.len()))
    });
    Ok(grouped.into_iter().map(|(_, rows)| rows).collect())
}

pub(super) fn project_aggregate_item(
    item: &SelectItem,
    group: &[Map<String, Value>],
    base: &Map<String, Value>,
    last_insert_id: u64,
    order_hints: &BTreeMap<String, ColumnHint>,
    eval: &dyn Fn(&Expr, &Map<String, Value>, u64) -> Result<Value>,
    out: &mut Map<String, Value>,
) -> Result<()> {
    match item {
        SelectItem::Wildcard(_) => {
            out.extend(
                base.iter()
                    .filter(|(column, _)| {
                        !column.contains('.') && !is_historical_column_marker(column)
                    })
                    .map(|(column, value)| (column.clone(), value.clone())),
            );
        }
        SelectItem::UnnamedExpr(expr) => {
            let column = projection_output_column_name(expr);
            let value =
                aggregate_or_eval_expr(expr, group, base, last_insert_id, order_hints, eval)?;
            out.insert(column, value);
        }
        SelectItem::ExprWithAlias { expr, alias } => {
            let value =
                aggregate_or_eval_expr(expr, group, base, last_insert_id, order_hints, eval)?;
            if aggregate_call(expr).is_some() {
                out.insert(projection_expr_column_name(expr), value.clone());
            }
            out.insert(alias.value.clone(), value);
        }
        _ => {}
    }
    Ok(())
}

pub(super) fn aggregate_or_eval_expr(
    expr: &Expr,
    group: &[Map<String, Value>],
    base: &Map<String, Value>,
    last_insert_id: u64,
    order_hints: &BTreeMap<String, ColumnHint>,
    eval: &dyn Fn(&Expr, &Map<String, Value>, u64) -> Result<Value>,
) -> Result<Value> {
    eval_aggregate_expr(expr, group, base, last_insert_id, order_hints, eval)
}

fn eval_aggregate_expr(
    expr: &Expr,
    group: &[Map<String, Value>],
    base: &Map<String, Value>,
    last_insert_id: u64,
    order_hints: &BTreeMap<String, ColumnHint>,
    eval: &dyn Fn(&Expr, &Map<String, Value>, u64) -> Result<Value>,
) -> Result<Value> {
    if let Some(call) = aggregate_call(expr) {
        return eval_aggregate_call(&call, group, last_insert_id, order_hints, eval);
    }
    match expr {
        Expr::BinaryOp { left, op, right } => eval_binary_values(
            eval_aggregate_expr(left, group, base, last_insert_id, order_hints, eval)?,
            op,
            eval_aggregate_expr(right, group, base, last_insert_id, order_hints, eval)?,
        ),
        Expr::Nested(inner) => {
            eval_aggregate_expr(inner, group, base, last_insert_id, order_hints, eval)
        }
        _ => {
            let mut context = base.clone();
            materialize_aggregate_exprs(
                expr,
                group,
                base,
                last_insert_id,
                order_hints,
                eval,
                &mut context,
            )?;
            eval(expr, &context, last_insert_id)
        }
    }
}

pub(super) fn materialize_aggregate_exprs(
    expr: &Expr,
    group: &[Map<String, Value>],
    base: &Map<String, Value>,
    last_insert_id: u64,
    order_hints: &BTreeMap<String, ColumnHint>,
    eval: &dyn Fn(&Expr, &Map<String, Value>, u64) -> Result<Value>,
    out: &mut Map<String, Value>,
) -> Result<()> {
    if let Some(call) = aggregate_call(expr) {
        out.insert(
            projection_expr_column_name(expr),
            eval_aggregate_call(&call, group, last_insert_id, order_hints, eval)?,
        );
        return Ok(());
    }

    match expr {
        Expr::BinaryOp { left, right, .. } => {
            materialize_aggregate_exprs(left, group, base, last_insert_id, order_hints, eval, out)?;
            materialize_aggregate_exprs(
                right,
                group,
                base,
                last_insert_id,
                order_hints,
                eval,
                out,
            )?;
        }
        Expr::UnaryOp { expr, .. }
        | Expr::Nested(expr)
        | Expr::IsFalse(expr)
        | Expr::IsNotFalse(expr)
        | Expr::IsTrue(expr)
        | Expr::IsNotTrue(expr)
        | Expr::IsNull(expr)
        | Expr::IsNotNull(expr)
        | Expr::IsUnknown(expr)
        | Expr::IsNotUnknown(expr)
        | Expr::Extract { expr, .. }
        | Expr::Ceil { expr, .. }
        | Expr::Floor { expr, .. }
        | Expr::Cast { expr, .. } => {
            materialize_aggregate_exprs(expr, group, base, last_insert_id, order_hints, eval, out)?;
        }
        Expr::Convert { expr, styles, .. } => {
            materialize_aggregate_exprs(expr, group, base, last_insert_id, order_hints, eval, out)?;
            for style in styles {
                materialize_aggregate_exprs(
                    style,
                    group,
                    base,
                    last_insert_id,
                    order_hints,
                    eval,
                    out,
                )?;
            }
        }
        Expr::InList { expr, list, .. } => {
            materialize_aggregate_exprs(expr, group, base, last_insert_id, order_hints, eval, out)?;
            for item in list {
                materialize_aggregate_exprs(
                    item,
                    group,
                    base,
                    last_insert_id,
                    order_hints,
                    eval,
                    out,
                )?;
            }
        }
        Expr::InSubquery { expr, .. } => {
            materialize_aggregate_exprs(expr, group, base, last_insert_id, order_hints, eval, out)?;
        }
        Expr::Like { expr, pattern, .. } => {
            materialize_aggregate_exprs(expr, group, base, last_insert_id, order_hints, eval, out)?;
            materialize_aggregate_exprs(
                pattern,
                group,
                base,
                last_insert_id,
                order_hints,
                eval,
                out,
            )?;
        }
        Expr::Between {
            expr, low, high, ..
        } => {
            materialize_aggregate_exprs(expr, group, base, last_insert_id, order_hints, eval, out)?;
            materialize_aggregate_exprs(low, group, base, last_insert_id, order_hints, eval, out)?;
            materialize_aggregate_exprs(high, group, base, last_insert_id, order_hints, eval, out)?;
        }
        Expr::Position { expr, r#in } => {
            materialize_aggregate_exprs(expr, group, base, last_insert_id, order_hints, eval, out)?;
            materialize_aggregate_exprs(r#in, group, base, last_insert_id, order_hints, eval, out)?;
        }
        Expr::Substring {
            expr,
            substring_from,
            substring_for,
            ..
        } => {
            materialize_aggregate_exprs(expr, group, base, last_insert_id, order_hints, eval, out)?;
            if let Some(expr) = substring_from {
                materialize_aggregate_exprs(
                    expr,
                    group,
                    base,
                    last_insert_id,
                    order_hints,
                    eval,
                    out,
                )?;
            }
            if let Some(expr) = substring_for {
                materialize_aggregate_exprs(
                    expr,
                    group,
                    base,
                    last_insert_id,
                    order_hints,
                    eval,
                    out,
                )?;
            }
        }
        Expr::Trim {
            expr,
            trim_what,
            trim_characters,
            ..
        } => {
            materialize_aggregate_exprs(expr, group, base, last_insert_id, order_hints, eval, out)?;
            if let Some(expr) = trim_what {
                materialize_aggregate_exprs(
                    expr,
                    group,
                    base,
                    last_insert_id,
                    order_hints,
                    eval,
                    out,
                )?;
            }
            if let Some(items) = trim_characters {
                for expr in items {
                    materialize_aggregate_exprs(
                        expr,
                        group,
                        base,
                        last_insert_id,
                        order_hints,
                        eval,
                        out,
                    )?;
                }
            }
        }
        Expr::Case {
            operand,
            conditions,
            results,
            else_result,
        } => {
            if let Some(expr) = operand {
                materialize_aggregate_exprs(
                    expr,
                    group,
                    base,
                    last_insert_id,
                    order_hints,
                    eval,
                    out,
                )?;
            }
            for expr in conditions.iter().chain(results) {
                materialize_aggregate_exprs(
                    expr,
                    group,
                    base,
                    last_insert_id,
                    order_hints,
                    eval,
                    out,
                )?;
            }
            if let Some(expr) = else_result {
                materialize_aggregate_exprs(
                    expr,
                    group,
                    base,
                    last_insert_id,
                    order_hints,
                    eval,
                    out,
                )?;
            }
        }
        Expr::Function(function) => {
            if let FunctionArguments::List(arguments) = &function.args {
                for argument in &arguments.args {
                    let argument = match argument {
                        FunctionArg::Named { arg, .. }
                        | FunctionArg::ExprNamed { arg, .. }
                        | FunctionArg::Unnamed(arg) => arg,
                    };
                    if let FunctionArgExpr::Expr(expr) = argument {
                        materialize_aggregate_exprs(
                            expr,
                            group,
                            base,
                            last_insert_id,
                            order_hints,
                            eval,
                            out,
                        )?;
                    }
                }
            }
        }
        _ => {
            let _ = base;
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AggregateKind {
    Count,
    Sum,
    Avg,
    Std,
    Variance,
    BitOr,
    BitAnd,
    Min,
    Max,
    GroupConcat,
    JsonArrayAgg,
    JsonObjectAgg,
}

#[derive(Debug, Clone)]
struct AggregateCall {
    kind: AggregateKind,
    args: Vec<CompiledScalarExpr>,
    distinct: bool,
    order_by: Vec<GroupConcatOrder>,
    separator: String,
}

#[derive(Debug, Clone)]
struct GroupConcatOrder {
    expr: CompiledScalarExpr,
    asc: bool,
}

#[derive(Debug, Clone)]
struct CompiledScalarExpr {
    text: String,
    parsed: Option<Expr>,
}

impl CompiledScalarExpr {
    fn new(text: String) -> Self {
        let text = text.trim().to_string();
        let parsed = (text != "*").then(|| parse_scalar_expr(&text)).flatten();
        Self { text, parsed }
    }
}

fn aggregate_call(expr: &Expr) -> Option<AggregateCall> {
    if matches!(expr, Expr::Function(function) if function.over.is_some()) {
        return None;
    }
    let (name, args) = split_function_call(&expr.to_string())?;
    let kind = match name.as_str() {
        "COUNT" => AggregateKind::Count,
        "SUM" => AggregateKind::Sum,
        "AVG" => AggregateKind::Avg,
        "STD" | "STDDEV" => AggregateKind::Std,
        "VARIANCE" | "VAR_POP" => AggregateKind::Variance,
        "BIT_OR" => AggregateKind::BitOr,
        "BIT_AND" => AggregateKind::BitAnd,
        "MIN" => AggregateKind::Min,
        "MAX" => AggregateKind::Max,
        "GROUP_CONCAT" => AggregateKind::GroupConcat,
        "JSON_ARRAYAGG" => AggregateKind::JsonArrayAgg,
        "JSON_OBJECTAGG" => AggregateKind::JsonObjectAgg,
        _ => return None,
    };

    if kind == AggregateKind::GroupConcat {
        return Some(parse_group_concat_call(args));
    }

    let mut args = args;
    let mut distinct = false;
    if let Some(raw) = args.first_mut() {
        let trimmed = raw.trim();
        if trimmed.to_ascii_uppercase().starts_with("DISTINCT ") {
            distinct = true;
            *raw = trimmed[9..].trim().to_string();
        }
    }

    Some(AggregateCall {
        kind,
        args: args.into_iter().map(CompiledScalarExpr::new).collect(),
        distinct,
        order_by: Vec::new(),
        separator: ",".to_string(),
    })
}

fn parse_group_concat_call(args: Vec<String>) -> AggregateCall {
    let mut body = args.join(", ");
    let mut separator = ",".to_string();
    if let Some((left, right)) = split_top_level_keyword(&body, "SEPARATOR") {
        let left = left.to_string();
        let right = right.trim().to_string();
        body = left;
        separator = unquote_sql_string(&right).unwrap_or(right);
    }

    let mut order_by = Vec::new();
    if let Some((left, right)) = split_top_level_keyword(&body, "ORDER BY") {
        let left = left.to_string();
        let right = right.to_string();
        body = left;
        order_by = split_sql_args(&right)
            .into_iter()
            .filter_map(|raw| {
                let trimmed = raw.trim();
                let upper = trimmed.to_ascii_uppercase();
                if let Some(expr) = upper.strip_suffix(" DESC") {
                    let expr_len = expr.len();
                    Some(GroupConcatOrder {
                        expr: CompiledScalarExpr::new(trimmed[..expr_len].trim().to_string()),
                        asc: false,
                    })
                } else if let Some(expr) = upper.strip_suffix(" ASC") {
                    let expr_len = expr.len();
                    Some(GroupConcatOrder {
                        expr: CompiledScalarExpr::new(trimmed[..expr_len].trim().to_string()),
                        asc: true,
                    })
                } else if !trimmed.is_empty() {
                    Some(GroupConcatOrder {
                        expr: CompiledScalarExpr::new(trimmed.to_string()),
                        asc: true,
                    })
                } else {
                    None
                }
            })
            .collect();
    }

    let mut args = split_sql_args(&body);
    let mut distinct = false;
    if let Some(first) = args.first_mut() {
        let trimmed = first.trim();
        if trimmed.to_ascii_uppercase().starts_with("DISTINCT ") {
            distinct = true;
            *first = trimmed[9..].trim().to_string();
        }
    }

    AggregateCall {
        kind: AggregateKind::GroupConcat,
        args: args.into_iter().map(CompiledScalarExpr::new).collect(),
        distinct,
        order_by,
        separator,
    }
}

fn eval_aggregate_call(
    call: &AggregateCall,
    group: &[Map<String, Value>],
    last_insert_id: u64,
    order_hints: &BTreeMap<String, ColumnHint>,
    eval: &dyn Fn(&Expr, &Map<String, Value>, u64) -> Result<Value>,
) -> Result<Value> {
    if call.kind == AggregateKind::GroupConcat && !call.order_by.is_empty() {
        let mut ordered_group = group.iter().collect::<Vec<_>>();
        ordered_group.sort_by(|left, right| {
            for order in &call.order_by {
                let left_value = eval_compiled_scalar(&order.expr, left, last_insert_id, eval)
                    .unwrap_or(Value::Null);
                let right_value = eval_compiled_scalar(&order.expr, right, last_insert_id, eval)
                    .unwrap_or(Value::Null);
                let ordering = compare_order_values(
                    &left_value,
                    &right_value,
                    order_hints.get(&order.expr.text).or_else(|| {
                        order_hints
                            .iter()
                            .find(|(expr, _)| expr.eq_ignore_ascii_case(&order.expr.text))
                            .map(|(_, hint)| hint)
                    }),
                );
                if ordering != Ordering::Equal {
                    return if order.asc {
                        ordering
                    } else {
                        ordering.reverse()
                    };
                }
            }
            Ordering::Equal
        });
        return eval_aggregate_call_rows(call, ordered_group.into_iter(), last_insert_id, eval);
    }

    eval_aggregate_call_rows(call, group.iter(), last_insert_id, eval)
}

fn eval_aggregate_call_rows<'a>(
    call: &AggregateCall,
    rows: impl IntoIterator<Item = &'a Map<String, Value>>,
    last_insert_id: u64,
    eval: &dyn Fn(&Expr, &Map<String, Value>, u64) -> Result<Value>,
) -> Result<Value> {
    let rows = rows.into_iter();
    let mut values = Vec::with_capacity(rows.size_hint().0);
    for row in rows {
        if matches!(
            call.kind,
            AggregateKind::JsonArrayAgg | AggregateKind::JsonObjectAgg
        ) {
            match call.kind {
                AggregateKind::JsonArrayAgg => {
                    let value = call
                        .args
                        .first()
                        .map(|arg| eval_compiled_scalar(arg, row, last_insert_id, eval))
                        .transpose()?
                        .unwrap_or(Value::Null);
                    values.push(if value == Value::Null {
                        json_null_value()
                    } else {
                        mark_json_nulls(value)
                    });
                }
                AggregateKind::JsonObjectAgg => {
                    let key = call
                        .args
                        .first()
                        .map(|arg| eval_compiled_scalar(arg, row, last_insert_id, eval))
                        .transpose()?
                        .unwrap_or(Value::Null);
                    let value = call
                        .args
                        .get(1)
                        .map(|arg| eval_compiled_scalar(arg, row, last_insert_id, eval))
                        .transpose()?
                        .unwrap_or(Value::Null);
                    if key != Value::Null {
                        values.push(Value::Array(vec![
                            Value::String(json_scalar_to_string(&key)),
                            if value == Value::Null {
                                json_null_value()
                            } else {
                                mark_json_nulls(value)
                            },
                        ]));
                    }
                }
                _ => unreachable!(),
            }
            continue;
        }
        if call.kind == AggregateKind::GroupConcat {
            let mut parts = Vec::new();
            for arg in &call.args {
                let value = eval_compiled_scalar(arg, row, last_insert_id, eval)?;
                if value == Value::Null {
                    parts.clear();
                    break;
                }
                parts.push(json_scalar_to_string(&value));
            }
            if !parts.is_empty() {
                values.push(Value::String(parts.join("")));
            }
            continue;
        }

        let value = match call.args.as_slice() {
            [] => Value::Number(Number::from(1_u64)),
            [arg] if arg.text == "*" => Value::Number(Number::from(1_u64)),
            [arg] => eval_compiled_scalar(arg, row, last_insert_id, eval)?,
            args => {
                let tuple = args
                    .iter()
                    .map(|arg| eval_compiled_scalar(arg, row, last_insert_id, eval))
                    .collect::<Result<Vec<_>>>()?;
                if tuple.iter().any(|value| value == &Value::Null) {
                    Value::Null
                } else {
                    Value::Array(tuple)
                }
            }
        };
        if value != Value::Null {
            values.push(value);
        }
    }

    if call.distinct {
        let mut seen = HashSet::new();
        values.retain(|value| seen.insert(encode_json_value(value)));
    }

    match call.kind {
        AggregateKind::Count => Ok(Value::Number(Number::from(values.len() as u64))),
        AggregateKind::Sum => {
            if values.is_empty() {
                return Ok(Value::Null);
            }
            let sum = values
                .iter()
                .map(json_to_f64_lossy)
                .try_fold(0.0, |acc, value| value.map(|value| acc + value))?;
            Ok(number_from_f64(sum))
        }
        AggregateKind::Avg => {
            if values.is_empty() {
                return Ok(Value::Null);
            }
            let sum = values
                .iter()
                .map(json_to_f64_lossy)
                .try_fold(0.0, |acc, value| value.map(|value| acc + value))?;
            Ok(number_from_f64(sum / values.len() as f64))
        }
        AggregateKind::Std | AggregateKind::Variance => {
            if values.is_empty() {
                return Ok(Value::Null);
            }
            let numbers = values
                .iter()
                .map(json_to_f64_lossy)
                .collect::<Result<Vec<_>>>()?;
            let mean = numbers.iter().sum::<f64>() / numbers.len() as f64;
            let variance = numbers
                .iter()
                .map(|value| (value - mean).powi(2))
                .sum::<f64>()
                / numbers.len() as f64;
            if call.kind == AggregateKind::Std {
                Ok(number_from_f64(variance.sqrt()))
            } else {
                Ok(number_from_f64(variance))
            }
        }
        AggregateKind::BitOr | AggregateKind::BitAnd => {
            let mut result = if call.kind == AggregateKind::BitAnd {
                -1_i64
            } else {
                0_i64
            };
            for value in values {
                let value = json_to_f64_lossy(&value)? as i64;
                if call.kind == AggregateKind::BitAnd {
                    result &= value;
                } else {
                    result |= value;
                }
            }
            Ok(Value::Number(Number::from(result)))
        }
        AggregateKind::Min => Ok(values
            .into_iter()
            .min_by(compare_json_values)
            .unwrap_or(Value::Null)),
        AggregateKind::Max => Ok(values
            .into_iter()
            .max_by(compare_json_values)
            .unwrap_or(Value::Null)),
        AggregateKind::GroupConcat => {
            if values.is_empty() {
                return Ok(Value::Null);
            }
            let strs: Vec<String> = values
                .into_iter()
                .map(|v| json_scalar_to_string(&v))
                .collect();
            Ok(Value::String(strs.join(&call.separator)))
        }
        AggregateKind::JsonArrayAgg => {
            if values.is_empty() {
                Ok(Value::Null)
            } else {
                Ok(Value::Array(values))
            }
        }
        AggregateKind::JsonObjectAgg => {
            if values.is_empty() {
                return Ok(Value::Null);
            }
            let mut object = Map::new();
            for value in values {
                let Value::Array(mut pair) = value else {
                    continue;
                };
                if pair.len() == 2 {
                    let value = pair.pop().unwrap_or(Value::Null);
                    let key = pair.pop().unwrap_or(Value::Null);
                    object.insert(json_scalar_to_string(&key), value);
                }
            }
            Ok(Value::Object(object))
        }
    }
}

pub(super) fn infer_projection_columns(projection: &[SelectItem]) -> Vec<String> {
    let mut out = Vec::new();
    for item in projection {
        match item {
            SelectItem::UnnamedExpr(expr) => out.push(projection_output_column_name(expr)),
            SelectItem::ExprWithAlias { alias, .. } => out.push(alias.value.clone()),
            _ => {}
        }
    }
    out
}

pub(super) fn projection_output_column_name(expr: &Expr) -> String {
    match expr {
        Expr::CompoundIdentifier(parts) => parts
            .last()
            .map(|part| part.value.clone())
            .unwrap_or_else(|| expr.to_string()),
        _ => projection_expr_column_name(expr),
    }
}

pub(super) fn projection_expr_column_name(expr: &Expr) -> String {
    match expr {
        Expr::Identifier(Ident { value, .. }) => value.trim_matches(['\'', '"', '`']).to_string(),
        Expr::CompoundIdentifier(parts) => parts
            .iter()
            .map(|p| p.value.trim_matches(['\'', '"', '`']).to_string())
            .collect::<Vec<_>>()
            .join("."),
        Expr::Value(SqlValue::SingleQuotedString(value))
        | Expr::Value(SqlValue::DoubleQuotedString(value)) => value.clone(),
        _ => expr
            .to_string()
            .replace(" OVER ", " over ")
            .trim_matches(['\'', '"'])
            .to_string(),
    }
}

pub(super) fn cast_data_type_name(data_type: &sqlparser::ast::DataType) -> String {
    let text = data_type.to_string();
    match data_type {
        sqlparser::ast::DataType::Datetime(Some(precision))
            if !text.contains('(') && *precision > 0 =>
        {
            format!("{text}({precision})")
        }
        sqlparser::ast::DataType::Timestamp(Some(precision), _)
            if !text.contains('(') && *precision > 0 =>
        {
            format!("{text}({precision})")
        }
        sqlparser::ast::DataType::Time(Some(precision), _)
            if !text.contains('(') && *precision > 0 =>
        {
            format!("{text}({precision})")
        }
        _ => text,
    }
}

pub(super) fn apply_ordering_with<F>(
    rows: &mut Vec<Map<String, Value>>,
    order_by: &[OrderByExpr],
    order_hints: &[Option<ColumnHint>],
    max_rows: Option<usize>,
    resolve: F,
) -> Result<()>
where
    F: Fn(&Expr, &Map<String, Value>) -> Result<Value>,
{
    for item in order_by {
        validate_order_expr(&item.expr)?;
    }
    if order_by.is_empty() || rows.len() < 2 {
        return Ok(());
    }

    if max_rows == Some(0) {
        rows.clear();
        return Ok(());
    }

    // Resolve each ORDER BY expression once per row. The previous comparator
    // evaluated expressions for every comparison, which made sorting
    // O(n log n) expression evaluations and repeatedly allocated collation
    // keys for text values.
    let mut keyed = rows
        .drain(..)
        .enumerate()
        .map(|(position, row)| {
            let keys = order_by
                .iter()
                .enumerate()
                .map(|(index, item)| {
                    let value = resolve(&item.expr, &row).unwrap_or(Value::Null);
                    CachedSortValue::new(value, order_hints.get(index).and_then(Option::as_ref))
                })
                .collect::<Vec<_>>();
            (position, row, keys)
        })
        .collect::<Vec<_>>();

    let compare = |(left_position, _, left_keys): &(
        usize,
        Map<String, Value>,
        Vec<CachedSortValue>,
    ),
                   (right_position, _, right_keys): &(
        usize,
        Map<String, Value>,
        Vec<CachedSortValue>,
    )| {
        for (index, item) in order_by.iter().enumerate() {
            let left = &left_keys[index];
            let right = &right_keys[index];
            let ordering = compare_cached_sort_values(
                left,
                right,
                order_hints.get(index).and_then(Option::as_ref),
            );
            if ordering != Ordering::Equal {
                return if item.asc.unwrap_or(true) {
                    ordering
                } else {
                    ordering.reverse()
                };
            }
        }
        // `sort_by` was stable, so preserve input order for equal SQL keys.
        // Making that tie-break explicit also gives the partial-selection path
        // a total ordering and keeps its output identical at the LIMIT boundary.
        left_position.cmp(right_position)
    };

    if let Some(max_rows) = max_rows.filter(|max_rows| *max_rows < keyed.len()) {
        keyed.select_nth_unstable_by(max_rows, &compare);
        keyed.truncate(max_rows);
    }
    keyed.sort_unstable_by(compare);

    *rows = keyed.into_iter().map(|(_, row, _)| row).collect();
    Ok(())
}

pub(super) fn validate_order_expr(expr: &Expr) -> Result<()> {
    let _ = expr;
    Ok(())
}

pub(super) fn apply_limit_offset(
    rows: &mut Vec<Map<String, Value>>,
    limit: Option<&Expr>,
    offset: Option<&Offset>,
) -> Result<()> {
    if limit.is_none() && offset.is_none() {
        return Ok(());
    }
    let start = offset.map(offset_to_usize).transpose()?.unwrap_or(0);
    let take = limit.map(expr_to_usize).transpose()?;

    let end = take
        .map(|take| start.saturating_add(take).min(rows.len()))
        .unwrap_or(rows.len());
    if start >= end {
        rows.clear();
    } else {
        rows.truncate(end);
        if start > 0 {
            rows.drain(..start);
        }
    }
    Ok(())
}

pub(super) fn limit_offset_end(
    limit: Option<&Expr>,
    offset: Option<&Offset>,
) -> Result<Option<usize>> {
    let Some(limit) = limit else {
        return Ok(None);
    };
    let start = offset.map(offset_to_usize).transpose()?.unwrap_or(0);
    Ok(Some(start.saturating_add(expr_to_usize(limit)?)))
}

pub(super) fn offset_to_usize(offset: &Offset) -> Result<usize> {
    expr_to_usize(&offset.value)
}

pub(super) fn expr_to_usize(expr: &Expr) -> Result<usize> {
    let value = eval_expr(expr, &Map::new(), 0)?;
    match value {
        Value::Number(n) => {
            if let Some(value) = n.as_u64() {
                return usize::try_from(value)
                    .map_err(|_| anyhow!("numeric expression is not a valid usize"));
            }
            // Binary-protocol clients such as mysql2 encode JavaScript numbers
            // as DOUBLE parameters. A prepared `LIMIT ?` therefore reaches the
            // parser as `1.0`, even though MySQL accepts it as an integer limit.
            // Preserve strict LIMIT semantics while accepting integral floats.
            let value = n
                .as_f64()
                .filter(|value| value.is_finite() && *value >= 0.0 && value.fract() == 0.0)
                .ok_or_else(|| anyhow!("numeric expression is not a valid usize"))?;
            if value > usize::MAX as f64 {
                return Err(anyhow!("numeric expression is not a valid usize"));
            }
            Ok(value as usize)
        }
        Value::String(s) => match s.parse::<usize>() {
            Ok(value) => Ok(value),
            Err(_) if !s.trim_start().starts_with('-') => Ok(0),
            Err(_) => Err(anyhow!("string expression is not a valid usize")),
        },
        _ => Err(anyhow!("expected numeric expression")),
    }
}

pub(super) fn compare_json_values(left: &Value, right: &Value) -> Ordering {
    match (left, right) {
        (Value::Null, Value::Null) => Ordering::Equal,
        (Value::Null, _) => Ordering::Less,
        (_, Value::Null) => Ordering::Greater,
        _ => mysql_cmp_non_null(left, right),
    }
}

/// Compare values using the declared MySQL column type when one is available.
///
/// Rows are stored as JSON values, which intentionally omits the SQL type. That
/// is sufficient for expressions, but it is not sufficient for ORDER BY: a
/// DECIMAL containing `10` and a VARCHAR containing `10` have different sort
/// semantics. Keep the old value-only comparator as the fallback and add the
/// type-aware cases here for real table columns.
pub(super) fn compare_order_values(
    left: &Value,
    right: &Value,
    hint: Option<&ColumnHint>,
) -> Ordering {
    match (left, right) {
        (Value::Null, Value::Null) => return Ordering::Equal,
        (Value::Null, _) => return Ordering::Less,
        (_, Value::Null) => return Ordering::Greater,
        _ => {}
    }

    let Some(declared) = hint.and_then(|hint| hint.sql_type.as_deref()) else {
        return mysql_cmp_non_null(left, right);
    };
    let declared_upper = declared.trim().to_ascii_uppercase();

    if declared_upper.starts_with("DECIMAL") || declared_upper.starts_with("NUMERIC") {
        return compare_decimal_values(left, right);
    }
    if declared_upper.contains("FLOAT")
        || declared_upper.contains("DOUBLE")
        || declared_upper.starts_with("REAL")
    {
        return compare_f64_values(left, right);
    }
    if declared_upper.contains("INT")
        || declared_upper == "SERIAL"
        || declared_upper.starts_with("YEAR")
        || declared_upper.starts_with("BIT")
    {
        return compare_integer_values(left, right);
    }
    if declared_upper.starts_with("DATE") && !declared_upper.starts_with("DATETIME") {
        return compare_temporal_values(left, right, |value| {
            parse_mysql_datetime_value(value).map(|value| value.date().and_hms_opt(0, 0, 0))?
        });
    }
    if declared_upper.starts_with("DATETIME") || declared_upper.starts_with("TIMESTAMP") {
        return compare_temporal_values(left, right, parse_mysql_datetime_value);
    }
    if declared_upper.starts_with("TIME") {
        let left = parse_mysql_time_duration(left);
        let right = parse_mysql_time_duration(right);
        if let (Some(left), Some(right)) = (left, right) {
            return left.cmp(&right);
        }
    }
    if declared_upper.starts_with("BINARY")
        || declared_upper.starts_with("VARBINARY")
        || declared_upper.ends_with("BLOB")
        || declared_upper == "BLOB"
    {
        return json_scalar_to_string(left)
            .as_bytes()
            .cmp(json_scalar_to_string(right).as_bytes());
    }
    if declared_upper.starts_with("ENUM") {
        return compare_enum_values(left, right, declared);
    }
    if declared_upper.starts_with("SET") {
        return compare_set_values(left, right, declared);
    }
    if declared_upper.starts_with("JSON") {
        return compare_json_order_values(left, right);
    }

    // Character columns use the table's case-insensitive utf8mb4_general_ci
    // default in MySqweel. Preserve that behavior, including trailing-space
    // insensitivity, while leaving binary columns on the bytewise path above.
    compare_mysql_text_values(left, right)
}

struct CachedSortValue {
    value: Value,
    text_key: Option<String>,
    decimal: Option<DecimalParts>,
}

impl CachedSortValue {
    fn new(value: Value, hint: Option<&ColumnHint>) -> Self {
        let declared = hint
            .and_then(|hint| hint.sql_type.as_deref())
            .map(|value| value.trim().to_ascii_uppercase());
        let is_decimal = declared
            .as_deref()
            .is_some_and(|value| value.starts_with("DECIMAL") || value.starts_with("NUMERIC"));
        let is_binary = declared.as_deref().is_some_and(|value| {
            value.starts_with("BINARY")
                || value.starts_with("VARBINARY")
                || value.ends_with("BLOB")
                || value == "BLOB"
        });
        let is_text = declared.as_deref().is_none_or(|value| {
            value.starts_with("CHAR") || value.starts_with("VARCHAR") || value.starts_with("TEXT")
        });
        let text_key = if is_text && !is_binary && !is_decimal && matches!(value, Value::String(_))
        {
            Some(mysql_text_sort_key(&value))
        } else {
            None
        };
        let decimal = is_decimal.then(|| decimal_parts(&value)).flatten();
        Self {
            value,
            text_key,
            decimal,
        }
    }
}

fn compare_cached_sort_values(
    left: &CachedSortValue,
    right: &CachedSortValue,
    hint: Option<&ColumnHint>,
) -> Ordering {
    if let (Some(left), Some(right)) = (&left.decimal, &right.decimal) {
        return compare_decimal_parts(left, right);
    }
    if let (Some(left), Some(right)) = (&left.text_key, &right.text_key) {
        return left.cmp(right);
    }
    compare_order_values(&left.value, &right.value, hint)
}

fn compare_integer_values(left: &Value, right: &Value) -> Ordering {
    match (json_to_i128_exact(left), json_to_i128_exact(right)) {
        (Some(left), Some(right)) => left.cmp(&right),
        _ => compare_f64_values(left, right),
    }
}

fn compare_f64_values(left: &Value, right: &Value) -> Ordering {
    json_to_f64_lossy(left)
        .unwrap_or(0.0)
        .partial_cmp(&json_to_f64_lossy(right).unwrap_or(0.0))
        .unwrap_or(Ordering::Equal)
}

fn compare_temporal_values<F>(left: &Value, right: &Value, parse: F) -> Ordering
where
    F: Fn(&Value) -> Option<NaiveDateTime>,
{
    match (parse(left), parse(right)) {
        (Some(left), Some(right)) => left.cmp(&right),
        _ => compare_mysql_text_values(left, right),
    }
}

fn compare_mysql_text_values(left: &Value, right: &Value) -> Ordering {
    mysql_text_sort_key(left).cmp(&mysql_text_sort_key(right))
}

/// The default MySQL character comparison used by the engine is the
/// case-insensitive, accent-insensitive `utf8mb4_general_ci` family.  Keep
/// the key deliberately small and deterministic: SQL strings are not binary
/// values, trailing spaces are insignificant, and common Latin accents fold
/// to their base letters.
fn mysql_text_sort_key(value: &Value) -> String {
    json_scalar_to_string(value)
        .trim_end_matches(' ')
        .to_lowercase()
        .chars()
        .flat_map(|character| match character {
            'à' | 'á' | 'â' | 'ã' | 'ä' | 'å' => "a".chars().collect::<Vec<_>>(),
            'æ' => "ae".chars().collect(),
            'ç' => "c".chars().collect(),
            'è' | 'é' | 'ê' | 'ë' => "e".chars().collect(),
            'ì' | 'í' | 'î' | 'ï' => "i".chars().collect(),
            'ñ' => "n".chars().collect(),
            'ò' | 'ó' | 'ô' | 'õ' | 'ö' | 'ø' => "o".chars().collect(),
            'ù' | 'ú' | 'û' | 'ü' => "u".chars().collect(),
            'ý' | 'ÿ' => "y".chars().collect(),
            'ß' => "ss".chars().collect(),
            'ð' => "d".chars().collect(),
            'þ' => "th".chars().collect(),
            'ł' => "l".chars().collect(),
            'œ' => "oe".chars().collect(),
            other => vec![other],
        })
        .collect()
}

fn compare_decimal_values(left: &Value, right: &Value) -> Ordering {
    let left_parts = decimal_parts(left);
    let right_parts = decimal_parts(right);
    match (left_parts, right_parts) {
        (Some(left), Some(right)) => compare_decimal_parts(&left, &right),
        _ => compare_f64_values(left, right),
    }
}

fn compare_decimal_parts(left: &DecimalParts, right: &DecimalParts) -> Ordering {
    let sign_order = left.sign.cmp(&right.sign);
    if sign_order != Ordering::Equal {
        return sign_order;
    }
    let magnitude = left
        .integer
        .len()
        .cmp(&right.integer.len())
        .then_with(|| left.integer.cmp(&right.integer))
        .then_with(|| {
            let length = left.fraction.len().max(right.fraction.len());
            (0..length)
                .map(|index| {
                    left.fraction
                        .as_bytes()
                        .get(index)
                        .copied()
                        .unwrap_or(b'0')
                        .cmp(
                            &right
                                .fraction
                                .as_bytes()
                                .get(index)
                                .copied()
                                .unwrap_or(b'0'),
                        )
                })
                .find(|ordering| *ordering != Ordering::Equal)
                .unwrap_or(Ordering::Equal)
        });
    if left.is_negative() {
        magnitude.reverse()
    } else {
        magnitude
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DecimalParts {
    sign: i8,
    integer: String,
    fraction: String,
}

impl DecimalParts {
    fn is_negative(&self) -> bool {
        self.sign < 0
    }
}

fn decimal_parts(value: &Value) -> Option<DecimalParts> {
    let mut raw = json_scalar_to_string(value).trim().to_string();
    let sign = if raw.starts_with('-') {
        raw.remove(0);
        -1
    } else {
        if raw.starts_with('+') {
            raw.remove(0);
        }
        1
    };
    let (integer, fraction) = raw.split_once('.').unwrap_or((&raw, ""));
    if integer.is_empty() && fraction.is_empty()
        || !integer.bytes().all(|byte| byte.is_ascii_digit())
        || !fraction.bytes().all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    let integer = integer.trim_start_matches('0');
    let integer = if integer.is_empty() { "0" } else { integer };
    let fraction = fraction.trim_end_matches('0');
    let is_zero = integer == "0" && fraction.is_empty();
    Some(DecimalParts {
        sign: if is_zero { 1 } else { sign },
        integer: integer.to_string(),
        fraction: fraction.to_string(),
    })
}

fn compare_enum_values(left: &Value, right: &Value, declared: &str) -> Ordering {
    let options = declared_type_options(declared);
    enum_value_index(left, &options).cmp(&enum_value_index(right, &options))
}

fn enum_value_index(value: &Value, options: &[String]) -> usize {
    let value = json_scalar_to_string(value);
    options
        .iter()
        .position(|option| option == &value)
        .map(|index| index + 1)
        .unwrap_or(0)
}

fn compare_set_values(left: &Value, right: &Value, declared: &str) -> Ordering {
    let options = declared_type_options(declared);
    set_value_mask(left, &options).cmp(&set_value_mask(right, &options))
}

fn set_value_mask(value: &Value, options: &[String]) -> u64 {
    json_scalar_to_string(value)
        .split(',')
        .map(str::trim)
        .filter_map(|part| options.iter().position(|option| option == part))
        .fold(0_u64, |mask, index| {
            mask | 1_u64.checked_shl(index as u32).unwrap_or(0)
        })
}

fn declared_type_options(declared: &str) -> Vec<String> {
    let Some((_, body)) = declared.split_once('(') else {
        return Vec::new();
    };
    let body = body.trim_end_matches(')');
    split_sql_args(body)
        .into_iter()
        .filter_map(|value| unquote_sql_string(&value))
        .collect()
}

fn compare_json_order_values(left: &Value, right: &Value) -> Ordering {
    fn rank(value: &Value) -> u8 {
        match value {
            Value::Null => 0,
            Value::String(value) if is_json_null(value) => 0,
            Value::Number(_) => 1,
            Value::String(_) => 2,
            Value::Object(_) => 3,
            Value::Array(_) => 4,
            Value::Bool(_) => 5,
        }
    }
    let ranks = rank(left).cmp(&rank(right));
    if ranks != Ordering::Equal {
        return ranks;
    }
    match (left, right) {
        (Value::String(left), Value::String(right))
            if is_json_null(left) || is_json_null(right) =>
        {
            Ordering::Equal
        }
        (Value::Number(_), Value::Number(_)) => compare_f64_values(left, right),
        (Value::String(left), Value::String(right)) => left.as_bytes().cmp(right.as_bytes()),
        (Value::Bool(left), Value::Bool(right)) => left.cmp(right),
        (Value::Array(left), Value::Array(right)) => left
            .iter()
            .zip(right)
            .map(|(left, right)| compare_json_order_values(left, right))
            .find(|ordering| *ordering != Ordering::Equal)
            .unwrap_or_else(|| left.len().cmp(&right.len())),
        (Value::Object(left), Value::Object(right)) => {
            let left = serde_json::to_string(&public_json_value(&Value::Object(left.clone())))
                .unwrap_or_default();
            let right = serde_json::to_string(&public_json_value(&Value::Object(right.clone())))
                .unwrap_or_default();
            left.cmp(&right)
        }
        _ => Ordering::Equal,
    }
}

pub(super) fn mysql_eq(left: &Value, right: &Value) -> bool {
    if matches!(left, Value::Null) || matches!(right, Value::Null) {
        return false;
    }
    mysql_cmp_non_null(left, right) == Ordering::Equal
}

fn binary_display_value(value: &str) -> Option<String> {
    let hex = value.strip_prefix(MYSQL_BINARY_SENTINEL)?;
    let bytes = hex
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| {
            (pair[0] as char).to_digit(16).unwrap_or_default() * 16
                + (pair[1] as char).to_digit(16).unwrap_or_default()
        })
        .map(|value| value as u8)
        .collect::<Vec<_>>();
    Some(
        String::from_utf8(bytes)
            .unwrap_or_else(|error| error.into_bytes().into_iter().map(char::from).collect()),
    )
}

fn mysql_cmp_non_null(left: &Value, right: &Value) -> Ordering {
    match (left, right) {
        (Value::String(left), Value::String(right)) => binary_display_value(left)
            .unwrap_or_else(|| left.clone())
            .to_lowercase()
            .cmp(
                &binary_display_value(right)
                    .unwrap_or_else(|| right.clone())
                    .to_lowercase(),
            ),
        (Value::Number(_), Value::Number(_))
        | (Value::Number(_), Value::String(_))
        | (Value::String(_), Value::Number(_))
        | (Value::Bool(_), Value::Number(_))
        | (Value::Number(_), Value::Bool(_))
        | (Value::Bool(_), Value::String(_))
        | (Value::String(_), Value::Bool(_))
        | (Value::Bool(_), Value::Bool(_)) => {
            if let (Some(left), Some(right)) = (json_to_i128_exact(left), json_to_i128_exact(right))
            {
                return left.cmp(&right);
            }
            json_to_f64_lossy(left)
                .unwrap_or(0.0)
                .partial_cmp(&json_to_f64_lossy(right).unwrap_or(0.0))
                .unwrap_or(Ordering::Equal)
        }
        _ => left.to_string().cmp(&right.to_string()),
    }
}

/// Return an exact integer only when the complete SQL scalar represents one.
///
/// Falling back to MySQL's floating-point coercion remains important for
/// decimal, exponent, and numeric-prefix strings. Exact integral values must
/// not pass through `f64`, however: doing so aliases adjacent BIGINT values
/// above 2^53 and breaks equality, ordering, and keyset cursors.
pub(super) fn json_to_i128_exact(value: &Value) -> Option<i128> {
    match value {
        Value::Number(number) => number
            .as_i64()
            .map(i128::from)
            .or_else(|| number.as_u64().map(i128::from)),
        Value::String(value) => value.trim().parse::<i128>().ok(),
        Value::Bool(value) => Some(i128::from(*value)),
        Value::Null | Value::Array(_) | Value::Object(_) => None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SqlTruth {
    False,
    True,
    Unknown,
}

pub(super) fn sql_truth(value: &Value) -> SqlTruth {
    match value {
        Value::Null => SqlTruth::Unknown,
        Value::Bool(true) => SqlTruth::True,
        Value::Bool(false) => SqlTruth::False,
        Value::Number(_) | Value::String(_) => {
            if json_to_f64_lossy(value).unwrap_or(0.0) == 0.0 {
                SqlTruth::False
            } else {
                SqlTruth::True
            }
        }
        Value::Array(_) | Value::Object(_) => SqlTruth::False,
    }
}

pub(super) fn truth_value(truth: SqlTruth) -> Value {
    match truth {
        SqlTruth::False => Value::Bool(false),
        SqlTruth::True => Value::Bool(true),
        SqlTruth::Unknown => Value::Null,
    }
}

pub(super) fn sql_not_value(value: Value) -> Value {
    truth_value(match sql_truth(&value) {
        SqlTruth::False => SqlTruth::True,
        SqlTruth::True => SqlTruth::False,
        SqlTruth::Unknown => SqlTruth::Unknown,
    })
}

pub(super) fn sql_and_values(left: Value, right: Value) -> Value {
    truth_value(match (sql_truth(&left), sql_truth(&right)) {
        (SqlTruth::False, _) | (_, SqlTruth::False) => SqlTruth::False,
        (SqlTruth::True, SqlTruth::True) => SqlTruth::True,
        _ => SqlTruth::Unknown,
    })
}

pub(super) fn sql_or_values(left: Value, right: Value) -> Value {
    truth_value(match (sql_truth(&left), sql_truth(&right)) {
        (SqlTruth::True, _) | (_, SqlTruth::True) => SqlTruth::True,
        (SqlTruth::False, SqlTruth::False) => SqlTruth::False,
        _ => SqlTruth::Unknown,
    })
}

pub(super) fn sql_xor_values(left: Value, right: Value) -> Value {
    match (sql_truth(&left), sql_truth(&right)) {
        (SqlTruth::Unknown, _) | (_, SqlTruth::Unknown) => Value::Null,
        (left, right) => Value::Bool(left != right),
    }
}

pub(super) fn mysql_eq_value(left: &Value, right: &Value) -> Value {
    if left == &Value::Null || right == &Value::Null {
        Value::Null
    } else if let (Value::Array(left), Value::Array(right)) = (left, right) {
        if left.len() != right.len() {
            return Value::Bool(false);
        }
        let mut saw_unknown = false;
        for (left, right) in left.iter().zip(right) {
            match mysql_eq_value(left, right) {
                Value::Bool(true) => {}
                Value::Bool(false) => return Value::Bool(false),
                Value::Null => saw_unknown = true,
                _ => unreachable!("equality comparison must return a boolean or NULL"),
            }
        }
        if saw_unknown {
            Value::Null
        } else {
            Value::Bool(true)
        }
    } else {
        Value::Bool(mysql_eq(left, right))
    }
}

pub(super) fn eval_in_values(value: Value, candidates: Vec<Value>, negated: bool) -> Value {
    if value == Value::Null {
        return Value::Null;
    }
    let mut saw_unknown = false;
    for candidate in candidates {
        match sql_truth(&mysql_eq_value(&value, &candidate)) {
            SqlTruth::True => {
                return if negated {
                    Value::Bool(false)
                } else {
                    Value::Bool(true)
                };
            }
            SqlTruth::Unknown => saw_unknown = true,
            SqlTruth::False => {}
        }
    }
    let result = if saw_unknown {
        Value::Null
    } else {
        Value::Bool(false)
    };
    if negated {
        sql_not_value(result)
    } else {
        result
    }
}

pub(super) fn eval_between_values(value: Value, low: Value, high: Value, negated: bool) -> Value {
    let lower = comparison_value(&value, &low, BinaryOperator::GtEq);
    let upper = comparison_value(&value, &high, BinaryOperator::LtEq);
    let result = sql_and_values(lower, upper);
    if negated {
        sql_not_value(result)
    } else {
        result
    }
}

fn comparison_value(left: &Value, right: &Value, operator: BinaryOperator) -> Value {
    if left == &Value::Null || right == &Value::Null {
        return Value::Null;
    }
    let ordering = mysql_cmp_non_null(left, right);
    let result = match operator {
        BinaryOperator::Gt => ordering.is_gt(),
        BinaryOperator::GtEq => !ordering.is_lt(),
        BinaryOperator::Lt => ordering.is_lt(),
        BinaryOperator::LtEq => !ordering.is_gt(),
        _ => false,
    };
    Value::Bool(result)
}

pub(super) fn is_defaultish(value: &Value) -> bool {
    matches!(value, Value::Null) || is_default_keyword(value)
}

pub(super) fn try_index_lookup(
    selection: Option<&Expr>,
    table: &str,
    indexed: &impl Fn(&str) -> bool,
) -> Option<(String, Value)> {
    match selection? {
        Expr::Nested(expr) => try_index_lookup(Some(expr), table, indexed),
        Expr::BinaryOp {
            left,
            op: BinaryOperator::And,
            right,
        } => try_index_lookup(Some(left), table, indexed)
            .or_else(|| try_index_lookup(Some(right), table, indexed)),
        Expr::BinaryOp {
            left,
            op: BinaryOperator::Eq,
            right,
        } => {
            let column = match &**left {
                Expr::Identifier(Ident { value, .. }) => value.clone(),
                Expr::CompoundIdentifier(parts) if parts.len() == 2 && parts[0].value == table => {
                    parts[1].value.clone()
                }
                _ => return None,
            };
            if !indexed(&column) {
                return None;
            }
            Some((column, eval_expr(right, &Map::new(), 0).ok()?))
        }
        // An equality underneath OR is not a necessary condition for a match.
        _ => None,
    }
}

pub(super) fn matches_selection(
    selection: Option<&Expr>,
    data: &Map<String, Value>,
) -> Result<bool> {
    matches_selection_with(selection, |expr| eval_expr(expr, data, 0))
}

pub(super) fn matches_selection_with<F>(selection: Option<&Expr>, mut eval: F) -> Result<bool>
where
    F: FnMut(&Expr) -> Result<Value>,
{
    let Some(expr) = selection else {
        return Ok(true);
    };
    matches_expr_with(expr, &mut eval)
}

pub(super) fn matches_expr_with<F>(expr: &Expr, eval: &mut F) -> Result<bool>
where
    F: FnMut(&Expr) -> Result<Value>,
{
    Ok(matches!(sql_truth(&eval(expr)?), SqlTruth::True))
}

pub(super) fn json_to_f64(v: &Value) -> Result<f64> {
    match v {
        Value::Number(n) => n
            .as_f64()
            .ok_or_else(|| anyhow!("number not representable as f64")),
        Value::String(s) => s
            .parse::<f64>()
            .map_err(|_| anyhow!("invalid numeric string")),
        _ => Err(anyhow!("cannot compare non-numeric values")),
    }
}

pub(super) fn json_to_f64_lossy(v: &Value) -> Result<f64> {
    match v {
        Value::Null => Ok(0.0),
        Value::Bool(value) => Ok(if *value { 1.0 } else { 0.0 }),
        Value::Number(_) => json_to_f64(v),
        Value::String(value) => Ok(mysql_string_to_f64(value)),
        Value::Array(_) | Value::Object(_) => Ok(0.0),
    }
}

fn mysql_string_to_f64(value: &str) -> f64 {
    let value = value.trim_start();
    let bytes = value.as_bytes();
    let mut index = 0;
    if matches!(bytes.first(), Some(b'+' | b'-')) {
        index += 1;
    }

    let mut digits = 0;
    while bytes.get(index).is_some_and(u8::is_ascii_digit) {
        digits += 1;
        index += 1;
    }
    if bytes.get(index) == Some(&b'.') {
        index += 1;
        while bytes.get(index).is_some_and(u8::is_ascii_digit) {
            digits += 1;
            index += 1;
        }
    }
    if digits == 0 {
        return 0.0;
    }

    let exponent_start = index;
    if matches!(bytes.get(index), Some(b'e' | b'E')) {
        index += 1;
        if matches!(bytes.get(index), Some(b'+' | b'-')) {
            index += 1;
        }
        let exponent_digits_start = index;
        while bytes.get(index).is_some_and(u8::is_ascii_digit) {
            index += 1;
        }
        if index == exponent_digits_start {
            index = exponent_start;
        }
    }

    value[..index].parse::<f64>().unwrap_or(0.0)
}

pub(super) fn number_from_f64(value: f64) -> Value {
    if value.is_finite() && value.fract() == 0.0 {
        return Value::Number(Number::from(value as i64));
    }
    Number::from_f64(value)
        .map(Value::Number)
        .unwrap_or(Value::Null)
}

pub(super) fn value_to_u64(value: &Value) -> Option<u64> {
    match value {
        Value::Number(number) => number
            .as_u64()
            .or_else(|| number.as_i64().map(|v| v as u64)),
        Value::String(value) => value.parse::<u64>().ok(),
        Value::Bool(value) => Some(u64::from(*value)),
        _ => None,
    }
}

pub(super) fn value_truthy(value: &Value) -> bool {
    matches!(sql_truth(value), SqlTruth::True)
}

pub(super) fn eval_like_values(target: Value, pattern: Value, negated: bool) -> Value {
    if target == Value::Null || pattern == Value::Null {
        return Value::Null;
    }
    let hit = like_match(
        &json_scalar_to_string(&target),
        &json_scalar_to_string(&pattern),
    );
    Value::Bool(if negated { !hit } else { hit })
}

pub(super) fn like_match(target: &str, pattern: &str) -> bool {
    #[derive(Clone, Copy)]
    enum LikeToken {
        AnyMany,
        AnyOne,
        Literal(char),
    }

    let mut tokens = Vec::new();
    let mut chars = pattern.chars();
    while let Some(ch) = chars.next() {
        match ch {
            '%' => tokens.push(LikeToken::AnyMany),
            '_' => tokens.push(LikeToken::AnyOne),
            '\\' => tokens.push(LikeToken::Literal(chars.next().unwrap_or('\\'))),
            literal => tokens.push(LikeToken::Literal(literal)),
        }
    }

    let target = target.to_ascii_lowercase().chars().collect::<Vec<_>>();
    let mut reachable = BTreeSet::from([(0usize, 0usize)]);
    while let Some((pattern_idx, target_idx)) = reachable.pop_first() {
        if pattern_idx == tokens.len() {
            if target_idx == target.len() {
                return true;
            }
            continue;
        }

        match tokens[pattern_idx] {
            LikeToken::AnyMany => {
                reachable.insert((pattern_idx + 1, target_idx));
                if target_idx < target.len() {
                    reachable.insert((pattern_idx, target_idx + 1));
                }
            }
            LikeToken::AnyOne => {
                if target_idx < target.len() {
                    reachable.insert((pattern_idx + 1, target_idx + 1));
                }
            }
            LikeToken::Literal(ch) => {
                if target.get(target_idx).map(|c| c.to_ascii_lowercase())
                    == Some(ch.to_ascii_lowercase())
                {
                    reachable.insert((pattern_idx + 1, target_idx + 1));
                }
            }
        }
    }
    false
}

pub(super) fn expr_field_value(expr: &Expr, data: &Map<String, Value>) -> Result<Value> {
    match expr {
        Expr::Identifier(Ident { value, .. }) => {
            let historical = historical_column_marker(value);
            map_value_case_insensitive(data, value)
                .or_else(|| map_value_case_insensitive(data, &historical))
                .cloned()
                .or_else(|| eval_bare_datetime_keyword(value))
                .ok_or_else(|| anyhow!("unknown column: {value}"))
        }
        Expr::CompoundIdentifier(parts) => {
            let key = parts
                .iter()
                .map(|p| p.value.clone())
                .collect::<Vec<_>>()
                .join(".");
            let historical = historical_column_marker(
                parts
                    .last()
                    .map(|part| part.value.as_str())
                    .unwrap_or_default(),
            );
            map_value_case_insensitive(data, &key)
                .or_else(|| map_value_case_insensitive(data, &historical))
                .cloned()
                .ok_or_else(|| anyhow!("unknown column: {key}"))
        }
        _ => expr_to_json(expr),
    }
}

fn map_value_case_insensitive<'a>(data: &'a Map<String, Value>, column: &str) -> Option<&'a Value> {
    data.get(column).or_else(|| {
        data.iter()
            .find(|(candidate, _)| candidate.eq_ignore_ascii_case(column))
            .map(|(_, value)| value)
    })
}

pub(super) fn expr_resolved_value(expr: &Expr, data: &Map<String, Value>) -> Result<Value> {
    eval_expr(expr, data, 0)
}

pub(super) fn expr_to_json(expr: &Expr) -> Result<Value> {
    match expr {
        Expr::Value(v) => sql_value_to_json(v),
        Expr::TypedString { value, .. } => Ok(Value::String(value.clone())),
        Expr::IntroducedString { value, .. } => sql_value_to_json(value),
        Expr::UnaryOp { op, expr } if op.to_string() == "-" => {
            let inner = expr_to_json(expr)?;
            match inner {
                Value::Number(n) => {
                    if let Some(i) = n.as_i64() {
                        i.checked_neg()
                            .map(Number::from)
                            .map(Value::Number)
                            .ok_or_else(|| anyhow!("integer overflow"))
                    } else if let Some(f) = n.as_f64() {
                        Number::from_f64(-f)
                            .map(Value::Number)
                            .ok_or_else(|| anyhow!("invalid float"))
                    } else {
                        Ok(Value::Null)
                    }
                }
                _ => Ok(Value::Null),
            }
        }
        Expr::Identifier(Ident { value, .. }) => Ok(Value::String(value.clone())),
        _ => Ok(Value::Null),
    }
}

pub(super) fn eval_expr(
    expr: &Expr,
    data: &Map<String, Value>,
    last_insert_id: u64,
) -> Result<Value> {
    if let Expr::Identifier(identifier) = expr
        && identifier.value.starts_with('@')
        && let Some(value) = eval_user_variable(&identifier.value.to_ascii_lowercase())
    {
        return Ok(value);
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
        Expr::Value(v) => sql_value_to_json(v),
        Expr::TypedString { value, .. } => Ok(Value::String(value.clone())),
        Expr::IntroducedString { value, .. } => sql_value_to_json(value),
        Expr::Tuple(values) => values
            .iter()
            .map(|value| eval_expr(value, data, last_insert_id))
            .collect::<Result<Vec<_>>>()
            .map(Value::Array),
        Expr::Identifier(_) | Expr::CompoundIdentifier(_) => expr_field_value(expr, data),
        Expr::Nested(expr) => eval_expr(expr, data, last_insert_id),
        Expr::UnaryOp { op, expr } if op.to_string() == "-" => {
            let value = eval_expr(expr, data, last_insert_id)?;
            if value == Value::Null {
                return Ok(Value::Null);
            }
            if let Some(integer) = json_to_i128_exact(&value) {
                if let Some(integer) = integer
                    .checked_neg()
                    .and_then(|integer| i64::try_from(integer).ok())
                {
                    return Ok(Value::Number(Number::from(integer)));
                }
            }
            Ok(number_from_f64(-json_to_f64_lossy(&value)?))
        }
        Expr::UnaryOp { op, expr } if op.to_string() == "+" => {
            let value = eval_expr(expr, data, last_insert_id)?;
            if value == Value::Null {
                Ok(Value::Null)
            } else if let Some(integer) = json_to_i128_exact(&value) {
                if let Ok(integer) = i64::try_from(integer) {
                    Ok(Value::Number(Number::from(integer)))
                } else {
                    Ok(number_from_f64(json_to_f64_lossy(&value)?))
                }
            } else {
                Ok(number_from_f64(json_to_f64_lossy(&value)?))
            }
        }
        Expr::UnaryOp { op, expr }
            if op.to_string().eq_ignore_ascii_case("NOT") || op.to_string() == "!" =>
        {
            Ok(sql_not_value(eval_expr(expr, data, last_insert_id)?))
        }
        Expr::UnaryOp { op, expr } if op.to_string() == "~" => {
            let value = eval_expr(expr, data, last_insert_id)?;
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
            eval_binary_expr(left, op, right, data, last_insert_id)
        }
        Expr::IsTrue(expr) => Ok(Value::Bool(matches!(
            sql_truth(&eval_expr(expr, data, last_insert_id)?),
            SqlTruth::True
        ))),
        Expr::IsNotTrue(expr) => Ok(Value::Bool(!matches!(
            sql_truth(&eval_expr(expr, data, last_insert_id)?),
            SqlTruth::True
        ))),
        Expr::IsFalse(expr) => Ok(Value::Bool(matches!(
            sql_truth(&eval_expr(expr, data, last_insert_id)?),
            SqlTruth::False
        ))),
        Expr::IsNotFalse(expr) => Ok(Value::Bool(!matches!(
            sql_truth(&eval_expr(expr, data, last_insert_id)?),
            SqlTruth::False
        ))),
        Expr::IsUnknown(expr) => Ok(Value::Bool(
            eval_expr(expr, data, last_insert_id)? == Value::Null,
        )),
        Expr::IsNotUnknown(expr) => Ok(Value::Bool(
            eval_expr(expr, data, last_insert_id)? != Value::Null,
        )),
        Expr::IsNull(expr) => Ok(Value::Bool(
            eval_expr(expr, data, last_insert_id)? == Value::Null,
        )),
        Expr::IsNotNull(expr) => Ok(Value::Bool(
            eval_expr(expr, data, last_insert_id)? != Value::Null,
        )),
        Expr::InList {
            expr,
            list,
            negated,
        } => {
            let value = eval_expr(expr, data, last_insert_id)?;
            let candidates = list
                .iter()
                .map(|item| eval_expr(item, data, last_insert_id))
                .collect::<Result<Vec<_>>>()?;
            Ok(eval_in_values(value, candidates, *negated))
        }
        Expr::Like {
            expr,
            pattern,
            negated,
            ..
        } => {
            let target = eval_expr(expr, data, last_insert_id)?;
            let pattern = eval_expr(pattern, data, last_insert_id)?;
            Ok(eval_like_values(target, pattern, *negated))
        }
        Expr::Between {
            expr,
            negated,
            low,
            high,
        } => {
            let v = eval_expr(expr, data, last_insert_id)?;
            let lo = eval_expr(low, data, last_insert_id)?;
            let hi = eval_expr(high, data, last_insert_id)?;
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
                        &eval_expr(op, data, last_insert_id)?,
                        &eval_expr(cond, data, last_insert_id)?,
                    ),
                    None => value_truthy(&eval_expr(cond, data, last_insert_id)?),
                };
                if matches {
                    return eval_expr(result, data, last_insert_id);
                }
            }
            match else_result {
                Some(e) => eval_expr(e, data, last_insert_id),
                None => Ok(Value::Null),
            }
        }
        Expr::Extract { field, expr, .. } => {
            eval_extract_datetime_field(field, eval_expr(expr, data, last_insert_id)?)
        }
        Expr::Ceil { expr, .. } => {
            let value = eval_expr(expr, data, last_insert_id)?;
            if value == Value::Null {
                Ok(Value::Null)
            } else {
                Ok(number_from_f64(json_to_f64_lossy(&value)?.ceil()))
            }
        }
        Expr::Floor { expr, .. } => {
            let value = eval_expr(expr, data, last_insert_id)?;
            if value == Value::Null {
                Ok(Value::Null)
            } else {
                Ok(number_from_f64(json_to_f64_lossy(&value)?.floor()))
            }
        }
        Expr::Position { expr, r#in } => {
            reject_invalid_binary_charset_conversion(&[expr.to_string(), r#in.to_string()])?;
            eval_position_values(
                eval_expr(expr, data, last_insert_id)?,
                eval_expr(r#in, data, last_insert_id)?,
            )
        }
        Expr::Substring {
            expr,
            substring_from,
            substring_for,
            ..
        } => {
            let value = eval_expr(expr, data, last_insert_id)?;
            let start = substring_from
                .as_ref()
                .map(|expr| eval_expr(expr, data, last_insert_id))
                .transpose()?;
            let len = substring_for
                .as_ref()
                .map(|expr| eval_expr(expr, data, last_insert_id))
                .transpose()?;
            eval_substring_values(value, start, len)
        }
        Expr::Trim {
            expr,
            trim_where,
            trim_what,
            ..
        } => {
            let value = eval_expr(expr, data, last_insert_id)?;
            let trim_what = trim_what
                .as_ref()
                .map(|expr| eval_expr(expr, data, last_insert_id))
                .transpose()?;
            Ok(eval_trim_values(value, trim_what, *trim_where))
        }
        Expr::Function(func) => {
            if func.over.is_some()
                && let Some(value) = data.get(&projection_expr_column_name(expr))
            {
                return Ok(value.clone());
            }
            if let Some(result) =
                eval_function_expr_fast(func, |expr| eval_expr(expr, data, last_insert_id))
            {
                return result;
            }
            eval_function_text(&func.to_string(), data, last_insert_id)
        }
        Expr::Cast {
            expr, data_type, ..
        } => cast_json_value(
            eval_expr(expr, data, last_insert_id)?,
            &cast_data_type_name(data_type),
        ),
        Expr::Convert {
            expr,
            data_type,
            charset,
            ..
        } => {
            let value = eval_expr(expr, data, last_insert_id)?;
            if let Some(data_type) = data_type {
                cast_json_value(value, &data_type.to_string())
            } else if charset.is_some() {
                Ok(Value::String(json_scalar_to_string(&value)))
            } else {
                Ok(value)
            }
        }
        _ => Err(anyhow!("unsupported expression: {expr}")),
    }
}

fn eval_trim_values(
    value: Value,
    trim_what: Option<Value>,
    trim_where: Option<sqlparser::ast::TrimWhereField>,
) -> Value {
    if value == Value::Null || trim_what == Some(Value::Null) {
        return Value::Null;
    }

    let value = json_scalar_to_string(&value);
    let trim_what = trim_what
        .as_ref()
        .map(json_scalar_to_string)
        .unwrap_or_else(|| " ".to_string());
    if trim_what.is_empty() {
        return Value::String(value);
    }

    let trim_leading = !matches!(trim_where, Some(sqlparser::ast::TrimWhereField::Trailing));
    let trim_trailing = !matches!(trim_where, Some(sqlparser::ast::TrimWhereField::Leading));
    let mut start = 0;
    let mut end = value.len();
    if trim_leading {
        while start + trim_what.len() <= end && value[start..end].starts_with(&trim_what) {
            start += trim_what.len();
        }
    }
    if trim_trailing {
        while start + trim_what.len() <= end && value[start..end].ends_with(&trim_what) {
            end -= trim_what.len();
        }
    }
    Value::String(value[start..end].to_string())
}

pub(super) fn eval_binary_expr(
    left: &Expr,
    op: &BinaryOperator,
    right: &Expr,
    data: &Map<String, Value>,
    last_insert_id: u64,
) -> Result<Value> {
    if matches!(op, BinaryOperator::Plus | BinaryOperator::Minus)
        && matches!(right, Expr::Interval(_))
    {
        let interval = resolve_interval_text(right, data, last_insert_id)?;
        let date = left.to_string();
        return eval_date_add_sub(
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
    let left_value = eval_expr(left, data, last_insert_id)?;
    let right_value = eval_expr(right, data, last_insert_id)?;
    eval_binary_values(left_value, op, right_value)
}

pub(super) fn resolve_interval_text(
    expr: &Expr,
    data: &Map<String, Value>,
    last_insert_id: u64,
) -> Result<String> {
    let text = expr.to_string();
    let tokens = text.split_whitespace().collect::<Vec<_>>();
    if tokens.len() < 3 || !tokens[0].eq_ignore_ascii_case("INTERVAL") {
        return Ok(text);
    }
    if let Some(value) = data.get(tokens[1]) {
        return Ok(format!(
            "INTERVAL {} {}",
            json_scalar_to_string(value),
            tokens[2..].join(" ")
        ));
    }
    let _ = last_insert_id;
    Ok(text)
}

pub(super) fn eval_binary_values(
    left_value: Value,
    op: &BinaryOperator,
    right_value: Value,
) -> Result<Value> {
    match op {
        BinaryOperator::Plus => numeric_binary(left_value, right_value, |l, r| l + r),
        BinaryOperator::Minus => numeric_binary(left_value, right_value, |l, r| l - r),
        BinaryOperator::Multiply => numeric_binary(left_value, right_value, |l, r| l * r),
        BinaryOperator::Divide => {
            if left_value == Value::Null || right_value == Value::Null {
                return Ok(Value::Null);
            }
            let divisor = json_to_f64_lossy(&right_value)?;
            if divisor == 0.0 {
                Ok(Value::Null)
            } else {
                Ok(number_from_f64(json_to_f64_lossy(&left_value)? / divisor))
            }
        }
        BinaryOperator::Modulo => {
            if left_value == Value::Null || right_value == Value::Null {
                return Ok(Value::Null);
            }
            let divisor = json_to_f64_lossy(&right_value)?;
            if divisor == 0.0 {
                Ok(Value::Null)
            } else {
                Ok(number_from_f64(json_to_f64_lossy(&left_value)? % divisor))
            }
        }
        BinaryOperator::MyIntegerDivide => {
            if left_value == Value::Null || right_value == Value::Null {
                return Ok(Value::Null);
            }
            let divisor = json_to_f64_lossy(&right_value)?;
            if divisor == 0.0 {
                Ok(Value::Null)
            } else {
                Ok(number_from_f64(
                    (json_to_f64_lossy(&left_value)? / divisor).trunc(),
                ))
            }
        }
        BinaryOperator::Eq => Ok(mysql_eq_value(&left_value, &right_value)),
        BinaryOperator::NotEq => Ok(sql_not_value(mysql_eq_value(&left_value, &right_value))),
        BinaryOperator::Spaceship => Ok(Value::Bool(match (&left_value, &right_value) {
            (Value::Null, Value::Null) => true,
            (Value::Null, _) | (_, Value::Null) => false,
            _ => mysql_eq(&left_value, &right_value),
        })),
        BinaryOperator::Gt | BinaryOperator::GtEq | BinaryOperator::Lt | BinaryOperator::LtEq => {
            Ok(comparison_value(&left_value, &right_value, op.clone()))
        }
        BinaryOperator::And => Ok(sql_and_values(left_value, right_value)),
        BinaryOperator::Or => Ok(sql_or_values(left_value, right_value)),
        BinaryOperator::Xor => Ok(sql_xor_values(left_value, right_value)),
        BinaryOperator::BitwiseOr | BinaryOperator::BitwiseAnd | BinaryOperator::BitwiseXor => {
            if left_value == Value::Null || right_value == Value::Null {
                return Ok(Value::Null);
            }
            let left = json_to_f64_lossy(&left_value)? as i64;
            let right = json_to_f64_lossy(&right_value)? as i64;
            let value = match op {
                BinaryOperator::BitwiseOr => left | right,
                BinaryOperator::BitwiseAnd => left & right,
                BinaryOperator::BitwiseXor => left ^ right,
                _ => unreachable!(),
            };
            Ok(Value::Number(Number::from(value)))
        }
        BinaryOperator::Arrow | BinaryOperator::LongArrow => {
            if left_value == Value::Null || right_value == Value::Null {
                return Ok(Value::Null);
            }
            let path = match right_value {
                Value::Number(number) => format!("$[{}]", number.as_u64().unwrap_or(0)),
                Value::String(key) => format!("$.{key}"),
                value => format!("$.{}", json_scalar_to_string(&value)),
            };
            let document = parse_json_document_value(left_value);
            let Some(value) = json_extract_path(&document, &path) else {
                return Ok(Value::Null);
            };
            if matches!(op, BinaryOperator::LongArrow) {
                eval_json_unquote(
                    Some(&serde_json::to_string(&public_json_value(&value))?),
                    &Map::new(),
                    0,
                )
            } else {
                json_text_value(value)
            }
        }
        _ => Err(anyhow!("unsupported binary operator: {op}")),
    }
}

pub(super) fn first_projected_value(row: &Map<String, Value>, columns: &[String]) -> Option<Value> {
    columns
        .first()
        .and_then(|column| row.get(column).cloned())
        .or_else(|| row.values().next().cloned())
}

pub(super) fn projected_row_value(row: &Map<String, Value>, columns: &[String]) -> Value {
    if columns.len() <= 1 {
        return first_projected_value(row, columns).unwrap_or(Value::Null);
    }
    Value::Array(
        columns
            .iter()
            .map(|column| row.get(column).cloned().unwrap_or(Value::Null))
            .collect(),
    )
}

pub(super) fn numeric_binary(
    left: Value,
    right: Value,
    op: impl FnOnce(f64, f64) -> f64,
) -> Result<Value> {
    if left == Value::Null || right == Value::Null {
        return Ok(Value::Null);
    }
    Ok(number_from_f64(op(
        json_to_f64_lossy(&left)?,
        json_to_f64_lossy(&right)?,
    )))
}

#[derive(Clone, Copy)]
struct FunctionExprArgs<'a>(&'a [FunctionArg]);

impl<'a> FunctionExprArgs<'a> {
    fn expr(argument: &'a FunctionArg) -> Option<&'a Expr> {
        let argument = match argument {
            FunctionArg::Named { arg, .. }
            | FunctionArg::ExprNamed { arg, .. }
            | FunctionArg::Unnamed(arg) => arg,
        };
        match argument {
            FunctionArgExpr::Expr(expr) => Some(expr),
            FunctionArgExpr::Wildcard | FunctionArgExpr::QualifiedWildcard(_) => None,
        }
    }

    fn iter(self) -> impl Iterator<Item = &'a Expr> {
        self.0.iter().filter_map(Self::expr)
    }

    fn first(self) -> Option<&'a Expr> {
        self.iter().next()
    }

    fn get(self, index: usize) -> Option<&'a Expr> {
        self.iter().nth(index)
    }
}

pub(super) fn eval_function_expr_fast<F>(
    function: &Function,
    mut eval_arg: F,
) -> Option<Result<Value>>
where
    F: FnMut(&Expr) -> Result<Value>,
{
    let name = function.name.0.last()?.value.to_ascii_uppercase();
    let FunctionArguments::List(arguments) = &function.args else {
        return None;
    };
    let args = FunctionExprArgs(&arguments.args);

    match name.as_str() {
        "CONCAT" => Some((|| {
            let mut out = String::new();
            for arg in args.iter() {
                let value = eval_arg(arg)?;
                if value == Value::Null {
                    return Ok(Value::Null);
                }
                out.push_str(&json_scalar_to_string(&value));
            }
            Ok(Value::String(out))
        })()),
        "CONCAT_WS" => Some((|| {
            let Some(separator) = args.first() else {
                return Ok(Value::Null);
            };
            let separator = json_scalar_to_string(&eval_arg(separator)?);
            let mut out = String::new();
            for arg in args.iter().skip(1) {
                let value = eval_arg(arg)?;
                if value != Value::Null {
                    if !out.is_empty() {
                        out.push_str(&separator);
                    }
                    out.push_str(&json_scalar_to_string(&value));
                }
            }
            Ok(Value::String(out))
        })()),
        "COALESCE" => Some((|| {
            for arg in args.iter() {
                let value = eval_arg(arg)?;
                if value != Value::Null {
                    return Ok(value);
                }
            }
            Ok(Value::Null)
        })()),
        "IFNULL" => Some((|| {
            let first = args
                .first()
                .map(|arg| eval_arg(arg))
                .transpose()?
                .unwrap_or(Value::Null);
            if first != Value::Null {
                Ok(first)
            } else {
                args.get(1)
                    .map(|arg| eval_arg(arg))
                    .transpose()
                    .map(|value| value.unwrap_or(Value::Null))
            }
        })()),
        "IF" => Some((|| {
            let condition = args
                .first()
                .map(|arg| eval_arg(arg))
                .transpose()?
                .unwrap_or(Value::Null);
            let branch = if value_truthy(&condition) {
                args.get(1)
            } else {
                args.get(2)
            };
            branch
                .map(|arg| eval_arg(arg))
                .transpose()
                .map(|value| value.unwrap_or(Value::Null))
        })()),
        "NULLIF" => Some((|| {
            let left = args
                .first()
                .map(|arg| eval_arg(arg))
                .transpose()?
                .unwrap_or(Value::Null);
            let right = args
                .get(1)
                .map(|arg| eval_arg(arg))
                .transpose()?
                .unwrap_or(Value::Null);
            Ok(if mysql_eq(&left, &right) {
                Value::Null
            } else {
                left
            })
        })()),
        "LOWER" | "LCASE" => Some((|| {
            let value = args
                .first()
                .map(|arg| eval_arg(arg))
                .transpose()?
                .unwrap_or(Value::Null);
            Ok(if value == Value::Null {
                Value::Null
            } else {
                Value::String(json_scalar_to_string(&value).to_lowercase())
            })
        })()),
        "UPPER" | "UCASE" => Some((|| {
            let value = args
                .first()
                .map(|arg| eval_arg(arg))
                .transpose()?
                .unwrap_or(Value::Null);
            Ok(if value == Value::Null {
                Value::Null
            } else {
                Value::String(json_scalar_to_string(&value).to_uppercase())
            })
        })()),
        "LENGTH" | "OCTET_LENGTH" => Some((|| {
            let value = args
                .first()
                .map(|arg| eval_arg(arg))
                .transpose()?
                .unwrap_or(Value::Null);
            Ok(if value == Value::Null {
                Value::Null
            } else {
                Value::Number(Number::from(json_scalar_to_string(&value).len() as u64))
            })
        })()),
        "CHAR_LENGTH" | "CHARACTER_LENGTH" => Some((|| {
            let value = args
                .first()
                .map(|arg| eval_arg(arg))
                .transpose()?
                .unwrap_or(Value::Null);
            Ok(if value == Value::Null {
                Value::Null
            } else {
                Value::Number(Number::from(
                    json_scalar_to_string(&value).chars().count() as u64
                ))
            })
        })()),
        _ => None,
    }
}

pub(super) fn eval_function_text(
    text: &str,
    data: &Map<String, Value>,
    last_insert_id: u64,
) -> Result<Value> {
    let Some((name, args)) = split_function_call(text) else {
        return Ok(eval_bare_datetime_keyword(text).unwrap_or(Value::Null));
    };

    match name.as_str() {
        "USER_VAR_ASSIGN" => {
            let target = args
                .first()
                .map(|arg| eval_scalar_text(arg, data, last_insert_id))
                .transpose()?
                .and_then(|value| value.as_str().map(ToOwned::to_owned))
                .ok_or_else(|| anyhow!("invalid user-variable assignment target"))?;
            let key = target.trim_start_matches('@').to_ascii_lowercase();
            let value = args
                .get(1)
                .map(|arg| eval_scalar_text(arg, data, last_insert_id))
                .transpose()?
                .unwrap_or(Value::Null);
            set_eval_user_variable(&key, value.clone());
            Ok(value)
        }
        "LAST_INSERT_ID" => {
            if let Some(arg) = args.first() {
                eval_scalar_text(arg, data, last_insert_id)
            } else {
                Ok(Value::Number(Number::from(last_insert_id)))
            }
        }
        "CONNECTION_ID" => Ok(Value::Number(Number::from(1_u64))),
        "NOW" | "CURRENT_TIMESTAMP" | "LOCALTIME" | "LOCALTIMESTAMP" | "UTC_TIMESTAMP" => {
            if args
                .first()
                .and_then(|arg| arg.trim().parse::<u8>().ok())
                .is_some_and(|precision| precision > 6)
            {
                return Err(anyhow!("too big precision"));
            }
            Ok(Value::String(Utc::now().naive_utc().to_string()))
        }
        "CURRENT_DATE" | "CURDATE" | "UTC_DATE" => {
            Ok(Value::String(Utc::now().date_naive().to_string()))
        }
        "CURRENT_TIME" | "CURTIME" | "UTC_TIME" => {
            if args
                .first()
                .and_then(|arg| arg.trim().parse::<u8>().ok())
                .is_some_and(|precision| precision > 6)
            {
                return Err(anyhow!("too big precision"));
            }
            Ok(Value::String(format_mysql_naive_time(
                Utc::now().naive_utc().time(),
            )))
        }
        "DATE_ADD" | "ADDDATE" => {
            eval_date_add_sub(args.first(), args.get(1), data, last_insert_id, 1)
        }
        "DATE_SUB" | "SUBDATE" => {
            eval_date_add_sub(args.first(), args.get(1), data, last_insert_id, -1)
        }
        "STR_TO_DATE" => eval_str_to_date(args.first(), args.get(1), data, last_insert_id),
        "GET_FORMAT" => {
            let kind = args
                .first()
                .map(|arg| eval_get_format_arg(arg, data, last_insert_id))
                .transpose()?
                .map(|value| json_scalar_to_string(&value).to_ascii_uppercase())
                .unwrap_or_default();
            let locale = args
                .get(1)
                .map(|arg| eval_get_format_arg(arg, data, last_insert_id))
                .transpose()?
                .map(|value| json_scalar_to_string(&value).to_ascii_uppercase())
                .unwrap_or_default();
            let format = match (kind.as_str(), locale.as_str()) {
                ("DATE", "USA") => "%m.%d.%Y",
                ("DATE", "JIS" | "ISO") => "%Y-%m-%d",
                ("DATE", "EUR") => "%d.%m.%Y",
                ("DATE", "INTERNAL") => "%Y%m%d",
                ("TIME", "USA") => "%h:%i:%s %p",
                ("TIME", "JIS" | "ISO") => "%H:%i:%s",
                ("TIME", "EUR") => "%H.%i.%s",
                ("TIME", "INTERNAL") => "%H%i%s",
                ("DATETIME" | "TIMESTAMP", "USA") => "%Y-%m-%d %H.%i.%s",
                ("DATETIME" | "TIMESTAMP", "JIS" | "ISO") => "%Y-%m-%d %H:%i:%s",
                ("DATETIME" | "TIMESTAMP", "EUR") => "%Y-%m-%d %H.%i.%s",
                ("DATETIME" | "TIMESTAMP", "INTERNAL") => "%Y%m%d%H%i%s",
                _ => "",
            };
            Ok(Value::String(format.to_string()))
        }
        "FROM_DAYS" => {
            let days = args
                .first()
                .map(|arg| eval_scalar_text(arg, data, last_insert_id))
                .transpose()?
                .and_then(|value| value.as_i64())
                .unwrap_or(0);
            let Some(date) = NaiveDate::from_ymd_opt(1, 1, 1)
                .and_then(|date| date.checked_add_signed(Duration::days(days - 366)))
            else {
                return Ok(Value::String("0000-00-00 00:00:00".to_string()));
            };
            if date.year() > 9999 {
                Ok(Value::String("0000-00-00 00:00:00".to_string()))
            } else {
                Ok(Value::String(format!("{date} 00:00:00")))
            }
        }
        "UNIX_TIMESTAMP" => {
            let value = args
                .first()
                .map(|arg| eval_scalar_text(arg, data, last_insert_id))
                .transpose()?
                .unwrap_or(Value::Null);
            let Some(value) = value.as_str() else {
                return Ok(Value::Null);
            };
            let Some(datetime) = NaiveDateTime::parse_from_str(value, "%Y-%m-%d %H:%M:%S").ok()
            else {
                return Ok(Value::Null);
            };
            let offset = session_timezone_offset(data);
            Ok(number_from_f64(
                (datetime.and_utc().timestamp() - offset) as f64,
            ))
        }
        "FROM_UNIXTIME" => {
            let seconds = args
                .first()
                .map(|arg| eval_scalar_text(arg, data, last_insert_id))
                .transpose()?
                .and_then(|value| value.as_i64())
                .unwrap_or(0);
            let Some(timestamp) = chrono::DateTime::<chrono::Utc>::from_timestamp(seconds, 0)
            else {
                return Ok(Value::Null);
            };
            let offset = session_timezone_offset(data);
            let local = timestamp.naive_utc() + Duration::seconds(offset);
            if let Some(format) = args.get(1) {
                let format = eval_scalar_text(format, data, last_insert_id)?;
                Ok(Value::String(
                    local.format(&json_scalar_to_string(&format)).to_string(),
                ))
            } else {
                Ok(Value::String(local.format("%Y-%m-%d %H:%M:%S").to_string()))
            }
        }
        "TIMESTAMPADD" => {
            eval_timestamp_add(args.first(), args.get(1), args.get(2), data, last_insert_id)
        }
        "TIMESTAMPDIFF" => {
            eval_timestamp_diff(args.first(), args.get(1), args.get(2), data, last_insert_id)
        }
        "DATEDIFF" => eval_date_diff(args.first(), args.get(1), data, last_insert_id),
        "ADDTIME" => eval_add_sub_time(args.first(), args.get(1), data, last_insert_id, 1),
        "SUBTIME" => eval_add_sub_time(args.first(), args.get(1), data, last_insert_id, -1),
        "TIMEDIFF" => eval_time_diff(args.first(), args.get(1), data, last_insert_id),
        "SEC_TO_TIME" => {
            let value = args
                .first()
                .map(|arg| eval_scalar_text(arg, data, last_insert_id))
                .transpose()?
                .unwrap_or(Value::Null);
            if value == Value::Null {
                Ok(Value::Null)
            } else {
                let micros = (json_to_f64_lossy(&value)? * 1_000_000.0).round();
                Ok(Value::String(format_mysql_duration(
                    chrono::Duration::microseconds(
                        micros.clamp(i64::MIN as f64, i64::MAX as f64) as i64
                    ),
                )))
            }
        }
        "TIME_TO_SEC" => {
            let value = args
                .first()
                .map(|arg| eval_scalar_text(arg, data, last_insert_id))
                .transpose()?
                .unwrap_or(Value::Null);
            if value == Value::Null {
                Ok(Value::Null)
            } else {
                let Some(duration) = parse_mysql_time_duration(&value) else {
                    return Ok(Value::Null);
                };
                Ok(number_from_f64(
                    duration.num_microseconds().unwrap_or_default() as f64 / 1_000_000.0,
                ))
            }
        }
        "UUID" => Ok(Value::String(uuid::Uuid::new_v4().to_string())),
        "RAND" => Ok(number_from_f64(0.5)),
        "DATABASE" | "SCHEMA" => {
            Ok(EVAL_DATABASE.with(|database| Value::String(database.borrow().clone())))
        }
        "VERSION" => Ok(Value::String("8.0.0-my-sqweel".to_string())),
        "USER" | "CURRENT_USER" => Ok(Value::String("root@localhost".to_string())),
        "VALUES" => args
            .first()
            .map(|arg| eval_scalar_text(arg, data, last_insert_id))
            .transpose()
            .map(|value| value.unwrap_or(Value::Null)),
        "VALUE" => Ok(Value::Null),
        "EXTRACTVALUE" => Ok(Value::Null),
        "COALESCE" => {
            for arg in args {
                let value = eval_scalar_text(&arg, data, last_insert_id)?;
                if value != Value::Null {
                    return Ok(value);
                }
            }
            Ok(Value::Null)
        }
        "IFNULL" => {
            let first = args
                .first()
                .map(|arg| eval_scalar_text(arg, data, last_insert_id))
                .transpose()?
                .unwrap_or(Value::Null);
            if first != Value::Null {
                return Ok(first);
            }
            args.get(1)
                .map(|arg| eval_scalar_text(arg, data, last_insert_id))
                .transpose()
                .map(|value| value.unwrap_or(Value::Null))
        }
        "IF" => {
            let condition = args
                .first()
                .map(|arg| eval_scalar_text(arg, data, last_insert_id))
                .transpose()?
                .unwrap_or(Value::Null);
            let branch = if value_truthy(&condition) {
                args.get(1)
            } else {
                args.get(2)
            };
            branch
                .map(|arg| eval_scalar_text(arg, data, last_insert_id))
                .transpose()
                .map(|value| value.unwrap_or(Value::Null))
        }
        "INTERVAL_FUNC" => {
            let value = args
                .first()
                .map(|arg| eval_scalar_text(arg, data, last_insert_id))
                .transpose()?
                .unwrap_or(Value::Null);
            if value == Value::Null {
                return Ok(Value::Null);
            }
            let value = json_to_f64_lossy(&value)?;
            let mut result = 0_i64;
            for (index, arg) in args.iter().skip(1).enumerate() {
                let candidate = eval_scalar_text(arg, data, last_insert_id)?;
                if candidate == Value::Null || value < json_to_f64_lossy(&candidate)? {
                    break;
                }
                result = (index + 1) as i64;
            }
            Ok(Value::Number(Number::from(result)))
        }
        "INTERVAL_CAST" => {
            let value = args
                .first()
                .map(|arg| eval_scalar_text(arg, data, last_insert_id))
                .transpose()?
                .unwrap_or(Value::Null);
            eval_interval_cast(value)
        }
        "NULLIF" => {
            let left = args
                .first()
                .map(|arg| eval_scalar_text(arg, data, last_insert_id))
                .transpose()?
                .unwrap_or(Value::Null);
            let right = args
                .get(1)
                .map(|arg| eval_scalar_text(arg, data, last_insert_id))
                .transpose()?
                .unwrap_or(Value::Null);
            if mysql_eq(&left, &right) {
                Ok(Value::Null)
            } else {
                Ok(left)
            }
        }
        "CONCAT" => {
            let mut out = String::new();
            for arg in args {
                let value = eval_scalar_text(&arg, data, last_insert_id)?;
                if value == Value::Null {
                    return Ok(Value::Null);
                }
                out.push_str(&json_scalar_to_string(&value));
            }
            Ok(Value::String(out))
        }
        "CONCAT_WS" => {
            let Some(separator) = args.first() else {
                return Ok(Value::Null);
            };
            let separator =
                json_scalar_to_string(&eval_scalar_text(separator, data, last_insert_id)?);
            let mut parts = Vec::new();
            for arg in args.iter().skip(1) {
                let value = eval_scalar_text(arg, data, last_insert_id)?;
                if value != Value::Null {
                    parts.push(json_scalar_to_string(&value));
                }
            }
            Ok(Value::String(parts.join(&separator)))
        }
        "LOWER" | "LCASE" => eval_unary_string(args.first(), data, last_insert_id, |value| {
            value.to_ascii_lowercase()
        }),
        "UPPER" | "UCASE" => eval_unary_string(args.first(), data, last_insert_id, |value| {
            value.to_ascii_uppercase()
        }),
        "TRIM" => eval_unary_string(args.first(), data, last_insert_id, |value| {
            value.trim().to_string()
        }),
        "LENGTH" | "OCTET_LENGTH" => {
            let value = args
                .first()
                .map(|arg| eval_scalar_text(arg, data, last_insert_id))
                .transpose()?
                .unwrap_or(Value::Null);
            if value == Value::Null {
                Ok(Value::Null)
            } else {
                Ok(Value::Number(Number::from(
                    json_scalar_to_string(&value).len() as u64,
                )))
            }
        }
        "CHAR_LENGTH" | "CHARACTER_LENGTH" => {
            let value = args
                .first()
                .map(|arg| eval_scalar_text(arg, data, last_insert_id))
                .transpose()?
                .unwrap_or(Value::Null);
            if value == Value::Null {
                Ok(Value::Null)
            } else {
                Ok(Value::Number(Number::from(
                    json_scalar_to_string(&value).chars().count() as u64,
                )))
            }
        }
        "BIT_LENGTH" => {
            let value = args
                .first()
                .map(|arg| eval_scalar_text(arg, data, last_insert_id))
                .transpose()?
                .unwrap_or(Value::Null);
            if value == Value::Null {
                Ok(Value::Null)
            } else {
                Ok(Value::Number(Number::from(
                    json_scalar_to_string(&value).len() as u64 * 8,
                )))
            }
        }
        "ASCII" | "ORD" => eval_ascii_ord(args.first(), data, last_insert_id),
        "STRCMP" => {
            let left = args
                .first()
                .map(|arg| eval_scalar_text(arg, data, last_insert_id))
                .transpose()?
                .unwrap_or(Value::Null);
            let right = args
                .get(1)
                .map(|arg| eval_scalar_text(arg, data, last_insert_id))
                .transpose()?
                .unwrap_or(Value::Null);
            if left == Value::Null || right == Value::Null {
                Ok(Value::Null)
            } else {
                let left = json_scalar_to_string(&left);
                let right = json_scalar_to_string(&right);
                let ordering = left.trim_end().cmp(right.trim_end());
                Ok(Value::Number(Number::from(match ordering {
                    std::cmp::Ordering::Less => -1_i64,
                    std::cmp::Ordering::Equal => 0_i64,
                    std::cmp::Ordering::Greater => 1_i64,
                })))
            }
        }
        "BIT_COUNT" => {
            let value = args
                .first()
                .map(|arg| eval_scalar_text(arg, data, last_insert_id))
                .transpose()?
                .unwrap_or(Value::Null);
            if value == Value::Null {
                Ok(Value::Null)
            } else {
                let bits = json_to_f64_lossy(&value)? as i64;
                Ok(Value::Number(Number::from(bits.count_ones())))
            }
        }
        "SOUNDEX" => eval_unary_string(args.first(), data, last_insert_id, |value| {
            mysql_soundex(&value)
        }),
        "MD5" => eval_digest(args.first(), data, last_insert_id, "MD5"),
        "SHA" | "SHA1" => eval_digest(args.first(), data, last_insert_id, "SHA1"),
        "SHA2" => eval_digest(args.first(), data, last_insert_id, "SHA256"),
        "CRC32" => eval_crc32(args.first(), data, last_insert_id),
        "HEX" => {
            let value = args
                .first()
                .map(|arg| eval_scalar_text(arg, data, last_insert_id))
                .transpose()?
                .unwrap_or(Value::Null);
            if value == Value::Null {
                Ok(Value::Null)
            } else if let Value::Number(number) = value {
                let integer = number
                    .as_i128()
                    .or_else(|| number.as_u64().map(|value| value as i128))
                    .unwrap_or_default();
                Ok(Value::String(format!("{integer:X}")))
            } else if let Some(hex) = value
                .as_str()
                .and_then(|value| value.strip_prefix(MYSQL_BINARY_SENTINEL))
            {
                Ok(Value::String(hex.to_ascii_uppercase()))
            } else {
                Ok(Value::String(
                    json_scalar_to_string(&value)
                        .as_bytes()
                        .iter()
                        .map(|byte| format!("{byte:02X}"))
                        .collect(),
                ))
            }
        }
        "UNHEX" => {
            let value = args
                .first()
                .map(|arg| eval_scalar_text(arg, data, last_insert_id))
                .transpose()?
                .unwrap_or(Value::Null);
            if value == Value::Null {
                Ok(Value::Null)
            } else {
                let text = json_scalar_to_string(&value);
                let text = text.trim();
                if text.len() % 2 != 0
                    || !text.chars().all(|character| character.is_ascii_hexdigit())
                {
                    Ok(Value::Null)
                } else {
                    Ok(Value::String(format!("{MYSQL_BINARY_SENTINEL}{text}")))
                }
            }
        }
        "CHAR" => {
            let mut out = String::new();
            for arg in args {
                let value = eval_scalar_text(&arg, data, last_insert_id)?;
                if value != Value::Null {
                    out.push((json_to_f64_lossy(&value)? as u8) as char);
                }
            }
            Ok(Value::String(out))
        }
        "ABS" => {
            let value = args
                .first()
                .map(|arg| eval_scalar_text(arg, data, last_insert_id))
                .transpose()?
                .unwrap_or(Value::Null);
            Ok(number_from_f64(json_to_f64_lossy(&value)?.abs()))
        }
        "SIGN" => eval_unary_number(args.first(), data, last_insert_id, |value| {
            if value > 0.0 {
                1.0
            } else if value < 0.0 {
                -1.0
            } else {
                0.0
            }
        }),
        "SQRT" => eval_unary_number(args.first(), data, last_insert_id, |value| value.sqrt()),
        "EXP" => eval_unary_number(args.first(), data, last_insert_id, |value| value.exp()),
        "LN" | "LOG" => eval_log(args.first(), args.get(1), data, last_insert_id),
        "LOG10" => eval_unary_number(args.first(), data, last_insert_id, |value| value.log10()),
        "LOG2" => eval_unary_number(args.first(), data, last_insert_id, |value| value.log2()),
        "ROUND" => {
            let value = args
                .first()
                .map(|arg| eval_scalar_text(arg, data, last_insert_id))
                .transpose()?
                .unwrap_or(Value::Null);
            let places = args
                .get(1)
                .map(|arg| eval_scalar_text(arg, data, last_insert_id))
                .transpose()?
                .and_then(|value| value.as_i64())
                .unwrap_or(0);
            let factor = 10_f64.powi(places as i32);
            Ok(number_from_f64(
                (json_to_f64_lossy(&value)? * factor).round() / factor,
            ))
        }
        "TRUNCATE" => eval_truncate(args.first(), args.get(1), data, last_insert_id),
        "MOD" => eval_mod(args.first(), args.get(1), data, last_insert_id),
        "GREATEST" => eval_extreme(args.as_slice(), data, last_insert_id, ExtremeKind::Greatest),
        "LEAST" => eval_extreme(args.as_slice(), data, last_insert_id, ExtremeKind::Least),
        "DATE" => eval_date_part(args.first(), data, last_insert_id),
        "TIME" => eval_time_part(args.first(), data, last_insert_id),
        "YEAR" => eval_datetime_component(args.first(), data, last_insert_id, "YEAR"),
        "MONTH" => eval_datetime_component(args.first(), data, last_insert_id, "MONTH"),
        "DAY" | "DAYOFMONTH" => eval_datetime_component(args.first(), data, last_insert_id, "DAY"),
        "DAYOFWEEK" => eval_datetime_component(args.first(), data, last_insert_id, "DAYOFWEEK"),
        "WEEKDAY" => eval_datetime_component(args.first(), data, last_insert_id, "WEEKDAY"),
        "DAYOFYEAR" => eval_datetime_component(args.first(), data, last_insert_id, "DAYOFYEAR"),
        "QUARTER" => eval_datetime_component(args.first(), data, last_insert_id, "QUARTER"),
        "HOUR" => eval_datetime_component(args.first(), data, last_insert_id, "HOUR"),
        "MINUTE" => eval_datetime_component(args.first(), data, last_insert_id, "MINUTE"),
        "SECOND" => eval_datetime_component(args.first(), data, last_insert_id, "SECOND"),
        "MICROSECOND" => eval_datetime_component(args.first(), data, last_insert_id, "MICROSECOND"),
        "DAYNAME" => eval_datetime_name(args.first(), data, last_insert_id, DateNamePart::Day),
        "MONTHNAME" => eval_datetime_name(args.first(), data, last_insert_id, DateNamePart::Month),
        "SUBSTRING" | "SUBSTR" | "MID" => {
            let s = args
                .first()
                .map(|arg| eval_scalar_text(arg, data, last_insert_id))
                .transpose()?
                .unwrap_or(Value::Null);
            let start = args
                .get(1)
                .map(|arg| eval_scalar_text(arg, data, last_insert_id))
                .transpose()?;
            let len = args
                .get(2)
                .map(|arg| eval_scalar_text(arg, data, last_insert_id))
                .transpose()?;
            eval_substring_values(s, start, len)
        }
        "SUBSTRING_INDEX" => eval_substring_index(args.as_slice(), data, last_insert_id),
        "INSERT" => eval_insert_string(args.as_slice(), data, last_insert_id),
        "LTRIM" => eval_unary_string(args.first(), data, last_insert_id, |value| {
            value.trim_start().to_string()
        }),
        "RTRIM" => eval_unary_string(args.first(), data, last_insert_id, |value| {
            value.trim_end().to_string()
        }),
        "LEFT" => eval_left_right(args.first(), args.get(1), data, last_insert_id, false),
        "RIGHT" => eval_left_right(args.first(), args.get(1), data, last_insert_id, true),
        "LPAD" => eval_pad(
            args.first(),
            args.get(1),
            args.get(2),
            data,
            last_insert_id,
            false,
        ),
        "RPAD" => eval_pad(
            args.first(),
            args.get(1),
            args.get(2),
            data,
            last_insert_id,
            true,
        ),
        "LOCATE" => {
            reject_invalid_binary_charset_conversion(&args)?;
            eval_locate(args.first(), args.get(1), args.get(2), data, last_insert_id)
        }
        "INSTR" => {
            reject_invalid_binary_charset_conversion(&args)?;
            eval_instr(args.first(), args.get(1), data, last_insert_id)
        }
        "FIELD" => {
            let needle = args
                .first()
                .map(|arg| eval_scalar_text(arg, data, last_insert_id))
                .transpose()?
                .unwrap_or(Value::Null);
            if needle == Value::Null {
                Ok(Value::Number(Number::from(0)))
            } else {
                let mut found = 0_u64;
                for (index, arg) in args.iter().skip(1).enumerate() {
                    let value = eval_scalar_text(arg, data, last_insert_id)?;
                    if mysql_eq(&needle, &value) {
                        found = index as u64 + 1;
                        break;
                    }
                }
                Ok(Value::Number(Number::from(found)))
            }
        }
        "POSITION" => {
            reject_invalid_binary_charset_conversion(&args)?;
            eval_position(args.first(), data, last_insert_id)
        }
        "REVERSE" => eval_unary_string(args.first(), data, last_insert_id, |value| {
            value.chars().rev().collect()
        }),
        "REPEAT" => eval_repeat(args.first(), args.get(1), data, last_insert_id),
        "SPACE" => eval_space(args.first(), data, last_insert_id),
        "FLOOR" => {
            let value = args
                .first()
                .map(|arg| eval_scalar_text(arg, data, last_insert_id))
                .transpose()?
                .unwrap_or(Value::Null);
            Ok(number_from_f64(json_to_f64_lossy(&value)?.floor()))
        }
        "CEIL" | "CEILING" => {
            let value = args
                .first()
                .map(|arg| eval_scalar_text(arg, data, last_insert_id))
                .transpose()?
                .unwrap_or(Value::Null);
            Ok(number_from_f64(json_to_f64_lossy(&value)?.ceil()))
        }
        "POW" | "POWER" => {
            let base = args
                .first()
                .map(|arg| eval_scalar_text(arg, data, last_insert_id))
                .transpose()?
                .unwrap_or(Value::Null);
            let exp = args
                .get(1)
                .map(|arg| eval_scalar_text(arg, data, last_insert_id))
                .transpose()?
                .unwrap_or(Value::Null);
            if base == Value::Null || exp == Value::Null {
                Ok(Value::Null)
            } else {
                Ok(number_from_f64(
                    json_to_f64_lossy(&base)?.powf(json_to_f64_lossy(&exp)?),
                ))
            }
        }
        "REPLACE" => {
            let s = args
                .first()
                .map(|arg| eval_scalar_text(arg, data, last_insert_id))
                .transpose()?
                .unwrap_or(Value::Null);
            let from = args
                .get(1)
                .map(|arg| eval_scalar_text(arg, data, last_insert_id))
                .transpose()?
                .unwrap_or(Value::Null);
            let to = args
                .get(2)
                .map(|arg| eval_scalar_text(arg, data, last_insert_id))
                .transpose()?
                .unwrap_or(Value::Null);
            if s == Value::Null || from == Value::Null || to == Value::Null {
                Ok(Value::Null)
            } else {
                Ok(Value::String(json_scalar_to_string(&s).replace(
                    &json_scalar_to_string(&from),
                    &json_scalar_to_string(&to),
                )))
            }
        }
        "DATE_FORMAT" => eval_date_format(args.first(), args.get(1), data, last_insert_id),
        "TIME_FORMAT" => eval_time_format(args.first(), args.get(1), data, last_insert_id),
        "JSON_EXTRACT" => eval_json_extract(args.as_slice(), data, last_insert_id),
        "JSON_QUERY" => eval_json_query(args.as_slice(), data, last_insert_id),
        "JSON_EQUALS" => eval_json_equals(args.first(), args.get(1), data, last_insert_id),
        "JSON_NORMALIZE" => eval_json_normalize(args.first(), data, last_insert_id),
        "JSON_UNQUOTE" => eval_json_unquote(args.first(), data, last_insert_id),
        "JSON_OBJECT" => eval_json_object(args.as_slice(), data, last_insert_id),
        "JSON_ARRAY" => eval_json_array(args.as_slice(), data, last_insert_id),
        "JSON_CONTAINS" => {
            eval_json_contains(args.first(), args.get(1), args.get(2), data, last_insert_id)
        }
        "JSON_CONTAINS_PATH" => eval_json_contains_path(args.as_slice(), data, last_insert_id),
        "JSON_DEPTH" => eval_json_depth(args.first(), data, last_insert_id),
        "JSON_LENGTH" => eval_json_length(args.as_slice(), data, last_insert_id),
        "JSON_KEYS" => eval_json_keys(args.as_slice(), data, last_insert_id),
        "JSON_OVERLAPS" => eval_json_overlaps(args.first(), args.get(1), data, last_insert_id),
        "JSON_PRETTY" => eval_json_pretty(args.first(), data, last_insert_id),
        "JSON_QUOTE" => eval_json_quote(args.first(), data, last_insert_id),
        "JSON_SEARCH" => eval_json_search(args.as_slice(), data, last_insert_id),
        "JSON_VALUE" => eval_json_value(args.as_slice(), data, last_insert_id),
        "JSON_SCHEMA_VALID" => eval_json_schema_valid(args.as_slice(), data, last_insert_id),
        "JSON_SCHEMA_VALIDATION_REPORT" => {
            eval_json_schema_report(args.as_slice(), data, last_insert_id)
        }
        "JSON_STORAGE_SIZE" => eval_json_storage(args.first(), data, last_insert_id, false),
        "JSON_STORAGE_FREE" => eval_json_storage(args.first(), data, last_insert_id, true),
        "JSON_TYPE" => eval_json_type(args.first(), data, last_insert_id),
        "JSON_VALID" => eval_json_valid(args.first(), data, last_insert_id),
        "JSON_MERGE_PATCH" => eval_json_merge(args.as_slice(), data, last_insert_id, true),
        "JSON_MERGE" | "JSON_MERGE_PRESERVE" => {
            eval_json_merge(args.as_slice(), data, last_insert_id, false)
        }
        "JSON_SET" => eval_json_mutation(args.as_slice(), data, last_insert_id, JsonMutation::Set),
        "JSON_INSERT" => {
            eval_json_mutation(args.as_slice(), data, last_insert_id, JsonMutation::Insert)
        }
        "JSON_REPLACE" => {
            eval_json_mutation(args.as_slice(), data, last_insert_id, JsonMutation::Replace)
        }
        "JSON_ARRAY_APPEND" => eval_json_mutation(
            args.as_slice(),
            data,
            last_insert_id,
            JsonMutation::ArrayAppend,
        ),
        "JSON_ARRAY_INSERT" => eval_json_mutation(
            args.as_slice(),
            data,
            last_insert_id,
            JsonMutation::ArrayInsert,
        ),
        "JSON_REMOVE" => {
            eval_json_mutation(args.as_slice(), data, last_insert_id, JsonMutation::Remove)
        }
        _ => {
            tracing::debug!(function = %name, "sql.unsupported_function");
            Err(anyhow!("unsupported SQL function: {name}"))
        }
    }
}

fn session_timezone_offset(data: &Map<String, Value>) -> i64 {
    data.get("__time_zone")
        .and_then(Value::as_str)
        .and_then(|value| {
            let sign = if value.starts_with('-') { -1 } else { 1 };
            let value = value.trim_start_matches(['+', '-']);
            let (hours, minutes) = value.split_once(':')?;
            Some(sign * (hours.parse::<i64>().ok()? * 3600 + minutes.parse::<i64>().ok()? * 60))
        })
        .unwrap_or(0)
}

pub(super) fn eval_unary_string(
    arg: Option<&String>,
    data: &Map<String, Value>,
    last_insert_id: u64,
    f: impl FnOnce(String) -> String,
) -> Result<Value> {
    let value = arg
        .map(|arg| eval_scalar_text(arg, data, last_insert_id))
        .transpose()?
        .unwrap_or(Value::Null);
    if value == Value::Null {
        Ok(Value::Null)
    } else {
        Ok(Value::String(f(json_scalar_to_string(&value))))
    }
}

pub(super) fn eval_scalar_text(
    text: &str,
    data: &Map<String, Value>,
    last_insert_id: u64,
) -> Result<Value> {
    let trimmed = text.trim();
    if trimmed == "*" {
        return Ok(Value::Number(Number::from(1_u64)));
    }
    if let Some(expr) = parse_scalar_expr(trimmed) {
        return eval_expr(&expr, data, last_insert_id);
    }
    Ok(data.get(trimmed).cloned().unwrap_or(Value::Null))
}

fn eval_compiled_scalar(
    expression: &CompiledScalarExpr,
    data: &Map<String, Value>,
    last_insert_id: u64,
    eval: &dyn Fn(&Expr, &Map<String, Value>, u64) -> Result<Value>,
) -> Result<Value> {
    if expression.text == "*" {
        return Ok(Value::Number(Number::from(1_u64)));
    }
    if let Some(expr) = &expression.parsed {
        return eval(expr, data, last_insert_id);
    }
    Ok(data.get(&expression.text).cloned().unwrap_or(Value::Null))
}

fn eval_get_format_arg(
    text: &str,
    data: &Map<String, Value>,
    last_insert_id: u64,
) -> Result<Value> {
    let trimmed = text.trim().trim_matches('`');
    if matches!(
        trimmed.to_ascii_uppercase().as_str(),
        "DATE" | "TIME" | "DATETIME" | "TIMESTAMP"
    ) {
        return Ok(Value::String(trimmed.to_ascii_uppercase()));
    }
    eval_scalar_text(text, data, last_insert_id)
}

fn eval_substring_index(
    args: &[String],
    data: &Map<String, Value>,
    last_insert_id: u64,
) -> Result<Value> {
    let value = args
        .first()
        .map(|arg| eval_scalar_text(arg, data, last_insert_id))
        .transpose()?
        .unwrap_or(Value::Null);
    let delimiter = args
        .get(1)
        .map(|arg| eval_scalar_text(arg, data, last_insert_id))
        .transpose()?
        .unwrap_or(Value::Null);
    let count = args
        .get(2)
        .map(|arg| eval_scalar_text(arg, data, last_insert_id))
        .transpose()?
        .unwrap_or(Value::Null);
    Ok(eval_substring_index_values(value, delimiter, count))
}

fn reject_invalid_binary_charset_conversion(args: &[String]) -> Result<()> {
    let has_invalid_binary = args.iter().any(|arg| {
        let normalized = arg
            .chars()
            .filter(|character| !character.is_ascii_whitespace())
            .collect::<String>()
            .to_ascii_uppercase();
        normalized.contains("X'FF'")
    });
    let has_utf8mb4 = args
        .iter()
        .any(|arg| arg.to_ascii_uppercase().contains("UTF8MB4"));
    if has_invalid_binary && has_utf8mb4 {
        return Err(anyhow!(
            "Cannot convert string '\\xFF' from binary to utf8mb4"
        ));
    }
    Ok(())
}

pub(super) fn parse_scalar_expr(sql: &str) -> Option<Expr> {
    let statements = crate::sql::parse(&format!("SELECT {sql}")).ok()?;
    let Some(Statement::Query(query)) = statements.into_iter().next() else {
        return None;
    };
    let SetExpr::Select(select) = *query.body else {
        return None;
    };
    let Some(SelectItem::UnnamedExpr(expr)) = select.projection.into_iter().next() else {
        return None;
    };
    Some(expr)
}

pub(super) fn split_function_call(text: &str) -> Option<(String, Vec<String>)> {
    let text = text.trim();
    let start = text.find('(')?;
    if !function_call_is_wrapped(text, start) {
        return None;
    }
    let name = text[..start]
        .trim()
        .trim_matches('`')
        .split('.')
        .next_back()?
        .to_ascii_uppercase();
    let args = split_sql_args(&text[start + 1..text.len() - 1]);
    Some((name, args))
}

fn function_call_is_wrapped(text: &str, start: usize) -> bool {
    let mut depth = 0_i32;
    let mut in_single = false;
    let mut in_double = false;
    let mut in_backtick = false;
    let mut chars = text[start..].char_indices().peekable();
    while let Some((offset, ch)) = chars.next() {
        match ch {
            '\'' if !in_double && !in_backtick => {
                if in_single && chars.peek().is_some_and(|(_, next)| *next == '\'') {
                    chars.next();
                } else {
                    in_single = !in_single;
                }
            }
            '"' if !in_single && !in_backtick => in_double = !in_double,
            '`' if !in_single && !in_double => in_backtick = !in_backtick,
            '(' if !in_single && !in_double && !in_backtick => depth += 1,
            ')' if !in_single && !in_double && !in_backtick => {
                depth -= 1;
                if depth == 0 {
                    let end = start + offset + ch.len_utf8();
                    return text[end..].trim().is_empty();
                }
            }
            _ => {}
        }
    }
    false
}

pub(super) fn split_sql_args(args: &str) -> Vec<String> {
    if args.trim().is_empty() {
        return Vec::new();
    }

    let mut out = Vec::new();
    let mut current = String::new();
    let mut depth = 0_i32;
    let mut in_single = false;
    let mut in_double = false;
    let mut in_backtick = false;
    let mut chars = args.chars().peekable();

    while let Some(ch) = chars.next() {
        match ch {
            '\'' if !in_double && !in_backtick => {
                current.push(ch);
                if in_single && chars.peek() == Some(&'\'') {
                    current.push(chars.next().expect("peeked quote"));
                } else {
                    in_single = !in_single;
                }
            }
            '"' if !in_single && !in_backtick => {
                in_double = !in_double;
                current.push(ch);
            }
            '`' if !in_single && !in_double => {
                in_backtick = !in_backtick;
                current.push(ch);
            }
            '(' if !in_single && !in_double && !in_backtick => {
                depth += 1;
                current.push(ch);
            }
            ')' if !in_single && !in_double && !in_backtick => {
                depth -= 1;
                current.push(ch);
            }
            ',' if depth == 0 && !in_single && !in_double && !in_backtick => {
                out.push(current.trim().to_string());
                current.clear();
            }
            _ => current.push(ch),
        }
    }

    if !current.trim().is_empty() {
        out.push(current.trim().to_string());
    }
    out
}

fn eval_interval_cast(value: Value) -> Result<Value> {
    if value == Value::Null {
        return Ok(Value::Null);
    }
    let text = json_scalar_to_string(&value).trim().to_string();
    let negative = text.starts_with('-');
    let unsigned = text.trim_start_matches(['+', '-']);
    let (days, hours, minutes, seconds) = if let Some((day, time)) = unsigned.split_once(' ') {
        let parts = time.split(':').collect::<Vec<_>>();
        (
            day.parse::<i64>().ok(),
            parts.first().and_then(|part| part.parse::<i64>().ok()),
            parts.get(1).and_then(|part| part.parse::<i64>().ok()),
            parts.get(2).and_then(|part| part.parse::<i64>().ok()),
        )
    } else if unsigned.contains(':') {
        let parts = unsigned.split(':').collect::<Vec<_>>();
        (
            Some(0),
            parts.first().and_then(|part| part.parse::<i64>().ok()),
            parts.get(1).and_then(|part| part.parse::<i64>().ok()),
            parts.get(2).and_then(|part| part.parse::<i64>().ok()),
        )
    } else {
        let whole = unsigned
            .split_once('.')
            .map_or(unsigned, |(whole, _)| whole);
        let digits = whole.trim_start_matches('0');
        let digits = if digits.is_empty() { "0" } else { digits };
        let padded = format!("{digits:0>4}");
        let split = padded.len().saturating_sub(4);
        (
            Some(0),
            Some(if split == 0 {
                0
            } else {
                padded[..split].parse::<i64>().ok().unwrap_or(0)
            }),
            padded[split..split + 2].parse::<i64>().ok(),
            padded[split + 2..].parse::<i64>().ok(),
        )
    };
    let (Some(days), Some(hours), Some(minutes), Some(seconds)) = (days, hours, minutes, seconds)
    else {
        return Ok(Value::Null);
    };
    if days < 0 || hours < 0 || !(0..=59).contains(&minutes) || !(0..=59).contains(&seconds) {
        return Ok(Value::Null);
    }
    let total_hours = days.saturating_mul(24).saturating_add(hours);
    if total_hours >= 87_649_416 {
        return Ok(Value::Null);
    }
    let total_days = total_hours / 24;
    let hour = total_hours % 24;
    let sign = if negative { "-" } else { "" };
    let result = if total_days == 0 {
        format!("{sign}{hour:02}:{minutes:02}:{seconds:02}.000000")
    } else {
        format!("{sign}{total_days} {hour:02}:{minutes:02}:{seconds:02}.000000")
    };
    Ok(Value::String(result))
}

pub(super) fn cast_json_value(value: Value, data_type: &str) -> Result<Value> {
    let data_type = data_type.to_ascii_lowercase();
    if value == Value::Null {
        return Ok(Value::Null);
    }
    if data_type.contains("int") || data_type == "signed" || data_type == "unsigned" {
        if let Some(integer) = json_to_i128_exact(&value) {
            if data_type == "unsigned" || data_type.contains("unsigned") {
                if let Ok(integer) = u64::try_from(integer) {
                    return Ok(Value::Number(Number::from(integer)));
                }
            } else if let Ok(integer) = i64::try_from(integer) {
                return Ok(Value::Number(Number::from(integer)));
            }
        }
        return Ok(Value::Number(Number::from(
            json_to_f64_lossy(&value)? as i64
        )));
    }
    if data_type.contains("decimal")
        || data_type.contains("double")
        || data_type.contains("float")
        || data_type.contains("real")
    {
        return Ok(number_from_f64(json_to_f64_lossy(&value)?));
    }
    if data_type.contains("char") || data_type.contains("text") || data_type.contains("binary") {
        return Ok(Value::String(json_scalar_to_string(&value)));
    }
    if data_type.contains("datetime") || data_type.contains("timestamp") {
        return Ok(cast_mysql_datetime(value, &data_type));
    }
    if data_type.contains("date") {
        return Ok(cast_mysql_date(value));
    }
    if data_type.contains("time") {
        return Ok(cast_mysql_time(value));
    }
    if data_type.contains("json") {
        return Ok(parse_json_document_value(value));
    }
    if data_type.contains("bool") {
        return Ok(Value::Bool(value_truthy(&value)));
    }
    Ok(value)
}

fn cast_mysql_date(value: Value) -> Value {
    let raw = json_scalar_to_string(&value).trim().to_string();
    if raw == "0" && matches!(value, Value::String(_)) {
        return Value::Null;
    }
    if raw == "0" || raw == "0000-00-00" {
        return Value::String("0000-00-00".to_string());
    }
    if let Some(datetime) = parse_mysql_datetime_value(&Value::String(raw.clone())) {
        return Value::String(datetime.date().to_string());
    }
    if let Some((year, month, day)) = split_date_components(&raw) {
        return Value::String(format!("{year:04}-{month:02}-{day:02}"));
    }
    Value::Null
}

fn cast_mysql_datetime(value: Value, data_type: &str) -> Value {
    let raw = json_scalar_to_string(&value).trim().to_string();
    if raw.chars().all(|character| character.is_ascii_digit()) && raw.len() >= 14 {
        let normalized = format!(
            "{}-{}-{} {}:{}:{}",
            &raw[..4],
            &raw[4..6],
            &raw[6..8],
            &raw[8..10],
            &raw[10..12],
            &raw[12..14]
        );
        let result = if temporal_precision(data_type).is_some_and(|precision| precision > 0) {
            format!("{normalized}.000000")
        } else {
            normalized
        };
        return Value::String(result);
    }
    if raw.starts_with("12:00:00") && raw.len() > "12:00:00".len() {
        let suffix = raw["12:00:00".len()..].trim_matches(['.', '-', ' ']);
        let parts = suffix
            .split(['.', ':'])
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>();
        if parts.len() >= 3 {
            return Value::String(format!("2012-00-00 {}:{}:{}", parts[0], parts[1], parts[2]));
        }
    }
    if let Some(datetime) = parse_mysql_datetime_value(&Value::String(raw.clone())) {
        return Value::String(
            if temporal_precision(data_type).is_some_and(|precision| precision > 0) {
                format!("{}.000000", datetime.format("%Y-%m-%d %H:%M:%S"))
            } else {
                datetime.to_string()
            },
        );
    }
    Value::Null
}

fn temporal_precision(data_type: &str) -> Option<u8> {
    data_type
        .split_once('(')
        .and_then(|(_, rest)| rest.split(')').next())
        .and_then(|precision| precision.trim().parse::<u8>().ok())
}

fn cast_mysql_time(value: Value) -> Value {
    let raw = json_scalar_to_string(&value).trim().to_string();
    if let Some(datetime) = parse_mysql_datetime_value(&Value::String(raw.clone())) {
        return Value::String(format_mysql_naive_time(datetime.time()));
    }
    if raw.starts_with("12:00:00") {
        let suffix = raw["12:00:00".len()..].to_string();
        if suffix.starts_with('.') || suffix.starts_with('-') {
            return Value::String("12:00:00".to_string());
        }
        if let Some(rest) = suffix.strip_prefix(' ') {
            let parts = rest.split('.').collect::<Vec<_>>();
            if parts.len() >= 3 {
                return Value::String(format!("12:{}:{}", parts[1], parts[2]));
            }
        }
    }
    if let Some(duration) = parse_mysql_time_duration(&Value::String(raw)) {
        return Value::String(format_mysql_duration(duration));
    }
    Value::Null
}

fn split_date_components(raw: &str) -> Option<(i32, u32, u32)> {
    let normalized = raw.replace('.', "-");
    let parts = normalized.split('-').collect::<Vec<_>>();
    if parts.len() == 3 {
        let year = parts[0].parse().ok()?;
        let month = parts[1].parse().ok()?;
        let day = parts[2].parse().ok()?;
        if month <= 12 && day <= 31 {
            return Some((year, month, day));
        }
    }
    None
}

pub(super) fn eval_default_value(default: &str) -> Result<Value> {
    let trimmed = default.trim();
    let upper = trimmed.to_ascii_uppercase();
    if upper == "CURRENT_TIMESTAMP" || upper == "NOW()" || upper == "CURRENT_TIMESTAMP()" {
        return Ok(Value::String(Utc::now().naive_utc().to_string()));
    }
    if upper == "CURRENT_DATE" || upper == "CURDATE()" {
        return Ok(Value::String(Utc::now().date_naive().to_string()));
    }
    if let Some(expr) = parse_scalar_expr(trimmed) {
        return eval_expr(&expr, &Map::new(), 0);
    }
    Ok(Value::String(trimmed.trim_matches('\'').to_string()))
}

pub(super) fn read_default_value(hint: &ColumnHint) -> Option<Value> {
    let default = hint.default.as_deref()?;
    if is_volatile_default(default) {
        return None;
    }
    eval_default_value(default).ok()
}

pub(super) fn is_volatile_default(default: &str) -> bool {
    let normalized = default
        .trim()
        .trim_matches(|ch| ch == '(' || ch == ')')
        .to_ascii_uppercase();
    matches!(
        normalized.as_str(),
        "CURRENT_TIMESTAMP"
            | "CURRENT_DATE"
            | "NOW"
            | "NOW()"
            | "CURDATE"
            | "CURDATE()"
            | "UUID"
            | "UUID()"
    )
}
