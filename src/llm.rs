use std::thread::sleep;
use std::time::Duration;

use reqwest::blocking::Client;
use reqwest::StatusCode;
use serde_json::{json, Value};

use crate::types::Config;

/// Total attempts (initial try plus retries) for a single chat completion.
const MAX_ATTEMPTS: u32 = 3;

/// Rate limits (429) and server faults (5xx) are transient; everything else is not.
fn is_retryable(status: StatusCode) -> bool {
    status.as_u16() == 429 || status.is_server_error()
}

/// Backoff after attempt `n`: 1s, then 2s, then 4s.
fn backoff_secs(n: u32) -> u64 {
    1u64 << (n - 1)
}

fn snippet(text: &str) -> String {
    text.chars().take(500).collect()
}

/// POST an OpenAI chat completion and return `choices[0].message`.
pub fn chat(
    cfg: &Config,
    messages: &[Value],
    tools: &Value,
) -> Result<Value, String> {
    let url = format!(
        "{}/chat/completions",
        cfg.openai_base_url.trim_end_matches('/')
    );
    let client = Client::builder().build().map_err(|e| format!("llm client: {e}"))?;
    let payload = json!({
        "model": cfg.openai_model,
        "messages": messages,
        "tools": tools,
        "tool_choice": "auto",
    });

    let mut attempt: u32 = 0;
    loop {
        attempt += 1;
        let sent = client
            .post(&url)
            .bearer_auth(&cfg.openai_key)
            .json(&payload)
            .send();

        let resp = match sent {
            Ok(resp) => resp,
            Err(e) => {
                if attempt >= MAX_ATTEMPTS {
                    return Err(format!("POST {url} failed: {e}"));
                }
                sleep(Duration::from_secs(backoff_secs(attempt)));
                continue;
            }
        };

        let status = resp.status();
        if status.is_success() {
            let text = resp.text().map_err(|e| format!("POST {url}: body: {e}"))?;
            let parsed: Value = serde_json::from_str(&text)
                .map_err(|e| format!("POST {url}: invalid json: {e}"))?;
            return match parsed
                .get("choices")
                .and_then(|c| c.get(0))
                .and_then(|c| c.get("message"))
            {
                Some(message) if message.is_object() => Ok(message.clone()),
                _ => Err(format!(
                    "POST {url}: response has no choices[0].message: {}",
                    snippet(&text)
                )),
            };
        }

        if attempt >= MAX_ATTEMPTS || !is_retryable(status) {
            let body = resp.text().unwrap_or_default();
            return Err(format!("POST {url} failed: {status}: {body}"));
        }
        sleep(Duration::from_secs(backoff_secs(attempt)));
    }
}