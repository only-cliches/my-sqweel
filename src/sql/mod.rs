pub mod engine;

use sqlparser::ast::Statement;
use sqlparser::dialect::MySqlDialect;
use sqlparser::parser::Parser;

pub fn parse(sql: &str) -> Result<Vec<Statement>, sqlparser::parser::ParserError> {
    let parser_sql = rewrite_mysql_compound_intervals(sql);
    let rewritten_assignments = if parser_sql.contains(":=")
        && !parser_sql
            .trim_start()
            .to_ascii_uppercase()
            .starts_with("SET ")
    {
        rewrite_user_variable_assignments(&parser_sql)
    } else {
        None
    };
    let parse_sql = rewritten_assignments.as_deref().unwrap_or(&parser_sql);
    match Parser::parse_sql(&MySqlDialect {}, parse_sql) {
        Ok(statements) => Ok(statements),
        Err(err) => {
            if let Some(rewritten) = rewrite_user_variable_assignments(&parser_sql)
                .or_else(|| rewrite_drop_index_on_table(&parser_sql))
            {
                Parser::parse_sql(&MySqlDialect {}, &rewritten)
            } else {
                Err(err)
            }
        }
    }
}

fn rewrite_mysql_compound_intervals(sql: &str) -> String {
    let bytes = sql.as_bytes();
    let mut output = Vec::with_capacity(bytes.len());
    let needle = b"HOUR_MINUTE";
    let mut index = 0;
    let mut in_single = false;
    let mut in_double = false;
    let mut in_backtick = false;
    while index < bytes.len() {
        let byte = bytes[index];
        if (in_single || in_double) && byte == b'\\' && index + 1 < bytes.len() {
            output.extend_from_slice(&bytes[index..index + 2]);
            index += 2;
            continue;
        }
        if in_single && byte == b'\'' {
            output.push(byte);
            if index + 1 < bytes.len() && bytes[index + 1] == b'\'' {
                output.push(b'\'');
                index += 2;
            } else {
                in_single = false;
                index += 1;
            }
            continue;
        }
        if in_double && byte == b'"' {
            output.push(byte);
            if index + 1 < bytes.len() && bytes[index + 1] == b'"' {
                output.push(b'"');
                index += 2;
            } else {
                in_double = false;
                index += 1;
            }
            continue;
        }
        if in_backtick && byte == b'`' {
            output.push(byte);
            if index + 1 < bytes.len() && bytes[index + 1] == b'`' {
                output.push(b'`');
                index += 2;
            } else {
                in_backtick = false;
                index += 1;
            }
            continue;
        }
        if !in_single && !in_double && !in_backtick {
            match byte {
                b'\'' => in_single = true,
                b'"' => in_double = true,
                b'`' => in_backtick = true,
                _ => {}
            }
            if index + needle.len() <= bytes.len()
                && bytes[index..index + needle.len()].eq_ignore_ascii_case(needle)
                && !bytes
                    .get(index.wrapping_sub(1))
                    .is_some_and(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
                && !bytes
                    .get(index + needle.len())
                    .is_some_and(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
            {
                output.extend_from_slice(b"HOUR TO MINUTE");
                index += needle.len();
                continue;
            }
        }
        output.push(byte);
        index += 1;
    }
    String::from_utf8(output).expect("SQL input must be UTF-8")
}

/// sqlparser does not accept MySQL's expression assignment operator (`:=`)
/// in every expression context. Rewrite it to an internal function so the
/// evaluator can apply the assignment with normal expression semantics.
fn rewrite_user_variable_assignments(sql: &str) -> Option<String> {
    if !sql.contains(":=") {
        return None;
    }

    let bytes = sql.as_bytes();
    let mut output = String::with_capacity(sql.len() + 32);
    let mut index = 0;
    let mut in_single = false;
    let mut in_double = false;
    let mut changed = false;
    while index < bytes.len() {
        let byte = bytes[index];
        if byte == b'\'' && !in_double {
            output.push(byte as char);
            if index + 1 < bytes.len() && bytes[index + 1] == b'\'' {
                output.push('\'');
                index += 2;
                continue;
            }
            in_single = !in_single;
            index += 1;
            continue;
        }
        if byte == b'"' && !in_single {
            in_double = !in_double;
            output.push(byte as char);
            index += 1;
            continue;
        }
        if !in_single
            && !in_double
            && byte == b'@'
            && let Some(name_end) = (index + 1..bytes.len()).find(|position| {
                !bytes[*position].is_ascii_alphanumeric()
                    && bytes[*position] != b'_'
                    && bytes[*position] != b'$'
            })
        {
            let mut operator = name_end;
            while operator < bytes.len() && bytes[operator].is_ascii_whitespace() {
                operator += 1;
            }
            if bytes.get(operator..operator + 2) == Some(b":=") {
                let name = &sql[index + 1..name_end];
                output.push_str("USER_VAR_ASSIGN('");
                output.push_str(name.replace('\'', "''").as_str());
                output.push_str("', ");
                index = operator + 2;
                let mut depth = 0_u32;
                let mut rhs_single = false;
                let mut rhs_double = false;
                while index < bytes.len() {
                    let rhs_byte = bytes[index];
                    if rhs_byte == b'\'' && !rhs_double {
                        rhs_single = !rhs_single;
                    } else if rhs_byte == b'"' && !rhs_single {
                        rhs_double = !rhs_double;
                    } else if !rhs_single && !rhs_double {
                        if rhs_byte == b'(' {
                            depth += 1;
                        } else if rhs_byte == b')' {
                            if depth == 0 {
                                break;
                            }
                            depth -= 1;
                        } else if rhs_byte == b',' && depth == 0 {
                            break;
                        } else if depth == 0
                            && bytes
                                .get(index..index + 4)
                                .is_some_and(|slice| slice.eq_ignore_ascii_case(b" AS "))
                        {
                            break;
                        } else if depth == 0
                            && [
                                " FROM ",
                                " WHERE ",
                                " GROUP BY ",
                                " HAVING ",
                                " ORDER BY ",
                                " LIMIT ",
                                " ON DUPLICATE KEY UPDATE ",
                                " RETURNING ",
                            ]
                            .iter()
                            .any(|keyword| {
                                bytes
                                    .get(index..index + keyword.len())
                                    .is_some_and(|slice| {
                                        slice.eq_ignore_ascii_case(keyword.as_bytes())
                                    })
                            })
                        {
                            break;
                        }
                    }
                    output.push(rhs_byte as char);
                    index += 1;
                }
                output.push(')');
                changed = true;
                continue;
            }
        }
        output.push(byte as char);
        index += 1;
    }
    changed.then_some(output)
}

fn rewrite_drop_index_on_table(sql: &str) -> Option<String> {
    let trimmed = sql.trim().trim_end_matches(';').trim();
    let tokens = trimmed.split_whitespace().collect::<Vec<_>>();
    if tokens.len() >= 5
        && tokens[0].eq_ignore_ascii_case("DROP")
        && tokens[1].eq_ignore_ascii_case("INDEX")
    {
        let (name, on_pos) = if tokens[2].eq_ignore_ascii_case("IF")
            && tokens
                .get(3)
                .is_some_and(|token| token.eq_ignore_ascii_case("EXISTS"))
        {
            (tokens.get(4)?, 5)
        } else {
            (tokens.get(2)?, 3)
        };
        if tokens
            .get(on_pos)
            .is_some_and(|token| token.eq_ignore_ascii_case("ON"))
        {
            let if_exists = tokens[2].eq_ignore_ascii_case("IF");
            return Some(if if_exists {
                format!("DROP INDEX IF EXISTS {name}")
            } else {
                format!("DROP INDEX {name}")
            });
        }
    }
    None
}
