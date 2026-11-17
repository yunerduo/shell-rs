//! Conversations kept between launches. One JSON index names them and says
//! which was open; each session is a JSONL journal that is appended to, so
//! a long conversation is never rewritten to add a line.
//!
//! Only what a person typed and what the assistant answered is restored.
//! Tool calls, their results and every approval stay in the audit trail,
//! which is the record a session must not duplicate.

use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::attachment::{Attachment, AttachmentRef};
use super::chat::ChatMessage;

/// How long a session is kept after its last change.
const KEEP_DAYS: i64 = 30;
const DAY_MILLIS: i64 = 24 * 60 * 60 * 1000;

/// The most characters a title takes from the first user message.
const TITLE_LIMIT: usize = 24;

/// A session's name in the index.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionMeta {
    pub id: String,
    /// Empty until the user says something; the panel shows a placeholder.
    pub title: String,
    pub created_at: i64,
    pub updated_at: i64,
}

/// The index file: every session, and the one that was open.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SessionIndex {
    pub sessions: Vec<SessionMeta>,
    pub active: Option<String>,
}

/// What an earlier compaction left behind: the summary a conversation shows
/// and sends, and how many journaled messages it replaced.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Compaction {
    pub summary: String,
    /// How many user and assistant messages the summary stands for, counting
    /// earlier compactions. The divider shows this number.
    pub count: usize,
    /// The summary could not be written, so the messages were only dropped.
    pub trimmed: bool,
}

/// One line of a session's journal.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SessionEvent {
    User {
        text: String,
        #[serde(default)]
        attachments: Vec<AttachmentRef>,
        /// When it was said, in milliseconds since the epoch. Journals
        /// written before times were kept have none, and such a line shows
        /// without a time.
        #[serde(default)]
        at: i64,
    },
    Assistant {
        text: String,
        #[serde(default)]
        at: i64,
    },
    Error {
        text: String,
    },
    /// Earlier messages were replaced by a summary. `count` is cumulative:
    /// how many journaled messages the summary now stands for.
    Compact {
        summary: String,
        count: usize,
        #[serde(default)]
        trimmed: bool,
    },
}

/// Where one session's attachment files live.
pub fn attachments_path(dir: &Path, id: &str) -> PathBuf {
    dir.join("assistant").join("attachments").join(id)
}

/// The index's path under the data directory.
pub fn index_path(dir: &Path) -> PathBuf {
    dir.join("assistant").join("sessions.json")
}

/// One session's journal.
pub fn journal_path(dir: &Path, id: &str) -> PathBuf {
    dir.join("assistant")
        .join("sessions")
        .join(format!("{id}.jsonl"))
}

/// The sessions of one data directory, with the index already read.
pub struct Sessions {
    dir: PathBuf,
    index: SessionIndex,
}

impl Sessions {
    /// Read the index, forget sessions that have not changed for a month,
    /// and make sure one is open. A broken index starts over: a conversation
    /// list is not worth failing the panel for.
    pub fn load(dir: &Path, now: i64) -> Self {
        let mut index: SessionIndex = fs::read_to_string(index_path(dir))
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default();
        let cutoff = now - KEEP_DAYS * DAY_MILLIS;
        let expired: Vec<String> = index
            .sessions
            .iter()
            .filter(|session| session.updated_at < cutoff)
            .map(|session| session.id.clone())
            .collect();
        for id in &expired {
            let _ = fs::remove_file(journal_path(dir, id));
            let _ = fs::remove_dir_all(attachments_path(dir, id));
        }
        index
            .sessions
            .retain(|session| session.updated_at >= cutoff);
        for session in &index.sessions {
            ensure_journal(dir, &session.id);
        }
        // Files whose index entry is gone are left alone: they are small and
        // naming them would mean trusting a file name as an id.
        index.sessions.sort_by_key(|session| session.created_at);
        let mut sessions = Self {
            dir: dir.to_path_buf(),
            index,
        };
        let active_ok = sessions
            .index
            .active
            .as_ref()
            .is_some_and(|id| sessions.index.sessions.iter().any(|s| &s.id == id));
        if !active_ok {
            sessions.index.active = sessions.index.sessions.first().map(|s| s.id.clone());
        }
        if sessions.index.sessions.is_empty() {
            sessions.create(now);
        }
        sessions.save();
        sessions
    }

    pub fn index(&self) -> &SessionIndex {
        &self.index
    }

    pub fn sessions(&self) -> &[SessionMeta] {
        &self.index.sessions
    }

    pub fn active(&self) -> Option<&SessionMeta> {
        let id = self.index.active.as_deref()?;
        self.index.sessions.iter().find(|session| session.id == id)
    }

    pub fn active_id(&self) -> Option<&str> {
        self.index.active.as_deref()
    }

    /// Make a session the open one. `false` when no such session exists.
    pub fn set_active(&mut self, id: &str) -> bool {
        if !self.index.sessions.iter().any(|session| session.id == id) {
            return false;
        }
        self.index.active = Some(id.to_string());
        self.save();
        true
    }

    /// Move to the next or previous session, wrapping around. `false` when
    /// there is only one to be on.
    pub fn cycle(&mut self, forward: bool) -> bool {
        let count = self.index.sessions.len();
        if count < 2 {
            return false;
        }
        let current = self
            .index
            .sessions
            .iter()
            .position(|session| Some(&session.id) == self.index.active.as_ref())
            .unwrap_or(0);
        let next = if forward {
            (current + 1) % count
        } else {
            (current + count - 1) % count
        };
        self.index.active = Some(self.index.sessions[next].id.clone());
        self.save();
        true
    }

    /// Start a session and open it. The id is unique in this index.
    pub fn create(&mut self, now: i64) -> String {
        let id = self.unique_id(now);
        self.index.sessions.push(SessionMeta {
            id: id.clone(),
            title: String::new(),
            created_at: now,
            updated_at: now,
        });
        self.index.active = Some(id.clone());
        ensure_journal(&self.dir, &id);
        self.save();
        id
    }

    /// Remove a session and its journal. The last one is replaced by a new
    /// empty session, so the panel always has somewhere to type.
    pub fn delete(&mut self, id: &str, now: i64) -> bool {
        let before = self.index.sessions.len();
        self.index.sessions.retain(|session| session.id != id);
        if self.index.sessions.len() == before {
            return false;
        }
        let _ = fs::remove_file(journal_path(&self.dir, id));
        let _ = fs::remove_dir_all(attachments_path(&self.dir, id));
        if self.index.active.as_deref() == Some(id) {
            self.index.active = self.index.sessions.last().map(|session| session.id.clone());
        }
        if self.index.sessions.is_empty() {
            self.create(now);
        } else {
            self.save();
        }
        true
    }

    /// Add one line. The first user message names the session.
    pub fn append(&mut self, id: &str, event: &SessionEvent, now: i64) -> std::io::Result<()> {
        let path = journal_path(&self.dir, id);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut line = serde_json::to_string(event)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
        line.push('\n');
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;
        file.write_all(line.as_bytes())?;
        if let Some(session) = self.index.sessions.iter_mut().find(|s| s.id == id) {
            session.updated_at = now;
            if session.title.is_empty()
                && let SessionEvent::User { text, .. } = event
            {
                session.title = title_from(text);
            }
        }
        self.save();
        Ok(())
    }

    /// The conversation to show: the user's and the assistant's words, in
    /// order, with the empty turns a stopped answer leaves behind dropped,
    /// and the summary an earlier compaction left behind.
    pub fn messages(&self, id: &str) -> (Vec<ChatMessage>, Option<Compaction>) {
        let Ok(text) = fs::read_to_string(journal_path(&self.dir, id)) else {
            return (Vec::new(), None);
        };
        let dir = attachments_path(&self.dir, id);
        let mut messages = Vec::new();
        let mut compaction: Option<Compaction> = None;
        for line in text.lines() {
            let Ok(event) = serde_json::from_str::<SessionEvent>(line) else {
                continue;
            };
            match event {
                SessionEvent::User {
                    text,
                    attachments,
                    at,
                } if !text.trim().is_empty() || !attachments.is_empty() => {
                    let attachments = attachments
                        .iter()
                        .filter_map(|reference| Attachment::restore(&dir, reference))
                        .collect();
                    messages.push(ChatMessage::user_with(text, attachments).with_time(at));
                }
                SessionEvent::Assistant { text, at } if !text.trim().is_empty() => {
                    messages.push(ChatMessage::assistant(text).with_time(at));
                }
                SessionEvent::Compact {
                    summary,
                    count,
                    trimmed,
                } => {
                    // `count` is cumulative: drop what this compaction added
                    // beyond the last one, which is already gone.
                    let previous = compaction.as_ref().map_or(0, |current| current.count);
                    let drop = count.saturating_sub(previous).min(messages.len());
                    messages.drain(..drop);
                    compaction = Some(Compaction {
                        summary,
                        count,
                        trimmed,
                    });
                }
                SessionEvent::User { .. }
                | SessionEvent::Assistant { .. }
                | SessionEvent::Error { .. } => {}
            }
        }
        (messages, compaction)
    }

    /// Where the open session's attachment files go.
    pub fn attachment_dir(&self, id: &str) -> PathBuf {
        attachments_path(&self.dir, id)
    }

    fn unique_id(&self, now: i64) -> String {
        let mut id = format!("{now:x}");
        let mut suffix = 1;
        while self.index.sessions.iter().any(|session| session.id == id) {
            id = format!("{now:x}-{suffix}");
            suffix += 1;
        }
        id
    }

    /// Write the index in one step, so a half-written file is never read.
    fn save(&self) {
        let path = index_path(&self.dir);
        let Some(parent) = path.parent() else {
            return;
        };
        if fs::create_dir_all(parent).is_err() {
            return;
        }
        let Ok(text) = serde_json::to_string_pretty(&self.index) else {
            return;
        };
        let temporary = path.with_extension("json.tmp");
        if fs::write(&temporary, text).is_ok() {
            let _ = fs::rename(&temporary, path);
        }
    }
}

/// Make sure a session's file can be appended to.
fn ensure_journal(dir: &Path, id: &str) {
    let path = journal_path(dir, id);
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if !path.exists() {
        let _ = fs::write(&path, "");
    }
}

/// The first words of a message, on one line and short enough for a row.
fn title_from(text: &str) -> String {
    let collapsed: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    collapsed.chars().take(TITLE_LIMIT).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "shellas-sessions-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|elapsed| elapsed.as_nanos())
                .unwrap_or_default()
        ));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn a_fresh_directory_gets_one_empty_session() {
        let dir = temp_dir("fresh");
        let sessions = Sessions::load(&dir, 1_700_000_000_000);
        assert_eq!(sessions.sessions().len(), 1);
        assert_eq!(
            sessions.active_id(),
            Some(sessions.sessions()[0].id.as_str())
        );
        assert!(sessions.active().unwrap().title.is_empty());
        assert!(index_path(&dir).exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_journal_round_trips_the_words_that_were_said() {
        let dir = temp_dir("journal");
        let mut sessions = Sessions::load(&dir, 1_700_000_000_000);
        let id = sessions.active_id().unwrap().to_string();
        sessions
            .append(
                &id,
                &SessionEvent::User {
                    text: "check the disk".into(),
                    attachments: Vec::new(),
                    at: 1_700_000_000_500,
                },
                1,
            )
            .expect("append");
        sessions
            .append(
                &id,
                &SessionEvent::Assistant {
                    text: "It is 80% full.".into(),
                    at: 1_700_000_001_000,
                },
                2,
            )
            .expect("append");
        sessions
            .append(
                &id,
                &SessionEvent::Error {
                    text: "timeout".into(),
                },
                3,
            )
            .expect("append");
        let messages = sessions.messages(&id).0;
        assert_eq!(messages.len(), 2, "an error is not part of the talk");
        assert_eq!(messages[0].text, "check the disk");
        assert_eq!(messages[0].at, 1_700_000_000_500, "the time comes back too");
        assert_eq!(messages[1].text, "It is 80% full.");
        assert_eq!(messages[1].at, 1_700_000_001_000);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_first_user_message_names_the_session_and_stops_at_the_limit() {
        let dir = temp_dir("title");
        let mut sessions = Sessions::load(&dir, 1_700_000_000_000);
        let id = sessions.active_id().unwrap().to_string();
        let long = format!("  line one\nline two  {}", "x".repeat(80));
        sessions
            .append(
                &id,
                &SessionEvent::User {
                    text: long,
                    attachments: Vec::new(),
                    at: 1,
                },
                1,
            )
            .expect("append");
        sessions
            .append(
                &id,
                &SessionEvent::User {
                    text: "second".into(),
                    attachments: Vec::new(),
                    at: 2,
                },
                2,
            )
            .expect("append");
        let title = sessions.active().unwrap().title.clone();
        assert!(title.starts_with("line one line two"), "{title}");
        assert_eq!(title.chars().count(), TITLE_LIMIT);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn switching_wraps_and_keeps_one_open() {
        let dir = temp_dir("switch");
        let mut sessions = Sessions::load(&dir, 1_700_000_000_000);
        let first = sessions.active_id().unwrap().to_string();
        let second = sessions.create(1_700_000_001_000);
        assert_eq!(sessions.active_id(), Some(second.as_str()));
        assert!(sessions.cycle(true));
        assert_eq!(sessions.active_id(), Some(first.as_str()));
        assert!(sessions.cycle(false));
        assert_eq!(sessions.active_id(), Some(second.as_str()));
        assert!(sessions.set_active(&first));
        assert!(!sessions.set_active("missing"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn deleting_the_last_session_leaves_a_new_empty_one() {
        let dir = temp_dir("delete");
        let mut sessions = Sessions::load(&dir, 1_700_000_000_000);
        let id = sessions.active_id().unwrap().to_string();
        assert!(sessions.delete(&id, 1_700_000_002_000));
        assert_eq!(sessions.sessions().len(), 1);
        assert_ne!(sessions.active_id(), Some(id.as_str()));
        assert!(!journal_path(&dir, &id).exists());
        assert!(
            sessions
                .messages(sessions.active_id().unwrap())
                .0
                .is_empty()
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_session_untouched_for_a_month_is_forgotten() {
        let dir = temp_dir("stale");
        let now = 1_700_000_000_000;
        let mut sessions = Sessions::load(&dir, now);
        let stale = sessions.create(now - (KEEP_DAYS + 1) * DAY_MILLIS);
        let old_journal = journal_path(&dir, &stale);
        assert!(old_journal.exists());
        let sessions = Sessions::load(&dir, now);
        assert!(
            sessions
                .sessions()
                .iter()
                .all(|session| session.id != stale)
        );
        assert!(!old_journal.exists(), "the old journal is cleaned up too");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_broken_index_starts_over_instead_of_failing() {
        let dir = temp_dir("broken");
        let path = index_path(&dir);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "{not json").unwrap();
        let sessions = Sessions::load(&dir, 1_700_000_000_000);
        assert_eq!(sessions.sessions().len(), 1);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn attachments_come_back_with_the_message() {
        let dir = temp_dir("attachments");
        let mut sessions = Sessions::load(&dir, 1_700_000_000_000);
        let id = sessions.active_id().unwrap().to_string();
        let attachment = Attachment::text("build.log", b"line one").expect("attachment");
        attachment
            .save(&sessions.attachment_dir(&id))
            .expect("save");
        sessions
            .append(
                &id,
                &SessionEvent::User {
                    text: "look".into(),
                    attachments: vec![attachment.reference()],
                    at: 1,
                },
                1,
            )
            .expect("append");
        let messages = sessions.messages(&id).0;
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].attachments.len(), 1);
        assert_eq!(
            messages[0].attachments[0]
                .text_content()
                .map(|(text, _)| text),
            Some("line one")
        );

        sessions.delete(&id, 2);
        assert!(!attachments_path(&dir, &id).exists(), "files go with it");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_compaction_replays_and_keeps_the_merged_summary() {
        let dir = temp_dir("compaction");
        let mut sessions = Sessions::load(&dir, 1_700_000_000_000);
        let id = sessions.active_id().unwrap().to_string();
        let append = |sessions: &mut Sessions, text: &str, user: bool, at: i64| {
            let event = if user {
                SessionEvent::User {
                    text: text.into(),
                    attachments: Vec::new(),
                    at,
                }
            } else {
                SessionEvent::Assistant {
                    text: text.into(),
                    at,
                }
            };
            sessions.append(&id, &event, at).expect("append");
        };
        append(&mut sessions, "one", true, 1);
        append(&mut sessions, "two", false, 2);
        append(&mut sessions, "three", true, 3);
        append(&mut sessions, "four", false, 4);
        sessions
            .append(
                &id,
                &SessionEvent::Compact {
                    summary: "the start".into(),
                    count: 2,
                    trimmed: false,
                },
                5,
            )
            .expect("append");
        append(&mut sessions, "five", true, 6);
        append(&mut sessions, "six", false, 7);
        sessions
            .append(
                &id,
                &SessionEvent::Compact {
                    summary: "one to four".into(),
                    count: 4,
                    trimmed: false,
                },
                8,
            )
            .expect("append");

        let (messages, compaction) = sessions.messages(&id);
        assert_eq!(
            messages
                .iter()
                .map(|message| message.text.as_str())
                .collect::<Vec<_>>(),
            ["five", "six"]
        );
        let compaction = compaction.expect("a compaction comes back");
        assert_eq!(compaction.summary, "one to four");
        assert_eq!(compaction.count, 4);
        assert!(!compaction.trimmed);
        let _ = fs::remove_dir_all(&dir);
    }
}
