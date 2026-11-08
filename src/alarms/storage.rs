//! Versioned atomic persistent storage for alarms.
//! Serializes writers across processes and syncs snapshots before acknowledgement.

use crate::alarms::types::{Alarm, AlarmId};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::{self, DirBuilder, File, OpenOptions, TryLockError};
use std::io::{ErrorKind, Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const STORAGE_VERSION: u32 = 1;
const STORAGE_FILENAME: &str = "alarms.json";
const MAX_SNAPSHOT_BYTES: u64 = 1024 * 1024;
const WRITE_LOCK_TIMEOUT: Duration = Duration::from_millis(250);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AlarmsSnapshot {
    pub version: u32,
    pub alarms: Vec<Alarm>,
    #[serde(default)]
    pub idempotency_keys: HashMap<String, AlarmId>,
}

impl Default for AlarmsSnapshot {
    fn default() -> Self {
        Self {
            version: STORAGE_VERSION,
            alarms: Vec::new(),
            idempotency_keys: HashMap::new(),
        }
    }
}

pub struct AlarmStore {
    dir: PathBuf,
    file_path: PathBuf,
}

impl AlarmStore {
    pub fn new<P: AsRef<Path>>(dir: P) -> Result<Self> {
        let dir = dir.as_ref().to_path_buf();
        let mut builder = DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder.create(&dir).with_context(|| {
            format!(
                "Failed to create alarms storage directory: {}",
                dir.display()
            )
        })?;

        let file_path = dir.join(STORAGE_FILENAME);
        Ok(Self { dir, file_path })
    }

    /// Load the current alarms snapshot from disk.
    /// Returns default empty snapshot if file does not exist yet.
    pub fn load(&self) -> Result<AlarmsSnapshot> {
        let file = match File::open(&self.file_path) {
            Ok(file) => file,
            Err(error) if error.kind() == ErrorKind::NotFound => {
                return Ok(AlarmsSnapshot::default());
            }
            Err(error) => return Err(error).context("Failed to open alarms file"),
        };
        if !file.metadata()?.is_file() {
            bail!("Alarms snapshot must be a regular file");
        }
        let mut content = Vec::new();
        file.take(MAX_SNAPSHOT_BYTES + 1)
            .read_to_end(&mut content)
            .context("Failed to read alarms file")?;
        if content.len() as u64 > MAX_SNAPSHOT_BYTES {
            bail!("Alarms snapshot exceeds its size limit");
        }
        let snapshot: AlarmsSnapshot =
            serde_json::from_slice(&content).context("Invalid alarms snapshot JSON")?;

        if snapshot.version != STORAGE_VERSION {
            bail!("Unsupported alarms snapshot version {}", snapshot.version);
        }

        Ok(snapshot)
    }

    /// Read and optionally commit a fresh snapshot under the cross-process writer lock.
    pub(crate) fn update<T>(
        &self,
        change: impl FnOnce(&mut AlarmsSnapshot) -> Result<(T, bool)>,
    ) -> Result<T> {
        let _writer = self.acquire_lock("alarms.lock", WRITE_LOCK_TIMEOUT)?;
        let mut snapshot = self.load()?;
        let (result, changed) = change(&mut snapshot)?;
        if changed {
            self.save(&snapshot)?;
        }
        Ok(result)
    }

    /// Hold a separate lease for the one process that dispatches due alarms.
    pub(crate) fn claim_scheduler(&self) -> Result<File> {
        self.acquire_lock("scheduler.lock", Duration::ZERO)
            .context("Cannot claim the alarm scheduler")
    }

    fn acquire_lock(&self, name: &str, timeout: Duration) -> Result<File> {
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options
            .open(self.dir.join(name))
            .context("Failed to open alarm lock")?;
        let started = Instant::now();
        loop {
            match file.try_lock() {
                Ok(()) => return Ok(file),
                Err(TryLockError::WouldBlock) if started.elapsed() < timeout => {
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(TryLockError::WouldBlock) => bail!("Alarm lock is busy: {name}"),
                Err(TryLockError::Error(error)) => {
                    return Err(error).context("Failed to acquire alarm lock");
                }
            }
        }
    }

    /// Save the alarms snapshot atomically to disk:
    /// 1. Write to unique temporary file in the same directory.
    /// 2. fsync the temporary file.
    /// 3. Atomic rename temp file -> target alarms.json.
    /// 4. fsync the parent directory to ensure directory entry durability.
    fn save(&self, snapshot: &AlarmsSnapshot) -> Result<()> {
        let json_bytes = serde_json::to_vec_pretty(snapshot)
            .context("Failed to serialize alarms snapshot to JSON")?;
        if json_bytes.len() as u64 > MAX_SNAPSHOT_BYTES {
            bail!("Alarms snapshot exceeds its size limit");
        }

        let temp_filename = format!("{}.tmp.{}", STORAGE_FILENAME, uuid::Uuid::new_v4());
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
            .context("Failed to create temporary alarms snapshot")?;

        let write_result = file.write_all(&json_bytes).and_then(|()| file.sync_all());
        drop(file);
        if let Err(error) = write_result.and_then(|()| fs::rename(&temp_path, &self.file_path)) {
            let _ = fs::remove_file(&temp_path);
            return Err(error).context("Failed to commit alarms snapshot");
        }

        #[cfg(unix)]
        File::open(&self.dir)
            .and_then(|directory| directory.sync_all())
            .context("Alarms snapshot replaced, but directory sync failed")?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::alarms::types::{Alarm, DayOfWeek, TimeOfDay};
    use time::OffsetDateTime;

    struct TestTempDir(PathBuf);
    impl TestTempDir {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!("alarms-test-{}", uuid::Uuid::new_v4()));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
        fn path(&self) -> &Path {
            &self.0
        }
    }
    impl Drop for TestTempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn test_store_save_load_roundtrip() {
        let temp_dir = TestTempDir::new();
        let store = AlarmStore::new(temp_dir.path()).unwrap();

        let now = OffsetDateTime::now_utc();
        let alarm1 = Alarm::new_recurring(
            "test-1".to_string(),
            Some("Подъём".to_string()),
            TimeOfDay::new(7, 30, 0).unwrap(),
            DayOfWeek::weekdays(),
            "Asia/Tbilisi".to_string(),
            None,
            now,
        );

        let mut snapshot = AlarmsSnapshot::default();
        snapshot.alarms.push(alarm1.clone());
        snapshot
            .idempotency_keys
            .insert("client-req-123".to_string(), "test-1".to_string());

        store.save(&snapshot).unwrap();

        let loaded = store.load().unwrap();
        assert_eq!(loaded.alarms.len(), 1);
        assert_eq!(loaded.alarms[0].id, "test-1");
        assert_eq!(loaded.alarms[0].label.as_deref(), Some("Подъём"));
        assert_eq!(
            loaded.idempotency_keys.get("client-req-123"),
            Some(&"test-1".to_string())
        );
    }

    #[test]
    fn test_store_empty_file_non_existent() {
        let temp_dir = TestTempDir::new();
        let store = AlarmStore::new(temp_dir.path()).unwrap();

        let snapshot = store.load().unwrap();
        assert!(snapshot.alarms.is_empty());
    }

    #[test]
    fn test_store_corrupted_file_fails_cleanly() {
        let temp_dir = TestTempDir::new();
        let file_path = temp_dir.path().join(STORAGE_FILENAME);
        fs::write(&file_path, "not a valid json {{{").unwrap();

        let store = AlarmStore::new(temp_dir.path()).unwrap();
        assert!(store.load().is_err());
    }

    #[test]
    fn rejected_update_leaves_previous_snapshot_untouched() {
        let temp_dir = TestTempDir::new();
        let store = AlarmStore::new(temp_dir.path()).unwrap();
        store
            .update(|snapshot| {
                snapshot
                    .idempotency_keys
                    .insert("original".into(), "original-id".into());
                Ok(((), true))
            })
            .unwrap();
        let original = fs::read(temp_dir.path().join(STORAGE_FILENAME)).unwrap();

        let failure: Result<()> = store.update(|snapshot| {
            snapshot.idempotency_keys.clear();
            bail!("synthetic rejection");
        });
        assert!(failure.is_err());
        assert_eq!(
            fs::read(temp_dir.path().join(STORAGE_FILENAME)).unwrap(),
            original
        );
    }

    #[test]
    fn existing_empty_or_unknown_version_snapshot_fails_closed() {
        let temp_dir = TestTempDir::new();
        let store = AlarmStore::new(temp_dir.path()).unwrap();
        let path = temp_dir.path().join(STORAGE_FILENAME);
        for contents in ["", r#"{"version":99,"alarms":[]}"#] {
            fs::write(&path, contents).unwrap();
            assert!(store.load().is_err());
            assert!(store.update(|_| Ok(((), true))).is_err());
            assert_eq!(fs::read_to_string(&path).unwrap(), contents);
        }
    }
}
