//! Tool results: how one command's answer is written for the model, and how
//! the conversation reads it back for a card. Kept apart from the panel so
//! both shapes are tested without a window.

use gpui_kit::SharedString;

use crate::i18n::t;
use crate::terminal::ExecDetailResult;

use super::chat::{RunCommandArgs, ToolCall};
use super::output;

/// The most characters of one tool result a card draws. The result sent to
/// the model is capped separately.
const DISPLAY_CHARS: usize = 4000;

/// What the model is told when a call could not be read or resolved.
pub(super) fn tool_error(reason: &str) -> String {
    serde_json::json!({ "status": "error", "reason": reason }).to_string()
}

/// What the model is told when the policy refused the command, with the
/// reason a person would see on the card.
pub(super) fn denied_result(decision: &super::policy::Decision) -> String {
    serde_json::json!({
        "status": "denied",
        "risk": decision.risk.name(),
        "rule": decision.rule.name(),
        "reason": decision.rule.label(),
        "command": decision.masked_command,
    })
    .to_string()
}

/// What the model is told when this terminal cannot run a command beside
/// its shell.
pub(super) fn unsupported_result(reason: &str) -> String {
    serde_json::json!({ "status": "unsupported", "reason": reason }).to_string()
}

/// One command's answer as the model reads it. It is a JSON object, not
/// free text, so output is data the model may quote and never a line it
/// could take as its own instruction.
pub(super) fn tool_result_body(result: ExecDetailResult) -> (String, Option<i32>) {
    match result {
        Ok(detail) => {
            let (stdout, stdout_cut) = stream_text(&detail.stdout);
            let (stderr, stderr_cut) = stream_text(&detail.stderr);
            let exit_code = detail.exit_code.and_then(|code| i32::try_from(code).ok());
            let mut body = serde_json::json!({
                "status": "ok",
                "exit_code": exit_code,
                "stdout": stdout,
                "stderr": stderr,
                "truncated": detail.truncated || stdout_cut || stderr_cut,
            });
            if detail.timed_out {
                body["timed_out"] = serde_json::Value::Bool(true);
            }
            (body.to_string(), exit_code)
        }
        Err(reason) => (tool_error(&reason), None),
    }
}

/// One stream as the model may read it: a marker for binary data, otherwise
/// the cleaned text cut to the model's share.
fn stream_text(bytes: &[u8]) -> (String, bool) {
    if output::is_binary(bytes) {
        return (t!("assistant.tool.binary_output").to_string(), true);
    }
    let rendered = output::render(bytes, output::MODEL_CHARS);
    (rendered.text, rendered.truncated)
}

/// The summary line and the body of one tool result's card, read back from
/// the JSON the conversation keeps.
pub(super) fn tool_card_parts(text: &str) -> (String, String) {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(text) else {
        return (t!("assistant.tool.result").to_string(), text.to_string());
    };
    match value.get("status").and_then(|status| status.as_str()) {
        Some("ok") => {
            let terminal =
                value.get("channel").and_then(|channel| channel.as_str()) == Some("terminal");
            let background = value.get("background").and_then(|flag| flag.as_bool()) == Some(true);
            let mut summary = if background {
                t!("assistant.tool.background_started").to_string()
            } else {
                match value.get("exit_code").and_then(|code| code.as_i64()) {
                    Some(code) => t!("assistant.tool.exit", code = code).to_string(),
                    None => t!("assistant.tool.result").to_string(),
                }
            };
            if terminal {
                summary.push_str(" · ");
                summary.push_str(&t!("assistant.tool.channel_terminal"));
            }
            if let Some(cancelled) = value.get("cancelled_by").and_then(|by| by.as_str()) {
                summary.push_str(" · ");
                summary.push_str(&match cancelled {
                    "user" => t!("assistant.tool.cancelled_user"),
                    _ => t!("assistant.tool.cancelled_stop"),
                });
            }
            if value.get("timed_out").and_then(|flag| flag.as_bool()) == Some(true) {
                summary.push_str(" · ");
                summary.push_str(&t!("assistant.tool.timed_out"));
            }
            let mut body = String::new();
            for field in ["output", "stdout", "stderr"] {
                let Some(stream) = value.get(field).and_then(|stream| stream.as_str()) else {
                    continue;
                };
                if stream.trim().is_empty() {
                    continue;
                }
                if !body.is_empty() {
                    body.push('\n');
                }
                body.push_str(stream.trim_end_matches('\n'));
            }
            if body.is_empty() {
                body = t!("assistant.tool.no_output").to_string();
            }
            (summary, cut(body))
        }
        Some("denied") => (t!("assistant.tool.denied").to_string(), reason_text(&value)),
        Some("unsupported") => (
            t!("assistant.tool.unsupported").to_string(),
            reason_line(&value),
        ),
        Some("busy") => (t!("assistant.tool.busy").to_string(), reason_line(&value)),
        Some("unknown") => {
            let mut body = reason_line(&value);
            if value.get("may_have_run").and_then(|flag| flag.as_bool()) == Some(true) {
                body.push(' ');
                body.push_str(&t!("assistant.tool.may_have_run"));
            }
            (t!("assistant.tool.unknown").to_string(), body)
        }
        _ => (t!("assistant.tool.failed").to_string(), reason_text(&value)),
    }
}

/// The `reason` field of a tool result, as it was written.
fn reason_text(value: &serde_json::Value) -> String {
    value
        .get("reason")
        .and_then(|reason| reason.as_str())
        .unwrap_or_default()
        .to_string()
}

/// A reason code as a line for the card. A code the panel did not write (a
/// transport error) is shown as it came.
fn reason_line(value: &serde_json::Value) -> String {
    let reason = reason_text(value);
    match reason.as_str() {
        "unknown_tool" => t!("assistant.tool.reason.unknown_tool"),
        "bad_arguments" => t!("assistant.tool.reason.bad_arguments"),
        "unknown_terminal" => t!("assistant.tool.reason.unknown_terminal"),
        "terminal_closed" => t!("assistant.tool.reason.terminal_closed"),
        "command_changed" => t!("assistant.tool.reason.command_changed"),
        "shell_only" => t!("assistant.tool.reason.shell_only"),
        "closed" => t!("assistant.tool.reason.closed"),
        "not_running" => t!("assistant.tool.reason.not_running"),
        "not_connected" => t!("assistant.tool.reason.not_connected"),
        "unsupported_shell" => t!(
            "assistant.tool.reason.unsupported_shell",
            shell = value
                .get("shell")
                .and_then(|shell| shell.as_str())
                .unwrap_or_default()
        ),
        "too_long" => t!(
            "assistant.tool.reason.too_long",
            limit = value
                .get("limit")
                .and_then(|limit| limit.as_i64())
                .unwrap_or_default()
        ),
        "full_screen" => t!("assistant.tool.reason.full_screen"),
        "transfer" => t!("assistant.tool.reason.transfer"),
        "another_run" => t!("assistant.tool.reason.another_run"),
        "not_started" => t!("assistant.tool.reason.not_started"),
        "no_answer" => t!("assistant.tool.reason.no_answer"),
        "disconnected" => t!("assistant.tool.reason.disconnected"),
        "no_channel" => t!("assistant.tool.reason.no_channel"),
        other => SharedString::from(other.to_string()),
    }
    .to_string()
}

/// One call as a line in the conversation: the alias and command a person
/// can check, or the raw call when it is not `run_command`.
pub(super) fn call_line(call: &ToolCall) -> String {
    match serde_json::from_str::<RunCommandArgs>(&call.arguments) {
        Ok(args) => t!(
            "assistant.tool.calling",
            alias = args.terminal.as_str(),
            command = args.command.as_str()
        )
        .to_string(),
        Err(_) => t!(
            "assistant.tool.calling_raw",
            tool = call.name.as_str(),
            arguments = call.arguments.as_str()
        )
        .to_string(),
    }
}

/// The most of a card's body that is drawn; the whole result stays in the
/// conversation for the model.
fn cut(text: String) -> String {
    if text.chars().count() <= DISPLAY_CHARS {
        return text;
    }
    let mut cut: String = text.chars().take(DISPLAY_CHARS).collect();
    cut.push('…');
    cut
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminal::ExecDetail;

    fn detail(stdout: &str, stderr: &str, exit_code: Option<u32>) -> ExecDetail {
        ExecDetail {
            stdout: stdout.as_bytes().to_vec(),
            stderr: stderr.as_bytes().to_vec(),
            exit_code,
            truncated: false,
            timed_out: false,
        }
    }

    #[test]
    fn a_result_carries_the_code_and_both_streams_as_data() {
        let (body, code) = tool_result_body(Ok(detail("out\n", "err\n", Some(2))));
        assert_eq!(code, Some(2));
        let value: serde_json::Value = serde_json::from_str(&body).expect("json");
        assert_eq!(value["status"], "ok");
        assert_eq!(value["exit_code"], 2);
        assert_eq!(value["stdout"], "out\n");
        assert_eq!(value["stderr"], "err\n");
        assert_eq!(value["truncated"], false);
    }

    #[test]
    fn binary_output_is_marked_and_a_timeout_is_kept() {
        let mut output = detail("", "", None);
        output.stdout = vec![0, 1, 2];
        output.timed_out = true;
        let (body, code) = tool_result_body(Ok(output));
        assert_eq!(code, None, "a timed-out command has no code");
        let value: serde_json::Value = serde_json::from_str(&body).expect("json");
        assert_eq!(value["timed_out"], true);
        assert_eq!(value["truncated"], true);
        assert_eq!(
            value["stdout"].as_str().expect("text"),
            t!("assistant.tool.binary_output").as_ref()
        );
    }

    #[test]
    fn a_transport_error_is_a_result_not_a_panic() {
        let (body, code) = tool_result_body(Err("gone".into()));
        assert_eq!(code, None);
        let value: serde_json::Value = serde_json::from_str(&body).expect("json");
        assert_eq!(value["status"], "error");
        assert_eq!(value["reason"], "gone");
    }

    #[test]
    fn a_card_reads_an_ok_result_back() {
        let (summary, body) = tool_card_parts(
            r#"{"status":"ok","exit_code":0,"stdout":"a\nb\n","stderr":"","truncated":false}"#,
        );
        assert_eq!(summary, t!("assistant.tool.exit", code = 0).to_string());
        assert_eq!(body, "a\nb");
    }

    #[test]
    fn a_card_says_why_a_command_was_refused_or_could_not_run() {
        let (summary, body) = tool_card_parts(r#"{"status":"denied","reason":"touches /etc"}"#);
        assert_eq!(summary, t!("assistant.tool.denied").to_string());
        assert_eq!(body, "touches /etc");

        let (_, body) = tool_card_parts(r#"{"status":"unsupported","reason":"shell_only"}"#);
        assert_eq!(body, t!("assistant.tool.reason.shell_only").to_string());
    }

    #[test]
    fn a_card_reads_an_injected_answer_with_its_terminal_channel() {
        let (summary, body) = tool_card_parts(
            r#"{"status":"ok","channel":"terminal","background":false,"exit_code":3,"output":"one\ntwo\n"}"#,
        );
        assert!(summary.contains(t!("assistant.tool.exit", code = 3).as_ref()));
        assert!(summary.contains(t!("assistant.tool.channel_terminal").as_ref()));
        assert_eq!(body, "one\ntwo");
    }

    #[test]
    fn a_card_marks_a_background_start_and_a_cancelled_run() {
        let (summary, _) = tool_card_parts(
            r#"{"status":"ok","channel":"terminal","background":true,"output":""}"#,
        );
        assert!(summary.contains(t!("assistant.tool.background_started").as_ref()));
        assert!(summary.contains(t!("assistant.tool.channel_terminal").as_ref()));

        let (summary, _) = tool_card_parts(
            r#"{"status":"ok","channel":"terminal","exit_code":130,"cancelled_by":"user"}"#,
        );
        assert!(summary.contains(t!("assistant.tool.cancelled_user").as_ref()));
    }

    #[test]
    fn a_card_of_an_unknown_outcome_says_it_may_have_run() {
        let (summary, body) =
            tool_card_parts(r#"{"status":"unknown","reason":"disconnected","may_have_run":true}"#);
        assert_eq!(summary, t!("assistant.tool.unknown").to_string());
        assert!(body.contains(t!("assistant.tool.reason.disconnected").as_ref()));
        assert!(body.contains(t!("assistant.tool.may_have_run").as_ref()));
    }

    #[test]
    fn a_card_of_an_unsupported_shell_names_it() {
        let (summary, body) = tool_card_parts(
            r#"{"status":"unsupported","reason":"unsupported_shell","shell":"/bin/zsh"}"#,
        );
        assert_eq!(summary, t!("assistant.tool.unsupported").to_string());
        assert!(body.contains("/bin/zsh"));
    }

    #[test]
    fn a_card_of_a_busy_terminal_says_what_holds_it() {
        let (summary, body) = tool_card_parts(r#"{"status":"busy","reason":"full_screen"}"#);
        assert_eq!(summary, t!("assistant.tool.busy").to_string());
        assert_eq!(body, t!("assistant.tool.reason.full_screen").to_string());
    }

    #[test]
    fn a_card_that_is_not_json_shows_what_it_has() {
        let (summary, body) = tool_card_parts("not json");
        assert_eq!(summary, t!("assistant.tool.result").to_string());
        assert_eq!(body, "not json");
    }

    #[test]
    fn a_long_body_is_cut_for_the_card_only() {
        let long = "a".repeat(DISPLAY_CHARS + 10);
        let payload = serde_json::json!({
            "status": "ok",
            "exit_code": 0,
            "stdout": long,
        })
        .to_string();
        let (_, body) = tool_card_parts(&payload);
        assert_eq!(body.chars().count(), DISPLAY_CHARS + 1);
        assert!(body.ends_with('…'));
    }

    #[test]
    fn a_call_shows_the_alias_and_the_command() {
        let call = ToolCall {
            id: "1".into(),
            name: "run_command".into(),
            arguments: r#"{"terminal":"web","command":"df -h"}"#.into(),
        };
        let line = call_line(&call);
        assert!(line.contains("web"), "{line}");
        assert!(line.contains("df -h"), "{line}");
    }

    #[test]
    fn a_call_that_is_not_run_command_shows_what_it_was() {
        let call = ToolCall {
            id: "1".into(),
            name: "other".into(),
            arguments: "{".into(),
        };
        let line = call_line(&call);
        assert!(line.contains("other"), "{line}");
    }
}
