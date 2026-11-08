//! Data models and contracts for alarms.

use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use time::{Duration, OffsetDateTime, Time, UtcOffset, Weekday};

pub type AlarmId = String;
pub(super) const TRIGGER_GRACE: Duration = Duration::seconds(120);

/// Supported recurrence days
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DayOfWeek {
    Monday,
    Tuesday,
    Wednesday,
    Thursday,
    Friday,
    Saturday,
    Sunday,
}

impl DayOfWeek {
    pub fn from_weekday(w: Weekday) -> Self {
        match w {
            Weekday::Monday => Self::Monday,
            Weekday::Tuesday => Self::Tuesday,
            Weekday::Wednesday => Self::Wednesday,
            Weekday::Thursday => Self::Thursday,
            Weekday::Friday => Self::Friday,
            Weekday::Saturday => Self::Saturday,
            Weekday::Sunday => Self::Sunday,
        }
    }

    pub fn to_weekday(self) -> Weekday {
        match self {
            Self::Monday => Weekday::Monday,
            Self::Tuesday => Weekday::Tuesday,
            Self::Wednesday => Weekday::Wednesday,
            Self::Thursday => Weekday::Thursday,
            Self::Friday => Weekday::Friday,
            Self::Saturday => Weekday::Saturday,
            Self::Sunday => Weekday::Sunday,
        }
    }

    pub fn all() -> BTreeSet<Self> {
        [
            Self::Monday,
            Self::Tuesday,
            Self::Wednesday,
            Self::Thursday,
            Self::Friday,
            Self::Saturday,
            Self::Sunday,
        ]
        .into_iter()
        .collect()
    }

    pub fn weekdays() -> BTreeSet<Self> {
        [
            Self::Monday,
            Self::Tuesday,
            Self::Wednesday,
            Self::Thursday,
            Self::Friday,
        ]
        .into_iter()
        .collect()
    }

    pub fn weekends() -> BTreeSet<Self> {
        [Self::Saturday, Self::Sunday].into_iter().collect()
    }

    pub fn to_ru_short(self) -> &'static str {
        match self {
            Self::Monday => "пн",
            Self::Tuesday => "вт",
            Self::Wednesday => "ср",
            Self::Thursday => "чт",
            Self::Friday => "пт",
            Self::Saturday => "сб",
            Self::Sunday => "вс",
        }
    }
}

/// Time of day (hours 0..=23, minutes 0..=59, seconds 0..=59)
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct TimeOfDay {
    pub hour: u8,
    pub minute: u8,
    #[serde(default)]
    pub second: u8,
}

impl TimeOfDay {
    pub fn new(hour: u8, minute: u8, second: u8) -> Option<Self> {
        if hour < 24 && minute < 60 && second < 60 {
            Some(Self {
                hour,
                minute,
                second,
            })
        } else {
            None
        }
    }

    pub fn to_time(self) -> Result<Time, time::error::ComponentRange> {
        Time::from_hms(self.hour, self.minute, self.second)
    }
}

/// Schedule specification
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AlarmSchedule {
    /// One-off alarm scheduled for a specific UTC point in time
    Single {
        #[serde(with = "time::serde::rfc3339")]
        target_utc: OffsetDateTime,
        /// Timezone name/offset for user-facing display (e.g. "Asia/Tbilisi" or "+04:00")
        #[serde(default = "default_timezone")]
        timezone: String,
    },
    /// Repeating alarm at a given local time on specific days of the week
    Recurring {
        time: TimeOfDay,
        days: BTreeSet<DayOfWeek>,
        /// Timezone name/offset for local time evaluation
        #[serde(default = "default_timezone")]
        timezone: String,
    },
}

fn default_timezone() -> String {
    "Asia/Tbilisi".to_string()
}

/// Dynamic state of an alarm
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum AlarmState {
    /// Scheduled and awaiting its next trigger
    Active,
    /// Disabled by user
    Disabled,
    /// Currently ringing
    Ringing {
        #[serde(with = "time::serde::rfc3339")]
        started_utc: OffsetDateTime,
    },
    /// Snoozed until a specific time
    Snoozed {
        #[serde(with = "time::serde::rfc3339")]
        until_utc: OffsetDateTime,
        #[serde(with = "time::serde::rfc3339")]
        original_utc: OffsetDateTime,
    },
    /// The next occurrence is skipped
    SkippedNext {
        skip_date: String, // YYYY-MM-DD
    },
    /// Single alarm that has fired and completed
    Completed {
        #[serde(with = "time::serde::rfc3339")]
        completed_at_utc: OffsetDateTime,
    },
}

/// Stored Alarm Entity
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Alarm {
    pub id: AlarmId,
    pub label: Option<String>,
    pub schedule: AlarmSchedule,
    pub state: AlarmState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub melody_id: Option<String>,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at_utc: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at_utc: OffsetDateTime,
    /// Last processed trigger instant (prevents re-triggering upon reboot / clock step)
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "time::serde::rfc3339::option"
    )]
    pub last_triggered_utc: Option<OffsetDateTime>,
}

impl Alarm {
    pub fn new_single(
        id: AlarmId,
        label: Option<String>,
        target_utc: OffsetDateTime,
        timezone: String,
        melody_id: Option<String>,
        now_utc: OffsetDateTime,
    ) -> Self {
        Self {
            id,
            label,
            schedule: AlarmSchedule::Single {
                target_utc,
                timezone,
            },
            state: AlarmState::Active,
            melody_id,
            created_at_utc: now_utc,
            updated_at_utc: now_utc,
            last_triggered_utc: None,
        }
    }

    pub fn new_recurring(
        id: AlarmId,
        label: Option<String>,
        time: TimeOfDay,
        days: BTreeSet<DayOfWeek>,
        timezone: String,
        melody_id: Option<String>,
        now_utc: OffsetDateTime,
    ) -> Self {
        Self {
            id,
            label,
            schedule: AlarmSchedule::Recurring {
                time,
                days,
                timezone,
            },
            state: AlarmState::Active,
            melody_id,
            created_at_utc: now_utc,
            updated_at_utc: now_utc,
            last_triggered_utc: None,
        }
    }

    /// Calculate next trigger instant in UTC from current UTC time
    pub fn next_trigger_utc(&self, from_utc: OffsetDateTime) -> Option<OffsetDateTime> {
        match &self.state {
            AlarmState::Disabled | AlarmState::Completed { .. } => None,
            AlarmState::Snoozed { until_utc, .. } => Some(*until_utc),
            AlarmState::Ringing { .. } => {
                // Already ringing
                Some(from_utc)
            }
            AlarmState::Active | AlarmState::SkippedNext { .. } => {
                match &self.schedule {
                    AlarmSchedule::Single { target_utc, .. } => (self.last_triggered_utc.is_none()
                        && !matches!(self.state, AlarmState::SkippedNext { .. }))
                    .then_some(*target_utc),
                    AlarmSchedule::Recurring {
                        time,
                        days,
                        timezone,
                    } => {
                        let offset = parse_timezone_offset(timezone);
                        let local_from = from_utc.to_offset(offset);
                        let target_time = time.to_time().ok()?;

                        let skipped_date = match &self.state {
                            AlarmState::SkippedNext { skip_date } => Some(skip_date.as_str()),
                            _ => None,
                        };

                        // Include yesterday for a trigger whose grace spans midnight.
                        for day_offset in -1..=7 {
                            let Some(candidate_date) =
                                local_from.date().checked_add(Duration::days(day_offset))
                            else {
                                continue;
                            };
                            let candidate_day = DayOfWeek::from_weekday(candidate_date.weekday());

                            if !days.contains(&candidate_day) {
                                continue;
                            }

                            let date_str = format!(
                                "{:04}-{:02}-{:02}",
                                candidate_date.year(),
                                candidate_date.month() as u8,
                                candidate_date.day()
                            );
                            if skipped_date == Some(&date_str) {
                                continue;
                            }

                            let candidate_local =
                                candidate_date.with_time(target_time).assume_offset(offset);
                            let candidate_utc = candidate_local.to_offset(UtcOffset::UTC);

                            if candidate_utc >= self.created_at_utc
                                && self
                                    .last_triggered_utc
                                    .is_none_or(|last| candidate_utc > last)
                                && from_utc - candidate_utc <= TRIGGER_GRACE
                            {
                                return Some(candidate_utc);
                            }
                        }
                        None
                    }
                }
            }
        }
    }
}

/// Parse supported fixed zones and explicit numeric offsets for new schedules.
/// Returns None for unknown zones and names that would require DST rules.
pub fn try_parse_timezone_offset(zone: &str) -> Option<UtcOffset> {
    match zone {
        "Asia/Tbilisi" | "GMT+4" | "UTC+4" => return UtcOffset::from_hms(4, 0, 0).ok(),
        "Europe/Moscow" | "MSK" | "GMT+3" | "UTC+3" => return UtcOffset::from_hms(3, 0, 0).ok(),
        "UTC" | "Z" => return Some(UtcOffset::UTC),
        _ => {}
    }
    let sign = match zone.as_bytes().first()? {
        b'+' => 1,
        b'-' => -1,
        _ => return None,
    };
    let numeric = &zone[1..];
    let (hour, minute) = numeric.split_once(':').unwrap_or((numeric, "00"));
    if hour.len() != 2
        || minute.len() != 2
        || !hour
            .bytes()
            .chain(minute.bytes())
            .all(|b| b.is_ascii_digit())
    {
        return None;
    }
    UtcOffset::from_hms(
        hour.parse::<i8>().ok()? * sign,
        minute.parse::<i8>().ok()? * sign,
        0,
    )
    .ok()
}

/// Evaluate previously persisted fixed-offset schedules with their original fallback.
/// New schedules should use [`try_parse_timezone_offset`].
pub fn parse_timezone_offset(zone: &str) -> UtcOffset {
    try_parse_timezone_offset(zone).unwrap_or_else(|| {
        let legacy_offset = if matches!(zone, "Europe/Paris" | "CET") {
            1
        } else {
            4
        };
        UtcOffset::from_hms(legacy_offset, 0, 0).expect("legacy fixed offset is valid")
    })
}

/// Summary presentation of an alarm for CLI and assistant speech
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AlarmSummary {
    pub id: AlarmId,
    pub label: Option<String>,
    pub schedule_desc: String,
    pub next_trigger_local: Option<String>,
    pub next_trigger_utc: Option<String>,
    pub status: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AlarmEvent {
    Triggered {
        id: AlarmId,
        label: Option<String>,
        melody_id: Option<String>,
    },
    Stopped {
        id: AlarmId,
    },
    Snoozed {
        id: AlarmId,
        until_utc: OffsetDateTime,
    },
    Dismissed {
        id: AlarmId,
    },
}
