//! The injection path's logic that does not need a window: which way one
//! call runs (beside the shell, or typed into it), and what the model is told
//! when it does not run or does not end.

use serde_json::json;

use crate::terminal::{BusyReason, RunContext, RunFailure};

use super::output;

/// Where a call runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Route {
    /// Beside the shell: no terminal is touched.
    Exec,
    /// Typed into the shell, after the user's takeover authorization.
    Inject,
    /// Injection is the only path, and the user has not authorized it yet.
    NeedTakeover,
    /// No path exists.
    Unavailable(&'static str),
}

/// Choose the path for one call.
///
/// A background call, and a terminal with no channel beside the shell, can
/// only be injected, so they need the takeover authorization even when the
/// policy let the command run alone. On a terminal beside which a command
/// can run, the user's checkbox is what chooses injection.
pub fn route(background: bool, can_exec: bool, can_inject: bool, takeover: bool) -> Route {
    let injection = background || !can_exec || takeover;
    if !injection {
        return Route::Exec;
    }
    match (can_inject, takeover) {
        (false, _) => Route::Unavailable("no_channel"),
        (true, true) => Route::Inject,
        (true, false) => Route::NeedTakeover,
    }
}

/// One injection, as the panel finishes it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct InjectOutcome {
    pub context: Option<RunContext>,
    /// The command's output, both streams as one, as the scanner saw them.
    pub output: Vec<u8>,
    pub exit_code: Option<i32>,
    pub failure: Option<RunFailure>,
    pub background: bool,
    /// `Some("user")` when the user typed and took the terminal back,
    /// `Some("stop")` when the stop button sent the ^C.
    pub cancelled_by: Option<&'static str>,
}

impl InjectOutcome {
    /// What the model reads.
    pub fn body(&self) -> String {
        if let Some(failure) = &self.failure {
            return failure_body(failure);
        }
        let rendered = output::render(&self.output, output::MODEL_CHARS);
        let mut body = json!({
            "status": "ok",
            "channel": "terminal",
            "background": self.background,
            "output": rendered.text,
            "truncated": rendered.truncated,
        });
        if let Some(code) = self.exit_code {
            body["exit_code"] = json!(code);
        }
        if let Some(context) = &self.context {
            body["context"] = json!({
                "user": context.user,
                "host": context.host,
                "cwd": context.cwd,
                "shell": context.shell,
                "interp": context.interp,
            });
        }
        if let Some(cancelled) = self.cancelled_by {
            body["cancelled_by"] = json!(cancelled);
        }
        body.to_string()
    }
}

/// Why a run did not start or did not end, in the shape the card and the
/// model both read. `reason` names a translation key's last part.
pub fn failure_body(failure: &RunFailure) -> String {
    let (status, reason) = match failure {
        RunFailure::HostNotFound | RunFailure::NoTerminal | RunFailure::NotConnected => {
            ("unsupported", "not_connected")
        }
        RunFailure::UnsupportedShell(_) => ("unsupported", "unsupported_shell"),
        RunFailure::TooLong { .. } => ("unsupported", "too_long"),
        RunFailure::Busy(BusyReason::FullScreen) => ("busy", "full_screen"),
        RunFailure::Busy(BusyReason::Transfer) => ("busy", "transfer"),
        RunFailure::Busy(BusyReason::AnotherRun) => ("busy", "another_run"),
        RunFailure::Busy(BusyReason::NotStarted) => ("busy", "not_started"),
        RunFailure::Busy(BusyReason::NoAnswer) => ("busy", "no_answer"),
        RunFailure::Disconnected => ("unknown", "disconnected"),
    };
    let mut body = json!({
        "status": status,
        "reason": reason,
        "channel": "terminal",
    });
    match failure {
        RunFailure::UnsupportedShell(shell) => body["shell"] = json!(shell),
        RunFailure::TooLong { limit } => body["limit"] = json!(limit),
        // The command was typed before the connection went: part of it may
        // have run, and running it again is not safe.
        RunFailure::Disconnected => body["may_have_run"] = json!(true),
        _ => {}
    }
    body.to_string()
}

#[cfg(test)]
mod tests {
    use serde_json::Value;

    use super::*;

    #[test]
    fn a_background_call_can_only_be_injected() {
        assert_eq!(route(true, true, true, false), Route::NeedTakeover);
        assert_eq!(route(true, true, true, true), Route::Inject);
        assert_eq!(
            route(true, true, false, true),
            Route::Unavailable("no_channel")
        );
    }

    #[test]
    fn a_terminal_beside_the_shell_runs_there_unless_the_user_asks_for_the_terminal() {
        assert_eq!(route(false, true, true, false), Route::Exec);
        assert_eq!(route(false, true, true, true), Route::Inject);
    }

    #[test]
    fn a_terminal_with_no_channel_beside_the_shell_needs_takeover() {
        assert_eq!(route(false, false, true, false), Route::NeedTakeover);
        assert_eq!(route(false, false, true, true), Route::Inject);
        assert_eq!(
            route(false, false, false, true),
            Route::Unavailable("no_channel")
        );
    }

    #[test]
    fn a_disconnected_run_may_have_run_in_part() {
        let body: Value = serde_json::from_str(&failure_body(&RunFailure::Disconnected)).unwrap();
        assert_eq!(body["status"], "unknown");
        assert_eq!(body["reason"], "disconnected");
        assert_eq!(body["may_have_run"], true);
        assert_eq!(body["channel"], "terminal");
    }

    #[test]
    fn an_unsupported_shell_and_a_long_line_say_why() {
        let body: Value = serde_json::from_str(&failure_body(&RunFailure::UnsupportedShell(
            "/bin/zsh".into(),
        )))
        .unwrap();
        assert_eq!(body["reason"], "unsupported_shell");
        assert_eq!(body["shell"], "/bin/zsh");

        let body: Value =
            serde_json::from_str(&failure_body(&RunFailure::TooLong { limit: 1000 })).unwrap();
        assert_eq!(body["reason"], "too_long");
        assert_eq!(body["limit"], 1000);
    }

    #[test]
    fn a_busy_terminal_says_what_holds_it() {
        for (reason, name) in [
            (BusyReason::FullScreen, "full_screen"),
            (BusyReason::Transfer, "transfer"),
            (BusyReason::AnotherRun, "another_run"),
            (BusyReason::NotStarted, "not_started"),
            (BusyReason::NoAnswer, "no_answer"),
        ] {
            let body: Value =
                serde_json::from_str(&failure_body(&RunFailure::Busy(reason))).unwrap();
            assert_eq!(body["status"], "busy");
            assert_eq!(body["reason"], name);
        }
    }

    #[test]
    fn an_injected_answer_carries_its_channel_and_output() {
        let outcome = InjectOutcome {
            output: b"one\ntwo\n".to_vec(),
            exit_code: Some(0),
            context: Some(RunContext {
                user: "me".into(),
                host: "web".into(),
                cwd: "/srv".into(),
                shell: "bash".into(),
                interp: "bash".into(),
            }),
            ..InjectOutcome::default()
        };
        let body: Value = serde_json::from_str(&outcome.body()).unwrap();
        assert_eq!(body["status"], "ok");
        assert_eq!(body["channel"], "terminal");
        assert_eq!(body["background"], false);
        assert_eq!(body["output"], "one\ntwo\n");
        assert_eq!(body["exit_code"], 0);
        assert_eq!(body["context"]["host"], "web");
    }

    #[test]
    fn a_background_answer_has_no_exit_code() {
        let outcome = InjectOutcome {
            background: true,
            context: Some(RunContext {
                shell: "bash".into(),
                ..RunContext::default()
            }),
            ..InjectOutcome::default()
        };
        let body: Value = serde_json::from_str(&outcome.body()).unwrap();
        assert_eq!(body["background"], true);
        assert!(body.get("exit_code").is_none());
    }

    #[test]
    fn a_cancelled_run_says_who_stopped_it() {
        let outcome = InjectOutcome {
            exit_code: Some(130),
            cancelled_by: Some("user"),
            output: b"partial\n".to_vec(),
            ..InjectOutcome::default()
        };
        let body: Value = serde_json::from_str(&outcome.body()).unwrap();
        assert_eq!(body["exit_code"], 130);
        assert_eq!(body["cancelled_by"], "user");
    }
}
