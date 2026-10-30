//! Item sources, notes, flags, and change detection for AI drafting.
//! Storage and pure string helpers. No model calls.

use rusqlite::{params, Connection, OptionalExtension};
use std::collections::HashMap;

/// `Invalid` is a bad input. `Db` is SQLite.
#[derive(Debug, PartialEq)]
pub enum AiError {
    Invalid(String),
    Db(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Note {
    pub id: i64,
    pub kind: String,
    pub body: String,
    pub created_at: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Flag {
    pub id: i64,
    pub item_id: String,
    pub field: String,
    pub reason: String,
    pub raised_at: String,
}

fn db_err(err: impl ToString) -> AiError {
    AiError::Db(err.to_string())
}

fn require_source<'a>(
    item_id: &'a str,
    kind: &'a str,
    key: &'a str,
) -> Result<(&'a str, &'a str, &'a str), AiError> {
    let item_id = item_id.trim();
    let kind = kind.trim();
    let key = key.trim();
    if item_id.is_empty() {
        return Err(AiError::Invalid("Item id is required.".into()));
    }
    if key.is_empty() {
        return Err(AiError::Invalid("Source key is required.".into()));
    }
    if !matches!(kind, "epic" | "story" | "repo") {
        return Err(AiError::Invalid(
            "Source kind must be epic, story, or repo.".into(),
        ));
    }
    Ok((item_id, kind, key))
}

/// Link `item_id` to one epic, story, or repo. A duplicate link is a no-op.
pub fn map_source(
    conn: &Connection,
    item_id: &str,
    kind: &str,
    key: &str,
    now: &str,
) -> Result<(), AiError> {
    let (item_id, kind, key) = require_source(item_id, kind, key)?;
    conn.execute(
        "INSERT OR IGNORE INTO lu_item_sources (item_id, source_kind, source_key, created_at)
         VALUES (?1, ?2, ?3, ?4)",
        params![item_id, kind, key, now],
    )
    .map_err(db_err)?;
    Ok(())
}

pub fn unmap_source(
    conn: &Connection,
    item_id: &str,
    kind: &str,
    key: &str,
) -> Result<bool, AiError> {
    let (item_id, kind, key) = require_source(item_id, kind, key)?;
    let n = conn
        .execute(
            "DELETE FROM lu_item_sources
             WHERE item_id=?1 AND source_kind=?2 AND source_key=?3",
            params![item_id, kind, key],
        )
        .map_err(db_err)?;
    Ok(n == 1)
}

pub fn sources(conn: &Connection, item_id: &str) -> Result<Vec<(String, String)>, AiError> {
    let item_id = item_id.trim();
    if item_id.is_empty() {
        return Err(AiError::Invalid("Item id is required.".into()));
    }
    let mut stmt = conn
        .prepare(
            "SELECT source_kind, source_key FROM lu_item_sources
             WHERE item_id=?1 ORDER BY source_kind, source_key",
        )
        .map_err(db_err)?;
    let rows = stmt
        .query_map(params![item_id], |row| Ok((row.get(0)?, row.get(1)?)))
        .map_err(db_err)?;
    rows.collect::<Result<Vec<_>, _>>().map_err(db_err)
}

fn split_glossary(body: &str) -> Option<(&str, &str)> {
    let (heard, correct) = body.split_once(" => ")?;
    let heard = heard.trim();
    let correct = correct.trim();
    if heard.is_empty() || correct.is_empty() {
        None
    } else {
        Some((heard, correct))
    }
}

fn valid_rule(body: &str) -> bool {
    ["drop:", "no_end_date:", "never_say:"]
        .into_iter()
        .any(|prefix| {
            body.strip_prefix(prefix)
                .is_some_and(|value| !value.trim().is_empty())
        })
}

fn check_note(kind: &str, body: &str) -> Result<(), AiError> {
    if !matches!(kind, "fact" | "glossary" | "rule" | "escalate") {
        return Err(AiError::Invalid(
            "Note kind must be fact, glossary, rule, or escalate.".into(),
        ));
    }
    if body.is_empty() {
        return Err(AiError::Invalid("Write the note.".into()));
    }
    if body.chars().count() > 2000 {
        return Err(AiError::Invalid("Keep the note to 2000 characters.".into()));
    }
    if kind == "glossary" && split_glossary(body).is_none() {
        return Err(AiError::Invalid(
            "A glossary note looks like \"heard => correct\" with both sides filled in.".into(),
        ));
    }
    if kind == "rule" && !valid_rule(body) {
        return Err(AiError::Invalid(
            "A rule looks like drop:<item_id>, no_end_date:<item_id>, or never_say:<phrase>."
                .into(),
        ));
    }
    Ok(())
}

pub fn add_note(conn: &Connection, kind: &str, body: &str, now: &str) -> Result<i64, AiError> {
    let kind = kind.trim();
    let body = body.trim();
    check_note(kind, body)?;
    if let Some(id) = conn
        .query_row(
            "SELECT id FROM ai_notes
             WHERE resolved_at IS NULL AND kind=?1 AND body=?2
             ORDER BY id LIMIT 1",
            params![kind, body],
            |row| row.get(0),
        )
        .optional()
        .map_err(db_err)?
    {
        return Ok(id);
    }
    conn.execute(
        "INSERT INTO ai_notes (kind, body, created_at, resolved_at) VALUES (?1, ?2, ?3, NULL)",
        params![kind, body, now],
    )
    .map_err(db_err)?;
    Ok(conn.last_insert_rowid())
}

pub fn open_notes(conn: &Connection, kind: Option<&str>) -> Result<Vec<Note>, AiError> {
    let kind = kind.map(str::trim);
    let mut stmt = conn
        .prepare(
            "SELECT id, kind, body, created_at FROM ai_notes
             WHERE resolved_at IS NULL AND (?1 IS NULL OR kind=?1)
             ORDER BY created_at, id",
        )
        .map_err(db_err)?;
    let rows = stmt
        .query_map(params![kind], |row| {
            Ok(Note {
                id: row.get(0)?,
                kind: row.get(1)?,
                body: row.get(2)?,
                created_at: row.get(3)?,
            })
        })
        .map_err(db_err)?;
    rows.collect::<Result<Vec<_>, _>>().map_err(db_err)
}

pub fn resolve_note(conn: &Connection, id: i64, now: &str) -> Result<bool, AiError> {
    let n = conn
        .execute(
            "UPDATE ai_notes SET resolved_at=?1 WHERE id=?2 AND resolved_at IS NULL",
            params![now, id],
        )
        .map_err(db_err)?;
    Ok(n == 1)
}

fn boundary_ok(text: &str, start: usize, end: usize) -> bool {
    let before_ok = start == 0
        || !text[..start]
            .chars()
            .next_back()
            .is_some_and(char::is_alphanumeric);
    let after_ok = end == text.len()
        || !text[end..]
            .chars()
            .next()
            .is_some_and(char::is_alphanumeric);
    before_ok && after_ok
}

/// One left-to-right pass, so a replacement is never rewritten by a later pair.
fn replace_whole_words(text: &str, pairs: &[(String, String)]) -> String {
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    'scan: while i < text.len() {
        for (heard, correct) in pairs {
            let end = i + heard.len();
            if !heard.is_empty()
                && text[i..].starts_with(heard.as_str())
                && boundary_ok(text, i, end)
            {
                out.push_str(correct);
                i = end;
                continue 'scan;
            }
        }
        let ch = text[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// Apply every open glossary note. Whole word, case sensitive, longest heard first.
pub fn apply_glossary(conn: &Connection, text: &str) -> Result<String, AiError> {
    // ponytail: plain whole-word replace; a fuzzy matcher only if misheard names keep slipping through
    let mut pairs = Vec::new();
    for note in open_notes(conn, Some("glossary"))? {
        if let Some((heard, correct)) = split_glossary(&note.body) {
            pairs.push((heard.to_string(), correct.to_string()));
        }
    }
    pairs.sort_by(|a, b| b.0.len().cmp(&a.0.len()).then(a.0.cmp(&b.0)));
    Ok(replace_whole_words(text, &pairs))
}

/// Insert an open flag unless the same item, field, and reason are already open.
/// True only when a new row was created.
pub fn raise_flag(
    conn: &Connection,
    item_id: &str,
    field: &str,
    reason: &str,
    now: &str,
) -> Result<bool, AiError> {
    let item_id = item_id.trim();
    let field = field.trim();
    let reason = reason.trim();
    if item_id.is_empty() || field.is_empty() || reason.is_empty() {
        return Err(AiError::Invalid(
            "Item id, field and reason are required.".into(),
        ));
    }
    let n = conn
        .execute(
            "INSERT INTO ai_flags (item_id, field, reason, raised_at, resolved_at)
             VALUES (?1, ?2, ?3, ?4, NULL)
             ON CONFLICT(item_id, field, reason) WHERE resolved_at IS NULL DO NOTHING",
            params![item_id, field, reason, now],
        )
        .map_err(db_err)?;
    Ok(n == 1)
}

/// A detected change that belongs to at least one leadership item.
#[derive(Debug, Clone, PartialEq)]
pub struct Fact {
    pub kind: String,
    pub r#ref: String,
    pub items: Vec<String>,
}

/// Facts written this pass, and refs that map to no leadership item.
#[derive(Debug, Clone, PartialEq)]
pub struct Detection {
    pub facts: Vec<Fact>,
    pub unmapped: Vec<String>,
}

struct StoryNow {
    key: String,
    epic: String,
    state: String,
    title: String,
}

struct PrNow {
    id: String,
    repo: String,
    state: String,
    merged: String,
    title: String,
}

fn load_stories(conn: &Connection) -> Result<Vec<StoryNow>, AiError> {
    let mut stmt = conn
        .prepare(
            "SELECT story_key, epic_key, COALESCE(status, ''), summary
             FROM stories ORDER BY story_key",
        )
        .map_err(db_err)?;
    let rows = stmt
        .query_map([], |row| {
            Ok(StoryNow {
                key: row.get(0)?,
                epic: row.get(1)?,
                state: row.get(2)?,
                title: row.get(3)?,
            })
        })
        .map_err(db_err)?;
    rows.collect::<Result<Vec<_>, _>>().map_err(db_err)
}

fn load_prs(conn: &Connection) -> Result<Vec<PrNow>, AiError> {
    let mut stmt = conn
        .prepare(
            "SELECT id, repo, state, COALESCE(merged_at, ''), title
             FROM pull_requests ORDER BY id",
        )
        .map_err(db_err)?;
    let rows = stmt
        .query_map([], |row| {
            Ok(PrNow {
                id: row.get(0)?,
                repo: row.get(1)?,
                state: row.get(2)?,
                merged: row.get(3)?,
                title: row.get(4)?,
            })
        })
        .map_err(db_err)?;
    rows.collect::<Result<Vec<_>, _>>().map_err(db_err)
}

fn snap_map(conn: &Connection, kind: &str) -> Result<HashMap<String, String>, AiError> {
    let mut stmt = conn
        .prepare("SELECT key, state FROM ai_snapshot WHERE kind=?1")
        .map_err(db_err)?;
    let rows = stmt
        .query_map(params![kind], |row| Ok((row.get(0)?, row.get(1)?)))
        .map_err(db_err)?;
    rows.collect::<Result<_, _>>().map_err(db_err)
}

fn story_items(conn: &Connection, story_key: &str, epic_key: &str) -> Result<Vec<String>, AiError> {
    let mut stmt = conn
        .prepare(
            "SELECT DISTINCT item_id FROM lu_item_sources
             WHERE (source_kind='story' AND source_key=?1)
                OR (source_kind='epic' AND source_key=?2)
             ORDER BY item_id",
        )
        .map_err(db_err)?;
    let rows = stmt
        .query_map(params![story_key, epic_key], |row| row.get(0))
        .map_err(db_err)?;
    rows.collect::<Result<Vec<_>, _>>().map_err(db_err)
}

fn pr_items(conn: &Connection, pr_id: &str, repo: &str) -> Result<Vec<String>, AiError> {
    let mut stmt = conn
        .prepare(
            "SELECT DISTINCT item_id FROM lu_item_sources
             WHERE (source_kind='repo' AND source_key=?1)
                OR (source_kind='story' AND source_key IN (
                      SELECT story_key FROM pr_story_links
                      WHERE pr_id=?2 AND active=1 AND status != 'rejected'))
                OR (source_kind='epic' AND source_key IN (
                      SELECT st.epic_key FROM pr_story_links l
                      JOIN stories st ON st.story_key = l.story_key
                      WHERE l.pr_id=?2 AND l.active=1 AND l.status != 'rejected'))
             ORDER BY item_id",
        )
        .map_err(db_err)?;
    let rows = stmt
        .query_map(params![repo, pr_id], |row| row.get(0))
        .map_err(db_err)?;
    rows.collect::<Result<Vec<_>, _>>().map_err(db_err)
}

fn note_change(
    conn: &Connection,
    now: &str,
    facts: &mut Vec<Fact>,
    unmapped: &mut Vec<String>,
    kind: &str,
    r#ref: &str,
    title: &str,
    from: Option<&str>,
    to: Option<&str>,
    items: Vec<String>,
) -> Result<(), AiError> {
    if items.is_empty() {
        unmapped.push(r#ref.to_string());
        return Ok(());
    }
    let payload = serde_json::json!({
        "items": &items,
        "from": from,
        "to": to,
        "title": title,
    })
    .to_string();
    conn.execute(
        "INSERT INTO activity_events (source, type, ref, actor, occurred_at, payload)
         VALUES ('detector', ?1, ?2, NULL, ?3, ?4)",
        params![kind, r#ref, now, payload],
    )
    .map_err(db_err)?;
    facts.push(Fact {
        kind: kind.to_string(),
        r#ref: r#ref.to_string(),
        items,
    });
    Ok(())
}

fn save_snap(conn: &Connection, stories: &[StoryNow], prs: &[PrNow]) -> Result<(), AiError> {
    conn.execute("DELETE FROM ai_snapshot", [])
        .map_err(db_err)?;
    for story in stories {
        conn.execute(
            "INSERT INTO ai_snapshot (kind, key, state) VALUES ('story', ?1, ?2)",
            params![story.key, story.state],
        )
        .map_err(db_err)?;
    }
    for pr in prs {
        // ponytail: "state|merged_at" in one column on purpose; split it only when the snapshot grows another field
        conn.execute(
            "INSERT INTO ai_snapshot (kind, key, state) VALUES ('pr', ?1, ?2)",
            params![pr.id, format!("{}|{}", pr.state, pr.merged)],
        )
        .map_err(db_err)?;
    }
    Ok(())
}

fn detect_tx(conn: &Connection, now: &str) -> Result<Detection, AiError> {
    let stories = load_stories(conn)?;
    let prs = load_prs(conn)?;
    let story_snap = snap_map(conn, "story")?;
    let pr_snap = snap_map(conn, "pr")?;
    let mut facts = Vec::new();
    let mut unmapped = Vec::new();
    if !story_snap.is_empty() || !pr_snap.is_empty() {
        for story in &stories {
            match story_snap.get(&story.key) {
                None => {
                    let items = story_items(conn, &story.key, &story.epic)?;
                    note_change(
                        conn,
                        now,
                        &mut facts,
                        &mut unmapped,
                        "story_added",
                        &story.key,
                        &story.title,
                        None,
                        Some(story.state.as_str()),
                        items,
                    )?;
                }
                Some(prev) if prev != &story.state => {
                    let items = story_items(conn, &story.key, &story.epic)?;
                    note_change(
                        conn,
                        now,
                        &mut facts,
                        &mut unmapped,
                        "story_status",
                        &story.key,
                        &story.title,
                        Some(prev.as_str()),
                        Some(story.state.as_str()),
                        items,
                    )?;
                }
                _ => {}
            }
        }
        for pr in &prs {
            let current = format!("{}|{}", pr.state, pr.merged);
            match pr_snap.get(&pr.id) {
                None => {
                    let items = pr_items(conn, &pr.id, &pr.repo)?;
                    note_change(
                        conn,
                        now,
                        &mut facts,
                        &mut unmapped,
                        "pr_opened",
                        &pr.id,
                        &pr.title,
                        None,
                        Some(pr.state.as_str()),
                        items,
                    )?;
                }
                Some(prev) if prev != &current => {
                    let (old_state, old_merged) = prev.split_once('|').unwrap_or((prev.as_str(), ""));
                    let kind = if old_merged.is_empty() && !pr.merged.is_empty() {
                        Some("pr_merged")
                    } else if old_state != "closed" && pr.state == "closed" && pr.merged.is_empty()
                    {
                        Some("pr_closed")
                    } else {
                        None
                    };
                    if let Some(kind) = kind {
                        let items = pr_items(conn, &pr.id, &pr.repo)?;
                        let to = if kind == "pr_closed" {
                            "closed"
                        } else {
                            pr.state.as_str()
                        };
                        note_change(
                            conn,
                            now,
                            &mut facts,
                            &mut unmapped,
                            kind,
                            &pr.id,
                            &pr.title,
                            Some(old_state),
                            Some(to),
                            items,
                        )?;
                    }
                }
                _ => {}
            }
        }
    }
    save_snap(conn, &stories, &prs)?;
    Ok(Detection { facts, unmapped })
}

/// Compare stories and pull requests with the last snapshot. The write lock comes
/// first, so two overlapping runs take turns and the second finds nothing new.
pub fn detect(conn: &Connection, now: &str) -> Result<Detection, AiError> {
    // ponytail: full-table snapshot; per-source cursors only if the tables grow past a few thousand rows
    let tx = rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)
        .map_err(db_err)?;
    let detection = detect_tx(&tx, now)?;
    tx.commit().map_err(db_err)?;
    Ok(detection)
}

pub fn open_flags(conn: &Connection) -> Result<Vec<Flag>, AiError> {
    let mut stmt = conn
        .prepare(
            "SELECT id, item_id, field, reason, raised_at FROM ai_flags
             WHERE resolved_at IS NULL
             ORDER BY raised_at, id",
        )
        .map_err(db_err)?;
    let rows = stmt
        .query_map([], |row| {
            Ok(Flag {
                id: row.get(0)?,
                item_id: row.get(1)?,
                field: row.get(2)?,
                reason: row.get(3)?,
                raised_at: row.get(4)?,
            })
        })
        .map_err(db_err)?;
    rows.collect::<Result<Vec<_>, _>>().map_err(db_err)
}

pub fn resolve_flag(conn: &Connection, id: i64, now: &str) -> Result<bool, AiError> {
    let n = conn
        .execute(
            "UPDATE ai_flags SET resolved_at=?1 WHERE id=?2 AND resolved_at IS NULL",
            params![now, id],
        )
        .map_err(db_err)?;
    Ok(n == 1)
}

/// One `ai_runs` row the page can show. Ids and counts only: `detail` stays in the table.
#[derive(Debug, Clone, PartialEq)]
pub struct RunRow {
    pub id: i64,
    pub started_at: String,
    pub finished_at: Option<String>,
    pub mode: String,
    pub outcome: String,
    pub facts: i64,
    pub accepted: i64,
    pub rejected: i64,
    pub model: Option<String>,
}

impl RunRow {
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "id": self.id,
            "started_at": self.started_at,
            "finished_at": self.finished_at,
            "mode": self.mode,
            "outcome": self.outcome,
            "facts": self.facts,
            "accepted": self.accepted,
            "rejected": self.rejected,
            "model": self.model,
        })
    }
}

/// Newest runs. `limit` is clamped to 1..=100.
pub fn recent_runs(conn: &Connection, limit: i64) -> Result<Vec<RunRow>, AiError> {
    let limit = limit.clamp(1, 100);
    let mut stmt = conn
        .prepare(
            "SELECT id, started_at, finished_at, mode, outcome, facts, accepted, rejected, model
             FROM ai_runs
             ORDER BY started_at DESC, id DESC
             LIMIT ?1",
        )
        .map_err(db_err)?;
    let rows = stmt
        .query_map(params![limit], |row| {
            Ok(RunRow {
                id: row.get(0)?,
                started_at: row.get(1)?,
                finished_at: row.get(2)?,
                mode: row.get(3)?,
                outcome: row.get(4)?,
                facts: row.get(5)?,
                accepted: row.get(6)?,
                rejected: row.get(7)?,
                model: row.get(8)?,
            })
        })
        .map_err(db_err)?;
    rows.collect::<Result<Vec<_>, _>>().map_err(db_err)
}

/// One detector event for the "changed since" strip.
#[derive(Debug, Clone, PartialEq)]
pub struct ChangeRow {
    pub id: i64,
    pub r#type: String,
    pub r#ref: String,
    pub occurred_at: String,
    pub payload: serde_json::Value,
    pub item_ids: Vec<String>,
}

impl ChangeRow {
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "id": self.id,
            "type": self.r#type,
            "ref": self.r#ref,
            "occurred_at": self.occurred_at,
            "payload": self.payload,
            "item_ids": self.item_ids,
        })
    }
}

fn utc_stamp(text: &str) -> bool {
    // ponytail: digit shape and clock ranges, not days-per-month; a calendar check only if a bad date gets through
    let b = text.as_bytes();
    if b.len() != 20
        || b[4] != b'-'
        || b[7] != b'-'
        || b[10] != b'T'
        || b[13] != b':'
        || b[16] != b':'
        || b[19] != b'Z'
    {
        return false;
    }
    let digits = |range: std::ops::Range<usize>| b[range].iter().all(|c| c.is_ascii_digit());
    if !(digits(0..4) && digits(5..7) && digits(8..10) && digits(11..13) && digits(14..16) && digits(17..19))
    {
        return false;
    }
    let pair = |at: usize| (b[at] - b'0') as u16 * 10 + (b[at + 1] - b'0') as u16;
    (1..=12).contains(&pair(5))
        && (1..=31).contains(&pair(8))
        && pair(11) <= 23
        && pair(14) <= 59
        && pair(17) <= 59
}

fn items_for_event(conn: &Connection, kind: &str, r#ref: &str) -> Result<Vec<String>, AiError> {
    // ponytail: one source lookup per event (the strip stops at 200); a join if this gets slow
    match kind {
        "story_added" | "story_status" => {
            let epic: String = conn
                .query_row(
                    "SELECT epic_key FROM stories WHERE story_key=?1",
                    params![r#ref],
                    |row| row.get(0),
                )
                .optional()
                .map_err(db_err)?
                .unwrap_or_default();
            story_items(conn, r#ref, &epic)
        }
        "pr_opened" | "pr_merged" | "pr_closed" => {
            let repo: String = conn
                .query_row(
                    "SELECT repo FROM pull_requests WHERE id=?1",
                    params![r#ref],
                    |row| row.get(0),
                )
                .optional()
                .map_err(db_err)?
                .unwrap_or_default();
            pr_items(conn, r#ref, &repo)
        }
        _ => Ok(Vec::new()),
    }
}

/// Detector rows at or after `since` (`YYYY-MM-DDTHH:MM:SSZ`), newest first, at most 200.
/// `item_ids` uses the same story and pull-request mapping as `detect`.
pub fn changes_since(conn: &Connection, since: &str) -> Result<Vec<ChangeRow>, AiError> {
    let since = since.trim();
    if !utc_stamp(since) {
        return Err(AiError::Invalid(
            "since must be a UTC time like 2026-10-19T08:00:00Z.".into(),
        ));
    }
    let raw = {
        let mut stmt = conn
            .prepare(
                "SELECT id, type, ref, occurred_at, payload FROM activity_events
                 WHERE source='detector' AND occurred_at >= ?1
                 ORDER BY occurred_at DESC, id DESC
                 LIMIT 200",
            )
            .map_err(db_err)?;
        let rows = stmt
            .query_map(params![since], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                ))
            })
            .map_err(db_err)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(db_err)?
    };
    let mut out = Vec::with_capacity(raw.len());
    for (id, kind, r#ref, occurred_at, payload) in raw {
        let payload = serde_json::from_str(&payload).unwrap_or_else(|_| serde_json::json!({}));
        let item_ids = items_for_event(conn, &kind, &r#ref)?;
        out.push(ChangeRow {
            id,
            r#type: kind,
            r#ref,
            occurred_at,
            payload,
            item_ids,
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(crate::store::SCHEMA).unwrap();
        conn
    }

    fn invalid_note(err: Result<i64, AiError>) -> String {
        match err {
            Err(AiError::Invalid(msg)) => msg,
            other => panic!("expected Invalid, got {other:?}"),
        }
    }

    #[test]
    fn map_unmap_and_sources() {
        let conn = db();
        map_source(&conn, "item-1", "story", "S-2", "t1").unwrap();
        map_source(&conn, "item-1", "epic", "E-1", "t2").unwrap();
        map_source(&conn, "item-1", "repo", "org/name", "t3").unwrap();
        map_source(&conn, "item-1", "epic", "E-1", "t9").unwrap();
        assert_eq!(
            sources(&conn, "item-1").unwrap(),
            vec![
                ("epic".into(), "E-1".into()),
                ("repo".into(), "org/name".into()),
                ("story".into(), "S-2".into()),
            ]
        );
        let created: String = conn
            .query_row(
                "SELECT created_at FROM lu_item_sources
                 WHERE item_id='item-1' AND source_kind='epic' AND source_key='E-1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(created, "t2");
        assert!(unmap_source(&conn, "item-1", "story", "S-2").unwrap());
        assert!(!unmap_source(&conn, "item-1", "story", "S-2").unwrap());
        assert_eq!(sources(&conn, "item-1").unwrap().len(), 2);
        assert!(sources(&conn, "other").unwrap().is_empty());
        assert_eq!(
            map_source(&conn, "  ", "epic", "E-1", "t").unwrap_err(),
            AiError::Invalid("Item id is required.".into())
        );
        assert_eq!(
            map_source(&conn, "item-1", "epic", " ", "t").unwrap_err(),
            AiError::Invalid("Source key is required.".into())
        );
        assert_eq!(
            map_source(&conn, "item-1", "ticket", "E-1", "t").unwrap_err(),
            AiError::Invalid("Source kind must be epic, story, or repo.".into())
        );
    }

    #[test]
    fn each_note_kind_accepts_its_shape() {
        let conn = db();
        let ids = [
            add_note(&conn, "glossary", "Pelix => Felix", "2029-03-01T00:00:00Z").unwrap(),
            add_note(&conn, " fact ", "  Ships Friday.  ", "2029-03-02T00:00:00Z").unwrap(),
            add_note(&conn, "rule", "drop:ITEM-1", "2029-03-03T00:00:00Z").unwrap(),
            add_note(&conn, "rule", "no_end_date:ITEM-1", "2029-03-04T00:00:00Z").unwrap(),
            add_note(
                &conn,
                "rule",
                "never_say:going well",
                "2029-03-05T00:00:00Z",
            )
            .unwrap(),
            add_note(
                &conn,
                "escalate",
                "Partner asked twice.",
                "2029-03-06T00:00:00Z",
            )
            .unwrap(),
            add_note(&conn, "fact", &"x".repeat(2000), "2029-03-07T00:00:00Z").unwrap(),
        ];
        assert_eq!(
            add_note(&conn, " fact ", "  Ships Friday.  ", "later").unwrap(),
            ids[1]
        );
        let all = open_notes(&conn, None).unwrap();
        assert_eq!(all.iter().map(|note| note.id).collect::<Vec<_>>(), ids);
        assert_eq!(all[1].body, "Ships Friday.");
        assert_eq!(all.last().unwrap().body.len(), 2000);
        assert_eq!(open_notes(&conn, Some("rule")).unwrap().len(), 3);
        assert_eq!(open_notes(&conn, Some(" rule ")).unwrap().len(), 3);
        assert!(resolve_note(&conn, ids[1], "2029-03-08T00:00:00Z").unwrap());
        assert!(!resolve_note(&conn, ids[1], "2029-03-09T00:00:00Z").unwrap());
        assert!(!resolve_note(&conn, 999, "2029-03-09T00:00:00Z").unwrap());
        let still = open_notes(&conn, Some("fact")).unwrap();
        assert!(still.iter().all(|note| note.id != ids[1]));

        let shape = "A glossary note looks like \"heard => correct\" with both sides filled in.";
        let rule =
            "A rule looks like drop:<item_id>, no_end_date:<item_id>, or never_say:<phrase>.";
        let bad = [
            ("fact", "   ", "Write the note."),
            ("glossary", "Pelix", shape),
            ("glossary", " => Felix", shape),
            ("glossary", "Pelix => ", shape),
            ("rule", "drop:", rule),
            ("rule", "drop:   ", rule),
            ("rule", "no_end_date:", rule),
            ("rule", "never_say:", rule),
            ("rule", "keep:ITEM-1", rule),
            ("escalate", "", "Write the note."),
            (
                "todo",
                "x",
                "Note kind must be fact, glossary, rule, or escalate.",
            ),
        ];
        for (kind, body, msg) in bad {
            assert_eq!(invalid_note(add_note(&conn, kind, body, "t")), msg);
        }
        let too_long = invalid_note(add_note(&conn, "fact", &"y".repeat(2001), "t"));
        assert_eq!(too_long, "Keep the note to 2000 characters.");
    }

    #[test]
    fn glossary_replaces_whole_words_only() {
        let conn = db();
        let apply = |text: &str| apply_glossary(&conn, text).unwrap();
        add_note(&conn, "glossary", "Pelix => Felix", "t1").unwrap();
        assert_eq!(apply("Pelix said Pelixa"), "Felix said Pelixa");
        assert_eq!(apply("pelix said Pelix."), "pelix said Felix.");
        let said = add_note(&conn, "glossary", "said => wrote", "t2").unwrap();
        assert!(resolve_note(&conn, said, "t3").unwrap());
        assert_eq!(apply("Pelix said Pelixa"), "Felix said Pelixa");
        add_note(&conn, "glossary", "Pelix said => Felix wrote", "t4").unwrap();
        assert_eq!(apply("Pelix said Pelixa"), "Felix wrote Pelixa");
        add_note(&conn, "glossary", "Felix => Phil", "t5").unwrap();
        assert_eq!(apply("Pelix met Felix"), "Felix met Phil");
    }

    #[test]
    fn flag_raises_once_until_resolved() {
        let conn = db();
        assert!(raise_flag(&conn, "ITEM-1", "end_date", "missing", "t1").unwrap());
        assert!(!raise_flag(&conn, "ITEM-1", "end_date", "missing", "t2").unwrap());
        let flags = open_flags(&conn).unwrap();
        assert_eq!(flags.len(), 1);
        assert_eq!(flags[0].raised_at, "t1");
        assert!(resolve_flag(&conn, flags[0].id, "t3").unwrap());
        assert!(!resolve_flag(&conn, flags[0].id, "t4").unwrap());
        assert!(open_flags(&conn).unwrap().is_empty());
        assert!(raise_flag(&conn, "ITEM-1", "end_date", "missing", "t5").unwrap());
        assert_eq!(open_flags(&conn).unwrap().len(), 1);
        assert_eq!(
            raise_flag(&conn, "  ", "  ", "  ", "t6").unwrap_err(),
            AiError::Invalid("Item id, field and reason are required.".into())
        );
    }

    #[test]
    fn one_open_note_per_kind_and_body() {
        let conn = db();
        let id = add_note(&conn, "fact", "Ships Friday.", "t1").unwrap();
        let again =
            "INSERT INTO ai_notes (kind, body, created_at) VALUES ('fact', 'Ships Friday.', 't2')";
        assert!(conn.execute(again, []).is_err());
        assert!(resolve_note(&conn, id, "t3").unwrap());
        conn.execute(again, []).unwrap();
        // A database from before the index may hold repeats. Applying the schema settles
        // them (the oldest stays open) instead of failing to build the index.
        conn.execute_batch("DROP INDEX ai_notes_open").unwrap();
        conn.execute(again, []).unwrap();
        conn.execute_batch(crate::store::SCHEMA).unwrap();
        assert_eq!(open_notes(&conn, None).unwrap().len(), 1);
    }

    fn count(conn: &Connection, sql: &str) -> i64 {
        conn.query_row(sql, [], |row| row.get(0)).unwrap()
    }

    #[test]
    fn detect_baseline_then_mapped_and_unmapped_changes() {
        let conn = db();
        conn.execute_batch(
            "INSERT INTO epics (epic_key, board, summary) VALUES
               ('E-1', 'b', 'Epic one'),
               ('E-2', 'b', 'Epic two'),
               ('E-3', 'b', 'Epic three');
             INSERT INTO stories (story_key, epic_key, summary, status) VALUES
               ('S-1', 'E-1', 'Ship it', 'todo'),
               ('S-2', 'E-2', 'Linked work', 'todo'),
               ('S-3', 'E-3', 'Rejected work', 'todo');
             INSERT INTO pull_requests (id, repo, number, title, state) VALUES
               ('pr-link', 'org/other', 2, 'Link me', 'open'),
               ('pr-merge', 'org/app', 1, 'Merge me', 'open'),
               ('pr-rej', 'org/other', 3, 'Reject me', 'open');
             INSERT INTO pr_story_links (pr_id, story_key, score, method, status, active) VALUES
               ('pr-link', 'S-2', 1, 'manual', 'accepted', 1),
               ('pr-rej', 'S-3', 1, 'manual', 'rejected', 1);",
        )
        .unwrap();
        map_source(&conn, "item-epic", "epic", "E-1", "t").unwrap();
        map_source(&conn, "item-repo", "repo", "org/app", "t").unwrap();
        map_source(&conn, "item-story", "story", "S-2", "t").unwrap();
        map_source(&conn, "item-nope", "story", "S-3", "t").unwrap();

        let first = detect(&conn, "t0").unwrap();
        assert!(first.facts.is_empty());
        assert!(first.unmapped.is_empty());
        assert_eq!(count(&conn, "SELECT COUNT(*) FROM activity_events"), 0);
        assert!(count(&conn, "SELECT COUNT(*) FROM ai_snapshot") > 0);

        conn.execute_batch(
            "UPDATE stories SET status='doing' WHERE story_key='S-1';
             INSERT INTO epics (epic_key, board, summary) VALUES ('E-9', 'b', 'Loose');
             INSERT INTO stories (story_key, epic_key, summary, status)
               VALUES ('S-9', 'E-9', 'Loose', 'todo');
             UPDATE pull_requests SET state='closed', merged_at='2026-04-02'
               WHERE id IN ('pr-merge', 'pr-link', 'pr-rej');",
        )
        .unwrap();
        let second = detect(&conn, "t1").unwrap();
        assert_eq!(
            second.facts,
            vec![
                Fact {
                    kind: "story_status".into(),
                    r#ref: "S-1".into(),
                    items: vec!["item-epic".into()],
                },
                Fact {
                    kind: "pr_merged".into(),
                    r#ref: "pr-link".into(),
                    items: vec!["item-story".into()],
                },
                Fact {
                    kind: "pr_merged".into(),
                    r#ref: "pr-merge".into(),
                    items: vec!["item-repo".into()],
                },
            ]
        );
        assert_eq!(
            second.unmapped,
            vec!["S-9".to_string(), "pr-rej".to_string()]
        );
        let payload: String = conn
            .query_row(
                "SELECT payload FROM activity_events WHERE type='story_status' AND ref='S-1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let body: serde_json::Value = serde_json::from_str(&payload).unwrap();
        assert_eq!(body["from"], "todo");
        assert_eq!(body["to"], "doing");
        assert_eq!(body["title"], "Ship it");
        assert_eq!(body["items"], serde_json::json!(["item-epic"]));
        assert_eq!(
            count(
                &conn,
                "SELECT COUNT(*) FROM activity_events WHERE ref IN ('S-9', 'pr-rej')"
            ),
            0
        );

        let third = detect(&conn, "t2").unwrap();
        assert!(third.facts.is_empty());
        assert!(third.unmapped.is_empty());
        assert_eq!(count(&conn, "SELECT COUNT(*) FROM activity_events"), 3);
    }

    #[test]
    fn recent_runs_are_newest_first_without_detail() {
        let conn = db();
        for (at, facts) in [("2026-10-19T08:00:00Z", 1i64), ("2026-10-19T07:00:00Z", 2)] {
            conn.execute(
                "INSERT INTO ai_runs (started_at, finished_at, mode, outcome, facts, accepted, rejected, model, detail)
                 VALUES (?1, ?1, 'leadership', 'drafted', ?2, 1, 0, 'fake', 'LEAK')",
                rusqlite::params![at, facts],
            )
            .unwrap();
        }
        conn.execute(
            "INSERT INTO ai_runs (started_at, finished_at, mode, outcome, facts, accepted, rejected, model, detail)
             VALUES ('2026-10-19T09:00:00Z', NULL, 'leadership', 'unchanged', 3, 0, 4, NULL, 'LEAK')",
            [],
        )
        .unwrap();
        let rows = recent_runs(&conn, 2).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].started_at, "2026-10-19T09:00:00Z");
        assert_eq!(rows[0].facts, 3);
        assert_eq!(rows[0].rejected, 4);
        assert!(rows[0].finished_at.is_none());
        assert!(rows[0].model.is_none());
        assert_eq!(rows[1].started_at, "2026-10-19T08:00:00Z");
        assert_eq!(rows[1].model.as_deref(), Some("fake"));
        let body = rows[0].to_json();
        assert!(body.get("detail").is_none());
        assert!(!body.to_string().contains("LEAK"));
        assert_eq!(recent_runs(&conn, 1000).unwrap().len(), 3);
    }

    #[test]
    fn changes_since_keeps_the_window_and_the_detector_mapping() {
        let conn = db();
        assert_eq!(
            changes_since(&conn, "yesterday").unwrap_err(),
            AiError::Invalid("since must be a UTC time like 2026-10-19T08:00:00Z.".into())
        );
        assert_eq!(
            changes_since(&conn, "2026-10-19T08:00:00").unwrap_err(),
            AiError::Invalid("since must be a UTC time like 2026-10-19T08:00:00Z.".into())
        );
        conn.execute_batch(
            "INSERT INTO epics (epic_key, board, summary) VALUES
               ('E-1', 'b', 'Epic one'),
               ('E-9', 'b', 'Rejected');
             INSERT INTO stories (story_key, epic_key, summary, status) VALUES
               ('S-1', 'E-1', 'Ship it', 'todo'),
               ('S-REJ', 'E-9', 'Nope', 'todo');
             INSERT INTO pull_requests (id, repo, number, title, state) VALUES
               ('pr-9', 'org/app', 9, 'Merge me', 'open');
             INSERT INTO pr_story_links (pr_id, story_key, score, method, status, active) VALUES
               ('pr-9', 'S-1', 1, 'manual', 'accepted', 1),
               ('pr-9', 'S-REJ', 1, 'manual', 'rejected', 1);",
        )
        .unwrap();
        map_source(&conn, "item-epic", "epic", "E-1", "t").unwrap();
        map_source(&conn, "item-story", "story", "S-1", "t").unwrap();
        map_source(&conn, "item-repo", "repo", "org/app", "t").unwrap();
        map_source(&conn, "item-rej", "story", "S-REJ", "t").unwrap();
        conn.execute_batch(
            "INSERT INTO activity_events (source, type, ref, occurred_at, payload) VALUES
               ('detector', 'story_status', 'S-1', '2026-10-19T07:00:00Z', '{\"items\":[\"nope\"],\"title\":\"Early\"}'),
               ('detector', 'story_status', 'S-1', '2026-10-19T08:00:00Z', '{\"items\":[\"nope\"],\"title\":\"Ship\"}'),
               ('detector', 'pr_merged', 'pr-9', '2026-10-19T09:00:00Z', '{\"items\":[\"nope\"],\"title\":\"Merge\"}'),
               ('github', 'push', 'pr-9', '2026-10-19T10:00:00Z', '{}');",
        )
        .unwrap();
        let rows = changes_since(&conn, "2026-10-19T08:00:00Z").unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].r#type, "pr_merged");
        assert_eq!(rows[0].r#ref, "pr-9");
        assert_eq!(rows[0].payload["title"], "Merge");
        assert_eq!(
            rows[0].item_ids,
            vec![
                "item-epic".to_string(),
                "item-repo".to_string(),
                "item-story".to_string()
            ]
        );
        assert_eq!(rows[1].r#type, "story_status");
        assert_eq!(rows[1].occurred_at, "2026-10-19T08:00:00Z");
        assert_eq!(
            rows[1].item_ids,
            vec!["item-epic".to_string(), "item-story".to_string()]
        );
        assert!(!rows[1].to_json().to_string().contains("Early"));
    }

    #[test]
    fn changes_since_stops_at_200() {
        let conn = db();
        for _ in 0..201 {
            conn.execute(
                "INSERT INTO activity_events (source, type, ref, occurred_at, payload)
                 VALUES ('detector', 'note', 'x', '2026-10-19T08:00:00Z', '{}')",
                [],
            )
            .unwrap();
        }
        let rows = changes_since(&conn, "2026-10-19T08:00:00Z").unwrap();
        assert_eq!(rows.len(), 200);
        assert_eq!(rows[0].id, 201);
        assert_eq!(rows[199].id, 2);
        assert!(rows[0].item_ids.is_empty());
    }
}
