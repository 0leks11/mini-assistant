//! Local authority for a bounded, explicitly activated user turn.
//! Energy, model output and network reconnects cannot grant this authority.
use std::sync::Mutex;
use std::time::{Duration, Instant};

pub type TurnId = u64;

#[derive(Debug)]
struct Permit {
    id: TurnId,
    deadline: Instant,
    submitted: bool,
    input_ended: bool,
    response_complete: bool,
}

#[derive(Debug, Default)]
struct State {
    sequence: TurnId,
    permit: Option<Permit>,
}

#[derive(Debug)]
pub struct TurnAuthorization {
    lifetime: Duration,
    state: Mutex<State>,
}

impl TurnAuthorization {
    pub fn new(lifetime: Duration) -> Self {
        Self {
            lifetime,
            state: Mutex::new(State::default()),
        }
    }

    /// Called only by a local keyword detector or an explicit local control.
    pub fn begin_explicit_turn(&self) -> TurnId {
        let Ok(mut state) = self.state.lock() else {
            return 0;
        };
        let Some(id) = state.sequence.checked_add(1) else {
            state.permit = None;
            return 0;
        };
        state.sequence = id;
        let Some(deadline) = Instant::now().checked_add(self.lifetime) else {
            state.permit = None;
            return 0;
        };
        state.permit = Some(Permit {
            id,
            deadline,
            submitted: false,
            input_ended: false,
            response_complete: false,
        });
        id
    }

    pub fn current_turn(&self) -> Option<TurnId> {
        let state = self.state.lock().ok()?;
        state
            .permit
            .as_ref()
            .filter(|p| Instant::now() < p.deadline)
            .map(|p| p.id)
    }

    /// Remains true during drain so already admitted audio can finish.
    pub fn is_current(&self, id: TurnId) -> bool {
        id != 0 && self.current_turn() == Some(id)
    }

    pub fn mark_submitted(&self, id: TurnId) -> bool {
        let Ok(mut state) = self.state.lock() else {
            return false;
        };
        if let Some(p) = state.permit.as_mut().filter(|p| {
            p.id == id && Instant::now() < p.deadline && !p.input_ended && !p.response_complete
        }) {
            p.submitted = true;
            return true;
        }
        false
    }

    /// Seal a submitted input exactly once, before its wire activityEnd.
    /// Empty/duplicate END or stale IDs cannot grant response authority.
    pub fn complete_input(&self, id: TurnId) -> bool {
        let Ok(mut state) = self.state.lock() else {
            return false;
        };
        if let Some(p) = state.permit.as_mut().filter(|p| {
            p.id == id
                && p.submitted
                && !p.input_ended
                && !p.response_complete
                && Instant::now() < p.deadline
        }) {
            p.input_ended = true;
            return true;
        }
        false
    }

    pub fn accepting_response(&self) -> Option<TurnId> {
        let state = self.state.lock().ok()?;
        state
            .permit
            .as_ref()
            .filter(|p| {
                p.submitted && p.input_ended && !p.response_complete && Instant::now() < p.deadline
            })
            .map(|p| p.id)
    }

    pub fn complete_response(&self, id: TurnId) {
        if let Ok(mut state) = self.state.lock() {
            if let Some(p) = state.permit.as_mut().filter(|p| p.id == id) {
                p.response_complete = true;
            }
        }
    }

    pub fn finish(&self, id: TurnId) {
        if let Ok(mut state) = self.state.lock() {
            if state.permit.as_ref().is_some_and(|p| p.id == id) {
                state.permit = None;
            }
        }
    }

    pub fn cancel(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.permit = None;
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub enum FastMediaCommand {
    Pause,
    Resume,
    Next,
    Previous,
    VolumeUp,
    VolumeDown,
    SetVolume(u8),
}

/// Deterministically parses user text to recognize rapid media playback commands
/// without incurring round-trip LLM latency.
#[allow(dead_code)]
pub fn parse_fast_media_command(text: &str) -> Option<FastMediaCommand> {
    let clean = text.trim().to_lowercase();
    let clean = clean.trim_matches(|c: char| {
        c.is_ascii_punctuation() || c == '!' || c == '?' || c == '.' || c == '—'
    });

    match clean {
        "пауза"
        | "стоп"
        | "останови"
        | "останови музыку"
        | "хватит"
        | "хватит играть"
        | "замолчи" => {
            return Some(FastMediaCommand::Pause);
        }
        "продолжи" | "продолжить" | "играй" | "возобнови" => {
            return Some(FastMediaCommand::Resume);
        }
        "дальше" | "следующий" | "следующий трек" | "следующая" | "пропусти" | "переключи" =>
        {
            return Some(FastMediaCommand::Next);
        }
        "назад" | "предыдущий" | "предыдущий трек" | "предыдущая" =>
        {
            return Some(FastMediaCommand::Previous);
        }
        "громче" | "сделай громче" | "прибавь" | "прибавь громкость" | "погромче" =>
        {
            return Some(FastMediaCommand::VolumeUp);
        }
        "тише" | "сделай тише" | "убавь" | "убавь громкость" | "потише" =>
        {
            return Some(FastMediaCommand::VolumeDown);
        }
        _ => {}
    }

    if let Some(rest) = clean
        .strip_prefix("громкость на ")
        .or_else(|| clean.strip_prefix("громкость "))
    {
        if let Ok(val) = rest.trim().parse::<u8>() {
            return Some(FastMediaCommand::SetVolume(val.min(100)));
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn startup_and_unsubmitted_turn_cannot_authorize_model() {
        let a = TurnAuthorization::new(Duration::from_secs(5));
        assert_eq!(a.accepting_response(), None);
        let id = a.begin_explicit_turn();
        assert_eq!(a.accepting_response(), None);
        assert!(a.mark_submitted(id));
        assert_eq!(a.accepting_response(), None);
        assert!(a.complete_input(id));
        assert!(!a.mark_submitted(id));
        assert!(!a.complete_input(id));
        assert_eq!(a.accepting_response(), Some(id));
        a.complete_response(id);
        assert_eq!(a.accepting_response(), None);
        assert!(a.is_current(id));
        a.finish(id);
        assert!(!a.is_current(id));
    }
    #[test]
    fn old_completion_and_cancel_never_grant_a_new_turn() {
        let a = TurnAuthorization::new(Duration::from_secs(5));
        let old = a.begin_explicit_turn();
        let new = a.begin_explicit_turn();
        a.finish(old);
        assert!(a.is_current(new));
        assert!(!a.mark_submitted(old));
        a.cancel();
        assert!(!a.is_current(new));
    }

    #[test]
    fn empty_end_and_cancelled_input_cannot_authorize_a_response() {
        let a = TurnAuthorization::new(Duration::from_secs(5));
        let id = a.begin_explicit_turn();
        assert!(!a.complete_input(id));
        assert_eq!(a.accepting_response(), None);
        assert!(a.mark_submitted(id));
        a.cancel();
        assert!(!a.complete_input(id));
        let next = a.begin_explicit_turn();
        assert!(!a.complete_input(id));
        assert_eq!(a.accepting_response(), None);
        assert!(a.mark_submitted(next));
        assert!(a.complete_input(next));
        assert_eq!(a.accepting_response(), Some(next));
    }
    #[test]
    fn expired_permit_fails_closed() {
        let a = TurnAuthorization::new(Duration::ZERO);
        let id = a.begin_explicit_turn();
        assert!(!a.mark_submitted(id));
        assert!(!a.is_current(id));
    }

    #[test]
    fn test_fast_media_command_parsing() {
        assert_eq!(
            parse_fast_media_command("Пауза!"),
            Some(FastMediaCommand::Pause)
        );
        assert_eq!(
            parse_fast_media_command("стоп"),
            Some(FastMediaCommand::Pause)
        );
        assert_eq!(
            parse_fast_media_command("останови музыку"),
            Some(FastMediaCommand::Pause)
        );
        assert_eq!(
            parse_fast_media_command("следующий трек"),
            Some(FastMediaCommand::Next)
        );
        assert_eq!(
            parse_fast_media_command("дальше"),
            Some(FastMediaCommand::Next)
        );
        assert_eq!(
            parse_fast_media_command("назад"),
            Some(FastMediaCommand::Previous)
        );
        assert_eq!(
            parse_fast_media_command("погромче"),
            Some(FastMediaCommand::VolumeUp)
        );
        assert_eq!(
            parse_fast_media_command("тише..."),
            Some(FastMediaCommand::VolumeDown)
        );
        assert_eq!(
            parse_fast_media_command("громкость на 45"),
            Some(FastMediaCommand::SetVolume(45))
        );
        assert_eq!(
            parse_fast_media_command("громкость 80"),
            Some(FastMediaCommand::SetVolume(80))
        );
        assert_eq!(
            parse_fast_media_command("громкость на 150"),
            Some(FastMediaCommand::SetVolume(100))
        );
        assert_eq!(parse_fast_media_command("какая сегодня погода?"), None);
        assert_eq!(parse_fast_media_command("поставь будильник на семь"), None);
    }
}
