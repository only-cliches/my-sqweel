pub mod engine;

use sqlparser::ast::Statement;
use sqlparser::dialect::MySqlDialect;
use sqlparser::parser::Parser;

pub fn parse(sql: &str) -> Result<Vec<Statement>, sqlparser::parser::ParserError> {
    match Parser::parse_sql(&MySqlDialect {}, sql) {
        Ok(statements) => Ok(statements),
        Err(err) => {
            if let Some(rewritten) = rewrite_drop_index_on_table(sql) {
                Parser::parse_sql(&MySqlDialect {}, &rewritten)
            } else {
                Err(err)
            }
        }
    }
}

fn rewrite_drop_index_on_table(sql: &str) -> Option<String> {
    let trimmed = sql.trim().trim_end_matches(';').trim();
    let tokens = trimmed.split_whitespace().collect::<Vec<_>>();
    if tokens.len() >= 5
        && tokens[0].eq_ignore_ascii_case("DROP")
        && tokens[1].eq_ignore_ascii_case("INDEX")
        && tokens[3].eq_ignore_ascii_case("ON")
    {
        return Some(format!("DROP INDEX {}", tokens[2]));
    }
    None
}
