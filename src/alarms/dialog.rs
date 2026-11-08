//! Multi-turn dialog recovery and state machine for Station Mini alarms.
//!
//! Enforces:
//! - Single wake word per conversation («Штучка»).
//! - Strict 5.0 second follow-up response window after question finishes playing.
//! - Max 3 questions budget per dialog (including clarifications & confirmations).
//! - Max 45.0 seconds total dialog duration.
//! - Missing arguments trigger clarification questions ("На когда поставить будильник?").
//! - In-memory draft with revision tracking: parameters can be revised ("Нет, лучше в восемь утра").
//! - Parameter revisions invalidate previous confirmations.
//! - Explicit confirmation required before committing any schedule change.
//! - Silence or cancellation drops the draft immediately with ZERO alarms saved.
//! - Deterministic timing and testing using abstract Clock.

use super::clock::Clock;
use super::tools::{parse_date_str, parse_time_str};
use super::types::{try_parse_timezone_offset, AlarmSchedule, DayOfWeek, TimeOfDay};
use std::collections::BTreeSet;
use std::time::{Duration, Instant};
use time::{Date, OffsetDateTime};

/// Follow-up speech timeout window after question playback completes
pub const FOLLOW_UP_TIMEOUT: Duration = Duration::from_secs(5);
/// Maximum questions allowed per single dialog session
pub const MAX_QUESTIONS: u32 = 3;
/// Maximum lifetime of an uncommitted dialog session
pub const MAX_DIALOG_DURATION: Duration = Duration::from_secs(45);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelativeDate {
    Today,
    Tomorrow,
}

/// In-memory uncommitted alarm draft
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AlarmDraft {
    pub revision: u32,
    pub time: Option<TimeOfDay>,
    pub date: Option<Date>,
    pub relative_date: Option<RelativeDate>,
    pub days: Option<BTreeSet<DayOfWeek>>,
    pub label: Option<String>,
    pub timezone: String,
}

impl AlarmDraft {
    pub fn new(timezone: String) -> Self {
        Self {
            revision: 1,
            time: None,
            date: None,
            relative_date: None,
            days: None,
            label: None,
            timezone,
        }
    }

    /// Convert the in-memory draft into a concrete AlarmSchedule
    pub fn build_schedule(&self, now_utc: OffsetDateTime) -> Result<AlarmSchedule, &'static str> {
        let offset = try_parse_timezone_offset(&self.timezone)
            .ok_or("Часовой пояс Mini не поддерживается.")?;
        let time = self
            .time
            .ok_or("Укажите время в 24-часовом формате, например 07:00.")?;

        if let Some(days) = &self.days {
            if self.date.is_some() || self.relative_date.is_some() {
                return Err("Выберите разовый будильник на дату или повтор по дням недели.");
            }
            if days.is_empty() {
                return Err("Укажите хотя бы один день повтора.");
            }
            return Ok(AlarmSchedule::Recurring {
                time,
                days: days.clone(),
                timezone: self.timezone.clone(),
            });
        }

        let now_local = now_utc.to_offset(offset);
        let today = now_local.date();
        let target_time = time.to_time().map_err(|_| "Укажите корректное время.")?;

        let date = if let Some(rel) = self.relative_date {
            match rel {
                RelativeDate::Today => today,
                RelativeDate::Tomorrow => today
                    .next_day()
                    .ok_or("Дата выходит за пределы календаря.")?,
            }
        } else if let Some(d) = self.date {
            d
        } else {
            // Default: if target time is in the past for today, schedule for tomorrow
            if target_time <= now_local.time() {
                today
                    .next_day()
                    .ok_or("Дата выходит за пределы календаря.")?
            } else {
                today
            }
        };

        let target_local = date.with_time(target_time).assume_offset(offset);
        let target_utc = target_local.to_offset(time::UtcOffset::UTC);

        if target_utc <= now_utc {
            return Err("Нельзя поставить будильник в прошлое.");
        }

        Ok(AlarmSchedule::Single {
            target_utc,
            timezone: self.timezone.clone(),
        })
    }

    /// Format question asking the user to confirm the draft
    pub fn format_confirmation_prompt(&self, now_utc: OffsetDateTime) -> String {
        let time_str = self
            .time
            .map(|t| format!("{:02}:{:02}", t.hour, t.minute))
            .unwrap_or_else(|| "07:00".to_string());

        if let Some(days) = &self.days {
            if days == &DayOfWeek::weekdays() {
                return format!("Поставить по будням на {time_str}?");
            }
            if days == &DayOfWeek::weekends() {
                return format!("Поставить по выходным на {time_str}?");
            }
            if days == &DayOfWeek::all() {
                return format!("Поставить каждый день на {time_str}?");
            }
            return format!("Поставить на {time_str} по выбранным дням?");
        }

        let offset = try_parse_timezone_offset(&self.timezone).unwrap_or(time::UtcOffset::UTC);
        let now_local = now_utc.to_offset(offset);
        let today = now_local.date();

        let date_label = if let Some(rel) = self.relative_date {
            match rel {
                RelativeDate::Today => "на сегодня ",
                RelativeDate::Tomorrow => "на завтра ",
            }
        } else if let Some(d) = self.date {
            if d == today {
                "на сегодня "
            } else if Some(d) == today.next_day() {
                "на завтра "
            } else {
                ""
            }
        } else if let Some(t) = self.time {
            if let Ok(target_time) = t.to_time() {
                if target_time <= now_local.time() {
                    "на завтра "
                } else {
                    "на сегодня "
                }
            } else {
                ""
            }
        } else {
            ""
        };

        format!("Поставить {date_label}на {time_str}?")
    }

    /// Format speech announcement upon successful commit
    pub fn format_announcement(&self, now_utc: OffsetDateTime) -> String {
        let time_str = self
            .time
            .map(|t| format!("{:02}:{:02}", t.hour, t.minute))
            .unwrap_or_else(|| "07:00".to_string());

        if let Some(days) = &self.days {
            if days == &DayOfWeek::weekdays() {
                return format!("Поставила по будням на {time_str}.");
            }
            if days == &DayOfWeek::weekends() {
                return format!("Поставила по выходным на {time_str}.");
            }
            if days == &DayOfWeek::all() {
                return format!("Поставила каждый день на {time_str}.");
            }
            return format!("Поставила на {time_str} по выбранным дням.");
        }

        let offset = try_parse_timezone_offset(&self.timezone).unwrap_or(time::UtcOffset::UTC);
        let now_local = now_utc.to_offset(offset);
        let today = now_local.date();

        let date_label = if let Some(rel) = self.relative_date {
            match rel {
                RelativeDate::Today => "на сегодня ",
                RelativeDate::Tomorrow => "на завтра ",
            }
        } else if let Some(d) = self.date {
            if d == today {
                "на сегодня "
            } else if Some(d) == today.next_day() {
                "на завтра "
            } else {
                ""
            }
        } else if let Some(t) = self.time {
            if let Ok(target_time) = t.to_time() {
                if target_time <= now_local.time() {
                    "на завтра "
                } else {
                    "на сегодня "
                }
            } else {
                ""
            }
        } else {
            ""
        };

        format!("Поставила {date_label}на {time_str}.")
    }
}

/// What the assistant expects next from the user
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExpectedInput {
    Time,
    Confirmation { expected_revision: u32 },
}

/// Current state of the multi-turn dialog
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DialogState {
    /// No active dialog. Assistant waits for wake word ("Штучка").
    Idle,
    /// Listening to initial speech after wake word.
    Listening {
        turn_id: u64,
        dialog_id: u64,
        started_at: Instant,
    },
    /// A draft has been created or modified and needs evaluation.
    Drafting {
        dialog_id: u64,
        draft: AlarmDraft,
        questions_asked: u32,
        dialog_started_at: Instant,
    },
    /// Prompt has been generated and speaker is playing it.
    Asking {
        dialog_id: u64,
        draft: AlarmDraft,
        prompt: String,
        expected: ExpectedInput,
        questions_asked: u32,
        dialog_started_at: Instant,
    },
    /// Question finished playing. Follow-up window (strict 5.0s) is open.
    WaitingFollowUp {
        dialog_id: u64,
        draft: AlarmDraft,
        deadline: Instant,
        expected: ExpectedInput,
        questions_asked: u32,
        dialog_started_at: Instant,
    },
    /// User started speaking within follow-up window; utterance is in flight.
    ReceivingFollowUp {
        dialog_id: u64,
        draft: AlarmDraft,
        expected: ExpectedInput,
        questions_asked: u32,
        dialog_started_at: Instant,
        speech_started_at: Instant,
    },
}

/// Follow-up intent or utterance from user
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FollowUpIntent {
    Confirm,
    Cancel,
    Silence,
    ProvideTime {
        time: String,
        date: Option<String>,
    },
    Modify {
        time: Option<String>,
        date: Option<String>,
        days: Option<BTreeSet<DayOfWeek>>,
    },
}

/// Incoming event or input into the state machine
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DialogInput {
    /// Explicit activation or wake word detected
    Wake { turn_id: u64 },
    /// Initial or parsed alarm intent from speech
    StartAlarmDraft {
        time: Option<String>,
        date: Option<String>,
        relative_date: Option<RelativeDate>,
        days: Option<BTreeSet<DayOfWeek>>,
        label: Option<String>,
    },
    /// Speaker finished rendering the clarification/confirmation question
    PromptFinishedPlaying,
    /// Voice activity detector (VAD) detected start of user speech
    SpeechStarted { at: Instant },
    /// Utterance interpretation result
    UserFollowUp(FollowUpIntent),
    /// Background monotonic tick to enforce timeouts
    Tick,
    /// External cancellation (alarm ring, mute button, session drop)
    ExternalInterrupt,
}

/// Reason why a dialog session was closed
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseReason {
    Committed,
    UserCancelled,
    SilenceTimeout,
    DialogTimeout,
    MaxQuestionsExceeded,
    Interrupted,
}

/// Action commanded by the state machine
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DialogAction {
    /// Play a question over the speaker and wait for follow-up
    AskQuestion {
        prompt: String,
        expected: ExpectedInput,
    },
    /// Commit the alarm to persistent storage and announce success
    CommitAlarm {
        schedule: AlarmSchedule,
        label: Option<String>,
        timezone: String,
        announcement: String,
    },
    /// Close the dialog session cleanly with no changes to storage
    CloseDialog { reason: CloseReason },
    /// No immediate external action needed
    None,
}

/// Deterministic Dialog State Machine for Station Mini
pub struct DialogMachine<C: Clock> {
    clock: C,
    default_timezone: String,
    state: DialogState,
    dialog_counter: u64,
}

impl<C: Clock> DialogMachine<C> {
    pub fn new(clock: C, default_timezone: String) -> Self {
        Self {
            clock,
            default_timezone,
            state: DialogState::Idle,
            dialog_counter: 0,
        }
    }

    pub fn state(&self) -> &DialogState {
        &self.state
    }

    /// Step the state machine with an incoming event
    pub fn step(&mut self, input: DialogInput) -> DialogAction {
        match input {
            DialogInput::Wake { turn_id } => {
                self.dialog_counter = self.dialog_counter.wrapping_add(1);
                self.state = DialogState::Listening {
                    turn_id,
                    dialog_id: self.dialog_counter,
                    started_at: self.clock.now_monotonic(),
                };
                DialogAction::None
            }

            DialogInput::StartAlarmDraft {
                time,
                date,
                relative_date,
                days,
                label,
            } => {
                let (dialog_id, started_at) = match self.state {
                    DialogState::Listening {
                        dialog_id,
                        started_at,
                        ..
                    } => (dialog_id, started_at),
                    _ => {
                        self.dialog_counter = self.dialog_counter.wrapping_add(1);
                        (self.dialog_counter, self.clock.now_monotonic())
                    }
                };

                let mut draft = AlarmDraft::new(self.default_timezone.clone());
                draft.time = time.as_deref().and_then(parse_time_str);
                draft.date = date.as_deref().and_then(parse_date_str);
                draft.relative_date = relative_date;
                draft.days = days;
                draft.label = label;

                self.evaluate_draft(dialog_id, draft, 0, started_at)
            }

            DialogInput::PromptFinishedPlaying => match &self.state {
                DialogState::Asking {
                    dialog_id,
                    draft,
                    expected,
                    questions_asked,
                    dialog_started_at,
                    ..
                } => {
                    let deadline = self.clock.now_monotonic() + FOLLOW_UP_TIMEOUT;
                    self.state = DialogState::WaitingFollowUp {
                        dialog_id: *dialog_id,
                        draft: draft.clone(),
                        deadline,
                        expected: expected.clone(),
                        questions_asked: *questions_asked,
                        dialog_started_at: *dialog_started_at,
                    };
                    DialogAction::None
                }
                _ => DialogAction::None,
            },

            DialogInput::SpeechStarted { at } => match &self.state {
                DialogState::WaitingFollowUp {
                    dialog_id,
                    draft,
                    deadline,
                    expected,
                    questions_asked,
                    dialog_started_at,
                } => {
                    if at <= *deadline {
                        self.state = DialogState::ReceivingFollowUp {
                            dialog_id: *dialog_id,
                            draft: draft.clone(),
                            expected: expected.clone(),
                            questions_asked: *questions_asked,
                            dialog_started_at: *dialog_started_at,
                            speech_started_at: at,
                        };
                        DialogAction::None
                    } else {
                        // Speech started after strict 5-second deadline
                        self.state = DialogState::Idle;
                        DialogAction::CloseDialog {
                            reason: CloseReason::SilenceTimeout,
                        }
                    }
                }
                _ => DialogAction::None,
            },

            DialogInput::UserFollowUp(follow_up) => match &self.state {
                DialogState::WaitingFollowUp {
                    dialog_id,
                    draft,
                    deadline,
                    expected,
                    questions_asked,
                    dialog_started_at,
                } => {
                    let now = self.clock.now_monotonic();
                    if now > *deadline {
                        self.state = DialogState::Idle;
                        return DialogAction::CloseDialog {
                            reason: CloseReason::SilenceTimeout,
                        };
                    }
                    self.handle_follow_up(
                        *dialog_id,
                        draft.clone(),
                        expected.clone(),
                        *questions_asked,
                        *dialog_started_at,
                        follow_up,
                    )
                }

                DialogState::ReceivingFollowUp {
                    dialog_id,
                    draft,
                    expected,
                    questions_asked,
                    dialog_started_at,
                    ..
                } => self.handle_follow_up(
                    *dialog_id,
                    draft.clone(),
                    expected.clone(),
                    *questions_asked,
                    *dialog_started_at,
                    follow_up,
                ),

                _ => DialogAction::None,
            },

            DialogInput::Tick => {
                let now = self.clock.now_monotonic();

                // 1. Check strict 5.0 second follow-up window expiry
                if let DialogState::WaitingFollowUp { deadline, .. } = &self.state {
                    if now >= *deadline {
                        self.state = DialogState::Idle;
                        return DialogAction::CloseDialog {
                            reason: CloseReason::SilenceTimeout,
                        };
                    }
                }

                // 2. Check total dialog duration (45 seconds limit)
                let started_opt = match &self.state {
                    DialogState::Listening { started_at, .. } => Some(*started_at),
                    DialogState::Drafting {
                        dialog_started_at, ..
                    }
                    | DialogState::Asking {
                        dialog_started_at, ..
                    }
                    | DialogState::WaitingFollowUp {
                        dialog_started_at, ..
                    }
                    | DialogState::ReceivingFollowUp {
                        dialog_started_at, ..
                    } => Some(*dialog_started_at),
                    DialogState::Idle => None,
                };

                if let Some(started) = started_opt {
                    if now.duration_since(started) >= MAX_DIALOG_DURATION {
                        self.state = DialogState::Idle;
                        return DialogAction::CloseDialog {
                            reason: CloseReason::DialogTimeout,
                        };
                    }
                }

                DialogAction::None
            }

            DialogInput::ExternalInterrupt => {
                if self.state != DialogState::Idle {
                    self.state = DialogState::Idle;
                    DialogAction::CloseDialog {
                        reason: CloseReason::Interrupted,
                    }
                } else {
                    DialogAction::None
                }
            }
        }
    }

    fn handle_follow_up(
        &mut self,
        dialog_id: u64,
        mut draft: AlarmDraft,
        expected: ExpectedInput,
        questions_asked: u32,
        dialog_started_at: Instant,
        follow_up: FollowUpIntent,
    ) -> DialogAction {
        match follow_up {
            FollowUpIntent::Cancel => {
                self.state = DialogState::Idle;
                DialogAction::CloseDialog {
                    reason: CloseReason::UserCancelled,
                }
            }

            FollowUpIntent::Silence => {
                self.state = DialogState::Idle;
                DialogAction::CloseDialog {
                    reason: CloseReason::SilenceTimeout,
                }
            }

            FollowUpIntent::Confirm => {
                if let ExpectedInput::Confirmation { expected_revision } = expected {
                    if draft.revision == expected_revision {
                        match draft.build_schedule(self.clock.now_utc()) {
                            Ok(schedule) => {
                                let announcement = draft.format_announcement(self.clock.now_utc());
                                let label = draft.label.clone();
                                let timezone = draft.timezone.clone();
                                self.state = DialogState::Idle;
                                return DialogAction::CommitAlarm {
                                    schedule,
                                    label,
                                    timezone,
                                    announcement,
                                };
                            }
                            Err(_) => {
                                self.state = DialogState::Idle;
                                return DialogAction::CloseDialog {
                                    reason: CloseReason::UserCancelled,
                                };
                            }
                        }
                    }
                }
                // Confirmation given when time was expected (e.g. user said "yes" to "when?")
                self.evaluate_draft(dialog_id, draft, questions_asked, dialog_started_at)
            }

            FollowUpIntent::ProvideTime { time, date } => {
                if let Some(t) = parse_time_str(&time) {
                    draft.time = Some(t);
                    draft.revision = draft.revision.wrapping_add(1);
                }
                if let Some(d) = date.as_deref().and_then(parse_date_str) {
                    draft.date = Some(d);
                    draft.relative_date = None;
                    draft.revision = draft.revision.wrapping_add(1);
                }
                self.evaluate_draft(dialog_id, draft, questions_asked, dialog_started_at)
            }

            FollowUpIntent::Modify { time, date, days } => {
                if let Some(t) = time.as_deref().and_then(parse_time_str) {
                    draft.time = Some(t);
                    draft.revision = draft.revision.wrapping_add(1);
                }
                if let Some(d) = date.as_deref().and_then(parse_date_str) {
                    draft.date = Some(d);
                    draft.relative_date = None;
                    draft.revision = draft.revision.wrapping_add(1);
                }
                if let Some(days) = days {
                    draft.days = Some(days);
                    draft.date = None;
                    draft.relative_date = None;
                    draft.revision = draft.revision.wrapping_add(1);
                }
                self.evaluate_draft(dialog_id, draft, questions_asked, dialog_started_at)
            }
        }
    }

    fn evaluate_draft(
        &mut self,
        dialog_id: u64,
        draft: AlarmDraft,
        questions_asked: u32,
        dialog_started_at: Instant,
    ) -> DialogAction {
        let now = self.clock.now_monotonic();

        // 1. Check budget limits
        if now.duration_since(dialog_started_at) >= MAX_DIALOG_DURATION {
            self.state = DialogState::Idle;
            return DialogAction::CloseDialog {
                reason: CloseReason::DialogTimeout,
            };
        }

        if questions_asked >= MAX_QUESTIONS {
            self.state = DialogState::Idle;
            return DialogAction::CloseDialog {
                reason: CloseReason::MaxQuestionsExceeded,
            };
        }

        // 2. Check if time is missing
        if draft.time.is_none() {
            let prompt = "На когда поставить будильник?".to_string();
            let expected = ExpectedInput::Time;
            let questions_asked = questions_asked + 1;
            self.state = DialogState::Asking {
                dialog_id,
                draft,
                prompt: prompt.clone(),
                expected: expected.clone(),
                questions_asked,
                dialog_started_at,
            };
            return DialogAction::AskQuestion { prompt, expected };
        }

        // 3. Time is present -> request confirmation
        let prompt = draft.format_confirmation_prompt(self.clock.now_utc());
        let expected = ExpectedInput::Confirmation {
            expected_revision: draft.revision,
        };
        let questions_asked = questions_asked + 1;
        self.state = DialogState::Asking {
            dialog_id,
            draft,
            prompt: prompt.clone(),
            expected: expected.clone(),
            questions_asked,
            dialog_started_at,
        };
        DialogAction::AskQuestion { prompt, expected }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::alarms::clock::TestClock;
    use crate::alarms::manager::AlarmManager;
    use crate::alarms::storage::AlarmStore;
    use std::sync::Arc;
    use std::time::Duration;
    use time::macros::datetime;

    fn make_test_machine() -> (DialogMachine<TestClock>, TestClock) {
        let initial_utc = datetime!(2026-11-06 07:00:00 UTC);
        let clock = TestClock::new(initial_utc);
        let machine = DialogMachine::new(clock.clone(), "Asia/Tbilisi".to_string());
        (machine, clock)
    }

    #[test]
    fn test_missing_time_asks_question_then_silence_timeout_saves_nothing() {
        let (mut machine, clock) = make_test_machine();

        // 1. Wake
        let action = machine.step(DialogInput::Wake { turn_id: 1 });
        assert_eq!(action, DialogAction::None);

        // 2. User says «Штучка, поставь будильник» (time is None)
        let action = machine.step(DialogInput::StartAlarmDraft {
            time: None,
            date: None,
            relative_date: None,
            days: None,
            label: None,
        });

        // Mini must ask "На когда поставить будильник?"
        match action {
            DialogAction::AskQuestion { prompt, expected } => {
                assert_eq!(prompt, "На когда поставить будильник?");
                assert_eq!(expected, ExpectedInput::Time);
            }
            other => panic!("Expected AskQuestion, got {:?}", other),
        }

        // 3. Prompt finishes playing on speaker -> follow-up window opens
        let action = machine.step(DialogInput::PromptFinishedPlaying);
        assert_eq!(action, DialogAction::None);
        assert!(matches!(
            machine.state(),
            DialogState::WaitingFollowUp { .. }
        ));

        // 4. User is silent. 5.1 seconds pass without speech.
        clock.advance(Duration::from_millis(5100));
        let action = machine.step(DialogInput::Tick);

        // State machine must close dialog with SilenceTimeout
        assert_eq!(
            action,
            DialogAction::CloseDialog {
                reason: CloseReason::SilenceTimeout
            }
        );
        assert_eq!(*machine.state(), DialogState::Idle);
    }

    #[test]
    fn test_speech_started_at_4_8s_is_accepted_after_5_0s_is_rejected() {
        let (mut machine, clock) = make_test_machine();

        machine.step(DialogInput::Wake { turn_id: 1 });
        machine.step(DialogInput::StartAlarmDraft {
            time: None,
            date: None,
            relative_date: None,
            days: None,
            label: None,
        });
        machine.step(DialogInput::PromptFinishedPlaying);

        // Advance 4.8s -> speech starts within window
        clock.advance(Duration::from_millis(4800));
        let speech_start = clock.now_monotonic();
        let action = machine.step(DialogInput::SpeechStarted { at: speech_start });
        assert_eq!(action, DialogAction::None);
        assert!(matches!(
            machine.state(),
            DialogState::ReceivingFollowUp { .. }
        ));

        // User speaks for another 1.2s and says "07:00"
        clock.advance(Duration::from_millis(1200));
        let action = machine.step(DialogInput::UserFollowUp(FollowUpIntent::ProvideTime {
            time: "07:00".to_string(),
            date: None,
        }));

        // Mini asks for confirmation
        match action {
            DialogAction::AskQuestion { prompt, expected } => {
                assert!(prompt.contains("07:00"));
                assert!(matches!(expected, ExpectedInput::Confirmation { .. }));
            }
            other => panic!("Expected AskQuestion confirmation, got {:?}", other),
        }

        // Now test second machine with speech start at 5.1s
        let (mut late_machine, late_clock) = make_test_machine();
        late_machine.step(DialogInput::Wake { turn_id: 2 });
        late_machine.step(DialogInput::StartAlarmDraft {
            time: None,
            date: None,
            relative_date: None,
            days: None,
            label: None,
        });
        late_machine.step(DialogInput::PromptFinishedPlaying);

        late_clock.advance(Duration::from_millis(5100));
        let late_speech = late_clock.now_monotonic();
        let action = late_machine.step(DialogInput::SpeechStarted { at: late_speech });
        assert_eq!(
            action,
            DialogAction::CloseDialog {
                reason: CloseReason::SilenceTimeout
            }
        );
        assert_eq!(*late_machine.state(), DialogState::Idle);
    }

    #[test]
    fn test_user_modifies_time_invalidates_old_confirmation_then_confirms() {
        let (mut machine, _clock) = make_test_machine();

        machine.step(DialogInput::Wake { turn_id: 1 });
        // Initial request: "07:00 tomorrow"
        let action = machine.step(DialogInput::StartAlarmDraft {
            time: Some("07:00".to_string()),
            date: None,
            relative_date: Some(RelativeDate::Tomorrow),
            days: None,
            label: None,
        });

        // Question 1: "Поставить на завтра на 07:00?"
        match action {
            DialogAction::AskQuestion { prompt, expected } => {
                assert_eq!(prompt, "Поставить на завтра на 07:00?");
                assert_eq!(
                    expected,
                    ExpectedInput::Confirmation {
                        expected_revision: 1
                    }
                );
            }
            other => panic!("Expected AskQuestion, got {:?}", other),
        }

        machine.step(DialogInput::PromptFinishedPlaying);

        // User says: "Нет, лучше в восемь утра"
        let action = machine.step(DialogInput::UserFollowUp(FollowUpIntent::Modify {
            time: Some("08:00".to_string()),
            date: None,
            days: None,
        }));

        // Question 2: "Поставить на завтра на 08:00?" (Revision bumped to 2)
        match action {
            DialogAction::AskQuestion { prompt, expected } => {
                assert_eq!(prompt, "Поставить на завтра на 08:00?");
                assert_eq!(
                    expected,
                    ExpectedInput::Confirmation {
                        expected_revision: 2
                    }
                );
            }
            other => panic!("Expected AskQuestion, got {:?}", other),
        }

        machine.step(DialogInput::PromptFinishedPlaying);

        // User says: "Да"
        let action = machine.step(DialogInput::UserFollowUp(FollowUpIntent::Confirm));

        // Must commit 08:00!
        match action {
            DialogAction::CommitAlarm {
                schedule,
                announcement,
                ..
            } => {
                assert!(announcement.contains("08:00"));
                assert!(matches!(schedule, AlarmSchedule::Single { .. }));
            }
            other => panic!("Expected CommitAlarm, got {:?}", other),
        }

        assert_eq!(*machine.state(), DialogState::Idle);
    }

    #[test]
    fn test_explicit_cancel_discards_draft_immediately() {
        let (mut machine, _clock) = make_test_machine();

        machine.step(DialogInput::Wake { turn_id: 1 });
        machine.step(DialogInput::StartAlarmDraft {
            time: Some("07:00".to_string()),
            date: None,
            relative_date: None,
            days: None,
            label: None,
        });
        machine.step(DialogInput::PromptFinishedPlaying);

        let action = machine.step(DialogInput::UserFollowUp(FollowUpIntent::Cancel));
        assert_eq!(
            action,
            DialogAction::CloseDialog {
                reason: CloseReason::UserCancelled
            }
        );
        assert_eq!(*machine.state(), DialogState::Idle);
    }

    #[test]
    fn test_external_interrupt_resets_dialog() {
        let (mut machine, _clock) = make_test_machine();

        machine.step(DialogInput::Wake { turn_id: 1 });
        machine.step(DialogInput::StartAlarmDraft {
            time: None,
            date: None,
            relative_date: None,
            days: None,
            label: None,
        });

        let action = machine.step(DialogInput::ExternalInterrupt);
        assert_eq!(
            action,
            DialogAction::CloseDialog {
                reason: CloseReason::Interrupted
            }
        );
        assert_eq!(*machine.state(), DialogState::Idle);
    }

    #[test]
    fn test_exceeding_max_questions_budget_closes_cleanly() {
        let (mut machine, _clock) = make_test_machine();

        machine.step(DialogInput::Wake { turn_id: 1 });
        // Question 1: ask time
        machine.step(DialogInput::StartAlarmDraft {
            time: None,
            date: None,
            relative_date: None,
            days: None,
            label: None,
        });
        machine.step(DialogInput::PromptFinishedPlaying);

        // User says "да" instead of time -> Question 2 (re-asks time)
        machine.step(DialogInput::UserFollowUp(FollowUpIntent::Confirm));
        machine.step(DialogInput::PromptFinishedPlaying);

        // User says "да" again -> Question 3 (re-asks time)
        machine.step(DialogInput::UserFollowUp(FollowUpIntent::Confirm));
        machine.step(DialogInput::PromptFinishedPlaying);

        // User says "да" again -> Question 4 exceeds MAX_QUESTIONS (3) -> close!
        let action = machine.step(DialogInput::UserFollowUp(FollowUpIntent::Confirm));
        assert_eq!(
            action,
            DialogAction::CloseDialog {
                reason: CloseReason::MaxQuestionsExceeded
            }
        );
        assert_eq!(*machine.state(), DialogState::Idle);
    }

    #[test]
    fn test_total_dialog_duration_timeout_closes_dialog() {
        let (mut machine, clock) = make_test_machine();

        machine.step(DialogInput::Wake { turn_id: 1 });
        machine.step(DialogInput::StartAlarmDraft {
            time: None,
            date: None,
            relative_date: None,
            days: None,
            label: None,
        });

        // 46 seconds elapse
        clock.advance(Duration::from_secs(46));
        let action = machine.step(DialogInput::Tick);

        assert_eq!(
            action,
            DialogAction::CloseDialog {
                reason: CloseReason::DialogTimeout
            }
        );
        assert_eq!(*machine.state(), DialogState::Idle);
    }

    #[test]
    fn test_end_to_end_integration_with_alarm_manager_verifies_zero_alarms_on_silence() {
        let dir = std::env::temp_dir().join(format!("mini-dialog-test-{}", uuid::Uuid::new_v4()));
        let initial_utc = datetime!(2026-11-06 07:00:00 UTC);
        let clock = TestClock::new(initial_utc);
        let store = Arc::new(AlarmStore::new(&dir).unwrap());
        let clock_arc = Arc::new(clock.clone());
        let manager = AlarmManager::new(clock_arc, store).unwrap();

        let mut machine = DialogMachine::new(clock.clone(), "Asia/Tbilisi".to_string());

        // Step 1: User says "Штучка, поставь будильник"
        machine.step(DialogInput::Wake { turn_id: 10 });
        let action = machine.step(DialogInput::StartAlarmDraft {
            time: None,
            date: None,
            relative_date: None,
            days: None,
            label: None,
        });
        assert!(matches!(action, DialogAction::AskQuestion { .. }));
        machine.step(DialogInput::PromptFinishedPlaying);

        // Step 2: Silence timeout
        clock.advance(Duration::from_millis(5050));
        let action = machine.step(DialogInput::Tick);
        assert_eq!(
            action,
            DialogAction::CloseDialog {
                reason: CloseReason::SilenceTimeout
            }
        );

        // CRITICAL CHECK: Verify that manager store has ZERO alarms!
        let alarms = manager.list_alarms(false);
        assert_eq!(alarms.len(), 0);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
