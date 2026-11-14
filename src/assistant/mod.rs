//! The AI terminal assistant: a first-class tool of the right sidebar that
//! talks to a model service and, from P1 on, runs what the user's policy
//! allows. See `docs/ai-terminal-assistant.zh.md`.

pub mod bindings;
pub mod chat;
pub mod client;
pub mod panel;
pub mod protocol;
pub mod settings;

pub use bindings::AssistantTerminal;
pub use panel::AssistantPanel;
