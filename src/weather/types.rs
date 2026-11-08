use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum WeatherPeriod {
    #[default]
    Now,
    Today,
    Tomorrow,
    Next7Days,
    ThisWeekend,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum WeatherGranularity {
    #[default]
    Current,
    Hourly,
    Daily,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WeatherMetric {
    Temperature,
    FeelsLike,
    Humidity,
    CloudCover,
    Wind,
    Gusts,
    Precipitation,
    PrecipitationProbability,
    WeatherCode,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GetWeatherRequest {
    /// Saved location ID or city name alias (e.g. "batumi", "tbilisi", "tsikhisdziri")
    pub location_id: String,
    #[serde(default)]
    pub period: WeatherPeriod,
    #[serde(default)]
    pub granularity: WeatherGranularity,
    #[serde(default)]
    pub metrics: Vec<WeatherMetric>,
    #[serde(default)]
    pub provider: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WeatherUnits {
    pub temperature: String,
    pub wind_speed: String,
    pub precipitation: String,
    pub humidity: String,
    pub cloud_cover: String,
}

impl Default for WeatherUnits {
    fn default() -> Self {
        Self {
            temperature: "°C".to_string(),
            wind_speed: "м/с".to_string(),
            precipitation: "мм".to_string(),
            humidity: "%".to_string(),
            cloud_cover: "%".to_string(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CurrentConditions {
    pub temperature: f64,
    pub feels_like: Option<f64>,
    pub humidity: Option<u8>,
    pub cloud_cover: Option<u8>,
    pub wind_speed_ms: Option<f64>,
    pub wind_gusts_ms: Option<f64>,
    pub wind_direction_deg: Option<u16>,
    pub precipitation_mm: Option<f64>,
    pub weather_code: Option<u16>,
    pub description: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DailyForecast {
    pub date: String,
    pub temp_min: Option<f64>,
    pub temp_max: Option<f64>,
    pub feels_like_min: Option<f64>,
    pub feels_like_max: Option<f64>,
    pub humidity_mean: Option<u8>,
    pub cloud_cover_mean: Option<u8>,
    pub precipitation_sum_mm: Option<f64>,
    pub precipitation_probability_max: Option<u8>,
    pub wind_speed_max_ms: Option<f64>,
    pub wind_gusts_max_ms: Option<f64>,
    pub wind_direction_dominant: Option<u16>,
    pub weather_code: Option<u16>,
    pub description: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HourlyForecast {
    pub time: String,
    pub temperature: f64,
    pub feels_like: Option<f64>,
    pub humidity: Option<u8>,
    pub cloud_cover: Option<u8>,
    pub wind_speed_ms: Option<f64>,
    pub wind_gusts_ms: Option<f64>,
    pub wind_direction_deg: Option<u16>,
    pub precipitation_mm: Option<f64>,
    pub precipitation_probability: Option<u8>,
    pub weather_code: Option<u16>,
    pub description: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GetWeatherResponse {
    pub status: String,
    pub provider: String,
    pub location_id: String,
    pub location_name: String,
    pub timezone: String,
    pub units: WeatherUnits,
    pub fetched_at_utc: String,
    pub cache_state: String,
    pub age_seconds: u64,
    pub current: Option<CurrentConditions>,
    pub daily: Option<Vec<DailyForecast>>,
    pub hourly: Option<Vec<HourlyForecast>>,
    pub summary: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

#[derive(Error, Debug)]
pub enum WeatherError {
    #[error("Неизвестное место '{0}'. Доступные места: Тбилиси, Батуми, Цихисдзири")]
    UnknownLocation(String),

    #[error("Неподдерживаемый запрос: {0}")]
    UnsupportedRequest(String),

    #[error("Погодный провайдер '{0}' не настроен")]
    ProviderNotConfigured(String),

    #[error("Таймаут получения данных о погоде (превышен лимит 3с)")]
    Timeout,

    #[error("Превышен лимит запросов к сервису погоды (429 Rate limited)")]
    RateLimited,

    #[error("Некорректный ответ сервиса погоды: {0}")]
    InvalidProviderData(String),

    #[error("Сервис погоды временно недоступен: {0}")]
    Unavailable(String),
}
