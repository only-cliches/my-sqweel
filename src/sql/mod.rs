pub mod engine;

use sqlparser::ast::Statement;
use sqlparser::dialect::{Dialect, MySqlDialect};
use sqlparser::parser::Parser;
use sqlparser::tokenizer::{Token, Tokenizer, TokenizerError};

// Token boundaries must also work for account names like 'user'@'host'.
// MySqlDialect otherwise consumes the quote after @ as part of an identifier.
#[derive(Debug)]
struct BoundaryDialect;

impl Dialect for BoundaryDialect {
    fn dialect(&self) -> std::any::TypeId {
        std::any::TypeId::of::<MySqlDialect>()
    }
    fn is_identifier_start(&self, ch: char) -> bool {
        ch != '@' && MySqlDialect {}.is_identifier_start(ch)
    }
    fn is_identifier_part(&self, ch: char) -> bool {
        self.is_identifier_start(ch) || ch.is_ascii_digit()
    }
    fn is_delimited_identifier_start(&self, ch: char) -> bool {
        ch == '`'
    }
    fn supports_string_literal_backslash_escape(&self) -> bool {
        true
    }
    fn supports_numeric_prefix(&self) -> bool {
        true
    }
}

/// Tokenize once while retaining the original spelling and UTF-8 byte boundaries.
pub(crate) fn sql_tokens(sql: &str) -> Result<Vec<(Token, &str)>, TokenizerError> {
    let tokens = Tokenizer::new(&BoundaryDialect, sql).tokenize_with_location()?;
    let mut chars = sql.char_indices();
    let (mut line, mut column, mut end) = (1, 1, 0);
    let mut result = Vec::with_capacity(tokens.len());
    for token in tokens {
        let start = end;
        while (line, column) < (token.span.end.line, token.span.end.column) {
            let (index, ch) = chars.next().expect("token span is within SQL source");
            end = index + ch.len_utf8();
            if ch == '\n' {
                line += 1;
                column = 1;
            } else {
                column += 1;
            }
        }
        result.push((token.token, &sql[start..end]));
    }
    Ok(result)
}

pub fn parse(sql: &str) -> Result<Vec<Statement>, sqlparser::parser::ParserError> {
    let parser_sql = rewrite_mysql_extract_year_month(&rewrite_mysql_distinctrow(
        &rewrite_mysql_compound_intervals(sql),
    ));
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

fn rewrite_mysql_extract_year_month(sql: &str) -> String {
    let bytes = sql.as_bytes();
    let mut output = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        let is_extract = index + 7 <= bytes.len()
            && bytes[index..index + 7].eq_ignore_ascii_case(b"EXTRACT")
            && (index == 0
                || !bytes[index - 1].is_ascii_alphanumeric() && bytes[index - 1] != b'_')
            && (index + 7 == bytes.len()
                || !bytes[index + 7].is_ascii_alphanumeric() && bytes[index + 7] != b'_');
        if !is_extract {
            output.push(bytes[index]);
            index += 1;
            continue;
        }

        let mut cursor = index + 7;
        while cursor < bytes.len() && bytes[cursor].is_ascii_whitespace() {
            cursor += 1;
        }
        if bytes.get(cursor) != Some(&b'(') {
            output.push(bytes[index]);
            index += 1;
            continue;
        }
        cursor += 1;
        while cursor < bytes.len() && bytes[cursor].is_ascii_whitespace() {
            cursor += 1;
        }
        let field_start = cursor;
        while cursor < bytes.len()
            && (bytes[cursor].is_ascii_alphanumeric() || bytes[cursor] == b'_')
        {
            cursor += 1;
        }
        if !bytes[field_start..cursor].eq_ignore_ascii_case(b"YEAR_MONTH") {
            output.extend_from_slice(&bytes[index..cursor]);
            index = cursor;
            continue;
        }
        while cursor < bytes.len() && bytes[cursor].is_ascii_whitespace() {
            cursor += 1;
        }
        let from_start = cursor;
        while cursor < bytes.len() && bytes[cursor].is_ascii_alphabetic() {
            cursor += 1;
        }
        if !bytes[from_start..cursor].eq_ignore_ascii_case(b"FROM") {
            output.extend_from_slice(&bytes[index..cursor]);
            index = cursor;
            continue;
        }
        while cursor < bytes.len() && bytes[cursor].is_ascii_whitespace() {
            cursor += 1;
        }
        let expression_start = cursor;
        let mut depth = 0_u32;
        let mut in_single = false;
        let mut in_double = false;
        let mut in_backtick = false;
        while cursor < bytes.len() {
            let byte = bytes[cursor];
            if (in_single || in_double) && byte == b'\\' {
                cursor = cursor.saturating_add(2);
                continue;
            }
            if in_single {
                if byte == b'\'' {
                    in_single = false;
                }
            } else if in_double {
                if byte == b'"' {
                    in_double = false;
                }
            } else if in_backtick {
                if byte == b'`' {
                    in_backtick = false;
                }
            } else {
                match byte {
                    b'\'' => in_single = true,
                    b'"' => in_double = true,
                    b'`' => in_backtick = true,
                    b'(' => depth += 1,
                    b')' if depth == 0 => break,
                    b')' => depth -= 1,
                    _ => {}
                }
            }
            cursor += 1;
        }
        if cursor >= bytes.len() || bytes.get(cursor) != Some(&b')') {
            output.extend_from_slice(&bytes[index..cursor]);
            index = cursor;
            continue;
        }
        output.extend_from_slice(b"EXTRACT_YEAR_MONTH(");
        output.extend_from_slice(&bytes[expression_start..cursor]);
        output.push(b')');
        index = cursor + 1;
    }
    String::from_utf8(output).expect("SQL input must be UTF-8")
}

fn rewrite_mysql_distinctrow(sql: &str) -> String {
    let bytes = sql.as_bytes();
    let mut output = Vec::with_capacity(bytes.len());
    let needle = b"DISTINCTROW";
    let big_result = b"SQL_BIG_RESULT";
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
                output.extend_from_slice(b"DISTINCT");
                index += needle.len();
                continue;
            }
            if index + big_result.len() <= bytes.len()
                && bytes[index..index + big_result.len()].eq_ignore_ascii_case(big_result)
                && !bytes
                    .get(index.wrapping_sub(1))
                    .is_some_and(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
                && !bytes
                    .get(index + big_result.len())
                    .is_some_and(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
            {
                index += big_result.len();
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

#[cfg(test)]
mod tests {
    use super::parse;

    #[test]
    fn parses_mysql_year_month_extract() {
        let statements =
            parse("SELECT EXTRACT(YEAR_MONTH FROM '2026-01-05 08:00:00') AS month_key")
                .expect("YEAR_MONTH should parse through the MySQL compatibility rewrite");
        assert_eq!(statements.len(), 1);
    }
}
