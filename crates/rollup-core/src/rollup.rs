//! Daily rollup per epic (PRD-1 section 10, deterministic template for P1).
//!
//! The LLM narrated pass is P2; this builder produces the validated JSON shape
//! the dashboard needs. Same output contract, no LLM yet.

use rusqlite::Connection;

pub const MODEL: &str = "template-v1";
use serde_json::{json, Value};

/// Build the rollup for every active epic.
pub fn build_rollups(conn: &Connection) -> rusqlite::Result<Vec<Value>> {
    let epics: Vec<(String, String, String)> = {
        let mut stmt =
            conn.prepare("SELECT epic_key, summary, status FROM epics ORDER BY epic_key")?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        let v: Vec<(String, String, String)> = rows.filter_map(|r| r.ok()).collect();
        v
    };
    let mut out = Vec::new();
    for (epic_key, summary, epic_status) in epics {
        out.push(build_one(conn, &epic_key, &summary, epic_status.as_str())?);
    }
    Ok(out)
}

fn build_one(
    conn: &Connection,
    epic_key: &str,
    epic_summary: &str,
    epic_status: &str,
) -> rusqlite::Result<Value> {
    // done: linked merged PRs
    let done: Vec<Value> = {
        let mut stmt = conn.prepare(
            "SELECT s.story_key, s.summary, p.repo, p.number, p.title, l.method, l.score
             FROM pr_story_links l
             JOIN stories s ON s.story_key = l.story_key
             JOIN pull_requests p ON p.id = l.pr_id
             WHERE s.epic_key = ?1 AND l.active = 1 AND p.state = 'merged'
             ORDER BY s.story_key",
        )?;
        let rows = stmt.query_map([epic_key], |r| {
            Ok(json!({
                "story_key": r.get::<_, String>(0)?,
                "story_summary": r.get::<_, String>(1)?,
                "repo": r.get::<_, String>(2)?,
                "pr_number": r.get::<_, i64>(3)?,
                "pr_title": r.get::<_, String>(4)?,
                "match": {"method": r.get::<_, String>(5)?, "score": r.get::<_, f64>(6)?},
            }))
        })?;
        let v: Vec<Value> = rows.filter_map(|r| r.ok()).collect();
        v
    };

    // in flight: linked open PRs
    let in_flight: Vec<Value> = {
        let mut stmt = conn.prepare(
            "SELECT s.story_key, s.summary, p.repo, p.number, p.title
             FROM pr_story_links l
             JOIN stories s ON s.story_key = l.story_key
             JOIN pull_requests p ON p.id = l.pr_id
             WHERE s.epic_key = ?1 AND l.active = 1 AND p.state = 'open'
             ORDER BY s.story_key",
        )?;
        let rows = stmt.query_map([epic_key], |r| {
            Ok(json!({
                "story_key": r.get::<_, String>(0)?,
                "story_summary": r.get::<_, String>(1)?,
                "repo": r.get::<_, String>(2)?,
                "pr_number": r.get::<_, i64>(3)?,
                "pr_title": r.get::<_, String>(4)?,
            }))
        })?;
        let v: Vec<Value> = rows.filter_map(|r| r.ok()).collect();
        v
    };

    // planned: stories in To Do / backlog with no merged PR
    let planned: Vec<Value> = {
        let mut stmt = conn.prepare(
            "SELECT story_key, summary, assignee, sprint FROM stories
             WHERE epic_key = ?1 AND status NOT IN ('Done','Closed')
               AND story_key NOT IN (SELECT s.story_key FROM pr_story_links l
                                     JOIN stories s ON s.story_key = l.story_key
                                     JOIN pull_requests p ON p.id = l.pr_id
                                     WHERE s.epic_key = ?1 AND l.active=1 AND p.state='merged')
             ORDER BY story_key",
        )?;
        let rows = stmt.query_map([epic_key], |r| {
            Ok(json!({
                "story_key": r.get::<_, String>(0)?,
                "summary": r.get::<_, String>(1)?,
                "assignee": r.get::<_, Option<String>>(2)?,
                "sprint": r.get::<_, Option<String>>(3)?,
            }))
        })?;
        let v: Vec<Value> = rows.filter_map(|r| r.ok()).collect();
        v
    };

    // attention: PRs open > 5 days without merge
    let attention: Vec<Value> = {
        let mut stmt = conn.prepare(
            "SELECT p.repo, p.number, p.title, p.created_at,
                    CAST((julianday('now') - julianday(p.created_at)) AS INTEGER)
             FROM pull_requests p
             WHERE p.state = 'open' AND p.repo IN (SELECT repo FROM repo_epic_map WHERE active=1 AND epic_key = ?1)
               AND julianday('now') - julianday(p.created_at) > 5",
        )?;
        let rows = stmt.query_map([epic_key], |r| {
            Ok(json!({
                "repo": r.get::<_, String>(0)?,
                "pr_number": r.get::<_, i64>(1)?,
                "title": r.get::<_, String>(2)?,
                "created_at": r.get::<_, Option<String>>(3)?,
                "days_open": r.get::<_, i64>(4)?,
            }))
        })?;
        let v: Vec<Value> = rows.filter_map(|r| r.ok()).collect();
        v
    };

    // unmatched count for this epic's repos
    let unmatched: i64 = {
        let mut stmt = conn.prepare(
            "SELECT COUNT(*) FROM review_queue q
             JOIN pull_requests p ON p.id = q.pr_id
             WHERE q.status IN ('unmatched','candidate')
               AND p.repo IN (SELECT repo FROM repo_epic_map WHERE active=1 AND epic_key = ?1)",
        )?;
        stmt.query_row([epic_key], |r| r.get(0))?
    };

    Ok(json!({
        "epic_key": epic_key,
        "summary": epic_summary,
        "status": epic_status,
        "done": done,
        "in_flight": in_flight,
        "planned": planned,
        "attention": attention,
        "unmatched_count": unmatched,
    }))
}
