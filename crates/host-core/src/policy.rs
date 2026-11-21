//! Host admin policies: who may connect and when, and how long a session may
//! sit unused.
//!
//! Everything here is pure (the caller passes the local time and the clock),
//! so the rules are unit-testable. The viewer's ID comes from the relay's
//! pairing message and is not authenticated, so an ID list is a filter, not a
//! credential: every viewer still needs the password.
use crate::reconnect::reject_code;
use serde::{Deserialize, Serialize};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::time::Instant;

/// How the host treats the IDs on its allowed list.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AllowMode {
    /// Everyone gets the approval prompt; listed IDs are marked as known.
    #[default]
    MarkKnown,
    /// Listed IDs connect without the prompt (they still need the password);
    /// everyone else gets the prompt.
    AutoAccept,
    /// Only listed IDs may connect at all; others are refused without a
    /// prompt. Listed IDs still get the prompt unless unattended access is on.
    OnlyListed,
}

/// One weekly time window, in the host's local time. `days` are 0 (Monday)
/// to 6 (Sunday). `start` and `end` are minutes after midnight. A window
/// whose `end` is before its `start` runs past midnight into the next day
/// (22:00 to 06:00 on Friday covers Friday night and early Saturday). A
/// window whose `start` equals its `end` covers the whole day.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HoursRule {
    pub days: Vec<u8>,
    pub start: u16,
    pub end: u16,
}

/// When connections are allowed. Off by default, which means any time.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HoursSchedule {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub rules: Vec<HoursRule>,
}

/// A moment in the host's local time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LocalTime {
    /// 0 (Monday) to 6 (Sunday).
    pub weekday: u8,
    /// Minutes after midnight, 0 to 1439.
    pub minute: u16,
}

pub const MINUTES_PER_DAY: u16 = 24 * 60;
/// Most windows a schedule may have.
pub const MAX_HOURS_RULES: usize = 32;
/// Most IDs on the allowed list.
pub const MAX_ALLOWED_IDS: usize = 500;
/// Longest label for an allowed ID.
pub const MAX_LABEL_CHARS: usize = 64;
/// Longest idle timeout, in minutes (a day).
pub const MAX_IDLE_MINUTES: u32 = 24 * 60;

impl HoursRule {
    fn has_day(&self, day: u8) -> bool {
        self.days.contains(&day)
    }

    /// Whether `now` falls inside this window.
    pub fn matches(&self, now: LocalTime) -> bool {
        let yesterday = (now.weekday + 6) % 7;
        match self.start.cmp(&self.end) {
            std::cmp::Ordering::Equal => self.has_day(now.weekday),
            std::cmp::Ordering::Less => self.has_day(now.weekday) && (self.start..self.end).contains(&now.minute),
            // Past midnight: the evening part on a listed day, or the early
            // morning part on the day after one.
            std::cmp::Ordering::Greater => {
                (self.has_day(now.weekday) && now.minute >= self.start) || (self.has_day(yesterday) && now.minute < self.end)
            }
        }
    }

    fn validate(&self) -> Result<(), String> {
        if self.days.is_empty() {
            return Err("Pick at least one day for each time range.".into());
        }
        if self.days.iter().any(|day| *day > 6) {
            return Err("A time range has a day that doesn't exist.".into());
        }
        if self.start >= MINUTES_PER_DAY || self.end >= MINUTES_PER_DAY {
            return Err("Times must be between 00:00 and 23:59.".into());
        }
        Ok(())
    }
}

impl HoursSchedule {
    /// Whether a connection at `now` is inside the allowed hours. Always true
    /// when the schedule is off.
    pub fn allows(&self, now: LocalTime) -> bool {
        !self.enabled || self.rules.iter().any(|rule| rule.matches(now))
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.rules.len() > MAX_HOURS_RULES {
            return Err(format!("Use at most {MAX_HOURS_RULES} time ranges."));
        }
        if self.enabled && self.rules.is_empty() {
            return Err("Add at least one time range, or turn allowed hours off.".into());
        }
        self.rules.iter().try_for_each(HoursRule::validate)
    }
}

/// An allowed viewer ID and an optional label for it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AllowedId {
    pub id: String,
    #[serde(default)]
    pub label: String,
}

/// The access rules as the settings screen edits them.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccessRules {
    #[serde(default)]
    pub mode: AllowMode,
    #[serde(default)]
    pub allowed: Vec<AllowedId>,
    #[serde(default)]
    pub hours: HoursSchedule,
}

/// A 9-digit OpenDesk ID.
pub fn valid_id(id: &str) -> bool {
    id.len() == 9 && id.bytes().all(|byte| byte.is_ascii_digit())
}

impl AccessRules {
    /// Checks the rules and tidies them: labels are trimmed, and an ID listed
    /// twice keeps its first entry.
    pub fn validated(mut self) -> Result<Self, String> {
        if self.allowed.len() > MAX_ALLOWED_IDS {
            return Err(format!("Keep the list to {MAX_ALLOWED_IDS} IDs or fewer."));
        }
        let mut seen = std::collections::HashSet::new();
        let mut allowed = Vec::with_capacity(self.allowed.len());
        for entry in self.allowed {
            let id: String = entry.id.chars().filter(|c| !c.is_whitespace() && *c != '-').collect();
            if !valid_id(&id) {
                return Err(format!("\"{}\" is not a 9-digit ID.", entry.id));
            }
            let label = entry.label.trim().to_owned();
            if label.chars().count() > MAX_LABEL_CHARS {
                return Err(format!("Labels can be at most {MAX_LABEL_CHARS} characters."));
            }
            if seen.insert(id.clone()) {
                allowed.push(AllowedId { id, label });
            }
        }
        if self.mode == AllowMode::OnlyListed && allowed.is_empty() {
            return Err("Add at least one ID before allowing only listed devices.".into());
        }
        self.hours.validate()?;
        self.allowed = allowed;
        Ok(self)
    }
}

/// Why the host refuses a viewer before asking anyone.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// The ID is not on the list and only listed IDs may connect.
    NotAllowed,
    /// The connection came outside the allowed hours.
    OutsideHours,
}

impl Refusal {
    /// The `Control::Error` code the viewer gets.
    pub fn code(self) -> &'static str {
        match self {
            Refusal::NotAllowed => reject_code::NOT_ALLOWED,
            Refusal::OutsideHours => reject_code::OUTSIDE_HOURS,
        }
    }

    /// The audit log's detail, and the host's own error text.
    pub fn detail(self) -> &'static str {
        match self {
            Refusal::NotAllowed => "refused: not on the allowed list",
            Refusal::OutsideHours => "refused: outside allowed hours",
        }
    }
}

/// What the host does with a viewer that knows the password.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Admission {
    /// Ask the host user (or let unattended access accept it).
    Ask,
    /// Accept without the prompt.
    AutoAccept,
    /// Refuse it with a reason, without a prompt.
    Refuse(Refusal),
}

impl Admission {
    pub fn refusal(self) -> Option<Refusal> {
        match self {
            Admission::Refuse(refusal) => Some(refusal),
            _ => None,
        }
    }
}

/// Decide what to do with a viewer whose ID is (or isn't) `listed`, connecting
/// at `now`. The ID rule comes first, so a stranger outside hours hears that
/// it isn't allowed at all.
pub fn admit(mode: AllowMode, listed: bool, hours: &HoursSchedule, now: LocalTime) -> Admission {
    if mode == AllowMode::OnlyListed && !listed {
        return Admission::Refuse(Refusal::NotAllowed);
    }
    if !hours.allows(now) {
        return Admission::Refuse(Refusal::OutsideHours);
    }
    if mode == AllowMode::AutoAccept && listed {
        Admission::AutoAccept
    } else {
        Admission::Ask
    }
}

/// How long before an idle end the viewer is warned.
pub const IDLE_WARNING: Duration = Duration::from_secs(60);

/// When the viewer last did something: input, a file, clipboard or chat
/// message, or "stay connected". Shared between the session's tasks.
#[derive(Clone, Debug)]
pub struct Activity(Arc<Mutex<Instant>>);

impl Activity {
    pub fn new() -> Self {
        Self(Arc::new(Mutex::new(Instant::now())))
    }

    pub fn touch(&self) {
        *self.0.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = Instant::now();
    }

    pub fn last(&self) -> Instant {
        *self.0.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl Default for Activity {
    fn default() -> Self {
        Self::new()
    }
}

/// What the idle timer wants done now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IdleAction {
    /// Nothing yet; check again at `next`.
    Wait { next: Instant },
    /// Warn the viewer the session ends in `seconds`; check again at `next`.
    Warn { seconds: u32, next: Instant },
    /// Time is up: end the session.
    End,
}

/// Ends a session nobody uses. Warns once, [`IDLE_WARNING`] before the end
/// (or halfway, for a timeout shorter than twice that), and warns again if
/// activity resumed and then stopped once more.
#[derive(Clone, Debug)]
pub struct IdleTimer {
    timeout: Duration,
    warn_before: Duration,
    warned: bool,
}

impl IdleTimer {
    pub fn new(timeout: Duration) -> Self {
        Self { timeout, warn_before: IDLE_WARNING.min(timeout / 2), warned: false }
    }

    pub fn poll(&mut self, last_activity: Instant, now: Instant) -> IdleAction {
        let idle = now.saturating_duration_since(last_activity);
        let ends = last_activity + self.timeout;
        let warn_at = ends - self.warn_before;
        if idle >= self.timeout {
            return IdleAction::End;
        }
        if now < warn_at {
            self.warned = false;
            return IdleAction::Wait { next: warn_at };
        }
        if self.warned {
            return IdleAction::Wait { next: ends };
        }
        self.warned = true;
        let left = ends.saturating_duration_since(now);
        IdleAction::Warn { seconds: left.as_secs_f64().ceil() as u32, next: ends }
    }
}

/// The idle timeout for `minutes` from the settings, or `None` when it is off.
pub fn idle_timeout(minutes: u32) -> Option<Duration> {
    (minutes > 0).then(|| Duration::from_secs(u64::from(minutes.min(MAX_IDLE_MINUTES)) * 60))
}

#[cfg(test)]
mod tests {
    use super::*;

    const MON: u8 = 0;
    const FRI: u8 = 4;
    const SAT: u8 = 5;
    const SUN: u8 = 6;

    fn at(weekday: u8, hour: u16, minute: u16) -> LocalTime {
        LocalTime { weekday, minute: hour * 60 + minute }
    }

    fn rule(days: &[u8], start: (u16, u16), end: (u16, u16)) -> HoursRule {
        HoursRule { days: days.to_vec(), start: start.0 * 60 + start.1, end: end.0 * 60 + end.1 }
    }

    fn schedule(rules: Vec<HoursRule>) -> HoursSchedule {
        HoursSchedule { enabled: true, rules }
    }

    #[test]
    fn a_daytime_range_covers_its_days_from_start_up_to_end() {
        let weekdays = schedule(vec![rule(&[0, 1, 2, 3, 4], (9, 0), (17, 30))]);
        assert!(weekdays.allows(at(MON, 9, 0)));
        assert!(weekdays.allows(at(FRI, 17, 29)));
        assert!(!weekdays.allows(at(FRI, 17, 30)));
        assert!(!weekdays.allows(at(MON, 8, 59)));
        assert!(!weekdays.allows(at(SAT, 12, 0)));
    }

    #[test]
    fn a_range_crossing_midnight_runs_into_the_next_day() {
        let nights = schedule(vec![rule(&[FRI], (22, 0), (6, 0))]);
        assert!(nights.allows(at(FRI, 22, 0)));
        assert!(nights.allows(at(FRI, 23, 59)));
        assert!(nights.allows(at(SAT, 0, 0)));
        assert!(nights.allows(at(SAT, 5, 59)));
        assert!(!nights.allows(at(SAT, 6, 0)));
        assert!(!nights.allows(at(SAT, 22, 30)), "Saturday is not listed");
        assert!(!nights.allows(at(FRI, 3, 0)), "the early part belongs to Thursday night");
        // Sunday night wraps into Monday morning.
        let sunday = schedule(vec![rule(&[SUN], (23, 0), (1, 0))]);
        assert!(sunday.allows(at(MON, 0, 30)));
        assert!(!sunday.allows(at(SUN, 0, 30)));
    }

    #[test]
    fn equal_start_and_end_means_all_day() {
        let weekend = schedule(vec![rule(&[SAT, SUN], (0, 0), (0, 0))]);
        assert!(weekend.allows(at(SAT, 0, 0)));
        assert!(weekend.allows(at(SUN, 23, 59)));
        assert!(!weekend.allows(at(MON, 12, 0)));
    }

    #[test]
    fn any_matching_range_allows_and_an_off_schedule_allows_everything() {
        let split = schedule(vec![rule(&[MON], (8, 0), (12, 0)), rule(&[MON], (13, 0), (17, 0))]);
        assert!(split.allows(at(MON, 9, 0)) && split.allows(at(MON, 14, 0)));
        assert!(!split.allows(at(MON, 12, 30)));
        let off = HoursSchedule { enabled: false, rules: Vec::new() };
        assert!(off.allows(at(SUN, 3, 0)));
        assert!(HoursSchedule::default().allows(at(SUN, 3, 0)));
    }

    #[test]
    fn schedules_are_validated() {
        assert!(schedule(vec![rule(&[MON], (9, 0), (17, 0))]).validate().is_ok());
        assert!(schedule(Vec::new()).validate().is_err(), "on with no ranges refuses everyone");
        assert!(HoursSchedule { enabled: false, rules: Vec::new() }.validate().is_ok());
        assert!(schedule(vec![rule(&[], (9, 0), (17, 0))]).validate().is_err());
        assert!(schedule(vec![rule(&[7], (9, 0), (17, 0))]).validate().is_err());
        assert!(schedule(vec![HoursRule { days: vec![MON], start: 0, end: MINUTES_PER_DAY }]).validate().is_err());
        assert!(schedule(vec![rule(&[MON], (9, 0), (17, 0)); MAX_HOURS_RULES + 1]).validate().is_err());
    }

    #[test]
    fn allow_modes_decide_who_is_asked_accepted_or_refused() {
        let any_time = HoursSchedule::default();
        let now = at(MON, 12, 0);
        assert_eq!(admit(AllowMode::MarkKnown, true, &any_time, now), Admission::Ask);
        assert_eq!(admit(AllowMode::MarkKnown, false, &any_time, now), Admission::Ask);
        assert_eq!(admit(AllowMode::AutoAccept, true, &any_time, now), Admission::AutoAccept);
        assert_eq!(admit(AllowMode::AutoAccept, false, &any_time, now), Admission::Ask);
        assert_eq!(admit(AllowMode::OnlyListed, true, &any_time, now), Admission::Ask);
        assert_eq!(admit(AllowMode::OnlyListed, false, &any_time, now), Admission::Refuse(Refusal::NotAllowed));
    }

    #[test]
    fn hours_refuse_even_listed_ids_and_the_id_rule_wins() {
        let office = schedule(vec![rule(&[MON], (9, 0), (17, 0))]);
        let night = at(MON, 22, 0);
        assert_eq!(admit(AllowMode::AutoAccept, true, &office, night), Admission::Refuse(Refusal::OutsideHours));
        assert_eq!(admit(AllowMode::MarkKnown, false, &office, night), Admission::Refuse(Refusal::OutsideHours));
        assert_eq!(admit(AllowMode::OnlyListed, false, &office, night), Admission::Refuse(Refusal::NotAllowed));
        assert_eq!(admit(AllowMode::AutoAccept, true, &office, at(MON, 10, 0)), Admission::AutoAccept);
        assert_eq!(Refusal::NotAllowed.code(), reject_code::NOT_ALLOWED);
        assert_eq!(Refusal::OutsideHours.code(), reject_code::OUTSIDE_HOURS);
    }

    #[test]
    fn rules_are_validated_and_tidied() {
        let rules = AccessRules {
            mode: AllowMode::OnlyListed,
            allowed: vec![
                AllowedId { id: "123 456 789".into(), label: "  Office PC ".into() },
                AllowedId { id: "123456789".into(), label: "dupe".into() },
            ],
            hours: HoursSchedule::default(),
        }
        .validated()
        .unwrap();
        assert_eq!(rules.allowed, vec![AllowedId { id: "123456789".into(), label: "Office PC".into() }]);
        let bad_id = AccessRules { allowed: vec![AllowedId { id: "12345".into(), label: String::new() }], ..AccessRules::default() };
        assert!(bad_id.validated().is_err());
        let long_label = AccessRules { allowed: vec![AllowedId { id: "123456789".into(), label: "x".repeat(MAX_LABEL_CHARS + 1) }], ..AccessRules::default() };
        assert!(long_label.validated().is_err());
        let nobody = AccessRules { mode: AllowMode::OnlyListed, ..AccessRules::default() };
        assert!(nobody.validated().is_err(), "only-listed with an empty list would lock everyone out");
        assert!(AccessRules::default().validated().is_ok());
    }

    #[test]
    fn rules_read_from_older_and_partial_settings() {
        let rules: AccessRules = serde_json::from_str("{}").unwrap();
        assert_eq!(rules, AccessRules::default());
        let rules: AccessRules = serde_json::from_str(r#"{"mode":"auto_accept","allowed":[{"id":"123456789"}]}"#).unwrap();
        assert_eq!(rules.mode, AllowMode::AutoAccept);
        assert_eq!(rules.allowed[0].label, "");
        assert!(!rules.hours.enabled);
    }

    #[test]
    fn idle_timeout_is_off_at_zero_and_capped() {
        assert_eq!(idle_timeout(0), None);
        assert_eq!(idle_timeout(15), Some(Duration::from_secs(900)));
        assert_eq!(idle_timeout(u32::MAX), Some(Duration::from_secs(u64::from(MAX_IDLE_MINUTES) * 60)));
    }

    #[test]
    fn idle_timer_warns_a_minute_before_and_then_ends() {
        let start = Instant::now();
        let mut timer = IdleTimer::new(Duration::from_secs(300));
        let warn_at = start + Duration::from_secs(240);
        assert_eq!(timer.poll(start, start), IdleAction::Wait { next: warn_at });
        assert_eq!(timer.poll(start, warn_at), IdleAction::Warn { seconds: 60, next: start + Duration::from_secs(300) });
        // Only once.
        assert_eq!(timer.poll(start, warn_at + Duration::from_secs(1)), IdleAction::Wait { next: start + Duration::from_secs(300) });
        assert_eq!(timer.poll(start, start + Duration::from_secs(300)), IdleAction::End);
    }

    #[test]
    fn activity_after_a_warning_resets_the_timer_and_rearms_the_warning() {
        let start = Instant::now();
        let mut timer = IdleTimer::new(Duration::from_secs(300));
        assert!(matches!(timer.poll(start, start + Duration::from_secs(250)), IdleAction::Warn { .. }));
        let active = start + Duration::from_secs(270);
        assert_eq!(timer.poll(active, active), IdleAction::Wait { next: active + Duration::from_secs(240) });
        assert!(matches!(timer.poll(active, active + Duration::from_secs(240)), IdleAction::Warn { seconds: 60, .. }));
    }

    #[test]
    fn a_short_timeout_warns_halfway() {
        let start = Instant::now();
        let mut timer = IdleTimer::new(Duration::from_secs(60));
        assert_eq!(timer.poll(start, start), IdleAction::Wait { next: start + Duration::from_secs(30) });
        assert!(matches!(timer.poll(start, start + Duration::from_secs(30)), IdleAction::Warn { seconds: 30, .. }));
    }

    #[tokio::test(start_paused = true)]
    async fn shared_activity_follows_the_paused_clock() {
        let activity = Activity::new();
        let mut timer = IdleTimer::new(Duration::from_secs(5 * 60));
        tokio::time::advance(Duration::from_secs(4 * 60)).await;
        assert!(matches!(timer.poll(activity.last(), Instant::now()), IdleAction::Warn { seconds: 60, .. }));
        // The viewer clicks "Stay connected".
        activity.touch();
        tokio::time::advance(Duration::from_secs(90)).await;
        assert!(matches!(timer.poll(activity.last(), Instant::now()), IdleAction::Wait { .. }));
        tokio::time::advance(Duration::from_secs(4 * 60)).await;
        assert_eq!(timer.poll(activity.last(), Instant::now()), IdleAction::End);
    }
}
