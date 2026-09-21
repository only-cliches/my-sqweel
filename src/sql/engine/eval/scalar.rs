use super::*;
use base64::Engine;
use md5::{Digest, Md5};
use regex::{NoExpand, Regex};
use sha1_smol::Sha1;
use sha2::{Sha224, Sha256, Sha384, Sha512};

pub(super) fn eval_to_base64_value(value: Value) -> Result<Value> {
    if value == Value::Null {
        return Ok(Value::Null);
    }
    let bytes =
        binary_value_bytes(&value).unwrap_or_else(|| json_scalar_to_string(&value).into_bytes());
    Ok(Value::String(
        base64::engine::general_purpose::STANDARD.encode(bytes),
    ))
}

pub(super) fn eval_from_base64_value(value: Value) -> Result<Value> {
    if value == Value::Null {
        return Ok(Value::Null);
    }
    let encoded = json_scalar_to_string(&value);
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded.as_bytes())
        .map_err(|error| anyhow!("invalid base64 data: {error}"))?;
    let hex = bytes
        .iter()
        .map(|byte| format!("{byte:02X}"))
        .collect::<String>();
    Ok(Value::String(format!("{MYSQL_BINARY_SENTINEL}{hex}")))
}

fn binary_value_bytes(value: &Value) -> Option<Vec<u8>> {
    let hex = value.as_str()?.strip_prefix(MYSQL_BINARY_SENTINEL)?;
    if hex.len() % 2 != 0 {
        return None;
    }
    hex.as_bytes()
        .chunks_exact(2)
        .map(|pair| {
            let text = std::str::from_utf8(pair).ok()?;
            u8::from_str_radix(text, 16).ok()
        })
        .collect()
}
fn binary_value_length(value: &Value) -> Option<usize> {
    let hex = value.as_str()?.strip_prefix(MYSQL_BINARY_SENTINEL)?;
    (hex.len() % 2 == 0).then_some(hex.len() / 2)
}

pub(super) fn eval_octet_length_value(value: &Value) -> usize {
    binary_value_length(value).unwrap_or_else(|| json_scalar_to_string(value).len())
}

pub(super) fn eval_quote_value(value: Value) -> Value {
    if value == Value::Null {
        return Value::String("NULL".to_string());
    }
    let input = json_scalar_to_string(&value);
    let mut quoted = String::with_capacity(input.len() + 2);
    quoted.push('\'');
    for character in input.chars() {
        match character {
            '\0' => quoted.push_str("\\0"),
            '\n' => quoted.push_str("\\n"),
            '\r' => quoted.push_str("\\r"),
            '\\' => quoted.push_str("\\\\"),
            '\'' => quoted.push_str("\\'"),
            '"' => quoted.push_str("\\\""),
            '\u{001A}' => quoted.push_str("\\Z"),
            character => quoted.push(character),
        }
    }
    quoted.push('\'');
    Value::String(quoted)
}

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

pub(super) fn eval_conv_values(values: &[Value]) -> Result<Value> {
    if values.len() < 3 {
        return Err(anyhow!("CONV requires three arguments"));
    }
    if values[..3].iter().any(|value| *value == Value::Null) {
        return Ok(Value::Null);
    }

    let input = json_scalar_to_string(&values[0]);
    let Some(from_base) = value_to_i64(&values[1]) else {
        return Ok(Value::Null);
    };
    let Some(to_base) = value_to_i64(&values[2]) else {
        return Ok(Value::Null);
    };
    let output_base = to_base.unsigned_abs();
    if !(2..=36).contains(&(from_base as u64)) || from_base < 2 || !(2..=36).contains(&output_base)
    {
        return Ok(Value::Null);
    }

    let mut digits = input.trim();
    let negative = digits.strip_prefix('-').is_some();
    if negative {
        digits = &digits[1..];
    } else if let Some(unsigned) = digits.strip_prefix('+') {
        digits = unsigned;
    }
    let mut magnitude = 0_u64;
    let mut parsed = false;
    for character in digits.chars() {
        let digit = match character {
            '0'..='9' => character as u32 - '0' as u32,
            'a'..='z' => character as u32 - 'a' as u32 + 10,
            'A'..='Z' => character as u32 - 'A' as u32 + 10,
            _ => break,
        };
        if digit >= from_base as u32 {
            break;
        }
        parsed = true;
        magnitude = magnitude
            .saturating_mul(from_base as u64)
            .saturating_add(digit as u64);
    }
    if !parsed {
        magnitude = 0;
    }

    if to_base < 0 {
        let signed = if negative {
            -(magnitude as i128)
        } else if magnitude <= i64::MAX as u64 {
            magnitude as i128
        } else {
            magnitude as i128 - (1_i128 << 64)
        };
        Ok(format_signed_radix(signed, output_base))
    } else {
        let unsigned = if negative {
            0_u64.wrapping_sub(magnitude)
        } else {
            magnitude
        };
        Ok(Value::String(format_unsigned_radix(unsigned, output_base)))
    }
}

fn format_signed_radix(value: i128, base: u64) -> Value {
    if value < 0 {
        Value::String(format!(
            "-{}",
            format_unsigned_radix(value.unsigned_abs() as u64, base)
        ))
    } else {
        Value::String(format_unsigned_radix(value as u64, base))
    }
}

fn format_unsigned_radix(mut value: u64, base: u64) -> String {
    const DIGITS: &[u8] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ";
    if value == 0 {
        return "0".to_string();
    }
    let mut output = Vec::new();
    while value > 0 {
        output.push(DIGITS[(value % base) as usize] as char);
        value /= base;
    }
    output.into_iter().rev().collect()
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

pub(super) fn eval_inet_ntoa(
    value_arg: Option<&String>,
    data: &Map<String, Value>,
    last_insert_id: u64,
) -> Result<Value> {
    eval_inet_ntoa_value(eval_arg(value_arg, data, last_insert_id)?)
}

pub(super) fn eval_inet_ntoa_value(value: Value) -> Result<Value> {
    if value == Value::Null {
        return Ok(Value::Null);
    }
    let Some(address) =
        value_to_i64(&value).filter(|address| (0..=u32::MAX as i64).contains(address))
    else {
        return Ok(Value::Null);
    };
    let address = address as u32;
    Ok(Value::String(format!(
        "{}.{}.{}.{}",
        address >> 24,
        (address >> 16) & 0xff,
        (address >> 8) & 0xff,
        address & 0xff
    )))
}

pub(super) fn eval_inet6_aton(
    value_arg: Option<&String>,
    data: &Map<String, Value>,
    last_insert_id: u64,
) -> Result<Value> {
    eval_inet6_aton_value(eval_arg(value_arg, data, last_insert_id)?)
}

pub(super) fn eval_inet6_aton_value(value: Value) -> Result<Value> {
    if value == Value::Null {
        return Ok(Value::Null);
    }
    let text = json_scalar_to_string(&value);
    let Ok(address) = text.parse::<std::net::IpAddr>() else {
        return Ok(Value::Null);
    };
    let bytes = match address {
        std::net::IpAddr::V4(address) => address.octets().to_vec(),
        std::net::IpAddr::V6(address) => address.octets().to_vec(),
    };
    let hex = bytes
        .iter()
        .map(|byte| format!("{byte:02X}"))
        .collect::<String>();
    Ok(Value::String(format!("{MYSQL_BINARY_SENTINEL}{hex}")))
}

pub(super) fn eval_inet6_ntoa(
    value_arg: Option<&String>,
    data: &Map<String, Value>,
    last_insert_id: u64,
) -> Result<Value> {
    eval_inet6_ntoa_value(eval_arg(value_arg, data, last_insert_id)?)
}

pub(super) fn eval_inet6_ntoa_value(value: Value) -> Result<Value> {
    if value == Value::Null {
        return Ok(Value::Null);
    }
    let Some(bytes) = binary_value_bytes(&value) else {
        return Ok(Value::Null);
    };
    let Ok(bytes) = <[u8; 16]>::try_from(bytes.as_slice()) else {
        return Ok(Value::Null);
    };
    Ok(Value::String(std::net::Ipv6Addr::from(bytes).to_string()))
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
        if index >= u64::BITS as usize || bits & (1_u64 << index) == 0 || value == Value::Null {
            continue;
        }
        if !out.is_empty() {
            out.push(',');
        }
        out.push_str(&json_scalar_to_string(&value));
    }
    Ok(Value::String(out))
}
pub(super) fn eval_export_set_values(values: &[Value]) -> Result<Value> {
    let Some(bits) = values.first() else {
        return Err(anyhow!("EXPORT_SET requires at least three arguments"));
    };
    let Some(on) = values.get(1) else {
        return Err(anyhow!("EXPORT_SET requires at least three arguments"));
    };
    let Some(off) = values.get(2) else {
        return Err(anyhow!("EXPORT_SET requires at least three arguments"));
    };
    if bits == &Value::Null || on == &Value::Null || off == &Value::Null {
        return Ok(Value::Null);
    }
    let bits = value_to_i64(bits).unwrap_or(0) as u64;
    let on = json_scalar_to_string(on);
    let off = json_scalar_to_string(off);
    let separator = values
        .get(3)
        .and_then(|value| (value != &Value::Null).then(|| json_scalar_to_string(value)))
        .unwrap_or_else(|| ",".to_string());
    let width = values
        .get(4)
        .filter(|value| *value != &Value::Null)
        .and_then(|value| value_to_i64(value))
        .unwrap_or(64)
        .clamp(0, 64) as usize;
    Ok(Value::String(
        (0..width)
            .map(|index| {
                if bits & (1_u64 << index) != 0 {
                    on.as_str()
                } else {
                    off.as_str()
                }
            })
            .collect::<Vec<_>>()
            .join(&separator),
    ))
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
    let regex =
        Regex::new(&pattern).map_err(|error| anyhow!("invalid regular expression: {error}"))?;
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
    let regex =
        Regex::new(&pattern).map_err(|error| anyhow!("invalid regular expression: {error}"))?;
    Ok(regex
        .find(&subject)
        .map(|matched| Value::String(matched.as_str().to_string()))
        .unwrap_or(Value::Null))
}
pub(super) fn eval_regexp_instr_values(values: &[Value]) -> Result<Value> {
    let [subject, pattern] = values else {
        return Err(anyhow!("REGEXP_INSTR requires two arguments"));
    };
    if subject == &Value::Null || pattern == &Value::Null {
        return Ok(Value::Null);
    }
    let subject = json_scalar_to_string(subject);
    let pattern = json_scalar_to_string(pattern);
    let regex =
        Regex::new(&pattern).map_err(|error| anyhow!("invalid regular expression: {error}"))?;
    Ok(regex
        .find(&subject)
        .map(|matched| {
            Value::Number(Number::from(
                subject[..matched.start()].chars().count() as u64 + 1,
            ))
        })
        .unwrap_or_else(|| Value::Number(Number::from(0))))
}

pub(super) fn eval_regexp_values(target: Value, pattern: Value, negated: bool) -> Result<Value> {
    if target == Value::Null || pattern == Value::Null {
        return Ok(Value::Null);
    }
    let target = json_scalar_to_string(&target);
    let pattern = json_scalar_to_string(&pattern);
    let regex =
        Regex::new(&pattern).map_err(|error| anyhow!("invalid regular expression: {error}"))?;
    let matched = regex.is_match(&target);
    Ok(Value::Bool(if negated { !matched } else { matched }))
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
        if code != '0' {
            if code != previous {
                output.push(code);
            }
            previous = code;
        }
    }
    while output.chars().count() < 4 {
        output.push('0');
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

pub(super) fn eval_sha2(
    args: &[String],
    data: &Map<String, Value>,
    last_insert_id: u64,
) -> Result<Value> {
    let value = eval_arg(args.first(), data, last_insert_id)?;
    let hash_length = args
        .get(1)
        .map(|arg| eval_arg(Some(arg), data, last_insert_id))
        .transpose()?
        .unwrap_or(Value::Null);
    if value == Value::Null || hash_length == Value::Null {
        return Ok(Value::Null);
    }
    let Some(hash_length) = value_to_i64(&hash_length) else {
        return Ok(Value::Null);
    };
    let bytes = json_scalar_to_string(&value);
    let hex = match hash_length {
        224 => format_digest(&Sha224::digest(bytes.as_bytes())),
        256 => format_digest(&Sha256::digest(bytes.as_bytes())),
        384 => format_digest(&Sha384::digest(bytes.as_bytes())),
        512 => format_digest(&Sha512::digest(bytes.as_bytes())),
        _ => return Ok(Value::Null),
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
