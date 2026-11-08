//! Spotify Soloist Controller for GPT Mini.
//!
//! Controls the local Spotify Soloist companion engine running on device.
//! Provides sub-100ms playback commands (`pause`, `resume`, `next`, `prev`, `volume`)
//! via the `soloist-ctl` tool and parses rich JSON playback status.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use tokio::process::Command;
use tracing::info;

#[derive(Debug, Clone)]
pub struct SoloistClient {
    ctl_path: PathBuf,
    #[allow(dead_code)]
    pub ws_addr: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PlaybackState {
    Playing,
    Paused,
    Stopped,
    Unknown(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CurrentTrackInfo {
    pub name: String,
    pub artist: String,
    pub album: Option<String>,
    pub uri: String,
    pub duration_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SoloistStatus {
    pub state: PlaybackState,
    pub track: Option<CurrentTrackInfo>,
    pub volume: u8,
    pub is_active: bool,
}

impl SoloistClient {
    pub fn new(ctl_path: impl Into<PathBuf>, ws_addr: impl Into<String>) -> Self {
        Self {
            ctl_path: ctl_path.into(),
            ws_addr: ws_addr.into(),
        }
    }

    /// Default client configuration for GPT Mini.
    #[allow(dead_code)]
    pub fn default_local() -> Self {
        let ctl = if Path::new("/data/opt/mini-assistant/soloist-ctl").exists() {
            PathBuf::from("/data/opt/mini-assistant/soloist-ctl")
        } else {
            PathBuf::from("/tmp/soloist-ctl")
        };
        Self::new(ctl, "127.0.0.1:9090")
    }

    /// Run a ctl command and return stdout.
    async fn run_ctl(&self, args: &[&str]) -> Result<String> {
        let mut cmd = Command::new(&self.ctl_path);
        cmd.args(args);
        cmd.stdin(Stdio::null());
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());

        let output = cmd
            .output()
            .await
            .with_context(|| format!("Failed to execute soloist ctl at {:?}", self.ctl_path))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!(
                "soloist ctl failed (exit code {:?}): {}",
                output.status.code(),
                stderr.trim()
            );
        }

        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    }

    /// Pause music playback immediately (critical for acoustic barge-in).
    pub async fn pause(&self) -> Result<()> {
        info!("Soloist: pausing playback");
        self.run_ctl(&["pause"]).await?;
        Ok(())
    }

    /// Resume music playback.
    pub async fn resume(&self) -> Result<()> {
        info!("Soloist: resuming playback");
        self.run_ctl(&["play"]).await?;
        Ok(())
    }

    /// Play a specific track/album/playlist URI.
    #[allow(dead_code)]
    pub async fn play_uri(&self, uri: &str) -> Result<()> {
        info!(uri = %uri, "Soloist: playing URI");
        self.run_ctl(&["play", uri]).await?;
        Ok(())
    }

    /// Skip to next track.
    pub async fn next(&self) -> Result<()> {
        info!("Soloist: skipping to next track");
        self.run_ctl(&["next"]).await?;
        Ok(())
    }

    /// Skip to previous track.
    pub async fn previous(&self) -> Result<()> {
        info!("Soloist: skipping to previous track");
        self.run_ctl(&["prev"]).await?;
        Ok(())
    }

    /// Set playback volume (0..100).
    pub async fn set_volume(&self, level: u8) -> Result<()> {
        let clamped = level.min(100);
        info!(volume = clamped, "Soloist: setting volume");
        self.run_ctl(&["volume", &clamped.to_string()]).await?;
        Ok(())
    }

    /// Query current playback status via `soloist ctl now --json`.
    pub async fn get_status(&self) -> Result<SoloistStatus> {
        let raw_json = self.run_ctl(&["now", "--json"]).await?;
        Self::parse_status_json(&raw_json)
    }

    /// Parse JSON payload returned by `soloist ctl now --json`.
    pub fn parse_status_json(raw_json: &str) -> Result<SoloistStatus> {
        let v: serde_json::Value = serde_json::from_str(raw_json)
            .with_context(|| format!("Failed to parse soloist JSON output: {raw_json}"))?;

        let status_str = v
            .get("status")
            .and_then(|s| s.as_str())
            .unwrap_or("unknown");
        let state = match status_str {
            "playing" => PlaybackState::Playing,
            "paused" => PlaybackState::Paused,
            "stopped" => PlaybackState::Stopped,
            other => PlaybackState::Unknown(other.to_string()),
        };

        let volume = v.get("volume").and_then(|n| n.as_u64()).unwrap_or(0) as u8;
        let is_active = v
            .get("is_active")
            .and_then(|b| b.as_bool())
            .unwrap_or(false);

        let track = v.get("item").and_then(|item| {
            let uri = item.get("uri")?.as_str()?.to_string();
            let name = item
                .pointer("/decorations/identity/name")
                .and_then(|n| n.as_str())
                .unwrap_or("Unknown")
                .to_string();
            let artist = item
                .pointer("/creators/0/entity/decorations/identity/name")
                .and_then(|a| a.as_str())
                .unwrap_or("Unknown")
                .to_string();
            let album = item
                .pointer("/parent/entity/decorations/identity/name")
                .and_then(|a| a.as_str())
                .map(|s| s.to_string());
            let duration_ms = item
                .pointer("/playback/duration_ms")
                .and_then(|d| d.as_u64())
                .unwrap_or(0);

            Some(CurrentTrackInfo {
                name,
                artist,
                album,
                uri,
                duration_ms,
            })
        });

        Ok(SoloistStatus {
            state,
            track,
            volume,
            is_active,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_status_json_paused() {
        let json = r#"{
            "type": "playback_state",
            "status": "paused",
            "item": {
                "uri": "spotify:track:0d8cGMosMcPJA8UsxQCtZq",
                "entity_type": "track",
                "decorations": {
                    "identity": {
                        "name": "You Spin Me Round (Like a Record)"
                    }
                },
                "parent": {
                    "entity": {
                        "decorations": {
                            "identity": {
                                "name": "That's The Way I Like It"
                            }
                        }
                    }
                },
                "creators": [
                    {
                        "entity": {
                            "decorations": {
                                "identity": {
                                    "name": "Dead Or Alive"
                                }
                            }
                        }
                    }
                ],
                "playback": {
                    "duration_ms": 195640
                }
            },
            "volume": 30,
            "is_active": true
        }"#;

        let status = SoloistClient::parse_status_json(json).unwrap();
        assert_eq!(status.state, PlaybackState::Paused);
        assert_eq!(status.volume, 30);
        assert!(status.is_active);

        let track = status.track.unwrap();
        assert_eq!(track.name, "You Spin Me Round (Like a Record)");
        assert_eq!(track.artist, "Dead Or Alive");
        assert_eq!(track.album.as_deref(), Some("That's The Way I Like It"));
        assert_eq!(track.uri, "spotify:track:0d8cGMosMcPJA8UsxQCtZq");
        assert_eq!(track.duration_ms, 195640);
    }

    #[test]
    fn test_parse_status_json_playing_no_track() {
        let json = r#"{
            "type": "playback_state",
            "status": "playing",
            "volume": 50,
            "is_active": false
        }"#;

        let status = SoloistClient::parse_status_json(json).unwrap();
        assert_eq!(status.state, PlaybackState::Playing);
        assert_eq!(status.volume, 50);
        assert!(!status.is_active);
        assert!(status.track.is_none());
    }
}
