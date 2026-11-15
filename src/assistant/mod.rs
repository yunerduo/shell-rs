//! The AI terminal assistant: a first-class tool of the right sidebar that
//! talks to a model service and, from P1 on, runs what the user's policy
//! allows. See `docs/ai-terminal-assistant.zh.md`.

pub mod audit;
pub mod bindings;
pub mod chat;
pub mod classifier;
pub mod client;
pub mod inject;
pub mod output;
pub mod panel;
pub mod policy;
pub mod protocol;
pub mod settings;
pub mod tool_result;
pub mod wait;

#[cfg(test)]
mod live_tests;

pub use bindings::AssistantTerminal;
pub use panel::AssistantPanel;
