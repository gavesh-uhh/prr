mod agent;
mod github;
mod llm;
mod tui;
mod types;

use std::env;
use std::sync::mpsc;

use types::{Config, Event, Finding};

const USAGE: &str = "usage: prr owner/repo#123 [--dry-run] [--no-post] [--json]";

fn main() {
    if let Err(e) = start() {
        eprintln!("prr: {e}");
        std::process::exit(1);
    }
}

fn env_var(key: &str) -> Result<String, String> {
    match env::var(key) {
        Ok(v) if !v.trim().is_empty() => Ok(v),
        _ => Err(format!("environment variable {key} is not set")),
    }
}

fn start() -> Result<(), String> {
    let mut target = String::new();
    let mut dry_run = false;
    let mut no_post = false;
    let mut json = false;
    for arg in env::args().skip(1) {
        match arg.as_str() {
            "--dry-run" => dry_run = true,
            "--no-post" => no_post = true,
            "--json" => json = true,
            "-h" | "--help" => {
                println!("{USAGE}");
                return Ok(());
            }
            _ if arg.starts_with('-') => return Err(format!("unknown flag {arg}\n{USAGE}")),
            _ if target.is_empty() => target = arg,
            _ => return Err(format!("unexpected argument {arg}\n{USAGE}")),
        }
    }
    if target.is_empty() {
        return Err(format!("missing pull request reference\n{USAGE}"));
    }
    let (slug, number) = target
        .split_once('#')
        .ok_or_else(|| format!("expected owner/repo#number, got {target}\n{USAGE}"))?;
    let (owner, repo) = slug
        .split_once('/')
        .ok_or_else(|| format!("expected owner/repo#number, got {target}\n{USAGE}"))?;
    let number: u64 = number
        .parse()
        .map_err(|_| format!("expected a pull request number, got {target}\n{USAGE}"))?;

    let cfg = Config {
        openai_key: env_var("OPENAI_API_KEY")?,
        openai_base_url: env::var("OPENAI_BASE_URL")
            .unwrap_or_else(|_| "https://api.openai.com/v1".into()),
        openai_model: env::var("OPENAI_MODEL").unwrap_or_else(|_| "gpt-4o".into()),
        github_token: env_var("GITHUB_TOKEN")?,
        dry_run,
        no_post,
    };

    // Announce the mode before any network work, so it is visible even if
// fetching the pull request fails.
if no_post {
        eprintln!("prr: --no-post, findings are read only and will not be posted");
    }

    let pr = github::get_pr(&cfg, owner, repo, number)?;
    let head = pr.head_sha.get(..8).unwrap_or(&pr.head_sha);
    eprintln!("prr: {} - {} files, head {head}", pr.title, pr.files.len());

    if json {
        let (tx, rx) = mpsc::channel();
        agent::run(cfg, pr, tx);
        let mut findings: Vec<Finding> = Vec::new();
        while let Ok(event) = rx.try_recv() {
            match event {
                Event::Log(s) => eprintln!("{s}"),
                Event::Finding(f) => findings.push(f),
                Event::Done(s) => eprintln!("done: {s}"),
                Event::Error(e) => eprintln!("error: {e}"),
            }
        }
        let json = serde_json::to_string_pretty(&findings).map_err(|e| format!("serialize: {e}"))?;
        println!("{json}");
        return Ok(());
    }

    tui::run(cfg, pr)
}