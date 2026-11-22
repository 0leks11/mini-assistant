//! Room profiles and candidate management for acoustic calibration.
//!
//! Provides durable, versioned storage for room-specific acoustic profiles
//! (e.g., "Гостиная с эхом", "Спальня с коврами"), handling the full lifecycle
//! (draft -> automated_tested -> owner_confirmed), idempotency keys, and
//! atomic cross-process persistence.

use crate::acoustics::AcousticSettingsDto;
use anyhow::{bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{ErrorKind, Read, Write};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

pub const PROFILES_STORAGE_VERSION: u32 = 1;
pub const DEFAULT_PROFILES_FILENAME: &str = "profiles.json";
pub const MAX_PROFILES_SNAPSHOT_BYTES: u64 = 2 * 1024 * 1024; // 2 MB
pub const DEFAULT_PIPELINE_VERSION: &str = "v1-a113x-alsa";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProfileStatus {
    Draft,
    AutomatedTested,
    OwnerConfirmed,
}

impl std::fmt::Display for ProfileStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProfileStatus::Draft => write!(f, "draft"),
            ProfileStatus::AutomatedTested => write!(f, "automated_tested"),
            ProfileStatus::OwnerConfirmed => write!(f, "owner_confirmed"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AcousticProfile {
    pub id: String,
    pub schema_version: u32,
    pub revision: u64,
    pub name: String,
    pub notes: Option<String>,
    pub settings: AcousticSettingsDto,
    pub model_id: Option<String>,
    pub model_hash: Option<String>,
    pub audio_pipeline_version: String,
    pub conditions: Option<String>,
    pub status: ProfileStatus,
    pub created_at: String,
    pub updated_at: String,
    pub archived: bool,
    #[serde(default)]
    pub report_ids: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateProfileDto {
    pub id: Option<String>,
    pub name: String,
    pub notes: Option<String>,
    pub settings: AcousticSettingsDto,
    pub model_id: Option<String>,
    pub model_hash: Option<String>,
    pub audio_pipeline_version: Option<String>,
    pub conditions: Option<String>,
    pub status: Option<ProfileStatus>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateProfileDto {
    pub name: Option<String>,
    pub notes: Option<String>,
    pub settings: Option<AcousticSettingsDto>,
    pub conditions: Option<String>,
    pub status: Option<ProfileStatus>,
    pub archived: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProfilesSnapshot {
    pub version: u32,
    pub active_profile_id: Option<String>,
    pub profiles: Vec<AcousticProfile>,
    #[serde(default)]
    pub idempotency_keys: HashMap<String, String>,
}

impl Default for ProfilesSnapshot {
    fn default() -> Self {
        Self {
            version: PROFILES_STORAGE_VERSION,
            active_profile_id: None,
            profiles: Vec::new(),
            idempotency_keys: HashMap::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProfilesExportDto {
    pub version: u32,
    pub exported_at: String,
    pub active_profile_id: Option<String>,
    pub profiles: Vec<AcousticProfile>,
}

fn rfc3339_now() -> String {
    let now = SystemTime::now();
    let format = time::format_description::well_known::Rfc3339;
    time::OffsetDateTime::from(now)
        .format(&format)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_string())
}

pub struct ProfilesStore {
    dir: PathBuf,
    file_path: PathBuf,
    writer_mutex: std::sync::Mutex<()>,
}

impl ProfilesStore {
    pub fn new<P: AsRef<Path>>(storage_path: P) -> Result<Self> {
        let path = storage_path.as_ref().to_path_buf();
        let (dir, file_path) = if path.is_dir() {
            let fp = path.join(DEFAULT_PROFILES_FILENAME);
            (path, fp)
        } else {
            let parent = path
                .parent()
                .map(|p| p.to_path_buf())
                .unwrap_or_else(|| PathBuf::from("."));
            (parent, path)
        };

        let mut builder = DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder.create(&dir).with_context(|| {
            format!(
                "Failed to create profiles storage directory: {}",
                dir.display()
            )
        })?;

        Ok(Self {
            dir,
            file_path,
            writer_mutex: std::sync::Mutex::new(()),
        })
    }

    pub fn file_path(&self) -> &Path {
        &self.file_path
    }

    pub fn load(&self) -> Result<ProfilesSnapshot> {
        let file = match File::open(&self.file_path) {
            Ok(file) => file,
            Err(error) if error.kind() == ErrorKind::NotFound => {
                return Ok(ProfilesSnapshot::default());
            }
            Err(error) => return Err(error).context("Failed to open profiles file"),
        };

        if !file.metadata()?.is_file() {
            bail!("Profiles snapshot must be a regular file");
        }

        let mut content = Vec::new();
        file.take(MAX_PROFILES_SNAPSHOT_BYTES + 1)
            .read_to_end(&mut content)
            .context("Failed to read profiles file")?;

        if content.len() as u64 > MAX_PROFILES_SNAPSHOT_BYTES {
            bail!("Profiles snapshot exceeds size limit");
        }

        let snapshot: ProfilesSnapshot =
            serde_json::from_slice(&content).context("Invalid profiles snapshot JSON")?;

        if snapshot.version != PROFILES_STORAGE_VERSION {
            bail!(
                "Unsupported profiles snapshot version {}, expected {}",
                snapshot.version,
                PROFILES_STORAGE_VERSION
            );
        }

        Ok(snapshot)
    }

    /// Save the profiles snapshot atomically to disk:
    /// 1. Write to unique temporary file in the same directory.
    /// 2. fsync the temporary file.
    /// 3. Atomic rename temp file -> target file.
    /// 4. fsync the parent directory to ensure directory entry durability.
    pub fn save(&self, snapshot: &ProfilesSnapshot) -> Result<()> {
        let json_bytes = serde_json::to_vec_pretty(snapshot)
            .context("Failed to serialize profiles snapshot to JSON")?;

        if json_bytes.len() as u64 > MAX_PROFILES_SNAPSHOT_BYTES {
            bail!("Profiles snapshot exceeds size limit");
        }

        let temp_filename = format!(
            "{}.tmp.{}",
            self.file_path
                .file_name()
                .and_then(|f| f.to_str())
                .unwrap_or(DEFAULT_PROFILES_FILENAME),
            uuid::Uuid::new_v4()
        );
        let temp_path = self.dir.join(&temp_filename);

        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }

        let mut file = options
            .open(&temp_path)
            .context("Failed to create temporary profiles snapshot")?;

        let write_result = file.write_all(&json_bytes).and_then(|()| file.sync_all());
        drop(file);

        if let Err(error) = write_result.and_then(|()| fs::rename(&temp_path, &self.file_path)) {
            let _ = fs::remove_file(&temp_path);
            return Err(error).context("Failed to commit profiles snapshot atomically");
        }

        #[cfg(unix)]
        if let Ok(dir_file) = File::open(&self.dir) {
            let _ = dir_file.sync_all();
        }

        Ok(())
    }

    /// List all room profiles.
    pub fn list(&self, include_archived: bool) -> Result<Vec<AcousticProfile>> {
        let snapshot = self.load()?;
        let profiles = snapshot
            .profiles
            .into_iter()
            .filter(|p| include_archived || !p.archived)
            .collect();
        Ok(profiles)
    }

    /// Get profile by stable ID.
    pub fn get(&self, id: &str) -> Result<Option<AcousticProfile>> {
        let snapshot = self.load()?;
        Ok(snapshot.profiles.into_iter().find(|p| p.id == id))
    }

    /// Get the currently active profile.
    pub fn get_active(&self) -> Result<Option<AcousticProfile>> {
        let snapshot = self.load()?;
        if let Some(ref active_id) = snapshot.active_profile_id {
            Ok(snapshot.profiles.into_iter().find(|p| &p.id == active_id))
        } else {
            Ok(None)
        }
    }

    /// Create a new room profile.
    ///
    /// If `idempotency_key` is provided and already recorded, returns the existing profile
    /// without creating a duplicate.
    /// Non-owner callers (e.g. AI agents) are strictly forbidden from setting `OwnerConfirmed`.
    pub fn create(
        &self,
        dto: CreateProfileDto,
        idempotency_key: Option<&str>,
        is_owner: bool,
    ) -> Result<AcousticProfile> {
        dto.settings.validate()?;

        let status = dto.status.unwrap_or(ProfileStatus::Draft);
        if status == ProfileStatus::OwnerConfirmed && !is_owner {
            bail!("Only the owner can set owner_confirmed status on a profile");
        }

        let _lock = self.writer_mutex.lock().unwrap_or_else(|e| e.into_inner());
        let mut snapshot = self.load()?;

        if let Some(key) = idempotency_key {
            if let Some(existing_id) = snapshot.idempotency_keys.get(key) {
                if let Some(existing) = snapshot.profiles.iter().find(|p| &p.id == existing_id) {
                    return Ok(existing.clone());
                }
            }
        }

        let profile_id = dto
            .id
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| format!("prof_{}", uuid::Uuid::new_v4()));

        ensure!(
            !profile_id.contains('/') && !profile_id.contains('\\') && !profile_id.contains(".."),
            "Invalid profile ID; path traversal sequences not allowed"
        );

        if snapshot.profiles.iter().any(|p| p.id == profile_id) {
            bail!("Profile with ID '{}' already exists", profile_id);
        }

        let now = rfc3339_now();
        let profile = AcousticProfile {
            id: profile_id.clone(),
            schema_version: PROFILES_STORAGE_VERSION,
            revision: 1,
            name: dto.name.trim().to_string(),
            notes: dto.notes.map(|n| n.trim().to_string()),
            settings: dto.settings,
            model_id: dto.model_id,
            model_hash: dto.model_hash,
            audio_pipeline_version: dto
                .audio_pipeline_version
                .unwrap_or_else(|| DEFAULT_PIPELINE_VERSION.to_string()),
            conditions: dto.conditions,
            status,
            created_at: now.clone(),
            updated_at: now,
            archived: false,
            report_ids: Vec::new(),
        };

        snapshot.profiles.push(profile.clone());
        if let Some(key) = idempotency_key {
            snapshot
                .idempotency_keys
                .insert(key.to_string(), profile_id);
        }

        self.save(&snapshot)?;
        Ok(profile)
    }

    /// Update an existing profile.
    ///
    /// Requires `if_match_revision` matching current revision for optimistic concurrency control.
    /// Non-owner callers cannot promote profile to `OwnerConfirmed`.
    pub fn update(
        &self,
        id: &str,
        dto: UpdateProfileDto,
        if_match_revision: Option<u64>,
        is_owner: bool,
    ) -> Result<AcousticProfile> {
        if let Some(ref settings) = dto.settings {
            settings.validate()?;
        }

        if let Some(status) = dto.status {
            if status == ProfileStatus::OwnerConfirmed && !is_owner {
                bail!("Only the owner can set owner_confirmed status on a profile");
            }
        }

        let _lock = self.writer_mutex.lock().unwrap_or_else(|e| e.into_inner());
        let mut snapshot = self.load()?;

        let profile = snapshot
            .profiles
            .iter_mut()
            .find(|p| p.id == id)
            .with_context(|| format!("Profile '{}' not found", id))?;

        if let Some(expected_rev) = if_match_revision {
            ensure!(
                profile.revision == expected_rev,
                "Precondition Failed: profile revision mismatch (current: {}, expected: {})",
                profile.revision,
                expected_rev
            );
        }

        if let Some(name) = dto.name {
            let n = name.trim();
            if !n.is_empty() {
                profile.name = n.to_string();
            }
        }

        if let Some(notes) = dto.notes {
            profile.notes = Some(notes.trim().to_string());
        }

        if let Some(settings) = dto.settings {
            profile.settings = settings;
        }

        if let Some(conditions) = dto.conditions {
            profile.conditions = Some(conditions.trim().to_string());
        }

        if let Some(status) = dto.status {
            profile.status = status;
        }

        if let Some(archived) = dto.archived {
            profile.archived = archived;
        }

        profile.revision = profile.revision.saturating_add(1);
        profile.updated_at = rfc3339_now();

        let updated = profile.clone();
        self.save(&snapshot)?;
        Ok(updated)
    }

    /// Rename a profile.
    pub fn rename(&self, id: &str, new_name: &str) -> Result<AcousticProfile> {
        let name_clean = new_name.trim();
        ensure!(!name_clean.is_empty(), "Profile name cannot be empty");
        self.update(
            id,
            UpdateProfileDto {
                name: Some(name_clean.to_string()),
                notes: None,
                settings: None,
                conditions: None,
                status: None,
                archived: None,
            },
            None,
            true,
        )
    }

    /// Soft-delete / archive a profile.
    pub fn archive(&self, id: &str) -> Result<()> {
        let _lock = self.writer_mutex.lock().unwrap_or_else(|e| e.into_inner());
        let mut snapshot = self.load()?;

        let profile = snapshot
            .profiles
            .iter_mut()
            .find(|p| p.id == id)
            .with_context(|| format!("Profile '{}' not found", id))?;

        profile.archived = true;
        profile.revision = profile.revision.saturating_add(1);
        profile.updated_at = rfc3339_now();

        if snapshot.active_profile_id.as_deref() == Some(id) {
            snapshot.active_profile_id = None;
        }

        self.save(&snapshot)?;
        Ok(())
    }

    /// Hard-delete a profile (allowed for drafts or archived).
    pub fn delete(&self, id: &str) -> Result<()> {
        let _lock = self.writer_mutex.lock().unwrap_or_else(|e| e.into_inner());
        let mut snapshot = self.load()?;

        let initial_len = snapshot.profiles.len();
        snapshot.profiles.retain(|p| p.id != id);
        ensure!(
            snapshot.profiles.len() < initial_len,
            "Profile '{}' not found",
            id
        );

        if snapshot.active_profile_id.as_deref() == Some(id) {
            snapshot.active_profile_id = None;
        }

        snapshot.idempotency_keys.retain(|_, v| v != id);
        self.save(&snapshot)?;
        Ok(())
    }

    /// Set the active profile ID.
    pub fn set_active_profile_id(&self, active_id: Option<String>) -> Result<()> {
        let _lock = self.writer_mutex.lock().unwrap_or_else(|e| e.into_inner());
        let mut snapshot = self.load()?;

        if let Some(ref id) = active_id {
            ensure!(
                snapshot.profiles.iter().any(|p| &p.id == id && !p.archived),
                "Cannot activate non-existent or archived profile '{}'",
                id
            );
        }

        snapshot.active_profile_id = active_id;
        self.save(&snapshot)?;
        Ok(())
    }

    /// Export all non-archived profiles as a portable JSON document.
    pub fn export_all(&self) -> Result<ProfilesExportDto> {
        let snapshot = self.load()?;
        Ok(ProfilesExportDto {
            version: snapshot.version,
            exported_at: rfc3339_now(),
            active_profile_id: snapshot.active_profile_id,
            profiles: snapshot
                .profiles
                .into_iter()
                .filter(|p| !p.archived)
                .collect(),
        })
    }

    /// Import profiles with schema and settings compatibility validation.
    ///
    /// Non-owner callers cannot import profiles with `OwnerConfirmed` status (they are downgraded to `AutomatedTested`).
    pub fn import_all(&self, export: ProfilesExportDto, is_owner: bool) -> Result<usize> {
        ensure!(
            export.version <= PROFILES_STORAGE_VERSION,
            "Unsupported export version {}",
            export.version
        );

        for profile in &export.profiles {
            profile.settings.validate().with_context(|| {
                format!(
                    "Import failed: invalid settings in profile '{}' ({})",
                    profile.name, profile.id
                )
            })?;
        }

        let _lock = self.writer_mutex.lock().unwrap_or_else(|e| e.into_inner());
        let mut snapshot = self.load()?;
        let mut imported_count = 0;

        for mut profile in export.profiles {
            if profile.status == ProfileStatus::OwnerConfirmed && !is_owner {
                profile.status = ProfileStatus::AutomatedTested;
            }
            if let Some(existing) = snapshot.profiles.iter_mut().find(|p| p.id == profile.id) {
                profile.revision = existing.revision.saturating_add(1);
                profile.updated_at = rfc3339_now();
                *existing = profile;
            } else {
                profile.revision = 1;
                profile.updated_at = rfc3339_now();
                snapshot.profiles.push(profile);
            }
            imported_count += 1;
        }

        self.save(&snapshot)?;
        Ok(imported_count)
    }

    /// Flag profiles as requiring re-validation when the underlying audio pipeline or model changes.
    pub fn check_audio_pipeline_compatibility(
        &self,
        current_pipeline_version: &str,
        current_model_id: Option<&str>,
    ) -> Result<Vec<String>> {
        let _lock = self.writer_mutex.lock().unwrap_or_else(|e| e.into_inner());
        let mut snapshot = self.load()?;
        let mut outdated = Vec::new();

        for profile in &mut snapshot.profiles {
            let pipeline_mismatch = profile.audio_pipeline_version != current_pipeline_version;
            let model_mismatch = match (&profile.model_id, current_model_id) {
                (Some(prof_m), Some(curr_m)) => prof_m != curr_m,
                _ => false,
            };

            if pipeline_mismatch || model_mismatch {
                outdated.push(profile.id.clone());
                // Downgrade status if pipeline or model changed
                if profile.status != ProfileStatus::Draft {
                    profile.status = ProfileStatus::Draft;
                    let reason = format!(
                        " [Требуется повторная калибровка: аудиопайплайн {} / модель {:?}]",
                        current_pipeline_version, current_model_id
                    );
                    profile.notes = Some(format!(
                        "{}{}",
                        profile.notes.as_deref().unwrap_or(""),
                        reason
                    ));
                    profile.revision = profile.revision.saturating_add(1);
                    profile.updated_at = rfc3339_now();
                }
            }
        }

        if !outdated.is_empty() {
            self.save(&snapshot)?;
        }

        Ok(outdated)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dummy_settings() -> AcousticSettingsDto {
        let cfg = crate::config::WakewordCalibrationConfig::default();
        AcousticSettingsDto::from(cfg)
    }

    #[test]
    fn test_profile_crud_and_idempotency() {
        let temp_dir = std::env::temp_dir().join(format!("prof_test_{}", uuid::Uuid::new_v4()));
        let store = ProfilesStore::new(&temp_dir).unwrap();

        // 1. Create profile
        let dto = CreateProfileDto {
            id: Some("prof_living_room".to_string()),
            name: "Гостиная с эхом".to_string(),
            notes: Some("Плиточный пол".to_string()),
            settings: dummy_settings(),
            model_id: Some("hey_mini".to_string()),
            model_hash: None,
            audio_pipeline_version: Some("v1-a113x-alsa".to_string()),
            conditions: Some("echo".to_string()),
            status: Some(ProfileStatus::Draft),
        };

        let p1 = store
            .create(dto.clone(), Some("idempotent-key-1"), true)
            .unwrap();
        assert_eq!(p1.id, "prof_living_room");
        assert_eq!(p1.name, "Гостиная с эхом");
        assert_eq!(p1.revision, 1);
        assert_eq!(p1.status, ProfileStatus::Draft);

        // 2. Duplicate create with same idempotency key returns identical profile
        let p1_dup = store
            .create(dto.clone(), Some("idempotent-key-1"), true)
            .unwrap();
        assert_eq!(p1_dup.id, p1.id);
        assert_eq!(p1_dup.revision, p1.revision);

        // 3. Agent cannot set owner_confirmed
        let agent_dto = CreateProfileDto {
            id: Some("prof_agent_attempt".to_string()),
            name: "Попытка агента".to_string(),
            notes: None,
            settings: dummy_settings(),
            model_id: None,
            model_hash: None,
            audio_pipeline_version: None,
            conditions: None,
            status: Some(ProfileStatus::OwnerConfirmed),
        };
        assert!(store.create(agent_dto, None, false).is_err());

        // 4. Update profile with revision match
        let upd = UpdateProfileDto {
            name: Some("Гостиная (откалибровано)".to_string()),
            notes: Some("Добавлен ковёр".to_string()),
            settings: None,
            conditions: Some("medium_echo".to_string()),
            status: Some(ProfileStatus::AutomatedTested),
            archived: None,
        };
        let p2 = store
            .update("prof_living_room", upd.clone(), Some(1), false)
            .unwrap();
        assert_eq!(p2.revision, 2);
        assert_eq!(p2.name, "Гостиная (откалибровано)");
        assert_eq!(p2.status, ProfileStatus::AutomatedTested);

        // 5. Update with mismatched revision fails (optimistic locking)
        assert!(store
            .update("prof_living_room", upd, Some(1), false)
            .is_err());

        // 6. Activate and get active
        store
            .set_active_profile_id(Some("prof_living_room".to_string()))
            .unwrap();
        let active = store.get_active().unwrap().unwrap();
        assert_eq!(active.id, "prof_living_room");

        // 7. Archive
        store.archive("prof_living_room").unwrap();
        assert!(store.list(false).unwrap().is_empty());
        assert_eq!(store.list(true).unwrap().len(), 1);
        assert!(store.get_active().unwrap().is_none());

        // Cleanup
        let _ = fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_pipeline_version_change_downgrades_status() {
        let temp_dir =
            std::env::temp_dir().join(format!("prof_pipe_test_{}", uuid::Uuid::new_v4()));
        let store = ProfilesStore::new(&temp_dir).unwrap();

        let dto = CreateProfileDto {
            id: Some("prof_pipe".to_string()),
            name: "Кабинет".to_string(),
            notes: None,
            settings: dummy_settings(),
            model_id: Some("hey_mini_v1".to_string()),
            model_hash: None,
            audio_pipeline_version: Some("v1-a113x-alsa".to_string()),
            conditions: None,
            status: Some(ProfileStatus::OwnerConfirmed),
        };
        store.create(dto, None, true).unwrap();

        // Change model/pipeline
        let outdated = store
            .check_audio_pipeline_compatibility("v2-a113x-alsa", Some("hey_mini_v2"))
            .unwrap();
        assert_eq!(outdated, vec!["prof_pipe"]);

        let p = store.get("prof_pipe").unwrap().unwrap();
        assert_eq!(p.status, ProfileStatus::Draft);
        assert!(p.notes.unwrap().contains("Требуется повторная калибровка"));

        let _ = fs::remove_dir_all(&temp_dir);
    }
}
