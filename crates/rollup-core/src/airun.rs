//! One AI pass over the open leadership update: detect, draft, validate, apply, log.
//! The model is a trait object, so every test here runs offline.

use crate::ai::{self, AiError};
use crate::draft::{self, DraftRequest, ItemRequest, Model, SourceFact, Verdict};
use crate::updates::{self, Item, ItemInput, UpdateError};
use rusqlite::{params, Connection, OptionalExtension};
use std::collections::HashMap;

/// Stored in `ai_runs.mode`: the format this pass drafts.
// The brief pass logs mode "brief" and reads its own cursor. This one stays "leadership".
const MODE: &str = "leadership";

/// What one pass did. `outcome` is unchanged, drafted, no_draft, or failed.
/// `facts` counts the changes detected, `accepted` and `rejected` count patch lines.
#[derive(Debug, Clone, PartialEq)]
pub struct RunSummary {
    pub outcome: String,
    pub facts: usize,
    pub accepted: usize,
    pub rejected: usize,
}

fn db_err(err: impl ToString) -> AiError {
    AiError::Db(err.to_string())
}

/// Stored text is capped, so a long error can't fill the log.
fn clip(text: &str) -> String {
    text.chars().take(200).collect()
}

fn input_of(item: &Item) -> ItemInput {
    ItemInput {
        item_id: item.item_id.clone(),
        category: item.category.clone(),
        position: item.position,
        title: item.title.clone(),
        priority: item.priority,
        start_date: item.start_date.clone(),
        end_date: item.end_date.clone(),
        pct: item.pct,
        capacity: item.capacity,
        completed: item.completed.clone(),
        next: item.next.clone(),
        future: item.future.clone(),
    }
}

/// One plain sentence for a detected change. Dashes become hyphens, which the
/// validator would otherwise reject on a title copied from Jira or GitHub.
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

struct DetectorEvent {
    id: i64,
    r#ref: String,
    payload: String,
}

/// Newest detector event a drafted or unchanged run of this mode has already covered.
fn watermark(conn: &Connection) -> Result<i64, AiError> {
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
    Ok(row.flatten().unwrap_or(0))
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

pub(crate) fn flat_facts(items: &[ItemRequest]) -> Vec<SourceFact> {
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

fn log_run(
    conn: &Connection,
    now: &str,
    model: Option<&str>,
    run: &RunSummary,
    detail: Option<&str>,
    last_event: Option<i64>,
) -> Result<(), AiError> {
    // ponytail: one clock per run, so finished_at equals started_at; read a real clock when run length matters
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

fn pass(
    conn: &Connection,
    model: &dyn Model,
    now: &str,
) -> Result<(RunSummary, Option<String>, Option<i64>), AiError> {
    ai::detect(conn, now)?;
    let after = watermark(conn)?;
    let events = open_events(conn, after)?;
    let found = events.len();
    let covered = events.last().map(|event| event.id).unwrap_or(after);
    let done = |outcome: &str, accepted: usize, rejected: usize, detail: Option<&str>| {
        let run = RunSummary {
            outcome: outcome.to_string(),
            facts: found,
            accepted,
            rejected,
        };
        // A failed or no_draft row leaves last_event null, so the next pass offers these rows again.
        let mark = matches!(outcome, "drafted" | "unchanged").then_some(covered);
        (run, detail.map(String::from), mark)
    };
    if events.is_empty() {
        return Ok(done("unchanged", 0, 0, None));
    }
    // ponytail: list() reads every entry; read only the draft once the archive is long
    let live = updates::list(conn, true)
        .map_err(|err| match err {
            UpdateError::Db(msg) => AiError::Db(msg),
            other => db_err(format!("{other:?}")),
        })?
        .into_iter()
        .find(|entry| entry.sent_at.is_none());
    let Some(live) = live else {
        return Ok(done("no_draft", 0, 0, None));
    };
    // Each item as read now. A write below must find it unchanged (compare and swap).
    let mut current: HashMap<String, ItemInput> = live
        .items
        .iter()
        .map(|item| (item.item_id.clone(), input_of(item)))
        .collect();
    let mut items: Vec<ItemRequest> = Vec::new();
    for event in &events {
        let mine: Vec<String> = payload_items(&event.payload)
            .into_iter()
            .filter(|id| current.contains_key(id))
            .collect();
        if mine.is_empty() {
            continue;
        }
        let fact = SourceFact {
            id: format!("ev-{}", event.id),
            text: sentence(&event.r#ref, &event.payload),
        };
        for id in mine {
            if let Some(slot) = items.iter_mut().find(|item| item.item_id == id) {
                slot.facts.push(fact.clone());
                continue;
            }
            let have = &current[&id];
            items.push(ItemRequest {
                item_id: id.clone(),
                facts: vec![fact.clone()],
                completed: have.completed.clone(),
                next: have.next.clone(),
                future: have.future.clone(),
            });
        }
    }
    if items.is_empty() {
        return Ok(done(
            "unchanged",
            0,
            0,
            Some("no changed item is on the open draft"),
        ));
    }
    // The model may patch only the items it was asked about.
    current.retain(|id, _| items.iter().any(|item| &item.item_id == id));
    let facts = flat_facts(&items);
    let request = DraftRequest { items };
    let patch = match model.draft(&request) {
        Ok(patch) => patch,
        Err(text) => return Ok(done("failed", 0, 0, Some(clip(&text).as_str()))),
    };
    let (mut accepted, mut rejected) = (0, 0);
    for verdict in draft::validate(conn, &patch, &facts, &request.items, now)? {
        let Verdict::Accepted(line) = verdict else {
            rejected += 1;
            continue;
        };
        let Some(have) = current.get_mut(&line.item_id) else {
            rejected += 1;
            continue;
        };
        let mut want = have.clone();
        match line.field.as_str() {
            "completed" => want.completed = line.lines,
            "next" => want.next = line.lines,
            "future" => want.future = line.lines,
            _ => {
                rejected += 1;
                continue;
            }
        }
        match updates::upsert_item_if(conn, &want, Some(&*have)) {
            Ok(()) => {
                *have = want;
                accepted += 1;
            }
            Err(UpdateError::Changed) => {
                let why = "changed by the owner during the run";
                ai::raise_flag(conn, &line.item_id, &line.field, why, now)?;
                rejected += 1;
            }
            Err(UpdateError::Db(msg)) => return Err(AiError::Db(msg)),
            // The draft closed mid-run, or a stored value is invalid. Nothing was written.
            Err(_) => rejected += 1,
        }
    }
    Ok(done("drafted", accepted, rejected, None))
}

/// Detect changes, draft the affected items on the open leadership update, validate,
/// apply, and log one `ai_runs` row. No facts means no model call. A model error is
/// logged as `failed` and returns Ok. A database error returns Err after a best-effort
/// `failed` row. Sent entries are never written. A failed run does not advance
/// `last_event`, so the next pass offers the same detector rows.
pub fn run_once(
    conn: &Connection,
    model: &dyn Model,
    model_name: &str,
    now: &str,
) -> Result<RunSummary, AiError> {
    let (summary, detail, last_event) = match pass(conn, model, now) {
        Ok(done) => done,
        Err(err) => {
            // Best effort, the database may be what failed. A partial pass still leaves a row.
            let (AiError::Invalid(msg) | AiError::Db(msg)) = &err;
            let failed = RunSummary {
                outcome: "failed".to_string(),
                facts: 0,
                accepted: 0,
                rejected: 0,
            };
            let _ = log_run(conn, now, None, &failed, Some(clip(msg).as_str()), None);
            return Err(err);
        }
    };
    let called = matches!(summary.outcome.as_str(), "drafted" | "failed");
    let used = called.then_some(model_name);
    log_run(conn, now, used, &summary, detail.as_deref(), last_event)?;
    Ok(summary)
}

/// `YYYY-MM-DDTHH:MM:SSZ` to unix seconds. No chrono: the civil date is Howard
/// Hinnant's days-from-civil, the same conversion `store::format_epoch` inverts.
fn unix_utc(text: &str) -> Option<i64> {
    let b = text.as_bytes();
    if b.len() != 20
        || b[4] != b'-'
        || b[7] != b'-'
        || b[10] != b'T'
        || b[13] != b':'
        || b[16] != b':'
        || b[19] != b'Z'
    {
        return None;
    }
    let num = |at: usize, len: usize| text.get(at..at + len)?.parse::<i64>().ok();
    let (year, month, day) = (num(0, 4)?, num(5, 2)?, num(8, 2)?);
    let (hour, min, sec) = (num(11, 2)?, num(14, 2)?, num(17, 2)?);
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || !(0..=23).contains(&hour)
        || !(0..=59).contains(&min)
        || !(0..=59).contains(&sec)
    {
        return None;
    }
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = (y - era * 400) as u64;
    let mp = (if month > 2 { month - 3 } else { month + 9 }) as u64;
    let doy = (153 * mp + 2) / 5 + day as u64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe as i64 - 719_468;
    Some(days * 86_400 + hour * 3_600 + min * 60 + sec)
}

fn last_drafted_or_unchanged(conn: &Connection) -> Result<Option<String>, AiError> {
    match conn.query_row(
        "SELECT started_at FROM ai_runs
         WHERE outcome IN ('drafted', 'unchanged')
         ORDER BY id DESC LIMIT 1",
        [],
        |row| row.get(0),
    ) {
        Ok(started) => Ok(Some(started)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(err) => Err(db_err(err)),
    }
}

/// `None` when a pass is due. `Some(reason)` when it should not run.
/// `weekday` is 1 (Monday) through 7 (Sunday). `minute_of_day` is local minutes
/// since midnight. `now` is UTC `YYYY-MM-DDTHH:MM:SSZ`. Today for the model cap
/// is that UTC date. A lid that opens after two quiet hours runs once, because
/// this gap check passes once.
pub fn is_due(
    conn: &Connection,
    weekday: u8,
    minute_of_day: u16,
    now: &str,
) -> Result<Option<&'static str>, AiError> {
    if weekday == 6 || weekday == 7 {
        return Ok(Some("weekend"));
    }
    // 08:00 and 17:00 are inside the window. The minute after 17:00 is not.
    if !(8 * 60..=17 * 60).contains(&minute_of_day) {
        return Ok(Some("outside 08:00-17:00"));
    }
    let now_secs =
        unix_utc(now).ok_or_else(|| AiError::Invalid("now must be YYYY-MM-DDTHH:MM:SSZ".into()))?;
    if let Some(started) = last_drafted_or_unchanged(conn)? {
        let then = unix_utc(&started).ok_or_else(|| {
            AiError::Invalid("ai_runs.started_at must be YYYY-MM-DDTHH:MM:SSZ".into())
        })?;
        // Less than 2 hours. A run exactly 120 minutes ago is due again. A row dated
        // in the future (clock moved back) does not block; the daily cap still holds.
        if (0..120 * 60).contains(&(now_secs - then)) {
            return Ok(Some("ran less than 2 hours ago"));
        }
    }
    let today = now.get(..10).unwrap_or(now);
    let calls: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM ai_runs
             WHERE model IS NOT NULL AND substr(started_at, 1, 10) = ?1",
            [today],
            |row| row.get(0),
        )
        .map_err(db_err)?;
    if calls >= 10 {
        return Ok(Some("daily model-call cap reached"));
    }
    if std::env::var("ROLLUP_AI").ok().as_deref() == Some("off") {
        return Ok(Some("ROLLUP_AI is off"));
    }
    Ok(None)
}

/// Set `published_at` on the open draft to `now`. No open draft is
/// [`UpdateError::NoDraft`].
pub fn publish_open(conn: &Connection, now: &str) -> Result<String, UpdateError> {
    let id: Option<i64> = conn
        .query_row(
            "SELECT id FROM lu_entries WHERE sent_at IS NULL LIMIT 1",
            [],
            |row| row.get(0),
        )
        .optional()
        .map_err(|err| UpdateError::Db(err.to_string()))?;
    let Some(id) = id else {
        return Err(UpdateError::NoDraft);
    };
    conn.execute(
        "UPDATE lu_entries SET published_at=?1 WHERE id=?2 AND sent_at IS NULL",
        params![now, id],
    )
    .map_err(|err| UpdateError::Db(err.to_string()))?;
    Ok(now.to_string())
}

/// `auto` is always published. In any other mode, the open draft's `published_at`
/// must be set and not older than the newest `ai_runs.started_at` with outcome
/// `drafted`. Both are UTC ISO, so text order is time order. No drafted row
/// means a stamp is enough. No open draft is unpublished, except in `auto`.
pub fn open_ai_published(conn: &Connection, mode: &str) -> Result<bool, UpdateError> {
    if mode == "auto" {
        return Ok(true);
    }
    let published_at: Option<String> = match conn.query_row(
        "SELECT published_at FROM lu_entries WHERE sent_at IS NULL LIMIT 1",
        [],
        |row| row.get(0),
    ) {
        Ok(value) => value,
        Err(rusqlite::Error::QueryReturnedNoRows) => None,
        Err(err) => return Err(UpdateError::Db(err.to_string())),
    };
    let newest: Option<String> = match conn.query_row(
        "SELECT started_at FROM ai_runs WHERE outcome = 'drafted' ORDER BY started_at DESC LIMIT 1",
        [],
        |row| row.get(0),
    ) {
        Ok(value) => Some(value),
        Err(rusqlite::Error::QueryReturnedNoRows) => None,
        Err(err) => return Err(UpdateError::Db(err.to_string())),
    };
    Ok(stamp_covers(published_at.as_deref(), newest.as_deref()))
}

fn stamp_covers(published_at: Option<&str>, newest_drafted: Option<&str>) -> bool {
    let Some(published) = published_at.filter(|text| !text.is_empty()) else {
        return false;
    };
    match newest_drafted {
        Some(started) => published >= started,
        None => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::draft::{FakeModel, PatchLine};
    use crate::updates::{next_from_last, send, upsert_item};

    /// A model that must never be asked.
    struct NoModel;

    impl Model for NoModel {
        fn draft(&self, _: &DraftRequest) -> Result<Vec<PatchLine>, String> {
            panic!("the model was called");
        }
    }

    /// A model that always gives the same answer.
    struct Says(Result<Vec<PatchLine>, String>);

    impl Model for Says {
        fn draft(&self, _: &DraftRequest) -> Result<Vec<PatchLine>, String> {
            self.0.clone()
        }
    }

    /// An owner who edits pay-1 while the run waits on the model.
    struct Editing<'a>(&'a Connection);

    impl Model for Editing<'_> {
        fn draft(&self, req: &DraftRequest) -> Result<Vec<PatchLine>, String> {
            upsert_item(self.0, &item("pay-1", &["owner edit"])).unwrap();
            FakeModel.draft(req)
        }
    }

    fn item(item_id: &str, completed: &[&str]) -> ItemInput {
        ItemInput {
            item_id: item_id.into(),
            category: "Now".into(),
            position: 0,
            title: item_id.into(),
            priority: 0,
            start_date: None,
            end_date: None,
            pct: None,
            capacity: None,
            completed: completed.iter().map(|line| line.to_string()).collect(),
            next: vec![],
            future: vec![],
        }
    }

    /// Story S-1 feeds item pay-1. Item pay-2 maps to nothing.
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

    /// A sent entry, an open draft copied from it, and the baseline snapshot taken.
    fn live() -> Connection {
        let conn = db();
        next_from_last(&conn, "2029-03-02", "t").unwrap();
        upsert_item(&conn, &item("pay-1", &["sent words"])).unwrap();
        upsert_item(&conn, &item("pay-2", &["sent words"])).unwrap();
        send(&conn, "t").unwrap();
        next_from_last(&conn, "2029-03-16", "t").unwrap();
        run_once(&conn, &NoModel, "fake", "t0").unwrap();
        conn
    }

    fn move_story(conn: &Connection) {
        conn.execute("UPDATE stories SET status='doing'", [])
            .unwrap();
    }

    fn completed(conn: &Connection, item_id: &str) -> Vec<String> {
        let entries = updates::list(conn, true).unwrap();
        let open = entries.iter().find(|e| e.sent_at.is_none()).unwrap();
        let found = open.items.iter().find(|i| i.item_id == item_id);
        found.unwrap().completed.clone()
    }

    fn runs(conn: &Connection) -> Vec<String> {
        conn.prepare(
            "SELECT outcome || ' ' || facts || ' ' || accepted || ' ' || rejected
                    || ' ' || COALESCE(model, '-')
             FROM ai_runs ORDER BY id",
        )
        .unwrap()
        .query_map([], |row| row.get::<_, String>(0))
        .unwrap()
        .map(Result::unwrap)
        .collect()
    }

    fn summary(outcome: &str, facts: usize, accepted: usize, rejected: usize) -> RunSummary {
        RunSummary {
            outcome: outcome.into(),
            facts,
            accepted,
            rejected,
        }
    }

    #[test]
    fn unchanged_pass_never_calls_the_model() {
        let conn = live();
        let again = run_once(&conn, &NoModel, "fake", "t1").unwrap();
        assert_eq!(again, summary("unchanged", 0, 0, 0));
        assert_eq!(runs(&conn), ["unchanged 0 0 0 -", "unchanged 0 0 0 -"]);
    }

    #[test]
    fn a_change_drafts_the_mapped_item_and_never_touches_a_sent_entry() {
        let conn = live();
        move_story(&conn);
        let sent = updates::list(&conn, false).unwrap();
        let done = run_once(&conn, &FakeModel, "fake", "t1").unwrap();
        assert_eq!(done, summary("drafted", 1, 1, 0));
        assert_eq!(completed(&conn, "pay-1"), ["S-1 Ship it: todo -> doing"]);
        assert!(completed(&conn, "pay-2").is_empty());
        assert_eq!(updates::list(&conn, false).unwrap(), sent);
        assert_eq!(runs(&conn)[1], "drafted 1 1 0 fake");
        // The next pass has nothing new, so it never reaches the model.
        let quiet = run_once(&conn, &NoModel, "fake", "t2").unwrap();
        assert_eq!(quiet, summary("unchanged", 0, 0, 0));
    }

    #[test]
    fn a_failing_model_logs_failed_and_returns_ok() {
        let conn = live();
        move_story(&conn);
        let down = Says(Err("gateway down ".repeat(30)));
        let done = run_once(&conn, &down, "fake", "t1").unwrap();
        assert_eq!(done, summary("failed", 1, 0, 0));
        let detail: String = conn
            .query_row(
                "SELECT detail FROM ai_runs WHERE outcome='failed'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(detail.chars().count(), 200);
        assert_eq!(runs(&conn)[1], "failed 1 0 0 fake");
        assert!(completed(&conn, "pay-1").is_empty());
    }

    #[test]
    fn no_open_draft_logs_no_draft_and_touches_nothing() {
        let conn = live();
        send(&conn, "t").unwrap();
        move_story(&conn);
        let before = updates::list(&conn, true).unwrap();
        let done = run_once(&conn, &NoModel, "fake", "t1").unwrap();
        assert_eq!(done, summary("no_draft", 1, 0, 0));
        assert_eq!(updates::list(&conn, true).unwrap(), before);
        assert_eq!(runs(&conn)[1], "no_draft 1 0 0 -");
    }

    #[test]
    fn a_database_error_returns_err_and_still_leaves_a_failed_row() {
        let conn = live();
        move_story(&conn);
        conn.execute_batch("DROP TABLE activity_events").unwrap();
        let err = run_once(&conn, &NoModel, "fake", "t1").unwrap_err();
        assert!(matches!(err, AiError::Db(_)));
        assert_eq!(runs(&conn)[1], "failed 0 0 0 -");
    }

    #[test]
    fn an_owner_edit_during_the_run_wins_and_is_flagged() {
        let conn = live();
        move_story(&conn);
        let done = run_once(&conn, &Editing(&conn), "fake", "t1").unwrap();
        assert_eq!(done, summary("drafted", 1, 0, 1));
        assert_eq!(completed(&conn, "pay-1"), ["owner edit"]);
        let flags = ai::open_flags(&conn).unwrap();
        assert_eq!(flags.len(), 1);
        assert_eq!(flags[0].reason, "changed by the owner during the run");
    }

    #[test]
    fn model_output_is_untrusted() {
        let conn = live();
        move_story(&conn);
        let line = |item_id: &str, text: &str| PatchLine {
            item_id: item_id.into(),
            field: "completed".into(),
            lines: vec![text.into()],
            source_ids: vec!["ev-1".into()],
        };
        let model = Says(Ok(vec![
            // An item on the draft that no changed fact belongs to.
            line("pay-2", "S-1 Ship it: todo -> doing"),
            // A number its source does not have: the validator rejects and flags it.
            line("pay-1", "Done in 3 days"),
            // An item that is not on the draft at all.
            line("ghost", "S-1 Ship it: todo -> doing"),
        ]));
        let done = run_once(&conn, &model, "fake", "t1").unwrap();
        assert_eq!(done, summary("drafted", 1, 0, 3));
        assert!(completed(&conn, "pay-1").is_empty());
        assert!(completed(&conn, "pay-2").is_empty());
        assert_eq!(ai::open_flags(&conn).unwrap().len(), 1);
    }

    #[test]
    fn a_change_to_no_item_on_the_draft_is_unchanged() {
        let conn = live();
        updates::remove_item(&conn, "pay-1").unwrap();
        move_story(&conn);
        let done = run_once(&conn, &NoModel, "fake", "t1").unwrap();
        assert_eq!(done, summary("unchanged", 1, 0, 0));
        assert_eq!(runs(&conn)[1], "unchanged 1 0 0 -");
    }

    #[test]
    fn a_dash_in_a_title_still_passes_the_validator() {
        let conn = live();
        move_story(&conn);
        conn.execute("UPDATE stories SET summary='Ship it \u{2013} now'", [])
            .unwrap();
        let done = run_once(&conn, &FakeModel, "fake", "t1").unwrap();
        assert_eq!(done, summary("drafted", 1, 1, 0));
        assert_eq!(
            completed(&conn, "pay-1"),
            ["S-1 Ship it - now: todo -> doing"]
        );
    }

    /// Holds `ROLLUP_AI` still for one test, then puts the previous value back.
    struct AiEnv {
        _guard: std::sync::MutexGuard<'static, ()>,
        prev: Option<String>,
    }

    impl Drop for AiEnv {
        fn drop(&mut self) {
            match &self.prev {
                Some(value) => std::env::set_var("ROLLUP_AI", value),
                None => std::env::remove_var("ROLLUP_AI"),
            }
        }
    }

    fn hold_ai(value: Option<&str>) -> AiEnv {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let guard = LOCK.lock().unwrap_or_else(|poison| poison.into_inner());
        let prev = std::env::var("ROLLUP_AI").ok();
        match value {
            Some(value) => std::env::set_var("ROLLUP_AI", value),
            None => std::env::remove_var("ROLLUP_AI"),
        }
        AiEnv {
            _guard: guard,
            prev,
        }
    }

    fn blank() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(crate::store::SCHEMA).unwrap();
        conn
    }

    fn log_at(conn: &Connection, started_at: &str, outcome: &str, model: Option<&str>) {
        conn.execute(
            "INSERT INTO ai_runs (started_at, mode, outcome, model) VALUES (?1, 'leadership', ?2, ?3)",
            params![started_at, outcome, model],
        )
        .unwrap();
    }

    const NOON: &str = "2024-03-01T16:00:00Z";

    #[test]
    fn weekend_is_not_due() {
        let _env = hold_ai(None);
        let conn = blank();
        assert_eq!(is_due(&conn, 6, 12 * 60, NOON).unwrap(), Some("weekend"));
        assert_eq!(is_due(&conn, 7, 9 * 60, NOON).unwrap(), Some("weekend"));
        assert_eq!(is_due(&conn, 1, 12 * 60, NOON).unwrap(), None);
    }

    #[test]
    fn before_0800_and_after_1700_are_not_due() {
        let _env = hold_ai(None);
        let conn = blank();
        assert_eq!(
            is_due(&conn, 3, 7 * 60 + 59, NOON).unwrap(),
            Some("outside 08:00-17:00")
        );
        assert_eq!(
            is_due(&conn, 3, 17 * 60 + 1, NOON).unwrap(),
            Some("outside 08:00-17:00")
        );
        assert_eq!(is_due(&conn, 3, 8 * 60, NOON).unwrap(), None);
        assert_eq!(is_due(&conn, 3, 17 * 60, NOON).unwrap(), None);
    }

    #[test]
    fn a_run_30_minutes_ago_blocks() {
        let _env = hold_ai(None);
        let conn = blank();
        // Crosses the leap day so a clock-only compare cannot see 30 minutes.
        let now = "2024-03-01T00:10:00Z";
        log_at(&conn, "2024-02-29T23:40:00Z", "drafted", Some("fake"));
        assert_eq!(
            is_due(&conn, 5, 10 * 60, now).unwrap(),
            Some("ran less than 2 hours ago")
        );
    }

    #[test]
    fn a_failed_pass_is_offered_again() {
        let conn = live();
        move_story(&conn);
        let failed = run_once(&conn, &Says(Err("down".into())), "fake", "t1").unwrap();
        assert_eq!(failed, summary("failed", 1, 0, 0));
        let missed: Option<i64> = conn
            .query_row(
                "SELECT last_event FROM ai_runs WHERE outcome='failed'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(missed, None);
        let done = run_once(&conn, &FakeModel, "fake", "t2").unwrap();
        assert_eq!(done, summary("drafted", 1, 1, 0));
        assert_eq!(completed(&conn, "pay-1"), ["S-1 Ship it: todo -> doing"]);
    }

    #[test]
    fn two_items_each_get_only_their_own_facts() {
        let conn = live();
        conn.execute(
            "INSERT INTO stories (story_key, epic_key, summary, status)
             VALUES ('S-2', 'E-1', 'Second', 'todo')",
            [],
        )
        .unwrap();
        ai::map_source(&conn, "pay-2", "story", "S-2", "t").unwrap();
        move_story(&conn);
        let done = run_once(&conn, &FakeModel, "fake", "t1").unwrap();
        assert_eq!(done, summary("drafted", 2, 2, 0));
        assert_eq!(completed(&conn, "pay-1"), ["S-1 Ship it: todo -> doing"]);
        assert_eq!(completed(&conn, "pay-2"), ["S-2 Second: new -> doing"]);
    }

    #[test]
    fn the_fake_keeps_the_owners_completed_lines() {
        let conn = live();
        upsert_item(&conn, &item("pay-1", &["owner line"])).unwrap();
        move_story(&conn);
        let done = run_once(&conn, &FakeModel, "fake", "t1").unwrap();
        assert_eq!(done, summary("drafted", 1, 1, 0));
        assert_eq!(
            completed(&conn, "pay-1"),
            ["owner line", "S-1 Ship it: todo -> doing"]
        );
    }

    #[test]
    fn a_run_3_hours_ago_allows() {
        let _env = hold_ai(None);
        let conn = blank();
        let now = "2024-03-01T00:10:00Z";
        log_at(&conn, "2024-02-29T21:10:00Z", "unchanged", None);
        assert_eq!(is_due(&conn, 5, 10 * 60, now).unwrap(), None);
    }

    #[test]
    fn ten_model_rows_today_block() {
        let _env = hold_ai(None);
        let conn = blank();
        // Yesterday's model call is a different UTC date, so it does not count.
        log_at(&conn, "2024-02-29T23:00:00Z", "failed", Some("fake"));
        for hour in 1..=9 {
            log_at(
                &conn,
                &format!("2024-03-01T{hour:02}:00:00Z"),
                "failed",
                Some("fake"),
            );
        }
        assert_eq!(is_due(&conn, 5, 12 * 60, NOON).unwrap(), None);
        log_at(&conn, "2024-03-01T10:00:00Z", "failed", Some("fake"));
        assert_eq!(
            is_due(&conn, 5, 12 * 60, NOON).unwrap(),
            Some("daily model-call cap reached")
        );
    }

    #[test]
    fn a_failed_run_does_not_block() {
        let _env = hold_ai(None);
        let conn = blank();
        log_at(&conn, "2024-03-01T15:30:00Z", "failed", Some("fake"));
        assert_eq!(is_due(&conn, 5, 12 * 60, NOON).unwrap(), None);
    }

    #[test]
    fn rollup_ai_off_blocks() {
        let _env = hold_ai(Some("off"));
        let conn = blank();
        assert_eq!(
            is_due(&conn, 3, 12 * 60, NOON).unwrap(),
            Some("ROLLUP_AI is off")
        );
    }

    #[test]
    fn upgrade_backfills_last_event_so_old_facts_are_not_offered_again() {
        let conn = live();
        move_story(&conn);
        assert_eq!(
            run_once(&conn, &FakeModel, "fake", "t1").unwrap(),
            summary("drafted", 1, 1, 0)
        );
        conn.execute("ALTER TABLE ai_runs DROP COLUMN last_event", [])
            .unwrap();
        crate::store::ensure_last_event_column(&conn).unwrap();
        assert_eq!(
            run_once(&conn, &NoModel, "fake", "t2").unwrap(),
            summary("unchanged", 0, 0, 0)
        );
        conn.execute("UPDATE stories SET status='done'", []).unwrap();
        assert_eq!(
            run_once(&conn, &FakeModel, "fake", "t3").unwrap(),
            summary("drafted", 1, 1, 0)
        );
        assert_eq!(
            completed(&conn, "pay-1"),
            ["S-1 Ship it: todo -> doing", "S-1 Ship it: doing -> done"]
        );
    }

    #[test]
    fn publish_needs_a_draft_and_a_later_drafted_run_clears_it() {
        let conn = db();
        assert_eq!(
            publish_open(&conn, "2020-01-01T00:00:00Z").unwrap_err(),
            UpdateError::NoDraft
        );
        next_from_last(&conn, "2029-03-02", "t").unwrap();
        assert!(!open_ai_published(&conn, "review").unwrap());
        assert!(open_ai_published(&conn, "auto").unwrap());
        assert_eq!(
            publish_open(&conn, "2020-01-01T00:00:00Z").unwrap(),
            "2020-01-01T00:00:00Z"
        );
        let stored: String = conn
            .query_row(
                "SELECT published_at FROM lu_entries WHERE sent_at IS NULL",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(stored, "2020-01-01T00:00:00Z");
        assert!(open_ai_published(&conn, "review").unwrap());
        conn.execute(
            "INSERT INTO ai_runs (started_at, mode, outcome) VALUES ('2024-01-01T00:00:00Z', 'leadership', 'drafted')",
            [],
        )
        .unwrap();
        assert!(!open_ai_published(&conn, "review").unwrap());
        assert!(open_ai_published(&conn, "auto").unwrap());
        assert!(!open_ai_published(&conn, "nope").unwrap());
        assert_eq!(
            publish_open(&conn, "2024-06-01T00:00:00Z").unwrap(),
            "2024-06-01T00:00:00Z"
        );
        assert!(open_ai_published(&conn, "review").unwrap());
        conn.execute(
            "INSERT INTO ai_runs (started_at, mode, outcome) VALUES ('2024-06-01T00:00:00Z', 'leadership', 'drafted')",
            [],
        )
        .unwrap();
        assert!(open_ai_published(&conn, "review").unwrap());
    }
}
