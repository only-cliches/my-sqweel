use std::ops::ControlFlow;

use anyhow::{Result, anyhow};
use sqlparser::ast::{
    BinaryOperator, CastKind, Distinct, Expr, GroupByExpr, JoinConstraint, JoinOperator, LockType,
    Query, Select, SelectItem, SetExpr, SetQuantifier, Statement, TableFactor, UnaryOperator,
    Visit, Visitor,
};

pub(super) fn validate_statement_support(statement: &Statement) -> Result<()> {
    let mut validator = SupportValidator;
    match statement.visit(&mut validator) {
        ControlFlow::Continue(()) => Ok(()),
        ControlFlow::Break(feature) => Err(anyhow!("unsupported SQL feature: {feature}")),
    }
}

struct SupportValidator;

impl Visitor for SupportValidator {
    type Break = String;

    fn pre_visit_query(&mut self, query: &Query) -> ControlFlow<Self::Break> {
        if query.with.as_ref().is_some_and(|with| {
            with.cte_tables
                .iter()
                .any(|cte| cte.from.is_some() || cte.materialized.is_some())
        }) {
            return unsupported("common table expression modifier");
        }
        if query.fetch.is_some() {
            return unsupported("FETCH");
        }
        if query.locks.iter().any(|lock| {
            !matches!(lock.lock_type, LockType::Update)
                || lock.of.is_some()
                || lock.nonblock.is_some()
        }) {
            return unsupported("SELECT locking clauses other than plain FOR UPDATE");
        }
        if query.for_clause.is_some()
            || query.settings.is_some()
            || query.format_clause.is_some()
            || !query.limit_by.is_empty()
        {
            return unsupported("non-MySQL query modifiers");
        }
        if let Some(order_by) = &query.order_by
            && (order_by.interpolate.is_some()
                || order_by
                    .exprs
                    .iter()
                    .any(|expr| expr.nulls_first.is_some() || expr.with_fill.is_some()))
        {
            return unsupported("ORDER BY NULLS/WITH FILL/INTERPOLATE modifiers");
        }

        validate_set_expr(&query.body)
    }

    fn pre_visit_table_factor(&mut self, factor: &TableFactor) -> ControlFlow<Self::Break> {
        match factor {
            TableFactor::Table { .. }
            | TableFactor::Derived { .. }
            | TableFactor::NestedJoin { .. }
            | TableFactor::JsonTable { .. } => ControlFlow::Continue(()),
            _ => unsupported(format!("table factor `{factor}`")),
        }
    }

    fn pre_visit_expr(&mut self, expr: &Expr) -> ControlFlow<Self::Break> {
        match expr {
            Expr::BinaryOp { op, .. } if !supported_binary_operator(op) => {
                unsupported(format!("binary operator `{op}`"))
            }
            Expr::UnaryOp { op, .. } if !supported_unary_operator(op) => {
                unsupported(format!("unary operator `{op}`"))
            }
            Expr::Like {
                any, escape_char, ..
            } if *any || escape_char.is_some() => unsupported("LIKE ANY/custom ESCAPE"),
            Expr::Function(function)
                if function.filter.is_some()
                    || function.null_treatment.is_some()
                    || !function.within_group.is_empty()
                    || function.uses_odbc_syntax =>
            {
                unsupported(format!("function modifier in `{function}`"))
            }
            Expr::Function(function) if function.over.is_some() => {
                let name = function
                    .name
                    .0
                    .last()
                    .map(|name| name.value.to_ascii_uppercase())
                    .unwrap_or_default();
                if matches!(
                    name.as_str(),
                    "ROW_NUMBER"
                        | "RANK"
                        | "DENSE_RANK"
                        | "PERCENT_RANK"
                        | "CUME_DIST"
                        | "NTILE"
                        | "LAG"
                        | "LEAD"
                        | "FIRST_VALUE"
                        | "LAST_VALUE"
                        | "NTH_VALUE"
                        | "COUNT"
                        | "SUM"
                        | "AVG"
                        | "MIN"
                        | "MAX"
                        | "STD"
                        | "STDDEV"
                ) {
                    ControlFlow::Continue(())
                } else {
                    unsupported(format!("window function `{name}`"))
                }
            }
            Expr::Cast { kind, format, .. }
                if !matches!(kind, CastKind::Cast) || format.is_some() =>
            {
                unsupported(format!("cast form `{expr}`"))
            }
            Expr::Convert { is_try, styles, .. } if *is_try || !styles.is_empty() => {
                unsupported(format!("convert form `{expr}`"))
            }
            Expr::Ceil { field, .. } | Expr::Floor { field, .. }
                if !matches!(
                    field,
                    sqlparser::ast::CeilFloorKind::DateTimeField(
                        sqlparser::ast::DateTimeField::NoDateTime
                    )
                ) =>
            {
                unsupported(format!("ceil/floor form `{expr}`"))
            }
            Expr::Trim {
                trim_characters: Some(_),
                ..
            } => unsupported(format!("trim form `{expr}`")),
            Expr::Identifier(_)
            | Expr::CompoundIdentifier(_)
            | Expr::IsFalse(_)
            | Expr::IsNotFalse(_)
            | Expr::IsTrue(_)
            | Expr::IsNotTrue(_)
            | Expr::IsNull(_)
            | Expr::IsNotNull(_)
            | Expr::IsUnknown(_)
            | Expr::IsNotUnknown(_)
            | Expr::InList { .. }
            | Expr::InSubquery { .. }
            | Expr::Between { .. }
            | Expr::BinaryOp { .. }
            | Expr::Like { .. }
            | Expr::RLike { .. }
            | Expr::UnaryOp { .. }
            | Expr::Convert { .. }
            | Expr::Cast { .. }
            | Expr::Extract { .. }
            | Expr::Ceil { .. }
            | Expr::Floor { .. }
            | Expr::Position { .. }
            | Expr::Substring { .. }
            | Expr::Trim { .. }
            | Expr::Nested(_)
            | Expr::Value(_)
            | Expr::TypedString { .. }
            | Expr::IntroducedString { .. }
            | Expr::Function(_)
            | Expr::Case { .. }
            | Expr::Exists { .. }
            | Expr::Subquery(_)
            | Expr::Tuple(_)
            | Expr::Interval(_) => ControlFlow::Continue(()),
            _ => unsupported(format!("expression `{expr}`")),
        }
    }
}

fn validate_set_expr(expr: &SetExpr) -> ControlFlow<String> {
    match expr {
        SetExpr::Select(select) => validate_select(select),
        SetExpr::Query(query) => validate_set_expr(&query.body),
        SetExpr::SetOperation {
            op: _,
            set_quantifier,
            left,
            right,
        } => {
            if !matches!(
                set_quantifier,
                SetQuantifier::All | SetQuantifier::Distinct | SetQuantifier::None
            ) {
                return unsupported(format!("set quantifier `{set_quantifier}`"));
            }
            validate_set_expr(left)?;
            validate_set_expr(right)
        }
        // VALUES is supported as an INSERT source. Other top-level shapes are
        // rejected by the query executor with a precise error.
        SetExpr::Values(_) => ControlFlow::Continue(()),
        _ => unsupported(format!("query body `{expr}`")),
    }
}

fn validate_select(select: &Select) -> ControlFlow<String> {
    if matches!(select.distinct, Some(Distinct::On(_))) {
        return unsupported("DISTINCT ON");
    }
    if select.top.is_some()
        || select.into.is_some()
        || !select.lateral_views.is_empty()
        || select.prewhere.is_some()
        || !select.cluster_by.is_empty()
        || !select.distribute_by.is_empty()
        || !select.sort_by.is_empty()
        || select.qualify.is_some()
        || select.value_table_mode.is_some()
        || select.connect_by.is_some()
    {
        return unsupported("SELECT modifier");
    }
    if matches!(select.group_by, GroupByExpr::All(_)) {
        return unsupported("GROUP BY ALL");
    }
    for item in &select.projection {
        match item {
            SelectItem::QualifiedWildcard(_, options) if wildcard_has_options(options) => {
                return unsupported("qualified wildcard projection options");
            }
            SelectItem::Wildcard(options) if wildcard_has_options(options) => {
                return unsupported("wildcard projection options");
            }
            _ => {}
        }
    }
    for table in &select.from {
        for join in &table.joins {
            match &join.join_operator {
                JoinOperator::Inner(
                    JoinConstraint::On(_)
                    | JoinConstraint::Using(_)
                    | JoinConstraint::Natural
                    | JoinConstraint::None,
                )
                | JoinOperator::LeftOuter(
                    JoinConstraint::On(_) | JoinConstraint::Using(_) | JoinConstraint::Natural,
                )
                | JoinOperator::RightOuter(
                    JoinConstraint::On(_) | JoinConstraint::Using(_) | JoinConstraint::Natural,
                )
                | JoinOperator::CrossJoin => {}
                JoinOperator::LeftOuter(constraint) | JoinOperator::RightOuter(constraint) => {
                    return unsupported(format!("join constraint `{constraint:?}`"));
                }
                operator => return unsupported(format!("join operator `{operator:?}`")),
            }
        }
    }
    ControlFlow::Continue(())
}

fn wildcard_has_options(options: &sqlparser::ast::WildcardAdditionalOptions) -> bool {
    options.opt_ilike.is_some()
        || options.opt_exclude.is_some()
        || options.opt_except.is_some()
        || options.opt_replace.is_some()
        || options.opt_rename.is_some()
}

fn supported_binary_operator(operator: &BinaryOperator) -> bool {
    matches!(
        operator,
        BinaryOperator::Plus
            | BinaryOperator::Minus
            | BinaryOperator::Multiply
            | BinaryOperator::Divide
            | BinaryOperator::Modulo
            | BinaryOperator::Gt
            | BinaryOperator::Lt
            | BinaryOperator::GtEq
            | BinaryOperator::LtEq
            | BinaryOperator::Spaceship
            | BinaryOperator::Eq
            | BinaryOperator::NotEq
            | BinaryOperator::And
            | BinaryOperator::Or
            | BinaryOperator::Xor
            | BinaryOperator::BitwiseOr
            | BinaryOperator::BitwiseAnd
            | BinaryOperator::BitwiseXor
            | BinaryOperator::MyIntegerDivide
            | BinaryOperator::Arrow
            | BinaryOperator::LongArrow
    )
}

fn supported_unary_operator(operator: &UnaryOperator) -> bool {
    matches!(
        operator,
        UnaryOperator::Plus
            | UnaryOperator::Minus
            | UnaryOperator::Not
            | UnaryOperator::BangNot
            | UnaryOperator::PGBitwiseNot
    )
}

fn unsupported<T>(feature: impl Into<String>) -> ControlFlow<String, T> {
    ControlFlow::Break(feature.into())
}
