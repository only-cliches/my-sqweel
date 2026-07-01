use super::*;

pub(super) fn value_to_i64(value: &Value) -> Option<i64> {
    match value {
        Value::Number(number) => number
            .as_i64()
            .or_else(|| number.as_u64().and_then(|value| value.try_into().ok()))
            .or_else(|| number.as_f64().map(|value| value as i64)),
        Value::String(value) => value
            .parse::<i64>()
            .ok()
            .or_else(|| value.parse::<f64>().ok().map(|value| value as i64)),
        Value::Bool(value) => Some(i64::from(*value)),
        _ => None,
    }
}

pub(super) fn split_top_level_keyword<'a>(
    text: &'a str,
    keyword: &str,
) -> Option<(&'a str, &'a str)> {
    let mut depth = 0_i32;
    let mut in_single = false;
    let mut in_double = false;
    let keyword_upper = keyword.to_ascii_uppercase();
    for (idx, ch) in text.char_indices() {
        match ch {
            '\'' if !in_double => in_single = !in_single,
            '"' if !in_single => in_double = !in_double,
            '(' if !in_single && !in_double => depth += 1,
            ')' if !in_single && !in_double => depth -= 1,
            _ => {}
        }
        if depth == 0 && !in_single && !in_double {
            let rest = &text[idx..];
            if rest.len() >= keyword.len()
                && rest[..keyword.len()].eq_ignore_ascii_case(&keyword_upper)
                && text[..idx].chars().last().is_some_and(char::is_whitespace)
                && rest[keyword.len()..]
                    .chars()
                    .next()
                    .is_some_and(char::is_whitespace)
            {
                return Some((text[..idx].trim(), rest[keyword.len()..].trim()));
            }
        }
    }
    None
}

pub(super) fn unquote_sql_string(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    let quote = trimmed.chars().next()?;
    if quote != '\'' && quote != '"' {
        return None;
    }
    if !trimmed.ends_with(quote) || trimmed.len() < 2 {
        return None;
    }
    Some(
        trimmed[quote.len_utf8()..trimmed.len() - quote.len_utf8()]
            .replace("''", "'")
            .replace("\\'", "'")
            .replace("\\\"", "\""),
    )
}
