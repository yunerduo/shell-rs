//! Scheduled tasks: a fixed command or a fixed prompt run on a simple
//! recurrence, in the app only, with no catch-up for the time it was not
//! running. See section 8 of `docs/ai-terminal-assistant.zh.md`.
//!
//! The module is pure: the recurrence arithmetic takes the time it works
//! from, and the store takes the directory, so the tests need neither a
//! clock nor the app's data directory. The panel owns the running.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use chrono::{DateTime, Datelike, Duration as ChronoDuration, NaiveDate, TimeZone, Weekday};
use serde::{Deserialize, Serialize};

use super::policy::{self, AuthorizationLevel, Policy, Risk, Verdict};

/// The shortest and longest period an interval task may ask for.
pub const MIN_INTERVAL_MINUTES: u32 = 1;
pub const MAX_INTERVAL_MINUTES: u32 = 1440;

/// How many missed occurrences are counted before the rest are given up on.
/// A clock that jumped a year must not keep the panel counting.
pub const MISSED_CAP: u32 = 1000;

/// When a task runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Recurrence {
    /// Every `n` minutes from the previous due time.
    EveryMinutes { n: u32 },
    /// Every day at a local time.
    DailyAt { hour: u32, minute: u32 },
    /// Every week on a weekday (1 = Monday to 7 = Sunday) at a local time.
    WeeklyAt {
        weekday: u32,
        hour: u32,
        minute: u32,
    },
}

impl Default for Recurrence {
    fn default() -> Self {
        Self::EveryMinutes { n: 15 }
    }
}

impl Recurrence {
    /// The same recurrence with its numbers brought into range, so a hand
    /// edited file cannot make the panel loop.
    pub fn normalized(self) -> Self {
        match self {
            Self::EveryMinutes { n } => Self::EveryMinutes {
                n: n.clamp(MIN_INTERVAL_MINUTES, MAX_INTERVAL_MINUTES),
            },
            Self::DailyAt { hour, minute } => Self::DailyAt {
                hour: hour.min(23),
                minute: minute.min(59),
            },
            Self::WeeklyAt {
                weekday,
                hour,
                minute,
            } => Self::WeeklyAt {
                weekday: weekday.clamp(1, 7),
                hour: hour.min(23),
                minute: minute.min(59),
            },
        }
    }

    /// The longest gap the recurrence can have between two occurrences, in
    /// minutes. Used to tell a due time in the future from a clock that was
    /// set back.
    pub fn period_minutes(self) -> u32 {
        match self.normalized() {
            Self::EveryMinutes { n } => n,
            Self::DailyAt { .. } => 24 * 60,
            Self::WeeklyAt { .. } => 7 * 24 * 60,
        }
    }
}

/// What a task does when it triggers. A fixed command is frozen when the
/// task is made; a fixed prompt only reads and reports.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ScheduleAction {
    FixedCommand { command: String },
    FixedPrompt { prompt: String },
}

impl ScheduleAction {
    /// The text kept on the task, for the list.
    pub fn text(&self) -> &str {
        match self {
            Self::FixedCommand { command } => command,
            Self::FixedPrompt { prompt } => prompt,
        }
    }

    pub fn is_prompt(&self) -> bool {
        matches!(self, Self::FixedPrompt { .. })
    }
}

/// The one terminal a task runs on. It is a snapshot: the runtime id can be
/// matched again while the session lasts, the title and the side are what a
/// later session can look for.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScheduleTarget {
    pub terminal_id: String,
    pub title: String,
    pub alias: String,
    #[serde(default)]
    pub local: bool,
}

/// Why a second terminal cannot be bound to a task.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BindError {
    /// The task already runs on a different terminal.
    SecondTarget,
}

impl ScheduleTarget {
    pub fn new(
        terminal_id: impl Into<String>,
        title: impl Into<String>,
        alias: impl Into<String>,
        local: bool,
    ) -> Self {
        Self {
            terminal_id: terminal_id.into(),
            title: title.into(),
            alias: alias.into(),
            local,
        }
    }
}

/// One scheduled task.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Schedule {
    pub id: String,
    pub name: String,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    pub target: ScheduleTarget,
    pub action: ScheduleAction,
    #[serde(default)]
    pub recurrence: Recurrence,
    #[serde(default = "default_timeout")]
    pub timeout_seconds: u32,
    #[serde(default)]
    pub last_run_at: Option<i64>,
    #[serde(default)]
    pub next_due_at: i64,
    #[serde(default)]
    pub created_at: i64,
    #[serde(default)]
    pub updated_at: i64,
    /// Set when a later session could not find the terminal it names. The
    /// task does not run until the user binds it again.
    #[serde(default)]
    pub needs_rebind: bool,
}

fn default_enabled() -> bool {
    true
}

fn default_timeout() -> u32 {
    30
}

impl Schedule {
    /// A fixed-command task, due one period from `now`.
    pub fn fixed_command<Tz: TimeZone>(
        now: DateTime<Tz>,
        name: impl Into<String>,
        target: ScheduleTarget,
        command: impl Into<String>,
        recurrence: Recurrence,
    ) -> Self
    where
        Tz::Offset: std::fmt::Display,
    {
        let recurrence = recurrence.normalized();
        let created = now.timestamp_millis();
        Self {
            id: format!("schedule-{created}"),
            name: name.into(),
            enabled: true,
            target,
            action: ScheduleAction::FixedCommand {
                command: command.into(),
            },
            recurrence,
            timeout_seconds: 30,
            last_run_at: None,
            next_due_at: next_due_after(now, recurrence).timestamp_millis(),
            created_at: created,
            updated_at: created,
            needs_rebind: false,
        }
    }

    /// A fixed-prompt task, due one period from `now`.
    pub fn fixed_prompt<Tz: TimeZone>(
        now: DateTime<Tz>,
        name: impl Into<String>,
        target: ScheduleTarget,
        prompt: impl Into<String>,
        recurrence: Recurrence,
    ) -> Self
    where
        Tz::Offset: std::fmt::Display,
    {
        let recurrence = recurrence.normalized();
        let created = now.timestamp_millis();
        Self {
            id: format!("prompt-{created}"),
            name: name.into(),
            enabled: true,
            target,
            action: ScheduleAction::FixedPrompt {
                prompt: prompt.into(),
            },
            recurrence,
            timeout_seconds: 60,
            last_run_at: None,
            next_due_at: next_due_after(now, recurrence).timestamp_millis(),
            created_at: created,
            updated_at: created,
            needs_rebind: false,
        }
    }

    /// Bind the one terminal this task may use. A different terminal is
    /// refused rather than replaced: §8.1 allows exactly one.
    pub fn bind_target(&mut self, target: ScheduleTarget) -> Result<(), BindError> {
        if self.target.terminal_id != target.terminal_id {
            return Err(BindError::SecondTarget);
        }
        self.target = target;
        self.needs_rebind = false;
        Ok(())
    }

    /// Whether the task is due at `now` (in milliseconds).
    pub fn is_due(&self, now_ms: i64) -> bool {
        self.enabled && !self.needs_rebind && self.next_due_at <= now_ms
    }
}

/// What a task's stored due time means at startup.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ResumePlan {
    /// Occurrences that came and went while the app was not running.
    pub missed: u32,
    /// What the next due time is now.
    pub next_due: i64,
    /// The due time was further ahead than one period, which reads as a
    /// clock that was set back.
    pub rewound: bool,
}

/// The next occurrence strictly after `now`.
pub fn next_due_after<Tz: TimeZone>(now: DateTime<Tz>, recurrence: Recurrence) -> DateTime<Tz>
where
    Tz::Offset: std::fmt::Display,
{
    match recurrence.normalized() {
        Recurrence::EveryMinutes { n } => now + ChronoDuration::minutes(i64::from(n)),
        Recurrence::DailyAt { hour, minute } => {
            let mut date = now.date_naive();
            for _ in 0..4 {
                if let Some(candidate) = at_time(&now.timezone(), date, hour, minute)
                    && candidate > now
                {
                    return candidate;
                }
                date = next_date(date);
            }
            now + ChronoDuration::days(1)
        }
        Recurrence::WeeklyAt {
            weekday,
            hour,
            minute,
        } => {
            let wanted = (weekday.saturating_sub(1).min(6)) as i64;
            let today = i64::from(now.weekday().num_days_from_monday());
            let start = (wanted - today).rem_euclid(7);
            for offset in start..start + 14 {
                let date = now.date_naive() + ChronoDuration::days(offset);
                if let Some(candidate) = at_time(&now.timezone(), date, hour, minute)
                    && candidate > now
                {
                    return candidate;
                }
            }
            now + ChronoDuration::days(7)
        }
    }
}

/// The occurrence after `due`.
pub fn advance_due<Tz: TimeZone>(due: DateTime<Tz>, recurrence: Recurrence) -> DateTime<Tz>
where
    Tz::Offset: std::fmt::Display,
{
    match recurrence.normalized() {
        Recurrence::EveryMinutes { n } => due + ChronoDuration::minutes(i64::from(n)),
        Recurrence::DailyAt { hour, minute } => {
            let mut date = next_date(due.date_naive());
            for _ in 0..4 {
                if let Some(candidate) = at_time(&due.timezone(), date, hour, minute) {
                    return candidate;
                }
                date = next_date(date);
            }
            due + ChronoDuration::days(1)
        }
        Recurrence::WeeklyAt { hour, minute, .. } => {
            let mut date = due.date_naive() + ChronoDuration::days(7);
            for _ in 0..4 {
                if let Some(candidate) = at_time(&due.timezone(), date, hour, minute) {
                    return candidate;
                }
                date += ChronoDuration::days(7);
            }
            due + ChronoDuration::days(7)
        }
    }
}

/// What to do with a stored due time at startup: count the misses, then move
/// the due time forward. Nothing is ever run to make up for a miss.
pub fn resume_at<Tz: TimeZone>(now: DateTime<Tz>, due_ms: i64, recurrence: Recurrence) -> ResumePlan
where
    Tz::Offset: std::fmt::Display,
{
    let Some(due) = now.timezone().timestamp_millis_opt(due_ms).earliest() else {
        return ResumePlan {
            missed: 0,
            next_due: next_due_after(now, recurrence).timestamp_millis(),
            rewound: true,
        };
    };
    let period = ChronoDuration::minutes(i64::from(recurrence.period_minutes()));
    if due > now {
        // A due time further ahead than one period is not the next
        // occurrence; the clock was set back. Recompute from now.
        let ahead = due_ms.saturating_sub(now.timestamp_millis());
        if ahead > period.num_milliseconds() * 2 {
            return ResumePlan {
                missed: 0,
                next_due: next_due_after(now, recurrence).timestamp_millis(),
                rewound: true,
            };
        }
        return ResumePlan {
            missed: 0,
            next_due: due_ms,
            rewound: false,
        };
    }
    let mut cursor = due;
    let mut missed = 0;
    while cursor <= now && missed < MISSED_CAP {
        missed += 1;
        cursor = advance_due(cursor, recurrence);
    }
    let next_due = if cursor > now {
        cursor
    } else {
        next_due_after(now, recurrence)
    };
    ResumePlan {
        missed,
        next_due: next_due.timestamp_millis(),
        rewound: false,
    }
}

/// What an unattended trigger does: run it, put it on the review list, or
/// refuse it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Unattended {
    Run,
    Review(policy::Decision),
    Refuse(policy::Decision),
}

/// Judge a scheduled command with no user present. The level is always
/// restricted — full access is a decision about an interactive session and
/// does not reach a scheduled run — and only the whitelist's R0 may run by
/// itself.
pub fn unattended_action(command: &str, policy: &Policy) -> Unattended {
    let decision = policy::decide(command, AuthorizationLevel::Restricted, policy);
    match decision.verdict {
        Verdict::Auto if decision.risk == Risk::R0 => Unattended::Run,
        Verdict::Deny => Unattended::Refuse(decision),
        Verdict::Auto | Verdict::Confirm => Unattended::Review(decision),
    }
}

/// The file the tasks are kept in, under the assistant's directory.
pub fn path_in(dir: &Path) -> PathBuf {
    dir.join("assistant").join("schedules.json")
}

/// Every task of this installation.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ScheduleStore {
    pub schedules: Vec<Schedule>,
}

impl ScheduleStore {
    /// Read the tasks. A missing or broken file means none, not an error the
    /// panel has to show: the tasks can be made again.
    pub fn load(path: &Path) -> Self {
        fs::read_to_string(path)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }

    /// Write the tasks in one step, so a half-written file is never read.
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let text = serde_json::to_string_pretty(self)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
        let temporary = path.with_extension("json.tmp");
        fs::write(&temporary, text)?;
        fs::rename(&temporary, path)
    }

    /// Give the task an id no other task holds.
    pub fn add(&mut self, mut schedule: Schedule) -> usize {
        let taken = |id: &str| self.schedules.iter().any(|task| task.id == id);
        if taken(&schedule.id) {
            for suffix in 2.. {
                let candidate = format!("{}-{suffix}", schedule.id);
                if !taken(&candidate) {
                    schedule.id = candidate;
                    break;
                }
            }
        }
        schedule.created_at = now_millis();
        schedule.updated_at = schedule.created_at;
        self.schedules.push(schedule);
        self.schedules.len() - 1
    }

    pub fn remove(&mut self, id: &str) -> bool {
        let before = self.schedules.len();
        self.schedules.retain(|task| task.id != id);
        self.schedules.len() != before
    }

    pub fn get(&self, id: &str) -> Option<&Schedule> {
        self.schedules.iter().find(|task| task.id == id)
    }

    pub fn get_mut(&mut self, id: &str) -> Option<&mut Schedule> {
        self.schedules.iter_mut().find(|task| task.id == id)
    }
}

/// Milliseconds since the epoch, the unit the due times are stored in.
pub fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis().min(i64::MAX as u128) as i64)
        .unwrap_or_default()
}

fn at_time<Tz: TimeZone>(
    zone: &Tz,
    date: NaiveDate,
    hour: u32,
    minute: u32,
) -> Option<DateTime<Tz>> {
    zone.with_ymd_and_hms(date.year(), date.month(), date.day(), hour, minute, 0)
        .earliest()
}

/// The day after `date`, or `date` itself at the end of the calendar.
fn next_date(date: NaiveDate) -> NaiveDate {
    date.succ_opt().unwrap_or(date)
}

/// The weekday as the UI shows it: 1 = Monday to 7 = Sunday.
pub fn weekday_number(weekday: Weekday) -> u32 {
    weekday.num_days_from_monday() + 1
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};

    fn at(year: i32, month: u32, day: u32, hour: u32, minute: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(year, month, day, hour, minute, 0)
            .single()
            .expect("a valid time")
    }

    fn target() -> ScheduleTarget {
        ScheduleTarget::new("remote:1", "web", "web", false)
    }

    #[test]
    fn an_interval_task_is_due_one_period_after_it_is_made() {
        let now = at(2026, 11, 15, 10, 0);
        let task = Schedule::fixed_command(
            now,
            "disk",
            target(),
            "df -h",
            Recurrence::EveryMinutes { n: 15 },
        );
        assert_eq!(
            task.next_due_at,
            at(2026, 11, 15, 10, 15).timestamp_millis()
        );
        assert!(!task.is_due(now.timestamp_millis()));
        assert!(task.is_due(at(2026, 11, 15, 10, 15).timestamp_millis()));
    }

    #[test]
    fn a_daily_task_waits_for_tomorrow_when_today_has_passed() {
        let now = at(2026, 11, 15, 10, 0);
        let next = next_due_after(
            now,
            Recurrence::DailyAt {
                hour: 9,
                minute: 30,
            },
        );
        assert_eq!(next, at(2026, 11, 16, 9, 30));
        let later_today = next_due_after(
            now,
            Recurrence::DailyAt {
                hour: 18,
                minute: 0,
            },
        );
        assert_eq!(later_today, at(2026, 11, 15, 18, 0));
    }

    #[test]
    fn a_weekly_task_finds_its_weekday() {
        // 2026-11-15 is a Sunday.
        let now = at(2026, 11, 15, 10, 0);
        assert_eq!(weekday_number(now.weekday()), 7);
        let monday = next_due_after(
            now,
            Recurrence::WeeklyAt {
                weekday: 1,
                hour: 8,
                minute: 0,
            },
        );
        assert_eq!(monday, at(2026, 11, 16, 8, 0));
        let sunday = next_due_after(
            now,
            Recurrence::WeeklyAt {
                weekday: 7,
                hour: 11,
                minute: 0,
            },
        );
        assert_eq!(sunday, at(2026, 11, 15, 11, 0));
    }

    #[test]
    fn a_missed_run_is_counted_and_not_run() {
        let now = at(2026, 11, 15, 10, 0);
        let due = at(2026, 11, 15, 9, 29);
        let plan = resume_at(
            now,
            due.timestamp_millis(),
            Recurrence::EveryMinutes { n: 15 },
        );
        assert_eq!(plan.missed, 3, "09:29, 09:44 and 09:59 came and went");
        assert_eq!(plan.next_due, at(2026, 11, 15, 10, 14).timestamp_millis());
        assert!(!plan.rewound);
    }

    #[test]
    fn a_due_time_in_the_near_future_is_kept() {
        let now = at(2026, 11, 15, 10, 0);
        let due = at(2026, 11, 15, 10, 5);
        let plan = resume_at(
            now,
            due.timestamp_millis(),
            Recurrence::EveryMinutes { n: 15 },
        );
        assert_eq!(plan.missed, 0);
        assert_eq!(plan.next_due, due.timestamp_millis());
        assert!(!plan.rewound);
    }

    #[test]
    fn a_clock_set_back_recomputes_from_now() {
        let now = at(2026, 11, 15, 10, 0);
        let due = at(2026, 11, 25, 10, 0);
        let plan = resume_at(
            now,
            due.timestamp_millis(),
            Recurrence::EveryMinutes { n: 15 },
        );
        assert!(plan.rewound);
        assert_eq!(plan.missed, 0);
        assert_eq!(plan.next_due, at(2026, 11, 15, 10, 15).timestamp_millis());
    }

    #[test]
    fn a_daily_task_missed_for_days_only_counts_them() {
        let now = at(2026, 11, 15, 12, 0);
        let due = at(2026, 11, 12, 9, 0);
        let plan = resume_at(
            now,
            due.timestamp_millis(),
            Recurrence::DailyAt { hour: 9, minute: 0 },
        );
        assert_eq!(plan.missed, 4, "12, 13, 14 and 15 at 09:00");
        assert_eq!(plan.next_due, at(2026, 11, 16, 9, 0).timestamp_millis());
    }

    #[test]
    fn a_whitelisted_read_only_command_runs_unattended() {
        let policy = Policy::new(Vec::new(), vec!["df -h".into()]);
        assert_eq!(unattended_action("df -h", &policy), Unattended::Run);
    }

    #[test]
    fn a_write_waits_for_review_and_a_refusal_stays_refused() {
        let policy = Policy::new(Vec::new(), Vec::new());
        assert!(matches!(
            unattended_action("touch /tmp/report", &policy),
            Unattended::Review(_)
        ));
        assert!(matches!(
            unattended_action("rm -rf /", &policy),
            Unattended::Refuse(_)
        ));
        let blacklisted = Policy::new(vec!["systemctl stop *".into()], Vec::new());
        assert!(matches!(
            unattended_action("systemctl stop nginx", &blacklisted),
            Unattended::Refuse(_)
        ));
    }

    #[test]
    fn full_access_never_reaches_an_unattended_command() {
        // The unattended judgment has no level argument: a write is a card
        // even when the interactive session is in full access.
        let policy = Policy::new(Vec::new(), Vec::new());
        assert!(matches!(
            unattended_action("mkdir /tmp/x", &policy),
            Unattended::Review(_)
        ));
    }

    #[test]
    fn a_second_terminal_is_refused() {
        let mut task = Schedule::fixed_command(
            at(2026, 11, 15, 10, 0),
            "disk",
            target(),
            "df -h",
            Recurrence::default(),
        );
        assert_eq!(
            task.bind_target(ScheduleTarget::new("remote:2", "db", "db", false)),
            Err(BindError::SecondTarget)
        );
        assert_eq!(task.target.terminal_id, "remote:1");
        assert_eq!(
            task.bind_target(ScheduleTarget::new("remote:1", "web-2", "web-2", false)),
            Ok(())
        );
        assert_eq!(task.target.alias, "web-2");
    }

    #[test]
    fn a_disabled_or_unbound_task_is_never_due() {
        let now = at(2026, 11, 15, 10, 0);
        let mut task = Schedule::fixed_command(
            now,
            "disk",
            target(),
            "df -h",
            Recurrence::EveryMinutes { n: 15 },
        );
        let due = task.next_due_at;
        task.enabled = false;
        assert!(!task.is_due(due));
        task.enabled = true;
        task.needs_rebind = true;
        assert!(!task.is_due(due));
    }

    #[test]
    fn tasks_survive_a_write_and_a_read() {
        let dir = std::env::temp_dir().join(format!(
            "shellrs-schedules-{}-{}",
            std::process::id(),
            now_millis()
        ));
        let path = path_in(&dir);
        let mut store = ScheduleStore::default();
        let first = Schedule::fixed_command(
            at(2026, 11, 15, 10, 0),
            "disk",
            target(),
            "df -h",
            Recurrence::EveryMinutes { n: 15 },
        );
        store.add(first);
        store.add(Schedule::fixed_prompt(
            at(2026, 11, 15, 10, 0),
            "summary",
            target(),
            "Summarize the screen",
            Recurrence::DailyAt { hour: 9, minute: 0 },
        ));
        store.save(&path).expect("save");
        let loaded = ScheduleStore::load(&path);
        assert_eq!(loaded.schedules.len(), 2);
        assert_eq!(loaded.schedules[0].target.alias, "web");
        assert!(loaded.schedules[1].action.is_prompt());
        assert_eq!(
            loaded.schedules[1].recurrence,
            Recurrence::DailyAt { hour: 9, minute: 0 }
        );
        let _ = std::fs::remove_dir_all(&dir);
        assert!(ScheduleStore::load(&path).schedules.is_empty());
    }

    #[test]
    fn ids_do_not_collide_when_two_tasks_are_made_in_one_millisecond() {
        let now = at(2026, 11, 15, 10, 0);
        let mut store = ScheduleStore::default();
        let first = Schedule::fixed_command(now, "a", target(), "df -h", Recurrence::default());
        store.add(first);
        let second = Schedule::fixed_command(now, "b", target(), "uptime", Recurrence::default());
        store.add(second);
        assert_ne!(store.schedules[0].id, store.schedules[1].id);
    }

    #[test]
    fn numbers_from_a_hand_edited_file_are_brought_into_range() {
        assert_eq!(
            Recurrence::EveryMinutes { n: 0 }.normalized(),
            Recurrence::EveryMinutes {
                n: MIN_INTERVAL_MINUTES
            }
        );
        assert_eq!(
            Recurrence::DailyAt {
                hour: 99,
                minute: 99
            }
            .normalized(),
            Recurrence::DailyAt {
                hour: 23,
                minute: 59
            }
        );
        assert_eq!(
            Recurrence::WeeklyAt {
                weekday: 0,
                hour: 9,
                minute: 0
            }
            .normalized(),
            Recurrence::WeeklyAt {
                weekday: 1,
                hour: 9,
                minute: 0
            }
        );
    }
}
