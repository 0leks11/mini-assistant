//! Alarm manager: coordinates persistence, scheduling, and event dispatch.

use crate::alarms::clock::Clock;
use crate::alarms::storage::AlarmStore;
use crate::alarms::types::{
    parse_timezone_offset, try_parse_timezone_offset, Alarm, AlarmEvent, AlarmId, AlarmSchedule,
    AlarmState, AlarmSummary, TRIGGER_GRACE,
};
use anyhow::{bail, Context, Result};
use std::fs::File;
use std::sync::{Arc, Mutex};
use time::{Duration, OffsetDateTime};
use tokio::sync::broadcast;
use tracing::error;

const MAX_REQUEST_KEYS: usize = 4096;
const MAX_REQUEST_KEY_BYTES: usize = 256;

struct Scheduler {
    _lease: File,
    dispatched: Option<(AlarmId, OffsetDateTime)>,
}

pub struct AlarmManager<C: Clock> {
    clock: Arc<C>,
    store: Arc<AlarmStore>,
    event_tx: broadcast::Sender<AlarmEvent>,
    scheduler: Mutex<Option<Scheduler>>,
}

impl<C: Clock> AlarmManager<C> {
    pub fn new(clock: Arc<C>, store: Arc<AlarmStore>) -> Result<Self> {
        store.load().context("Failed to load alarms from store")?;
        let (event_tx, _) = broadcast::channel(32);
        Ok(Self {
            clock,
            store,
            event_tx,
            scheduler: Mutex::new(None),
        })
    }

    /// Subscribe to events dispatched by this process after persistence.
    /// Other processes' changes are visible through fresh snapshot reads.
    pub fn subscribe(&self) -> broadcast::Receiver<AlarmEvent> {
        self.event_tx.subscribe()
    }

    /// Claim the exclusive scheduler lease until this manager is dropped.
    ///
    /// Returns an error when another scheduler already owns this store.
    pub fn try_claim_scheduler(&self) -> Result<()> {
        let mut scheduler = self.scheduler.lock().unwrap();
        if scheduler.is_none() {
            *scheduler = Some(Scheduler {
                _lease: self.store.claim_scheduler()?,
                dispatched: None,
            });
        }
        Ok(())
    }

    /// Read current alarm records, including changes from other processes.
    /// Returns an error if the snapshot cannot be read or decoded.
    pub fn alarms_snapshot(&self) -> Result<Vec<Alarm>> {
        Ok(self.store.load()?.alarms)
    }

    /// Read the current persisted ringing alarm, if any.
    /// Returns an error if the snapshot cannot be read or decoded.
    pub fn try_active_ringing_id(&self) -> Result<Option<AlarmId>> {
        let alarms = self.alarms_snapshot()?;
        Ok(alarms
            .iter()
            .find(|alarm| matches!(alarm.state, AlarmState::Ringing { .. }))
            .map(|alarm| alarm.id.clone()))
    }

    pub fn active_ringing_id(&self) -> Option<AlarmId> {
        self.try_active_ringing_id().unwrap_or_else(|error| {
            error!(%error, "Failed to read active alarm");
            None
        })
    }

    /// Create and persist an alarm. Repeating a request key returns the original alarm.
    pub fn create_alarm(
        &self,
        schedule: AlarmSchedule,
        label: Option<String>,
        melody_id: Option<String>,
        idempotency_key: Option<String>,
    ) -> Result<Alarm> {
        self.store.update(|snapshot| {
            if let Some(existing_id) = idempotency_key
                .as_ref()
                .and_then(|key| snapshot.idempotency_keys.get(key))
            {
                let existing = snapshot
                    .alarms
                    .iter()
                    .find(|alarm| &alarm.id == existing_id)
                    .context("The alarm for this request was already deleted")?;
                // A retry can follow an uncertain directory sync. Confirm the snapshot again.
                return Ok((existing.clone(), true));
            }
            if let Some(key) = &idempotency_key {
                if key.is_empty() || key.len() > MAX_REQUEST_KEY_BYTES {
                    bail!("Alarm request key must contain 1–{MAX_REQUEST_KEY_BYTES} bytes");
                }
                // Preserve tombstones instead of making an old request create a new alarm.
                if snapshot.idempotency_keys.len() >= MAX_REQUEST_KEYS {
                    bail!("Alarm request history is full");
                }
            }

            let now_utc = self.clock.now_utc();
            if now_utc.year() < 2020 {
                bail!("System clock is not ready");
            }
            let timezone = match &schedule {
                AlarmSchedule::Single { timezone, .. }
                | AlarmSchedule::Recurring { timezone, .. } => timezone,
            };
            if try_parse_timezone_offset(timezone).is_none() {
                bail!("Unsupported alarm time zone");
            }
            let id = uuid::Uuid::new_v4().to_string();
            let alarm = match schedule {
                AlarmSchedule::Single {
                    target_utc,
                    timezone,
                } => {
                    if target_utc < now_utc {
                        bail!("Cannot set an alarm in the past");
                    }
                    Alarm::new_single(id.clone(), label, target_utc, timezone, melody_id, now_utc)
                }
                AlarmSchedule::Recurring {
                    time,
                    days,
                    timezone,
                } => {
                    time.to_time().context("Invalid recurring alarm time")?;
                    if days.is_empty() {
                        bail!("Recurring alarm must specify at least one day of the week");
                    }
                    Alarm::new_recurring(
                        id.clone(),
                        label,
                        time,
                        days,
                        timezone,
                        melody_id,
                        now_utc,
                    )
                }
            };
            snapshot.alarms.push(alarm.clone());
            if let Some(key) = idempotency_key {
                snapshot.idempotency_keys.insert(key, id);
            }
            Ok((alarm, true))
        })
    }

    /// List a fresh snapshot, optionally including disabled and completed alarms.
    /// Returns an error if the snapshot cannot be read or decoded.
    pub fn try_list_alarms(&self, include_inactive: bool) -> Result<Vec<AlarmSummary>> {
        let now_utc = self.clock.now_utc();
        let alarms = self.alarms_snapshot()?;

        let mut summaries: Vec<(Option<OffsetDateTime>, AlarmSummary)> = alarms
            .iter()
            .filter(|a| {
                if include_inactive {
                    true
                } else {
                    matches!(
                        a.state,
                        AlarmState::Active
                            | AlarmState::Ringing { .. }
                            | AlarmState::Snoozed { .. }
                    ) || (matches!(a.state, AlarmState::SkippedNext { .. })
                        && matches!(a.schedule, AlarmSchedule::Recurring { .. }))
                }
            })
            .map(|a| {
                let next_utc = a.next_trigger_utc(now_utc);
                let tz_offset = match &a.schedule {
                    AlarmSchedule::Single { timezone, .. } => parse_timezone_offset(timezone),
                    AlarmSchedule::Recurring { timezone, .. } => parse_timezone_offset(timezone),
                };

                let next_local_str = next_utc.map(|utc| {
                    let local = utc.to_offset(tz_offset);
                    format!(
                        "{:02}:{:02} ({:04}-{:02}-{:02})",
                        local.hour(),
                        local.minute(),
                        local.year(),
                        local.month() as u8,
                        local.day()
                    )
                });

                let schedule_desc = match &a.schedule {
                    AlarmSchedule::Single {
                        target_utc,
                        timezone,
                    } => {
                        let local = target_utc.to_offset(parse_timezone_offset(timezone));
                        format!(
                            "Разовый на {:02}:{:02} ({:04}-{:02}-{:02})",
                            local.hour(),
                            local.minute(),
                            local.year(),
                            local.month() as u8,
                            local.day()
                        )
                    }
                    AlarmSchedule::Recurring { time, days, .. } => {
                        let days_str: Vec<&'static str> =
                            days.iter().map(|d| d.to_ru_short()).collect();
                        format!(
                            "{:02}:{:02} ({})",
                            time.hour,
                            time.minute,
                            days_str.join(",")
                        )
                    }
                };

                let status_str = match &a.state {
                    AlarmState::Active => "активен".to_string(),
                    AlarmState::Disabled => "выключен".to_string(),
                    AlarmState::Ringing { .. } => "звенит!".to_string(),
                    AlarmState::Snoozed { until_utc, .. } => {
                        let local = until_utc.to_offset(tz_offset);
                        format!("отложен до {:02}:{:02}", local.hour(), local.minute())
                    }
                    AlarmState::SkippedNext { skip_date } => {
                        format!("пропущен на {}", skip_date)
                    }
                    AlarmState::Completed { .. } => "завершён".to_string(),
                };

                let summary = AlarmSummary {
                    id: a.id.clone(),
                    label: a.label.clone(),
                    schedule_desc,
                    next_trigger_local: next_local_str,
                    next_trigger_utc: next_utc.map(|u| {
                        u.format(&time::format_description::well_known::Rfc3339)
                            .unwrap_or_default()
                    }),
                    status: status_str,
                };

                (next_utc, summary)
            })
            .collect();

        // Sort by nearest trigger time (earliest first, None at the end)
        summaries.sort_by(|a, b| match (a.0, b.0) {
            (Some(t1), Some(t2)) => t1.cmp(&t2),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => std::cmp::Ordering::Equal,
        });

        Ok(summaries.into_iter().map(|(_, summary)| summary).collect())
    }

    pub fn list_alarms(&self, include_inactive: bool) -> Vec<AlarmSummary> {
        self.try_list_alarms(include_inactive)
            .unwrap_or_else(|error| {
                error!(%error, "Failed to list alarms");
                Vec::new()
            })
    }

    /// Find an exact ID or an unambiguous nonempty prefix in a fresh snapshot.
    /// Returns an error for an ambiguous prefix or unavailable snapshot.
    pub fn try_get_alarm(&self, id_or_prefix: &str) -> Result<Option<Alarm>> {
        let alarms = self.alarms_snapshot()?;
        Ok(find_alarm(&alarms, id_or_prefix)?.map(|index| alarms[index].clone()))
    }

    pub fn get_alarm(&self, id_or_prefix: &str) -> Option<Alarm> {
        self.try_get_alarm(id_or_prefix).unwrap_or_else(|error| {
            error!(%error, "Failed to get alarm");
            None
        })
    }

    /// Delete one alarm identified by its exact ID or an unambiguous prefix.
    pub fn delete_alarm(&self, id: &str) -> Result<bool> {
        self.store.update(|snapshot| {
            let Some(index) = find_alarm(&snapshot.alarms, id)? else {
                return Ok((false, false));
            };
            snapshot.alarms.remove(index);
            Ok((true, true))
        })
    }

    /// Enable or disable an alarm.
    pub fn set_enabled(&self, id: &str, enabled: bool) -> Result<Alarm> {
        self.store.update(|snapshot| {
            let index = find_alarm(&snapshot.alarms, id)?.context("Alarm not found")?;
            let alarm = &mut snapshot.alarms[index];
            let change = if enabled {
                matches!(alarm.state, AlarmState::Disabled)
            } else {
                !matches!(alarm.state, AlarmState::Disabled)
            };
            if change {
                alarm.state = if enabled {
                    AlarmState::Active
                } else {
                    AlarmState::Disabled
                };
                alarm.updated_at_utc = self.clock.now_utc();
            }
            Ok((alarm.clone(), change))
        })
    }

    /// Skip the next occurrence of an active alarm.
    pub fn skip_next(&self, id: &str) -> Result<Alarm> {
        self.store.update(|snapshot| {
            let index = find_alarm(&snapshot.alarms, id)?.context("Alarm not found")?;
            let alarm = &mut snapshot.alarms[index];
            let now_utc = self.clock.now_utc();
            let timezone = match &alarm.schedule {
                AlarmSchedule::Single { timezone, .. }
                | AlarmSchedule::Recurring { timezone, .. } => timezone,
            };
            let offset = parse_timezone_offset(timezone);
            match &alarm.state {
                AlarmState::Active => {}
                AlarmState::SkippedNext { skip_date }
                    if *skip_date < now_utc.to_offset(offset).date().to_string() => {}
                _ => bail!("Alarm must be active with no pending skip"),
            }
            let next = alarm
                .next_trigger_utc(now_utc)
                .context("Alarm has no upcoming trigger")?;
            alarm.state = AlarmState::SkippedNext {
                skip_date: next.to_offset(offset).date().to_string(),
            };
            alarm.last_triggered_utc = Some(next);
            alarm.updated_at_utc = now_utc;
            Ok((alarm.clone(), true))
        })
    }

    /// Defer only an already ringing occurrence.
    pub fn snooze(&self, id: &str, duration_minutes: u32) -> Result<Alarm> {
        if !(1..=60).contains(&duration_minutes) {
            bail!("Snooze duration must be between 1 and 60 minutes");
        }
        let (alarm, until_utc) = self.store.update(|snapshot| {
            let index = find_alarm(&snapshot.alarms, id)?.context("Alarm not found")?;
            let alarm = &mut snapshot.alarms[index];
            if !matches!(alarm.state, AlarmState::Ringing { .. }) {
                bail!("Alarm is not ringing");
            }
            let now_utc = self.clock.now_utc();
            let until_utc = now_utc + Duration::minutes(i64::from(duration_minutes));
            alarm.state = AlarmState::Snoozed {
                until_utc,
                original_utc: alarm.last_triggered_utc.unwrap_or(now_utc),
            };
            alarm.updated_at_utc = now_utc;
            Ok(((alarm.clone(), until_utc), true))
        })?;
        let _ = self.event_tx.send(AlarmEvent::Snoozed {
            id: alarm.id.clone(),
            until_utc,
        });
        Ok(alarm)
    }

    /// Stop a ringing occurrence only. When no ID is provided, use the persisted active ring.
    pub fn stop(&self, id_opt: Option<&str>) -> Result<Option<Alarm>> {
        let alarm = self.store.update(|snapshot| {
            let index = match id_opt {
                Some(id) => find_alarm(&snapshot.alarms, id)?,
                None => snapshot
                    .alarms
                    .iter()
                    .position(|alarm| matches!(alarm.state, AlarmState::Ringing { .. })),
            };
            let Some(index) = index else {
                return Ok((None, false));
            };
            let alarm = &mut snapshot.alarms[index];
            if !matches!(alarm.state, AlarmState::Ringing { .. }) {
                return Ok((None, false));
            }
            finish_occurrence(alarm, self.clock.now_utc());
            Ok((Some(alarm.clone()), true))
        })?;
        if let Some(alarm) = &alarm {
            let _ = self.event_tx.send(AlarmEvent::Stopped {
                id: alarm.id.clone(),
            });
        }
        Ok(alarm)
    }

    /// Evaluate schedules as the exclusive scheduler and dispatch a committed ring.
    ///
    /// Returns an error when the scheduler lease or store is unavailable.
    pub fn try_tick(&self) -> Result<Vec<AlarmEvent>> {
        self.try_claim_scheduler()?;
        let mut scheduler = self.scheduler.lock().unwrap();
        let scheduler = scheduler.as_mut().expect("scheduler lease is held");
        let now_utc = self.clock.now_utc();
        // This catches a reset epoch. Actual clock trust after cold boot is a device concern.
        if now_utc.year() < 2020 {
            return Ok(Vec::new());
        }

        let next = self.store.update(|snapshot| {
            let mut changed = false;
            let mut next_due = None;
            for (index, alarm) in snapshot.alarms.iter_mut().enumerate() {
                if let AlarmState::Ringing { started_utc } = alarm.state {
                    let occurrence = (alarm.id.clone(), started_utc);
                    if scheduler.dispatched.as_ref() == Some(&occurrence) {
                        return Ok((None, changed));
                    }
                    if now_utc >= started_utc && now_utc - started_utc <= TRIGGER_GRACE {
                        // Re-commit a recent ring before resuming it on a fresh process.
                        return Ok((Some((triggered(alarm), occurrence)), true));
                    }
                    finish_occurrence(alarm, now_utc);
                    changed = true;
                    continue;
                }
                let Some(due) = alarm
                    .next_trigger_utc(now_utc)
                    .filter(|due| *due <= now_utc)
                else {
                    continue;
                };
                if now_utc - due > TRIGGER_GRACE {
                    finish_occurrence(alarm, now_utc);
                    changed = true;
                } else if next_due.is_none_or(|(_, previous)| due < previous) {
                    next_due = Some((index, due));
                }
            }

            let Some((index, due)) = next_due else {
                return Ok((None, changed));
            };
            let alarm = &mut snapshot.alarms[index];
            alarm.last_triggered_utc = Some(match alarm.state {
                AlarmState::Snoozed { original_utc, .. } => original_utc,
                _ => due,
            });
            alarm.state = AlarmState::Ringing {
                started_utc: now_utc,
            };
            alarm.updated_at_utc = now_utc;
            Ok((Some((triggered(alarm), (alarm.id.clone(), now_utc))), true))
        })?;

        if let Some((event, occurrence)) = next {
            scheduler.dispatched = Some(occurrence);
            let _ = self.event_tx.send(event.clone());
            return Ok(vec![event]);
        }
        Ok(Vec::new())
    }

    pub fn tick(&self) -> Vec<AlarmEvent> {
        self.try_tick().unwrap_or_else(|error| {
            error!(%error, "Failed to evaluate alarms");
            Vec::new()
        })
    }
}

fn find_alarm(alarms: &[Alarm], id: &str) -> Result<Option<usize>> {
    if id.is_empty() {
        bail!("Alarm ID cannot be empty");
    }
    if let Some(index) = alarms.iter().position(|alarm| alarm.id == id) {
        return Ok(Some(index));
    }
    let mut matches = alarms
        .iter()
        .enumerate()
        .filter(|(_, alarm)| alarm.id.starts_with(id));
    let found = matches.next().map(|(index, _)| index);
    if matches.next().is_some() {
        bail!("Alarm ID prefix is ambiguous");
    }
    Ok(found)
}

fn finish_occurrence(alarm: &mut Alarm, now_utc: OffsetDateTime) {
    alarm.updated_at_utc = now_utc;
    alarm.state = match alarm.schedule {
        AlarmSchedule::Single { .. } => AlarmState::Completed {
            completed_at_utc: now_utc,
        },
        AlarmSchedule::Recurring { .. } => AlarmState::Active,
    };
}

fn triggered(alarm: &Alarm) -> AlarmEvent {
    AlarmEvent::Triggered {
        id: alarm.id.clone(),
        label: alarm.label.clone(),
        melody_id: alarm.melody_id.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::alarms::clock::TestClock;
    use crate::alarms::types::{DayOfWeek, TimeOfDay};
    use std::fs;
    use std::path::{Path, PathBuf};
    use time::macros::datetime;

    struct TestTempDir(PathBuf);
    impl TestTempDir {
        fn new() -> Self {
            let path =
                std::env::temp_dir().join(format!("alarms-mgr-test-{}", uuid::Uuid::new_v4()));
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
    fn test_create_and_list_single_alarm() {
        let temp_dir = TestTempDir::new();
        let store = Arc::new(AlarmStore::new(temp_dir.path()).unwrap());
        let clock = Arc::new(TestClock::new(datetime!(2026-11-05 06:00:00 UTC)));
        let manager = AlarmManager::new(clock.clone(), store).unwrap();

        let target = datetime!(2026-11-05 07:00:00 UTC);
        let alarm = manager
            .create_alarm(
                AlarmSchedule::Single {
                    target_utc: target,
                    timezone: "Asia/Tbilisi".to_string(),
                },
                Some("Утренний подъём".to_string()),
                None,
                None,
            )
            .unwrap();

        assert_eq!(alarm.label.as_deref(), Some("Утренний подъём"));

        let list = manager.list_alarms(false);
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].id, alarm.id);
        assert_eq!(list[0].status, "активен");
    }

    #[test]
    fn test_tick_triggers_alarm_and_snooze() {
        let temp_dir = TestTempDir::new();
        let store = Arc::new(AlarmStore::new(temp_dir.path()).unwrap());
        let clock = Arc::new(TestClock::new(datetime!(2026-11-05 06:59:50 UTC)));
        let manager = AlarmManager::new(clock.clone(), store).unwrap();

        let target = datetime!(2026-11-05 07:00:00 UTC);
        let alarm = manager
            .create_alarm(
                AlarmSchedule::Single {
                    target_utc: target,
                    timezone: "Asia/Tbilisi".to_string(),
                },
                None,
                None,
                None,
            )
            .unwrap();

        // 10s before alarm -> no events
        let events = manager.tick();
        assert!(events.is_empty());
        assert_eq!(manager.active_ringing_id(), None);

        // Advance past alarm
        clock.advance(std::time::Duration::from_secs(15));
        let events = manager.tick();
        assert_eq!(events.len(), 1);
        assert_eq!(manager.active_ringing_id(), Some(alarm.id.clone()));

        // Snooze for 5 minutes
        let snoozed = manager.snooze(&alarm.id, 5).unwrap();
        assert!(matches!(snoozed.state, AlarmState::Snoozed { .. }));
        assert_eq!(manager.active_ringing_id(), None);

        // Advance 4 minutes -> still snoozed
        clock.advance(std::time::Duration::from_secs(240));
        let events = manager.tick();
        assert!(events.is_empty());

        // Advance 2 more minutes -> re-triggers
        clock.advance(std::time::Duration::from_secs(120));
        let events = manager.tick();
        assert_eq!(events.len(), 1);
        assert_eq!(manager.active_ringing_id(), Some(alarm.id.clone()));

        // Stop alarm -> completes single alarm
        let stopped = manager.stop(None).unwrap().unwrap();
        assert!(matches!(stopped.state, AlarmState::Completed { .. }));
        assert_eq!(manager.active_ringing_id(), None);
    }

    #[test]
    fn test_recurring_alarm_weekday_schedule() {
        let temp_dir = TestTempDir::new();
        let store = Arc::new(AlarmStore::new(temp_dir.path()).unwrap());
        // 2026-11-05 is Thursday. 03:00 UTC = 07:00 in Asia/Tbilisi (+04:00)
        let clock = Arc::new(TestClock::new(datetime!(2026-11-05 02:59:50 UTC)));
        let manager = AlarmManager::new(clock.clone(), store).unwrap();

        let alarm = manager
            .create_alarm(
                AlarmSchedule::Recurring {
                    time: TimeOfDay::new(7, 0, 0).unwrap(),
                    days: DayOfWeek::weekdays(),
                    timezone: "Asia/Tbilisi".to_string(),
                },
                Some("Будни".to_string()),
                None,
                None,
            )
            .unwrap();

        // Next trigger should be today at 03:00 UTC (07:00 Tbilisi)
        let next = alarm.next_trigger_utc(clock.now_utc()).unwrap();
        assert_eq!(next, datetime!(2026-11-05 03:00:00 UTC));

        // Advance 20s past trigger (now 03:00:10 UTC)
        clock.advance(std::time::Duration::from_secs(20));
        let events = manager.tick();
        assert_eq!(events.len(), 1);

        // Stop alarm -> recurs to Friday
        let stopped = manager.stop(None).unwrap().unwrap();
        assert_eq!(stopped.state, AlarmState::Active);

        let next_friday = stopped.next_trigger_utc(clock.now_utc()).unwrap();
        assert_eq!(next_friday, datetime!(2026-11-06 03:00:00 UTC));
    }

    #[test]
    fn test_idempotency_prevents_duplicate() {
        let temp_dir = TestTempDir::new();
        let store = Arc::new(AlarmStore::new(temp_dir.path()).unwrap());
        let clock = Arc::new(TestClock::new(datetime!(2026-11-05 06:00:00 UTC)));
        let manager = AlarmManager::new(clock, store).unwrap();

        let req = AlarmSchedule::Single {
            target_utc: datetime!(2026-11-05 07:00:00 UTC),
            timezone: "Asia/Tbilisi".to_string(),
        };

        let a1 = manager
            .create_alarm(
                req.clone(),
                Some("Test".to_string()),
                None,
                Some("req-1".to_string()),
            )
            .unwrap();
        let a2 = manager
            .create_alarm(
                req,
                Some("Test".to_string()),
                None,
                Some("req-1".to_string()),
            )
            .unwrap();

        assert_eq!(a1.id, a2.id);
        assert_eq!(manager.list_alarms(true).len(), 1);
    }

    #[test]
    fn concurrent_writers_keep_both_alarms_and_respect_idempotency() {
        let temp_dir = TestTempDir::new();
        let clock = Arc::new(TestClock::new(datetime!(2026-11-05 06:00:00 UTC)));
        let first = AlarmManager::new(
            clock.clone(),
            Arc::new(AlarmStore::new(temp_dir.path()).unwrap()),
        )
        .unwrap();
        let second =
            AlarmManager::new(clock, Arc::new(AlarmStore::new(temp_dir.path()).unwrap())).unwrap();
        let start = std::sync::Barrier::new(2);
        let schedule = AlarmSchedule::Single {
            target_utc: datetime!(2026-11-05 07:00:00 UTC),
            timezone: "Asia/Tbilisi".to_string(),
        };
        let write = |manager: &AlarmManager<TestClock>, key: &str| {
            start.wait();
            manager
                .create_alarm(schedule.clone(), None, None, Some(key.to_string()))
                .unwrap()
        };
        std::thread::scope(|scope| {
            let a = scope.spawn(|| write(&first, "first"));
            let b = scope.spawn(|| write(&second, "second"));
            assert_ne!(a.join().unwrap().id, b.join().unwrap().id);
        });
        let duplicate = first
            .create_alarm(schedule.clone(), None, None, Some("second".into()))
            .unwrap();
        assert_eq!(second.try_list_alarms(false).unwrap().len(), 2);
        assert!(first.delete_alarm(&duplicate.id).unwrap());
        assert!(second
            .create_alarm(schedule, None, None, Some("second".into()))
            .is_err());
        assert_eq!(first.try_list_alarms(false).unwrap().len(), 1);
    }

    #[test]
    fn scheduler_lease_restart_and_cli_stop_use_persisted_occurrence() {
        let temp_dir = TestTempDir::new();
        let store = Arc::new(AlarmStore::new(temp_dir.path()).unwrap());
        let clock = Arc::new(TestClock::new(datetime!(2026-11-05 06:59:50 UTC)));
        let scheduler = AlarmManager::new(clock.clone(), store.clone()).unwrap();
        let cli = AlarmManager::new(clock.clone(), store.clone()).unwrap();
        scheduler.try_claim_scheduler().unwrap();
        let alarm = cli
            .create_alarm(
                AlarmSchedule::Single {
                    target_utc: datetime!(2026-11-05 07:00:00 UTC),
                    timezone: "UTC".to_string(),
                },
                None,
                None,
                None,
            )
            .unwrap();
        assert!(cli.stop(Some(&alarm.id)).unwrap().is_none());
        assert!(cli.snooze(&alarm.id, 5).is_err());
        clock.advance(std::time::Duration::from_secs(20));
        assert_eq!(scheduler.try_tick().unwrap().len(), 1);
        assert!(cli.try_tick().is_err());
        drop(scheduler);

        let restarted = AlarmManager::new(clock.clone(), store.clone()).unwrap();
        assert_eq!(restarted.try_tick().unwrap().len(), 1);
        assert!(restarted.try_tick().unwrap().is_empty());
        assert_eq!(cli.stop(None).unwrap().unwrap().id, alarm.id);
        assert!(restarted.try_active_ringing_id().unwrap().is_none());
        drop(restarted);
        let next_boot = AlarmManager::new(clock, store).unwrap();
        assert!(next_boot.try_tick().unwrap().is_empty());
    }

    #[test]
    fn expired_snooze_does_not_ring_on_late_restart() {
        let temp_dir = TestTempDir::new();
        let store = Arc::new(AlarmStore::new(temp_dir.path()).unwrap());
        let clock = Arc::new(TestClock::new(datetime!(2026-11-05 06:59:50 UTC)));
        let scheduler = AlarmManager::new(clock.clone(), store.clone()).unwrap();
        let alarm = scheduler
            .create_alarm(
                AlarmSchedule::Single {
                    target_utc: datetime!(2026-11-05 07:00:00 UTC),
                    timezone: "UTC".to_string(),
                },
                None,
                None,
                None,
            )
            .unwrap();
        clock.advance(std::time::Duration::from_secs(20));
        assert_eq!(scheduler.try_tick().unwrap().len(), 1);
        scheduler.snooze(&alarm.id, 5).unwrap();
        drop(scheduler);
        clock.advance(std::time::Duration::from_secs(421));
        let restarted = AlarmManager::new(clock, store).unwrap();
        assert!(restarted.try_tick().unwrap().is_empty());
        assert!(restarted.try_list_alarms(false).unwrap().is_empty());
    }

    #[test]
    fn recurring_alarm_grace_crosses_midnight_and_backward_step_cannot_replay() {
        let temp_dir = TestTempDir::new();
        let store = Arc::new(AlarmStore::new(temp_dir.path()).unwrap());
        let clock = Arc::new(TestClock::new(datetime!(2026-11-05 23:58:00 UTC)));
        let manager = AlarmManager::new(clock.clone(), store).unwrap();
        let alarm = manager
            .create_alarm(
                AlarmSchedule::Recurring {
                    time: TimeOfDay::new(23, 59, 0).unwrap(),
                    days: DayOfWeek::all(),
                    timezone: "UTC".into(),
                },
                None,
                None,
                None,
            )
            .unwrap();
        clock.step_wall_clock(datetime!(2026-11-06 00:00:10 UTC));
        assert_eq!(manager.try_tick().unwrap().len(), 1);
        manager.stop(None).unwrap().unwrap();
        clock.step_wall_clock(datetime!(2026-11-05 23:58:50 UTC));
        let next = manager
            .try_get_alarm(&alarm.id)
            .unwrap()
            .unwrap()
            .next_trigger_utc(clock.now_utc());
        assert_eq!(next, Some(datetime!(2026-11-06 23:59:00 UTC)));
        clock.step_wall_clock(datetime!(2026-11-05 23:59:05 UTC));
        assert!(manager.try_tick().unwrap().is_empty());
    }

    #[test]
    fn simultaneous_alarms_take_turns_and_ambiguous_delete_changes_nothing() {
        let temp_dir = TestTempDir::new();
        let store = Arc::new(AlarmStore::new(temp_dir.path()).unwrap());
        let clock = Arc::new(TestClock::new(datetime!(2026-11-05 06:59:50 UTC)));
        let manager = AlarmManager::new(clock.clone(), store.clone()).unwrap();
        let schedule = AlarmSchedule::Single {
            target_utc: datetime!(2026-11-05 07:00:00 UTC),
            timezone: "UTC".into(),
        };
        manager
            .create_alarm(schedule.clone(), None, None, None)
            .unwrap();
        manager.create_alarm(schedule, None, None, None).unwrap();
        store
            .update(|snapshot| {
                snapshot.alarms[0].id = "same-a".into();
                snapshot.alarms[1].id = "same-b".into();
                Ok(((), true))
            })
            .unwrap();
        assert!(manager.delete_alarm("same").is_err());
        assert_eq!(manager.try_list_alarms(false).unwrap().len(), 2);
        clock.advance(std::time::Duration::from_secs(20));

        assert!(
            matches!(&manager.try_tick().unwrap()[..], [AlarmEvent::Triggered { id, .. }] if id == "same-a")
        );
        assert!(manager.try_tick().unwrap().is_empty());
        manager.stop(Some("same-a")).unwrap().unwrap();
        assert!(
            matches!(&manager.try_tick().unwrap()[..], [AlarmEvent::Triggered { id, .. }] if id == "same-b")
        );
    }
}
