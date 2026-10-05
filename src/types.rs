use serde::{Deserialize, Serialize};

#[derive(Clone)]
pub struct Config {
    pub openai_key: String,
    pub openai_base_url: String,
    pub openai_model: String,
    pub github_token: String,
    /// `--dry-run`: posting prints the payload instead of sending it.
    pub dry_run: bool,
    /// `--no-post`: analyse only, posting is disabled outright.
    pub no_post: bool,
}

#[derive(Clone)]
pub struct FileDiff {
    pub path: String,
    pub patch: String,
}

#[derive(Clone)]
pub struct Pr {
    pub owner: String,
    pub repo: String,
    pub number: u64,
    pub title: String,
    pub body: String,
    pub head_sha: String,
    pub files: Vec<FileDiff>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Finding {
    pub path: String,
    pub line: u32,
    pub severity: String,
    pub body: String,
    #[serde(skip)]
    pub accepted: bool,
}

pub enum Event {
    Log(String),
    Finding(Finding),
    Done(String),
    Error(String),
}