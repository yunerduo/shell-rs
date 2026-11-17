//! 内置 AI · 试用：服务端持有唯一的 OpenAI 兼容上游，已登录的用户以自己
//! 的会话令牌调用。这里只有可用性探测、交给请求层的选择，以及把服务端的
//! 错误码翻成人话。上游地址与 Key 从不经过客户端。

use std::sync::mpsc::{self, Receiver};
use std::time::Duration;

use serde::Deserialize;
use serde_json::Value;
use zeroize::Zeroizing;

use crate::account::API_BASE;
use crate::i18n::t;
use crate::secrets::{KeychainSecretStore, SecretRef};

use super::protocol::{self, ChatError};
use super::settings::{DEFAULT_TOOL_ROUNDS, ModelSelection, ProviderProtocol};

/// The user-facing paths, relative to the service.
const STATUS_PATH: &str = "/api/v1/ai/status";
const CHAT_PATH: &str = "/api/v1/ai";

/// `GET /api/v1/ai/status` 的响应。`available=false` 也是 200。
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Status {
    pub available: bool,
    /// `disabled` / `user_disabled` / `not_configured` / `daily_limit_reached`。
    pub reason: Option<String>,
    pub model: String,
    pub max_context_tokens: u32,
    pub max_output_tokens: u32,
    pub daily_request_limit: u32,
    pub daily_token_limit: u32,
    pub requests_used_today: u32,
    pub requests_remaining_today: u32,
    pub tokens_used_today: u64,
    pub tokens_remaining_today: Option<u64>,
}

/// What the model menu does with the trial right now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Offer {
    /// 不出现：未登录、离线、被关闭或没配置。
    Hidden,
    /// 出现但禁用：今天的次数已经用完。
    Exhausted,
    /// 出现且可选。
    Ready,
}

/// 入口是否出现，只看登录态与状态本身。探测失败是 `None`，也就是不出现。
pub fn offer(signed_in: bool, status: Option<&Status>) -> Offer {
    if !signed_in {
        return Offer::Hidden;
    }
    match status {
        Some(status) if status.available && !status.model.trim().is_empty() => Offer::Ready,
        Some(status) if status.reason.as_deref() == Some("daily_limit_reached") => Offer::Exhausted,
        _ => Offer::Hidden,
    }
}

/// 今日剩余次数与已用 token。
pub fn quota(status: &Status) -> (u32, u64) {
    (status.requests_remaining_today, status.tokens_used_today)
}

/// 试用请求的全部参数：OpenAI 形状，地址是服务端的代理路径，模型与上限
/// 都由服务端状态给出。Key 一栏留空，令牌由请求层单独读。
pub fn selection(status: &Status) -> ModelSelection {
    ModelSelection {
        protocol: ProviderProtocol::OpenAi,
        base_url: format!("{API_BASE}{CHAT_PATH}"),
        full_url: false,
        model: status.model.clone(),
        keychain_id: String::new(),
        max_tokens: status.max_output_tokens.max(1),
        context_window: status.max_context_tokens.max(1_024),
        tool_rounds: DEFAULT_TOOL_ROUNDS,
        temperature: None,
        top_p: None,
        top_k: None,
        thinking_param: String::new(),
        thinking_effort: String::new(),
        stop: Vec::new(),
        extra: String::new(),
        instruction: String::new(),
        supports_images: true,
        headers: Vec::new(),
        trial: true,
    }
}

/// 令牌就在账号自己写入的钥匙串条目里，不经过 `AccountStore`。
pub fn token() -> Result<Zeroizing<String>, ChatError> {
    KeychainSecretStore::read_account(&SecretRef::AccountToken.account())
        .map_err(|error| ChatError::Keychain(error.to_string()))?
        .ok_or_else(|| ChatError::Trial(t!("assistant.trial.error.login").to_string()))
}

/// One probe, on its own thread. `None` covers every "not for you now": no
/// token, offline, a rejected token, a body that is not the contract.
pub fn probe() -> Receiver<Option<Status>> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(fetch());
    });
    rx
}

fn fetch() -> Option<Status> {
    let token = KeychainSecretStore::read_account(&SecretRef::AccountToken.account())
        .ok()
        .flatten()?;
    let client = reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(10))
        .build()
        .ok()?;
    let response = client
        .get(format!("{API_BASE}{STATUS_PATH}"))
        .bearer_auth(token.as_str())
        .send()
        .ok()?;
    if !response.status().is_success() {
        return None;
    }
    response.json::<Status>().ok()
}

/// 服务端对试用的说明，按错误码翻成固定的人话。没有认出的码时用服务端
/// 自己给的 `message`（契约保证可直接展示），最后才退回通用状态错误。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Reason {
    Upstream,
    Limit,
    Busy,
    Login,
    Account,
    Disabled,
}

fn reason_for(code: &str) -> Option<Reason> {
    match code {
        "upstream_unreachable" | "upstream_timeout" | "upstream_error" => Some(Reason::Upstream),
        "daily_limit_reached" | "tokens_exhausted" => Some(Reason::Limit),
        "too_concurrent" => Some(Reason::Busy),
        "unauthorized" => Some(Reason::Login),
        "account_disabled" => Some(Reason::Account),
        "ai_trial_disabled" | "ai_disabled" => Some(Reason::Disabled),
        _ => None,
    }
}

fn describe(reason: Reason) -> ChatError {
    ChatError::Trial(
        match reason {
            Reason::Upstream => t!("assistant.trial.error.upstream"),
            Reason::Limit => t!("assistant.trial.error.limit"),
            Reason::Busy => t!("assistant.trial.error.busy"),
            Reason::Login => t!("assistant.trial.error.login"),
            Reason::Account => t!("assistant.trial.error.account"),
            Reason::Disabled => t!("assistant.trial.error.disabled"),
        }
        .to_string(),
    )
}

/// 非 2xx 的错误体是 `{"error":{"message","type","code"}}`。
pub fn status_error(status: u16, body: &str) -> ChatError {
    let value: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    let code = value
        .pointer("/error/code")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if let Some(reason) = reason_for(code) {
        return describe(reason);
    }
    match protocol::server_message(&value) {
        Some(message) => ChatError::Trial(message),
        None => protocol::status_error(status, body),
    }
}

#[cfg(test)]
mod tests {
    use super::{Offer, Status, offer, quota, selection, status_error};
    use crate::assistant::protocol::ChatError;
    use crate::assistant::settings::ProviderProtocol;

    const SAMPLE: &str = r#"{
        "available": true,
        "reason": null,
        "model": "trial-model",
        "maxContextTokens": 32000,
        "maxOutputTokens": 4096,
        "dailyRequestLimit": 200,
        "dailyTokenLimit": 0,
        "requestsUsedToday": 3,
        "requestsRemainingToday": 197,
        "tokensUsedToday": 5120,
        "tokensRemainingToday": null
    }"#;

    fn status(available: bool, reason: Option<&str>) -> Status {
        Status {
            available,
            reason: reason.map(str::to_string),
            model: "trial-model".into(),
            ..Default::default()
        }
    }

    #[test]
    fn the_status_is_read_from_the_contract() {
        let status: Status = serde_json::from_str(SAMPLE).expect("the sample status");
        assert!(status.available);
        assert_eq!(status.model, "trial-model");
        assert_eq!(status.max_context_tokens, 32_000);
        assert_eq!(status.max_output_tokens, 4_096);
        assert_eq!(status.requests_remaining_today, 197);
        assert_eq!(status.tokens_used_today, 5_120);
        assert_eq!(status.tokens_remaining_today, None);
        assert_eq!(quota(&status), (197, 5_120));
    }

    #[test]
    fn the_trial_is_offered_only_when_it_can_be_used() {
        // 未登录：不管状态如何都不出现。
        assert_eq!(offer(false, Some(&status(true, None))), Offer::Hidden);
        // 离线或探测失败。
        assert_eq!(offer(true, None), Offer::Hidden);
        // 被单独关闭、全局关闭、没配置。
        assert_eq!(
            offer(true, Some(&status(false, Some("user_disabled")))),
            Offer::Hidden
        );
        assert_eq!(
            offer(true, Some(&status(false, Some("disabled")))),
            Offer::Hidden
        );
        assert_eq!(
            offer(true, Some(&status(false, Some("not_configured")))),
            Offer::Hidden
        );
        // 额度用尽：入口保留但只能看。
        assert_eq!(
            offer(true, Some(&status(false, Some("daily_limit_reached")))),
            Offer::Exhausted
        );
        // 可用。
        assert_eq!(offer(true, Some(&status(true, None))), Offer::Ready);
        // 可用但没有模型 id 不算可用。
        let mut empty = status(true, None);
        empty.model.clear();
        assert_eq!(offer(true, Some(&empty)), Offer::Hidden);
    }

    #[test]
    fn the_selection_is_the_openai_route_to_the_trial() {
        let status: Status = serde_json::from_str(SAMPLE).expect("the sample status");
        let selection = selection(&status);
        assert_eq!(selection.protocol, ProviderProtocol::OpenAi);
        assert_eq!(selection.base_url, "https://shellas.agree.host/api/v1/ai");
        assert!(!selection.full_url);
        assert_eq!(selection.model, "trial-model");
        assert!(selection.keychain_id.is_empty());
        assert_eq!(selection.max_tokens, 4_096);
        assert_eq!(selection.context_window, 32_000);
        assert!(selection.trial);
        assert!(selection.headers.is_empty());
    }

    #[test]
    fn the_documented_codes_become_sentences() {
        for code in [
            "upstream_unreachable",
            "upstream_timeout",
            "upstream_error",
            "daily_limit_reached",
            "tokens_exhausted",
            "too_concurrent",
            "unauthorized",
            "account_disabled",
            "ai_trial_disabled",
            "ai_disabled",
        ] {
            let body = format!(r#"{{"error":{{"message":"x","type":"t","code":"{code}"}}}}"#);
            assert!(
                matches!(status_error(429, &body), ChatError::Trial(_)),
                "{code} should be a trial sentence"
            );
        }
        // 认不出的码用服务端的 message。
        let body = r#"{"error":{"message":"上游说不行","type":"x","code":"unknown"}}"#;
        assert_eq!(
            status_error(400, body),
            ChatError::Trial("上游说不行".into())
        );
        // 连 message 都没有才退回通用错误。
        assert_eq!(status_error(502, "<html>"), ChatError::Http(502));
    }
}
