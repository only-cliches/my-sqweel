use super::*;
use md5::{Digest, Md5};
use sha1_smol::Sha1;
use sha2::Sha256;
use regex::{NoExpand, Regex};

pub(super) fn eval_arg(
    arg: Option<&String>,
    data: &Map<String, Value>,
    last_insert_id: u64,
) -> Result<Value> {
    arg.map(|arg| eval_scalar_text(arg, data, last_insert_id))
        .transpose()
        .map(|value| value.unwrap_or(Value::Null))
}

pub(super) fn eval_unary_number(
    arg: Option<&String>,
    data: &Map<String, Value>,
    last_insert_id: u64,
    f: impl FnOnce(f64) -> f64,
) -> Result<Value> {
    let value = eval_arg(arg, data, last_insert_id)?;
    if value == Value::Null {
        return Ok(Value::Null);
    }
    let out = f(json_to_f64_lossy(&value)?);
    if out.is_finite() {
        Ok(number_from_f64(out))
    } else {
        Ok(Value::Null)
    }
}

pub(super) fn eval_log(
    first_arg: Option<&String>,
    second_arg: Option<&String>,
    data: &Map<String, Value>,
    last_insert_id: u64,
) -> Result<Value> {
    let first = eval_arg(first_arg, data, last_insert_id)?;
    if first == Value::Null {
        return Ok(Value::Null);
    }
    let out = if let Some(second_arg) = second_arg {
        let second = eval_arg(Some(second_arg), data, last_insert_id)?;
        if second == Value::Null {
            return Ok(Value::Null);
        }
        json_to_f64_lossy(&second)?.log(json_to_f64_lossy(&first)?)
    } else {
        json_to_f64_lossy(&first)?.ln()
    };
    if out.is_finite() {
        Ok(number_from_f64(out))
    } else {
        Ok(Value::Null)
    }
}

pub(super) fn eval_truncate(
    value_arg: Option<&String>,
    places_arg: Option<&String>,
    data: &Map<String, Value>,
    last_insert_id: u64,
) -> Result<Value> {
    let value = eval_arg(value_arg, data, last_insert_id)?;
    if value == Value::Null {
        return Ok(Value::Null);
    }
    let places = eval_arg(places_arg, data, last_insert_id)?
        .as_i64()
        .unwrap_or(0);
    let factor = 10_f64.powi(places as i32);
    Ok(number_from_f64(
        (json_to_f64_lossy(&value)? * factor).trunc() / factor,
    ))
}

pub(super) fn eval_format_number(
    value_arg: Option<&String>,
    decimals_arg: Option<&String>,
    data: &Map<String, Value>,
    last_insert_id: u64,
) -> Result<Value> {
    let value = eval_arg(value_arg, data, last_insert_id)?;
    let decimals = eval_arg(decimals_arg, data, last_insert_id)?;
    if value == Value::Null || decimals == Value::Null {
        return Ok(Value::Null);
    }
    let number = json_to_f64_lossy(&value)?;
    if !number.is_finite() {
        return Ok(Value::Null);
    }
    let decimals = value_to_i64(&decimals).unwrap_or(0).clamp(0, 30) as usize;
    let rendered = format!("{number:.decimals$}");
    let (sign, unsigned) = rendered
        .strip_prefix('-')
        .map_or(("", rendered.as_str()), |_| ("-", &rendered[1..]));
    let (integer, fraction) = unsigned
        .split_once('.')
        .map_or((unsigned, ""), |(integer, fraction)| (integer, fraction));
    let mut grouped = String::with_capacity(unsigned.len() + integer.len() / 3);
    for (index, character) in integer.chars().enumerate() {
        if index > 0 && (integer.len() - index) % 3 == 0 {
            grouped.push(',');
        }
        grouped.push(character);
    }
    let suffix = if fraction.is_empty() {
        String::new()
    } else {
        format!(".{fraction}")
    };
    Ok(Value::String(format!("{sign}{grouped}{suffix}")))
}
pub(super) fn eval_inet_aton(
    value_arg: Option<&String>,
    data: &Map<String, Value>,
    last_insert_id: u64,
) -> Result<Value> {
    eval_inet_aton_value(eval_arg(value_arg, data, last_insert_id)?)
}

pub(super) fn eval_inet_aton_value(value: Value) -> Result<Value> {
    if value == Value::Null {
        return Ok(Value::Null);
    }
    let text = json_scalar_to_string(&value);
    let mut address = 0_u64;
    let mut parts = text.split('.');
    for _ in 0..4 {
        let Some(part) = parts.next() else {
            return Ok(Value::Null);
        };
        let Ok(octet) = part.parse::<u64>() else {
            return Ok(Value::Null);
        };
        if octet > 255 {
            return Ok(Value::Null);
        }
        address = (address << 8) | octet;
    }
    if parts.next().is_some() {
        return Ok(Value::Null);
    }
    Ok(Value::Number(Number::from(address)))
}

pub(super) fn eval_mod(
    left_arg: Option<&String>,
    right_arg: Option<&String>,
    data: &Map<String, Value>,
    last_insert_id: u64,
) -> Result<Value> {
    let left = eval_arg(left_arg, data, last_insert_id)?;
    let right = eval_arg(right_arg, data, last_insert_id)?;
    if left == Value::Null || right == Value::Null {
        return Ok(Value::Null);
    }
    let divisor = json_to_f64_lossy(&right)?;
    if divisor == 0.0 {
        Ok(Value::Null)
    } else {
        Ok(number_from_f64(json_to_f64_lossy(&left)? % divisor))
    }
}

#[derive(Debug, Clone, Copy)]
pub(super) enum ExtremeKind {
    Greatest,
    Least,
}

pub(super) fn eval_extreme(
    args: &[String],
    data: &Map<String, Value>,
    last_insert_id: u64,
    kind: ExtremeKind,
) -> Result<Value> {
    let mut values = Vec::new();
    for arg in args {
        let value = eval_scalar_text(arg, data, last_insert_id)?;
        if value == Value::Null {
            return Ok(Value::Null);
        }
        values.push(value);
    }
    let value = match kind {
        ExtremeKind::Greatest => values.into_iter().max_by(compare_json_values),
        ExtremeKind::Least => values.into_iter().min_by(compare_json_values),
    };
    Ok(value.unwrap_or(Value::Null))
}

pub(super) fn eval_ascii_ord(
    arg: Option<&String>,
    data: &Map<String, Value>,
    last_insert_id: u64,
) -> Result<Value> {
    let value = eval_arg(arg, data, last_insert_id)?;
    if value == Value::Null {
        return Ok(Value::Null);
    }
    let s = json_scalar_to_string(&value);
    Ok(Value::Number(Number::from(
        s.chars().next().map(u32::from).unwrap_or(0),
    )))
}

pub(super) fn eval_left_right(
    string_arg: Option<&String>,
    len_arg: Option<&String>,
    data: &Map<String, Value>,
    last_insert_id: u64,
    from_right: bool,
) -> Result<Value> {
    let value = eval_arg(string_arg, data, last_insert_id)?;
    let len = eval_arg(len_arg, data, last_insert_id)?;
    if value == Value::Null || len == Value::Null {
        return Ok(Value::Null);
    }
    let len = value_to_i64(&len).unwrap_or(0).max(0) as usize;
    let chars = json_scalar_to_string(&value).chars().collect::<Vec<_>>();
    let out = if from_right {
        chars
            .iter()
            .skip(chars.len().saturating_sub(len))
            .collect::<String>()
    } else {
        chars.iter().take(len).collect::<String>()
    };
    Ok(Value::String(out))
}

pub(super) fn eval_pad(
    string_arg: Option<&String>,
    len_arg: Option<&String>,
    pad_arg: Option<&String>,
    data: &Map<String, Value>,
    last_insert_id: u64,
    pad_right: bool,
) -> Result<Value> {
    let value = eval_arg(string_arg, data, last_insert_id)?;
    let len = eval_arg(len_arg, data, last_insert_id)?;
    let pad = eval_arg(pad_arg, data, last_insert_id)?;
    if value == Value::Null || len == Value::Null || pad == Value::Null {
        return Ok(Value::Null);
    }
    let target_len = value_to_i64(&len).unwrap_or(0);
    if target_len < 0 {
        return Ok(Value::Null);
    }
    let target_len = target_len as usize;
    let mut chars = json_scalar_to_string(&value).chars().collect::<Vec<_>>();
    if chars.len() >= target_len {
        return Ok(Value::String(chars.into_iter().take(target_len).collect()));
    }
    let pad_chars = json_scalar_to_string(&pad).chars().collect::<Vec<_>>();
    if pad_chars.is_empty() {
        return Ok(Value::Null);
    }
    let mut fill = Vec::new();
    while chars.len() + fill.len() < target_len {
        for ch in &pad_chars {
            if chars.len() + fill.len() >= target_len {
                break;
            }
            fill.push(*ch);
        }
    }
    let out = if pad_right {
        chars.extend(fill);
        chars.into_iter().collect()
    } else {
        fill.extend(chars);
        fill.into_iter().collect()
    };
    Ok(Value::String(out))
}

pub(super) fn eval_locate(
    needle_arg: Option<&String>,
    haystack_arg: Option<&String>,
    start_arg: Option<&String>,
    data: &Map<String, Value>,
    last_insert_id: u64,
) -> Result<Value> {
    let needle = eval_arg(needle_arg, data, last_insert_id)?;
    let haystack = eval_arg(haystack_arg, data, last_insert_id)?;
    if needle == Value::Null || haystack == Value::Null {
        return Ok(Value::Null);
    }
    let needle = json_scalar_to_string(&needle);
    let haystack = json_scalar_to_string(&haystack);
    let start = eval_arg(start_arg, data, last_insert_id)
        .ok()
        .and_then(|value| value_to_i64(&value))
        .unwrap_or(1)
        .max(1) as usize;
    let chars = haystack.chars().collect::<Vec<_>>();
    if start > chars.len() + 1 {
        return Ok(Value::Number(Number::from(0)));
    }
    let suffix = chars.iter().skip(start - 1).collect::<String>();
    let pos = suffix
        .find(&needle)
        .map(|idx| start + suffix[..idx].chars().count());
    Ok(Value::Number(Number::from(pos.unwrap_or(0))))
}

pub(super) fn eval_instr(
    haystack_arg: Option<&String>,
    needle_arg: Option<&String>,
    data: &Map<String, Value>,
    last_insert_id: u64,
) -> Result<Value> {
    eval_locate(needle_arg, haystack_arg, None, data, last_insert_id)
}

pub(super) fn eval_position(
    arg: Option<&String>,
    data: &Map<String, Value>,
    last_insert_id: u64,
) -> Result<Value> {
    let Some(arg) = arg else {
        return Ok(Value::Null);
    };
    let Some((needle, haystack)) = split_top_level_keyword(arg, "IN") else {
        return Ok(Value::Null);
    };
    let needle = eval_scalar_text(needle, data, last_insert_id)?;
    let haystack = eval_scalar_text(haystack, data, last_insert_id)?;
    if needle == Value::Null || haystack == Value::Null {
        return Ok(Value::Null);
    }
    eval_position_values(needle, haystack)
}

pub(super) fn eval_position_values(needle: Value, haystack: Value) -> Result<Value> {
    if needle == Value::Null || haystack == Value::Null {
        return Ok(Value::Null);
    }
    let needle = json_scalar_to_string(&needle);
    let haystack = json_scalar_to_string(&haystack);
    let pos = haystack
        .find(&needle)
        .map(|idx| haystack[..idx].chars().count() + 1)
        .unwrap_or(0);
    Ok(Value::Number(Number::from(pos)))
}
pub(super) fn eval_find_in_set_values(needle: Value, list: Value) -> Result<Value> {
    if needle == Value::Null || list == Value::Null {
        return Ok(Value::Null);
    }
    let needle = json_scalar_to_string(&needle);
    if needle.contains(',') {
        return Ok(Value::Number(Number::from(0)));
    }
    let list = json_scalar_to_string(&list);
    let position = list
        .split(',')
        .position(|candidate| candidate == needle)
        .map(|index| index as u64 + 1)
        .unwrap_or(0);
    Ok(Value::Number(Number::from(position)))
}
pub(super) fn eval_make_set_values<I>(bits: Value, values: I) -> Result<Value>
where
    I: IntoIterator<Item = Result<Value>>,
{
    if bits == Value::Null {
        return Ok(Value::Null);
    }
    let bits = value_to_i64(&bits).unwrap_or(0) as u64;
    let mut out = String::new();
    for (index, value) in values.into_iter().enumerate() {
        let value = value?;
        if index >= u64::BITS as usize
            || bits & (1_u64 << index) == 0
            || value == Value::Null
        {
            continue;
        }
        if !out.is_empty() {
            out.push(',');
        }
        out.push_str(&json_scalar_to_string(&value));
    }
    Ok(Value::String(out))
}

pub(super) fn eval_substring_values(
    value: Value,
    start: Option<Value>,
    len: Option<Value>,
) -> Result<Value> {
    if value == Value::Null {
        return Ok(Value::Null);
    }
    let pos = start.as_ref().and_then(value_to_i64).unwrap_or(1);
    let len = len.as_ref().and_then(value_to_i64);
    let s = json_scalar_to_string(&value);
    let chars: Vec<char> = s.chars().collect();
    let start = if pos < 0 {
        std::cmp::max(0, (chars.len() as i64) + pos) as usize
    } else {
        std::cmp::max(0, pos - 1) as usize
    };
    if start >= chars.len() {
        return Ok(Value::String(String::new()));
    }
    let end = if let Some(len) = len {
        if len <= 0 {
            start
        } else {
            std::cmp::min(chars.len(), start + len as usize)
        }
    } else {
        chars.len()
    };
    Ok(Value::String(chars[start..end].iter().collect()))
}

pub(super) fn eval_substring_index_values(value: Value, delimiter: Value, count: Value) -> Value {
    if value == Value::Null || delimiter == Value::Null || count == Value::Null {
        return Value::Null;
    }
    let value = json_scalar_to_string(&value);
    let delimiter = json_scalar_to_string(&delimiter);
    let count = value_to_i64(&count).unwrap_or(0);
    if count == 0 || delimiter.is_empty() {
        return Value::String(String::new());
    }
    let parts = value.split(&delimiter).collect::<Vec<_>>();
    let result = if count > 0 {
        let end = (count as usize).min(parts.len());
        parts[..end].join(&delimiter)
    } else {
        let start = parts.len().saturating_sub((-count) as usize);
        parts[start..].join(&delimiter)
    };
    Value::String(result)
}

pub(super) fn eval_regexp_replace_values(values: &[Value]) -> Result<Value> {
    let [subject, pattern, replacement, ..] = values else {
        return Err(anyhow!("REGEXP_REPLACE requires three arguments"));
    };
    if subject == &Value::Null || pattern == &Value::Null || replacement == &Value::Null {
        return Ok(Value::Null);
    }
    let subject = json_scalar_to_string(subject);
    let pattern = json_scalar_to_string(pattern);
    let replacement = json_scalar_to_string(replacement);
    let regex = Regex::new(&pattern).map_err(|error| anyhow!("invalid regular expression: {error}"))?;
    Ok(Value::String(
        regex
            .replace_all(&subject, NoExpand(replacement.as_str()))
            .into_owned(),
    ))
}

pub(super) fn eval_regexp_substr_values(values: &[Value]) -> Result<Value> {
    let [subject, pattern] = values else {
        return Err(anyhow!("REGEXP_SUBSTR requires two arguments"));
    };
    if subject == &Value::Null || pattern == &Value::Null {
        return Ok(Value::Null);
    }
    let subject = json_scalar_to_string(subject);
    let pattern = json_scalar_to_string(pattern);
    let regex = Regex::new(&pattern).map_err(|error| anyhow!("invalid regular expression: {error}"))?;
    Ok(regex
        .find(&subject)
        .map(|matched| Value::String(matched.as_str().to_string()))
        .unwrap_or(Value::Null))
}

pub(super) fn eval_repeat(
    string_arg: Option<&String>,
    count_arg: Option<&String>,
    data: &Map<String, Value>,
    last_insert_id: u64,
) -> Result<Value> {
    let value = eval_arg(string_arg, data, last_insert_id)?;
    let count = eval_arg(count_arg, data, last_insert_id)?;
    if value == Value::Null || count == Value::Null {
        return Ok(Value::Null);
    }
    let count = value_to_i64(&count).unwrap_or(0);
    if count <= 0 {
        return Ok(Value::String(String::new()));
    }
    Ok(Value::String(
        json_scalar_to_string(&value).repeat(count as usize),
    ))
}

pub(super) fn eval_space(
    count_arg: Option<&String>,
    data: &Map<String, Value>,
    last_insert_id: u64,
) -> Result<Value> {
    let count = eval_arg(count_arg, data, last_insert_id)?;
    if count == Value::Null {
        return Ok(Value::Null);
    }
    let count = value_to_i64(&count).unwrap_or(0);
    if count <= 0 {
        return Ok(Value::String(String::new()));
    }
    Ok(Value::String(" ".repeat(count as usize)))
}

pub(super) fn eval_insert_string(
    args: &[String],
    data: &Map<String, Value>,
    last_insert_id: u64,
) -> Result<Value> {
    let values = args
        .iter()
        .map(|arg| eval_scalar_text(arg, data, last_insert_id))
        .collect::<Result<Vec<_>>>()?;
    if values.iter().any(Value::is_null) || values.len() < 4 {
        return Ok(Value::Null);
    }
    let input = json_scalar_to_string(&values[0]);
    let position = json_to_i128_exact(&values[1]).unwrap_or(0);
    let length = json_to_i128_exact(&values[2]).unwrap_or(0);
    let replacement = json_scalar_to_string(&values[3]);
    if position <= 0 || position as usize > input.chars().count() {
        return Ok(Value::String(input));
    }
    let start = position as usize - 1;
    let end = start.saturating_add(length.max(0) as usize);
    let mut output = input.chars().take(start).collect::<String>();
    output.push_str(&replacement);
    output.extend(input.chars().skip(end));
    Ok(Value::String(output))
}

pub(crate) fn mysql_soundex(value: &str) -> String {
    let mut letters = value
        .chars()
        .filter(|character| character.is_ascii_alphabetic());
    let Some(first) = letters.next() else {
        return String::new();
    };
    let mut output = first.to_ascii_uppercase().to_string();
    let mut previous = soundex_code(first);
    for character in letters {
        let code = soundex_code(character);
        if code != '0' && code != previous {
            output.push(code);
        }
        previous = code;
    }
    output
}

pub(super) fn eval_digest(
    arg: Option<&String>,
    data: &Map<String, Value>,
    last_insert_id: u64,
    algorithm: &str,
) -> Result<Value> {
    let value = eval_arg(arg, data, last_insert_id)?;
    if value == Value::Null {
        return Ok(Value::Null);
    }
    let bytes = json_scalar_to_string(&value);
    let hex = match algorithm {
        "MD5" => format_digest(&Md5::digest(bytes.as_bytes())),
        "SHA" | "SHA1" => Sha1::from(bytes).digest().to_string(),
        "SHA256" => format_digest(&Sha256::digest(bytes.as_bytes())),
        _ => return Err(anyhow!("unsupported digest: {algorithm}")),
    };
    Ok(Value::String(hex))
}

fn format_digest(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

pub(super) fn eval_crc32(
    arg: Option<&String>,
    data: &Map<String, Value>,
    last_insert_id: u64,
) -> Result<Value> {
    let value = eval_arg(arg, data, last_insert_id)?;
    if value == Value::Null {
        return Ok(Value::Null);
    }
    let mut hasher = crc32fast::Hasher::new();
    hasher.update(json_scalar_to_string(&value).as_bytes());
    Ok(Value::Number(Number::from(hasher.finalize())))
}

fn soundex_code(character: char) -> char {
    match character.to_ascii_uppercase() {
        'B' | 'F' | 'P' | 'V' => '1',
        'C' | 'G' | 'J' | 'K' | 'Q' | 'S' | 'X' | 'Z' => '2',
        'D' | 'T' => '3',
        'L' => '4',
        'M' | 'N' => '5',
        'R' => '6',
        _ => '0',
    }
}
