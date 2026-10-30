//! PR→story matching (PRD-1 section 9).
//!
//! Pass 1: keyword — issue keys in title/body/branch auto-link at 0.99, but
//! only when the key's board matches the repo's mapped epic (D4).
//! Pass 2: text — tf-cosine-v1 similarity, narrowed to stories of the repo's
//! mapped epic. Thresholds are calibrated per model (PRD 9.4); the defaults in
//! Settings are for this model. Manual decisions always win over recompute
//! (PRD 9.5). A PR has at most one active link (PRD 9.5).

use rusqlite::Connection;
use std::collections::{HashMap, HashSet};

pub const MODEL_VERSION: &str = "tf-cosine-v1";
pub const KEYWORD_SCORE: f64 = 0.99;
const TIE_GAP: f64 = 0.05;

#[derive(Debug, Default)]
pub struct MatchResult {
    pub auto: usize,
    pub candidates: usize,
    pub unmatched: usize,
}

fn keyword_re() -> regex::Regex {
    regex::Regex::new(r"\b[A-Z][A-Z0-9]+-\d+\b").expect("static regex")
}

const STOPWORDS: &[&str] = &[
    "the", "and", "for", "with", "that", "this", "from", "into", "are", "was", "were", "has",
    "have", "had", "not", "but", "all", "can", "will", "its", "our", "their", "per", "via",
    "any", "out", "how", "why", "who", "when", "what", "which", "each", "also", "more", "than",
    "then", "over", "under", "between", "against", "adds", "add", "new",
];

pub fn tokens(text: &str) -> Vec<String> {
    text.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| t.len() > 2 && !STOPWORDS.contains(t))
        .map(String::from)
        .collect()
}

pub fn cosine(a: &[String], b: &[String]) -> f64 {
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    fn tf(toks: &[String]) -> HashMap<&str, f64> {
        let mut m: HashMap<&str, f64> = HashMap::new();
        for t in toks {
            *m.entry(t.as_str()).or_insert(0.0) += 1.0;
        }
        m
    }
    let (ta, tb) = (tf(a), tf(b));
    let dot: f64 = ta
        .iter()
        .filter_map(|(k, va)| tb.get(k).map(|vb| va * vb))
        .sum();
    let na: f64 = ta.values().map(|v| v * v).sum::<f64>().sqrt();
    let nb: f64 = tb.values().map(|v| v * v).sum::<f64>().sqrt();
    if na == 0.0 || nb == 0.0 {
        0.0
    } else {
        dot / (na * nb)
    }
}

struct StoryRow {
    key: String,
    epic_key: String,
    text: String,
}

/// Run one full match pass over every PR that has no protected (user-confirmed)
/// link and no active link yet.
pub fn run_match(conn: &Connection, t_high: f64, t_low: f64) -> rusqlite::Result<MatchResult> {
    let re = keyword_re();
    let mut result = MatchResult::default();

    // repo -> epic
    let mut mapping: HashMap<String, String> = HashMap::new();
    {
        let mut stmt = conn.prepare("SELECT repo, epic_key FROM repo_epic_map WHERE active=1")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        for row in rows {
            let (repo, epic) = row?;
            mapping.insert(repo, epic);
        }
    }

    // stories by epic
    let mut stories: Vec<StoryRow> = Vec::new();
    {
        let mut stmt =
            conn.prepare("SELECT story_key, epic_key, summary, description FROM stories")?;
        let rows = stmt.query_map([], |r| {
            Ok(StoryRow {
                key: r.get(0)?,
                epic_key: r.get(1)?,
                text: format!("{} {}", r.get::<_, String>(2)?, r.get::<_, String>(3)?),
            })
        })?;
        for row in rows {
            stories.push(row?);
        }
    }
    let mut stories_by_epic: HashMap<String, Vec<&StoryRow>> = HashMap::new();
    for s in &stories {
        stories_by_epic.entry(s.epic_key.clone()).or_default().push(s);
    }

    // PRs without an active link and without a user decision
    let mut protected: HashSet<String> = HashSet::new();
    {
        let mut stmt = conn.prepare(
            "SELECT DISTINCT pr_id FROM pr_story_links WHERE status IN ('confirmed','rejected')",
        )?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        for row in rows {
            protected.insert(row?);
        }
    }
    let mut prs: Vec<(String, String, String, String, String)> = Vec::new(); // id, repo, title, body, branch
    {
        let mut stmt = conn.prepare(
            "SELECT p.id, p.repo, p.title, p.body, COALESCE(p.head_branch,'')
             FROM pull_requests p
             WHERE NOT EXISTS (SELECT 1 FROM pr_story_links l
                               WHERE l.pr_id = p.id AND l.active = 1)
               AND p.id NOT IN (SELECT pr_id FROM review_queue WHERE status = 'ignored')",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
            ))
        })?;
        for row in rows {
            prs.push(row?);
        }
    }

    for (pr_id, repo, title, body, branch) in prs {
        if protected.contains(&pr_id) {
            continue;
        }
        let Some(epic_key) = mapping.get(&repo) else {
            queue(conn, &pr_id, "unmatched", None, None, Some("repo unmapped"))?;
            result.unmatched += 1;
            continue;
        };
        let candidates = stories_by_epic.get(epic_key);
        let Some(candidates) = candidates else {
            queue(conn, &pr_id, "unmatched", None, None, Some("epic has no stories"))?;
            result.unmatched += 1;
            continue;
        };

        // Pass 1: keyword. Key must exist and share the mapped epic's board prefix.
        let board = epic_key.split('-').next().unwrap_or_default();
        let hay = format!("{title}\n{body}\n{branch}");
        let key_hit = re
            .find_iter(&hay)
            .map(|m| m.as_str().to_string())
            .find(|k| {
                candidates.iter().any(|s| &s.key == k) && k.split('-').next() == Some(board)
            });
        if let Some(key) = key_hit {
            link(conn, &pr_id, &key, KEYWORD_SCORE, "keyword")?;
            queue(conn, &pr_id, "linked", Some(&key), Some(KEYWORD_SCORE), None)?;
            result.auto += 1;
            continue;
        }

        // Pass 2: text similarity, narrowed to the mapped epic's stories (D4).
        let pr_tokens = tokens(&format!("{title} {body}"));
        let mut scored: Vec<(f64, &StoryRow)> = candidates
            .iter()
            .map(|s| (cosine(&pr_tokens, &tokens(&s.text)), *s))
            .collect();
        scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));

        let top = scored.first().cloned();
        let second = scored.get(1).map(|(s, _)| *s);
        match top {
            Some((score, story)) if score >= t_high && second.map_or(true, |s2| score - s2 >= TIE_GAP) => {
                link(conn, &pr_id, &story.key, score, "text")?;
                queue(conn, &pr_id, "linked", Some(&story.key), Some(score), None)?;
                result.auto += 1;
            }
            Some((score, story)) if score >= t_low => {
                queue(conn, &pr_id, "candidate", Some(&story.key), Some(score), None)?;
                result.candidates += 1;
            }
            _ => {
                queue(conn, &pr_id, "unmatched", None, None, None)?;
                result.unmatched += 1;
            }
        }
    }
    Ok(result)
}

fn link(
    conn: &Connection,
    pr_id: &str,
    story_key: &str,
    score: f64,
    method: &str,
) -> rusqlite::Result<()> {
    // One active link per PR: deactivate other rows first.
    conn.execute("UPDATE pr_story_links SET active=0 WHERE pr_id=?1", [pr_id])?;
    conn.execute(
        "INSERT INTO pr_story_links (pr_id, story_key, score, method, status, model_version, created_at, active)
         VALUES (?1, ?2, ?3, ?4, 'auto', ?5, ?6, 1)
         ON CONFLICT(pr_id, story_key) DO UPDATE SET
           score=excluded.score, method=excluded.method, status='auto',
           model_version=excluded.model_version, active=1",
        rusqlite::params![pr_id, story_key, score, method, MODEL_VERSION, crate::store::utcnow()],
    )?;
    Ok(())
}

fn queue(
    conn: &Connection,
    pr_id: &str,
    status: &str,
    suggested: Option<&str>,
    score: Option<f64>,
    note: Option<&str>,
) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO review_queue (pr_id, status, suggested_story, suggested_score, notes, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT(pr_id) DO UPDATE SET
           status=excluded.status, suggested_story=excluded.suggested_story,
           suggested_score=excluded.suggested_score, notes=excluded.notes,
           updated_at=excluded.updated_at
         WHERE review_queue.status NOT IN ('ignored')",
        rusqlite::params![pr_id, status, suggested, score, note, crate::store::utcnow()],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn setup(name: &str) -> (Connection, PathBuf) {
        let db = std::env::temp_dir().join(format!("rollup_match_{name}.db"));
        let _ = std::fs::remove_file(&db);
        crate::store::init_db(&db).unwrap();
        let conn = crate::store::connect(&db).unwrap();
        conn.execute_batch(
            "INSERT INTO epics (epic_key, board, summary) VALUES ('PAY-100','PAY','Payments');
             INSERT INTO stories (story_key, epic_key, summary, description) VALUES
               ('PAY-101','PAY-100','Card refund endpoint','Build a refund endpoint for card payments with idempotency keys and audit logging of refund events.'),
               ('PAY-103','PAY-100','Chargeback intake form','Internal form for analysts to log chargebacks with reason codes.');
             INSERT INTO pull_requests (id, repo, number, title, body, state, head_branch) VALUES
               ('pr_kw','payments-core',1,'Add refund endpoint','Implements refund endpoint for card payments with idempotency keys and audit logging','merged','feat/PAY-101-card-refund'),
               ('pr_txt','payments-core',2,'Refund endpoint for card payments','Refund endpoint for card payments with idempotency keys and refund event audit logging','merged','feat/refund'),
               ('pr_low','payments-core',3,'Bump dependency versions','Routine dependency updates, patch releases, no behaviour change','merged','chore/bump');
             INSERT INTO repo_epic_map (repo, epic_key) VALUES ('payments-core','PAY-100');",
        )
        .unwrap();
        (conn, db)
    }

    #[test]
    fn keyword_in_branch_autolinks() {
        let (conn, db) = setup("keyword_in_branch_autolinks");
        let r = run_match(&conn, 0.55, 0.35).unwrap();
        assert_eq!(r.auto, 2); // pr_kw (keyword) + pr_txt (text)
        let (method, score): (String, f64) = conn
            .query_row(
                "SELECT method, score FROM pr_story_links WHERE pr_id='pr_kw' AND active=1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(method, "keyword");
        assert!((score - KEYWORD_SCORE).abs() < f64::EPSILON);
        let _ = std::fs::remove_file(&db);
    }

    #[test]
    fn text_match_links_and_low_scores_queue() {
        let (conn, db) = setup("text_match_links_and_low_scores_queue");
        let r = run_match(&conn, 0.55, 0.35).unwrap();
        let (method, story): (String, String) = conn
            .query_row(
                "SELECT method, story_key FROM pr_story_links WHERE pr_id='pr_txt' AND active=1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(method, "text");
        assert_eq!(story, "PAY-101");
        let status: String = conn
            .query_row(
                "SELECT status FROM review_queue WHERE pr_id='pr_low'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(status, "unmatched");
        assert_eq!(r.unmatched, 1);
        let _ = std::fs::remove_file(&db);
    }

    #[test]
    fn rerun_does_not_duplicate_or_overwrite() {
        let (conn, db) = setup("rerun_does_not_duplicate_or_overwrite");
        run_match(&conn, 0.55, 0.35).unwrap();
        run_match(&conn, 0.55, 0.35).unwrap();
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM pr_story_links WHERE active=1", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 2);
        let _ = std::fs::remove_file(&db);
    }

    #[test]
    fn keyword_only_in_body_autolinks() {
        let (conn, db) = setup("keyword_only_in_body");
        conn.execute(
            "INSERT INTO pull_requests (id, repo, number, title, body, state, head_branch) VALUES
             ('pr_body','payments-core',10,'Generic title','Resolves PAY-103 in testing','open','feat/patch-1')",
            [],
        )
        .unwrap();
        let _ = run_match(&conn, 0.55, 0.35).unwrap();
        let (method, score, story): (String, f64, String) = conn
            .query_row(
                "SELECT method, score, story_key FROM pr_story_links WHERE pr_id='pr_body' AND active=1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(method, "keyword");
        assert!((score - KEYWORD_SCORE).abs() < f64::EPSILON);
        assert_eq!(story, "PAY-103");

        let (status, suggested_story, suggested_score): (String, Option<String>, Option<f64>) = conn
            .query_row(
                "SELECT status, suggested_story, suggested_score FROM review_queue WHERE pr_id='pr_body'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(status, "linked");
        assert_eq!(suggested_story.as_deref(), Some("PAY-103"));
        assert!((suggested_score.unwrap() - KEYWORD_SCORE).abs() < f64::EPSILON);
        let _ = std::fs::remove_file(&db);
    }

    #[test]
    fn keyword_only_in_branch_autolinks() {
        let (conn, db) = setup("keyword_only_in_branch");
        conn.execute(
            "INSERT INTO pull_requests (id, repo, number, title, body, state, head_branch) VALUES
             ('pr_branch','payments-core',11,'Unrelated title','Completely unrelated description','open','feat/PAY-101-work')",
            [],
        )
        .unwrap();
        let _ = run_match(&conn, 0.55, 0.35).unwrap();
        let (method, score, story): (String, f64, String) = conn
            .query_row(
                "SELECT method, score, story_key FROM pr_story_links WHERE pr_id='pr_branch' AND active=1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(method, "keyword");
        assert!((score - KEYWORD_SCORE).abs() < f64::EPSILON);
        assert_eq!(story, "PAY-101");

        let (status, suggested_story, suggested_score): (String, Option<String>, Option<f64>) = conn
            .query_row(
                "SELECT status, suggested_story, suggested_score FROM review_queue WHERE pr_id='pr_branch'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(status, "linked");
        assert_eq!(suggested_story.as_deref(), Some("PAY-101"));
        assert!((suggested_score.unwrap() - KEYWORD_SCORE).abs() < f64::EPSILON);
        let _ = std::fs::remove_file(&db);
    }

    #[test]
    fn pr_with_no_key_stays_unmatched() {
        let (conn, db) = setup("pr_with_no_key_stays_unmatched");
        let r = run_match(&conn, 0.55, 0.35).unwrap();
        // In setup, pr_low has no key in title, body, or branch, and low cosine score
        let links: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM pr_story_links WHERE pr_id='pr_low'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(links, 0);

        let (status, suggested_story, suggested_score, notes): (
            String,
            Option<String>,
            Option<f64>,
            Option<String>,
        ) = conn
            .query_row(
                "SELECT status, suggested_story, suggested_score, notes FROM review_queue WHERE pr_id='pr_low'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap();
        assert_eq!(status, "unmatched");
        assert_eq!(suggested_story, None);
        assert_eq!(suggested_score, None);
        assert_eq!(notes, None);
        assert_eq!(r.unmatched, 1);
        let _ = std::fs::remove_file(&db);
    }
}
