pub mod cache;
pub mod locations;
pub mod open_meteo;
pub mod types;

use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Mutex, Semaphore};
use tracing::{debug, info, warn};

pub use locations::{Location, LocationRegistry};
pub use open_meteo::OpenMeteoClient;
pub use types::{
    CurrentConditions, DailyForecast, GetWeatherRequest, GetWeatherResponse, HourlyForecast,
    WeatherError, WeatherGranularity, WeatherMetric, WeatherPeriod, WeatherUnits,
};

/// High-level Weather Service managing locations, dynamic geocoding, caching, bounded HTTP concurrency and providers.
pub struct WeatherService {
    registry: Arc<Mutex<LocationRegistry>>,
    cache: Arc<Mutex<cache::WeatherCache>>,
    open_meteo: OpenMeteoClient,
    concurrency_gate: Arc<Semaphore>,
}

impl Default for WeatherService {
    fn default() -> Self {
        Self::new(None)
    }
}

impl WeatherService {
    pub fn new(custom_base_url: Option<String>) -> Self {
        let open_meteo = match custom_base_url {
            Some(url) => OpenMeteoClient::new(url),
            None => OpenMeteoClient::default(),
        };

        Self {
            registry: Arc::new(Mutex::new(LocationRegistry::new())),
            cache: Arc::new(Mutex::new(cache::WeatherCache::default())),
            open_meteo,
            // Strict bounded concurrency: 1 outgoing HTTP request at a time
            concurrency_gate: Arc::new(Semaphore::new(1)),
        }
    }

    pub fn with_cache_ttl(
        custom_base_url: Option<String>,
        fresh_ttl: Duration,
        max_stale_ttl: Duration,
    ) -> Self {
        let open_meteo = match custom_base_url {
            Some(url) => OpenMeteoClient::new(url),
            None => OpenMeteoClient::default(),
        };

        Self {
            registry: Arc::new(Mutex::new(LocationRegistry::new())),
            cache: Arc::new(Mutex::new(cache::WeatherCache::new(
                fresh_ttl,
                max_stale_ttl,
            ))),
            open_meteo,
            concurrency_gate: Arc::new(Semaphore::new(1)),
        }
    }

    pub async fn resolve_location(&self, query: &str) -> Option<Location> {
        let reg = self.registry.lock().await;
        reg.resolve(query).cloned()
    }

    pub async fn list_locations(&self) -> Vec<Location> {
        let reg = self.registry.lock().await;
        reg.list().to_vec()
    }

    /// Query weather for a requested location and period.
    /// Resolves location from registry or dynamic Open-Meteo geocoding.
    /// Uses cache when fresh; falls back to stale cache on network error;
    /// Bounded by 3-second HTTP timeout and strict 1-concurrency semaphore.
    pub async fn get_weather(
        &self,
        req: GetWeatherRequest,
    ) -> Result<GetWeatherResponse, WeatherError> {
        // 1. Resolve location from local registry or dynamic geocoding
        let existing = {
            let reg = self.registry.lock().await;
            reg.resolve(&req.location_id).cloned()
        };

        let location = match existing {
            Some(loc) => loc,
            None => {
                debug!(
                    "Location '{}' not in registry, performing dynamic geocoding...",
                    req.location_id
                );
                let geocoded = self.open_meteo.geocode(&req.location_id).await?;
                let mut reg = self.registry.lock().await;
                reg.register(geocoded.clone());
                info!(
                    "Geocoded '{}' to {} ({:.4}, {:.4})",
                    req.location_id, geocoded.name, geocoded.latitude, geocoded.longitude
                );
                geocoded
            }
        };

        let cache_key = cache::WeatherCache::make_key("open-meteo", &location.id);

        // 2. Check RAM cache
        {
            let cache_guard = self.cache.lock().await;
            if let Some((cached, is_fresh)) = cache_guard.get(&cache_key) {
                if is_fresh {
                    debug!("Weather cache HIT (fresh) for {}", location.name);
                    return Ok(cached);
                }
            }
        }

        // 3. Acquire concurrency permit (max 1 active fetch)
        let _permit = self.concurrency_gate.acquire().await.map_err(|_| {
            WeatherError::Unavailable("Weather concurrency gate closed".to_string())
        })?;

        // Double check cache after acquiring permit
        {
            let cache_guard = self.cache.lock().await;
            if let Some((cached, is_fresh)) = cache_guard.get(&cache_key) {
                if is_fresh {
                    return Ok(cached);
                }
            }
        }

        // 4. Fetch from provider
        info!(
            "Fetching fresh weather for {} from Open-Meteo",
            location.name
        );
        match self.open_meteo.fetch(&location, &req).await {
            Ok(fresh_resp) => {
                let mut cache_guard = self.cache.lock().await;
                cache_guard.insert(cache_key, fresh_resp.clone());
                Ok(fresh_resp)
            }
            Err(e) => {
                warn!("Weather provider fetch error: {e}. Checking stale cache...");
                let cache_guard = self.cache.lock().await;
                if let Some((stale_resp, _)) = cache_guard.get(&cache_key) {
                    info!("Serving stale cached weather for {}", location.name);
                    let mut resp = stale_resp;
                    resp.message = Some(format!(
                        "Внимание: данные получены из кеша ({}), свежее обновление недоступно.",
                        resp.cache_state
                    ));
                    return Ok(resp);
                }
                Err(e)
            }
        }
    }

    /// Gemini Live tool declaration for `get_weather`
    pub fn tool_declaration() -> serde_json::Value {
        serde_json::json!({
            "name": "get_weather",
            "description": "Получить актуальный прогноз погоды для любого города или населенного пункта мира (например: Батуми, Тбилиси, Москва, Париж, Кутаиси и др.).",
            "parameters": {
                "type": "OBJECT",
                "properties": {
                    "location_id": {
                        "type": "STRING",
                        "description": "Название города или населенного пункта на русском или английском (например: 'Батуми', 'Москва', 'Кутаиси', 'Тбилиси', 'Париж')"
                    },
                    "period": {
                        "type": "STRING",
                        "enum": ["now", "today", "tomorrow", "next_7_days", "this_weekend"],
                        "description": "Период прогноза: 'now' (сейчас), 'today' (сегодня), 'tomorrow' (завтра), 'next_7_days' (на 7 дней), 'this_weekend' (на выходные)"
                    },
                    "granularity": {
                        "type": "STRING",
                        "enum": ["current", "hourly", "daily"],
                        "description": "Детализация: 'current' (текущие условия), 'hourly' (по часам), 'daily' (по дням)"
                    }
                },
                "required": ["location_id"]
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_weather_service_resolve_location() {
        let service = WeatherService::default();
        assert_eq!(
            service.resolve_location("Батуми").await.map(|l| l.id),
            Some("batumi".to_string())
        );
        assert_eq!(
            service.resolve_location("tbilisi").await.map(|l| l.id),
            Some("tbilisi".to_string())
        );
        assert_eq!(
            service.resolve_location("sihidzyri").await.map(|l| l.id),
            Some("tsikhisdziri".to_string())
        );
        assert!(service.resolve_location("unknown_city").await.is_none());
    }

    #[tokio::test]
    async fn test_weather_unknown_location_fails_fast() {
        let (url, _handle) = mock_weather_server("200 OK", r#"{"results": []}"#);
        let service = WeatherService::new(Some(url));
        let req = GetWeatherRequest {
            location_id: "unknown_city".to_string(),
            period: WeatherPeriod::Now,
            granularity: WeatherGranularity::Current,
            metrics: vec![],
            provider: None,
        };
        let err = service.get_weather(req).await.unwrap_err();
        match err {
            WeatherError::UnknownLocation(loc) => assert_eq!(loc, "unknown_city"),
            other => panic!("Unexpected error: {:?}", other),
        }
    }

    #[test]
    fn test_weather_tool_declaration_schema() {
        let decl = WeatherService::tool_declaration();
        assert_eq!(decl["name"], "get_weather");
        assert!(decl["parameters"]["properties"]["location_id"].is_object());
        assert!(decl["parameters"]["required"]
            .as_array()
            .unwrap()
            .contains(&serde_json::json!("location_id")));
    }

    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::thread;

    fn mock_weather_server(status: &str, body: &str) -> (String, thread::JoinHandle<()>) {
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
        });

        (url, task)
    }

    #[tokio::test]
    async fn test_service_caching_and_stale_fallback() {
        let sample_json = r#"{
            "current": {
                "temperature_2m": 22.0,
                "apparent_temperature": 21.5,
                "relative_humidity_2m": 60,
                "cloud_cover": 10,
                "wind_speed_10m": 3.0,
                "wind_direction_10m": 120,
                "precipitation": 0.0,
                "weather_code": 0
            },
            "daily": {
                "time": ["2026-11-03"],
                "weather_code": [0],
                "temperature_2m_max": [24.0],
                "temperature_2m_min": [16.0]
            }
        }"#;

        // Start server for first request (50ms fresh TTL, 2000ms stale TTL)
        let (url, handle) = mock_weather_server("200 OK", sample_json);
        let service = WeatherService::with_cache_ttl(
            Some(url),
            Duration::from_millis(50),
            Duration::from_millis(2000),
        );

        let req = GetWeatherRequest {
            location_id: "batumi".to_string(),
            period: WeatherPeriod::Now,
            granularity: WeatherGranularity::Current,
            metrics: vec![],
            provider: None,
        };

        let resp = service.get_weather(req.clone()).await.unwrap();
        assert_eq!(resp.current.unwrap().temperature, 22.0);
        handle.join().unwrap();

        // Second immediate request should hit fresh cache without server
        let resp2 = service.get_weather(req.clone()).await.unwrap();
        assert_eq!(resp2.current.unwrap().temperature, 22.0);

        // Sleep to make cache stale
        tokio::time::sleep(Duration::from_millis(60)).await;

        // Third request fails to connect to dead mock server, but returns stale cache with warning
        let stale_resp = service.get_weather(req).await.unwrap();
        assert_eq!(stale_resp.current.unwrap().temperature, 22.0);
        assert!(stale_resp
            .message
            .unwrap()
            .contains("данные получены из кеша"));
    }
}
