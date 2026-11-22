//! Calibration Sessions and Crash Recovery for GPT Mini Acoustics.
//!
//! Implements bounded sessions, durable baseline recovery record,
//! owner live confirmation state, and candidate inspection.

use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::acoustics::{AcousticSettingsDto, WakewordCalibrationConfig};

pub const RECOVERY_FILENAME: &str = "recovery_session.json";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionState {
    Created,
    Running,
    Evaluating,
    AwaitingOwner,
    Completed,
    Cancelled,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionBounds {
    pub max_duration_secs: u64,
    pub max_jobs: usize,
    pub require_aux: bool,
}

impl Default for SessionBounds {
    fn default() -> Self {
        Self {
            max_duration_secs: 1800,
            max_jobs: 20,
            require_aux: false,
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct CreateSessionDto {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub max_duration_secs: Option<u64>,
    #[serde(default)]
    pub max_jobs: Option<usize>,
    #[serde(default)]
    pub require_aux: Option<bool>,
    #[serde(default)]
    pub notes: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AcousticSession {
    pub session_id: String,
    pub name: String,
    pub state: SessionState,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
    pub baseline_revision: u64,
    pub baseline_config: WakewordCalibrationConfig,
    pub active_candidate: Option<AcousticSettingsDto>,
    pub candidate_revision: Option<u64>,
    pub bounds: SessionBounds,
    pub jobs: Vec<String>,
    pub reports: Vec<String>,
    pub owner_confirmed: bool,
    pub notes: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecoveryRecord {
    pub session_id: String,
    pub baseline_revision: u64,
    pub baseline_config: WakewordCalibrationConfig,
    pub created_at_ms: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error("Another acoustic session is already active ({0})")]
    Conflict(String),
    #[error("Session '{0}' not found")]
    NotFound(String),
    #[error("Session is not running (current state: {0:?})")]
    InvalidState(SessionState),
    #[error("Session resource limit exceeded: {0}")]
    LimitExceeded(String),
    #[error("Internal storage error: {0}")]
    Storage(String),
}

pub struct AcousticSessionManager {
    recovery_file: PathBuf,
    active_session: Arc<Mutex<Option<AcousticSession>>>,
}

impl AcousticSessionManager {
    pub fn new(storage_dir: &Path) -> Result<Self> {
        fs::create_dir_all(storage_dir)
            .with_context(|| format!("Failed to create session dir {}", storage_dir.display()))?;
        let recovery_file = storage_dir.join(RECOVERY_FILENAME);
        Ok(Self {
            recovery_file,
            active_session: Arc::new(Mutex::new(None)),
        })
    }

    /// Checks if an unrecovered session baseline exists from a previous crash/power loss.
    pub fn check_recovery_on_startup(&self) -> Option<RecoveryRecord> {
        if !self.recovery_file.is_file() {
            return None;
        }
        let data = fs::read(&self.recovery_file).ok()?;
        serde_json::from_slice::<RecoveryRecord>(&data).ok()
    }

    /// Cleans up the recovery record file.
    pub fn clear_recovery(&self) -> Result<()> {
        if self.recovery_file.exists() {
            fs::remove_file(&self.recovery_file)
                .with_context(|| "Failed to remove recovery record file")?;
        }
        Ok(())
    }

    /// Writes durable recovery record before any candidate mutation.
    fn write_recovery(&self, rec: &RecoveryRecord) -> Result<()> {
        let json = serde_json::to_vec_pretty(rec)?;
        let tmp = self.recovery_file.with_extension("tmp");
        {
            let mut f = File::create(&tmp)?;
            f.write_all(&json)?;
            f.sync_all()?;
        }
        fs::rename(&tmp, &self.recovery_file)?;
        Ok(())
    }

    pub fn create_session(
        &self,
        baseline_cfg: WakewordCalibrationConfig,
        baseline_rev: u64,
        dto: CreateSessionDto,
    ) -> Result<AcousticSession, SessionError> {
        let mut guard = self.active_session.lock().unwrap();
        if let Some(ref current) = *guard {
            if matches!(
                current.state,
                SessionState::Running | SessionState::Created | SessionState::AwaitingOwner
            ) {
                return Err(SessionError::Conflict(current.session_id.clone()));
            }
        }

        let now_ms = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);

        let session_id = format!("sess_{}", uuid::Uuid::new_v4().simple());
        let name = dto
            .name
            .filter(|n| !n.trim().is_empty())
            .unwrap_or_else(|| format!("Acoustic Session {}", &session_id[5..13]));

        let bounds = SessionBounds {
            max_duration_secs: dto.max_duration_secs.unwrap_or(1800).clamp(60, 7200),
            max_jobs: dto.max_jobs.unwrap_or(20).clamp(1, 50),
            require_aux: dto.require_aux.unwrap_or(false),
        };

        let recovery = RecoveryRecord {
            session_id: session_id.clone(),
            baseline_revision: baseline_rev,
            baseline_config: baseline_cfg.clone(),
            created_at_ms: now_ms,
        };

        self.write_recovery(&recovery)
            .map_err(|e| SessionError::Storage(e.to_string()))?;

        let session = AcousticSession {
            session_id,
            name,
            state: SessionState::Running,
            created_at_ms: now_ms,
            updated_at_ms: now_ms,
            baseline_revision: baseline_rev,
            baseline_config: baseline_cfg,
            active_candidate: None,
            candidate_revision: None,
            bounds,
            jobs: Vec::new(),
            reports: Vec::new(),
            owner_confirmed: false,
            notes: dto.notes,
        };

        *guard = Some(session.clone());
        Ok(session)
    }

    pub fn get_session(&self, session_id: Option<&str>) -> Option<AcousticSession> {
        let guard = self.active_session.lock().unwrap();
        match (*guard).as_ref() {
            Some(s) => {
                if let Some(id) = session_id {
                    if s.session_id == id {
                        Some(s.clone())
                    } else {
                        None
                    }
                } else {
                    Some(s.clone())
                }
            }
            None => None,
        }
    }

    pub fn update_candidate(
        &self,
        session_id: &str,
        candidate: AcousticSettingsDto,
        rev: u64,
    ) -> Result<(), SessionError> {
        let mut guard = self.active_session.lock().unwrap();
        let s = guard
            .as_mut()
            .ok_or_else(|| SessionError::NotFound(session_id.to_string()))?;
        if s.session_id != session_id {
            return Err(SessionError::NotFound(session_id.to_string()));
        }
        if !matches!(
            s.state,
            SessionState::Running | SessionState::AwaitingOwner | SessionState::Evaluating
        ) {
            return Err(SessionError::InvalidState(s.state));
        }

        s.active_candidate = Some(candidate);
        s.candidate_revision = Some(rev);
        s.updated_at_ms = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        s.state = SessionState::AwaitingOwner;
        Ok(())
    }

    pub fn restore_baseline(&self, session_id: &str) -> Result<(), SessionError> {
        let mut guard = self.active_session.lock().unwrap();
        let s = guard
            .as_mut()
            .ok_or_else(|| SessionError::NotFound(session_id.to_string()))?;
        if s.session_id != session_id {
            return Err(SessionError::NotFound(session_id.to_string()));
        }
        s.active_candidate = None;
        s.candidate_revision = None;
        s.updated_at_ms = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        Ok(())
    }

    pub fn record_job(&self, session_id: &str, job_id: &str) -> Result<(), SessionError> {
        let mut guard = self.active_session.lock().unwrap();
        let s = guard
            .as_mut()
            .ok_or_else(|| SessionError::NotFound(session_id.to_string()))?;
        if s.session_id != session_id {
            return Err(SessionError::NotFound(session_id.to_string()));
        }
        if s.jobs.len() >= s.bounds.max_jobs {
            return Err(SessionError::LimitExceeded(format!(
                "Max jobs ({}) exceeded for this session",
                s.bounds.max_jobs
            )));
        }
        s.jobs.push(job_id.to_string());
        s.updated_at_ms = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        Ok(())
    }

    pub fn confirm_owner(
        &self,
        session_id: &str,
        notes: Option<String>,
    ) -> Result<(), SessionError> {
        let mut guard = self.active_session.lock().unwrap();
        let s = guard
            .as_mut()
            .ok_or_else(|| SessionError::NotFound(session_id.to_string()))?;
        if s.session_id != session_id {
            return Err(SessionError::NotFound(session_id.to_string()));
        }
        s.owner_confirmed = true;
        if let Some(n) = notes {
            s.notes = Some(n);
        }
        s.updated_at_ms = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        Ok(())
    }

    pub fn finish_session(
        &self,
        session_id: &str,
        owner_confirmed: bool,
        notes: Option<String>,
    ) -> Result<AcousticSession, SessionError> {
        let mut guard = self.active_session.lock().unwrap();
        let s = guard
            .as_mut()
            .ok_or_else(|| SessionError::NotFound(session_id.to_string()))?;
        if s.session_id != session_id {
            return Err(SessionError::NotFound(session_id.to_string()));
        }

        s.owner_confirmed = s.owner_confirmed || owner_confirmed;
        if let Some(n) = notes {
            s.notes = Some(n);
        }
        s.state = SessionState::Completed;
        s.updated_at_ms = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);

        self.clear_recovery()
            .map_err(|e| SessionError::Storage(e.to_string()))?;

        Ok(s.clone())
    }

    pub fn cancel_session(&self, session_id: &str) -> Result<AcousticSession, SessionError> {
        let mut guard = self.active_session.lock().unwrap();
        let s = guard
            .as_mut()
            .ok_or_else(|| SessionError::NotFound(session_id.to_string()))?;
        if s.session_id != session_id {
            return Err(SessionError::NotFound(session_id.to_string()));
        }

        s.state = SessionState::Cancelled;
        s.updated_at_ms = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);

        self.clear_recovery()
            .map_err(|e| SessionError::Storage(e.to_string()))?;

        Ok(s.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_session_lifecycle_and_recovery_persistence() {
        let temp_dir = std::env::temp_dir().join(format!("test_sess_{}", uuid::Uuid::new_v4()));
        let manager = AcousticSessionManager::new(&temp_dir).unwrap();

        // 1. Initially no recovery record
        assert!(manager.check_recovery_on_startup().is_none());

        // 2. Create session
        let baseline = WakewordCalibrationConfig::default();
        let session = manager
            .create_session(
                baseline.clone(),
                1,
                CreateSessionDto {
                    name: Some("Test Living Room".into()),
                    max_duration_secs: Some(600),
                    max_jobs: Some(5),
                    require_aux: Some(false),
                    notes: None,
                },
            )
            .unwrap();

        assert_eq!(session.name, "Test Living Room");
        assert_eq!(session.state, SessionState::Running);

        // 3. Recovery record exists
        let rec = manager
            .check_recovery_on_startup()
            .expect("Recovery record written");
        assert_eq!(rec.session_id, session.session_id);
        assert_eq!(rec.baseline_revision, 1);

        // 4. Concurrent create fails with Conflict
        let conflict_err = manager.create_session(baseline, 1, CreateSessionDto::default());
        assert!(conflict_err.is_err());

        // 5. Update candidate & confirm owner
        let mut dto = AcousticSettingsDto::from(WakewordCalibrationConfig::default());
        dto.threshold = 0.55;
        manager
            .update_candidate(&session.session_id, dto, 2)
            .unwrap();
        let active = manager.get_session(Some(&session.session_id)).unwrap();
        assert_eq!(active.state, SessionState::AwaitingOwner);
        assert_eq!(active.candidate_revision, Some(2));

        manager
            .confirm_owner(&session.session_id, Some("Owner heard test".into()))
            .unwrap();
        let confirmed = manager.get_session(Some(&session.session_id)).unwrap();
        assert!(confirmed.owner_confirmed);

        // 6. Finish session removes recovery record
        let finished = manager
            .finish_session(&session.session_id, true, None)
            .unwrap();
        assert_eq!(finished.state, SessionState::Completed);
        assert!(manager.check_recovery_on_startup().is_none());

        let _ = fs::remove_dir_all(&temp_dir);
    }
}
