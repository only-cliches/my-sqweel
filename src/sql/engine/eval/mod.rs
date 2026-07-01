use super::*;

mod common;
mod datetime;
mod json;
mod scalar;

use common::*;
use datetime::*;
use json::*;
use scalar::*;

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
    if projection
        .iter()
        .any(|p| matches!(p, SelectItem::Wildcard(_)))
    {
        return Ok(data.clone());
    }

    let mut out = Map::new();
    for item in projection {
        match item {
            SelectItem::UnnamedExpr(expr) => {
                let key = projection_expr_column_name(expr);
                let value = eval(expr)?;
                out.insert(key, value);
            }
            SelectItem::ExprWithAlias { expr, alias } => {
                let value = eval(expr)?;
                out.insert(alias.value.clone(), value);
            }
            _ => {}
        }
    }

    Ok(out)
}

pub(super) fn virtual_select_result(
    select: &Select,
    rows: Vec<Map<String, Value>>,
) -> Result<QueryResult> {
    let rows = rows
        .into_iter()
        .filter_map(
            |row| match matches_selection(select.selection.as_ref(), &row) {
                Ok(true) => Some(project_row(&select.projection, &row, 0)),
                Ok(false) => None,
                Err(err) => Some(Err(err)),
            },
        )
        .collect::<Result<Vec<_>>>()?;

    Ok(QueryResult {
        rows_affected: 0,
        last_insert_id: 0,
        columns: infer_projection_columns(&select.projection),
        rows,
    })
}

pub(super) fn aggregate_select_result(
    select: &Select,
    rows: Vec<Map<String, Value>>,
    order_by: &[OrderByExpr],
    limit: Option<&Expr>,
    offset: Option<&Offset>,
    last_insert_id: u64,
) -> Result<Option<QueryResult>> {
    let group_by = group_by_exprs(select);
    if group_by.is_empty() && !projection_has_aggregate(&select.projection) {
        return Ok(None);
    }

    let grouped = group_rows(rows, &group_by, last_insert_id)?;
    let mut output = Vec::new();
    for group in grouped {
        let base = group.first().cloned().unwrap_or_default();
        let mut row = Map::new();

        for item in &select.projection {
            project_aggregate_item(item, &group, &base, last_insert_id, &mut row)?;
        }
        if let Some(having) = &select.having {
            materialize_aggregate_exprs(having, &group, &base, last_insert_id, &mut row)?;
        }
        for order in order_by {
            materialize_aggregate_exprs(&order.expr, &group, &base, last_insert_id, &mut row)?;
        }

        if matches_selection(select.having.as_ref(), &row)? {
            output.push(row);
        }
    }

    apply_ordering(&mut output, order_by)?;
    apply_limit_offset(&mut output, limit, offset)?;

    Ok(Some(QueryResult {
        rows_affected: 0,
        last_insert_id: 0,
        columns: infer_projection_columns(&select.projection),
        rows: output,
    }))
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
            aggregate_call(expr).is_some()
        }
        _ => false,
    })
}

pub(super) fn group_rows(
    rows: Vec<Map<String, Value>>,
    group_by: &[Expr],
    last_insert_id: u64,
) -> Result<Vec<Vec<Map<String, Value>>>> {
    if group_by.is_empty() {
        return Ok(vec![rows]);
    }

    let mut grouped: BTreeMap<String, Vec<Map<String, Value>>> = BTreeMap::new();
    for row in rows {
        let mut key_parts = Vec::new();
        for expr in group_by {
            key_parts.push(encode_json_value(&eval_expr(expr, &row, last_insert_id)?));
        }
        grouped.entry(key_parts.join("|")).or_default().push(row);
    }
    Ok(grouped.into_values().collect())
}

pub(super) fn project_aggregate_item(
    item: &SelectItem,
    group: &[Map<String, Value>],
    base: &Map<String, Value>,
    last_insert_id: u64,
    out: &mut Map<String, Value>,
) -> Result<()> {
    match item {
        SelectItem::Wildcard(_) => {
            out.extend(base.clone());
        }
        SelectItem::UnnamedExpr(expr) => {
            let column = projection_expr_column_name(expr);
            let value = aggregate_or_eval_expr(expr, group, base, last_insert_id)?;
            out.insert(column, value);
        }
        SelectItem::ExprWithAlias { expr, alias } => {
            let value = aggregate_or_eval_expr(expr, group, base, last_insert_id)?;
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
) -> Result<Value> {
    if let Some(call) = aggregate_call(expr) {
        return eval_aggregate_call(&call, group, last_insert_id);
    }
    eval_expr(expr, base, last_insert_id)
}

pub(super) fn materialize_aggregate_exprs(
    expr: &Expr,
    group: &[Map<String, Value>],
    base: &Map<String, Value>,
    last_insert_id: u64,
    out: &mut Map<String, Value>,
) -> Result<()> {
    if let Some(call) = aggregate_call(expr) {
        out.insert(
            projection_expr_column_name(expr),
            eval_aggregate_call(&call, group, last_insert_id)?,
        );
        return Ok(());
    }

    match expr {
        Expr::BinaryOp { left, right, .. } => {
            materialize_aggregate_exprs(left, group, base, last_insert_id, out)?;
            materialize_aggregate_exprs(right, group, base, last_insert_id, out)?;
        }
        Expr::UnaryOp { expr, .. }
        | Expr::Nested(expr)
        | Expr::IsNull(expr)
        | Expr::IsNotNull(expr)
        | Expr::Cast { expr, .. } => {
            materialize_aggregate_exprs(expr, group, base, last_insert_id, out)?;
        }
        Expr::InList { expr, list, .. } => {
            materialize_aggregate_exprs(expr, group, base, last_insert_id, out)?;
            for item in list {
                materialize_aggregate_exprs(item, group, base, last_insert_id, out)?;
            }
        }
        Expr::Like { expr, pattern, .. } => {
            materialize_aggregate_exprs(expr, group, base, last_insert_id, out)?;
            materialize_aggregate_exprs(pattern, group, base, last_insert_id, out)?;
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
    Min,
    Max,
    GroupConcat,
}

#[derive(Debug, Clone)]
struct AggregateCall {
    kind: AggregateKind,
    args: Vec<String>,
    distinct: bool,
    order_by: Vec<GroupConcatOrder>,
    separator: String,
}

#[derive(Debug, Clone)]
struct GroupConcatOrder {
    expr: String,
    asc: bool,
}

fn aggregate_call(expr: &Expr) -> Option<AggregateCall> {
    let (name, args) = split_function_call(&expr.to_string())?;
    let kind = match name.as_str() {
        "COUNT" => AggregateKind::Count,
        "SUM" => AggregateKind::Sum,
        "AVG" => AggregateKind::Avg,
        "MIN" => AggregateKind::Min,
        "MAX" => AggregateKind::Max,
        "GROUP_CONCAT" => AggregateKind::GroupConcat,
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
        args,
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
                        expr: trimmed[..expr_len].trim().to_string(),
                        asc: false,
                    })
                } else if let Some(expr) = upper.strip_suffix(" ASC") {
                    let expr_len = expr.len();
                    Some(GroupConcatOrder {
                        expr: trimmed[..expr_len].trim().to_string(),
                        asc: true,
                    })
                } else if !trimmed.is_empty() {
                    Some(GroupConcatOrder {
                        expr: trimmed.to_string(),
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
        args,
        distinct,
        order_by,
        separator,
    }
}

fn eval_aggregate_call(
    call: &AggregateCall,
    group: &[Map<String, Value>],
    last_insert_id: u64,
) -> Result<Value> {
    let mut values = Vec::new();
    let mut ordered_group = group.iter().collect::<Vec<_>>();
    if call.kind == AggregateKind::GroupConcat && !call.order_by.is_empty() {
        ordered_group.sort_by(|left, right| {
            for order in &call.order_by {
                let left_value =
                    eval_scalar_text(&order.expr, left, last_insert_id).unwrap_or(Value::Null);
                let right_value =
                    eval_scalar_text(&order.expr, right, last_insert_id).unwrap_or(Value::Null);
                let ordering = compare_json_values(&left_value, &right_value);
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
    }

    for row in ordered_group {
        if call.kind == AggregateKind::GroupConcat {
            let mut parts = Vec::new();
            for arg in &call.args {
                let value = eval_scalar_text(arg, row, last_insert_id)?;
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
            [arg] if arg == "*" => Value::Number(Number::from(1_u64)),
            [arg] => eval_scalar_text(arg, row, last_insert_id)?,
            args => {
                let tuple = args
                    .iter()
                    .map(|arg| eval_scalar_text(arg, row, last_insert_id))
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
        let mut seen = BTreeSet::new();
        values.retain(|value| seen.insert(encode_json_value(value)));
    }

    match call.kind {
        AggregateKind::Count => Ok(Value::Number(Number::from(values.len() as u64))),
        AggregateKind::Sum => {
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
    }
}

pub(super) fn infer_projection_columns(projection: &[SelectItem]) -> Vec<String> {
    let mut out = Vec::new();
    for item in projection {
        match item {
            SelectItem::UnnamedExpr(expr) => out.push(projection_expr_column_name(expr)),
            SelectItem::ExprWithAlias { alias, .. } => out.push(alias.value.clone()),
            _ => {}
        }
    }
    out
}

pub(super) fn projection_expr_column_name(expr: &Expr) -> String {
    match expr {
        Expr::Identifier(Ident { value, .. }) => value.clone(),
        Expr::CompoundIdentifier(parts) => parts
            .iter()
            .map(|p| p.value.clone())
            .collect::<Vec<_>>()
            .join("."),
        _ => expr.to_string(),
    }
}

pub(super) fn apply_ordering(
    rows: &mut [Map<String, Value>],
    order_by: &[OrderByExpr],
) -> Result<()> {
    for item in order_by {
        validate_order_expr(&item.expr)?;
    }

    rows.sort_by(|a, b| {
        for item in order_by {
            let left = expr_resolved_value(&item.expr, a).unwrap_or(Value::Null);
            let right = expr_resolved_value(&item.expr, b).unwrap_or(Value::Null);
            let ordering = compare_json_values(&left, &right);
            if ordering != Ordering::Equal {
                return if item.asc.unwrap_or(true) {
                    ordering
                } else {
                    ordering.reverse()
                };
            }
        }
        Ordering::Equal
    });

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
    let start = offset.map(offset_to_usize).transpose()?.unwrap_or(0);
    let take = limit.map(expr_to_usize).transpose()?;

    let sliced = rows
        .iter()
        .skip(start)
        .take(take.unwrap_or(usize::MAX))
        .cloned()
        .collect();
    *rows = sliced;
    Ok(())
}

pub(super) fn offset_to_usize(offset: &Offset) -> Result<usize> {
    expr_to_usize(&offset.value)
}

pub(super) fn expr_to_usize(expr: &Expr) -> Result<usize> {
    let value = eval_expr(expr, &Map::new(), 0)?;
    match value {
        Value::Number(n) => n
            .as_u64()
            .map(|v| v as usize)
            .ok_or_else(|| anyhow!("numeric expression is not a valid usize")),
        Value::String(s) => s
            .parse::<usize>()
            .map_err(|_| anyhow!("string expression is not a valid usize")),
        _ => Err(anyhow!("expected numeric expression")),
    }
}

pub(super) fn compare_json_values(left: &Value, right: &Value) -> Ordering {
    match (left, right) {
        (Value::Null, Value::Null) => Ordering::Equal,
        (Value::Null, _) => Ordering::Less,
        (_, Value::Null) => Ordering::Greater,
        (Value::Number(a), Value::Number(b)) => a
            .as_f64()
            .partial_cmp(&b.as_f64())
            .unwrap_or(Ordering::Equal),
        (Value::Bool(a), Value::Bool(b)) => a.cmp(b),
        (Value::String(a), Value::String(b)) => a.cmp(b),
        _ => left.to_string().cmp(&right.to_string()),
    }
}

pub(super) fn mysql_eq(left: &Value, right: &Value) -> bool {
    if matches!(left, Value::Null) || matches!(right, Value::Null) {
        return false;
    }
    if left == right {
        return true;
    }

    match (left, right) {
        (Value::Number(_), Value::Number(_)) => {
            let Ok(l) = json_to_f64(left) else {
                return false;
            };
            let Ok(r) = json_to_f64(right) else {
                return false;
            };
            (l - r).abs() < f64::EPSILON
        }
        (Value::Number(_), Value::String(_)) | (Value::String(_), Value::Number(_)) => {
            let Ok(l) = json_to_f64(left) else {
                return false;
            };
            let Ok(r) = json_to_f64(right) else {
                return false;
            };
            (l - r).abs() < f64::EPSILON
        }
        (Value::Bool(a), Value::Number(b)) => b.as_i64() == Some(i64::from(*a)),
        (Value::Number(a), Value::Bool(b)) => a.as_i64() == Some(i64::from(*b)),
        _ => false,
    }
}

pub(super) fn mysql_ne(left: &Value, right: &Value) -> bool {
    if matches!(left, Value::Null) || matches!(right, Value::Null) {
        return false;
    }
    !mysql_eq(left, right)
}

pub(super) fn is_defaultish(value: &Value) -> bool {
    matches!(value, Value::Null)
        || value
            .as_str()
            .is_some_and(|value| value.eq_ignore_ascii_case("DEFAULT"))
}

pub(super) fn try_index_lookup(selection: Option<&Expr>, table: &str) -> Option<(String, String)> {
    let expr = selection?;
    if let Expr::BinaryOp { left, op, right } = expr {
        if *op != BinaryOperator::Eq {
            return None;
        }
        let col = match &**left {
            Expr::Identifier(Ident { value, .. }) => value.clone(),
            Expr::CompoundIdentifier(parts) if parts.len() == 2 && parts[0].value == table => {
                parts[1].value.clone()
            }
            _ => return None,
        };
        let val = eval_expr(right, &Map::new(), 0).ok()?.to_string();
        return Some((col, val));
    }
    None
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
    match expr {
        Expr::BinaryOp { left, op, right } => match op {
            BinaryOperator::Eq => Ok(mysql_eq(&eval(left)?, &eval(right)?)),
            BinaryOperator::NotEq => Ok(mysql_ne(&eval(left)?, &eval(right)?)),
            BinaryOperator::Gt => Ok(compare_predicate_values(
                eval(left)?,
                eval(right)?,
                |a, b| a > b,
            )),
            BinaryOperator::GtEq => Ok(compare_predicate_values(
                eval(left)?,
                eval(right)?,
                |a, b| a >= b,
            )),
            BinaryOperator::Lt => Ok(compare_predicate_values(
                eval(left)?,
                eval(right)?,
                |a, b| a < b,
            )),
            BinaryOperator::LtEq => Ok(compare_predicate_values(
                eval(left)?,
                eval(right)?,
                |a, b| a <= b,
            )),
            BinaryOperator::And => {
                Ok(matches_expr_with(left, eval)? && matches_expr_with(right, eval)?)
            }
            BinaryOperator::Or => {
                Ok(matches_expr_with(left, eval)? || matches_expr_with(right, eval)?)
            }
            _ => Ok(value_truthy(&eval(expr)?)),
        },
        Expr::IsNull(expr) => Ok(eval(expr)? == Value::Null),
        Expr::IsNotNull(expr) => Ok(eval(expr)? != Value::Null),
        Expr::InList {
            expr,
            list,
            negated,
        } => {
            let value = eval(expr)?;
            let hit = list.iter().any(|item| {
                eval(item)
                    .map(|candidate| mysql_eq(&value, &candidate))
                    .unwrap_or(false)
            });
            Ok(if *negated { !hit } else { hit })
        }
        Expr::Like {
            expr,
            pattern,
            negated,
            ..
        } => {
            let target = eval(expr)?;
            let pattern = eval(pattern)?;
            Ok(value_truthy(&eval_like_values(target, pattern, *negated)))
        }
        _ => Ok(value_truthy(&eval(expr)?)),
    }
}

pub(super) fn compare_predicate_values<F: Fn(f64, f64) -> bool>(
    left: Value,
    right: Value,
    f: F,
) -> bool {
    if matches!(left, Value::Null) || matches!(right, Value::Null) {
        return false;
    }
    if let (Ok(l), Ok(r)) = (json_to_f64(&left), json_to_f64(&right)) {
        return f(l, r);
    }
    let ordering = compare_json_values(&left, &right);
    f(ordering_to_f64(ordering), 0.0)
}

pub(super) fn ordering_to_f64(ordering: Ordering) -> f64 {
    match ordering {
        Ordering::Less => -1.0,
        Ordering::Equal => 0.0,
        Ordering::Greater => 1.0,
    }
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
        Value::Number(_) | Value::String(_) => json_to_f64(v).or_else(|_| Ok(0.0)),
        Value::Array(_) | Value::Object(_) => Ok(0.0),
    }
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
    match value {
        Value::Null => false,
        Value::Bool(value) => *value,
        Value::Number(number) => number.as_f64().is_some_and(|value| value != 0.0),
        Value::String(value) => value
            .parse::<f64>()
            .map(|v| v != 0.0)
            .unwrap_or(!value.is_empty()),
        Value::Array(value) => !value.is_empty(),
        Value::Object(value) => !value.is_empty(),
    }
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
        Expr::Identifier(Ident { value, .. }) => Ok(data
            .get(value)
            .cloned()
            .or_else(|| eval_bare_datetime_keyword(value))
            .unwrap_or(Value::Null)),
        Expr::CompoundIdentifier(parts) => {
            let key = parts
                .iter()
                .map(|p| p.value.clone())
                .collect::<Vec<_>>()
                .join(".");
            Ok(data
                .get(&key)
                .cloned()
                .or_else(|| data.get(&parts.last().expect("parts").value).cloned())
                .unwrap_or(Value::Null))
        }
        _ => expr_to_json(expr),
    }
}

pub(super) fn expr_resolved_value(expr: &Expr, data: &Map<String, Value>) -> Result<Value> {
    eval_expr(expr, data, 0)
}

pub(super) fn expr_to_json(expr: &Expr) -> Result<Value> {
    match expr {
        Expr::Value(v) => sql_value_to_json(v),
        Expr::UnaryOp { op, expr } if op.to_string() == "-" => {
            let inner = expr_to_json(expr)?;
            match inner {
                Value::Number(n) => {
                    if let Some(i) = n.as_i64() {
                        Ok(Value::Number(Number::from(-i)))
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
    if let Some(value) = data.get(&projection_expr_column_name(expr)) {
        return Ok(value.clone());
    }
    if let Some(value) = system_variable_expr_value(expr) {
        return Ok(value);
    }

    match expr {
        Expr::Value(v) => sql_value_to_json(v),
        Expr::Identifier(_) | Expr::CompoundIdentifier(_) => expr_field_value(expr, data),
        Expr::Nested(expr) => eval_expr(expr, data, last_insert_id),
        Expr::UnaryOp { op, expr } if op.to_string() == "-" => {
            let value = eval_expr(expr, data, last_insert_id)?;
            Ok(number_from_f64(-json_to_f64_lossy(&value)?))
        }
        Expr::UnaryOp { op, expr } if op.to_string().eq_ignore_ascii_case("NOT") => Ok(
            Value::Bool(!value_truthy(&eval_expr(expr, data, last_insert_id)?)),
        ),
        Expr::UnaryOp { expr, .. } => eval_expr(expr, data, last_insert_id),
        Expr::BinaryOp { left, op, right } => {
            eval_binary_expr(left, op, right, data, last_insert_id)
        }
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
            let hit = list.iter().any(|item| {
                eval_expr(item, data, last_insert_id)
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
        Expr::Position { expr, r#in } => eval_position_values(
            eval_expr(expr, data, last_insert_id)?,
            eval_expr(r#in, data, last_insert_id)?,
        ),
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
        Expr::Function(func) => eval_function_text(&func.to_string(), data, last_insert_id),
        Expr::Cast {
            expr, data_type, ..
        } => cast_json_value(
            eval_expr(expr, data, last_insert_id)?,
            &data_type.to_string(),
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
        _ => expr_to_json(expr),
    }
}

pub(super) fn eval_binary_expr(
    left: &Expr,
    op: &BinaryOperator,
    right: &Expr,
    data: &Map<String, Value>,
    last_insert_id: u64,
) -> Result<Value> {
    let left_value = eval_expr(left, data, last_insert_id)?;
    let right_value = eval_expr(right, data, last_insert_id)?;
    eval_binary_values(left_value, op, right_value)
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
            let divisor = json_to_f64_lossy(&right_value)?;
            if divisor == 0.0 {
                Ok(Value::Null)
            } else {
                Ok(number_from_f64(json_to_f64_lossy(&left_value)? / divisor))
            }
        }
        BinaryOperator::Modulo => {
            let divisor = json_to_f64_lossy(&right_value)?;
            if divisor == 0.0 {
                Ok(Value::Null)
            } else {
                Ok(number_from_f64(json_to_f64_lossy(&left_value)? % divisor))
            }
        }
        BinaryOperator::Eq => Ok(Value::Bool(mysql_eq(&left_value, &right_value))),
        BinaryOperator::NotEq => Ok(Value::Bool(mysql_ne(&left_value, &right_value))),
        BinaryOperator::Gt => Ok(Value::Bool(
            compare_json_values(&left_value, &right_value).is_gt(),
        )),
        BinaryOperator::GtEq => Ok(Value::Bool(
            !compare_json_values(&left_value, &right_value).is_lt(),
        )),
        BinaryOperator::Lt => Ok(Value::Bool(
            compare_json_values(&left_value, &right_value).is_lt(),
        )),
        BinaryOperator::LtEq => Ok(Value::Bool(
            !compare_json_values(&left_value, &right_value).is_gt(),
        )),
        BinaryOperator::And => Ok(Value::Bool(
            value_truthy(&left_value) && value_truthy(&right_value),
        )),
        BinaryOperator::Or => Ok(Value::Bool(
            value_truthy(&left_value) || value_truthy(&right_value),
        )),
        _ => Ok(Value::Null),
    }
}

pub(super) fn first_projected_value(row: &Map<String, Value>, columns: &[String]) -> Option<Value> {
    columns
        .first()
        .and_then(|column| row.get(column).cloned())
        .or_else(|| row.values().next().cloned())
}

pub(super) fn numeric_binary(
    left: Value,
    right: Value,
    op: impl FnOnce(f64, f64) -> f64,
) -> Result<Value> {
    Ok(number_from_f64(op(
        json_to_f64_lossy(&left)?,
        json_to_f64_lossy(&right)?,
    )))
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
        "LAST_INSERT_ID" => {
            if let Some(arg) = args.first() {
                eval_scalar_text(arg, data, last_insert_id)
            } else {
                Ok(Value::Number(Number::from(last_insert_id)))
            }
        }
        "NOW" | "CURRENT_TIMESTAMP" | "LOCALTIME" | "LOCALTIMESTAMP" | "UTC_TIMESTAMP" => {
            Ok(Value::String(Utc::now().naive_utc().to_string()))
        }
        "CURRENT_DATE" | "CURDATE" | "UTC_DATE" => {
            Ok(Value::String(Utc::now().date_naive().to_string()))
        }
        "CURRENT_TIME" | "CURTIME" | "UTC_TIME" => Ok(Value::String(format_mysql_naive_time(
            Utc::now().naive_utc().time(),
        ))),
        "DATE_ADD" | "ADDDATE" => {
            eval_date_add_sub(args.first(), args.get(1), data, last_insert_id, 1)
        }
        "DATE_SUB" | "SUBDATE" => {
            eval_date_add_sub(args.first(), args.get(1), data, last_insert_id, -1)
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
        "UUID" => Ok(Value::String(uuid::Uuid::new_v4().to_string())),
        "DATABASE" | "SCHEMA" => Ok(Value::String("app".to_string())),
        "VERSION" => Ok(Value::String("8.0.0-my-sqweel".to_string())),
        "USER" | "CURRENT_USER" => Ok(Value::String("root@localhost".to_string())),
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
        "LENGTH" | "CHAR_LENGTH" | "CHARACTER_LENGTH" => {
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
        "ASCII" | "ORD" => eval_ascii_ord(args.first(), data, last_insert_id),
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
        "SUBSTRING" | "SUBSTR" => {
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
        "LOCATE" => eval_locate(args.first(), args.get(1), args.get(2), data, last_insert_id),
        "INSTR" => eval_instr(args.first(), args.get(1), data, last_insert_id),
        "POSITION" => eval_position(args.first(), data, last_insert_id),
        "REVERSE" => eval_unary_string(args.first(), data, last_insert_id, |value| {
            value.chars().rev().collect()
        }),
        "REPEAT" => eval_repeat(args.first(), args.get(1), data, last_insert_id),
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
        "JSON_EXTRACT" => eval_json_extract(args.as_slice(), data, last_insert_id),
        "JSON_UNQUOTE" => eval_json_unquote(args.first(), data, last_insert_id),
        "JSON_OBJECT" => eval_json_object(args.as_slice(), data, last_insert_id),
        "JSON_ARRAY" => eval_json_array(args.as_slice(), data, last_insert_id),
        "JSON_CONTAINS" => {
            eval_json_contains(args.first(), args.get(1), args.get(2), data, last_insert_id)
        }
        "JSON_SET" => eval_json_mutation(args.as_slice(), data, last_insert_id, JsonMutation::Set),
        "JSON_REMOVE" => {
            eval_json_mutation(args.as_slice(), data, last_insert_id, JsonMutation::Remove)
        }
        _ => {
            tracing::debug!(function = %name, "sql.unsupported_function");
            Ok(Value::Null)
        }
    }
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
    if !text.ends_with(')') {
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

pub(super) fn cast_json_value(value: Value, data_type: &str) -> Result<Value> {
    let data_type = data_type.to_ascii_lowercase();
    if value == Value::Null {
        return Ok(Value::Null);
    }
    if data_type.contains("int") || data_type == "signed" || data_type == "unsigned" {
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
        return Ok(parse_mysql_datetime_value(&value)
            .map(|datetime| Value::String(datetime.to_string()))
            .unwrap_or(Value::Null));
    }
    if data_type.contains("date") {
        return Ok(parse_mysql_datetime_value(&value)
            .map(|datetime| Value::String(datetime.date().to_string()))
            .unwrap_or(Value::Null));
    }
    if data_type.contains("time") {
        if let Some(datetime) = parse_mysql_datetime_value(&value) {
            return Ok(Value::String(format_mysql_naive_time(datetime.time())));
        }
        return Ok(parse_mysql_time_duration(&value)
            .map(|duration| Value::String(format_mysql_duration(duration)))
            .unwrap_or(Value::Null));
    }
    if data_type.contains("json") {
        return Ok(parse_json_document_value(value));
    }
    if data_type.contains("bool") {
        return Ok(Value::Bool(value_truthy(&value)));
    }
    Ok(value)
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
