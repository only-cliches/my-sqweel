use super::*;

pub(super) fn eval_bare_datetime_keyword(name: &str) -> Option<Value> {
    match name.to_ascii_uppercase().as_str() {
        "NOW" | "CURRENT_TIMESTAMP" | "LOCALTIME" | "LOCALTIMESTAMP" | "UTC_TIMESTAMP" => {
            Some(Value::String(Utc::now().naive_utc().to_string()))
        }
        "CURRENT_DATE" | "CURDATE" | "UTC_DATE" => {
            Some(Value::String(Utc::now().date_naive().to_string()))
        }
        "CURRENT_TIME" | "CURTIME" | "UTC_TIME" => Some(Value::String(format_mysql_naive_time(
            Utc::now().naive_utc().time(),
        ))),
        _ => None,
    }
}

pub(super) fn eval_timestamp_add(
    unit_arg: Option<&String>,
    amount_arg: Option<&String>,
    datetime_arg: Option<&String>,
    data: &Map<String, Value>,
    last_insert_id: u64,
) -> Result<Value> {
    let Some(unit_arg) = unit_arg else {
        return Ok(Value::Null);
    };
    let Some(unit) = parse_mysql_interval_unit(unit_arg) else {
        return Ok(Value::Null);
    };
    let amount = amount_arg
        .map(|arg| eval_scalar_text(arg, data, last_insert_id))
        .transpose()?
        .and_then(|value| value_to_i64(&value))
        .unwrap_or(0);
    let datetime = datetime_arg
        .map(|arg| eval_scalar_text(arg, data, last_insert_id))
        .transpose()?
        .unwrap_or(Value::Null);
    if datetime == Value::Null {
        return Ok(Value::Null);
    }
    let Some(datetime) = parse_mysql_datetime_value(&datetime) else {
        return Ok(Value::Null);
    };
    let interval = MysqlInterval { amount, unit };
    let Some(result) = apply_mysql_interval(datetime, interval, 1) else {
        return Ok(Value::Null);
    };
    Ok(Value::String(result.to_string()))
}

pub(super) fn eval_timestamp_diff(
    unit_arg: Option<&String>,
    start_arg: Option<&String>,
    end_arg: Option<&String>,
    data: &Map<String, Value>,
    last_insert_id: u64,
) -> Result<Value> {
    let Some(unit_arg) = unit_arg else {
        return Ok(Value::Null);
    };
    let Some(unit) = parse_mysql_interval_unit(unit_arg) else {
        return Ok(Value::Null);
    };
    let start = start_arg
        .map(|arg| eval_scalar_text(arg, data, last_insert_id))
        .transpose()?
        .unwrap_or(Value::Null);
    let end = end_arg
        .map(|arg| eval_scalar_text(arg, data, last_insert_id))
        .transpose()?
        .unwrap_or(Value::Null);
    if start == Value::Null || end == Value::Null {
        return Ok(Value::Null);
    }
    let (Some(start), Some(end)) = (
        parse_mysql_datetime_value(&start),
        parse_mysql_datetime_value(&end),
    ) else {
        return Ok(Value::Null);
    };
    let Some(diff) = timestamp_diff(unit, start, end) else {
        return Ok(Value::Null);
    };
    Ok(Value::Number(Number::from(diff)))
}

pub(super) fn eval_date_diff(
    left_arg: Option<&String>,
    right_arg: Option<&String>,
    data: &Map<String, Value>,
    last_insert_id: u64,
) -> Result<Value> {
    let left = left_arg
        .map(|arg| eval_scalar_text(arg, data, last_insert_id))
        .transpose()?
        .unwrap_or(Value::Null);
    let right = right_arg
        .map(|arg| eval_scalar_text(arg, data, last_insert_id))
        .transpose()?
        .unwrap_or(Value::Null);
    if left == Value::Null || right == Value::Null {
        return Ok(Value::Null);
    }
    let (Some(left), Some(right)) = (
        parse_mysql_datetime_value(&left),
        parse_mysql_datetime_value(&right),
    ) else {
        return Ok(Value::Null);
    };
    Ok(Value::Number(Number::from(
        left.date().signed_duration_since(right.date()).num_days(),
    )))
}

pub(super) fn eval_add_sub_time(
    datetime_arg: Option<&String>,
    duration_arg: Option<&String>,
    data: &Map<String, Value>,
    last_insert_id: u64,
    direction: i32,
) -> Result<Value> {
    let datetime_value = datetime_arg
        .map(|arg| eval_scalar_text(arg, data, last_insert_id))
        .transpose()?
        .unwrap_or(Value::Null);
    let duration_value = duration_arg
        .map(|arg| eval_scalar_text(arg, data, last_insert_id))
        .transpose()?
        .unwrap_or(Value::Null);
    if datetime_value == Value::Null || duration_value == Value::Null {
        return Ok(Value::Null);
    }
    let Some(duration) = parse_mysql_time_duration(&duration_value)
        .and_then(|duration| scale_duration(duration, direction))
    else {
        return Ok(Value::Null);
    };

    if let Some(datetime) = parse_mysql_datetime_value(&datetime_value) {
        return Ok(datetime
            .checked_add_signed(duration)
            .map(|datetime| Value::String(datetime.to_string()))
            .unwrap_or(Value::Null));
    }
    if let Some(time) = parse_mysql_time_duration(&datetime_value) {
        return Ok(Value::String(format_mysql_duration(time + duration)));
    }
    Ok(Value::Null)
}

pub(super) fn eval_time_diff(
    left_arg: Option<&String>,
    right_arg: Option<&String>,
    data: &Map<String, Value>,
    last_insert_id: u64,
) -> Result<Value> {
    let left = left_arg
        .map(|arg| eval_scalar_text(arg, data, last_insert_id))
        .transpose()?
        .unwrap_or(Value::Null);
    let right = right_arg
        .map(|arg| eval_scalar_text(arg, data, last_insert_id))
        .transpose()?
        .unwrap_or(Value::Null);
    if left == Value::Null || right == Value::Null {
        return Ok(Value::Null);
    }

    if let (Some(left), Some(right)) = (
        parse_mysql_datetime_value(&left),
        parse_mysql_datetime_value(&right),
    ) {
        return Ok(Value::String(format_mysql_duration(
            left.signed_duration_since(right),
        )));
    }
    if let (Some(left), Some(right)) = (
        parse_mysql_time_duration(&left),
        parse_mysql_time_duration(&right),
    ) {
        return Ok(Value::String(format_mysql_duration(left - right)));
    }
    Ok(Value::Null)
}

pub(super) fn eval_date_part(
    arg: Option<&String>,
    data: &Map<String, Value>,
    last_insert_id: u64,
) -> Result<Value> {
    let value = arg
        .map(|arg| eval_scalar_text(arg, data, last_insert_id))
        .transpose()?
        .unwrap_or(Value::Null);
    if value == Value::Null {
        return Ok(Value::Null);
    }
    let Some(datetime) = parse_mysql_datetime_value(&value) else {
        return Ok(Value::Null);
    };
    Ok(Value::String(datetime.date().to_string()))
}

pub(super) fn eval_time_part(
    arg: Option<&String>,
    data: &Map<String, Value>,
    last_insert_id: u64,
) -> Result<Value> {
    let value = arg
        .map(|arg| eval_scalar_text(arg, data, last_insert_id))
        .transpose()?
        .unwrap_or(Value::Null);
    if value == Value::Null {
        return Ok(Value::Null);
    }
    if let Some(datetime) = parse_mysql_datetime_value(&value) {
        return Ok(Value::String(format_mysql_naive_time(datetime.time())));
    }
    if let Some(duration) = parse_mysql_time_duration(&value) {
        return Ok(Value::String(format_mysql_duration(duration)));
    }
    Ok(Value::Null)
}

pub(super) fn eval_datetime_component(
    arg: Option<&String>,
    data: &Map<String, Value>,
    last_insert_id: u64,
    field: &str,
) -> Result<Value> {
    let value = arg
        .map(|arg| eval_scalar_text(arg, data, last_insert_id))
        .transpose()?
        .unwrap_or(Value::Null);
    eval_extract_datetime_component(field, &value)
}

pub(super) fn eval_extract_datetime_field(field: &DateTimeField, value: Value) -> Result<Value> {
    match field {
        DateTimeField::Year => eval_extract_datetime_component("YEAR", &value),
        DateTimeField::Month => eval_extract_datetime_component("MONTH", &value),
        DateTimeField::Week(_) => eval_extract_datetime_component("WEEK", &value),
        DateTimeField::Day => eval_extract_datetime_component("DAY", &value),
        DateTimeField::DayOfWeek => eval_extract_datetime_component("DAYOFWEEK", &value),
        DateTimeField::DayOfYear | DateTimeField::Doy => {
            eval_extract_datetime_component("DAYOFYEAR", &value)
        }
        DateTimeField::Hour => eval_extract_datetime_component("HOUR", &value),
        DateTimeField::Minute => eval_extract_datetime_component("MINUTE", &value),
        DateTimeField::Second => eval_extract_datetime_component("SECOND", &value),
        DateTimeField::Microsecond | DateTimeField::Microseconds => {
            eval_extract_datetime_component("MICROSECOND", &value)
        }
        DateTimeField::Millisecond | DateTimeField::Milliseconds => {
            eval_extract_datetime_component("MILLISECOND", &value)
        }
        DateTimeField::Quarter => eval_extract_datetime_component("QUARTER", &value),
        DateTimeField::Date => {
            if value == Value::Null {
                return Ok(Value::Null);
            }
            Ok(parse_mysql_datetime_value(&value)
                .map(|datetime| Value::String(datetime.date().to_string()))
                .unwrap_or(Value::Null))
        }
        DateTimeField::Time => {
            if value == Value::Null {
                return Ok(Value::Null);
            }
            Ok(parse_mysql_datetime_value(&value)
                .map(|datetime| Value::String(format_mysql_naive_time(datetime.time())))
                .unwrap_or(Value::Null))
        }
        DateTimeField::Dow => eval_extract_datetime_component("DOW", &value),
        DateTimeField::Isodow => eval_extract_datetime_component("ISODOW", &value),
        DateTimeField::IsoWeek => eval_extract_datetime_component("WEEK", &value),
        DateTimeField::Isoyear => eval_extract_datetime_component("YEAR", &value),
        DateTimeField::Epoch => eval_extract_datetime_component("EPOCH", &value),
        DateTimeField::Custom(ident) => eval_extract_datetime_component(&ident.value, &value),
        _ => Ok(Value::Null),
    }
}

pub(super) fn eval_extract_datetime_component(field: &str, value: &Value) -> Result<Value> {
    if value == &Value::Null {
        return Ok(Value::Null);
    }
    let normalized = normalize_datetime_field(field);
    if let Some(datetime) = parse_mysql_datetime_value(value) {
        return extract_datetime_component(&normalized, datetime);
    }
    if let Some(duration) = parse_mysql_time_duration(value) {
        return extract_duration_component(&normalized, duration);
    }
    Ok(Value::Null)
}

fn extract_datetime_component(field: &str, datetime: NaiveDateTime) -> Result<Value> {
    let date = datetime.date();
    let time = datetime.time();
    let number = match field {
        "YEAR" => i64::from(date.year()),
        "MONTH" => i64::from(date.month()),
        "WEEK" => i64::from(date.iso_week().week()),
        "DAY" | "DAYOFMONTH" => i64::from(date.day()),
        "DAYOFWEEK" => i64::from(date.weekday().num_days_from_sunday() + 1),
        "WEEKDAY" => i64::from(date.weekday().num_days_from_monday()),
        "DAYOFYEAR" => i64::from(date.ordinal()),
        "DOW" => i64::from(date.weekday().num_days_from_sunday()),
        "ISODOW" => i64::from(date.weekday().num_days_from_monday() + 1),
        "QUARTER" => i64::from(((date.month() - 1) / 3) + 1),
        "HOUR" => i64::from(time.hour()),
        "MINUTE" => i64::from(time.minute()),
        "SECOND" => i64::from(time.second()),
        "MICROSECOND" => i64::from(time.nanosecond() / 1_000),
        "MILLISECOND" => i64::from(time.nanosecond() / 1_000_000),
        "EPOCH" => datetime.and_utc().timestamp(),
        _ => return Ok(Value::Null),
    };
    Ok(Value::Number(Number::from(number)))
}

fn extract_duration_component(field: &str, duration: Duration) -> Result<Value> {
    let Some(total_micros) = duration.num_microseconds() else {
        return Ok(Value::Null);
    };
    let sign = if total_micros < 0 { -1 } else { 1 };
    let abs = total_micros.unsigned_abs();
    let total_seconds = abs / 1_000_000;
    let number = match field {
        "HOUR" => sign * (total_seconds / 3_600) as i64,
        "MINUTE" => sign * ((total_seconds / 60) % 60) as i64,
        "SECOND" => sign * (total_seconds % 60) as i64,
        "MICROSECOND" => sign * (abs % 1_000_000) as i64,
        "MILLISECOND" => sign * ((abs % 1_000_000) / 1_000) as i64,
        _ => return Ok(Value::Null),
    };
    Ok(Value::Number(Number::from(number)))
}

#[derive(Debug, Clone, Copy)]
pub(super) enum DateNamePart {
    Day,
    Month,
}

pub(super) fn eval_datetime_name(
    arg: Option<&String>,
    data: &Map<String, Value>,
    last_insert_id: u64,
    part: DateNamePart,
) -> Result<Value> {
    let value = arg
        .map(|arg| eval_scalar_text(arg, data, last_insert_id))
        .transpose()?
        .unwrap_or(Value::Null);
    if value == Value::Null {
        return Ok(Value::Null);
    }
    let Some(datetime) = parse_mysql_datetime_value(&value) else {
        return Ok(Value::Null);
    };
    let name = match part {
        DateNamePart::Day => WEEKDAY_NAMES[datetime.weekday().num_days_from_sunday() as usize],
        DateNamePart::Month => MONTH_NAMES[(datetime.month() - 1) as usize],
    };
    Ok(Value::String(name.to_string()))
}

pub(super) fn eval_date_format(
    date_arg: Option<&String>,
    format_arg: Option<&String>,
    data: &Map<String, Value>,
    last_insert_id: u64,
) -> Result<Value> {
    let date = date_arg
        .map(|arg| eval_scalar_text(arg, data, last_insert_id))
        .transpose()?
        .unwrap_or(Value::Null);
    let format = format_arg
        .map(|arg| eval_scalar_text(arg, data, last_insert_id))
        .transpose()?
        .unwrap_or(Value::Null);
    if date == Value::Null || format == Value::Null {
        return Ok(Value::Null);
    }
    let Some(datetime) = parse_mysql_datetime_value(&date) else {
        return Ok(Value::Null);
    };
    Ok(Value::String(format_mysql_datetime(
        datetime,
        &json_scalar_to_string(&format),
    )))
}

pub(super) fn eval_date_add_sub(
    date_arg: Option<&String>,
    interval_arg: Option<&String>,
    data: &Map<String, Value>,
    last_insert_id: u64,
    direction: i32,
) -> Result<Value> {
    let date_value = date_arg
        .map(|arg| eval_scalar_text(arg, data, last_insert_id))
        .transpose()?
        .unwrap_or(Value::Null);
    if date_value == Value::Null {
        return Ok(Value::Null);
    }
    let Some(interval_arg) = interval_arg else {
        return Ok(Value::Null);
    };
    let Some(interval) = parse_mysql_interval(interval_arg) else {
        return Ok(Value::Null);
    };
    let Some(date) = parse_mysql_datetime_value(&date_value) else {
        return Ok(Value::Null);
    };
    let Some(result) = apply_mysql_interval(date, interval, direction) else {
        return Ok(Value::Null);
    };
    Ok(Value::String(result.to_string()))
}

#[derive(Debug, Clone, Copy)]
pub(super) struct MysqlInterval {
    amount: i64,
    unit: MysqlIntervalUnit,
}

#[derive(Debug, Clone, Copy)]
pub(super) enum MysqlIntervalUnit {
    Microsecond,
    Second,
    Minute,
    Hour,
    Day,
    Week,
    Month,
    Quarter,
    Year,
}

pub(super) fn parse_mysql_interval(raw: &str) -> Option<MysqlInterval> {
    let trimmed = raw.trim();
    let body = strip_ascii_prefix(trimmed, "INTERVAL")?.trim();
    let (amount_text, unit_text) = split_interval_amount_and_unit(body)?;
    let amount = amount_text
        .trim()
        .trim_matches('\'')
        .trim_matches('"')
        .parse::<i64>()
        .ok()?;
    let unit = parse_mysql_interval_unit(unit_text.trim())?;
    Some(MysqlInterval { amount, unit })
}

fn strip_ascii_prefix<'a>(text: &'a str, prefix: &str) -> Option<&'a str> {
    if text.len() < prefix.len() || !text[..prefix.len()].eq_ignore_ascii_case(prefix) {
        return None;
    }
    Some(&text[prefix.len()..])
}

fn split_interval_amount_and_unit(text: &str) -> Option<(&str, &str)> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }
    let mut in_single = false;
    let mut in_double = false;
    for (idx, ch) in trimmed.char_indices() {
        match ch {
            '\'' if !in_double => in_single = !in_single,
            '"' if !in_single => in_double = !in_double,
            ch if ch.is_whitespace() && !in_single && !in_double => {
                let amount = trimmed[..idx].trim();
                let unit = trimmed[idx..].trim();
                if !amount.is_empty() && !unit.is_empty() {
                    return Some((amount, unit));
                }
            }
            _ => {}
        }
    }
    None
}

fn parse_mysql_interval_unit(unit: &str) -> Option<MysqlIntervalUnit> {
    let normalized = normalize_datetime_field(unit);
    let normalized = normalized
        .strip_prefix("SQL_TSI_")
        .unwrap_or(&normalized)
        .to_string();
    let normalized = normalized.strip_suffix('S').unwrap_or(&normalized);
    match normalized {
        "FRAC_SECOND" => Some(MysqlIntervalUnit::Microsecond),
        "MICROSECOND" => Some(MysqlIntervalUnit::Microsecond),
        "SECOND" => Some(MysqlIntervalUnit::Second),
        "MINUTE" => Some(MysqlIntervalUnit::Minute),
        "HOUR" => Some(MysqlIntervalUnit::Hour),
        "DAY" => Some(MysqlIntervalUnit::Day),
        "WEEK" => Some(MysqlIntervalUnit::Week),
        "MONTH" => Some(MysqlIntervalUnit::Month),
        "QUARTER" => Some(MysqlIntervalUnit::Quarter),
        "YEAR" => Some(MysqlIntervalUnit::Year),
        _ => None,
    }
}

pub(super) fn parse_mysql_datetime_value(value: &Value) -> Option<NaiveDateTime> {
    let raw = json_scalar_to_string(value);
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Ok(datetime) = DateTime::parse_from_rfc3339(trimmed) {
        return Some(datetime.naive_utc());
    }
    for format in [
        "%Y-%m-%d %H:%M:%S%.f",
        "%Y-%m-%dT%H:%M:%S%.f",
        "%Y-%m-%d %H:%M:%S",
        "%Y-%m-%dT%H:%M:%S",
        "%Y-%m-%d %H:%M",
        "%Y-%m-%dT%H:%M",
    ] {
        if let Ok(datetime) = NaiveDateTime::parse_from_str(trimmed, format) {
            return Some(datetime);
        }
    }
    NaiveDate::parse_from_str(trimmed, "%Y-%m-%d")
        .ok()
        .and_then(|date| date.and_hms_opt(0, 0, 0))
}

fn apply_mysql_interval(
    date: NaiveDateTime,
    interval: MysqlInterval,
    direction: i32,
) -> Option<NaiveDateTime> {
    let amount = interval.amount.checked_mul(direction as i64)?;
    match interval.unit {
        MysqlIntervalUnit::Microsecond => date.checked_add_signed(Duration::microseconds(amount)),
        MysqlIntervalUnit::Second => date.checked_add_signed(Duration::seconds(amount)),
        MysqlIntervalUnit::Minute => date.checked_add_signed(Duration::minutes(amount)),
        MysqlIntervalUnit::Hour => date.checked_add_signed(Duration::hours(amount)),
        MysqlIntervalUnit::Day => date.checked_add_signed(Duration::days(amount)),
        MysqlIntervalUnit::Week => date.checked_add_signed(Duration::weeks(amount)),
        MysqlIntervalUnit::Month => apply_month_interval(date, amount),
        MysqlIntervalUnit::Quarter => apply_month_interval(date, amount.checked_mul(3)?),
        MysqlIntervalUnit::Year => apply_month_interval(date, amount.checked_mul(12)?),
    }
}

fn apply_month_interval(date: NaiveDateTime, months: i64) -> Option<NaiveDateTime> {
    let months_abs = Months::new(months.unsigned_abs().try_into().ok()?);
    if months >= 0 {
        date.checked_add_months(months_abs)
    } else {
        date.checked_sub_months(months_abs)
    }
}

fn timestamp_diff(
    unit: MysqlIntervalUnit,
    start: NaiveDateTime,
    end: NaiveDateTime,
) -> Option<i64> {
    match unit {
        MysqlIntervalUnit::Microsecond => end.signed_duration_since(start).num_microseconds(),
        MysqlIntervalUnit::Second => Some(end.signed_duration_since(start).num_seconds()),
        MysqlIntervalUnit::Minute => Some(end.signed_duration_since(start).num_minutes()),
        MysqlIntervalUnit::Hour => Some(end.signed_duration_since(start).num_hours()),
        MysqlIntervalUnit::Day => Some(end.signed_duration_since(start).num_days()),
        MysqlIntervalUnit::Week => Some(end.signed_duration_since(start).num_weeks()),
        MysqlIntervalUnit::Month => Some(complete_months_between(start, end)),
        MysqlIntervalUnit::Quarter => Some(complete_months_between(start, end) / 3),
        MysqlIntervalUnit::Year => Some(complete_months_between(start, end) / 12),
    }
}

fn complete_months_between(start: NaiveDateTime, end: NaiveDateTime) -> i64 {
    if end < start {
        return -complete_months_between(end, start);
    }

    let mut months = i64::from(end.year() - start.year()) * 12
        + i64::from(end.month() as i32 - start.month() as i32);
    if months > 0
        && (end.day() < start.day() || (end.day() == start.day() && end.time() < start.time()))
    {
        months -= 1;
    }
    months
}

pub(super) fn parse_mysql_time_duration(value: &Value) -> Option<Duration> {
    let raw = json_scalar_to_string(value);
    let mut trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let sign = if let Some(rest) = trimmed.strip_prefix('-') {
        trimmed = rest.trim_start();
        -1_i64
    } else if let Some(rest) = trimmed.strip_prefix('+') {
        trimmed = rest.trim_start();
        1_i64
    } else {
        1_i64
    };

    if let Ok(time) = NaiveTime::parse_from_str(trimmed, "%H:%M:%S%.f")
        .or_else(|_| NaiveTime::parse_from_str(trimmed, "%H:%M:%S"))
        .or_else(|_| NaiveTime::parse_from_str(trimmed, "%H:%M"))
    {
        return duration_from_time_parts(
            0,
            i64::from(time.hour()),
            i64::from(time.minute()),
            i64::from(time.second()),
            i64::from(time.nanosecond() / 1_000),
            sign,
        );
    }

    let mut days = 0_i64;
    let mut time_part = trimmed;
    if let Some((day_part, rest)) = trimmed.split_once(char::is_whitespace) {
        days = day_part.parse::<i64>().ok()?;
        time_part = rest.trim();
    }
    let pieces = time_part.split(':').collect::<Vec<_>>();
    let (hours, minutes, seconds, micros) = match pieces.as_slice() {
        [hours, minutes, seconds] => {
            let (seconds, micros) = parse_seconds_and_micros(seconds)?;
            (hours.parse().ok()?, minutes.parse().ok()?, seconds, micros)
        }
        [hours, minutes] => (hours.parse().ok()?, minutes.parse().ok()?, 0, 0),
        [seconds] => {
            let (seconds, micros) = parse_seconds_and_micros(seconds)?;
            (0, 0, seconds, micros)
        }
        _ => return None,
    };
    duration_from_time_parts(days, hours, minutes, seconds, micros, sign)
}

fn parse_seconds_and_micros(raw: &str) -> Option<(i64, i64)> {
    let trimmed = raw.trim();
    if let Some((seconds, fraction)) = trimmed.split_once('.') {
        let seconds = seconds.parse::<i64>().ok()?;
        let mut micros = fraction
            .chars()
            .take(6)
            .collect::<String>()
            .parse::<i64>()
            .unwrap_or(0);
        for _ in fraction.chars().take(6).count()..6 {
            micros *= 10;
        }
        Some((seconds, micros))
    } else {
        Some((trimmed.parse().ok()?, 0))
    }
}

fn duration_from_time_parts(
    days: i64,
    hours: i64,
    minutes: i64,
    seconds: i64,
    micros: i64,
    sign: i64,
) -> Option<Duration> {
    let total_micros = days
        .checked_mul(86_400_000_000)?
        .checked_add(hours.checked_mul(3_600_000_000)?)?
        .checked_add(minutes.checked_mul(60_000_000)?)?
        .checked_add(seconds.checked_mul(1_000_000)?)?
        .checked_add(micros)?
        .checked_mul(sign)?;
    Some(Duration::microseconds(total_micros))
}

fn scale_duration(duration: Duration, direction: i32) -> Option<Duration> {
    duration
        .num_microseconds()?
        .checked_mul(i64::from(direction))
        .map(Duration::microseconds)
}

pub(super) fn format_mysql_naive_time(time: NaiveTime) -> String {
    let micros = time.nanosecond() / 1_000;
    if micros == 0 {
        format!(
            "{:02}:{:02}:{:02}",
            time.hour(),
            time.minute(),
            time.second()
        )
    } else {
        format!(
            "{:02}:{:02}:{:02}.{:06}",
            time.hour(),
            time.minute(),
            time.second(),
            micros
        )
    }
}

pub(super) fn format_mysql_duration(duration: Duration) -> String {
    let Some(total_micros) = duration.num_microseconds() else {
        return "00:00:00".to_string();
    };
    let sign = if total_micros < 0 { "-" } else { "" };
    let abs = total_micros.unsigned_abs();
    let total_seconds = abs / 1_000_000;
    let micros = abs % 1_000_000;
    let hours = total_seconds / 3_600;
    let minutes = (total_seconds / 60) % 60;
    let seconds = total_seconds % 60;
    if micros == 0 {
        format!("{sign}{hours:02}:{minutes:02}:{seconds:02}")
    } else {
        format!("{sign}{hours:02}:{minutes:02}:{seconds:02}.{micros:06}")
    }
}

fn normalize_datetime_field(field: &str) -> String {
    field
        .trim()
        .trim_matches('`')
        .trim_matches('\'')
        .trim_matches('"')
        .to_ascii_uppercase()
}

const WEEKDAY_NAMES: [&str; 7] = [
    "Sunday",
    "Monday",
    "Tuesday",
    "Wednesday",
    "Thursday",
    "Friday",
    "Saturday",
];

const WEEKDAY_ABBREVIATIONS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];

const MONTH_NAMES: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];

const MONTH_ABBREVIATIONS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

fn format_mysql_datetime(datetime: NaiveDateTime, format: &str) -> String {
    let mut out = String::new();
    let mut chars = format.chars();
    while let Some(ch) = chars.next() {
        if ch != '%' {
            out.push(ch);
            continue;
        }
        let Some(token) = chars.next() else {
            out.push('%');
            break;
        };
        match token {
            '%' => out.push('%'),
            'Y' => out.push_str(&format!("{:04}", datetime.year())),
            'y' => out.push_str(&format!("{:02}", datetime.year().rem_euclid(100))),
            'm' => out.push_str(&format!("{:02}", datetime.month())),
            'c' => out.push_str(&datetime.month().to_string()),
            'M' => out.push_str(MONTH_NAMES[(datetime.month() - 1) as usize]),
            'b' => out.push_str(MONTH_ABBREVIATIONS[(datetime.month() - 1) as usize]),
            'd' => out.push_str(&format!("{:02}", datetime.day())),
            'e' => out.push_str(&datetime.day().to_string()),
            'D' => out.push_str(&format!(
                "{}{}",
                datetime.day(),
                ordinal_suffix(datetime.day())
            )),
            'j' => out.push_str(&format!("{:03}", datetime.ordinal())),
            'H' => out.push_str(&format!("{:02}", datetime.hour())),
            'k' => out.push_str(&datetime.hour().to_string()),
            'h' | 'I' => out.push_str(&format!("{:02}", hour_12(datetime.hour()))),
            'l' => out.push_str(&hour_12(datetime.hour()).to_string()),
            'i' => out.push_str(&format!("{:02}", datetime.minute())),
            's' | 'S' => out.push_str(&format!("{:02}", datetime.second())),
            'f' => out.push_str(&format!("{:06}", datetime.nanosecond() / 1_000)),
            'p' => out.push_str(if datetime.hour() < 12 { "AM" } else { "PM" }),
            'T' => out.push_str(&format!(
                "{:02}:{:02}:{:02}",
                datetime.hour(),
                datetime.minute(),
                datetime.second()
            )),
            'r' => out.push_str(&format!(
                "{:02}:{:02}:{:02} {}",
                hour_12(datetime.hour()),
                datetime.minute(),
                datetime.second(),
                if datetime.hour() < 12 { "AM" } else { "PM" }
            )),
            'W' => out.push_str(WEEKDAY_NAMES[datetime.weekday().num_days_from_sunday() as usize]),
            'a' => out.push_str(
                WEEKDAY_ABBREVIATIONS[datetime.weekday().num_days_from_sunday() as usize],
            ),
            'w' => out.push_str(&datetime.weekday().num_days_from_sunday().to_string()),
            unknown => {
                out.push('%');
                out.push(unknown);
            }
        }
    }
    out
}

fn hour_12(hour: u32) -> u32 {
    let hour = hour % 12;
    if hour == 0 { 12 } else { hour }
}

fn ordinal_suffix(day: u32) -> &'static str {
    if (11..=13).contains(&(day % 100)) {
        return "th";
    }
    match day % 10 {
        1 => "st",
        2 => "nd",
        3 => "rd",
        _ => "th",
    }
}
