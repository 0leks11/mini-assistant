use anyhow::{bail, ensure, Context, Result};
use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION, CONTENT_TYPE};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeSet;
use time::{format_description::well_known::Rfc3339, Duration, OffsetDateTime};
use tracing::info;

const MAX_RESPONSE_BYTES: usize = 1024 * 1024;
const MAX_HISTORY_HOURS: u32 = 168;

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
    base_url: reqwest::Url,
    client: reqwest::Client,
    allowed_entities: BTreeSet<String>,
}

fn valid_entity_id(entity_id: &str) -> bool {
    let Some((domain, name)) = entity_id.split_once('.') else {
        return false;
    };
    !domain.is_empty()
        && !name.is_empty()
        && domain.bytes().all(|b| b.is_ascii_lowercase() || b == b'_')
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

impl HomeAssistantClient {
    /// Build a bounded REST client with an explicit, non-empty entity allowlist.
    pub fn new(base_url: String, token: &str, allowed_entities: &[String]) -> Result<Self> {
        ensure!(!token.trim().is_empty(), "Home Assistant token is empty");
        ensure!(
            !allowed_entities.is_empty(),
            "HA tools require an explicit entity allowlist"
        );
        ensure!(
            allowed_entities.iter().all(|id| valid_entity_id(id)),
            "Invalid HA allowlist entity ID"
        );
        let base_url = reqwest::Url::parse(&format!("{}/", base_url.trim_end_matches('/')))
            .context("Invalid Home Assistant base URL")?;
        ensure!(
            matches!(base_url.scheme(), "http" | "https") && base_url.host_str().is_some(),
            "HA URL must be HTTP(S)"
        );
        ensure!(
            base_url.username().is_empty()
                && base_url.password().is_none()
                && base_url.query().is_none()
                && base_url.fragment().is_none(),
            "HA URL must not contain credentials, query or fragment"
        );
        let mut headers = HeaderMap::new();
        let mut auth_header = HeaderValue::from_str(&format!("Bearer {}", token.trim()))
            .context("Invalid characters in Home Assistant token")?;
        auth_header.set_sensitive(true);
        headers.insert(AUTHORIZATION, auth_header);
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        let client = reqwest::Client::builder()
            .default_headers(headers)
            .redirect(reqwest::redirect::Policy::none())
            .timeout(std::time::Duration::from_secs(5))
            .build()
            .context("Failed to build HTTP client for Home Assistant")?;
        Ok(Self {
            base_url,
            client,
            allowed_entities: allowed_entities.iter().cloned().collect(),
        })
    }

    async fn read_response<T: DeserializeOwned>(mut response: reqwest::Response) -> Result<T> {
        ensure!(
            response.status().is_success(),
            "HA returned error status: {}",
            response.status()
        );
        if let Some(length) = response.content_length() {
            ensure!(
                length <= MAX_RESPONSE_BYTES as u64,
                "HA response exceeds size limit"
            );
        }
        let mut body = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .context("Failed to read HA response")?
        {
            ensure!(
                chunk.len() <= MAX_RESPONSE_BYTES.saturating_sub(body.len()),
                "HA response exceeds size limit"
            );
            body.extend_from_slice(&chunk);
        }
        serde_json::from_slice(&body).context("Failed to parse HA JSON response")
    }

    /// Check that Home Assistant API is responsive and the token is authorized.
    pub async fn verify_auth(&self) -> Result<String> {
        let response = self
            .client
            .get(self.base_url.join("api/")?)
            .send()
            .await
            .context("Failed to connect to Home Assistant")?;
        ensure!(
            response.status().is_success(),
            "Home Assistant authentication rejected: HTTP {}",
            response.status()
        );
        #[derive(Deserialize)]
        struct ApiRootResponse {
            message: Option<String>,
        }
        let root: ApiRootResponse = Self::read_response(response).await?;
        Ok(root.message.unwrap_or_else(|| "API running.".to_string()))
    }

    /// Fetch states with a bounded response. Only allowlisted entities leave this client.
    pub async fn get_states(&self) -> Result<Vec<HaEntity>> {
        let response = self
            .client
            .get(self.base_url.join("api/states")?)
            .send()
            .await
            .context("Failed to connect to Home Assistant")?;
        let mut entities: Vec<HaEntity> = Self::read_response(response).await?;
        entities.retain(|entity| self.allowed_entities.contains(&entity.entity_id));
        Ok(entities)
    }

    /// Fetch the state of a single allowlisted entity. Returns Ok(None) if entity does not exist.
    pub async fn get_entity_state(&self, entity_id: &str) -> Result<Option<HaEntity>> {
        ensure!(
            valid_entity_id(entity_id) && self.allowed_entities.contains(entity_id),
            "HA entity is not allowed"
        );
        let response = self
            .client
            .get(self.base_url.join(&format!("api/states/{}", entity_id))?)
            .send()
            .await
            .context("Failed to connect to Home Assistant")?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let entity: HaEntity = Self::read_response(response).await?;
        Ok(Some(entity))
    }

    /// Format a roster, not an authorization policy. Authorization is enforced locally.
    pub fn format_device_summary(entities: &[HaEntity]) -> String {
        let mut lines = vec!["Разрешённые устройства Home Assistant:".to_owned()];
        for entity in entities {
            lines.push(format!(
                "- [{}] id: {} (домен: {}, текущее состояние: {})",
                entity.friendly_name(),
                entity.entity_id,
                entity.domain(),
                entity.state
            ));
        }
        lines.join("\n")
    }

    /// Fetch the allowlisted roster for model instructions without logging its content.
    pub async fn get_device_summary(&self) -> Result<String> {
        Ok(Self::format_device_summary(&self.get_states().await?))
    }

    fn validate_service(&self, domain: &str, service: &str, payload: &Value) -> Result<()> {
        let data = payload
            .as_object()
            .context("HA parameters must be an object")?;
        let entity_id = data
            .get("entity_id")
            .and_then(Value::as_str)
            .context("HA entity_id must be a string")?;
        ensure!(
            valid_entity_id(entity_id) && self.allowed_entities.contains(entity_id),
            "HA entity is not allowed"
        );
        ensure!(
            entity_id.split_once('.').map(|pair| pair.0) == Some(domain),
            "HA entity domain does not match service domain"
        );
        let optional: &[&str] = match (domain, service) {
            ("light", "turn_on") => &["brightness", "brightness_pct"],
            ("light" | "switch" | "climate", "turn_off") | ("switch" | "climate", "turn_on") => &[],
            ("climate", "set_temperature") => &["temperature"],
            ("climate", "set_hvac_mode") => &["hvac_mode"],
            _ => bail!("HA service is not allowed"),
        };
        ensure!(
            data.keys()
                .all(|key| key == "entity_id" || optional.contains(&key.as_str())),
            "HA parameter or target selector is not allowed"
        );
        if let Some(value) = data.get("brightness") {
            ensure!(
                value.as_u64().is_some_and(|n| n <= 255),
                "brightness must be an integer from 0 to 255"
            );
        }
        if let Some(value) = data.get("brightness_pct") {
            ensure!(
                value
                    .as_f64()
                    .is_some_and(|n| n.is_finite() && (0.0..=100.0).contains(&n)),
                "brightness_pct must be from 0 to 100"
            );
        }
        if service == "set_temperature" {
            ensure!(
                data.get("temperature")
                    .and_then(Value::as_f64)
                    .is_some_and(f64::is_finite),
                "temperature must be a finite number"
            );
        }
        if service == "set_hvac_mode" {
            let mode = data
                .get("hvac_mode")
                .and_then(Value::as_str)
                .context("hvac_mode must be a string")?;
            ensure!(
                [
                    "off",
                    "heat",
                    "cool",
                    "heat_cool",
                    "auto",
                    "dry",
                    "fan_only"
                ]
                .contains(&mode),
                "Unknown HVAC mode"
            );
        }
        Ok(())
    }

    /// Execute one allowlisted light/switch/climate action; no retries or broad targets.
    pub async fn call_service(&self, domain: &str, service: &str, payload: Value) -> Result<Value> {
        self.validate_service(domain, service, &payload)?;
        info!("Executing locally authorized HA action");
        let url = self
            .base_url
            .join(&format!("api/services/{domain}/{service}"))?;
        let response = self
            .client
            .post(url)
            .json(&payload)
            .send()
            .await
            .context("Failed to send service call to Home Assistant")?;
        // HA changed_states can include effects of unrelated automations. Do not
        // export those entities/attributes to the model after an allowed action.
        Self::read_response::<Value>(response).await?;
        Ok(serde_json::json!({ "accepted": true }))
    }

    fn history_url(
        &self,
        entity_id: &str,
        hours_ago: u32,
        now: OffsetDateTime,
    ) -> Result<reqwest::Url> {
        ensure!(
            valid_entity_id(entity_id) && self.allowed_entities.contains(entity_id),
            "HA entity is not allowed"
        );
        ensure!(
            (1..=MAX_HISTORY_HOURS).contains(&hours_ago),
            "History window must be from 1 to 168 hours"
        );
        let start = now
            .checked_sub(Duration::hours(i64::from(hours_ago)))
            .context("History start time is out of range")?
            .format(&Rfc3339)?;
        let end = now.format(&Rfc3339)?;
        let mut url = self.base_url.join(&format!("api/history/period/{start}"))?;
        url.query_pairs_mut()
            .append_pair("filter_entity_id", entity_id)
            .append_pair("end_time", &end)
            .append_pair("minimal_response", "1")
            .append_pair("no_attributes", "1");
        Ok(url)
    }

    /// Read at most seven days of allowlisted history, rejecting HTTP errors.
    pub async fn get_entity_history(&self, entity_id: &str, hours_ago: u32) -> Result<Value> {
        let url = self.history_url(entity_id, hours_ago, OffsetDateTime::now_utc())?;
        let response = self
            .client
            .get(url)
            .send()
            .await
            .context("Failed to fetch HA history")?;
        Self::read_response(response).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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
    fn test_format_device_summary_includes_provided_roster() {
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
            // The client filters by allowlist; formatting preserves supplied entities.
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

        // Read-only sensors are valid in an explicitly allowed roster.
        assert!(summary.contains("sensor.temperature_balcony"));
        assert!(summary.contains("automation.night_mode"));
    }
}

#[cfg(test)]
mod policy_and_http_tests {
    use super::*;
    use serde_json::json;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::thread;
    use std::time::{Duration as StdDuration, Instant};

    fn client(url: String) -> HomeAssistantClient {
        HomeAssistantClient::new(
            url,
            "offline-test-token",
            &[
                "light.test".to_owned(),
                "switch.test".to_owned(),
                "climate.test".to_owned(),
                "sensor.test".to_owned(),
            ],
        )
        .unwrap()
    }

    fn server(status: &str, body: &str) -> (String, thread::JoinHandle<String>) {
        let listener =
            TcpListener::bind("127.0.0.1:0").expect("local HTTP fixture needs loopback permission");
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let response = format!("HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
        let task = thread::spawn(move || {
            let deadline = Instant::now() + StdDuration::from_secs(6);
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(Instant::now() < deadline, "fixture accept timed out");
                        thread::sleep(StdDuration::from_millis(5));
                    }
                    Err(error) => panic!("fixture accept: {error}"),
                }
            };
            stream.set_nonblocking(false).unwrap();
            stream
                .set_read_timeout(Some(StdDuration::from_secs(3)))
                .unwrap();
            stream
                .set_write_timeout(Some(StdDuration::from_secs(3)))
                .unwrap();
            let mut request = Vec::new();
            let mut buf = [0; 4096];
            while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                let n = stream.read(&mut buf).unwrap();
                assert!(
                    n > 0 && request.len() + n <= 16384,
                    "invalid fixture request"
                );
                request.extend_from_slice(&buf[..n]);
            }
            let header = String::from_utf8(request).unwrap();
            stream.write_all(response.as_bytes()).unwrap();
            header
        });
        (url, task)
    }

    #[test]
    fn empty_policy_and_invalid_entities_fail_closed() {
        assert!(HomeAssistantClient::new("http://127.0.0.1:9".into(), "test", &[]).is_err());
        assert!(HomeAssistantClient::new(
            "http://127.0.0.1:9".into(),
            "test",
            &["light.a,b".into()]
        )
        .is_err());
        assert!(
            HomeAssistantClient::new("http://127.0.0.1:9".into(), "", &["light.test".into()])
                .is_err()
        );
    }

    #[test]
    fn service_policy_rejects_broad_targets_and_unrelated_services() {
        let client = client("http://127.0.0.1:9".into());
        for (domain, service, payload) in [
            ("lock", "unlock", json!({"entity_id":"lock.front_door"})),
            ("light", "turn_on", json!({"entity_id":"light.other"})),
            ("switch", "turn_off", json!({"entity_id":"light.test"})),
            (
                "light",
                "turn_on",
                json!({"entity_id":"light.test", "area_id":"all"}),
            ),
            (
                "light",
                "turn_on",
                json!({"entity_id":"light.test", "target":{"device_id":"other"}}),
            ),
            (
                "climate",
                "set_temperature",
                json!({"entity_id":"climate.test", "temperature":"hot"}),
            ),
            (
                "light",
                "turn_on",
                json!({"entity_id":"light.test", "brightness":300}),
            ),
            ("light", "turn_on", json!([])),
        ] {
            assert!(client.validate_service(domain, service, &payload).is_err());
        }
    }

    #[test]
    fn service_policy_accepts_typed_single_entity_commands() {
        let client = client("http://127.0.0.1:9".into());
        assert!(client
            .validate_service(
                "light",
                "turn_on",
                &json!({"entity_id":"light.test", "brightness":100})
            )
            .is_ok());
        assert!(client
            .validate_service(
                "climate",
                "set_temperature",
                &json!({"entity_id":"climate.test", "temperature":22.5})
            )
            .is_ok());
        assert!(client
            .validate_service(
                "climate",
                "set_hvac_mode",
                &json!({"entity_id":"climate.test", "hvac_mode":"cool"})
            )
            .is_ok());
        assert!(client
            .validate_service("switch", "turn_off", &json!({"entity_id":"switch.test"}))
            .is_ok());
    }

    #[test]
    fn history_window_is_iso8601_bounded_and_has_explicit_end() {
        let client = client("http://127.0.0.1:9".into());
        let now = OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap();
        let url = client.history_url("sensor.test", 48, now).unwrap();
        assert!(url.path().ends_with("2023-11-12T22:13:20Z"));
        let query: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();
        assert_eq!(query.get("end_time").unwrap(), "2023-11-14T22:13:20Z");
        assert_eq!(query.get("filter_entity_id").unwrap(), "sensor.test");
        assert!(client.history_url("sensor.other", 24, now).is_err());
        assert!(client.history_url("sensor.test", 0, now).is_err());
        assert!(client.history_url("sensor.test", 169, now).is_err());
    }

    #[tokio::test]
    async fn history_http_error_is_not_reported_as_success_or_echoed() {
        let (url, task) = server("400 Bad Request", r#"{"message":"private server data"}"#);
        let result = client(url).get_entity_history("sensor.test", 24).await;
        let request = task.join().unwrap();
        let error = result.unwrap_err().to_string();
        assert!(error.contains("400"));
        assert!(!error.contains("private server data"));
        assert!(request
            .lines()
            .next()
            .unwrap()
            .starts_with("GET /api/history/period/"));
    }

    #[tokio::test]
    async fn verify_auth_accepts_valid_token_and_rejects_unauthorized() {
        let (url, task) = server("200 OK", r#"{"message":"API running."}"#);
        let result = client(url).verify_auth().await;
        let request = task.join().unwrap();
        assert_eq!(result.unwrap(), "API running.");
        assert!(request.lines().next().unwrap().starts_with("GET /api/ "));
        assert!(request.contains("authorization: Bearer offline-test-token"));

        let (url_err, task_err) = server("401 Unauthorized", r#"{"message":"Unauthorized"}"#);
        let result_err = client(url_err).verify_auth().await;
        task_err.join().unwrap();
        assert!(result_err.is_err());
        assert!(result_err.unwrap_err().to_string().contains("401"));
    }

    #[tokio::test]
    async fn states_only_exposes_explicitly_allowed_entities() {
        let (url, task) = server(
            "200 OK",
            r#"[{"entity_id":"light.test","state":"on","attributes":{}},{"entity_id":"lock.front_door","state":"locked","attributes":{}}]"#,
        );
        let result = client(url).get_states().await;
        task.join().unwrap();
        let entities = result.unwrap();
        assert_eq!(entities.len(), 1);
        assert_eq!(entities[0].entity_id, "light.test");
    }

    #[tokio::test]
    async fn action_ack_does_not_export_unrelated_changed_states() {
        let (url, task) = server(
            "200 OK",
            r#"[{"entity_id":"light.test","state":"on","attributes":{}},{"entity_id":"lock.front_door","state":"unlocked","attributes":{"private":"foreign_data"},"context":{"user_id":"foreign_user"}}]"#,
        );
        let result = client(url)
            .call_service("light", "turn_on", json!({"entity_id":"light.test"}))
            .await;
        let request = task.join().unwrap();
        assert_eq!(result.unwrap(), json!({"accepted":true}));
        assert!(request
            .lines()
            .next()
            .unwrap()
            .starts_with("POST /api/services/light/turn_on "));
    }

    #[tokio::test]
    async fn forbidden_action_is_rejected_before_network() {
        let result = client("http://127.0.0.1:9".into())
            .call_service("lock", "unlock", json!({"entity_id":"lock.front_door"}))
            .await;
        assert_eq!(result.unwrap_err().to_string(), "HA entity is not allowed");
    }

    #[tokio::test]
    async fn get_entity_state_returns_allowed_entity_and_rejects_disallowed() {
        let (url, task) = server(
            "200 OK",
            r#"{"entity_id":"light.test","state":"off","attributes":{"friendly_name":"Test Light"}}"#,
        );
        let entity = client(url).get_entity_state("light.test").await.unwrap();
        task.join().unwrap();
        assert!(entity.is_some());
        let e = entity.unwrap();
        assert_eq!(e.entity_id, "light.test");
        assert_eq!(e.state, "off");
        assert_eq!(e.friendly_name(), "Test Light");

        let err = client("http://127.0.0.1:9".into())
            .get_entity_state("climate.disallowed")
            .await;
        assert!(err.is_err());
        assert_eq!(err.unwrap_err().to_string(), "HA entity is not allowed");
    }
}
