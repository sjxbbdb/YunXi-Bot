//! 天气证据只暴露必要字段；原始坐标和服务请求地址留在原生调用内。
use super::ToolError;
use serde_json::{Value, json};

fn invalid() -> ToolError {
    ToolError::Failed {
        detail: "天气证据缺失、时段不符或数值无效；不得据此生成预报".into(),
    }
}

pub(super) fn evidence(args: &Value, raw: Value) -> Result<Value, ToolError> {
    let date = args["date"].as_str().ok_or_else(invalid)?;
    let fetched =
        chrono::DateTime::parse_from_rfc3339(raw["fetched_at"].as_str().ok_or_else(invalid)?)
            .map_err(|_| invalid())?;
    let expires = fetched
        .checked_add_signed(chrono::Duration::minutes(30))
        .ok_or_else(invalid)?;
    let forecast = &raw["forecast"];
    let timezone = forecast["timezone"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or_else(invalid)?;
    let hourly = &forecast["hourly"];
    let times = hourly["time"]
        .as_array()
        .filter(|a| !a.is_empty() && a.len() <= 25)
        .ok_or_else(invalid)?;
    let units = &forecast["hourly_units"];
    for (field, unit) in [
        ("temperature_2m", "°C"),
        ("precipitation_probability", "%"),
        ("wind_speed_10m", "km/h"),
    ] {
        if units[field] != unit || hourly[field].as_array().map(Vec::len) != Some(times.len()) {
            return Err(invalid());
        }
    }
    if hourly["weather_code"].as_array().map(Vec::len) != Some(times.len()) {
        return Err(invalid());
    }
    let mut hours = Vec::new();
    let mut previous = None;
    for (i, value) in times.iter().enumerate() {
        let time = value.as_str().ok_or_else(invalid)?;
        let parsed =
            chrono::NaiveDateTime::parse_from_str(time, "%Y-%m-%dT%H:%M").map_err(|_| invalid())?;
        if parsed.format("%Y-%m-%d").to_string() != date || previous.is_some_and(|p| parsed <= p) {
            return Err(invalid());
        }
        previous = Some(parsed);
        let number = |key: &str, min: f64, max: f64| -> Result<f64, ToolError> {
            hourly[key][i]
                .as_f64()
                .filter(|v| v.is_finite() && (min..=max).contains(v))
                .ok_or_else(invalid)
        };
        let temp = number("temperature_2m", -100., 70.)?;
        let rain = number("precipitation_probability", 0., 100.)?;
        let wind = number("wind_speed_10m", 0., 500.)?;
        let code = hourly["weather_code"][i]
            .as_u64()
            .filter(|v| *v <= 99)
            .ok_or_else(invalid)?;
        hours.push(json!({"time":time,"temperature_c":temp,"rain_probability_percent":rain,"wind_kmh":wind,"weather_code":code}));
    }
    let offset = forecast["utc_offset_seconds"]
        .as_i64()
        .filter(|v| (-43200..=50400).contains(v))
        .ok_or_else(invalid)?;
    // 开机问候也读取 hourly；保持契约，只保留已经验证的字段。
    Ok(
        json!({"source":"Open-Meteo","source_url":"https://open-meteo.com/", "fetched_at":fetched.to_rfc3339(),
        "valid_until":expires.to_rfc3339(),"date":date,
        "forecast":{"timezone":timezone,"utc_offset_seconds":offset,
            "hourly_units":{"temperature_2m":"°C","precipitation_probability":"%","wind_speed_10m":"km/h","weather_code":"wmo code"},
            "hourly":{"time":hours.iter().map(|v|v["time"].clone()).collect::<Vec<_>>(),
                "temperature_2m":hours.iter().map(|v|v["temperature_c"].clone()).collect::<Vec<_>>(),
                "precipitation_probability":hours.iter().map(|v|v["rain_probability_percent"].clone()).collect::<Vec<_>>(),
                "wind_speed_10m":hours.iter().map(|v|v["wind_kmh"].clone()).collect::<Vec<_>>(),
                "weather_code":hours.iter().map(|v|v["weather_code"].clone()).collect::<Vec<_>>() }},
        "interpretation":"小时预报，不是实况或确定事件。precipitation_probability 是降雨概率，不证明几点开始或结束下雨。只对已列时段有依据；无历史数据不能比较降温。"}),
    )
}

pub(crate) fn fresh(text: &str, now_ms: u64) -> bool {
    let Ok(value) = serde_json::from_str::<Value>(text) else {
        return false;
    };
    let stamp = |key: &str| {
        value[key]
            .as_str()
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            .and_then(|t| u64::try_from(t.timestamp_millis()).ok())
    };
    matches!((stamp("fetched_at"),stamp("valid_until")), (Some(start),Some(end)) if start <= now_ms && now_ms < end && end - start <= 1_800_000)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn sample() -> Value {
        json!({"source":"Open-Meteo","source_url":"https://api.open-meteo.com/v1/forecast?latitude=31.2&longitude=121.5",
            "fetched_at":"2026-10-19T10:00:00+08:00","forecast":{"latitude":31.2,"longitude":121.5,
            "timezone":"Asia/Shanghai","utc_offset_seconds":28800,"hourly_units":{"temperature_2m":"°C","precipitation_probability":"%","wind_speed_10m":"km/h"},
            "hourly":{"time":["2026-10-20T15:00"],"temperature_2m":[18.5],"precipitation_probability":[40],"wind_speed_10m":[12],"weather_code":[3]}}})
    }
    #[test]
    fn forecast_is_dated_and_coordinate_free_without_strengthening_probability() {
        let out = evidence(&json!({"date":"2026-10-20"}), sample()).unwrap();
        assert!(!out.to_string().contains("latitude"));
        assert!(!out.to_string().contains("121.5"));
        assert_eq!(
            out["forecast"]["hourly"]["precipitation_probability"][0],
            40.0
        );
        assert_eq!(out["date"], "2026-10-20");
        assert_eq!(out["valid_until"], "2026-10-19T10:30:00+08:00");
    }
    #[test]
    fn expiry_and_clock_rollback_require_a_new_observation() {
        let out = evidence(&json!({"date":"2026-10-20"}), sample())
            .unwrap()
            .to_string();
        let now = chrono::DateTime::parse_from_rfc3339("2026-10-19T10:00:00+08:00")
            .unwrap()
            .timestamp_millis() as u64;
        assert!(fresh(&out, now));
        assert!(!fresh(&out, now - 1));
        assert!(!fresh(&out, now + 1_800_000));
        assert!(!fresh("已过期", now));
    }
    #[test]
    fn wrong_date_bad_probability_and_incomplete_hours_fail_closed() {
        for mutation in [0, 1, 2, 3] {
            let mut raw = sample();
            match mutation {
                0 => raw["forecast"]["hourly"]["time"][0] = json!("2026-10-21T15:00"),
                1 => raw["forecast"]["hourly"]["precipitation_probability"][0] = json!(101),
                2 => raw["forecast"]["hourly"]["temperature_2m"] = json!([]),
                _ => raw["fetched_at"] = json!("unknown"),
            }
            assert!(
                evidence(&json!({"date":"2026-10-20"}), raw).is_err(),
                "{mutation}"
            );
        }
    }
}
