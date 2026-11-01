use anyhow::{Context, Result};
use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION, CONTENT_TYPE};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tracing::info;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HaEntity {
    pub entity_id: String,
    pub state: String,
    pub attributes: Value,
}

impl HaEntity {
    pub fn friendly_name(&self) -> String {
        self.attributes
            .get("friendly_name")
            .and_then(|v| v.as_str())
            .unwrap_or(&self.entity_id)
            .to_string()
    }

    pub fn domain(&self) -> &str {
        self.entity_id.split('.').next().unwrap_or("")
    }
}

pub struct HomeAssistantClient {
    base_url: String,
    client: reqwest::Client,
}

impl HomeAssistantClient {
    pub fn new(base_url: String, token: &str) -> Result<Self> {
        let mut headers = HeaderMap::new();
        let auth_val = format!("Bearer {}", token);
        let mut auth_header = HeaderValue::from_str(&auth_val)
            .context("Invalid characters in Home Assistant token")?;
        auth_header.set_sensitive(true);

        headers.insert(AUTHORIZATION, auth_header);
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));

        let client = reqwest::Client::builder()
            .default_headers(headers)
            .timeout(std::time::Duration::from_secs(5))
            .build()
            .context("Failed to build HTTP client for Home Assistant")?;

        Ok(Self { base_url, client })
    }

    /// Fetch all device states from Home Assistant (/api/states)
    pub async fn get_states(&self) -> Result<Vec<HaEntity>> {
        let url = format!("{}/api/states", self.base_url);
        let resp = self
            .client
            .get(&url)
            .send()
            .await
            .context("Failed to connect to Home Assistant")?;

        if !resp.status().is_success() {
            anyhow::bail!("HA returned error status: {}", resp.status());
        }

        let entities: Vec<HaEntity> = resp
            .json()
            .await
            .context("Failed to parse HA entities JSON")?;

        Ok(entities)
    }

    /// Formats manageable devices (climate, light, switch, media_player, vacuum) into a concise
    /// text summary for Gemini's system instructions, so it knows all rooms & devices dynamically.
    pub fn format_device_summary(entities: &[HaEntity]) -> String {
        let mut lines = Vec::new();
        lines.push("Доступные устройства в умном доме (Home Assistant):".to_string());

        for e in entities {
            let domain = e.domain();
            // Filter to actionable domains
            if matches!(
                domain,
                "climate"
                    | "light"
                    | "switch"
                    | "media_player"
                    | "vacuum"
                    | "cover"
                    | "fan"
                    | "water_heater"
            ) {
                let name = e.friendly_name();
                lines.push(format!(
                    "- [{}] id: {} (текущее состояние: {})",
                    name, e.entity_id, e.state
                ));
            }
        }

        lines.join("\n")
    }

    /// Fetch device states and format them into a concise text summary
    pub async fn get_device_summary(&self) -> Result<String> {
        let entities = self.get_states().await?;
        Ok(Self::format_device_summary(&entities))
    }

    /// Call an action/service in Home Assistant:
    /// e.g. domain="climate", service="set_temperature", payload={"entity_id": "climate.midea", "temperature": 22}
    pub async fn call_service(&self, domain: &str, service: &str, payload: Value) -> Result<Value> {
        let url = format!("{}/api/services/{}/{}", self.base_url, domain, service);
        info!(
            "HA Service Call: {}/{} with payload: {:?}",
            domain, service, payload
        );

        let resp = self
            .client
            .post(&url)
            .json(&payload)
            .send()
            .await
            .context("Failed to send service call to Home Assistant")?;

        let status = resp.status();
        let body: Value = resp
            .json()
            .await
            .unwrap_or_else(|_| json!({"status": status.as_u16()}));

        if !status.is_success() {
            anyhow::bail!("HA service call failed ({}): {:?}", status, body);
        }

        Ok(body)
    }

    /// Query history for an entity over the past N hours
    pub async fn get_entity_history(&self, entity_id: &str, hours_ago: u32) -> Result<Value> {
        use std::time::{SystemTime, UNIX_EPOCH};

        let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
        let start_time = now.saturating_sub((hours_ago as u64) * 3600);

        // Format ISO8601 or timestamp
        let url = format!(
            "{}/api/history/period/{}?filter_entity_id={}",
            self.base_url, start_time, entity_id
        );

        let resp = self.client.get(&url).send().await?;
        let history: Value = resp.json().await?;
        Ok(history)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ha_entity_domain_and_friendly_name() {
        let entity = HaEntity {
            entity_id: "climate.midea_ac".to_string(),
            state: "cool".to_string(),
            attributes: json!({
                "friendly_name": "Кондиционер в спальне",
                "temperature": 22
            }),
        };

        assert_eq!(entity.domain(), "climate");
        assert_eq!(entity.friendly_name(), "Кондиционер в спальне");
    }

    #[test]
    fn test_ha_entity_friendly_name_fallback() {
        let entity = HaEntity {
            entity_id: "light.kitchen_strip".to_string(),
            state: "on".to_string(),
            attributes: json!({}),
        };

        assert_eq!(entity.domain(), "light");
        assert_eq!(entity.friendly_name(), "light.kitchen_strip");
    }

    #[test]
    fn test_format_device_summary_filtering() {
        let entities = vec![
            HaEntity {
                entity_id: "light.living_room".to_string(),
                state: "on".to_string(),
                attributes: json!({ "friendly_name": "Люстра в гостиной" }),
            },
            HaEntity {
                entity_id: "climate.ac".to_string(),
                state: "heat".to_string(),
                attributes: json!({ "friendly_name": "Сплит" }),
            },
            // Sensor and automation should be filtered out
            HaEntity {
                entity_id: "sensor.temperature_balcony".to_string(),
                state: "18.5".to_string(),
                attributes: json!({ "friendly_name": "Датчик температуры" }),
            },
            HaEntity {
                entity_id: "automation.night_mode".to_string(),
                state: "on".to_string(),
                attributes: json!({ "friendly_name": "Ночной режим" }),
            },
        ];

        let summary = HomeAssistantClient::format_device_summary(&entities);
        assert!(summary.contains("Люстра в гостиной"));
        assert!(summary.contains("light.living_room"));
        assert!(summary.contains("Сплит"));
        assert!(summary.contains("climate.ac"));

        // Must not contain filtered entities
        assert!(!summary.contains("sensor.temperature_balcony"));
        assert!(!summary.contains("automation.night_mode"));
    }
}
