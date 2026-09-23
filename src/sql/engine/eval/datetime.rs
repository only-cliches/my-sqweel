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

pub(super) fn eval_convert_tz(
    datetime_arg: Option<&String>,
    from_tz_arg: Option<&String>,
    to_tz_arg: Option<&String>,
    data: &Map<String, Value>,
    last_insert_id: u64,
) -> Result<Value> {
    let datetime_is_expression = datetime_arg.is_some_and(|arg| {
        let trimmed = arg.trim();
        !(trimmed.len() >= 2
            && ((trimmed.starts_with('\'') && trimmed.ends_with('\''))
                || (trimmed.starts_with('"') && trimmed.ends_with('"'))))
    });
    let datetime = datetime_arg
        .map(|arg| eval_scalar_text(arg, data, last_insert_id))
        .transpose()?
        .unwrap_or(Value::Null);
    let from_tz = from_tz_arg
        .map(|arg| eval_scalar_text(arg, data, last_insert_id))
        .transpose()?
        .unwrap_or(Value::Null);
    let to_tz = to_tz_arg
        .map(|arg| eval_scalar_text(arg, data, last_insert_id))
        .transpose()?
        .unwrap_or(Value::Null);
    if datetime == Value::Null || from_tz == Value::Null || to_tz == Value::Null {
        return Ok(Value::Null);
    }
    let (Some(datetime), Some(from_offset), Some(to_offset)) = (
        parse_mysql_datetime_value(&datetime),
        parse_timezone_offset(&from_tz),
        parse_timezone_offset(&to_tz),
    ) else {
        return Ok(Value::Null);
    };
    let Some(offset) = to_offset.checked_sub(from_offset) else {
        return Ok(Value::Null);
    };
    Ok(datetime
        .checked_add_signed(Duration::seconds(offset))
        .map(|value| {
            let text = if datetime_is_expression {
                value.format("%Y-%m-%d %H:%M:%S%.6f").to_string()
            } else {
                value.to_string()
            };
            Value::String(text)
        })
        .unwrap_or(Value::Null))
}

fn parse_timezone_offset(value: &Value) -> Option<i64> {
    let timezone = json_scalar_to_string(value);
    let timezone = timezone.trim();
    if timezone.eq_ignore_ascii_case("UTC") || timezone.eq_ignore_ascii_case("Z") {
        return Some(0);
    }
    let bytes = timezone.as_bytes();
    if bytes.len() != 6 || (bytes[0] != b'+' && bytes[0] != b'-') || bytes[3] != b':' {
        return None;
    }
    let hours = timezone[1..3].parse::<i64>().ok()?;
    let minutes = timezone[4..6].parse::<i64>().ok()?;
    if hours > 14 || minutes >= 60 || (hours == 14 && minutes != 0) {
        return None;
    }
    let seconds = hours
        .checked_mul(3_600)?
        .checked_add(minutes.checked_mul(60)?)?;
    Some(if bytes[0] == b'-' { -seconds } else { seconds })
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

pub(super) fn eval_to_days(
    value_arg: Option<&String>,
    data: &Map<String, Value>,
    last_insert_id: u64,
) -> Result<Value> {
    let value = value_arg
        .map(|arg| eval_scalar_text(arg, data, last_insert_id))
        .transpose()?
        .unwrap_or(Value::Null);
    if value == Value::Null {
        return Ok(Value::Null);
    }
    let Some(date) = parse_mysql_datetime_value(&value).map(|datetime| datetime.date()) else {
        return Ok(Value::Null);
    };
    let epoch = NaiveDate::from_ymd_opt(1, 1, 1).expect("year one is a valid date");
    Ok(Value::Number(Number::from(
        date.signed_duration_since(epoch).num_days() + 366,
    )))
}
pub(super) fn eval_add_months(
    date_arg: Option<&String>,
    months_arg: Option<&String>,
    data: &Map<String, Value>,
    last_insert_id: u64,
) -> Result<Value> {
    let value = date_arg
        .map(|arg| eval_scalar_text(arg, data, last_insert_id))
        .transpose()?
        .unwrap_or(Value::Null);
    let months = months_arg
        .map(|arg| eval_scalar_text(arg, data, last_insert_id))
        .transpose()?
        .and_then(|value| value_to_i64(&value))
        .unwrap_or(0);
    if value == Value::Null {
        return Ok(Value::Null);
    }
    let Some(datetime) = parse_mysql_datetime_value(&value) else {
        return Ok(Value::Null);
    };
    let month_index = i64::from(datetime.year()) * 12 + i64::from(datetime.month0()) + months;
    let year = month_index.div_euclid(12);
    let month = month_index.rem_euclid(12) as u32 + 1;
    let day = datetime.day().min(days_in_month(year, month));
    let preserve_time = json_scalar_to_string(&value).contains(':');
    Ok(NaiveDate::from_ymd_opt(year as i32, month, day)
        .map(|date| date.and_time(datetime.time()))
        .map(|value| {
            if preserve_time {
                Value::String(value.to_string())
            } else {
                Value::String(value.date().to_string())
            }
        })
        .unwrap_or(Value::Null))
}

fn days_in_month(year: i64, month: u32) -> u32 {
    let next = if month == 12 {
        NaiveDate::from_ymd_opt((year + 1) as i32, 1, 1)
    } else {
        NaiveDate::from_ymd_opt(year as i32, month + 1, 1)
    };
    next.and_then(|date| date.pred_opt())
        .map(|date| date.day())
        .unwrap_or(0)
}

pub(super) fn eval_to_seconds(
    date_arg: Option<&String>,
    data: &Map<String, Value>,
    last_insert_id: u64,
) -> Result<Value> {
    let value = date_arg
        .map(|arg| eval_scalar_text(arg, data, last_insert_id))
        .transpose()?
        .unwrap_or(Value::Null);
    if value == Value::Null {
        return Ok(Value::Null);
    }
    let Some(datetime) = parse_mysql_datetime_value(&value) else {
        return Ok(Value::Null);
    };
    let days = i64::from(datetime.date().num_days_from_ce()) + 365;
    let time = i64::from(datetime.time().num_seconds_from_midnight());
    Ok(Value::Number(Number::from(days * 86_400 + time)))
}

pub(super) fn eval_week(
    date_arg: Option<&String>,
    mode_arg: Option<&String>,
    data: &Map<String, Value>,
    last_insert_id: u64,
) -> Result<Value> {
    let value = date_arg
        .map(|arg| eval_scalar_text(arg, data, last_insert_id))
        .transpose()?
        .unwrap_or(Value::Null);
    if value == Value::Null {
        return Ok(Value::Null);
    }
    let mode = mode_arg
        .map(|arg| eval_scalar_text(arg, data, last_insert_id))
        .transpose()?
        .and_then(|value| value_to_i64(&value))
        .unwrap_or(0);
    let Some(datetime) = parse_mysql_datetime_value(&value) else {
        return Ok(Value::Null);
    };
    let week = if mode == 3 {
        datetime.date().iso_week().week()
    } else {
        datetime.date().iso_week().week()
    };
    Ok(Value::Number(Number::from(week)))
}

pub(super) fn eval_to_char(
    value_arg: Option<&String>,
    format_arg: Option<&String>,
    data: &Map<String, Value>,
    last_insert_id: u64,
) -> Result<Value> {
    let value = value_arg
        .map(|arg| eval_scalar_text(arg, data, last_insert_id))
        .transpose()?
        .unwrap_or(Value::Null);
    let format = format_arg
        .map(|arg| eval_scalar_text(arg, data, last_insert_id))
        .transpose()?
        .unwrap_or(Value::String("YYYY-MM-DD".to_string()));
    if value == Value::Null || format == Value::Null {
        return Ok(Value::Null);
    }
    let Some(datetime) = parse_mysql_datetime_value(&value) else {
        return Ok(Value::String(json_scalar_to_string(&value)));
    };
    let mut output = json_scalar_to_string(&format);
    for (token, replacement) in [
        ("YYYY", datetime.format("%Y").to_string()),
        ("HH24", datetime.format("%H").to_string()),
        ("HH12", datetime.format("%I").to_string()),
        (
            "MONTH",
            datetime.format("%B").to_string().to_ascii_uppercase(),
        ),
        (
            "MON",
            datetime.format("%b").to_string().to_ascii_uppercase(),
        ),
        ("MM", datetime.format("%m").to_string()),
        ("DD", datetime.format("%d").to_string()),
        ("MI", datetime.format("%M").to_string()),
        ("SS", datetime.format("%S").to_string()),
    ] {
        output = output.replace(token, &replacement);
    }
    Ok(Value::String(output))
}
pub(super) fn eval_make_date(
    year_arg: Option<&String>,
    day_arg: Option<&String>,
    data: &Map<String, Value>,
    last_insert_id: u64,
) -> Result<Value> {
    let year = year_arg
        .map(|arg| eval_scalar_text(arg, data, last_insert_id))
        .transpose()?
        .and_then(|value| value_to_i64(&value));
    let day = day_arg
        .map(|arg| eval_scalar_text(arg, data, last_insert_id))
        .transpose()?
        .and_then(|value| value_to_i64(&value));
    let (Some(year), Some(day)) = (year, day) else {
        return Ok(Value::Null);
    };
    if day <= 0 {
        return Ok(Value::Null);
    }
    let Ok(year) = i32::try_from(year) else {
        return Ok(Value::Null);
    };
    let Some(date) = NaiveDate::from_ymd_opt(year, 1, 1)
        .and_then(|date| date.checked_add_signed(Duration::days(day - 1)))
    else {
        return Ok(Value::Null);
    };
    Ok(Value::String(date.to_string()))
}
pub(super) fn eval_make_time(
    hour_arg: Option<&String>,
    minute_arg: Option<&String>,
    second_arg: Option<&String>,
    data: &Map<String, Value>,
    last_insert_id: u64,
) -> Result<Value> {
    let hour = hour_arg
        .map(|arg| eval_scalar_text(arg, data, last_insert_id))
        .transpose()?
        .and_then(|value| value_to_i64(&value));
    let minute = minute_arg
        .map(|arg| eval_scalar_text(arg, data, last_insert_id))
        .transpose()?
        .and_then(|value| value_to_i64(&value));
    let second = second_arg
        .map(|arg| eval_scalar_text(arg, data, last_insert_id))
        .transpose()?
        .and_then(|value| value_to_i64(&value));
    let (Some(hour), Some(minute), Some(second)) = (hour, minute, second) else {
        return Ok(Value::Null);
    };
    if !(0..60).contains(&minute) || !(0..60).contains(&second) {
        return Ok(Value::Null);
    }
    let sign = if hour < 0 { -1_i64 } else { 1_i64 };
    let magnitude = hour.unsigned_abs();
    let total_seconds = if magnitude > 838 {
        838_i64 * 3_600 + 59 * 60 + 59
    } else {
        magnitude as i64 * 3_600 + minute * 60 + second
    };
    Ok(Value::String(format_mysql_duration(Duration::seconds(
        total_seconds * sign,
    ))))
}
fn period_to_month_index(period: i64) -> Option<i64> {
    let period_year = period / 100;
    let year = if period >= 100_000 {
        period_year
    } else if period_year < 70 {
        2_000 + period_year
    } else {
        1_900 + period_year
    };
    year.checked_mul(12)?
        .checked_add(period.rem_euclid(100) - 1)
}

pub(super) fn eval_period_add(
    period_arg: Option<&String>,
    months_arg: Option<&String>,
    data: &Map<String, Value>,
    last_insert_id: u64,
) -> Result<Value> {
    let period = period_arg
        .map(|arg| eval_scalar_text(arg, data, last_insert_id))
        .transpose()?
        .and_then(|value| value_to_i64(&value));
    let months = months_arg
        .map(|arg| eval_scalar_text(arg, data, last_insert_id))
        .transpose()?
        .and_then(|value| value_to_i64(&value));
    let (Some(period), Some(months)) = (period, months) else {
        return Ok(Value::Null);
    };
    let Some(total_months) =
        period_to_month_index(period).and_then(|value| value.checked_add(months))
    else {
        return Ok(Value::Null);
    };
    let result_year = total_months.div_euclid(12);
    let result_month = total_months.rem_euclid(12) + 1;
    Ok(Value::Number(Number::from(
        result_year * 100 + result_month,
    )))
}
pub(super) fn eval_period_diff(
    left_arg: Option<&String>,
    right_arg: Option<&String>,
    data: &Map<String, Value>,
    last_insert_id: u64,
) -> Result<Value> {
    let left = left_arg
        .map(|arg| eval_scalar_text(arg, data, last_insert_id))
        .transpose()?
        .and_then(|value| value_to_i64(&value));
    let right = right_arg
        .map(|arg| eval_scalar_text(arg, data, last_insert_id))
        .transpose()?
        .and_then(|value| value_to_i64(&value));
    let (Some(left), Some(right)) = (left, right) else {
        return Ok(Value::Null);
    };
    let Some(difference) = period_to_month_index(left)
        .and_then(|left| period_to_month_index(right).and_then(|right| left.checked_sub(right)))
    else {
        return Ok(Value::Null);
    };
    Ok(Value::Number(Number::from(difference)))
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

pub(super) fn eval_last_day(
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
    let date = datetime.date();
    let first_next_month = if date.month() == 12 {
        NaiveDate::from_ymd_opt(date.year() + 1, 1, 1)
    } else {
        NaiveDate::from_ymd_opt(date.year(), date.month() + 1, 1)
    };
    Ok(first_next_month
        .and_then(|date| date.pred_opt())
        .map(|date| Value::String(date.to_string()))
        .unwrap_or(Value::Null))
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

pub(super) fn eval_year_week(
    date_arg: Option<&String>,
    mode_arg: Option<&String>,
    data: &Map<String, Value>,
    last_insert_id: u64,
) -> Result<Value> {
    let value = date_arg
        .map(|arg| eval_scalar_text(arg, data, last_insert_id))
        .transpose()?
        .unwrap_or(Value::Null);
    if value == Value::Null {
        return Ok(Value::Null);
    }
    let mode = mode_arg
        .map(|arg| eval_scalar_text(arg, data, last_insert_id))
        .transpose()?
        .and_then(|value| value_to_i64(&value))
        .unwrap_or(0);
    if mode != 3 {
        return Err(anyhow!("YEARWEEK supports ISO mode 3 only"));
    }
    let Some(datetime) = parse_mysql_datetime_value(&value) else {
        return Ok(Value::Null);
    };
    let iso_week = datetime.date().iso_week();
    Ok(Value::Number(Number::from(
        i64::from(iso_week.year()) * 100 + i64::from(iso_week.week()),
    )))
}

pub(super) fn eval_week_of_year(
    date_arg: Option<&String>,
    data: &Map<String, Value>,
    last_insert_id: u64,
) -> Result<Value> {
    let value = date_arg
        .map(|arg| eval_scalar_text(arg, data, last_insert_id))
        .transpose()?
        .unwrap_or(Value::Null);
    if value == Value::Null {
        return Ok(Value::Null);
    }
    let Some(datetime) = parse_mysql_datetime_value(&value) else {
        return Ok(Value::Null);
    };
    Ok(Value::Number(Number::from(i64::from(
        datetime.date().iso_week().week(),
    ))))
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
    if let Some((days, hours, minutes, seconds)) = compact_interval_parts(value) {
        return Ok(Value::Number(Number::from(match normalized.as_str() {
            "DAY" => days,
            "HOUR" => hours,
            "MINUTE" => minutes,
            "SECOND" => seconds,
            _ => return Ok(Value::Null),
        })));
    }
    let raw = json_scalar_to_string(value);
    // A full date-time also contains whitespace, but it must be parsed as a
    // date-time before the interval parser gets a chance to interpret it as
    // an interval.
    if let Some(datetime) = parse_mysql_datetime_value(value) {
        return extract_datetime_component(&normalized, datetime);
    }
    let interval_like = raw.contains(':')
        || raw.contains(char::is_whitespace)
        || raw
            .trim_start_matches(['+', '-'])
            .chars()
            .all(|character| character.is_ascii_digit() || character == '.');
    if interval_like {
        if let Some(duration) = parse_mysql_time_duration(value) {
            let total_hours = duration
                .num_seconds()
                .unsigned_abs()
                .checked_div(3_600)
                .unwrap_or(u64::MAX);
            if total_hours >= 87_649_416 {
                return Ok(Value::Null);
            }
            return extract_duration_component(&normalized, duration);
        }
        return Ok(Value::Null);
    }
    if let Some(duration) = parse_mysql_time_duration(value) {
        return extract_duration_component(&normalized, duration);
    }
    Ok(Value::Null)
}

fn compact_interval_parts(value: &Value) -> Option<(i64, i64, i64, i64)> {
    let raw = json_scalar_to_string(value).trim().to_string();
    let negative = raw.starts_with('-');
    let unsigned = raw.trim_start_matches(['+', '-']);
    let whole = unsigned
        .split_once('.')
        .map_or(unsigned, |(whole, _)| whole);
    let digits = whole.trim_start_matches('0');
    let digits = if digits.is_empty() { "0" } else { digits };
    let padded = format!("{digits:0>4}");
    let split = padded.len().saturating_sub(4);
    let hours = if split == 0 {
        0
    } else {
        padded[..split].parse::<i64>().ok()?
    };
    let minutes = padded[split..split + 2].parse::<i64>().ok()?;
    let seconds = padded[split + 2..].parse::<i64>().ok()?;
    if minutes > 59 || seconds > 59 {
        return None;
    }
    let total_hours = hours;
    if total_hours >= 87_649_416 {
        return None;
    }
    let sign = if negative { -1 } else { 1 };
    Some((
        (total_hours / 24) * sign,
        (total_hours % 24) * sign,
        minutes * sign,
        seconds * sign,
    ))
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
        "DAY" => sign * (total_seconds / 86_400) as i64,
        "HOUR" => sign * ((total_seconds / 3_600) % 24) as i64,
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

pub(super) fn eval_time_format(
    time_arg: Option<&String>,
    format_arg: Option<&String>,
    data: &Map<String, Value>,
    last_insert_id: u64,
) -> Result<Value> {
    let time = time_arg
        .map(|arg| eval_scalar_text(arg, data, last_insert_id))
        .transpose()?
        .unwrap_or(Value::Null);
    let format = format_arg
        .map(|arg| eval_scalar_text(arg, data, last_insert_id))
        .transpose()?
        .unwrap_or(Value::Null);
    if time == Value::Null || format == Value::Null {
        return Ok(Value::Null);
    }
    let raw = json_scalar_to_string(&time);
    let pieces = raw
        .trim_start_matches(['-', '+'])
        .split(':')
        .collect::<Vec<_>>();
    let hours = pieces
        .first()
        .and_then(|part| part.parse::<u32>().ok())
        .unwrap_or(0);
    let minutes = pieces
        .get(1)
        .and_then(|part| part.parse::<u32>().ok())
        .unwrap_or(0);
    let seconds_text = pieces.get(2).copied().unwrap_or("0");
    let seconds = seconds_text
        .split('.')
        .next()
        .and_then(|part| part.parse::<u32>().ok())
        .unwrap_or(0);
    let micros = seconds_text
        .split_once('.')
        .map(|(_, fraction)| {
            let mut value = fraction
                .chars()
                .take(6)
                .collect::<String>()
                .parse()
                .unwrap_or(0);
            for _ in fraction.chars().take(6).count()..6 {
                value *= 10;
            }
            value
        })
        .unwrap_or(0);
    let hour12 = match hours % 24 {
        0 | 12 => 12,
        hour => hour % 12,
    };
    let meridiem = if hours % 24 < 12 { "AM" } else { "PM" };
    let format = json_scalar_to_string(&format);
    let mut output = String::new();
    let mut chars = format.chars();
    while let Some(character) = chars.next() {
        if character != '%' {
            output.push(character);
            continue;
        }
        let Some(specifier) = chars.next() else { break };
        output.push_str(
            match specifier {
                'H' => format!("{hours:02}"),
                'k' => hours.to_string(),
                'h' | 'I' => format!("{hour12:02}"),
                'l' => hour12.to_string(),
                'i' => format!("{minutes:02}"),
                's' | 'S' => format!("{seconds:02}"),
                'f' => format!("{micros:06}"),
                'p' => meridiem.to_string(),
                'r' => format!("{hour12:02}:{minutes:02}:{seconds:02} {meridiem}"),
                'T' => format!("{hours:02}:{minutes:02}:{seconds:02}"),
                '%' => "%".to_string(),
                other => format!("%{other}"),
            }
            .as_str(),
        );
    }
    Ok(Value::String(output))
}

pub(crate) fn eval_date_add_sub(
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
    if !(0..=9999).contains(&result.year()) {
        return Ok(Value::Null);
    }
    let date_only = !json_scalar_to_string(&date_value).contains(' ')
        && matches!(
            interval.unit,
            MysqlIntervalUnit::Day
                | MysqlIntervalUnit::Week
                | MysqlIntervalUnit::Month
                | MysqlIntervalUnit::Quarter
                | MysqlIntervalUnit::Year
        );
    Ok(Value::String(if date_only {
        result.date().to_string()
    } else {
        result.to_string()
    }))
}

pub(super) fn eval_str_to_date(
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
    let input = json_scalar_to_string(&date);
    let format = json_scalar_to_string(&format)
        .replace("%i", "%M")
        .replace("%s", "%S")
        .replace("%#", "%f");
    if let Ok(value) = NaiveDateTime::parse_from_str(&input, &format) {
        let rendered = if format.contains("%f") {
            format_mysql_datetime(value, "%Y-%m-%d %H:%i:%s.%f")
        } else {
            value.to_string()
        };
        return Ok(Value::String(rendered));
    }
    if let Ok(value) = NaiveDate::parse_from_str(&input, &format) {
        return Ok(Value::String(value.to_string()));
    }
    if let Ok(value) = NaiveTime::parse_from_str(&input, &format) {
        return Ok(Value::String(format_mysql_naive_time(value)));
    }
    Ok(Value::Null)
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
    if unit_text.trim().eq_ignore_ascii_case("HOUR TO MINUTE") {
        let amount_text = amount_text.trim().trim_matches('\'').trim_matches('"');
        let negative = amount_text.starts_with('-');
        let amount_text = amount_text.trim_start_matches(['+', '-']);
        let (hours, minutes) = amount_text.split_once(':')?;
        let hours = hours.parse::<i64>().ok()?;
        let minutes = minutes.parse::<i64>().ok()?;
        if !(0..60).contains(&minutes) {
            return None;
        }
        let amount = hours.checked_mul(60)?.checked_add(minutes)?;
        let amount = if negative {
            amount.checked_neg()?
        } else {
            amount
        };
        return Some(MysqlInterval {
            amount,
            unit: MysqlIntervalUnit::Minute,
        });
    }
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

pub(crate) fn parse_mysql_datetime_value(value: &Value) -> Option<NaiveDateTime> {
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
