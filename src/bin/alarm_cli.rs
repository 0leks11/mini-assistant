//! CLI utility for local alarm management on Station Mini and Mac development host.

use mini_assistant::alarms::{
    types::try_parse_timezone_offset, AlarmManager, AlarmSchedule, AlarmStore, DayOfWeek,
    SystemClock, TimeOfDay,
};
use std::collections::BTreeSet;
use std::env;
use std::path::PathBuf;
use std::sync::Arc;
use time::{Date, Month, PrimitiveDateTime, Time};

fn default_storage_dir() -> PathBuf {
    if let Ok(dir) = env::var("MINI_ALARMS_DIR") {
        return PathBuf::from(dir);
    }
    let target_dir = PathBuf::from("/bkupgrade/data/opt/mini-assistant/alarms");
    if target_dir.parent().map(|p| p.exists()).unwrap_or(false) {
        target_dir
    } else {
        PathBuf::from("./data/alarms")
    }
}

fn print_help() {
    println!(
        r#"Station Mini Local Alarm CLI

Usage:
  alarm-cli list [--all]
  alarm-cli add --time <HH:MM> [--days <weekdays|weekends|daily|mon,tue,...>] [--label <NAME>] [--tz <TIMEZONE>]
  alarm-cli add-single --datetime <YYYY-MM-DDTHH:MM> [--label <NAME>] [--tz <TIMEZONE>]
  alarm-cli delete <ID>
  alarm-cli stop [<ID>]
  alarm-cli snooze <ID> [--minutes <N>]
  alarm-cli skip <ID>

Options:
  --dir <PATH>   Custom alarms storage directory (default: MINI_ALARMS_DIR or /bkupgrade/data/opt/mini-assistant/alarms)
  -h, --help     Show this help message
"#
    );
}

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 || args[1] == "-h" || args[1] == "--help" {
        print_help();
        return Ok(());
    }

    let mut storage_dir = default_storage_dir();
    let mut command_args = Vec::new();
    let mut i = 1;

    while i < args.len() {
        if args[i] == "--dir" && i + 1 < args.len() {
            storage_dir = PathBuf::from(&args[i + 1]);
            i += 2;
        } else {
            command_args.push(args[i].clone());
            i += 1;
        }
    }

    if command_args.is_empty() {
        print_help();
        return Ok(());
    }

    let clock = Arc::new(SystemClock);
    let store = Arc::new(AlarmStore::new(&storage_dir)?);
    let manager = AlarmManager::new(clock.clone(), store)?;

    let action = command_args[0].as_str();

    match action {
        "list" => {
            let include_all = command_args.iter().any(|a| a == "--all");
            let alarms = manager.try_list_alarms(include_all)?;
            if alarms.is_empty() {
                println!("Нет активных будильников.");
            } else {
                println!("Список будильников (хранилище: {}):", storage_dir.display());
                for a in alarms {
                    let label = a.label.map(|l| format!(" [{}]", l)).unwrap_or_default();
                    let next = a.next_trigger_local.unwrap_or_else(|| "нет".to_string());
                    println!(
                        "• [{}] {}{} — след: {} ({})",
                        &a.id[..a.id.len().min(8)],
                        a.schedule_desc,
                        label,
                        next,
                        a.status
                    );
                }
            }
        }
        "add" => {
            let mut time_str = None;
            let mut days_str = None;
            let mut label = None;
            let mut tz = "Asia/Tbilisi".to_string();

            let mut idx = 1;
            while idx < command_args.len() {
                match command_args[idx].as_str() {
                    "--time" if idx + 1 < command_args.len() => {
                        time_str = Some(command_args[idx + 1].clone());
                        idx += 2;
                    }
                    "--days" if idx + 1 < command_args.len() => {
                        days_str = Some(command_args[idx + 1].clone());
                        idx += 2;
                    }
                    "--label" if idx + 1 < command_args.len() => {
                        label = Some(command_args[idx + 1].clone());
                        idx += 2;
                    }
                    "--tz" if idx + 1 < command_args.len() => {
                        tz = command_args[idx + 1].clone();
                        idx += 2;
                    }
                    _ => idx += 1,
                }
            }

            let time_val =
                time_str.ok_or_else(|| anyhow::anyhow!("Параметр --time обязателен (HH:MM)"))?;
            let parts: Vec<&str> = time_val.split(':').collect();
            if parts.len() != 2 {
                anyhow::bail!("Неверный формат времени: {}", time_val);
            }
            let hour: u8 = parts[0].parse()?;
            let minute: u8 = parts[1].parse()?;
            let tod = TimeOfDay::new(hour, minute, 0)
                .ok_or_else(|| anyhow::anyhow!("Недопустимое время: {}:{}", hour, minute))?;

            let days: BTreeSet<DayOfWeek> = match days_str.as_deref() {
                None | Some("daily") | Some("каждый день") => DayOfWeek::all(),
                Some("weekdays") | Some("будни") => DayOfWeek::weekdays(),
                Some("weekends") | Some("выходные") => DayOfWeek::weekends(),
                Some(custom) => {
                    let mut set = BTreeSet::new();
                    for d in custom.split(',') {
                        match d.trim().to_lowercase().as_str() {
                            "mon" | "пн" => set.insert(DayOfWeek::Monday),
                            "tue" | "вт" => set.insert(DayOfWeek::Tuesday),
                            "wed" | "ср" => set.insert(DayOfWeek::Wednesday),
                            "thu" | "чт" => set.insert(DayOfWeek::Thursday),
                            "fri" | "пт" => set.insert(DayOfWeek::Friday),
                            "sat" | "сб" => set.insert(DayOfWeek::Saturday),
                            "sun" | "вс" => set.insert(DayOfWeek::Sunday),
                            other => anyhow::bail!("Неизвестный день недели: {}", other),
                        };
                    }
                    set
                }
            };

            let alarm = manager.create_alarm(
                AlarmSchedule::Recurring {
                    time: tod,
                    days,
                    timezone: tz,
                },
                label,
                None,
                None,
            )?;

            println!("Будильник успешно создан! ID: {}", alarm.id);
        }
        "add-single" => {
            let mut dt_str = None;
            let mut label = None;
            let mut tz = "Asia/Tbilisi".to_string();

            let mut idx = 1;
            while idx < command_args.len() {
                match command_args[idx].as_str() {
                    "--datetime" if idx + 1 < command_args.len() => {
                        dt_str = Some(command_args[idx + 1].clone());
                        idx += 2;
                    }
                    "--label" if idx + 1 < command_args.len() => {
                        label = Some(command_args[idx + 1].clone());
                        idx += 2;
                    }
                    "--tz" if idx + 1 < command_args.len() => {
                        tz = command_args[idx + 1].clone();
                        idx += 2;
                    }
                    _ => idx += 1,
                }
            }

            let dt_val = dt_str.ok_or_else(|| {
                anyhow::anyhow!("Параметр --datetime обязателен (YYYY-MM-DDTHH:MM)")
            })?;
            let offset = try_parse_timezone_offset(&tz)
                .ok_or_else(|| anyhow::anyhow!("Неподдерживаемый часовой пояс"))?;

            let parts: Vec<&str> = dt_val.split('T').collect();
            if parts.len() != 2 {
                anyhow::bail!("Ожидается формат YYYY-MM-DDTHH:MM");
            }
            let d_parts: Vec<&str> = parts[0].split('-').collect();
            let t_parts: Vec<&str> = parts[1].split(':').collect();
            if d_parts.len() != 3 || t_parts.len() != 2 {
                anyhow::bail!("Неверный формат даты/времени: {}", dt_val);
            }

            let year: i32 = d_parts[0].parse()?;
            let month: u8 = d_parts[1].parse()?;
            let day: u8 = d_parts[2].parse()?;
            let hour: u8 = t_parts[0].parse()?;
            let minute: u8 = t_parts[1].parse()?;

            let month_enum = Month::try_from(month)?;
            let date = Date::from_calendar_date(year, month_enum, day)?;
            let time = Time::from_hms(hour, minute, 0)?;
            let local_dt = PrimitiveDateTime::new(date, time).assume_offset(offset);
            let target_utc = local_dt.to_offset(time::UtcOffset::UTC);

            let alarm = manager.create_alarm(
                AlarmSchedule::Single {
                    target_utc,
                    timezone: tz,
                },
                label,
                None,
                None,
            )?;

            println!(
                "Разовый будильник установлен на {}! ID: {}",
                local_dt, alarm.id
            );
        }
        "delete" => {
            let id = command_args
                .get(1)
                .ok_or_else(|| anyhow::anyhow!("Укажите ID будильника"))?;
            if manager.delete_alarm(id)? {
                println!("Будильник '{}' удален.", id);
            } else {
                println!("Будильник '{}' не найден.", id);
            }
        }
        "stop" => {
            let id = command_args.get(1).map(|s| s.as_str());
            if let Some(stopped) = manager.stop(id)? {
                println!("Будильник '{}' остановлен.", stopped.id);
            } else {
                println!("Нет активного звонящего будильника.");
            }
        }
        "snooze" => {
            let id = command_args
                .get(1)
                .ok_or_else(|| anyhow::anyhow!("Укажите ID будильника"))?;
            let mut minutes = 10;
            if let Some(pos) = command_args.iter().position(|a| a == "--minutes") {
                if pos + 1 < command_args.len() {
                    minutes = command_args[pos + 1].parse()?;
                }
            }
            let snoozed = manager.snooze(id, minutes)?;
            println!("Будильник '{}' отложен на {} минут.", snoozed.id, minutes);
        }
        "skip" => {
            let id = command_args
                .get(1)
                .ok_or_else(|| anyhow::anyhow!("Укажите ID будильника"))?;
            let skipped = manager.skip_next(id)?;
            println!(
                "Следующее срабатывание будильника '{}' пропущено.",
                skipped.id
            );
        }
        other => {
            anyhow::bail!("Неизвестная команда: '{}'. Запустите с --help.", other);
        }
    }

    Ok(())
}
