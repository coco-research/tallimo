//! Operator feedback (human loop): append one schema-v1 loop-log line per flag
//! to a local `feedback.jsonl` next to the database. See docs/LOOP-LOG.md.
//! Local file only: nothing here sends it anywhere.

use std::io::Write;
use std::path::{Path, PathBuf};

/// Pages whose cards can be flagged. Leadership is out of scope for now.
pub const PAGES: [&str; 2] = ["today", "inbox"];
/// wrong: the card's number or claim is wrong. confirm / dismiss: the operator
/// confirms or dismisses what the card flagged.
pub const VERDICTS: [&str; 3] = ["wrong", "confirm", "dismiss"];
pub const NOTE_MAX: usize = 200;

#[derive(Debug, PartialEq)]
pub enum FeedbackError {
    Invalid(String),
    Io(String),
}

/// `feedback.jsonl` in the same folder as the database, always as an absolute
/// path. A relative DB_PATH (the default is `rollup.db`) is resolved against
/// the current directory, the same one SQLite resolves it against, so the file
/// sits next to the database the server opened (the server never changes
/// directory). The write never targets a bare relative name.
pub fn path_for(db_path: &Path) -> PathBuf {
    let base = std::env::current_dir().unwrap_or_else(|_| std::env::temp_dir());
    path_in(db_path, &base)
}

/// `path_for` with the base directory given (testable without changing cwd).
pub fn path_in(db_path: &Path, base: &Path) -> PathBuf {
    let db = if db_path.is_absolute() { db_path.to_path_buf() } else { base.join(db_path) };
    db.with_file_name("feedback.jsonl")
}

fn card_ok(card: &str) -> bool {
    !card.is_empty()
        && card.len() <= 64
        && card.bytes().all(|b| b.is_ascii_alphanumeric() || b"._:-".contains(&b))
}

/// Secret shapes, matched anywhere in the note (inside words, quotes, after
/// `token=` or `key:`, next to punctuation), the shapes a loop-log
/// writer redacts. Compiled once.
fn secret_re() -> &'static regex::Regex {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| {
        regex::Regex::new(
            r"sk-[A-Za-z0-9_-]{20,}|gh[pousr]_[A-Za-z0-9]{30,}|github_pat_[A-Za-z0-9_]{30,}|AKIA[0-9A-Z]{16}|xox[abprs]-[A-Za-z0-9-]{10,}|-----BEGIN",
        )
        .expect("static regex")
    })
}

/// Collapse whitespace, redact secret shapes anywhere, cap at NOTE_MAX characters.
pub fn clean_note(note: &str) -> String {
    let flat = note.split_whitespace().collect::<Vec<_>>().join(" ");
    secret_re().replace_all(&flat, "[redacted]").chars().take(NOTE_MAX).collect()
}

/// The JSON line for one flag (no trailing newline).
pub fn record(now_secs: u64, page: &str, card: &str, verdict: &str, note: &str) -> Result<String, FeedbackError> {
    if !PAGES.contains(&page) {
        return Err(FeedbackError::Invalid(format!("page must be one of {PAGES:?}")));
    }
    if !card_ok(card) {
        return Err(FeedbackError::Invalid("card must be 1-64 of A-Z a-z 0-9 . _ : -".into()));
    }
    if !VERDICTS.contains(&verdict) {
        return Err(FeedbackError::Invalid(format!("verdict must be one of {VERDICTS:?}")));
    }
    let note = clean_note(note);
    let detail = if note.is_empty() { verdict.to_string() } else { format!("{verdict}: {note}") };
    let ts = crate::store::format_epoch(now_secs);
    let ts = format!("{}Z", &ts[..19]);
    Ok(serde_json::json!({
        "v": 1,
        "ts": ts,
        "product": "tallimo",
        "loop": "human",
        "kind": "feedback",
        "actor": "operator",
        "ref": format!("{page}:{card}"),
        "detail": detail.chars().take(NOTE_MAX).collect::<String>(),
    })
    .to_string())
}

/// Append one line to `path` (created if missing). One write per line.
pub fn append(path: &Path, page: &str, card: &str, verdict: &str, note: &str) -> Result<(), FeedbackError> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|e| FeedbackError::Io(e.to_string()))?
        .as_secs();
    let mut line = record(now, page, card, verdict, note)?;
    line.push('\n');
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|e| FeedbackError::Io(e.to_string()))?;
    f.write_all(line.as_bytes()).map_err(|e| FeedbackError::Io(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("tallimo_fb_{}_{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d.join("feedback.jsonl")
    }

    #[test]
    fn the_file_sits_next_to_the_db_as_an_absolute_path() {
        let base = std::env::temp_dir().join("tallimo_base");
        assert_eq!(path_in(Path::new("/x/y/rollup.db"), &base), PathBuf::from("/x/y/feedback.jsonl"));
        // The default relative DB_PATH resolves against the start directory, never a bare relative name.
        assert_eq!(path_in(Path::new("rollup.db"), &base), base.join("feedback.jsonl"));
        assert_eq!(path_in(Path::new("data/rollup.db"), &base), base.join("data").join("feedback.jsonl"));
        assert!(path_for(Path::new("rollup.db")).is_absolute());
        assert!(path_for(Path::new("/x/rollup.db")).is_absolute());
    }

    fn tok(prefix: &str, n: usize) -> String {
        format!("{prefix}{}", "a".repeat(n))
    }

    #[test]
    fn secrets_are_redacted_anywhere_in_the_note() {
        let gh = tok(&format!("gh{}_", "p"), 36);
        let sk = tok(&format!("s{}-", "k"), 24);
        let aws = format!("AK{}{}", "IA", "ABCDEFGHIJKLMNOP");
        let cases = [
            (format!("token={gh}"), "token=[redacted]".to_string()),
            (format!("see ({sk}) now"), "see ([redacted]) now".to_string()),
            (format!("key:{aws}"), "key:[redacted]".to_string()),
            (format!("\"{gh}\""), "\"[redacted]\"".to_string()),
            (format!("'{sk}'"), "'[redacted]'".to_string()),
            (format!("{gh} at start"), "[redacted] at start".to_string()),
            (format!("at end {aws}"), "at end [redacted]".to_string()),
            (format!("comma,{sk}."), "comma,[redacted].".to_string()),
            (format!("{gh};{aws}"), "[redacted];[redacted]".to_string()),
            ("-----BEGIN RSA PRIVATE KEY".to_string(), "[redacted] RSA PRIVATE KEY".to_string()),
        ];
        for (input, want) in cases {
            assert_eq!(clean_note(&input), want, "{input}");
        }
    }

    #[test]
    fn ordinary_text_is_untouched() {
        for plain in [
            "count is off by 2",
            "task-runner sk-short ghp_short AKIA123",
            "ask-for-review, risk-free, desk-12",
            "token=abc key:value (fine) \"quoted\"",
            "PAY-101: merged, no date.",
        ] {
            assert_eq!(clean_note(plain), plain, "{plain}");
        }
    }

    #[test]
    fn a_record_is_schema_v1() {
        let line = record(1_760_000_000, "today", "pr-7", "wrong", "count  is\noff").unwrap();
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        let keys: Vec<_> = v.as_object().unwrap().keys().cloned().collect();
        let mut want = vec!["v", "ts", "product", "loop", "kind", "actor", "ref", "detail"];
        want.sort();
        let mut got = keys.clone();
        got.sort();
        assert_eq!(got, want);
        assert_eq!(v["v"], 1);
        assert_eq!(v["ts"], "2025-10-09T08:53:20Z");
        assert_eq!(v["product"], "tallimo");
        assert_eq!(v["loop"], "human");
        assert_eq!(v["kind"], "feedback");
        assert_eq!(v["ref"], "today:pr-7");
        assert_eq!(v["detail"], "wrong: count is off");
    }

    #[test]
    fn bad_input_is_refused() {
        for (p, c, verdict) in [
            ("leadership", "x", "wrong"),
            ("today", "", "wrong"),
            ("today", "../etc", "wrong"),
            ("today", &"a".repeat(65), "wrong"),
            ("today", "x", "maybe"),
        ] {
            assert!(matches!(record(0, p, c, verdict, ""), Err(FeedbackError::Invalid(_))), "{p} {c} {verdict}");
        }
    }

    #[test]
    fn notes_are_cleaned_and_capped() {
        let tok = format!("gh{}_{}", "p", "a".repeat(36));
        let n = clean_note(&format!("see {tok} here"));
        assert_eq!(n, "see [redacted] here");
        let long = record(0, "inbox", "c1", "dismiss", &"z".repeat(500)).unwrap();
        let v: serde_json::Value = serde_json::from_str(&long).unwrap();
        assert!(v["detail"].as_str().unwrap().chars().count() <= NOTE_MAX);
    }

    #[test]
    fn append_adds_one_line_per_call_and_never_rewrites() {
        let f = tmp("append");
        append(&f, "today", "a", "wrong", "").unwrap();
        append(&f, "inbox", "b", "confirm", "ok").unwrap();
        let text = std::fs::read_to_string(&f).unwrap();
        let lines: Vec<_> = text.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(text.ends_with('\n'));
        assert!(lines[1].contains("\"ref\":\"inbox:b\""));
        assert_eq!(append(&f, "x", "a", "wrong", ""), Err(FeedbackError::Invalid("page must be one of [\"today\", \"inbox\"]".into())));
        assert_eq!(std::fs::read_to_string(&f).unwrap().lines().count(), 2);
        let _ = std::fs::remove_dir_all(f.parent().unwrap());
    }
}
