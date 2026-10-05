//! GitHub REST client: pull request metadata and patches, file/directory
//! contents at the PR head, and review submission with inline comments.

use crate::types::{Config, Finding, FileDiff, Pr};
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use reqwest::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE, USER_AGENT};
use reqwest::Method;
use serde_json::{json, Value};

const API: &str = "https://api.github.com";
/// Patches above this size are usually generated blobs, not reviewable code.
const MAX_PATCH_LEN: usize = 20_000;
const MAX_FILE_CHARS: usize = 30_000;

/// Lockfiles, bundles and vendored/build output add noise, not review value.
fn skip(path: &str) -> bool {
    path.ends_with(".lock")
        || path == "package-lock.json"
        || path.ends_with(".min.js")
        || path.starts_with("dist/")
        || path.contains("/dist/")
        || path.starts_with("vendor/")
        || path.contains("/vendor/")
}

fn req(
    cfg: &Config,
    method: Method,
    url: &str,
) -> Result<reqwest::blocking::RequestBuilder, String> {
    let client = reqwest::blocking::Client::builder()
        .build()
        .map_err(|e| format!("client: {e}"))?;
    Ok(client
        .request(method, url)
        .header(AUTHORIZATION, format!("Bearer {}", cfg.github_token))
        .header(USER_AGENT, "prr")
        .header(ACCEPT, "application/vnd.github+json"))
}

fn retryable(s: reqwest::StatusCode) -> bool {
    s.as_u16() == 429 || s.is_server_error()
}

/// Send a prepared request, retrying transient failures up to 3 tries total.
/// Backoff between tries doubles: 1s, then 2s. Returns the raw response body.
fn send(
    method: &str,
    url: &str,
    rb: Result<reqwest::blocking::RequestBuilder, String>,
) -> Result<String, String> {
    let mut builder = match rb {
        Ok(rb) => rb,
        Err(e) => return Err(format!("{method} {url} failed: {e}")),
    };
    let mut last = "retries exhausted".to_string();
    let mut wait = 1u64;
    for attempt in 1..=3 {
        // Every body used here is a plain string or absent, so replaying is safe.
        let next = builder.try_clone();
        match builder.send() {
            Ok(resp) => {
                let status = resp.status();
                let body = resp.text().map_err(|e| format!("{method} {url} failed: {e}"))?;
                if status.is_success() {
                    return Ok(body);
                }
                if attempt == 3 || !retryable(status) {
                    return Err(format!("{method} {url} failed: {status}: {body}"));
                }
            }
            Err(e) => {
                last = e.to_string();
                if attempt == 3 {
                    return Err(format!("{method} {url} failed: {last}"));
                }
            }
        }
        match next {
            Some(rb) => builder = rb,
            None => return Err(format!("{method} {url} failed: {last}")),
        }
        // Only reached when another try follows, so 4s is never reached here.
        std::thread::sleep(std::time::Duration::from_secs(wait));
        wait *= 2;
    }
    Err(format!("{method} {url} failed: {last}"))
}

pub fn get_pr(cfg: &Config, owner: &str, repo: &str, number: u64) -> Result<Pr, String> {
    let url = format!("{API}/repos/{owner}/{repo}/pulls/{number}");
    let raw = send("GET", &url, req(cfg, Method::GET, &url))?;
    let meta: Value = serde_json::from_str(&raw).map_err(|e| format!("{url}: bad json: {e}"))?;
    let title = meta["title"].as_str().unwrap_or_default().to_string();
    let body = match meta.get("body") {
        Some(Value::Null) | None => String::new(),
        Some(v) => v.as_str().unwrap_or_default().to_string(),
    };
    let head_sha = meta["head"]["sha"]
        .as_str()
        .ok_or_else(|| format!("pull {number}: missing head.sha"))?
        .to_string();

    let files_url = format!("{API}/repos/{owner}/{repo}/pulls/{number}/files?per_page=100");
    let raw = send("GET", &files_url, req(cfg, Method::GET, &files_url))?;
    let list: Value =
        serde_json::from_str(&raw).map_err(|e| format!("{files_url}: bad json: {e}"))?;
    let entries = list
        .as_array()
        .ok_or_else(|| format!("{files_url}: expected an array"))?;

    let mut files = Vec::new();
    for entry in entries {
        let Some(path) = entry["filename"].as_str() else {
            continue;
        };
        // No patch means a binary or oversized file: nothing to review.
        let Some(patch) = entry.get("patch").and_then(Value::as_str) else {
            continue;
        };
        if skip(path) || patch.len() > MAX_PATCH_LEN {
            continue;
        }
        files.push(FileDiff {
            path: path.to_string(),
            patch: patch.to_string(),
        });
    }

    Ok(Pr {
        owner: owner.to_string(),
        repo: repo.to_string(),
        number,
        title,
        body,
        head_sha,
        files,
    })
}

pub fn get_file(cfg: &Config, pr: &Pr, path: &str) -> Result<String, String> {
    let url = format!(
        "{API}/repos/{}/{}/contents/{path}?ref={}",
        pr.owner, pr.repo, pr.head_sha
    );
    let raw = send("GET", &url, req(cfg, Method::GET, &url))?;
    let doc: Value =
        serde_json::from_str(&raw).map_err(|e| format!("get_file {url}: bad json: {e}"))?;
    let encoded = doc
        .get("content")
        .and_then(Value::as_str)
        .ok_or_else(|| format!("get_file {path}: response has no content"))?;
    let bytes = BASE64
        .decode(encoded.replace(['\n', '\r'], ""))
        .map_err(|e| format!("get_file {url}: {e}"))?;
    let text = String::from_utf8(bytes).map_err(|e| format!("get_file {url}: {e}"))?;
    Ok(text.chars().take(MAX_FILE_CHARS).collect())
}

pub fn list_dir(cfg: &Config, pr: &Pr, dir: &str) -> Result<Vec<String>, String> {
    let url = format!(
        "{API}/repos/{}/{}/contents/{dir}?ref={}",
        pr.owner, pr.repo, pr.head_sha
    );
    let raw = send("GET", &url, req(cfg, Method::GET, &url))?;
    let doc: Value =
        serde_json::from_str(&raw).map_err(|e| format!("list_dir {url}: bad json: {e}"))?;
    let entries = doc
        .as_array()
        .ok_or_else(|| format!("list_dir {url}: not a directory listing"))?;
    Ok(entries
        .iter()
        .map(|e| {
            format!(
                "{} {}",
                e["type"].as_str().unwrap_or("?"),
                e["path"].as_str().unwrap_or("")
            )
        })
        .collect())
}

pub fn post_review(cfg: &Config, pr: &Pr, findings: &[Finding]) -> Result<String, String> {
    let comments: Vec<Value> = findings
        .iter()
        .map(|f| {
            json!({
                "path": f.path.as_str(),
                "line": f.line,
                "side": "RIGHT",
                "body": format!("[{}] {}", f.severity, f.body),
            })
        })
        .collect();
    let body = json!({
        "commit_id": pr.head_sha,
        "event": "COMMENT",
        "body": "Automated review by prr",
        "comments": comments,
    });

    if findings.is_empty() {
        return Ok("no accepted findings to post".into());
    }
    // Analogue of `dry_run` for the other direction: refuse before any HTTP so
    // `--no-post` cannot be bypassed by a future caller reaching post_review
    // directly.
    if cfg.no_post {
        return Ok("posting is disabled (--no-post); nothing was sent".into());
    }
    if cfg.dry_run {
        return serde_json::to_string_pretty(&body).map_err(|e| format!("post_review: {e}"));
    }

    let url = format!(
        "{API}/repos/{}/{}/pulls/{}/reviews",
        pr.owner, pr.repo, pr.number
    );
    let rb = req(cfg, Method::POST, &url)
        .map(|rb| rb.header(CONTENT_TYPE, "application/json").json(&body));
    let raw = send("POST", &url, rb)?;
    let doc: Value =
        serde_json::from_str(&raw).map_err(|e| format!("post_review {url}: bad json: {e}"))?;
    doc.get("html_url")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| "post_review: response has no html_url".to_string())
}
#[cfg(test)]
mod tests {
    use super::{post_review, skip};
    use crate::types::{Config, Finding, Pr};

    fn cfg(dry_run: bool) -> Config {
        Config {
            openai_key: String::new(),
            openai_base_url: String::new(),
            openai_model: String::new(),
            github_token: String::new(),
            dry_run,
            no_post: false,
        }
    }

    fn no_post_cfg() -> Config {
        Config {
            no_post: true,
            ..cfg(false)
        }
    }

    fn pr() -> Pr {
        Pr {
            owner: "o".into(),
            repo: "r".into(),
            number: 7,
            title: "t".into(),
            body: String::new(),
            head_sha: "abc123".into(),
            files: Vec::new(),
        }
    }

    fn finding() -> Finding {
        Finding {
            path: "src/a.rs".into(),
            line: 12,
            severity: "high".into(),
            body: "boom".into(),
            accepted: true,
        }
    }

    /// --dry-run must build the exact review payload and never touch the network.
    #[test]
    fn dry_run_returns_the_review_payload() {
        let out = post_review(&cfg(true), &pr(), &[finding()]).expect("dry run must not fail");
        let v: serde_json::Value = serde_json::from_str(&out).expect("payload must be valid json");
        assert_eq!(v["commit_id"], "abc123");
        assert_eq!(v["event"], "COMMENT");
        assert_eq!(v["body"], "Automated review by prr");
        assert_eq!(v["comments"][0]["path"], "src/a.rs");
        assert_eq!(v["comments"][0]["line"], 12);
        assert_eq!(v["comments"][0]["side"], "RIGHT");
        assert_eq!(v["comments"][0]["body"], "[high] boom");
    }

    #[test]
    fn empty_findings_post_nothing() {
        assert_eq!(
            post_review(&cfg(true), &pr(), &[]).unwrap(),
            "no accepted findings to post"
        );
    }

    /// `--no-post` must refuse before it builds a request, and must not fall
    /// through to `--dry-run`'s payload dump: the whole point is that nothing
    /// resembling a post happens.
    #[test]
    fn no_post_refuses_and_never_reaches_the_network() {
        let out = post_review(&no_post_cfg(), &pr(), &[finding()]).unwrap();
        assert_eq!(out, "posting is disabled (--no-post); nothing was sent");
        assert!(!out.contains("commit_id"));
        // No matter how the two flags combine, nothing is sent.
        let mut both = no_post_cfg();
        both.dry_run = true;
        assert_eq!(
            post_review(&both, &pr(), &[finding()]).unwrap(),
            "posting is disabled (--no-post); nothing was sent"
        );
    }

    #[test]
    fn noisy_paths_are_filtered() {
        for p in [
            "Cargo.lock",
            "package-lock.json",
            "yarn.lock",
            "static/app.min.js",
            "dist/bundle.js",
            "web/dist/bundle.js",
            "vendor/x/y.go",
            "api/vendor/x.go",
        ] {
            assert!(skip(p), "{p} should be skipped");
        }
        for p in ["src/main.rs", "src/lockfile.rs", "src/distribution.rs", "README.md"] {
            assert!(!skip(p), "{p} should be kept");
        }
    }
}