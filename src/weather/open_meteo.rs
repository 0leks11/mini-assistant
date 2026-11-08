use std::time::Duration;

use super::locations::Location;
use super::types::{
    CurrentConditions, DailyForecast, GetWeatherRequest, GetWeatherResponse, HourlyForecast,
    WeatherError, WeatherPeriod, WeatherUnits,
};

const DEFAULT_BASE_URL: &str = "https://api.open-meteo.com/v1/forecast";
const DEFAULT_GEOCODING_URL: &str = "https://geocoding-api.open-meteo.com/v1/search";
const MAX_BODY_BYTES: usize = 512 * 1024; // 512 KiB limit per plan

pub struct OpenMeteoClient {
    base_url: String,
    geocoding_url: String,
    http_client: reqwest::Client,
}

impl Default for OpenMeteoClient {
    fn default() -> Self {
        Self::new(DEFAULT_BASE_URL.to_string())
    }
}

impl OpenMeteoClient {
    pub fn new(base_url: String) -> Self {
        let geocoding_url = if base_url == DEFAULT_BASE_URL {
            DEFAULT_GEOCODING_URL.to_string()
        } else {
            format!("{}/search", base_url.trim_end_matches('/'))
        };
        Self::with_urls(base_url, geocoding_url)
    }

    pub fn with_urls(base_url: String, geocoding_url: String) -> Self {
        let http_client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(2))
            .timeout(Duration::from_secs(3))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());

        Self {
            base_url,
            geocoding_url,
            http_client,
        }
    }

    pub fn wmo_code_to_ru(code: u16) -> &'static str {
        match code {
            0 => "ясно",
            1 => "преимущественно ясно",
            2 => "переменная облачность",
            3 => "пасмурно",
            45 | 48 => "туман",
            51 | 53 | 55 => "морось",
            61 => "небольшой дождь",
            63 => "дождь",
            65 => "сильный дождь",
            71 | 73 | 75 => "снег",
            77 => "снежные зёрна",
            80..=82 => "ливневый дождь",
            85 | 86 => "снегопад",
            95 => "гроза",
            96 | 99 => "гроза с градом",
            _ => "без существенных изменений",
        }
    }

    pub fn wind_deg_to_ru(deg: u16) -> &'static str {
        let d = deg % 360;
        match d {
            338..=359 | 0..=22 => "северный",
            23..=67 => "северо-восточный",
            68..=112 => "восточный",
            113..=157 => "юго-восточный",
            158..=202 => "южный",
            203..=247 => "юго-западный",
            248..=292 => "западный",
            293..=337 => "северо-западный",
            _ => "переменный",
        }
    }

    pub async fn fetch(
        &self,
        location: &Location,
        req: &GetWeatherRequest,
    ) -> Result<GetWeatherResponse, WeatherError> {
        let url = format!(
            "{base}?latitude={lat}&longitude={lon}&current=temperature_2m,relative_humidity_2m,apparent_temperature,precipitation,weather_code,cloud_cover,wind_speed_10m,wind_direction_10m,wind_gusts_10m&hourly=temperature_2m,relative_humidity_2m,apparent_temperature,precipitation_probability,precipitation,weather_code,cloud_cover,wind_speed_10m,wind_direction_10m,wind_gusts_10m&daily=weather_code,temperature_2m_max,temperature_2m_min,apparent_temperature_max,apparent_temperature_min,precipitation_sum,precipitation_probability_max,wind_speed_10m_max,wind_gusts_10m_max,wind_direction_10m_dominant&timezone={tz}&wind_speed_unit=ms&precipitation_unit=mm&forecast_days=7",
            base = self.base_url.trim_end_matches('/'),
            lat = location.latitude,
            lon = location.longitude,
            tz = location.timezone,
        );

        let response = self.http_client.get(&url).send().await.map_err(|e| {
            if e.is_timeout() {
                WeatherError::Timeout
            } else {
                WeatherError::Unavailable(e.to_string())
            }
        })?;

        let status = response.status();
        if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            return Err(WeatherError::RateLimited);
        }
        if !status.is_success() {
            return Err(WeatherError::Unavailable(format!("HTTP error {status}")));
        }

        // Bounded body read (max 512 KiB)
        let bytes = response
            .bytes()
            .await
            .map_err(|e| WeatherError::InvalidProviderData(e.to_string()))?;
        if bytes.len() > MAX_BODY_BYTES {
            return Err(WeatherError::InvalidProviderData(format!(
                "Response size {} bytes exceeds 512 KiB limit",
                bytes.len()
            )));
        }

        let raw: serde_json::Value = serde_json::from_slice(&bytes)
            .map_err(|e| WeatherError::InvalidProviderData(e.to_string()))?;

        self.parse_response(location, req, &raw)
    }

    pub async fn geocode(&self, query: &str) -> Result<Location, WeatherError> {
        let q = query.trim();
        if q.is_empty() {
            return Err(WeatherError::UnknownLocation(query.to_string()));
        }

        let response = self
            .http_client
            .get(&self.geocoding_url)
            .header(reqwest::header::USER_AGENT, "mini-assistant/0.1.0")
            .query(&[
                ("name", q),
                ("count", "1"),
                ("language", "ru"),
                ("format", "json"),
            ])
            .send()
            .await
            .map_err(|e| {
                if e.is_timeout() {
                    WeatherError::Timeout
                } else {
                    WeatherError::Unavailable(e.to_string())
                }
            })?;

        let status = response.status();
        if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            return Err(WeatherError::RateLimited);
        }
        if !status.is_success() {
            return Err(WeatherError::Unavailable(format!(
                "Geocoding HTTP error {status}"
            )));
        }

        let bytes = response
            .bytes()
            .await
            .map_err(|e| WeatherError::Unavailable(e.to_string()))?;

        if bytes.len() > MAX_BODY_BYTES {
            return Err(WeatherError::InvalidProviderData(format!(
                "Geocoding response size {} bytes exceeds 512 KiB limit",
                bytes.len()
            )));
        }

        #[derive(serde::Deserialize)]
        struct GeocodingItem {
            name: String,
            latitude: f64,
            longitude: f64,
            country: Option<String>,
            country_code: Option<String>,
            timezone: Option<String>,
        }

        #[derive(serde::Deserialize)]
        struct GeocodingPayload {
            results: Option<Vec<GeocodingItem>>,
        }

        let payload: GeocodingPayload = serde_json::from_slice(&bytes)
            .map_err(|e| WeatherError::InvalidProviderData(e.to_string()))?;

        if let Some(mut results) = payload.results {
            if let Some(first) = results.pop() {
                let slug = q.to_lowercase().replace(' ', "-");
                let tz = first.timezone.unwrap_or_else(|| "UTC".to_string());
                let country = first.country_code.or(first.country).unwrap_or_default();
                let lower_name = first.name.to_lowercase();
                let mut aliases = vec![lower_name.clone()];
                let lower_q = q.to_lowercase();
                if lower_q != lower_name {
                    aliases.push(lower_q);
                }
                return Ok(Location {
                    id: slug,
                    name: first.name,
                    country,
                    latitude: first.latitude,
                    longitude: first.longitude,
                    timezone: tz,
                    aliases,
                });
            }
        }

        Err(WeatherError::UnknownLocation(query.to_string()))
    }

    fn parse_response(
        &self,
        location: &Location,
        req: &GetWeatherRequest,
        raw: &serde_json::Value,
    ) -> Result<GetWeatherResponse, WeatherError> {
        let current_obj = raw.get("current");
        let current = current_obj.map(|c| {
            let temp = c
                .get("temperature_2m")
                .and_then(|v| v.as_f64())
                .unwrap_or(0.0);
            let code = c
                .get("weather_code")
                .and_then(|v| v.as_u64())
                .map(|n| n as u16);
            CurrentConditions {
                temperature: temp,
                feels_like: c.get("apparent_temperature").and_then(|v| v.as_f64()),
                humidity: c
                    .get("relative_humidity_2m")
                    .and_then(|v| v.as_u64())
                    .map(|n| n as u8),
                cloud_cover: c
                    .get("cloud_cover")
                    .and_then(|v| v.as_u64())
                    .map(|n| n as u8),
                wind_speed_ms: c.get("wind_speed_10m").and_then(|v| v.as_f64()),
                wind_gusts_ms: c.get("wind_gusts_10m").and_then(|v| v.as_f64()),
                wind_direction_deg: c
                    .get("wind_direction_10m")
                    .and_then(|v| v.as_u64())
                    .map(|n| n as u16),
                precipitation_mm: c.get("precipitation").and_then(|v| v.as_f64()),
                weather_code: code,
                description: code.map(Self::wmo_code_to_ru).map(|s| s.to_string()),
            }
        });

        let mut daily = Vec::new();
        if let Some(d) = raw.get("daily") {
            let times = d.get("time").and_then(|v| v.as_array());
            if let Some(time_arr) = times {
                let codes = d.get("weather_code").and_then(|v| v.as_array());
                let t_max = d.get("temperature_2m_max").and_then(|v| v.as_array());
                let t_min = d.get("temperature_2m_min").and_then(|v| v.as_array());
                let app_max = d.get("apparent_temperature_max").and_then(|v| v.as_array());
                let app_min = d.get("apparent_temperature_min").and_then(|v| v.as_array());
                let precip_sum = d.get("precipitation_sum").and_then(|v| v.as_array());
                let precip_prob = d
                    .get("precipitation_probability_max")
                    .and_then(|v| v.as_array());
                let wind_max = d.get("wind_speed_10m_max").and_then(|v| v.as_array());
                let gusts_max = d.get("wind_gusts_10m_max").and_then(|v| v.as_array());
                let wind_dir = d
                    .get("wind_direction_10m_dominant")
                    .and_then(|v| v.as_array());

                for (i, time_val) in time_arr.iter().enumerate() {
                    let date_str = time_val.as_str().unwrap_or("").to_string();
                    let code = codes
                        .and_then(|a| a.get(i))
                        .and_then(|v| v.as_u64())
                        .map(|n| n as u16);
                    daily.push(DailyForecast {
                        date: date_str,
                        temp_min: t_min.and_then(|a| a.get(i)).and_then(|v| v.as_f64()),
                        temp_max: t_max.and_then(|a| a.get(i)).and_then(|v| v.as_f64()),
                        feels_like_min: app_min.and_then(|a| a.get(i)).and_then(|v| v.as_f64()),
                        feels_like_max: app_max.and_then(|a| a.get(i)).and_then(|v| v.as_f64()),
                        humidity_mean: None,
                        cloud_cover_mean: None,
                        precipitation_sum_mm: precip_sum
                            .and_then(|a| a.get(i))
                            .and_then(|v| v.as_f64()),
                        precipitation_probability_max: precip_prob
                            .and_then(|a| a.get(i))
                            .and_then(|v| v.as_u64())
                            .map(|n| n as u8),
                        wind_speed_max_ms: wind_max.and_then(|a| a.get(i)).and_then(|v| v.as_f64()),
                        wind_gusts_max_ms: gusts_max
                            .and_then(|a| a.get(i))
                            .and_then(|v| v.as_f64()),
                        wind_direction_dominant: wind_dir
                            .and_then(|a| a.get(i))
                            .and_then(|v| v.as_u64())
                            .map(|n| n as u16),
                        weather_code: code,
                        description: code.map(Self::wmo_code_to_ru).map(|s| s.to_string()),
                    });
                }
            }
        }

        let mut hourly = Vec::new();
        if let Some(h) = raw.get("hourly") {
            let times = h.get("time").and_then(|v| v.as_array());
            if let Some(time_arr) = times {
                let temps = h.get("temperature_2m").and_then(|v| v.as_array());
                let app_temps = h.get("apparent_temperature").and_then(|v| v.as_array());
                let humids = h.get("relative_humidity_2m").and_then(|v| v.as_array());
                let clouds = h.get("cloud_cover").and_then(|v| v.as_array());
                let winds = h.get("wind_speed_10m").and_then(|v| v.as_array());
                let gusts = h.get("wind_gusts_10m").and_then(|v| v.as_array());
                let dirs = h.get("wind_direction_10m").and_then(|v| v.as_array());
                let precips = h.get("precipitation").and_then(|v| v.as_array());
                let probs = h
                    .get("precipitation_probability")
                    .and_then(|v| v.as_array());
                let codes = h.get("weather_code").and_then(|v| v.as_array());

                for (i, time_val) in time_arr.iter().enumerate().take(48) {
                    let time_str = time_val.as_str().unwrap_or("").to_string();
                    let temp = temps
                        .and_then(|a| a.get(i))
                        .and_then(|v| v.as_f64())
                        .unwrap_or(0.0);
                    let code = codes
                        .and_then(|a| a.get(i))
                        .and_then(|v| v.as_u64())
                        .map(|n| n as u16);
                    hourly.push(HourlyForecast {
                        time: time_str,
                        temperature: temp,
                        feels_like: app_temps.and_then(|a| a.get(i)).and_then(|v| v.as_f64()),
                        humidity: humids
                            .and_then(|a| a.get(i))
                            .and_then(|v| v.as_u64())
                            .map(|n| n as u8),
                        cloud_cover: clouds
                            .and_then(|a| a.get(i))
                            .and_then(|v| v.as_u64())
                            .map(|n| n as u8),
                        wind_speed_ms: winds.and_then(|a| a.get(i)).and_then(|v| v.as_f64()),
                        wind_gusts_ms: gusts.and_then(|a| a.get(i)).and_then(|v| v.as_f64()),
                        wind_direction_deg: dirs
                            .and_then(|a| a.get(i))
                            .and_then(|v| v.as_u64())
                            .map(|n| n as u16),
                        precipitation_mm: precips.and_then(|a| a.get(i)).and_then(|v| v.as_f64()),
                        precipitation_probability: probs
                            .and_then(|a| a.get(i))
                            .and_then(|v| v.as_u64())
                            .map(|n| n as u8),
                        weather_code: code,
                        description: code.map(Self::wmo_code_to_ru).map(|s| s.to_string()),
                    });
                }
            }
        }

        let summary = Self::build_voice_summary(location, req, &current, &daily);

        Ok(GetWeatherResponse {
            status: "ok".to_string(),
            provider: "open-meteo".to_string(),
            location_id: location.id.to_string(),
            location_name: location.name.to_string(),
            timezone: location.timezone.to_string(),
            units: WeatherUnits::default(),
            fetched_at_utc: time::OffsetDateTime::now_utc()
                .format(&time::format_description::well_known::Rfc3339)
                .unwrap_or_else(|_| "unknown".to_string()),
            cache_state: "fresh".to_string(),
            age_seconds: 0,
            current,
            daily: if daily.is_empty() { None } else { Some(daily) },
            hourly: if hourly.is_empty() {
                None
            } else {
                Some(hourly)
            },
            summary,
            message: None,
        })
    }

    fn build_voice_summary(
        location: &Location,
        req: &GetWeatherRequest,
        current: &Option<CurrentConditions>,
        daily: &[DailyForecast],
    ) -> String {
        match req.period {
            WeatherPeriod::Now | WeatherPeriod::Today => {
                let mut parts = Vec::new();
                if let Some(c) = current {
                    let desc = c.description.as_deref().unwrap_or("без осадков");
                    let temp_sign = if c.temperature > 0.0 { "+" } else { "" };
                    parts.push(format!(
                        "В {} сейчас {}{:.0}°C, {}",
                        location.name, temp_sign, c.temperature, desc
                    ));

                    if let Some(w) = c.wind_speed_ms {
                        let dir = c.wind_direction_deg.map(Self::wind_deg_to_ru).unwrap_or("");
                        parts.push(format!("ветер {} {:.0} м/с", dir, w));
                    }
                    if let Some(h) = c.humidity {
                        parts.push(format!("влажность {}%", h));
                    }
                }
                if let Some(today) = daily.first() {
                    if let (Some(min), Some(max)) = (today.temp_min, today.temp_max) {
                        parts.push(format!("сегодня днем от {:.0}° до {:.0}°", min, max));
                    }
                }
                if parts.is_empty() {
                    format!("Данные о погоде для {} получены.", location.name)
                } else {
                    parts.join(", ") + "."
                }
            }
            WeatherPeriod::Tomorrow => {
                if let Some(tomorrow) = daily.get(1) {
                    let desc = tomorrow.description.as_deref().unwrap_or("без осадков");
                    let temp_range = match (tomorrow.temp_min, tomorrow.temp_max) {
                        (Some(min), Some(max)) => format!("от {:.0}° до {:.0}°C", min, max),
                        _ => "".to_string(),
                    };
                    let wind_info = match tomorrow.wind_speed_max_ms {
                        Some(w) => format!(", ветер до {:.0} м/с", w),
                        None => "".to_string(),
                    };
                    format!(
                        "Завтра в {} {}, {}{}.",
                        location.name, desc, temp_range, wind_info
                    )
                } else {
                    format!("Прогноз на завтра для {} пока недоступен.", location.name)
                }
            }
            WeatherPeriod::Next7Days | WeatherPeriod::ThisWeekend => {
                if daily.len() >= 3 {
                    let mut summary_days = Vec::new();
                    for d in daily.iter().take(7) {
                        let desc = d.description.as_deref().unwrap_or("");
                        let range = match (d.temp_min, d.temp_max) {
                            (Some(min), Some(max)) => format!("{:.0}°..{:.0}°", min, max),
                            _ => "".to_string(),
                        };
                        summary_days.push(format!("{}: {} ({})", d.date, range, desc));
                    }
                    format!(
                        "Прогноз на 7 дней для {}:\n{}",
                        location.name,
                        summary_days.join("\n")
                    )
                } else {
                    format!("Недельный прогноз для {} формируется.", location.name)
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn test_location() -> Location {
        Location {
            id: "batumi".to_string(),
            name: "Батуми".to_string(),
            country: "GE".to_string(),
            latitude: 41.6434,
            longitude: 41.6399,
            timezone: "Asia/Tbilisi".to_string(),
            aliases: vec!["батуми".to_string()],
        }
    }

    #[test]
    fn test_parse_valid_response() {
        let client = OpenMeteoClient::default();
        let loc = test_location();
        let req = GetWeatherRequest {
            location_id: "batumi".to_string(),
            period: WeatherPeriod::Now,
            granularity: crate::weather::types::WeatherGranularity::Current,
            metrics: vec![],
            provider: None,
        };

        let raw = json!({
            "current": {
                "temperature_2m": 18.4,
                "apparent_temperature": 18.0,
                "relative_humidity_2m": 72,
                "cloud_cover": 40,
                "wind_speed_10m": 4.5,
                "wind_direction_10m": 220,
                "precipitation": 0.0,
                "weather_code": 2
            },
            "daily": {
                "time": ["2026-11-03", "2026-11-04"],
                "weather_code": [2, 61],
                "temperature_2m_max": [20.0, 17.5],
                "temperature_2m_min": [14.0, 13.0],
                "wind_speed_10m_max": [5.0, 6.2]
            }
        });

        let resp = client.parse_response(&loc, &req, &raw).unwrap();
        assert_eq!(resp.status, "ok");
        assert_eq!(resp.location_id, "batumi");
        assert_eq!(resp.location_name, "Батуми");
        let curr = resp.current.unwrap();
        assert_eq!(curr.temperature, 18.4);
        assert_eq!(curr.description.as_deref(), Some("переменная облачность"));
        assert!(resp.summary.contains("Батуми сейчас +18°C"));
    }

    #[test]
    fn test_wmo_code_descriptions() {
        assert_eq!(OpenMeteoClient::wmo_code_to_ru(0), "ясно");
        assert_eq!(OpenMeteoClient::wmo_code_to_ru(3), "пасмурно");
        assert_eq!(OpenMeteoClient::wmo_code_to_ru(63), "дождь");
        assert_eq!(OpenMeteoClient::wmo_code_to_ru(95), "гроза");
    }

    #[test]
    fn test_wind_direction_labels() {
        assert_eq!(OpenMeteoClient::wind_deg_to_ru(0), "северный");
        assert_eq!(OpenMeteoClient::wind_deg_to_ru(90), "восточный");
        assert_eq!(OpenMeteoClient::wind_deg_to_ru(180), "южный");
        assert_eq!(OpenMeteoClient::wind_deg_to_ru(225), "юго-западный");
        assert_eq!(OpenMeteoClient::wind_deg_to_ru(270), "западный");
    }

    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::thread;

    fn mock_server(status: &str, body: &str) -> (String, thread::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback port");
        let url = format!("http://{}/v1/forecast", listener.local_addr().unwrap());
        let response = format!(
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );

        let task = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut req = Vec::new();
            let mut buf = [0u8; 1024];
            while !req.windows(4).any(|w| w == b"\r\n\r\n") {
                let n = stream.read(&mut buf).unwrap();
                if n == 0 {
                    break;
                }
                req.extend_from_slice(&buf[..n]);
            }
            stream.write_all(response.as_bytes()).unwrap();
            String::from_utf8_lossy(&req).to_string()
        });

        (url, task)
    }

    #[tokio::test]
    async fn test_fetch_success_and_query_parameters() {
        let sample_json = r#"{
            "current": {
                "temperature_2m": 19.5,
                "apparent_temperature": 19.0,
                "relative_humidity_2m": 65,
                "cloud_cover": 20,
                "wind_speed_10m": 3.5,
                "wind_direction_10m": 180,
                "precipitation": 0.0,
                "weather_code": 0
            },
            "daily": {
                "time": ["2026-11-03"],
                "weather_code": [0],
                "temperature_2m_max": [21.0],
                "temperature_2m_min": [15.0]
            }
        }"#;

        let (url, handle) = mock_server("200 OK", sample_json);
        let client = OpenMeteoClient::new(url);
        let loc = test_location();
        let req = GetWeatherRequest {
            location_id: "batumi".to_string(),
            period: WeatherPeriod::Now,
            granularity: crate::weather::types::WeatherGranularity::Current,
            metrics: vec![],
            provider: None,
        };

        let resp = client.fetch(&loc, &req).await.unwrap();
        let request_str = handle.join().unwrap();

        assert_eq!(resp.status, "ok");
        assert_eq!(resp.current.unwrap().temperature, 19.5);
        assert!(request_str.contains("latitude=41.6434"));
        assert!(request_str.contains("longitude=41.6399"));
        assert!(request_str.contains("timezone=Asia/Tbilisi"));
    }

    #[tokio::test]
    async fn test_fetch_rate_limited_429() {
        let (url, handle) = mock_server("429 Too Many Requests", r#"{"error":true}"#);
        let client = OpenMeteoClient::new(url);
        let loc = test_location();
        let req = GetWeatherRequest {
            location_id: "batumi".to_string(),
            period: WeatherPeriod::Now,
            granularity: crate::weather::types::WeatherGranularity::Current,
            metrics: vec![],
            provider: None,
        };

        let err = client.fetch(&loc, &req).await.unwrap_err();
        handle.join().unwrap();
        assert!(matches!(err, WeatherError::RateLimited));
    }

    #[tokio::test]
    async fn test_fetch_server_error_500() {
        let (url, handle) = mock_server("500 Internal Server Error", r#"{"reason":"down"}"#);
        let client = OpenMeteoClient::new(url);
        let loc = test_location();
        let req = GetWeatherRequest {
            location_id: "batumi".to_string(),
            period: WeatherPeriod::Now,
            granularity: crate::weather::types::WeatherGranularity::Current,
            metrics: vec![],
            provider: None,
        };

        let err = client.fetch(&loc, &req).await.unwrap_err();
        handle.join().unwrap();
        assert!(matches!(err, WeatherError::Unavailable(_)));
    }

    #[tokio::test]
    async fn test_fetch_oversized_body_rejected() {
        // Create 600 KiB payload (exceeding 512 KiB limit)
        let large_body = "x".repeat(600 * 1024);
        let (url, handle) = mock_server("200 OK", &large_body);
        let client = OpenMeteoClient::new(url);
        let loc = test_location();
        let req = GetWeatherRequest {
            location_id: "batumi".to_string(),
            period: WeatherPeriod::Now,
            granularity: crate::weather::types::WeatherGranularity::Current,
            metrics: vec![],
            provider: None,
        };

        let err = client.fetch(&loc, &req).await.unwrap_err();
        handle.join().unwrap();
        match err {
            WeatherError::InvalidProviderData(msg) => {
                assert!(msg.contains("exceeds 512 KiB limit"));
            }
            other => panic!("Expected InvalidProviderData, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_geocode_success() {
        let geocode_json = r#"{
            "results": [
                {
                    "id": 524901,
                    "name": "Москва",
                    "latitude": 55.7522,
                    "longitude": 37.6156,
                    "country": "Россия",
                    "country_code": "RU",
                    "timezone": "Europe/Moscow"
                }
            ]
        }"#;

        let (url, handle) = mock_server("200 OK", geocode_json);
        let client = OpenMeteoClient::with_urls("http://unused".to_string(), url);
        let loc = client.geocode("Москва").await.unwrap();
        handle.join().unwrap();

        assert_eq!(loc.name, "Москва");
        assert_eq!(loc.latitude, 55.7522);
        assert_eq!(loc.longitude, 37.6156);
        assert_eq!(loc.timezone, "Europe/Moscow");
    }

    #[tokio::test]
    async fn test_geocode_not_found() {
        let (url, handle) = mock_server("200 OK", r#"{"results": []}"#);
        let client = OpenMeteoClient::with_urls("http://unused".to_string(), url);
        let err = client.geocode("unknown_city").await.unwrap_err();
        handle.join().unwrap();

        assert!(matches!(err, WeatherError::UnknownLocation(_)));
    }
}
