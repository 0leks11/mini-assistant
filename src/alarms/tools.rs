//! Gemini Live typed tool declarations and request handlers for alarms.

use crate::alarms::clock::Clock;
use crate::alarms::manager::AlarmManager;
use crate::alarms::types::{
    parse_timezone_offset, try_parse_timezone_offset, Alarm, AlarmSchedule, AlarmState, DayOfWeek,
    TimeOfDay,
};
use crate::gemini::ToolCallRequest;
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::sync::OnceLock;
use time::{Date, OffsetDateTime, UtcOffset};
use tracing::{info, warn};

static VOICE_CALL_NAMESPACE: OnceLock<uuid::Uuid> = OnceLock::new();

/// Return Gemini Live function declarations for alarm operations
pub fn alarm_tool_declarations() -> Vec<Value> {
    vec![
        json!({
            "name": "create_alarm",
            "description": "Поставить локальный будильник по явной команде. Уточни неоднозначное время. Подтверди голосом сохранённые дату и время только при status=ok.",
            "parameters": {
                "type": "OBJECT",
                "properties": {
                    "time": {
                        "type": "STRING",
                        "description": "Время будильника в 24-часовом формате HH:MM (например '07:00', '19:30', '08:15')."
                    },
                    "date": {
                        "type": "STRING",
                        "description": "Для разового будильника: точная дата YYYY-MM-DD, 'today' для «сегодня» или 'tomorrow' для «завтра». Опусти только если пользователь не назвал дату. Тогда выбирается ближайшее будущее время."
                    },
                    "days_of_week": {
                        "type": "ARRAY",
                        "items": { "type": "STRING" },
                        "description": "Дни недели для повторяющегося будильника: 'mon', 'tue', 'wed', 'thu', 'fri', 'sat', 'sun', либо 'weekdays' (будни), 'weekends' (выходные), 'daily' (каждый день)."
                    },
                    "label": {
                        "type": "STRING",
                        "description": "Необязательная метка или название будильника (например 'на работу', 'подъем')."
                    }
                },
                "required": ["time"]
            }
        }),
        json!({
            "name": "list_alarms",
            "description": "Получить список активных и запланированных локальных будильников на Мини.",
            "parameters": {
                "type": "OBJECT",
                "properties": {
                    "include_inactive": {
                        "type": "BOOLEAN",
                        "description": "Включать ли отключенные или завершенные будильники (по умолчанию false)."
                    }
                }
            }
        }),
        json!({
            "name": "delete_alarm",
            "description": "Удалить расписание по ID, названию или времени. Если status=ambiguous, попроси выбрать конкретный будильник.",
            "parameters": {
                "type": "OBJECT",
                "properties": {
                    "alarm_id": {
                        "type": "STRING",
                        "description": "Идентификатор будильника для удаления."
                    },
                    "time": {
                        "type": "STRING",
                        "description": "Запланированное местное время в формате HH:MM."
                    },
                    "label": {
                        "type": "STRING",
                        "description": "Название будильника, если оно известно."
                    }
                }
            }
        }),
        json!({
            "name": "snooze_alarm",
            "description": "Отложить только текущий звонок будильника. Постоянное расписание сохраняется.",
            "parameters": {
                "type": "OBJECT",
                "properties": {
                    "duration_minutes": {
                        "type": "INTEGER",
                        "description": "Целое число от 1 до 60 минут, по умолчанию 10."
                    }
                }
            }
        }),
        json!({
            "name": "stop_alarm",
            "description": "Остановить и выключить текущий играющий сигнал будильника.",
            "parameters": {
                "type": "OBJECT",
                "properties": {}
            }
        }),
    ]
}

pub(crate) fn parse_time_str(s: &str) -> Option<TimeOfDay> {
    let (hour, minute) = s.trim().split_once(':')?;
    if !(1..=2).contains(&hour.len())
        || minute.len() != 2
        || !hour
            .bytes()
            .chain(minute.bytes())
            .all(|b| b.is_ascii_digit())
    {
        return None;
    }
    TimeOfDay::new(hour.parse().ok()?, minute.parse().ok()?, 0)
}

pub(crate) fn parse_date_str(s: &str) -> Option<Date> {
    Date::parse(s, time::macros::format_description!("[year]-[month]-[day]")).ok()
}

pub(crate) fn parse_days_of_week(vals: &[Value]) -> Result<BTreeSet<DayOfWeek>, &'static str> {
    let mut days = BTreeSet::new();
    for v in vals {
        let Some(day) = v.as_str() else {
            return Err("Укажите дни недели строками.");
        };
        let single_day = match day.trim().to_lowercase().as_str() {
            "mon" | "monday" | "пн" | "понедельник" => DayOfWeek::Monday,
            "tue" | "tuesday" | "вт" | "вторник" => DayOfWeek::Tuesday,
            "wed" | "wednesday" | "ср" | "среда" => DayOfWeek::Wednesday,
            "thu" | "thursday" | "чт" | "четверг" => DayOfWeek::Thursday,
            "fri" | "friday" | "пт" | "пятница" => DayOfWeek::Friday,
            "sat" | "saturday" | "сб" | "суббота" => DayOfWeek::Saturday,
            "sun" | "sunday" | "вс" | "воскресенье" => DayOfWeek::Sunday,
            "weekdays" | "будни" | "по будням" => {
                days.extend(DayOfWeek::weekdays());
                continue;
            }
            "weekends" | "выходные" | "по выходным" => {
                days.extend(DayOfWeek::weekends());
                continue;
            }
            "daily" | "everyday" | "ежедневно" | "каждый день" => {
                days.extend(DayOfWeek::all());
                continue;
            }
            _ => return Err("Не удалось распознать один из дней повтора. Уточните дни недели."),
        };
        days.insert(single_day);
    }
    if days.is_empty() {
        return Err("Укажите хотя бы один день повтора.");
    }
    Ok(days)
}

fn optional_text<'a>(
    args: &'a Value,
    key: &str,
    message: &'static str,
) -> Result<Option<&'a str>, &'static str> {
    args.get(key)
        .map(|value| {
            value
                .as_str()
                .map(str::trim)
                .filter(|s| !s.is_empty() && s.len() <= 128)
                .ok_or(message)
        })
        .transpose()
}

fn parse_schedule(
    args: &Value,
    now_utc: OffsetDateTime,
    timezone: &str,
) -> Result<AlarmSchedule, &'static str> {
    let offset = try_parse_timezone_offset(timezone)
        .ok_or("Часовой пояс Mini не поддерживается. Проверьте настройку времени.")?;
    let time = args
        .get("time")
        .and_then(Value::as_str)
        .and_then(parse_time_str)
        .ok_or("Укажите время в 24-часовом формате, например 07:00.")?;
    let date = optional_text(args, "date", "Укажите дату в формате YYYY-MM-DD.")?;
    if let Some(days) = args.get("days_of_week") {
        if date.is_some() {
            return Err("Выберите разовый будильник на дату или повтор по дням недели.");
        }
        let days = parse_days_of_week(days.as_array().ok_or("Укажите список дней повтора.")?)?;
        return Ok(AlarmSchedule::Recurring {
            time,
            days: days.into_iter().collect(),
            timezone: timezone.to_owned(),
        });
    }

    let now_local = now_utc.to_offset(offset);
    let today = now_local.date();
    let tomorrow = || today.next_day().ok_or("Дата выходит за пределы календаря.");
    let time = time.to_time().map_err(|_| "Укажите корректное время.")?;
    let date = match date {
        Some(date) => match date.to_lowercase().as_str() {
            "today" | "сегодня" => today,
            "tomorrow" | "завтра" => tomorrow()?,
            _ => parse_date_str(date).ok_or("Укажите корректную дату в формате YYYY-MM-DD.")?,
        },
        None if time > now_local.time() => today,
        None => tomorrow()?,
    };
    let target_utc = date
        .with_time(time)
        .assume_offset(offset)
        .checked_to_offset(UtcOffset::UTC)
        .ok_or("Дата выходит за пределы календаря.")?;
    Ok(AlarmSchedule::Single {
        target_utc,
        timezone: timezone.to_owned(),
    })
}

fn alarm_timezone(alarm: &Alarm) -> &str {
    match &alarm.schedule {
        AlarmSchedule::Single { timezone, .. } | AlarmSchedule::Recurring { timezone, .. } => {
            timezone
        }
    }
}

fn alarm_selection(alarm: &Alarm) -> Value {
    match &alarm.schedule {
        AlarmSchedule::Single {
            target_utc,
            timezone,
        } => {
            let local = target_utc.checked_to_offset(parse_timezone_offset(timezone));
            json!({
                "id": alarm.id,
                "label": alarm.label,
                "schedule_type": "single",
                "time": local.map(|time| format!("{:02}:{:02}", time.hour(), time.minute())),
                "date": local.map(|time| time.date().to_string()),
                "timezone": timezone,
            })
        }
        AlarmSchedule::Recurring {
            time,
            days,
            timezone,
        } => json!({
            "id": alarm.id,
            "label": alarm.label,
            "schedule_type": "recurring",
            "time": format!("{:02}:{:02}", time.hour, time.minute),
            "days_of_week": days,
            "timezone": timezone,
        }),
    }
}

fn scheduled_time(alarm: &Alarm) -> Option<(u8, u8)> {
    match &alarm.schedule {
        AlarmSchedule::Single {
            target_utc,
            timezone,
        } => target_utc
            .checked_to_offset(parse_timezone_offset(timezone))
            .map(|local| (local.hour(), local.minute())),
        AlarmSchedule::Recurring { time, .. } => Some((time.hour, time.minute)),
    }
}

fn error(message: impl std::fmt::Display) -> Value {
    json!({ "status": "error", "message": message.to_string() })
}

fn not_found(message: &str) -> Value {
    json!({ "status": "not_found", "message": message })
}

fn delete_alarm<C: Clock>(manager: &AlarmManager<C>, args: &Value) -> Result<Value, anyhow::Error> {
    let id = optional_text(args, "alarm_id", "Укажите корректный ID будильника.")
        .map_err(anyhow::Error::msg)?;
    let label = optional_text(args, "label", "Укажите название будильника.")
        .map_err(anyhow::Error::msg)?
        .map(str::to_lowercase);
    let time = optional_text(args, "time", "Укажите время в формате 07:00.")
        .map_err(anyhow::Error::msg)?
        .map(|time| {
            parse_time_str(time)
                .map(|time| (time.hour, time.minute))
                .ok_or_else(|| anyhow::anyhow!("Укажите время в формате 07:00."))
        })
        .transpose()?;

    if id.is_none() && label.is_none() && time.is_none() {
        return Ok(error("Укажите ID, название или время будильника."));
    }

    let alarms = manager.alarms_snapshot()?;
    let mut matches: Vec<&Alarm> = alarms
        .iter()
        .filter(|alarm| match id {
            Some(id) => alarm.id.starts_with(id),
            None => !matches!(alarm.state, AlarmState::Completed { .. }),
        })
        .filter(|alarm| {
            label.as_ref().is_none_or(|label| {
                alarm
                    .label
                    .as_ref()
                    .is_some_and(|name| name.to_lowercase() == *label)
            })
        })
        .filter(|alarm| time.is_none_or(|time| scheduled_time(alarm) == Some(time)))
        .collect();
    match matches.as_slice() {
        [] => Ok(not_found("Подходящих будильников нет.")),
        [matched] => {
            if manager.delete_alarm(&matched.id)? {
                Ok(json!({
                    "status": "ok",
                    "deleted_id": matched.id,
                    "message": "Будильник удалён."
                }))
            } else {
                Ok(not_found("Подходящих будильников нет."))
            }
        }
        _ => {
            matches.sort_by(|a, b| a.id.cmp(&b.id));
            Ok(json!({
                "status": "ambiguous",
                "message": "Подходит несколько будильников. Уточните название, дату или дни повтора.",
                "matches": matches.into_iter().map(alarm_selection).collect::<Vec<_>>(),
            }))
        }
    }
}

/// Execute an alarm request. A known tool always returns its actual success or error status.
/// The player consumes manager events after the change has been saved.
pub fn handle_alarm_tool_call<C: Clock>(
    manager: &AlarmManager<C>,
    clock: &C,
    req: &ToolCallRequest,
    default_tz: &str,
) -> Option<Value> {
    if matches!(
        req.function_name.as_str(),
        "create_alarm" | "delete_alarm" | "snooze_alarm" | "stop_alarm"
    ) && (req.call_id.trim().is_empty() || req.call_id.len() > 128)
    {
        return Some(error("Некорректный идентификатор запроса."));
    }
    let result = match req.function_name.as_str() {
        "create_alarm" => {
            let now_utc = clock.now_utc();
            let schedule = match parse_schedule(&req.args, now_utc, default_tz) {
                Ok(schedule) => schedule,
                Err(message) => return Some(error(message)),
            };
            let label =
                match optional_text(&req.args, "label", "Укажите короткое название будильника.")
                {
                    Ok(label) => label.map(str::to_owned),
                    Err(message) => return Some(error(message)),
                };

            let namespace = VOICE_CALL_NAMESPACE.get_or_init(uuid::Uuid::new_v4);
            let key = format!("voice:{namespace}:{}:{}", req.turn_id, req.call_id);
            match manager.create_alarm(schedule, label, None, Some(key)) {
                Ok(alarm) => {
                    let mut result = alarm_selection(&alarm);
                    result["status"] = json!("ok");
                    result["next_trigger_local"] = json!(alarm
                        .next_trigger_utc(now_utc)
                        .and_then(|next| next
                            .checked_to_offset(parse_timezone_offset(alarm_timezone(&alarm))))
                        .map(|local| {
                            format!(
                                "{:02}:{:02} ({})",
                                local.hour(),
                                local.minute(),
                                local.date()
                            )
                        }));
                    info!("Assistant created alarm: id={}", alarm.id);
                    Ok(result)
                }
                Err(err) => Err(err),
            }
        }
        "list_alarms" => {
            let include_inactive = match req.args.get("include_inactive") {
                Some(value) => match value.as_bool() {
                    Some(value) => value,
                    None => {
                        return Some(error("Параметр include_inactive должен быть логическим."))
                    }
                },
                None => false,
            };
            manager
                .try_list_alarms(include_inactive)
                .map(|alarms| json!({ "status": "ok", "count": alarms.len(), "alarms": alarms }))
        }
        "delete_alarm" => delete_alarm(manager, &req.args),
        "snooze_alarm" => {
            let minutes = match req.args.get("duration_minutes") {
                None => 10,
                Some(value) => match value.as_u64() {
                    Some(value @ 1..=60) => value as u32,
                    _ => return Some(error("Укажите целое число от 1 до 60 минут.")),
                },
            };
            manager
                .try_active_ringing_id()
                .and_then(|active| match active {
                    Some(id) => manager.snooze(&id, minutes).map(|alarm| {
                        json!({
                            "status": "ok",
                            "id": alarm.id,
                            "message": format!("Будильник отложен на {minutes} мин.")
                        })
                    }),
                    None => Ok(not_found("Сейчас ни один будильник не звонит.")),
                })
        }
        "stop_alarm" => manager.stop(None).map(|stopped| match stopped {
            Some(alarm) => json!({
                "status": "ok",
                "id": alarm.id,
                "message": "Звонок будильника остановлен."
            }),
            None => not_found("Сейчас ни один будильник не звонит."),
        }),
        _ => return None,
    };

    Some(result.unwrap_or_else(|err| {
        warn!("Alarm tool failed");
        error(err)
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::alarms::clock::TestClock;
    use crate::alarms::storage::AlarmStore;
    use std::sync::Arc;
    use time::macros::datetime;

    struct Fixture {
        _directory: TestTempDir,
        clock: Arc<TestClock>,
        manager: AlarmManager<TestClock>,
    }

    impl Fixture {
        fn new() -> Self {
            let directory = TestTempDir::new();
            let store = Arc::new(AlarmStore::new(directory.0.clone()).unwrap());
            let clock = Arc::new(TestClock::new(datetime!(2026-11-05 01:00:00 UTC)));
            let manager = AlarmManager::new(clock.clone(), store).unwrap();
            Self {
                _directory: directory,
                clock,
                manager,
            }
        }

        fn call_in_zone(&self, name: &str, args: Value, zone: &str) -> Value {
            let request = ToolCallRequest {
                turn_id: 1,
                call_id: uuid::Uuid::new_v4().to_string(),
                function_name: name.to_string(),
                args,
                reply_tx: tokio::sync::oneshot::channel().0,
            };
            handle_alarm_tool_call(&self.manager, &*self.clock, &request, zone).unwrap()
        }

        fn call(&self, name: &str, args: Value) -> Value {
            self.call_in_zone(name, args, "Asia/Tbilisi")
        }
    }

    struct TestTempDir(std::path::PathBuf);

    impl TestTempDir {
        fn new() -> Self {
            let p = std::env::temp_dir().join(format!("alarm-tools-test-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&p).unwrap();
            Self(p)
        }
    }

    impl Drop for TestTempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn explicit_tomorrow_is_not_today_even_before_the_requested_hour() {
        let fixture = Fixture::new();
        let tomorrow = fixture.call("create_alarm", json!({"time": "07:00", "date": "tomorrow"}));
        assert_eq!(tomorrow["status"], "ok");
        assert_eq!(tomorrow["date"], "2026-11-06");
        assert_eq!(tomorrow["next_trigger_local"], "07:00 (2026-11-06)");

        let exact = fixture.call(
            "create_alarm",
            json!({"time": "06:30", "date": "2026-11-09"}),
        );
        assert_eq!(exact["status"], "ok");
        assert_eq!(exact["date"], "2026-11-09");

        let next = fixture.call("create_alarm", json!({"time": "07:00"}));
        assert_eq!(next["date"], "2026-11-05");
    }

    #[test]
    fn malformed_past_and_conflicting_schedules_never_create_an_alarm() {
        let fixture = Fixture::new();
        for args in [
            json!({"time": "07:00", "date": "2026-02-30"}),
            json!({"time": "07:00", "date": 20261106}),
            json!({"time": "07:00", "date": null}),
            json!({"time": "04:59", "date": "today"}),
            json!({"time": "07:00", "days_of_week": ["mon", "mno"]}),
            json!({"time": "07:00", "days_of_week": []}),
            json!({"time": "07:00", "days_of_week": ["daily"], "date": "tomorrow"}),
            json!({"time": "7"}),
        ] {
            assert_eq!(fixture.call("create_alarm", args)["status"], "error");
        }
        assert_eq!(
            fixture.call_in_zone("create_alarm", json!({"time": "07:00"}), "Europe/Paris")
                ["status"],
            "error",
        );
        assert_eq!(fixture.call("list_alarms", json!({}))["count"], 0);
    }

    #[test]
    fn voice_create_replays_only_the_same_call_in_the_same_turn() {
        let fixture = Fixture::new();
        let mut request = ToolCallRequest {
            turn_id: 1,
            call_id: "same-call".to_string(),
            function_name: "create_alarm".to_string(),
            args: json!({"time": "07:00", "date": "today"}),
            reply_tx: tokio::sync::oneshot::channel().0,
        };
        let call = |request: &ToolCallRequest| {
            handle_alarm_tool_call(&fixture.manager, &*fixture.clock, request, "Asia/Tbilisi")
                .unwrap()
        };
        let first = call(&request);
        assert_eq!(first["status"], "ok");
        assert_eq!(call(&request)["id"], first["id"]);
        fixture.clock.advance(std::time::Duration::from_secs(7201));
        assert_eq!(call(&request)["id"], first["id"]);
        assert_eq!(fixture.call("list_alarms", json!({}))["count"], 1);

        request.turn_id += 1;
        request.args["date"] = json!("tomorrow");
        let second = call(&request);
        assert_eq!(second["status"], "ok");
        assert_ne!(second["id"], first["id"]);

        request.call_id = "x".repeat(129);
        assert_eq!(call(&request)["status"], "error");
        assert_eq!(fixture.call("list_alarms", json!({}))["count"], 2);
    }

    #[test]
    fn delete_by_time_preserves_ambiguous_alarms_until_disambiguated() {
        let fixture = Fixture::new();
        let workout = fixture.call(
            "create_alarm",
            json!({"time": "07:30", "days_of_week": ["weekdays"], "label": "Тренировка"}),
        );
        assert_eq!(workout["status"], "ok");
        let travel = fixture.call(
            "create_alarm",
            json!({"time": "07:30", "date": "tomorrow", "label": "Поездка"}),
        );
        assert_eq!(travel["status"], "ok");

        let ambiguous = fixture.call("delete_alarm", json!({"time": "07:30"}));
        assert_eq!(ambiguous["status"], "ambiguous");
        assert_eq!(ambiguous["matches"].as_array().unwrap().len(), 2);
        assert_eq!(fixture.call("list_alarms", json!({}))["count"], 2);

        let deleted = fixture.call(
            "delete_alarm",
            json!({"time": "7:30", "label": "тренировка"}),
        );
        assert_eq!(deleted["status"], "ok");
        assert_eq!(deleted["deleted_id"], workout["id"]);
        let remaining = fixture.call("list_alarms", json!({}));
        assert_eq!(remaining["count"], 1);
        assert_eq!(remaining["alarms"][0]["id"], travel["id"]);
    }

    #[test]
    fn stop_and_snooze_require_an_actual_ring() {
        let fixture = Fixture::new();
        let scheduled = fixture.call("create_alarm", json!({"time": "05:01"}));
        assert_eq!(scheduled["status"], "ok");
        assert_eq!(fixture.call("stop_alarm", json!({}))["status"], "not_found");
        assert_eq!(
            fixture.call("snooze_alarm", json!({}))["status"],
            "not_found"
        );
        assert_eq!(
            fixture.call("snooze_alarm", json!({"duration_minutes": 4294967297_u64}))["status"],
            "error"
        );
        let alarms = fixture.call("list_alarms", json!({}));
        assert_eq!(
            alarms["alarms"][0]["next_trigger_local"],
            "05:01 (2026-11-05)"
        );

        fixture.clock.advance(std::time::Duration::from_secs(60));
        fixture.manager.tick();
        let snoozed = fixture.call("snooze_alarm", json!({"duration_minutes": 5}));
        assert_eq!(snoozed["status"], "ok");
        assert_eq!(snoozed["id"], scheduled["id"]);
        assert_eq!(fixture.call("stop_alarm", json!({}))["status"], "not_found");

        fixture.clock.advance(std::time::Duration::from_secs(300));
        fixture.manager.tick();
        let stopped = fixture.call("stop_alarm", json!({}));
        assert_eq!(stopped["status"], "ok");
        assert_eq!(stopped["id"], scheduled["id"]);
        assert_eq!(fixture.call("list_alarms", json!({}))["count"], 0);
    }
}
