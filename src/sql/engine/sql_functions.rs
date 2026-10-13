use super::MysqlColumnType;
use anyhow::{Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use sqlparser::ast::{Expr, Function, Visit, Visitor};
use sqlparser::dialect::MySqlDialect;
use sqlparser::parser::Parser;
use sqlparser::tokenizer::{Token, Word};
use std::collections::{BTreeMap, BTreeSet};
use std::ops::ControlFlow;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(super) struct SqlFunctionDef {
    pub name: String,
    pub args: Vec<(String, MysqlColumnType)>,
    pub return_type: MysqlColumnType,
    pub body: String,
    pub aggregate: bool,
}

#[derive(Clone, Debug)]
pub(super) enum SqlFunctionCommand {
    Create(SqlFunctionDef),
    Drop {
        name: String,
        aggregate: bool,
        if_exists: bool,
    },
}

pub(super) fn parse_function_command(sql: &str) -> Result<Option<SqlFunctionCommand>> {
    let tokens = crate::sql::sql_tokens(sql)?;
    let mut parser = FunctionTokens {
        tokens: &tokens,
        at: 0,
    };
    let creating = if parser.take_keyword("CREATE") {
        true
    } else if parser.take_keyword("DROP") {
        false
    } else {
        return Ok(None);
    };
    let aggregate = parser.take_keyword("AGGREGATE");
    if !parser.take_keyword("FUNCTION") {
        return Ok(None);
    }
    let if_exists = !creating && parser.take_keyword("IF") && {
        parser.expect_keyword("EXISTS")?;
        true
    };
    let name = parser.take_identifier()?;
    if !creating {
        parser.finish()?;
        return Ok(Some(SqlFunctionCommand::Drop {
            name,
            aggregate,
            if_exists,
        }));
    }

    parser.expect_char('(')?;
    let mut args = Vec::new();
    if !parser.take_char(')') {
        loop {
            let arg_name = parser.take_identifier()?;
            let arg_type = parser.take_type()?;
            if args
                .iter()
                .any(|(name, _)| name.eq_ignore_ascii_case(&arg_name))
            {
                bail!("duplicate SQL function parameter: {arg_name}");
            }
            args.push((arg_name, arg_type));
            if parser.take_char(')') {
                break;
            }
            parser.expect_char(',')?;
        }
    }
    parser.expect_keyword("RETURNS")?;
    let return_type = parser.take_type()?;
    parser.expect_keyword("RETURN")?;
    let body = tokens[parser.at..]
        .iter()
        .map(|(_, text)| *text)
        .collect::<String>();
    let body = body.trim();
    let body = body.strip_suffix(';').unwrap_or(body).trim().to_owned();
    if body.is_empty() {
        bail!("SQL function body is empty");
    }
    let mut expression_parser = Parser::new(&MySqlDialect {}).try_with_sql(&body)?;
    expression_parser.parse_expr()?;
    if expression_parser.peek_token().token != Token::EOF {
        bail!("SQL function body must be one expression");
    }
    Ok(Some(SqlFunctionCommand::Create(SqlFunctionDef {
        name,
        args,
        return_type,
        body,
        aggregate,
    })))
}

pub(super) fn validate_function_body(
    def: &SqlFunctionDef,
    hosts: &super::HostFunctionRegistry,
    sql: &BTreeMap<String, SqlFunctionDef>,
) -> Result<()> {
    let parameters = def
        .args
        .iter()
        .map(|(name, _)| name.to_ascii_lowercase())
        .collect();
    let scan = scan_expression(&def.body, Some(&parameters), hosts, sql, def.aggregate)?;
    if def.aggregate {
        if !scan.has_builtin_aggregate {
            bail!("SQL aggregate function must contain a built-in aggregate call");
        }
    } else if scan.has_builtin_aggregate {
        bail!("scalar SQL function cannot call an aggregate function");
    }
    Ok(())
}

pub(super) fn validate_schema_expression(
    expression: &str,
    hosts: &super::HostFunctionRegistry,
    sql: &BTreeMap<String, SqlFunctionDef>,
) -> Result<()> {
    validate_schema_expression_inner(expression, None, hosts, sql, &mut BTreeSet::new(), true)
}

fn validate_schema_expression_inner(
    expression: &str,
    parameters: Option<&BTreeSet<String>>,
    hosts: &super::HostFunctionRegistry,
    sql: &BTreeMap<String, SqlFunctionDef>,
    visiting: &mut BTreeSet<String>,
    allow_nondeterministic_builtins: bool,
) -> Result<()> {
    let scan = scan_expression(expression, parameters, hosts, sql, false)?;
    for call in scan.calls {
        let name = call.to_ascii_lowercase();
        if builtin_function(&name).is_some() {
            if !allow_nondeterministic_builtins && is_nondeterministic_builtin(&name) {
                bail!("SQL function depends on nondeterministic built-in {name}");
            }
            continue;
        }
        if let Some(host) = hosts.lookup(&name) {
            let signature = host.signature();
            if host.is_aggregate() || host.is_async() || !signature.deterministic {
                bail!(
                    "schema expression host function {name} must be a deterministic synchronous scalar"
                );
            }
            continue;
        }
        let Some((key, function)) = sql.iter().find(|(key, _)| key.eq_ignore_ascii_case(&name))
        else {
            bail!("unknown function: {name}");
        };
        if function.aggregate {
            bail!("schema expression cannot call aggregate function {name}");
        }
        if !visiting.insert(key.to_ascii_lowercase()) {
            bail!("cyclic SQL function dependency: {name}");
        }
        let parameters = function
            .args
            .iter()
            .map(|(parameter, _)| parameter.to_ascii_lowercase())
            .collect();
        let result = validate_schema_expression_inner(
            &function.body,
            Some(&parameters),
            hosts,
            sql,
            visiting,
            false,
        );
        visiting.remove(&key.to_ascii_lowercase());
        result?;
    }
    Ok(())
}

pub(super) fn expression_calls_function(expression: &str, name: &str) -> bool {
    let Ok(expr) = parse_function_expression(expression) else {
        return false;
    };
    struct CallFinder<'a> {
        name: &'a str,
        found: bool,
    }
    impl Visitor for CallFinder<'_> {
        type Break = ();

        fn pre_visit_expr(&mut self, expr: &Expr) -> ControlFlow<Self::Break> {
            if let Expr::Function(function) = expr
                && function_name(function)
                    .is_some_and(|found| found.eq_ignore_ascii_case(self.name))
            {
                self.found = true;
            }
            ControlFlow::Continue(())
        }
    }
    let mut finder = CallFinder { name, found: false };
    let _ = expr.visit(&mut finder);
    finder.found
}

struct ExpressionScan {
    calls: Vec<String>,
    has_builtin_aggregate: bool,
}

fn scan_expression(
    expression: &str,
    parameters: Option<&BTreeSet<String>>,
    hosts: &super::HostFunctionRegistry,
    sql: &BTreeMap<String, SqlFunctionDef>,
    aggregate_body: bool,
) -> Result<ExpressionScan> {
    struct Scanner<'a> {
        parameters: Option<&'a BTreeSet<String>>,
        hosts: &'a super::HostFunctionRegistry,
        sql: &'a BTreeMap<String, SqlFunctionDef>,
        aggregate_body: bool,
        calls: Vec<String>,
        has_builtin_aggregate: bool,
    }

    impl Visitor for Scanner<'_> {
        type Break = String;

        fn pre_visit_query(&mut self, _: &sqlparser::ast::Query) -> ControlFlow<Self::Break> {
            ControlFlow::Break("subqueries are not allowed in SQL function expressions".into())
        }

        fn pre_visit_expr(&mut self, expr: &Expr) -> ControlFlow<Self::Break> {
            match expr {
                Expr::Identifier(identifier) => {
                    if identifier.value.starts_with('@')
                        || self.parameters.is_some_and(|parameters| {
                            !parameters.contains(&identifier.value.to_ascii_lowercase())
                        })
                    {
                        return ControlFlow::Break(format!(
                            "SQL function body may only reference formal parameters: {}",
                            identifier.value
                        ));
                    }
                }
                Expr::CompoundIdentifier(_) => {
                    if self.parameters.is_some() {
                        return ControlFlow::Break(
                            "qualified identifiers are not allowed in SQL function bodies".into(),
                        );
                    }
                }
                Expr::Subquery(_) | Expr::InSubquery { .. } | Expr::Exists { .. } => {
                    return ControlFlow::Break(
                        "subqueries are not allowed in SQL function expressions".into(),
                    );
                }
                Expr::Function(function) => {
                    let Some(name) = function_name(function) else {
                        return ControlFlow::Break("invalid function name".into());
                    };
                    let lower = name.to_ascii_lowercase();
                    self.calls.push(name.to_owned());
                    if let Some(builtin) = builtin_function(&lower) {
                        if is_builtin_aggregate(&lower) {
                            self.has_builtin_aggregate = true;
                            if !self.sql_is_aggregate_body() {
                                return ControlFlow::Break(
                                    "scalar SQL function cannot call an aggregate function".into(),
                                );
                            }
                        }
                        if builtin.status != "implemented" {
                            return ControlFlow::Break(format!("unsupported function: {name}"));
                        }
                    } else if let Some(host) = self.hosts.lookup(&lower) {
                        if host.is_aggregate() && !self.sql_is_aggregate_body() {
                            return ControlFlow::Break(
                                "scalar SQL function cannot call an aggregate function".into(),
                            );
                        }
                        if self.sql_is_aggregate_body() && host.is_aggregate() {
                            return ControlFlow::Break(
                                "SQL aggregate function cannot call a custom aggregate".into(),
                            );
                        }
                    } else if let Some((_, function)) = self
                        .sql
                        .iter()
                        .find(|(key, _)| key.eq_ignore_ascii_case(&name))
                    {
                        if function.aggregate {
                            return ControlFlow::Break(if self.sql_is_aggregate_body() {
                                "SQL aggregate function cannot call a custom aggregate".into()
                            } else {
                                "scalar SQL function cannot call an aggregate function".into()
                            });
                        }
                    } else {
                        return ControlFlow::Break(format!("unknown function: {name}"));
                    }
                }
                _ => {}
            }
            ControlFlow::Continue(())
        }
    }

    impl Scanner<'_> {
        // The caller communicates aggregate context by setting this small flag below.
        fn sql_is_aggregate_body(&self) -> bool {
            self.aggregate_body
        }
    }

    let expr = parse_function_expression(expression)?;
    let mut scanner = Scanner {
        parameters,
        hosts,
        sql,
        aggregate_body,
        calls: Vec::new(),
        has_builtin_aggregate: false,
    };
    let result = expr.visit(&mut scanner);
    if let ControlFlow::Break(error) = result {
        bail!(error);
    }
    Ok(ExpressionScan {
        calls: scanner.calls,
        has_builtin_aggregate: scanner.has_builtin_aggregate,
    })
}

fn parse_function_expression(expression: &str) -> Result<Expr> {
    let mut parser = Parser::new(&MySqlDialect {}).try_with_sql(expression)?;
    let expression = parser.parse_expr()?;
    if parser.peek_token().token != Token::EOF {
        bail!("expected one SQL expression");
    }
    Ok(expression)
}

fn function_name(function: &Function) -> Option<&str> {
    function
        .name
        .0
        .last()?
        .as_ident()
        .map(|ident| ident.value.as_str())
}

fn builtin_function(
    name: &str,
) -> Option<&'static super::eval::function_catalog::FunctionScopeEntry> {
    super::eval::function_catalog::MARIADB_FUNCTION_SCOPE
        .iter()
        .find(|entry| entry.name.eq_ignore_ascii_case(name))
}

fn is_builtin_aggregate(name: &str) -> bool {
    matches!(
        name.to_ascii_uppercase().as_str(),
        "COUNT"
            | "SUM"
            | "AVG"
            | "STD"
            | "STDDEV"
            | "STDDEV_POP"
            | "STDDEV_SAMP"
            | "VAR_SAMP"
            | "VARIANCE"
            | "VAR_POP"
            | "BIT_OR"
            | "BIT_AND"
            | "BIT_XOR"
            | "MIN"
            | "MAX"
            | "GROUP_CONCAT"
            | "JSON_ARRAYAGG"
            | "JSON_OBJECTAGG"
    )
}

fn is_nondeterministic_builtin(name: &str) -> bool {
    matches!(
        name.to_ascii_uppercase().as_str(),
        "RAND"
            | "RANDOM_BYTES"
            | "UUID"
            | "UUID_SHORT"
            | "NOW"
            | "SYSDATE"
            | "CURRENT_TIMESTAMP"
            | "CURRENT_TIME"
            | "CURRENT_DATE"
    )
}

struct FunctionTokens<'a> {
    tokens: &'a [(Token, &'a str)],
    at: usize,
}

impl FunctionTokens<'_> {
    fn skip_whitespace(&mut self) {
        while self
            .tokens
            .get(self.at)
            .is_some_and(|(token, _)| matches!(token, Token::Whitespace(_)))
        {
            self.at += 1;
        }
    }

    fn take_keyword(&mut self, keyword: &str) -> bool {
        self.skip_whitespace();
        if self.tokens.get(self.at).is_some_and(|(token, _)| {
            matches!(token, Token::Word(Word { value, quote_style: None, .. }) if value.eq_ignore_ascii_case(keyword))
        }) {
            self.at += 1;
            true
        } else {
            false
        }
    }

    fn expect_keyword(&mut self, keyword: &str) -> Result<()> {
        if self.take_keyword(keyword) {
            Ok(())
        } else {
            bail!("expected {keyword}")
        }
    }

    fn take_char(&mut self, character: char) -> bool {
        self.skip_whitespace();
        let expected = match character {
            '(' => Token::LParen,
            ')' => Token::RParen,
            ',' => Token::Comma,
            ';' => Token::SemiColon,
            _ => Token::Char(character),
        };
        if self
            .tokens
            .get(self.at)
            .is_some_and(|(token, _)| *token == expected)
        {
            self.at += 1;
            true
        } else {
            false
        }
    }

    fn expect_char(&mut self, character: char) -> Result<()> {
        if self.take_char(character) {
            Ok(())
        } else {
            bail!("expected '{character}'")
        }
    }

    fn take_identifier(&mut self) -> Result<String> {
        self.skip_whitespace();
        let Some((Token::Word(word), _)) = self.tokens.get(self.at) else {
            bail!("expected SQL identifier")
        };
        self.at += 1;
        Ok(word.value.clone())
    }

    fn take_type(&mut self) -> Result<MysqlColumnType> {
        let value = self.take_identifier()?.to_ascii_uppercase();
        let sql_type = match value.as_str() {
            "NULL" => MysqlColumnType::Null,
            "BOOL" | "BOOLEAN" | "TINYINT" => MysqlColumnType::TinyInt,
            "SMALLINT" => MysqlColumnType::SmallInt,
            "INT" | "INTEGER" | "MEDIUMINT" => MysqlColumnType::Integer,
            "BIGINT" => MysqlColumnType::BigInt,
            "FLOAT" => MysqlColumnType::Float,
            "DOUBLE" | "REAL" => MysqlColumnType::Double,
            "DECIMAL" | "NUMERIC" => MysqlColumnType::Decimal,
            "DATE" => MysqlColumnType::Date,
            "TIME" => MysqlColumnType::Time,
            "DATETIME" => MysqlColumnType::DateTime,
            "TIMESTAMP" => MysqlColumnType::Timestamp,
            "YEAR" => MysqlColumnType::Year,
            "CHAR" => MysqlColumnType::Char,
            "VARCHAR" => MysqlColumnType::VarChar,
            "TEXT" => MysqlColumnType::Text,
            "BINARY" => MysqlColumnType::Binary,
            "VARBINARY" => MysqlColumnType::VarBinary,
            "BLOB" => MysqlColumnType::Blob,
            "MEDIUMBLOB" => MysqlColumnType::MediumBlob,
            "LONGBLOB" => MysqlColumnType::LongBlob,
            "JSON" => MysqlColumnType::Json,
            "BIT" => MysqlColumnType::Bit,
            _ => return Err(anyhow!("unsupported SQL function type: {value}")),
        };
        self.take_type_modifier(&value)?;
        Ok(sql_type)
    }

    fn take_type_modifier(&mut self, value: &str) -> Result<()> {
        if !self.take_char('(') {
            return Ok(());
        }
        let limit = match value {
            "DECIMAL" | "NUMERIC" => 2,
            "BOOL" | "BOOLEAN" | "TINYINT" | "SMALLINT" | "INT" | "INTEGER" | "MEDIUMINT"
            | "BIGINT" | "FLOAT" | "DOUBLE" | "REAL" | "CHAR" | "VARCHAR" | "BINARY"
            | "VARBINARY" | "BIT" => 1,
            _ => bail!("type {value} does not accept a length"),
        };
        for index in 0..limit {
            if index > 0 && !self.take_char(',') {
                break;
            }
            self.skip_whitespace();
            if !matches!(self.tokens.get(self.at), Some((Token::Number(number, false), _)) if number.bytes().all(|byte| byte.is_ascii_digit()))
            {
                bail!("type length must be an unsigned integer");
            }
            self.at += 1;
            if index + 1 == limit || !self.peek_char(',') {
                break;
            }
        }
        self.expect_char(')')
    }

    fn peek_char(&mut self, character: char) -> bool {
        self.skip_whitespace();
        let expected = match character {
            '(' => Token::LParen,
            ')' => Token::RParen,
            ',' => Token::Comma,
            ';' => Token::SemiColon,
            _ => Token::Char(character),
        };
        self.tokens
            .get(self.at)
            .is_some_and(|(token, _)| *token == expected)
    }

    fn finish(&mut self) -> Result<()> {
        if self.take_char(';') {
            self.skip_whitespace();
        }
        if self.at == self.tokens.len()
            || self
                .tokens
                .get(self.at)
                .is_some_and(|(token, _)| *token == Token::EOF)
        {
            return Ok(());
        }
        bail!("unexpected tokens after SQL function command")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_scalar_and_aggregate_definitions_with_type_modifiers() -> Result<()> {
        let Some(SqlFunctionCommand::Create(scalar)) = parse_function_command(
            "CREATE FUNCTION f(x INT(11), label VARCHAR(255)) RETURNS DECIMAL(10, 2) RETURN x + 1",
        )?
        else {
            bail!("expected scalar function definition");
        };
        assert_eq!(scalar.name, "f");
        assert_eq!(
            scalar.args,
            [
                ("x".into(), MysqlColumnType::Integer),
                ("label".into(), MysqlColumnType::VarChar)
            ]
        );
        assert_eq!(scalar.return_type, MysqlColumnType::Decimal);
        assert_eq!(scalar.body, "x + 1");
        assert!(!scalar.aggregate);

        let Some(SqlFunctionCommand::Create(aggregate)) = parse_function_command(
            "CREATE AGGREGATE FUNCTION total(x DOUBLE) RETURNS DOUBLE RETURN SUM(x)",
        )?
        else {
            bail!("expected aggregate function definition");
        };
        assert!(aggregate.aggregate);
        Ok(())
    }

    #[test]
    fn parses_drop_and_rejects_malformed_commands() -> Result<()> {
        assert!(matches!(
            parse_function_command("DROP AGGREGATE FUNCTION IF EXISTS total;")?,
            Some(SqlFunctionCommand::Drop {
                aggregate: true,
                if_exists: true,
                ..
            })
        ));
        assert!(parse_function_command("CREATE FUNCTION f(x INT) RETURNS INT RETURN x +").is_err());
        assert!(
            parse_function_command("CREATE FUNCTION f(x INT(abc)) RETURNS INT RETURN x").is_err()
        );
        assert!(
            parse_function_command("CREATE FUNCTION f(x INT) RETURNS INT RETURN x; DROP TABLE t")
                .is_err()
        );
        Ok(())
    }

    #[test]
    fn ignores_non_function_statements() -> Result<()> {
        assert!(parse_function_command("CREATE TABLE t (id INT)")?.is_none());
        assert!(parse_function_command("SELECT 1")?.is_none());
        Ok(())
    }

    fn definition(name: &str, body: &str, aggregate: bool) -> SqlFunctionDef {
        SqlFunctionDef {
            name: name.into(),
            args: vec![("x".into(), MysqlColumnType::Integer)],
            return_type: MysqlColumnType::Integer,
            body: body.into(),
            aggregate,
        }
    }

    #[test]
    fn validates_sql_function_references_and_aggregate_rules() -> Result<()> {
        let hosts = super::super::HostFunctionRegistry::new();
        let sql = BTreeMap::new();
        validate_function_body(&definition("f", "ABS(x) + 1", false), &hosts, &sql)?;
        assert!(
            validate_function_body(&definition("f", "x + outer_col", false), &hosts, &sql).is_err()
        );
        assert!(validate_function_body(&definition("f", "t.x", false), &hosts, &sql).is_err());
        assert!(
            validate_function_body(&definition("f", "missing(x)", false), &hosts, &sql).is_err()
        );
        assert!(validate_function_body(&definition("f", "COUNT(x)", false), &hosts, &sql).is_err());
        assert!(
            validate_function_body(&definition("f", "(SELECT x)", false), &hosts, &sql).is_err()
        );
        validate_function_body(&definition("f", "SUM(x)", true), &hosts, &sql)?;
        assert!(validate_function_body(&definition("f", "ABS(x)", true), &hosts, &sql).is_err());

        let custom_aggregate =
            BTreeMap::from([("other".into(), definition("other", "SUM(x)", true))]);
        assert!(
            validate_function_body(
                &definition("f", "other(x) + SUM(x)", true),
                &hosts,
                &custom_aggregate
            )
            .is_err()
        );
        Ok(())
    }

    #[test]
    fn validates_schema_function_dependencies_and_cycles() -> Result<()> {
        let hosts = super::super::HostFunctionRegistry::new();
        assert!(validate_schema_expression("RAND()", &hosts, &BTreeMap::new()).is_ok());
        assert!(validate_schema_expression("missing()", &hosts, &BTreeMap::new()).is_err());
        let sql = BTreeMap::from([("f".into(), definition("f", "ABS(x)", false))]);
        validate_schema_expression("f(1)", &hosts, &sql)?;
        let nondeterministic = BTreeMap::from([("f".into(), definition("f", "RAND() + x", false))]);
        assert!(validate_schema_expression("f(1)", &hosts, &nondeterministic).is_err());
        let cyclic = BTreeMap::from([
            ("f".into(), definition("f", "g(x)", false)),
            ("g".into(), definition("g", "f(x)", false)),
        ]);
        assert!(validate_schema_expression("f(1)", &hosts, &cyclic).is_err());
        assert!(expression_calls_function("ABS(f(1))", "f"));
        assert!(!expression_calls_function("ABS(1)", "f"));
        Ok(())
    }
}
