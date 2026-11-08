use std::collections::VecDeque;
use std::time::{Duration, Instant};

use super::types::GetWeatherResponse;

const MAX_CACHE_ENTRIES: usize = 8;
const DEFAULT_FRESH_TTL: Duration = Duration::from_secs(60 * 60); // 60 minutes
const MAX_STALE_TTL: Duration = Duration::from_secs(6 * 60 * 60); // 6 hours

#[derive(Debug, Clone)]
struct CacheEntry {
    key: String,
    created_at: Instant,
    response: GetWeatherResponse,
}

pub struct WeatherCache {
    entries: VecDeque<CacheEntry>,
    fresh_ttl: Duration,
    max_stale_ttl: Duration,
}

impl Default for WeatherCache {
    fn default() -> Self {
        Self::new(DEFAULT_FRESH_TTL, MAX_STALE_TTL)
    }
}

impl WeatherCache {
    pub fn new(fresh_ttl: Duration, max_stale_ttl: Duration) -> Self {
        Self {
            entries: VecDeque::with_capacity(MAX_CACHE_ENTRIES),
            fresh_ttl,
            max_stale_ttl,
        }
    }

    pub fn make_key(provider: &str, location_id: &str) -> String {
        format!("{provider}:{location_id}")
    }

    /// Retrieve entry if within max_stale_ttl.
    /// Returns (response, is_fresh) or None.
    pub fn get(&self, key: &str) -> Option<(GetWeatherResponse, bool)> {
        let entry = self.entries.iter().find(|e| e.key == key)?;
        let elapsed = entry.created_at.elapsed();
        if elapsed > self.max_stale_ttl {
            return None;
        }

        let is_fresh = elapsed <= self.fresh_ttl;
        let mut resp = entry.response.clone();
        resp.cache_state = if is_fresh {
            "fresh".to_string()
        } else {
            "stale".to_string()
        };
        resp.age_seconds = elapsed.as_secs();
        Some((resp, is_fresh))
    }

    /// Insert or update an entry, respecting max capacity
    pub fn insert(&mut self, key: String, mut response: GetWeatherResponse) {
        response.cache_state = "fresh".to_string();
        response.age_seconds = 0;

        // If key already exists, update it
        if let Some(pos) = self.entries.iter().position(|e| e.key == key) {
            self.entries.remove(pos);
        }

        // Evict oldest if capacity reached
        while self.entries.len() >= MAX_CACHE_ENTRIES {
            self.entries.pop_front();
        }

        self.entries.push_back(CacheEntry {
            key,
            created_at: Instant::now(),
            response,
        });
    }

    pub fn clear(&mut self) {
        self.entries.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::weather::types::WeatherUnits;

    fn sample_response(location: &str) -> GetWeatherResponse {
        GetWeatherResponse {
            status: "ok".to_string(),
            provider: "open-meteo".to_string(),
            location_id: location.to_string(),
            location_name: location.to_string(),
            timezone: "Asia/Tbilisi".to_string(),
            units: WeatherUnits::default(),
            fetched_at_utc: "2026-11-03T12:00:00Z".to_string(),
            cache_state: "fresh".to_string(),
            age_seconds: 0,
            current: None,
            daily: None,
            hourly: None,
            summary: "Тест".to_string(),
            message: None,
        }
    }

    #[test]
    fn test_cache_fresh_and_stale() {
        // 50ms fresh TTL, 200ms stale TTL
        let mut cache = WeatherCache::new(Duration::from_millis(50), Duration::from_millis(200));
        let key = WeatherCache::make_key("open-meteo", "batumi");
        cache.insert(key.clone(), sample_response("batumi"));

        // Immediately fresh
        let (resp, is_fresh) = cache.get(&key).expect("entry must exist");
        assert!(is_fresh);
        assert_eq!(resp.cache_state, "fresh");

        // Sleep 70ms -> stale
        std::thread::sleep(Duration::from_millis(70));
        let (resp, is_fresh) = cache.get(&key).expect("entry must exist");
        assert!(!is_fresh);
        assert_eq!(resp.cache_state, "stale");

        // Sleep 150ms -> expired
        std::thread::sleep(Duration::from_millis(150));
        assert!(cache.get(&key).is_none());
    }

    #[test]
    fn test_cache_capacity_eviction() {
        let mut cache = WeatherCache::default();
        for i in 0..10 {
            let key = format!("provider:loc{i}");
            cache.insert(key, sample_response(&format!("loc{i}")));
        }
        // First 2 must have been evicted, only last 8 present
        assert_eq!(cache.entries.len(), MAX_CACHE_ENTRIES);
        assert!(cache.get("provider:loc0").is_none());
        assert!(cache.get("provider:loc1").is_none());
        assert!(cache.get("provider:loc9").is_some());
    }
}
