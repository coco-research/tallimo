//! One AI pass over the open partner brief. Detector facts become FYI items.
//! A needs item is created only from an owner escalate note. The model is a trait object.

use crate::ai::{self, AiError, Note};
use crate::airun::RunSummary;
use crate::briefs::{self, Brief, BriefError, BriefItemInput};
use crate::draft::{self, DraftRequest, ItemRequest, Model, SourceFact, Verdict};
use crate::updates::{self, UpdateError};
use rusqlite::{params, Connection, OptionalExtension};
use std::collections::{HashMap, HashSet};

/// Stored in `ai_runs.mode`. The leadership pass keeps its own cursor.
const MODE: &str = "brief";

fn db_err(err: impl ToString) -> AiError {
    AiError::Db(err.to_string())
}

fn clip(text: &str) -> String {
    text.chars().take(200).collect()
}

fn update_err(err: UpdateError) -> AiError {
    match err {
        UpdateError::Db(msg) => AiError::Db(msg),
        other => db_err(format!("{other:?}")),
    }
}

fn brief_db(err: BriefError) -> AiError {
    match err {
        BriefError::Db(msg) => AiError::Db(msg),
        other => db_err(format!("{other:?}")),
    }
}

fn is_constraint(err: &rusqlite::Error) -> bool {
    matches!(
        err,
        rusqlite::Error::SqliteFailure(e, _)
            if e.code == rusqlite::ErrorCode::ConstraintViolation
    )
}

/// One plain sentence for a detected change. Same shape as the leadership pass,
/// so a number in the line is a number in the cited source.
fn sentence(r#ref: &str, payload: &str) -> String {
    let body: serde_json::Value = serde_json::from_str(payload).unwrap_or_default();
    let part = |key: &str, blank: &str| match body[key].as_str().map(str::trim) {
        Some(text) if !text.is_empty() => text.to_string(),
        _ => blank.to_string(),
    };
    format!(
        "{} {}: {} -> {}",
        r#ref,
        part("title", "untitled"),
        part("from", "new"),
        part("to", "none"),
    )
    .replace(['\u{2013}', '\u{2014}'], "-")
}

fn payload_items(payload: &str) -> Vec<String> {
    let body: serde_json::Value = serde_json::from_str(payload).unwrap_or_default();
    body["items"]
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

fn lines_of(text: &str) -> Vec<String> {
    if text.is_empty() {
        Vec::new()
    } else {
        text.split('\n').map(str::to_string).collect()
    }
}

struct DetectorEvent {
    id: i64,
    r#ref: String,
    payload: String,
}

/// Fields this pass may not clobber. Compared again at write time.
struct Held {
    kind: String,
    position: i64,
    headline: String,
    what: String,
    why: String,
    action: String,
    at_this_stage: i64,
}

/// Newest detector event a drafted or unchanged brief run already covered.
/// `None` when this mode has not recorded a cursor.
fn watermark(conn: &Connection) -> Result<Option<i64>, AiError> {
    let row = conn
        .query_row(
            "SELECT last_event FROM ai_runs
             WHERE mode=?1 AND outcome IN ('drafted', 'unchanged')
             ORDER BY id DESC LIMIT 1",
            params![MODE],
            |row| row.get::<_, Option<i64>>(0),
        )
        .optional()
        .map_err(db_err)?;
    Ok(row.flatten())
}

fn open_events(conn: &Connection, after: i64) -> Result<Vec<DetectorEvent>, AiError> {
    let mut stmt = conn
        .prepare(
            "SELECT id, ref, payload FROM activity_events
             WHERE source='detector' AND id > ?1
             ORDER BY id",
        )
        .map_err(db_err)?;
    let rows = stmt
        .query_map(params![after], |row| {
            Ok(DetectorEvent {
                id: row.get(0)?,
                r#ref: row.get(1)?,
                payload: row.get(2)?,
            })
        })
        .map_err(db_err)?;
    rows.collect::<Result<Vec<_>, _>>().map_err(db_err)
}

fn flat_facts(items: &[ItemRequest]) -> Vec<SourceFact> {
    let mut facts = Vec::new();
    for item in items {
        for fact in &item.facts {
            if facts.iter().any(|have: &SourceFact| have.id == fact.id) {
                continue;
            }
            facts.push(fact.clone());
        }
    }
    facts
}

fn open_brief(conn: &Connection) -> Result<Option<Brief>, AiError> {
    Ok(briefs::list(conn, true)
        .map_err(brief_db)?
        .into_iter()
        .find(|brief| brief.sent_at.is_none()))
}

fn pending_notes(conn: &Connection, brief: Option<&Brief>) -> Result<Vec<Note>, AiError> {
    let have: HashSet<String> = brief
        .map(|brief| {
            brief
                .items
                .iter()
                .map(|item| item.item_id.clone())
                .collect()
        })
        .unwrap_or_default();
    let notes = ai::open_notes(conn, Some("escalate"))?;
    Ok(notes
        .into_iter()
        .filter(|note| !have.contains(&format!("esc-{}", note.id)))
        .collect())
}

/// Open leadership draft when one exists, otherwise the newest entry.
/// An open draft does not fall through to the sent copy.
fn director_titles(conn: &Connection) -> Result<HashMap<String, String>, AiError> {
    let entries = updates::list(conn, true).map_err(update_err)?;
    let page = entries
        .iter()
        .find(|entry| entry.sent_at.is_none())
        .or_else(|| entries.first());
    let Some(page) = page else {
        return Ok(HashMap::new());
    };
    Ok(page
        .items
        .iter()
        .map(|item| (item.item_id.clone(), item.title.clone()))
        .collect())
}

fn hold(brief: &Brief) -> HashMap<String, Held> {
    brief
        .items
        .iter()
        .map(|item| {
            (
                item.item_id.clone(),
                Held {
                    kind: item.kind.clone(),
                    position: item.position,
                    headline: item.headline.clone(),
                    what: item.what.clone(),
                    why: item.why.clone(),
                    action: item.action.clone(),
                    at_this_stage: item.at_this_stage,
                },
            )
        })
        .collect()
}

fn changed(result: Result<usize, rusqlite::Error>) -> Result<bool, AiError> {
    match result {
        Ok(n) => Ok(n > 0),
        Err(err) if is_constraint(&err) => Ok(false),
        Err(err) => Err(AiError::Db(err.to_string())),
    }
}

/// Write `what` for one accepted line. A needs row keeps headline, why, and action.
/// A new row is an FYI with an empty action: the editor's upsert refuses that, and
/// this pass must not invent a support line.
fn apply_what(
    conn: &Connection,
    entry_id: i64,
    held: Option<&Held>,
    headline: &str,
    item_id: &str,
    what: &str,
) -> Result<bool, AiError> {
    let Some(row) = held else {
        if what.trim().is_empty() || headline.trim().is_empty() {
            return Ok(false);
        }
        return changed(conn.execute(
            "INSERT INTO a1_items (
                entry_id, item_id, kind, position, headline, what, why, action, at_this_stage
             )
             SELECT ?1, ?2, 'fyi',
                COALESCE((SELECT MAX(position) FROM a1_items WHERE entry_id=?1 AND kind='fyi'), -1) + 1,
                ?3, ?4, '', '', 0
             WHERE EXISTS (SELECT 1 FROM a1_entries WHERE id=?1 AND sent_at IS NULL)
               AND NOT EXISTS (SELECT 1 FROM a1_items WHERE entry_id=?1 AND item_id=?2)",
            params![entry_id, item_id, headline, what],
        ));
    };
    if what.trim().is_empty() {
        return Ok(false);
    }
    let same = row.what == what && (row.kind != "fyi" || row.headline == headline);
    if same {
        return Ok(true);
    }
    if row.kind == "needs" {
        return changed(conn.execute(
            "UPDATE a1_items SET what=?1
             WHERE entry_id=?2 AND item_id=?3 AND kind='needs'
               AND headline=?4 AND what=?5 AND why=?6 AND action=?7
               AND at_this_stage=?8 AND position=?9
               AND EXISTS (SELECT 1 FROM a1_entries WHERE id=?2 AND sent_at IS NULL)",
            params![
                what,
                entry_id,
                item_id,
                row.headline,
                row.what,
                row.why,
                row.action,
                row.at_this_stage,
                row.position,
            ],
        ));
    }
    if headline.trim().is_empty() {
        return Ok(false);
    }
    changed(conn.execute(
        "UPDATE a1_items SET what=?1, headline=?2
         WHERE entry_id=?3 AND item_id=?4 AND kind='fyi'
           AND headline=?5 AND what=?6 AND why=?7 AND action=?8
           AND at_this_stage=?9 AND position=?10
           AND EXISTS (SELECT 1 FROM a1_entries WHERE id=?3 AND sent_at IS NULL)",
        params![
            what,
            headline,
            entry_id,
            item_id,
            row.headline,
            row.what,
            row.why,
            row.action,
            row.at_this_stage,
            row.position,
        ],
    ))
}

fn on_open_draft(conn: &Connection, item_id: &str) -> Result<bool, AiError> {
    let found = conn
        .query_row(
            "SELECT 1 FROM a1_items i
             JOIN a1_entries e ON e.id = i.entry_id
             WHERE e.sent_at IS NULL AND i.item_id=?1",
            params![item_id],
            |_| Ok(true),
        )
        .optional()
        .map_err(db_err)?;
    Ok(found.unwrap_or(false))
}

fn next_position(conn: &Connection, kind: &str) -> Result<i64, AiError> {
    let max = conn
        .query_row(
            "SELECT MAX(i.position) FROM a1_items i
             JOIN a1_entries e ON e.id = i.entry_id
             WHERE e.sent_at IS NULL AND i.kind=?1",
            params![kind],
            |row| row.get::<_, Option<i64>>(0),
        )
        .map_err(db_err)?;
    Ok(max.unwrap_or(-1) + 1)
}

fn write_escalations(
    conn: &Connection,
    notes: &[Note],
    accepted: &mut usize,
    rejected: &mut usize,
) -> Result<(), AiError> {
    for note in notes {
        let item_id = format!("esc-{}", note.id);
        if on_open_draft(conn, &item_id)? {
            continue;
        }
        let position = next_position(conn, "needs")?;
        // upsert_item rejects an empty action. The note body is only a placeholder
        // so the row can be saved; the owner still writes the real support line.
        let action = note.body.clone();
        let item = BriefItemInput {
            item_id,
            kind: "needs".into(),
            position,
            headline: note.body.clone(),
            what: note.body.clone(),
            why: String::new(),
            action,
            at_this_stage: 0,
        };
        match briefs::upsert_item(conn, &item) {
            Ok(()) => *accepted += 1,
            // The open-draft check above raced a second writer. The row is there, so this is a duplicate.
            Err(BriefError::Db(msg)) => match on_open_draft(conn, &item.item_id) {
                Ok(true) => *rejected += 1,
                Ok(false) => return Err(AiError::Db(msg)),
                Err(err) => return Err(err),
            },
            Err(_) => *rejected += 1,
        }
    }
    Ok(())
}

fn log_run(
    conn: &Connection,
    now: &str,
    model: Option<&str>,
    run: &RunSummary,
    detail: Option<&str>,
    last_event: Option<i64>,
) -> Result<(), AiError> {
    conn.execute(
        "INSERT INTO ai_runs (started_at, finished_at, mode, outcome, facts, accepted, rejected, model, detail, last_event)
         VALUES (?1, ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            now,
            MODE,
            run.outcome,
            run.facts as i64,
            run.accepted as i64,
            run.rejected as i64,
            model,
            detail,
            last_event,
        ],
    )
    .map_err(db_err)?;
    Ok(())
}

enum PassDone {
    /// No brief or flag writes. The caller logs the `ai_runs` row.
    Quiet(RunSummary, Option<String>, Option<i64>, bool),
    /// Brief, flag, and `ai_runs` writes are already committed.
    Committed(RunSummary),
}

fn quiet(done: (RunSummary, Option<String>, Option<i64>, bool)) -> Result<PassDone, AiError> {
    Ok(PassDone::Quiet(done.0, done.1, done.2, done.3))
}

/// Writes of one pass. Drop rolls them back.
enum PassTx<'a> {
    Own(rusqlite::Transaction<'a>),
    Nested(NestedSp<'a>),
}

struct NestedSp<'a> {
    conn: &'a Connection,
    open: bool,
}

impl Drop for NestedSp<'_> {
    fn drop(&mut self) {
        if !self.open {
            return;
        }
        // Statement failed inside the savepoint. Undo it, then release, so the caller's transaction stays open.
        let _ = self.conn.execute_batch("ROLLBACK TO brief_pass");
        let _ = self.conn.execute_batch("RELEASE brief_pass");
        self.open = false;
    }
}

impl PassTx<'_> {
    fn conn(&self) -> &Connection {
        match self {
            PassTx::Own(tx) => &*tx,
            PassTx::Nested(sp) => sp.conn,
        }
    }

    fn commit(self) -> Result<(), AiError> {
        match self {
            PassTx::Own(tx) => tx.commit().map_err(db_err),
            PassTx::Nested(mut sp) => {
                sp.conn.execute_batch("RELEASE brief_pass").map_err(db_err)?;
                sp.open = false;
                Ok(())
            }
        }
    }
}

/// BEGIN when this connection is idle. A caller that already has a transaction
/// gets a savepoint: `Connection::savepoint` needs `&mut`, and this pass has `&Connection`.
fn begin_pass(conn: &Connection) -> Result<PassTx<'_>, AiError> {
    if conn.is_autocommit() {
        conn.unchecked_transaction().map(PassTx::Own).map_err(db_err)
    } else {
        conn.execute_batch("SAVEPOINT brief_pass").map_err(db_err)?;
        Ok(PassTx::Nested(NestedSp { conn, open: true }))
    }
}

fn pass(
    conn: &Connection,
    model: &dyn Model,
    model_name: &str,
    now: &str,
    after: i64,
) -> Result<PassDone, AiError> {
    ai::detect(conn, now)?;
    let events = open_events(conn, after)?;
    let found = events.len();
    let covered = events.last().map(|event| event.id).unwrap_or(after);
    let finish = |outcome: &str,
                  accepted: usize,
                  rejected: usize,
                  detail: Option<&str>,
                  asked: bool| {
        let run = RunSummary {
            outcome: outcome.to_string(),
            facts: found,
            accepted,
            rejected,
        };
        let mark = matches!(outcome, "drafted" | "unchanged").then_some(covered);
        (run, detail.map(str::to_string), mark, asked)
    };

    let brief = open_brief(conn)?;
    let pending = pending_notes(conn, brief.as_ref())?;
    if events.is_empty() && pending.is_empty() {
        return quiet(finish("unchanged", 0, 0, None, false));
    }
    let Some(brief) = brief else {
        return quiet(finish("no_draft", 0, 0, None, false));
    };

    let titles = director_titles(conn)?;
    let held = hold(&brief);
    let mut items: Vec<ItemRequest> = Vec::new();
    for event in &events {
        let fact = SourceFact {
            id: format!("ev-{}", event.id),
            text: sentence(&event.r#ref, &event.payload),
        };
        for id in payload_items(&event.payload) {
            let Some(title) = titles.get(&id) else {
                continue;
            };
            if title.trim().is_empty() {
                continue;
            }
            if let Some(slot) = items.iter_mut().find(|item| item.item_id == id) {
                slot.facts.push(fact.clone());
                continue;
            }
            // Current `what` is the kept lines, the same role as `completed` on the director pass.
            // why and action are not lines on this request.
            let completed = held
                .get(&id)
                .map(|row| lines_of(&row.what))
                .unwrap_or_default();
            items.push(ItemRequest {
                item_id: id,
                facts: vec![fact.clone()],
                completed,
                next: Vec::new(),
                future: Vec::new(),
            });
        }
    }
    if items.is_empty() && pending.is_empty() {
        return quiet(finish(
            "unchanged",
            0,
            0,
            Some("no changed item is on the director page"),
            false,
        ));
    }

    let mut accepted = 0;
    let mut rejected = 0;
    let mut asked = false;
    let mut model_error: Option<String> = None;
    // The model runs before the write transaction. Its lines are skipped on error.
    // An owner escalation does not need the model and is still written below.
    let staged = if items.is_empty() {
        None
    } else {
        asked = true;
        let facts = flat_facts(&items);
        let request = DraftRequest {
            items: items.clone(),
        };
        match model.draft(&request) {
            Ok(patch) => Some((patch, facts, request)),
            Err(text) => {
                model_error = Some(clip(&text));
                None
            }
        }
    };

    let tx = begin_pass(conn)?;
    let written = (|| -> Result<RunSummary, AiError> {
        let db = tx.conn();
        if let Some((patch, facts, request)) = &staged {
            let asked_ids: HashSet<&str> = items.iter().map(|item| item.item_id.as_str()).collect();
            for verdict in draft::validate(db, patch, facts, &request.items, now)? {
                let Verdict::Accepted(line) = verdict else {
                    rejected += 1;
                    continue;
                };
                if line.field != "completed" || !asked_ids.contains(line.item_id.as_str()) {
                    rejected += 1;
                    continue;
                }
                let what = line.lines.join("\n");
                let headline = titles.get(&line.item_id).map(String::as_str).unwrap_or("");
                let wrote = apply_what(
                    db,
                    brief.id,
                    held.get(&line.item_id),
                    headline,
                    &line.item_id,
                    &what,
                )?;
                if wrote {
                    accepted += 1;
                } else {
                    rejected += 1;
                }
            }
        }
        write_escalations(db, &pending, &mut accepted, &mut rejected)?;
        // `unchanged` is only an empty pass: no facts and no pending note.
        // A model error stays `failed` even when the escalation was written.
        let outcome = if model_error.is_some() {
            "failed"
        } else if asked || accepted > 0 || rejected > 0 {
            "drafted"
        } else {
            "unchanged"
        };
        let (summary, detail, last_event, asked_model) = finish(
            outcome,
            accepted,
            rejected,
            model_error.as_deref(),
            asked,
        );
        log_run(
            db,
            now,
            asked_model.then_some(model_name),
            &summary,
            detail.as_deref(),
            last_event,
        )?;
        Ok(summary)
    })();
    let summary = match written {
        Ok(summary) => summary,
        Err(err) => {
            // Brief and flag writes die with the transaction. The caller logs `failed` after this.
            drop(tx);
            return Err(err);
        }
    };
    tx.commit()?;
    Ok(PassDone::Committed(summary))
}

/// Detect changes since the last brief run, draft FYI lines for director-page items,
/// validate, apply, and log one `ai_runs` row with mode `brief`. Ids and counts only.
/// The first run has no cursor, so it starts at detector rows already stored and only
/// drafts facts this pass detects. Later runs keep that cursor.
/// No facts and no pending escalate note means no model call. A model error is logged
/// as `failed` and returns Ok. A pending escalate note is still written on that pass.
/// A database error rolls this pass's brief and flag writes back, then returns Err after
/// a best-effort `failed` row.
pub fn brief_run_once(
    conn: &Connection,
    model: &dyn Model,
    model_name: &str,
    now: &str,
) -> Result<RunSummary, AiError> {
    // Read before detect. A missing brief cursor uses this max, so months of detector
    // rows are not drafted, and facts this pass writes (higher ids) still are.
    let seen = conn
        .query_row(
            "SELECT MAX(id) FROM activity_events WHERE source='detector'",
            [],
            |row| row.get::<_, Option<i64>>(0),
        )
        .map_err(db_err)?
        .unwrap_or(0);
    let after = watermark(conn)?.unwrap_or(seen);
    match pass(conn, model, model_name, now, after) {
        Ok(PassDone::Quiet(summary, detail, last_event, asked)) => {
            log_run(
                conn,
                now,
                asked.then_some(model_name),
                &summary,
                detail.as_deref(),
                last_event,
            )?;
            Ok(summary)
        }
        Ok(PassDone::Committed(summary)) => Ok(summary),
        Err(err) => {
            let (AiError::Invalid(msg) | AiError::Db(msg)) = &err;
            let failed = RunSummary {
                outcome: "failed".to_string(),
                facts: 0,
                accepted: 0,
                rejected: 0,
            };
            let _ = log_run(conn, now, None, &failed, Some(clip(msg).as_str()), None);
            Err(err)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::draft::FakeModel;
    use crate::updates::ItemInput;

    struct NoModel;

    impl Model for NoModel {
        fn draft(&self, _: &DraftRequest) -> Result<Vec<crate::draft::PatchLine>, String> {
            panic!("the model was called");
        }
    }

    fn db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(crate::store::SCHEMA).unwrap();
        conn.execute_batch(
            "INSERT INTO epics (epic_key, board, summary) VALUES ('E-1', 'b', 'Epic one');
             INSERT INTO stories (story_key, epic_key, summary, status)
               VALUES ('S-1', 'E-1', 'Ship it', 'todo');",
        )
        .unwrap();
        ai::map_source(&conn, "pay-1", "story", "S-1", "t").unwrap();
        conn
    }

    fn page_item(item_id: &str, title: &str) -> ItemInput {
        ItemInput {
            item_id: item_id.into(),
            category: "Now".into(),
            position: 0,
            title: title.into(),
            priority: 0,
            start_date: None,
            end_date: None,
            pct: None,
            capacity: None,
            completed: vec![],
            next: vec![],
            future: vec![],
        }
    }

    /// Open director page, open brief, and the baseline snapshot. No facts yet.
    fn ready() -> Connection {
        let conn = db();
        updates::next_from_last(&conn, "2029-03-16", "t").unwrap();
        updates::upsert_item(&conn, &page_item("pay-1", "Payments vendor")).unwrap();
        briefs::next_from_last(&conn, "2029-03-16", "t").unwrap();
        brief_run_once(&conn, &NoModel, "fake", "t0").unwrap();
        conn
    }

    fn move_story(conn: &Connection) {
        conn.execute("UPDATE stories SET status='doing'", []).unwrap();
    }

    fn summary(outcome: &str, facts: usize, accepted: usize, rejected: usize) -> RunSummary {
        RunSummary {
            outcome: outcome.into(),
            facts,
            accepted,
            rejected,
        }
    }

    fn open_item(conn: &Connection, item_id: &str) -> (String, String, String, String, String, i64) {
        conn.query_row(
            "SELECT kind, headline, what, why, action, at_this_stage
             FROM a1_items i
             JOIN a1_entries e ON e.id = i.entry_id
             WHERE e.sent_at IS NULL AND i.item_id=?1",
            params![item_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?)),
        )
        .unwrap()
    }

    fn count_where(conn: &Connection, sql: &str) -> i64 {
        conn.query_row(sql, [], |row| row.get(0)).unwrap()
    }

    #[test]
    fn no_escalate_note_means_no_needs_item() {
        let conn = ready();
        move_story(&conn);
        let done = brief_run_once(&conn, &FakeModel, "fake", "t1").unwrap();
        assert_eq!(done, summary("drafted", 1, 1, 0));
        assert_eq!(count_where(&conn, "SELECT COUNT(*) FROM a1_items WHERE kind='needs'"), 0);
        let (kind, _, _, _, action, _) = open_item(&conn, "pay-1");
        assert_eq!(kind, "fyi");
        assert_eq!(action, "");
    }

    #[test]
    fn an_escalate_note_creates_one_needs_item_and_a_second_pass_does_not() {
        let conn = ready();
        let id = ai::add_note(&conn, "escalate", "Partner asked twice.", "t1").unwrap();
        let done = brief_run_once(&conn, &NoModel, "fake", "t2").unwrap();
        assert_eq!(done, summary("drafted", 0, 1, 0));
        let item_id = format!("esc-{id}");
        let (kind, headline, what, why, action, stage) = open_item(&conn, &item_id);
        assert_eq!(kind, "needs");
        assert_eq!(headline, "Partner asked twice.");
        assert_eq!(what, "Partner asked twice.");
        assert_eq!(why, "");
        assert_eq!(action, "Partner asked twice.");
        assert_eq!(stage, 0);
        assert_eq!(count_where(&conn, "SELECT COUNT(*) FROM a1_items WHERE kind='needs'"), 1);
        conn.execute(
            "UPDATE a1_items SET action='Ask by Friday.' WHERE item_id=?1",
            params![item_id],
        )
        .unwrap();
        let again = brief_run_once(&conn, &NoModel, "fake", "t3").unwrap();
        assert_eq!(again, summary("unchanged", 0, 0, 0));
        assert_eq!(count_where(&conn, "SELECT COUNT(*) FROM a1_items"), 1);
        let (_, _, _, _, action, _) = open_item(&conn, &item_id);
        assert_eq!(action, "Ask by Friday.");
        let model: Option<String> = conn
            .query_row(
                "SELECT model FROM ai_runs WHERE outcome='drafted'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(model, None);
    }

    struct ErrModel;

    impl Model for ErrModel {
        fn draft(&self, _: &DraftRequest) -> Result<Vec<crate::draft::PatchLine>, String> {
            Err("model down".into())
        }
    }

    #[test]
    fn a_model_error_still_writes_the_pending_escalation() {
        let conn = ready();
        move_story(&conn);
        let id = ai::add_note(&conn, "escalate", "Partner asked twice.", "t1").unwrap();
        let done = brief_run_once(&conn, &ErrModel, "fake", "t2").unwrap();
        assert_eq!(done, summary("failed", 1, 1, 0));
        let item_id = format!("esc-{id}");
        let (kind, headline, what, why, action, stage) = open_item(&conn, &item_id);
        assert_eq!(kind, "needs");
        assert_eq!(headline, "Partner asked twice.");
        assert_eq!(what, "Partner asked twice.");
        assert_eq!(why, "");
        assert_eq!(action, "Partner asked twice.");
        assert_eq!(stage, 0);
        assert_eq!(count_where(&conn, "SELECT COUNT(*) FROM a1_items WHERE kind='needs'"), 1);
        assert_eq!(count_where(&conn, "SELECT COUNT(*) FROM a1_items WHERE kind='fyi'"), 0);
        let row: (String, String) = conn
            .query_row(
                "SELECT mode, outcome FROM ai_runs WHERE outcome='failed'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(row, ("brief".into(), "failed".into()));
    }

    #[test]
    fn a_mapped_director_item_with_facts_gets_an_fyi() {
        let conn = ready();
        move_story(&conn);
        let done = brief_run_once(&conn, &FakeModel, "fake", "t1").unwrap();
        assert_eq!(done, summary("drafted", 1, 1, 0));
        let (kind, headline, what, why, action, stage) = open_item(&conn, "pay-1");
        assert_eq!(kind, "fyi");
        assert_eq!(headline, "Payments vendor");
        assert_eq!(what, "S-1 Ship it: todo -> doing");
        assert_eq!(why, "");
        assert_eq!(action, "");
        assert_eq!(stage, 0);
        let quiet = brief_run_once(&conn, &NoModel, "fake", "t2").unwrap();
        assert_eq!(quiet, summary("unchanged", 0, 0, 0));
        assert_eq!(count_where(&conn, "SELECT COUNT(*) FROM a1_items"), 1);
    }

    #[test]
    fn an_existing_needs_item_keeps_headline_why_and_action() {
        let conn = ready();
        briefs::upsert_item(
            &conn,
            &BriefItemInput {
                item_id: "pay-1".into(),
                kind: "needs".into(),
                position: 0,
                headline: "Vendor late".into(),
                what: "Old fact.".into(),
                why: "Partner is waiting.".into(),
                action: "Ask by Friday.".into(),
                at_this_stage: 1,
            },
        )
        .unwrap();
        move_story(&conn);
        let done = brief_run_once(&conn, &FakeModel, "fake", "t1").unwrap();
        assert_eq!(done, summary("drafted", 1, 1, 0));
        let (kind, headline, what, why, action, stage) = open_item(&conn, "pay-1");
        assert_eq!(kind, "needs");
        assert_eq!(headline, "Vendor late");
        assert_eq!(why, "Partner is waiting.");
        assert_eq!(action, "Ask by Friday.");
        assert_eq!(what, "Old fact.\nS-1 Ship it: todo -> doing");
        assert_eq!(stage, 0);
        assert_eq!(count_where(&conn, "SELECT COUNT(*) FROM a1_items"), 1);
    }

    #[test]
    fn an_existing_fyi_keeps_why_action_and_stage() {
        let conn = ready();
        briefs::upsert_item(
            &conn,
            &BriefItemInput {
                item_id: "pay-1".into(),
                kind: "fyi".into(),
                position: 0,
                headline: "Old headline".into(),
                what: "Old fact.".into(),
                why: "Partner uses it.".into(),
                action: "Watch the queue.".into(),
                at_this_stage: 1,
            },
        )
        .unwrap();
        move_story(&conn);
        let done = brief_run_once(&conn, &FakeModel, "fake", "t1").unwrap();
        assert_eq!(done, summary("drafted", 1, 1, 0));
        let (kind, headline, what, why, action, stage) = open_item(&conn, "pay-1");
        assert_eq!(kind, "fyi");
        assert_eq!(headline, "Payments vendor");
        assert_eq!(what, "Old fact.\nS-1 Ship it: todo -> doing");
        assert_eq!(why, "Partner uses it.");
        assert_eq!(action, "Watch the queue.");
        assert_eq!(stage, 1);
    }

    #[test]
    fn the_ai_runs_row_has_mode_brief() {
        let conn = ready();
        move_story(&conn);
        brief_run_once(&conn, &FakeModel, "fake", "t1").unwrap();
        let rows: Vec<(String, Option<String>)> = conn
            .prepare("SELECT mode, detail FROM ai_runs ORDER BY id")
            .unwrap()
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert!(rows.iter().all(|(mode, _)| mode == "brief"));
        let drafted: Vec<_> = conn
            .prepare("SELECT mode, detail, model FROM ai_runs WHERE outcome='drafted'")
            .unwrap()
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<String>>(2)?,
                ))
            })
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(drafted.len(), 1);
        assert_eq!(drafted[0].0, "brief");
        assert_eq!(drafted[0].1, None);
        assert_eq!(drafted[0].2.as_deref(), Some("fake"));
    }

    #[test]
    fn a_pass_with_no_facts_is_unchanged_and_does_not_call_the_model() {
        let conn = ready();
        let again = brief_run_once(&conn, &NoModel, "fake", "t1").unwrap();
        assert_eq!(again, summary("unchanged", 0, 0, 0));
        let runs: Vec<String> = conn
            .prepare(
                "SELECT mode || ' ' || outcome || ' ' || facts || ' ' || COALESCE(model, '-')
                 FROM ai_runs ORDER BY id",
            )
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(
            runs,
            ["brief unchanged 0 -", "brief unchanged 0 -"]
        );
    }

    #[test]
    fn a_write_that_fails_partway_leaves_the_brief_and_logs_a_failed_run() {
        let conn = ready();
        briefs::upsert_item(
            &conn,
            &BriefItemInput {
                item_id: "pay-1".into(),
                kind: "fyi".into(),
                position: 0,
                headline: "Old headline".into(),
                what: "Old fact.".into(),
                why: "Partner uses it.".into(),
                action: "Watch the queue.".into(),
                at_this_stage: 1,
            },
        )
        .unwrap();
        move_story(&conn);
        // The item update runs first. The drafted log is the later write and it fails.
        conn.execute_batch(
            "CREATE TRIGGER fail_drafted_log BEFORE INSERT ON ai_runs
             WHEN NEW.outcome = 'drafted'
             BEGIN
               SELECT RAISE(FAIL, 'drafted log failed');
             END;",
        )
        .unwrap();
        let err = brief_run_once(&conn, &FakeModel, "fake", "t1").unwrap_err();
        assert!(matches!(err, AiError::Db(ref msg) if msg.contains("drafted log failed")));
        assert_eq!(count_where(&conn, "SELECT COUNT(*) FROM a1_items"), 1);
        let (kind, headline, what, why, action, stage) = open_item(&conn, "pay-1");
        assert_eq!(kind, "fyi");
        assert_eq!(headline, "Old headline");
        assert_eq!(what, "Old fact.");
        assert_eq!(why, "Partner uses it.");
        assert_eq!(action, "Watch the queue.");
        assert_eq!(stage, 1);
        let failed: Vec<(String, String)> = conn
            .prepare("SELECT mode, outcome FROM ai_runs WHERE outcome='failed'")
            .unwrap()
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(failed, vec![("brief".into(), "failed".into())]);
    }

    /// Panics when the pass offers anything except the one new story fact.
    struct NewFactOnly;

    impl Model for NewFactOnly {
        fn draft(&self, req: &DraftRequest) -> Result<Vec<crate::draft::PatchLine>, String> {
            let texts: Vec<&str> = req
                .items
                .iter()
                .flat_map(|item| item.facts.iter().map(|fact| fact.text.as_str()))
                .collect();
            if texts != ["S-1 Ship it: todo -> doing"] {
                panic!("called when there are no new facts");
            }
            FakeModel.draft(req)
        }
    }

    #[test]
    fn the_first_brief_pass_skips_old_detector_events() {
        let conn = db();
        updates::next_from_last(&conn, "2029-03-16", "t").unwrap();
        updates::upsert_item(&conn, &page_item("pay-1", "Payments vendor")).unwrap();
        briefs::next_from_last(&conn, "2029-03-16", "t").unwrap();
        for n in 1..=3 {
            conn.execute(
                "INSERT INTO activity_events (source, type, ref, actor, occurred_at, payload)
                 VALUES ('detector', 'story_status', ?1, NULL, 't-old', ?2)",
                params![
                    format!("S-old-{n}"),
                    serde_json::json!({
                        "items": ["pay-1"],
                        "from": "old",
                        "to": "older",
                        "title": format!("Months ago {n}"),
                    })
                    .to_string(),
                ],
            )
            .unwrap();
        }

        let first = brief_run_once(&conn, &NewFactOnly, "fake", "t0").unwrap();
        assert_eq!(first, summary("unchanged", 0, 0, 0));
        let (mark, model): (Option<i64>, Option<String>) = conn
            .query_row(
                "SELECT last_event, model FROM ai_runs WHERE mode='brief'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        let high: i64 = conn
            .query_row(
                "SELECT MAX(id) FROM activity_events WHERE source='detector'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(mark, Some(high));
        assert_eq!(model, None);

        move_story(&conn);
        let second = brief_run_once(&conn, &NewFactOnly, "fake", "t1").unwrap();
        assert_eq!(second, summary("drafted", 1, 1, 0));
        let (_, _, what, _, _, _) = open_item(&conn, "pay-1");
        assert_eq!(what, "S-1 Ship it: todo -> doing");
    }
}
