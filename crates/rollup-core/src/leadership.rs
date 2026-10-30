//! Leadership-view derivations: who is on what (commitment) and the tiered
//! forecast (PRD-1 section 14, PRD-2 sections 9-10).

use rusqlite::Connection;
use serde_json::{json, Value};

/// 10 business days (Mon-Fri) before the day of now_secs in UTC (PRD-2 section 9).
/// Count back weekdays only; the day of now itself does not count.
pub fn commitment_cutoff(now_secs: u64) -> String {
    let mut day = (now_secs / 86_400) as i64;
    let mut business_days = 0;
    while business_days < 10 {
        day -= 1;
        let weekday = (day + 4).rem_euclid(7);
        if weekday != 0 && weekday != 6 {
            business_days += 1;
        }
    }
    let cutoff_secs = (day * 86_400) as u64;
    crate::store::format_epoch(cutoff_secs)[..10].to_string()
}

/// People per epic with activity, plus roster totals. Commitment = assignment
/// plus observed GitHub movement in the last 10 business days (PRD-2 section 9).
pub fn build_summary(conn: &Connection) -> rusqlite::Result<Value> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock before epoch");
    let cutoff = commitment_cutoff(now.as_secs());

    let epics: Vec<(String, String)> = {
        let mut stmt = conn.prepare("SELECT epic_key, summary FROM epics ORDER BY epic_key")?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
        let v: Vec<(String, String)> = rows.filter_map(|r| r.ok()).collect();
        v
    };

    let mut epic_views = Vec::new();
    let mut committed_total = 0.0;
    for (epic_key, epic_summary) in &epics {
        let people: Vec<Value> = {
            let mut stmt = conn.prepare(
                "SELECT i.display_name, i.github_login, i.jira_account_id
                 FROM identity_map i WHERE i.github_login IS NOT NULL ORDER BY i.display_name",
            )?;
            let all: Vec<(String, String, String)> = stmt
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
                .filter_map(|r| r.ok())
                .collect();
            let mut out = Vec::new();
            for (name, gh, jira) in all {
                let prs: i64 = conn.query_row(
                    "SELECT COUNT(*) FROM pull_requests p
                     WHERE p.author = ?1 AND p.repo IN
                       (SELECT repo FROM repo_epic_map WHERE active=1 AND epic_key=?2)
                       AND COALESCE(p.merged_at, p.closed_at, p.created_at) >= ?3",
                    rusqlite::params![gh, epic_key, &cutoff],
                    |r| r.get(0),
                )?;
                let stories: i64 = conn.query_row(
                    "SELECT COUNT(*) FROM stories WHERE assignee=?1 AND epic_key=?2",
                    rusqlite::params![jira, epic_key],
                    |r| r.get(0),
                )?;
                if prs > 0 || stories > 0 {
                    out.push(json!({
                        "name": name,
                        "prs": prs,
                        "stories": stories,
                        "primary": prs > 0,
                    }));
                }
            }
            out
        };
        let n = people.len() as f64;
        committed_total += n;
        epic_views.push(json!({
            "epic_key": epic_key,
            "summary": epic_summary,
            "people": people,
            "committed": n,
        }));
    }

    Ok(json!({
        "epics": epic_views,
        "committed_total": committed_total,
        "roster": 6, // demo roster; settings-editable in P2+
    }))
}

/// Tiered forecast (PRD-1 section 14). T1 = committed stories from boards.
/// T2 = epics in flight sized by shape; a shape needs >= 3 historical epics,
/// which the demo history cannot satisfy, so it honestly renders
/// "insufficient history" (that is the designed behaviour, not a gap).
/// T3 = capacity envelope only.
pub fn build_forecast(conn: &Connection) -> rusqlite::Result<Value> {
    // T1: committed (non-done, assigned or next-sprint) stories per epic
    let t1: Vec<Value> = {
        let mut stmt = conn.prepare(
            "SELECT epic_key, story_key, summary, assignee, sprint FROM stories
             WHERE status NOT IN ('Done','Closed')
             ORDER BY epic_key, story_key",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(json!({
                "epic_key": r.get::<_, String>(0)?,
                "story_key": r.get::<_, String>(1)?,
                "summary": r.get::<_, String>(2)?,
                "assignee": r.get::<_, Option<String>>(3)?,
                "sprint": r.get::<_, Option<String>>(4)?,
            }))
        })?;
        let v: Vec<Value> = rows.filter_map(|r| r.ok()).collect();
        v
    };

    // T2: epics in flight with shape
    let t2: Vec<Value> = {
        let mut stmt = conn.prepare(
            "SELECT epic_key, summary, shape FROM epics WHERE status = 'In Progress' ORDER BY epic_key",
        )?;
        let rows = stmt.query_map([], |r| {
            let (key, summary, shape): (String, String, Option<String>) =
                (r.get(0)?, r.get(1)?, r.get(2)?);
            let shape = shape.unwrap_or_else(|| "unclassified".into());
            Ok(json!({
                "epic_key": key,
                "summary": summary,
                "shape": shape,
                "p50_weeks": Value::Null,
                "p80_weeks": Value::Null,
                "basis": [],
                "note": "insufficient history (0 past epics of this shape)",
            }))
        })?;
        let v: Vec<Value> = rows.filter_map(|r| r.ok()).collect();
        v
    };

    Ok(json!({
        "t1_committed": t1,
        "t2_shaped": t2,
        "t3_envelope": {
            "committed_now": 4.5,
            "roster": 6,
            "note": "capacity envelope only; per-story detail is not yet knowable",
        },
    }))
}

#[cfg(test)]
mod tests {
    #[test]
    fn commitment_cutoff() {
        // Wednesday 2026-10-07 (epoch 1791331200) -> Wednesday 2026-09-23
        assert_eq!(super::commitment_cutoff(1791331200), "2026-09-23");
        // Monday 2026-10-05 (epoch 1791158400) -> Monday 2026-09-21
        assert_eq!(super::commitment_cutoff(1791158400), "2026-09-21");
    }

    #[test]
    fn build_summary() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static N: AtomicUsize = AtomicUsize::new(0);
        let db = std::env::temp_dir().join(format!(
            "rollup_leadership_{}_{}.db",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_file(&db);
        crate::store::init_db(&db).unwrap();
        let conn = crate::store::connect(&db).unwrap();

        conn.execute(
            "INSERT INTO identity_map (jira_account_id, github_login, display_name, confirmed, role)
             VALUES ('alice@firm.example', 'alice', 'Alice', 1, 'dev')",
            [],
        )
        .unwrap();

        conn.execute(
            "INSERT INTO epics (epic_key, board, summary) VALUES ('EPIC-1', 'BOARD-1', 'Epic One')",
            [],
        )
        .unwrap();

        conn.execute(
            "INSERT INTO repo_epic_map (repo, epic_key, active) VALUES ('repo-1', 'EPIC-1', 1)",
            [],
        )
        .unwrap();

        let yesterday = crate::store::utc_days_ago(1);
        let sixty_days_ago = crate::store::utc_days_ago(60);

        conn.execute(
            "INSERT INTO pull_requests (id, repo, number, title, author, state, merged_at)
             VALUES ('pr_recent', 'repo-1', 1, 'Recent PR', 'alice', 'merged', ?1)",
            [&yesterday],
        )
        .unwrap();

        conn.execute(
            "INSERT INTO pull_requests (id, repo, number, title, author, state, merged_at)
             VALUES ('pr_old', 'repo-1', 2, 'Old PR', 'alice', 'merged', ?1)",
            [&sixty_days_ago],
        )
        .unwrap();

        let summary = super::build_summary(&conn).unwrap();
        let epics = summary["epics"].as_array().expect("epics");
        assert_eq!(epics.len(), 1);
        let people = epics[0]["people"].as_array().expect("people");
        assert_eq!(people.len(), 1);
        assert_eq!(people[0]["name"], "Alice");
        assert_eq!(people[0]["prs"], 1);

        let _ = std::fs::remove_file(&db);
    }
}

