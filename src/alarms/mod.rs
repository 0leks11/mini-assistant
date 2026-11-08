//! Local alarm service and scheduler for Station Mini.
//! Fully independent of external networks, Home Assistant, and cloud connectivity.

pub mod clock;
pub mod dialog;
pub mod manager;
pub mod player;
pub mod sound;
pub mod storage;
pub mod tools;
pub mod types;

pub use clock::{Clock, SystemClock, TestClock};
pub use dialog::{
    AlarmDraft, CloseReason, DialogAction, DialogInput, DialogMachine, DialogState, ExpectedInput,
    FollowUpIntent, RelativeDate, FOLLOW_UP_TIMEOUT, MAX_DIALOG_DURATION, MAX_QUESTIONS,
};
pub use manager::AlarmManager;
pub use player::{AlarmPlayer, ALARM_RESPONSE_ID};
pub use sound::{apply_volume_ramp, generate_synthetic_chime, load_wav_pcm_16};
pub use storage::{AlarmStore, AlarmsSnapshot};
pub use tools::{alarm_tool_declarations, handle_alarm_tool_call};
pub use types::{
    parse_timezone_offset, Alarm, AlarmEvent, AlarmId, AlarmSchedule, AlarmState, AlarmSummary,
    DayOfWeek, TimeOfDay,
};
