//! Weekly partner brief: one open draft. The header is copied from the last
//! sent brief. Items are not. Nothing is computed.

// ponytail: one-page length (A1.2.7) is not enforced; the owner owns it
// ponytail: launch dates are not cross-checked against B (A1.4); a later helper may flag a mismatch

use rusqlite::{params, Connection, OptionalExtension};

/// `Sent`, `DraftOpen`, and `NoDraft` are HTTP 409. `NotFound` is 404.
#[derive(Debug, PartialEq)]
pub enum BriefError {
    Sent,
    DraftOpen,
    NoDraft,
    NotFound,
    Invalid(String),
    Db(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Brief {
    pub id: i64,
    pub week_ending: String,
    pub created_at: String,
    pub sent_at: Option<String>,
    pub org_unit: Option<String>,
    pub partner_name: Option<String>,
    pub prepared_by: Option<String>,
    pub bottom_line: Option<String>,
    pub items: Vec<BriefItem>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct BriefItem {
    pub id: i64,
    pub entry_id: i64,
    pub item_id: String,
    pub kind: String,
    pub position: i64,
    pub headline: String,
    pub what: String,
    pub why: String,
    pub action: String,
    pub at_this_stage: i64,
}

/// Full item body for the open draft.
#[derive(Debug, Clone, PartialEq)]
pub struct BriefItemInput {
    pub item_id: String,
    pub kind: String,
    pub position: i64,
    pub headline: String,
    pub what: String,
    pub why: String,
    pub action: String,
    pub at_this_stage: i64,
}

const ITEM_SQL: &str = "SELECT i.id, i.entry_id, i.item_id, i.kind, i.position, i.headline,
    i.what, i.why, i.action, i.at_this_stage
    FROM a1_items i";

fn db_err(err: impl ToString) -> BriefError {
    BriefError::Db(err.to_string())
}

fn require_week(value: &str) -> Result<(), BriefError> {
    match crate::updates::require_date(value, "Week ending") {
        Ok(()) => Ok(()),
        Err(crate::updates::UpdateError::Invalid(message)) => Err(BriefError::Invalid(message)),
        Err(other) => Err(BriefError::Db(format!("{other:?}"))),
    }
}

/// Newest first (`week_ending`, then `id`). Items are needs, then fyi, then position, then id.
pub fn list(conn: &Connection, include_draft: bool) -> Result<Vec<Brief>, BriefError> {
    let sql = if include_draft {
        "SELECT id, week_ending, created_at, sent_at, org_unit, partner_name, prepared_by, bottom_line
         FROM a1_entries
         ORDER BY week_ending DESC, id DESC"
    } else {
        "SELECT id, week_ending, created_at, sent_at, org_unit, partner_name, prepared_by, bottom_line
         FROM a1_entries
         WHERE sent_at IS NOT NULL
         ORDER BY week_ending DESC, id DESC"
    };
    let mut stmt = conn.prepare(sql).map_err(db_err)?;
    let heads: Vec<(
        i64,
        String,
        String,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
    )> = stmt
        .query_map([], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
                row.get(6)?,
                row.get(7)?,
            ))
        })
        .map_err(db_err)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(db_err)?;

    let mut briefs = Vec::with_capacity(heads.len());
    for (id, week_ending, created_at, sent_at, org_unit, partner_name, prepared_by, bottom_line) in
        heads
    {
        briefs.push(Brief {
            id,
            week_ending,
            created_at,
            sent_at,
            org_unit,
            partner_name,
            prepared_by,
            bottom_line,
            items: load_items(conn, id)?,
        });
    }
    Ok(briefs)
}

/// Open a draft for `week_ending`. Errors [`BriefError::DraftOpen`] if one is already open.
/// Copies org unit, partner name, and prepared by from the newest sent brief.
/// Bottom line stays empty. Items are not copied. `now` is stored as `created_at`.
pub fn next_from_last(conn: &Connection, week_ending: &str, now: &str) -> Result<i64, BriefError> {
    require_week(week_ending)?;
    let tx = conn.unchecked_transaction().map_err(db_err)?;
    if draft_id(&tx)?.is_some() {
        return Err(BriefError::DraftOpen);
    }
    // ponytail: no carry of last week's items; the brief is exception-based.
    // No "carry this escalation" toggle; add one if the owner keeps retyping open escalations.
    let sent = newest_sent(&tx)?;
    tx.execute(
        "INSERT INTO a1_entries (
            week_ending, created_at, sent_at, org_unit, partner_name, prepared_by, bottom_line
         ) VALUES (?1, ?2, NULL, ?3, ?4, ?5, NULL)",
        params![
            week_ending,
            now,
            sent.as_ref().and_then(|row| row.org_unit.clone()),
            sent.as_ref().and_then(|row| row.partner_name.clone()),
            sent.as_ref().and_then(|row| row.prepared_by.clone()),
        ],
    )
    .map_err(|err| {
        if is_one_draft(&err) {
            BriefError::DraftOpen
        } else {
            db_err(err)
        }
    })?;
    let id = tx.last_insert_rowid();
    tx.commit().map_err(db_err)?;
    Ok(id)
}

/// Insert or update `item.item_id` on the open draft.
/// A write aimed at a sent brief is [`BriefError::Sent`] before the field checks.
pub fn upsert_item(conn: &Connection, item: &BriefItemInput) -> Result<(), BriefError> {
    let Some(draft) = draft_id(conn)? else {
        if item_on_sent(conn, &item.item_id)? {
            return Err(BriefError::Sent);
        }
        return Err(BriefError::NoDraft);
    };
    // ponytail: pull from B is out, items authored directly
    if item.headline.trim().is_empty() {
        return Err(BriefError::Invalid("Write the headline.".into()));
    }
    if item.action.trim().is_empty() {
        return Err(BriefError::Invalid(
            "Every item needs its action line.".into(),
        ));
    }
    let kind = item.kind.trim();
    if kind != "needs" && kind != "fyi" {
        return Err(BriefError::Invalid("Use Needs attention or FYI.".into()));
    }
    // Meaningful on fyi only. Needs items always store 0.
    let at_this_stage = if kind == "fyi" { item.at_this_stage } else { 0 };
    if item_on_entry(conn, draft, &item.item_id)? {
        conn.execute(
            "UPDATE a1_items SET kind=?1, position=?2, headline=?3, what=?4, why=?5,
             action=?6, at_this_stage=?7
             WHERE entry_id=?8 AND item_id=?9",
            params![
                kind,
                item.position,
                item.headline,
                item.what,
                item.why,
                item.action,
                at_this_stage,
                draft,
                item.item_id,
            ],
        )
        .map_err(db_err)?;
        return Ok(());
    }
    conn.execute(
        "INSERT INTO a1_items (
            entry_id, item_id, kind, position, headline, what, why, action, at_this_stage
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            draft,
            item.item_id,
            kind,
            item.position,
            item.headline,
            item.what,
            item.why,
            item.action,
            at_this_stage,
        ],
    )
    .map_err(db_err)?;
    Ok(())
}

pub fn remove_item(conn: &Connection, item_id: &str) -> Result<(), BriefError> {
    let draft = draft_owning(conn, item_id)?;
    conn.execute(
        "DELETE FROM a1_items WHERE entry_id=?1 AND item_id=?2",
        params![draft, item_id],
    )
    .map_err(db_err)?;
    Ok(())
}

/// Move a draft item within its own kind. Kind is left as it is.
pub fn move_item(conn: &Connection, item_id: &str, position: i64) -> Result<(), BriefError> {
    let draft = draft_owning(conn, item_id)?;
    conn.execute(
        "UPDATE a1_items SET position=?1 WHERE entry_id=?2 AND item_id=?3",
        params![position, draft, item_id],
    )
    .map_err(db_err)?;
    Ok(())
}

/// Set the open draft's header and bottom line. A blank stores NULL.
/// No open draft is [`BriefError::NoDraft`].
pub fn set_header(
    conn: &Connection,
    org_unit: &str,
    partner_name: &str,
    prepared_by: &str,
    bottom_line: &str,
) -> Result<(), BriefError> {
    let draft = draft_id(conn)?.ok_or(BriefError::NoDraft)?;
    let n = conn
        .execute(
            "UPDATE a1_entries SET org_unit=?1, partner_name=?2, prepared_by=?3, bottom_line=?4
             WHERE id=?5 AND sent_at IS NULL",
            params![
                empty_as_null(org_unit),
                empty_as_null(partner_name),
                empty_as_null(prepared_by),
                empty_as_null(bottom_line),
                draft,
            ],
        )
        .map_err(db_err)?;
    if n == 0 {
        return Err(BriefError::NoDraft);
    }
    Ok(())
}

/// Stamp `sent_at` on the open draft.
pub fn send(conn: &Connection, now: &str) -> Result<(), BriefError> {
    let id = draft_id(conn)?.ok_or(BriefError::NoDraft)?;
    let n = conn
        .execute(
            "UPDATE a1_entries SET sent_at=?1 WHERE id=?2 AND sent_at IS NULL",
            params![now, id],
        )
        .map_err(db_err)?;
    if n == 0 {
        return Err(BriefError::NoDraft);
    }
    Ok(())
}

fn empty_as_null(value: &str) -> Option<&str> {
    let trimmed = value.trim();
    (!trimmed.is_empty()).then_some(trimmed)
}

struct SentRow {
    org_unit: Option<String>,
    partner_name: Option<String>,
    prepared_by: Option<String>,
}

fn newest_sent(conn: &Connection) -> Result<Option<SentRow>, BriefError> {
    conn.query_row(
        "SELECT org_unit, partner_name, prepared_by FROM a1_entries
         WHERE sent_at IS NOT NULL
         ORDER BY week_ending DESC, id DESC LIMIT 1",
        [],
        |row| {
            Ok(SentRow {
                org_unit: row.get(0)?,
                partner_name: row.get(1)?,
                prepared_by: row.get(2)?,
            })
        },
    )
    .optional()
    .map_err(db_err)
}

fn draft_id(conn: &Connection) -> Result<Option<i64>, BriefError> {
    conn.query_row(
        "SELECT id FROM a1_entries WHERE sent_at IS NULL LIMIT 1",
        [],
        |row| row.get(0),
    )
    .optional()
    .map_err(db_err)
}

/// Entry id of the open draft that currently holds `item_id`.
fn draft_owning(conn: &Connection, item_id: &str) -> Result<i64, BriefError> {
    if let Some(draft) = draft_id(conn)? {
        if item_on_entry(conn, draft, item_id)? {
            return Ok(draft);
        }
    }
    if item_on_sent(conn, item_id)? {
        return Err(BriefError::Sent);
    }
    Err(BriefError::NotFound)
}

fn item_on_sent(conn: &Connection, item_id: &str) -> Result<bool, BriefError> {
    let hit: Option<i64> = conn
        .query_row(
            "SELECT 1 FROM a1_items i
             JOIN a1_entries e ON e.id = i.entry_id
             WHERE i.item_id=?1 AND e.sent_at IS NOT NULL LIMIT 1",
            params![item_id],
            |row| row.get(0),
        )
        .optional()
        .map_err(db_err)?;
    Ok(hit.is_some())
}

fn item_on_entry(conn: &Connection, entry_id: i64, item_id: &str) -> Result<bool, BriefError> {
    let hit: Option<i64> = conn
        .query_row(
            "SELECT 1 FROM a1_items WHERE entry_id=?1 AND item_id=?2",
            params![entry_id, item_id],
            |row| row.get(0),
        )
        .optional()
        .map_err(db_err)?;
    Ok(hit.is_some())
}

fn load_items(conn: &Connection, entry_id: i64) -> Result<Vec<BriefItem>, BriefError> {
    let sql = format!(
        "{ITEM_SQL} WHERE i.entry_id=?1
         ORDER BY CASE i.kind WHEN 'needs' THEN 0 ELSE 1 END, i.position, i.id"
    );
    let mut stmt = conn.prepare(&sql).map_err(db_err)?;
    let items = stmt
        .query_map(params![entry_id], read_item)
        .map_err(db_err)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(db_err)?;
    Ok(items)
}

fn read_item(row: &rusqlite::Row<'_>) -> rusqlite::Result<BriefItem> {
    Ok(BriefItem {
        id: row.get(0)?,
        entry_id: row.get(1)?,
        item_id: row.get(2)?,
        kind: row.get(3)?,
        position: row.get(4)?,
        headline: row.get(5)?,
        what: row.get(6)?,
        why: row.get(7)?,
        action: row.get(8)?,
        at_this_stage: row.get(9)?,
    })
}

fn is_one_draft(err: &rusqlite::Error) -> bool {
    matches!(
        err,
        rusqlite::Error::SqliteFailure(code, Some(msg))
            if code.code == rusqlite::ErrorCode::ConstraintViolation
                && msg.contains("a1_one_draft")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn with_db(name: &str, f: impl FnOnce(&Connection)) {
        static N: AtomicUsize = AtomicUsize::new(0);
        let db = std::env::temp_dir().join(format!(
            "rollup_briefs_{name}_{}_{}.db",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_file(&db);
        crate::store::init_db(&db).unwrap();
        let conn = crate::store::connect(&db).unwrap();
        f(&conn);
        drop(conn);
        let _ = std::fs::remove_file(&db);
    }

    fn input(item_id: &str, kind: &str, headline: &str, action: &str) -> BriefItemInput {
        BriefItemInput {
            item_id: item_id.into(),
            kind: kind.into(),
            position: 0,
            headline: headline.into(),
            what: String::new(),
            why: String::new(),
            action: action.into(),
            at_this_stage: 0,
        }
    }

    fn open(conn: &Connection) {
        next_from_last(conn, "2029-03-09", "2029-03-09T09:00:00Z").unwrap();
    }

    #[test]
    fn next_from_last_carries_the_header_only() {
        with_db("carry", |conn| {
            next_from_last(conn, "2029-02-23", "2029-02-23T09:00:00Z").unwrap();
            set_header(conn, "Lending", "Riley Demo", "Jordan", "Old bottom line.").unwrap();
            upsert_item(conn, &input("esc-old", "needs", "Old escalation", "Ask.")).unwrap();
            send(conn, "2029-02-23T18:00:00Z").unwrap();

            next_from_last(conn, "2029-03-02", "2029-03-02T09:00:00Z").unwrap();
            set_header(conn, "Payments", "Sam Sample", "Alex", "Do not copy.").unwrap();
            upsert_item(conn, &input("esc-new", "fyi", "Newest note", "Milestone.")).unwrap();
            send(conn, "2029-03-02T18:00:00Z").unwrap();

            let draft = next_from_last(conn, "2029-03-09", "2029-03-09T09:00:00Z").unwrap();
            let briefs = list(conn, true).unwrap();
            assert_eq!(briefs[0].id, draft);
            assert!(briefs[0].sent_at.is_none());
            assert_eq!(briefs[0].org_unit.as_deref(), Some("Payments"));
            assert_eq!(briefs[0].partner_name.as_deref(), Some("Sam Sample"));
            assert_eq!(briefs[0].prepared_by.as_deref(), Some("Alex"));
            assert_eq!(briefs[0].bottom_line, None);
            assert!(briefs[0].items.is_empty());
            assert_eq!(briefs[1].bottom_line.as_deref(), Some("Do not copy."));
            assert_eq!(briefs[1].items.len(), 1);
            assert_eq!(briefs[2].org_unit.as_deref(), Some("Lending"));
            assert_eq!(briefs[2].items.len(), 1);
            let sent_only = list(conn, false).unwrap();
            assert!(sent_only.iter().all(|brief| brief.id != draft));
        });
    }

    #[test]
    fn upsert_requires_headline_action_and_kind() {
        with_db("fields", |conn| {
            open(conn);
            let mut item = input("n1", "needs", "  ", "Ask for a decision.");
            assert_eq!(
                upsert_item(conn, &item).unwrap_err(),
                BriefError::Invalid("Write the headline.".into())
            );
            item.headline = "Parser is blocked".into();
            item.action = "   ".into();
            assert_eq!(
                upsert_item(conn, &item).unwrap_err(),
                BriefError::Invalid("Every item needs its action line.".into())
            );
            item.action = "Ask for a decision.".into();
            item.kind = "later".into();
            assert_eq!(
                upsert_item(conn, &item).unwrap_err(),
                BriefError::Invalid("Use Needs attention or FYI.".into())
            );
            assert!(list(conn, true).unwrap()[0].items.is_empty());
            item.kind = "needs".into();
            upsert_item(conn, &item).unwrap();
            item.headline = "Parser is still blocked".into();
            upsert_item(conn, &item).unwrap();
            let items = &list(conn, true).unwrap()[0].items;
            assert_eq!(items.len(), 1);
            assert_eq!(items[0].headline, "Parser is still blocked");
            assert_eq!(items[0].action, "Ask for a decision.");
        });
    }

    #[test]
    fn second_draft_is_draft_open() {
        with_db("second", |conn| {
            open(conn);
            assert_eq!(
                next_from_last(conn, "2029-03-16", "2029-03-16T09:00:00Z").unwrap_err(),
                BriefError::DraftOpen
            );
            let err = conn
                .execute(
                    "INSERT INTO a1_entries (week_ending, created_at) VALUES ('2029-03-23', 't')",
                    [],
                )
                .unwrap_err();
            assert!(
                err.to_string().contains("a1_one_draft"),
                "second draft must hit a1_one_draft: {err}"
            );
        });
    }

    #[test]
    fn edit_after_send_returns_sent() {
        with_db("sent", |conn| {
            let missing = input("missing", "needs", "No draft", "Ask.");
            assert_eq!(
                upsert_item(conn, &missing).unwrap_err(),
                BriefError::NoDraft
            );
            open(conn);
            set_header(conn, "Payments", "Sam Sample", "Alex", "Bottom line.").unwrap();
            upsert_item(conn, &input("n1", "needs", "Parser is blocked", "Ask.")).unwrap();
            send(conn, "2029-03-09T18:00:00Z").unwrap();

            // State is checked before the field rules, so a blank headline is still Sent.
            let mut blank = input("n1", "nope", "", "");
            assert_eq!(upsert_item(conn, &blank).unwrap_err(), BriefError::Sent);
            blank.headline = "Changed".into();
            blank.action = "Still ask.".into();
            blank.kind = "needs".into();
            assert_eq!(upsert_item(conn, &blank).unwrap_err(), BriefError::Sent);
            assert_eq!(remove_item(conn, "n1").unwrap_err(), BriefError::Sent);
            assert_eq!(move_item(conn, "n1", 3).unwrap_err(), BriefError::Sent);
            assert_eq!(
                set_header(conn, "Other", "Other", "Other", "Other").unwrap_err(),
                BriefError::NoDraft
            );
            let briefs = list(conn, true).unwrap();
            assert_eq!(briefs[0].org_unit.as_deref(), Some("Payments"));
            assert_eq!(briefs[0].bottom_line.as_deref(), Some("Bottom line."));
            assert_eq!(briefs[0].items[0].headline, "Parser is blocked");
            assert_eq!(briefs[0].items[0].position, 0);
        });
    }

    #[test]
    fn list_orders_needs_before_fyi_and_move_stays_in_kind() {
        with_db("order", |conn| {
            open(conn);
            let mut needs_late = input("n2", "needs", "Late need", "Ask for access.");
            needs_late.position = 5;
            needs_late.at_this_stage = 1;
            let mut needs_early = input("n1", "needs", "Early need", "Ask for a decision.");
            needs_early.position = 1;
            let mut fyi_early = input("f1", "fyi", "Shipped", "Milestone.");
            fyi_early.position = 4;
            fyi_early.at_this_stage = 1;
            let mut fyi_later = input("f2", "fyi", "Noted", "Watch the queue.");
            fyi_later.position = 0;
            upsert_item(conn, &needs_late).unwrap();
            upsert_item(conn, &fyi_early).unwrap();
            upsert_item(conn, &needs_early).unwrap();
            upsert_item(conn, &fyi_later).unwrap();

            assert!(list(conn, false).unwrap().is_empty());
            let items = list(conn, true).unwrap();
            let ids: Vec<_> = items[0]
                .items
                .iter()
                .map(|item| item.item_id.as_str())
                .collect();
            assert_eq!(ids, vec!["n1", "n2", "f2", "f1"]);
            assert_eq!(items[0].items[1].at_this_stage, 0);
            assert_eq!(items[0].items[3].at_this_stage, 1);

            move_item(conn, "n2", 0).unwrap();
            let moved = list(conn, true).unwrap();
            let ids: Vec<_> = moved[0]
                .items
                .iter()
                .map(|item| item.item_id.as_str())
                .collect();
            assert_eq!(ids, vec!["n2", "n1", "f2", "f1"]);
            assert_eq!(moved[0].items[0].kind, "needs");
            assert_eq!(moved[0].items[0].position, 0);
            assert_eq!(moved[0].items[3].kind, "fyi");
            assert_eq!(moved[0].items[3].position, 4);
            assert_eq!(
                remove_item(conn, "missing").unwrap_err(),
                BriefError::NotFound
            );
        });
    }

    #[test]
    fn week_ending_uses_the_shared_date_check() {
        with_db("date", |conn| {
            let invalid = BriefError::Invalid("Week ending must be a date like 2029-03-16.".into());
            assert_eq!(
                next_from_last(conn, "9 March 2029", "2029-03-09T09:00:00Z").unwrap_err(),
                invalid
            );
            open(conn);
            let brief = &list(conn, true).unwrap()[0];
            assert_eq!(brief.org_unit, None);
            assert_eq!(brief.bottom_line, None);
            assert!(brief.items.is_empty());
        });
    }
}
