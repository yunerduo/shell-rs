//! 匿名使用统计: how many installations, how many devices a day, which
//! versions, and which features get used, reported to this fork's own
//! endpoint with the Aptabase protocol.
//!
//! What goes out: the version, the operating system and its version, the
//! CPU architecture, a few settings (each from a fixed set of values, the
//! saved hosts and such only counted, in bands), and how many times each
//! feature was used. Never a host, credential, command, file name, path or
//! anything that identifies the machine: the server tells devices apart from
//! the address and the User-Agent, hashed with a salt it replaces daily.
//!
//! Aptabase has no Rust SDK beyond its Tauri plugin, so this posts to the
//! REST API itself, as its other SDKs do: batches of up to 25 events, the
//! network and its 5xx retried, any other refusal dropped. Unlike them,
//! nothing runs on a timer: the workspace sends signals (started, the
//! window came forward, a feature was used, the switch flipped) to one
//! thread, which counts, works out what is due and sends it then. Feature
//! uses are summed for four hours into one `usage` event and kept on disk
//! until it is sent, so quitting loses nothing and never waits.
//!
//! Only release builds report; a development build does with
//! `SHELLAS_ANALYTICS_DEBUG` set, as debug events, summed by the minute and
//! retried after ten seconds. The external CLI, a second copy passing its
//! links on, and the UI tests never start it.

mod client;
mod model;
mod system;

pub use client::{Analytics, AnalyticsServices, HttpTransport, Transport};
pub use model::{Counter, Delivery, Signal, Snapshot};
pub use system::SystemInfo;

/// The app the events are sent as. Not a secret: every build carries it,
/// and all it allows is sending events.
pub const APP_KEY: &str = "A-US-1582708162";

/// Where the events go. The one place the address lives: the protocol, the
/// path and the failure handling are the Aptabase ones.
pub const ENDPOINT: &str = "https://shellas.agree.host/api/v0/events";
