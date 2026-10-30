//! Configuration from environment (ROLLUP_-prefixed) and a local .env file.
//!
//! No external dotenv crate: reads .env manually so the dependency surface
//! stays small.

use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct Settings {
    pub sync_mode: String,
    pub db_path: PathBuf,
    pub jira_base_url: String,
    pub jira_email: String,
    pub jira_token: String,
    pub jira_boards: Vec<String>,
    pub github_token: String,
    pub github_org: String,
    pub github_repos: Vec<String>,
    /// Local tracker base URL. Read only. Env: ROLLUP_TRACKER_URL.
    /// Defaults to http://127.0.0.1:4321 when unset.
    pub tracker_url: String,
    pub t_high: f64,
    pub t_low: f64,
    /// `review` (default) or `auto`. Env: ROLLUP_AI_PUBLISH.
    /// Any other value is `review`.
    pub ai_publish: String,
}

/// Default local tracker base URL when `ROLLUP_TRACKER_URL` is unset: http://127.0.0.1:4321.
pub const DEFAULT_TRACKER_URL: &str = "http://127.0.0.1:4321";

fn read_dotenv(path: &Path) -> Vec<(String, String)> {
    let mut pairs = Vec::new();
    if let Ok(text) = std::fs::read_to_string(path) {
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Some((k, v)) = line.split_once('=') {
                pairs.push((k.trim().to_string(), v.trim().to_string()));
            }
        }
    }
    pairs
}

impl Settings {
    pub fn load() -> Self {
        let file_vars = read_dotenv(Path::new(".env"));
        let get = |key: &str| -> Option<String> {
            std::env::var(format!("ROLLUP_{key}"))
                .ok()
                .filter(|v| !v.is_empty())
                .or_else(|| {
                    file_vars
                        .iter()
                        .find(|(k, _)| k == &format!("ROLLUP_{key}"))
                        .map(|(_, v)| v.clone())
                })
        };

        let split = |v: Option<String>| -> Vec<String> {
            v.unwrap_or_default()
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect()
        };

        Self {
            sync_mode: get("SYNC_MODE").unwrap_or_else(|| "fixture".into()),
            db_path: PathBuf::from(get("DB_PATH").unwrap_or_else(|| "rollup.db".into())),
            jira_base_url: get("JIRA_BASE_URL").unwrap_or_default(),
            jira_email: get("JIRA_EMAIL").unwrap_or_default(),
            jira_token: get("JIRA_TOKEN").unwrap_or_default(),
            jira_boards: split(get("JIRA_BOARDS")),
            github_token: get("GITHUB_TOKEN").unwrap_or_default(),
            github_org: get("GITHUB_ORG").unwrap_or_default(),
            github_repos: split(get("GITHUB_REPOS")),
            tracker_url: get("TRACKER_URL").unwrap_or_else(|| DEFAULT_TRACKER_URL.into()),
            t_high: get("T_HIGH")
                .and_then(|v| v.parse().ok())
                .unwrap_or(0.55),
            t_low: get("T_LOW").and_then(|v| v.parse().ok()).unwrap_or(0.35),
            ai_publish: ai_publish_mode(get("AI_PUBLISH").as_deref()).into(),
        }
    }

    pub fn jira_live(&self) -> bool {
        self.sync_mode == "live"
            && !self.jira_base_url.is_empty()
            && !self.jira_token.is_empty()
    }

    pub fn github_live(&self) -> bool {
        self.sync_mode == "live" && !self.github_token.is_empty() && !self.github_org.is_empty()
    }

    /// Live mode reads Jira, so it needs the project keys before the server listens.
    pub fn live_ready(&self) -> Result<(), String> {
        if self.sync_mode == "live" && self.jira_boards.is_empty() {
            return Err(
                "ROLLUP_SYNC_MODE=live needs ROLLUP_JIRA_BOARDS: the Jira project keys to read, for example PROJ,OPS."
                    .into(),
            );
        }
        Ok(())
    }
}

/// `auto` publishes every open draft. Anything else, including unset, is `review`.
fn ai_publish_mode(raw: Option<&str>) -> &'static str {
    if raw == Some("auto") { "auto" } else { "review" }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(mode: &str, boards: &[&str]) -> Settings {
        Settings {
            sync_mode: mode.into(),
            db_path: PathBuf::from("rollup.db"),
            jira_base_url: String::new(),
            jira_email: String::new(),
            jira_token: String::new(),
            jira_boards: boards.iter().map(|s| (*s).to_string()).collect(),
            github_token: String::new(),
            github_org: String::new(),
            github_repos: Vec::new(),
            tracker_url: DEFAULT_TRACKER_URL.into(),
            ai_publish: "review".into(),
            t_high: 0.55,
            t_low: 0.35,
        }
    }

    #[test]
    fn live_ready_needs_boards_only_when_live() {
        let err = settings("live", &[]).live_ready().unwrap_err();
        assert_eq!(
            err,
            "ROLLUP_SYNC_MODE=live needs ROLLUP_JIRA_BOARDS: the Jira project keys to read, for example PROJ,OPS."
        );
        assert!(settings("live", &["PROJ"]).live_ready().is_ok());
        assert!(settings("fixture", &[]).live_ready().is_ok());
    }

    #[test]
    fn ai_publish_is_review_unless_auto() {
        assert_eq!(ai_publish_mode(None), "review");
        assert_eq!(ai_publish_mode(Some("")), "review");
        assert_eq!(ai_publish_mode(Some("review")), "review");
        assert_eq!(ai_publish_mode(Some("AUTO")), "review");
        assert_eq!(ai_publish_mode(Some("yes")), "review");
        assert_eq!(ai_publish_mode(Some("auto")), "auto");
    }
}
