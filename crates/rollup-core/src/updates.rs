//! Leadership updates: one open draft, copied forward from the last sent entry.
//!
//! `carried` lists which of `pct`, `capacity`, `start_date`, and `end_date` were
//! copied and not yet changed by the owner. `is_new` is computed for items of
//! the newest sent entry only and is never stored. NULL stays NULL.
//! A sent entry is a snapshot. [`correct`] is the only edit, and it changes text only.
//! An applied correction records one `lu_corrections` row.

use rusqlite::{params, Connection, OptionalExtension};

/// `Sent`, `DraftOpen`, `NoDraft`, and `Changed` are HTTP 409. `NotFound` is 404.
#[derive(Debug, PartialEq)]
pub enum UpdateError {
    Sent,
    DraftOpen,
    NoDraft,
    Changed,
    NotFound,
    Invalid(String),
    Db(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub id: i64,
    pub entry_date: String,
    pub created_at: String,
    pub sent_at: Option<String>,
    pub programme: Option<String>,
    pub items: Vec<Item>,
    pub total_capacity: Option<i64>,
    pub capacity_label: Option<String>,
    /// True when this sent entry's id is in any `lu_corrections` row. Never on a draft.
    pub corrected: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Item {
    pub id: i64,
    pub entry_id: i64,
    pub item_id: String,
    pub category: String,
    pub position: i64,
    pub title: String,
    pub priority: i64,
    pub start_date: Option<String>,
    pub end_date: Option<String>,
    pub pct: Option<i64>,
    pub capacity: Option<i64>,
    pub carried: Vec<String>,
    pub completed: Vec<String>,
    pub next: Vec<String>,
    pub future: Vec<String>,
    /// True only on the newest entry that has `sent_at` set. Never on a draft.
    pub is_new: bool,
}

/// Full item body for the open draft. `carried` is not owner-writable.
#[derive(Debug, Clone, PartialEq)]
pub struct ItemInput {
    pub item_id: String,
    pub category: String,
    pub position: i64,
    pub title: String,
    pub priority: i64,
    pub start_date: Option<String>,
    pub end_date: Option<String>,
    pub pct: Option<i64>,
    pub capacity: Option<i64>,
    pub completed: Vec<String>,
    pub next: Vec<String>,
    pub future: Vec<String>,
}

const ITEM_SQL: &str = "SELECT i.id, i.entry_id, i.item_id, i.category, i.position, i.title,
    i.priority, i.start_date, i.end_date, i.pct, i.capacity, i.carried, i.completed, i.next, i.future
    FROM lu_items i";

fn db_err(err: impl ToString) -> UpdateError {
    UpdateError::Db(err.to_string())
}

pub(crate) fn require_date(value: &str, what: &str) -> Result<(), UpdateError> {
    let bytes = value.as_bytes();
    let shape = bytes.len() == 10
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && bytes
            .iter()
            .enumerate()
            .all(|(i, byte)| i == 4 || i == 7 || byte.is_ascii_digit());
    // ponytail: day is 01..=31 only; add a days-in-month check if invalid calendar dates matter.
    let in_range = shape
        && value[5..7]
            .parse::<u8>()
            .is_ok_and(|month| (1..=12).contains(&month))
        && value[8..10]
            .parse::<u8>()
            .is_ok_and(|day| (1..=31).contains(&day));
    if in_range {
        Ok(())
    } else {
        Err(UpdateError::Invalid(format!(
            "{what} must be a date like 2029-03-16."
        )))
    }
}

/// Sum of non-null capacities. None when every capacity is null. An explicit 0 counts.
pub fn total_capacity(items: &[Item]) -> Option<i64> {
    items
        .iter()
        .filter_map(|item| item.capacity)
        .reduce(|a, b| a.saturating_add(b))
}

/// None for None. Over 100 names the overflow. Otherwise the gap to 100.
pub fn capacity_label(total: Option<i64>) -> Option<String> {
    let total = total?;
    if total > 100 {
        Some(format!("Over-allocated by {}%", total.saturating_sub(100)))
    } else {
        Some(format!("{}% unallocated", 100i64.saturating_sub(total)))
    }
}

/// Newest first (`entry_date`, then `id`). Items follow category position, then item position.
pub fn list(conn: &Connection, include_draft: bool) -> Result<Vec<Entry>, UpdateError> {
    let newest_sent = newest_sent_id(conn)?;
    let corrected_ids = corrected_entry_ids(conn)?;
    let sql = if include_draft {
        "SELECT id, entry_date, created_at, sent_at, programme FROM lu_entries
         ORDER BY entry_date DESC, id DESC"
    } else {
        "SELECT id, entry_date, created_at, sent_at, programme FROM lu_entries
         WHERE sent_at IS NOT NULL
         ORDER BY entry_date DESC, id DESC"
    };
    let mut stmt = conn.prepare(sql).map_err(db_err)?;
    let heads: Vec<(i64, String, String, Option<String>, Option<String>)> = stmt
        .query_map([], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
        })
        .map_err(db_err)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(db_err)?;

    let mut entries = Vec::with_capacity(heads.len());
    for (id, entry_date, created_at, sent_at, programme) in heads {
        let mut items = load_items(conn, id)?;
        let mark_new = sent_at.is_some() && Some(id) == newest_sent;
        for item in &mut items {
            item.is_new = mark_new;
        }
        let total_capacity = total_capacity(&items);
        let corrected = sent_at.is_some() && corrected_ids.contains(&id);
        entries.push(Entry {
            id,
            entry_date,
            created_at,
            sent_at,
            programme,
            capacity_label: capacity_label(total_capacity),
            total_capacity,
            items,
            corrected,
        });
    }
    Ok(entries)
}

/// Open a draft for `entry_date`. Errors [`UpdateError::DraftOpen`] if one is already open.
/// Copies items off the newest sent entry when there is one. `now` is stored as `created_at`.
pub fn next_from_last(conn: &Connection, entry_date: &str, now: &str) -> Result<i64, UpdateError> {
    require_date(entry_date, "Entry date")?;
    let tx = conn.unchecked_transaction().map_err(db_err)?;
    if draft_id(&tx)?.is_some() {
        return Err(UpdateError::DraftOpen);
    }
    let sent = newest_sent(&tx)?;
    let programme = sent.as_ref().and_then(|row| row.programme.clone());
    tx.execute(
        "INSERT INTO lu_entries (entry_date, created_at, sent_at, programme) VALUES (?1, ?2, NULL, ?3)",
        params![entry_date, now, programme],
    )
    .map_err(|err| {
        if is_one_draft(&err) {
            UpdateError::DraftOpen
        } else {
            db_err(err)
        }
    })?;
    let id = tx.last_insert_rowid();
    if let Some(sent) = sent {
        copy_items(&tx, sent.id, id)?;
    }
    tx.commit().map_err(db_err)?;
    Ok(id)
}

/// Insert or update `item.item_id` on the open draft.
/// A changed pct, capacity, or date leaves `carried`. An unchanged copy stays carried.
pub fn upsert_item(conn: &Connection, item: &ItemInput) -> Result<(), UpdateError> {
    upsert_item_if(conn, item, None)
}

/// [`upsert_item`] with a compare and swap. `None` writes. `Some` returns
/// [`UpdateError::Changed`] and writes nothing when an owner editable field on the
/// draft row differs, or the row is missing. A `Some` naming another item returns
/// [`UpdateError::Invalid`]. The read shares the write transaction.
pub fn upsert_item_if(
    conn: &Connection,
    item: &ItemInput,
    expect: Option<&ItemInput>,
) -> Result<(), UpdateError> {
    if expect.is_some_and(|expect| expect.item_id != item.item_id) {
        return Err(UpdateError::Invalid(
            "expect must describe the same item.".into(),
        ));
    }
    let tx = rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)
        .map_err(db_err)?;
    let Some(draft) = draft_id(&tx)? else {
        if item_on_sent(&tx, &item.item_id)? {
            return Err(UpdateError::Sent);
        }
        return Err(UpdateError::NoDraft);
    };
    if let Some(pct) = item.pct {
        if !(0..=100).contains(&pct) {
            return Err(UpdateError::Invalid(
                "Percent complete must be between 0 and 100.".into(),
            ));
        }
    }
    if let Some(capacity) = item.capacity {
        if capacity < 0 {
            return Err(UpdateError::Invalid("Capacity can't be negative.".into()));
        }
    }
    if let Some(start_date) = &item.start_date {
        require_date(start_date, "Start date")?;
    }
    if let Some(end_date) = &item.end_date {
        require_date(end_date, "End date")?;
    }
    let existing = item_on_entry(&tx, draft, &item.item_id)?;
    if let Some(expect) = expect {
        if !existing.as_ref().is_some_and(|row| same_editable(row, expect)) {
            return Err(UpdateError::Changed);
        }
    }
    if let Some(existing) = existing {
        let carried = carried_after(&existing, item);
        tx.execute(
            "UPDATE lu_items SET category=?1, position=?2, title=?3, priority=?4,
             start_date=?5, end_date=?6, pct=?7, capacity=?8, carried=?9,
             completed=?10, next=?11, future=?12
             WHERE entry_id=?13 AND item_id=?14",
            params![
                item.category,
                item.position,
                item.title,
                item.priority,
                item.start_date,
                item.end_date,
                item.pct,
                item.capacity,
                to_json(&carried)?,
                to_json(&item.completed)?,
                to_json(&item.next)?,
                to_json(&item.future)?,
                draft,
                item.item_id,
            ],
        )
        .map_err(db_err)?;
        tx.commit().map_err(db_err)?;
        return Ok(());
    }
    tx.execute(
        "INSERT INTO lu_items (
            entry_id, item_id, category, position, title, priority,
            start_date, end_date, pct, capacity, carried, completed, next, future
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, '[]', ?11, ?12, ?13)",
        params![
            draft,
            item.item_id,
            item.category,
            item.position,
            item.title,
            item.priority,
            item.start_date,
            item.end_date,
            item.pct,
            item.capacity,
            to_json(&item.completed)?,
            to_json(&item.next)?,
            to_json(&item.future)?,
        ],
    )
    .map_err(db_err)?;
    tx.commit().map_err(db_err)?;
    Ok(())
}

pub fn remove_item(conn: &Connection, item_id: &str) -> Result<(), UpdateError> {
    let draft = draft_owning(conn, item_id)?;
    conn.execute(
        "DELETE FROM lu_items WHERE entry_id=?1 AND item_id=?2",
        params![draft, item_id],
    )
    .map_err(db_err)?;
    Ok(())
}

pub fn move_item(
    conn: &Connection,
    item_id: &str,
    new_category: &str,
    new_position: i64,
) -> Result<(), UpdateError> {
    let draft = draft_owning(conn, item_id)?;
    conn.execute(
        "UPDATE lu_items SET category=?1, position=?2 WHERE entry_id=?3 AND item_id=?4",
        params![new_category, new_position, draft, item_id],
    )
    .map_err(db_err)?;
    Ok(())
}

/// Write the open draft's programme and the shared category order.
/// `None` leaves that part unchanged. No open draft is [`UpdateError::NoDraft`].
/// A blank programme stores NULL. Category names are trimmed; a blank or
/// repeated name is [`UpdateError::Invalid`]. A name that is not stored yet is inserted.
pub fn set_settings(
    conn: &Connection,
    programme: Option<&str>,
    category_order: Option<&[String]>,
) -> Result<(), UpdateError> {
    let names = category_order.map(clean_category_order).transpose()?;
    let tx = conn.unchecked_transaction().map_err(db_err)?;
    let draft = draft_id(&tx)?.ok_or(UpdateError::NoDraft)?;
    if let Some(raw) = programme {
        let value = {
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_string())
            }
        };
        tx.execute(
            "UPDATE lu_entries SET programme=?1 WHERE id=?2 AND sent_at IS NULL",
            params![value, draft],
        )
        .map_err(db_err)?;
    }
    if let Some(names) = names {
        for (position, name) in names.iter().enumerate() {
            let n = tx
                .execute(
                    "UPDATE lu_categories SET position=?1 WHERE name=?2",
                    params![position as i64, name],
                )
                .map_err(db_err)?;
            if n == 0 {
                tx.execute(
                    "INSERT INTO lu_categories (name, position) VALUES (?1, ?2)",
                    params![name, position as i64],
                )
                .map_err(db_err)?;
            }
        }
    }
    tx.commit().map_err(db_err)?;
    Ok(())
}

fn clean_category_order(order: &[String]) -> Result<Vec<String>, UpdateError> {
    let mut names = Vec::with_capacity(order.len());
    for raw in order {
        let name = raw.trim();
        if name.is_empty() {
            return Err(UpdateError::Invalid(
                "Category names can't be blank.".into(),
            ));
        }
        if names.iter().any(|have| have == name) {
            return Err(UpdateError::Invalid(
                "Category names can't repeat.".into(),
            ));
        }
        names.push(name.to_string());
    }
    Ok(names)
}

/// Stamp `sent_at` on the open draft.
pub fn send(conn: &Connection, now: &str) -> Result<(), UpdateError> {
    let id = draft_id(conn)?.ok_or(UpdateError::NoDraft)?;
    let n = conn
        .execute(
            "UPDATE lu_entries SET sent_at=?1 WHERE id=?2 AND sent_at IS NULL",
            params![now, id],
        )
        .map_err(db_err)?;
    if n == 0 {
        return Err(UpdateError::NoDraft);
    }
    Ok(())
}

/// Delete the open draft and its items. Sent entries stay.
pub fn discard(conn: &Connection) -> Result<(), UpdateError> {
    let tx = conn.unchecked_transaction().map_err(db_err)?;
    let id = draft_id(&tx)?.ok_or(UpdateError::NoDraft)?;
    tx.execute("DELETE FROM lu_items WHERE entry_id=?1", params![id])
        .map_err(db_err)?;
    let n = tx
        .execute(
            "DELETE FROM lu_entries WHERE id=?1 AND sent_at IS NULL",
            params![id],
        )
        .map_err(db_err)?;
    if n == 0 {
        return Err(UpdateError::NoDraft);
    }
    tx.commit().map_err(db_err)?;
    Ok(())
}

/// `Preview` counts matches in memory. `Apply` writes them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CorrectMode {
    Preview,
    Apply,
}

/// Rows a correction changes, and the distinct `lu_entries` ids those rows belong to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Correction {
    pub rows: usize,
    pub entry_ids: Vec<i64>,
}

struct ItemText {
    id: i64,
    entry_id: i64,
    title: String,
    completed: Vec<String>,
    next: Vec<String>,
    future: Vec<String>,
}

/// Replace every case-sensitive occurrence of `find` in titles and in each line
/// of `completed`, `next`, and `future`, on sent entries and the draft.
/// One `lu_items` row counts once. `find` is trimmed only to reject a blank search.
/// Preview counts in memory: no `UPDATE` and no write transaction.
/// Apply writes in one transaction. When any row changes it inserts one
/// `lu_corrections` row, attributed to `by` (blank is stored as "owner").
pub fn correct(
    conn: &Connection,
    find: &str,
    replace: &str,
    mode: CorrectMode,
    by: &str,
) -> Result<Correction, UpdateError> {
    if find.trim().is_empty() {
        return Err(UpdateError::Invalid("Type the text to find.".into()));
    }
    if find == replace {
        return Err(UpdateError::Invalid(
            "The replacement is the same text.".into(),
        ));
    }
    match mode {
        CorrectMode::Preview => {
            let changes = changed_items(load_item_text(conn)?, find, replace);
            Ok(correction_of(&changes))
        }
        CorrectMode::Apply => {
            let by = if by.is_empty() { "owner" } else { by };
            let tx = conn.unchecked_transaction().map_err(db_err)?;
            let changes = changed_items(load_item_text(&tx)?, find, replace);
            for change in &changes {
                tx.execute(
                    "UPDATE lu_items SET title=?1, completed=?2, next=?3, future=?4 WHERE id=?5",
                    params![
                        change.title,
                        to_json(&change.completed)?,
                        to_json(&change.next)?,
                        to_json(&change.future)?,
                        change.id,
                    ],
                )
                .map_err(db_err)?;
            }
            let result = correction_of(&changes);
            if result.rows > 0 {
                tx.execute(
                    "INSERT INTO lu_corrections (at, by, find, replace, rows, entry_ids)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    params![
                        crate::store::utcnow(),
                        by,
                        find,
                        replace,
                        result.rows as i64,
                        serde_json::to_string(&result.entry_ids).map_err(db_err)?,
                    ],
                )
                .map_err(db_err)?;
            }
            tx.commit().map_err(db_err)?;
            Ok(result)
        }
    }
}

fn load_item_text(conn: &Connection) -> Result<Vec<ItemText>, UpdateError> {
    let mut stmt = conn
        .prepare("SELECT id, entry_id, title, completed, next, future FROM lu_items ORDER BY id")
        .map_err(db_err)?;
    let rows = stmt
        .query_map([], |row| {
            let completed: String = row.get(3)?;
            let next: String = row.get(4)?;
            let future: String = row.get(5)?;
            Ok(ItemText {
                id: row.get(0)?,
                entry_id: row.get(1)?,
                title: row.get(2)?,
                completed: parse_list(&completed, 3)?,
                next: parse_list(&next, 4)?,
                future: parse_list(&future, 5)?,
            })
        })
        .map_err(db_err)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(db_err)?;
    Ok(rows)
}

fn changed_items(rows: Vec<ItemText>, find: &str, replace: &str) -> Vec<ItemText> {
    let mut changed = Vec::new();
    for row in rows {
        let title = row.title.replace(find, replace);
        let completed = replace_lines(&row.completed, find, replace);
        let next = replace_lines(&row.next, find, replace);
        let future = replace_lines(&row.future, find, replace);
        if title == row.title
            && completed == row.completed
            && next == row.next
            && future == row.future
        {
            continue;
        }
        changed.push(ItemText {
            id: row.id,
            entry_id: row.entry_id,
            title,
            completed,
            next,
            future,
        });
    }
    changed
}

fn correction_of(changes: &[ItemText]) -> Correction {
    let mut entry_ids = Vec::new();
    for change in changes {
        if !entry_ids.contains(&change.entry_id) {
            entry_ids.push(change.entry_id);
        }
    }
    Correction {
        rows: changes.len(),
        entry_ids,
    }
}

fn corrected_entry_ids(conn: &Connection) -> Result<Vec<i64>, UpdateError> {
    let mut stmt = conn
        .prepare("SELECT entry_ids FROM lu_corrections")
        .map_err(db_err)?;
    let raws = stmt
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(db_err)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(db_err)?;
    let mut ids = Vec::new();
    for raw in raws {
        let parsed: Vec<i64> = serde_json::from_str(&raw).map_err(db_err)?;
        for id in parsed {
            if !ids.contains(&id) {
                ids.push(id);
            }
        }
    }
    Ok(ids)
}

fn replace_lines(lines: &[String], find: &str, replace: &str) -> Vec<String> {
    lines
        .iter()
        .map(|line| line.replace(find, replace))
        .collect()
}

struct SentRow {
    id: i64,
    programme: Option<String>,
}

fn newest_sent(conn: &Connection) -> Result<Option<SentRow>, UpdateError> {
    conn.query_row(
        "SELECT id, programme FROM lu_entries
         WHERE sent_at IS NOT NULL
         ORDER BY entry_date DESC, id DESC LIMIT 1",
        [],
        |r| {
            Ok(SentRow {
                id: r.get(0)?,
                programme: r.get(1)?,
            })
        },
    )
    .optional()
    .map_err(db_err)
}

fn newest_sent_id(conn: &Connection) -> Result<Option<i64>, UpdateError> {
    Ok(newest_sent(conn)?.map(|row| row.id))
}

fn draft_id(conn: &Connection) -> Result<Option<i64>, UpdateError> {
    conn.query_row(
        "SELECT id FROM lu_entries WHERE sent_at IS NULL LIMIT 1",
        [],
        |r| r.get(0),
    )
    .optional()
    .map_err(db_err)
}

/// Entry id of the open draft that currently holds `item_id`.
fn draft_owning(conn: &Connection, item_id: &str) -> Result<i64, UpdateError> {
    if let Some(draft) = draft_id(conn)? {
        if item_on_entry(conn, draft, item_id)?.is_some() {
            return Ok(draft);
        }
    }
    if item_on_sent(conn, item_id)? {
        return Err(UpdateError::Sent);
    }
    Err(UpdateError::NotFound)
}

fn item_on_sent(conn: &Connection, item_id: &str) -> Result<bool, UpdateError> {
    let hit: Option<i64> = conn
        .query_row(
            "SELECT 1 FROM lu_items i
             JOIN lu_entries e ON e.id = i.entry_id
             WHERE i.item_id=?1 AND e.sent_at IS NOT NULL LIMIT 1",
            params![item_id],
            |r| r.get(0),
        )
        .optional()
        .map_err(db_err)?;
    Ok(hit.is_some())
}

fn item_on_entry(
    conn: &Connection,
    entry_id: i64,
    item_id: &str,
) -> Result<Option<Item>, UpdateError> {
    conn.query_row(
        &format!("{ITEM_SQL} WHERE i.entry_id=?1 AND i.item_id=?2"),
        params![entry_id, item_id],
        read_item,
    )
    .optional()
    .map_err(db_err)
}

fn load_items(conn: &Connection, entry_id: i64) -> Result<Vec<Item>, UpdateError> {
    // Unknown category names sort after every named category.
    let sql = format!(
        "{ITEM_SQL} WHERE i.entry_id=?1
         ORDER BY COALESCE(
           (SELECT MIN(c.position) FROM lu_categories c WHERE c.name = i.category),
           9223372036854775807
         ), i.position, i.id"
    );
    let mut stmt = conn.prepare(&sql).map_err(db_err)?;
    let items = stmt
        .query_map(params![entry_id], read_item)
        .map_err(db_err)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(db_err)?;
    Ok(items)
}

fn read_item(row: &rusqlite::Row<'_>) -> rusqlite::Result<Item> {
    let carried: String = row.get(11)?;
    let completed: String = row.get(12)?;
    let next: String = row.get(13)?;
    let future: String = row.get(14)?;
    Ok(Item {
        id: row.get(0)?,
        entry_id: row.get(1)?,
        item_id: row.get(2)?,
        category: row.get(3)?,
        position: row.get(4)?,
        title: row.get(5)?,
        priority: row.get(6)?,
        start_date: row.get(7)?,
        end_date: row.get(8)?,
        pct: row.get(9)?,
        capacity: row.get(10)?,
        carried: parse_list(&carried, 11)?,
        completed: parse_list(&completed, 12)?,
        next: parse_list(&next, 13)?,
        future: parse_list(&future, 14)?,
        is_new: false,
    })
}

fn copy_items(conn: &Connection, from_id: i64, to_id: i64) -> Result<(), UpdateError> {
    for item in load_items(conn, from_id)? {
        let carried = carried_of_copy(&item);
        conn.execute(
            "INSERT INTO lu_items (
                entry_id, item_id, category, position, title, priority,
                start_date, end_date, pct, capacity, carried, completed, next, future
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, '[]', '[]', '[]')",
            params![
                to_id,
                item.item_id,
                item.category,
                item.position,
                item.title,
                item.priority,
                item.start_date,
                item.end_date,
                item.pct,
                item.capacity,
                to_json(&carried)?,
            ],
        )
        .map_err(db_err)?;
    }
    Ok(())
}

fn carried_of_copy(item: &Item) -> Vec<String> {
    let mut names = Vec::new();
    if item.pct.is_some() {
        names.push("pct".into());
    }
    if item.capacity.is_some() {
        names.push("capacity".into());
    }
    if item.start_date.is_some() {
        names.push("start_date".into());
    }
    if item.end_date.is_some() {
        names.push("end_date".into());
    }
    names
}

/// Names every `ItemInput` field so a new one fails to compile until this decides; `_` is not owner-editable.
fn same_editable(row: &Item, expect: &ItemInput) -> bool {
    let ItemInput {
        item_id: _,
        title,
        category,
        position,
        priority,
        start_date,
        end_date,
        pct,
        capacity,
        completed,
        next,
        future,
    } = expect;
    &row.title == title
        && &row.category == category
        && row.position == *position
        && row.priority == *priority
        && &row.start_date == start_date
        && &row.end_date == end_date
        && row.pct == *pct
        && row.capacity == *capacity
        && &row.completed == completed
        && &row.next == next
        && &row.future == future
}

fn carried_after(existing: &Item, input: &ItemInput) -> Vec<String> {
    let mut carried = existing.carried.clone();
    if existing.pct != input.pct {
        carried.retain(|field| field != "pct");
    }
    if existing.capacity != input.capacity {
        carried.retain(|field| field != "capacity");
    }
    if existing.start_date != input.start_date {
        carried.retain(|field| field != "start_date");
    }
    if existing.end_date != input.end_date {
        carried.retain(|field| field != "end_date");
    }
    carried
}

fn to_json(values: &[String]) -> Result<String, UpdateError> {
    serde_json::to_string(values).map_err(db_err)
}

fn parse_list(raw: &str, col: usize) -> rusqlite::Result<Vec<String>> {
    serde_json::from_str(raw).map_err(|err| {
        rusqlite::Error::FromSqlConversionFailure(col, rusqlite::types::Type::Text, Box::new(err))
    })
}

fn is_one_draft(err: &rusqlite::Error) -> bool {
    matches!(
        err,
        rusqlite::Error::SqliteFailure(code, Some(msg))
            if code.code == rusqlite::ErrorCode::ConstraintViolation
                && msg.contains("lu_one_draft")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn with_db(name: &str, f: impl FnOnce(&Connection)) {
        static N: AtomicUsize = AtomicUsize::new(0);
        let db = std::env::temp_dir().join(format!(
            "rollup_updates_{name}_{}_{}.db",
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

    fn bare(capacity: Option<i64>) -> Item {
        Item {
            id: 0,
            entry_id: 0,
            item_id: "x".into(),
            category: "Now".into(),
            position: 0,
            title: "t".into(),
            priority: 0,
            start_date: None,
            end_date: None,
            pct: None,
            capacity,
            carried: vec![],
            completed: vec![],
            next: vec![],
            future: vec![],
            is_new: false,
        }
    }

    fn insert_entry(
        conn: &Connection,
        entry_date: &str,
        created_at: &str,
        sent_at: Option<&str>,
    ) -> i64 {
        conn.execute(
            "INSERT INTO lu_entries (entry_date, created_at, sent_at) VALUES (?1, ?2, ?3)",
            params![entry_date, created_at, sent_at],
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    fn input(item_id: &str, title: &str) -> ItemInput {
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

    fn stored_items(conn: &Connection) -> Vec<(i64, String, String, String, String)> {
        let mut stmt = conn
            .prepare("SELECT id, title, completed, next, future FROM lu_items ORDER BY id")
            .unwrap();
        stmt.query_map([], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
            ))
        })
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap()
    }

    fn correction_audit(conn: &Connection) -> Vec<(String, String, String, i64, String)> {
        let mut stmt = conn
            .prepare(
                "SELECT \"by\", find, replace, rows, entry_ids FROM lu_corrections ORDER BY id",
            )
            .unwrap();
        stmt.query_map([], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
            ))
        })
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap()
    }

    #[test]
    fn total_and_label() {
        let all_null = [bare(None), bare(None)];
        assert_eq!(total_capacity(&all_null), None);
        assert_eq!(capacity_label(None), None);

        let zero = [bare(Some(0))];
        assert_eq!(total_capacity(&zero), Some(0));
        assert_eq!(capacity_label(Some(0)), Some("100% unallocated".into()));

        let mixed = [
            bare(Some(25)),
            bare(Some(38)),
            bare(None),
            bare(Some(15)),
            bare(Some(5)),
        ];
        assert_eq!(total_capacity(&mixed), Some(83));
        assert_eq!(capacity_label(Some(83)), Some("17% unallocated".into()));

        let over = [bare(Some(60)), bare(Some(44))];
        assert_eq!(total_capacity(&over), Some(104));
        assert_eq!(
            capacity_label(Some(104)),
            Some("Over-allocated by 4%".into())
        );
    }

    #[test]
    fn next_from_last_carries_and_blocks_a_second_draft() {
        with_db("carry", |conn| {
            let first = next_from_last(conn, "2026-10-01", "2026-10-01T09:00:00Z").unwrap();
            let empty = list(conn, true).unwrap();
            assert_eq!(empty.len(), 1);
            assert!(empty[0].items.is_empty());
            assert_eq!(empty[0].total_capacity, None);

            let mut item = input("pay-1", "Payments");
            item.position = 1;
            item.priority = 2;
            item.pct = Some(40);
            item.capacity = Some(25);
            item.start_date = Some("2026-10-01".into());
            item.end_date = Some("2026-10-31".into());
            item.completed = vec!["shipped the parser".into()];
            item.next = vec!["roll out".into()];
            item.future = vec!["later".into()];
            upsert_item(conn, &item).unwrap();
            conn.execute(
                "UPDATE lu_entries SET programme=?1 WHERE id=?2",
                params!["Weekly", first],
            )
            .unwrap();
            send(conn, "2026-10-01T18:00:00Z").unwrap();

            let draft = next_from_last(conn, "2026-10-08", "2026-10-08T09:00:00Z").unwrap();
            let entries = list(conn, true).unwrap();
            assert_eq!(entries[0].id, draft);
            assert_eq!(entries[0].programme.as_deref(), Some("Weekly"));
            assert!(entries[0].sent_at.is_none());
            let copied = &entries[0].items[0];
            assert_eq!(copied.item_id, "pay-1");
            assert_eq!(copied.title, "Payments");
            assert_eq!(copied.category, "Now");
            assert_eq!(copied.position, 1);
            assert_eq!(copied.priority, 2);
            assert_eq!(copied.pct, Some(40));
            assert_eq!(copied.capacity, Some(25));
            assert_eq!(copied.start_date.as_deref(), Some("2026-10-01"));
            assert_eq!(copied.end_date.as_deref(), Some("2026-10-31"));
            assert_eq!(
                copied.carried,
                vec!["pct", "capacity", "start_date", "end_date"]
            );
            assert!(copied.completed.is_empty());
            assert!(copied.next.is_empty());
            assert!(copied.future.is_empty());
            assert!(!copied.is_new);
            assert_eq!(entries[0].total_capacity, Some(25));
            assert_eq!(
                entries[0].capacity_label.as_deref(),
                Some("75% unallocated")
            );

            item.pct = Some(55);
            item.completed.clear();
            item.next.clear();
            item.future.clear();
            upsert_item(conn, &item).unwrap();
            let edited = &list(conn, true).unwrap()[0].items[0];
            assert_eq!(edited.pct, Some(55));
            assert_eq!(edited.capacity, Some(25));
            assert_eq!(edited.carried, vec!["capacity", "start_date", "end_date"]);

            let err = next_from_last(conn, "2026-10-15", "2026-10-15T09:00:00Z").unwrap_err();
            assert_eq!(err, UpdateError::DraftOpen);
            let hidden = list(conn, false).unwrap();
            assert!(hidden.iter().all(|entry| entry.id != draft));
        });
    }

    #[test]
    fn list_is_newest_first_and_respects_include_draft() {
        with_db("list", |conn| {
            let older = insert_entry(
                conn,
                "2026-10-01",
                "2026-10-01T09:00:00Z",
                Some("2026-10-01T18:00:00Z"),
            );
            let mid = insert_entry(
                conn,
                "2026-10-08",
                "2026-10-08T09:00:00Z",
                Some("2026-10-08T18:00:00Z"),
            );
            // Same date, higher id: sorts ahead of `mid`.
            let mid_later = insert_entry(
                conn,
                "2026-10-08",
                "2026-10-08T10:00:00Z",
                Some("2026-10-08T19:00:00Z"),
            );
            let draft = insert_entry(conn, "2026-10-15", "2026-10-15T09:00:00Z", None);

            let all = list(conn, true).unwrap();
            assert_eq!(
                all.iter().map(|entry| entry.id).collect::<Vec<_>>(),
                vec![draft, mid_later, mid, older]
            );

            let sent = list(conn, false).unwrap();
            assert_eq!(
                sent.iter().map(|entry| entry.id).collect::<Vec<_>>(),
                vec![mid_later, mid, older]
            );
            assert!(sent.iter().all(|entry| entry.sent_at.is_some()));
        });
    }

    #[test]
    fn is_new_follows_the_newest_sent_entry_only() {
        with_db("is_new", |conn| {
            next_from_last(conn, "2026-10-01", "2026-10-01T09:00:00Z").unwrap();
            upsert_item(conn, &input("old", "Older")).unwrap();
            send(conn, "2026-10-01T18:00:00Z").unwrap();

            next_from_last(conn, "2026-10-08", "2026-10-08T09:00:00Z").unwrap();
            upsert_item(conn, &input("new", "Newer")).unwrap();
            send(conn, "2026-10-08T18:00:00Z").unwrap();

            let sent = list(conn, true).unwrap();
            assert!(sent[0].items.iter().all(|item| item.is_new));
            assert!(sent[1].items.iter().all(|item| !item.is_new));
            assert_eq!(sent[0].entry_date, "2026-10-08");

            next_from_last(conn, "2026-10-15", "2026-10-15T09:00:00Z").unwrap();
            upsert_item(conn, &input("draft-only", "Draft only")).unwrap();
            let with_draft = list(conn, true).unwrap();
            assert!(with_draft[0].sent_at.is_none());
            assert!(with_draft[0].items.iter().all(|item| !item.is_new));
            let newest_sent = with_draft
                .iter()
                .find(|entry| entry.sent_at.is_some())
                .unwrap();
            assert!(newest_sent.items.iter().all(|item| item.is_new));
            assert!(with_draft[2].items.iter().all(|item| !item.is_new));
        });
    }

    #[test]
    fn sent_rows_reject_edits_and_upsert_needs_a_draft() {
        with_db("sent", |conn| {
            let err = upsert_item(conn, &input("missing", "No draft")).unwrap_err();
            assert_eq!(err, UpdateError::NoDraft);

            next_from_last(conn, "2026-10-01", "2026-10-01T09:00:00Z").unwrap();
            upsert_item(conn, &input("pay-1", "Payments")).unwrap();
            send(conn, "2026-10-01T18:00:00Z").unwrap();

            let mut edit = input("pay-1", "Changed");
            edit.pct = Some(10);
            assert_eq!(upsert_item(conn, &edit).unwrap_err(), UpdateError::Sent);
            assert_eq!(remove_item(conn, "pay-1").unwrap_err(), UpdateError::Sent);
            assert_eq!(
                move_item(conn, "pay-1", "Later", 3).unwrap_err(),
                UpdateError::Sent
            );

            let entries = list(conn, true).unwrap();
            assert_eq!(entries.len(), 1);
            assert!(entries[0].sent_at.is_some());
            assert_eq!(entries[0].items[0].title, "Payments");
            assert_eq!(entries[0].items[0].category, "Now");
            assert_eq!(entries[0].items[0].pct, None);
            let n: i64 = conn
                .query_row("SELECT COUNT(*) FROM lu_items", [], |r| r.get(0))
                .unwrap();
            assert_eq!(n, 1);
        });
    }

    #[test]
    fn discard_removes_the_draft_and_leaves_the_sent_entry() {
        with_db("discard", |conn| {
            assert_eq!(discard(conn).unwrap_err(), UpdateError::NoDraft);

            next_from_last(conn, "2026-10-01", "2026-10-01T09:00:00Z").unwrap();
            upsert_item(conn, &input("pay-1", "Payments")).unwrap();
            send(conn, "2026-10-01T18:00:00Z").unwrap();
            next_from_last(conn, "2026-10-08", "2026-10-08T09:00:00Z").unwrap();
            upsert_item(conn, &input("pay-2", "Draft only")).unwrap();

            discard(conn).unwrap();
            assert_eq!(discard(conn).unwrap_err(), UpdateError::NoDraft);
            let left = list(conn, true).unwrap();
            assert_eq!(left.len(), 1);
            assert!(left[0].sent_at.is_some());
            assert_eq!(left[0].items.len(), 1);
            assert_eq!(left[0].items[0].item_id, "pay-1");
            assert_eq!(left[0].items[0].title, "Payments");
            let n: i64 = conn
                .query_row("SELECT COUNT(*) FROM lu_items", [], |r| r.get(0))
                .unwrap();
            assert_eq!(n, 1);

            let opened = next_from_last(conn, "2026-10-15", "2026-10-15T09:00:00Z").unwrap();
            let again = list(conn, true).unwrap();
            assert_eq!(again[0].id, opened);
            assert!(again[0].sent_at.is_none());
            assert_eq!(again[1].items[0].item_id, "pay-1");
        });
    }

    #[test]
    fn one_draft_index_rejects_a_second_draft() {
        with_db("index", |conn| {
            conn.execute(
                "INSERT INTO lu_entries (entry_date, created_at, sent_at)
                 VALUES ('2026-09-17', '2026-09-17T09:00:00Z', '2026-09-17T18:00:00Z')",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO lu_entries (entry_date, created_at, sent_at)
                 VALUES ('2026-09-24', '2026-09-24T09:00:00Z', '2026-09-24T18:00:00Z')",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO lu_entries (entry_date, created_at)
                 VALUES ('2026-10-01', '2026-10-01T09:00:00Z')",
                [],
            )
            .unwrap();
            let err = conn.execute(
                "INSERT INTO lu_entries (entry_date, created_at)
                 VALUES ('2026-10-08', '2026-10-08T09:00:00Z')",
                [],
            );
            assert!(err.is_err(), "second draft must hit lu_one_draft");
            let n: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM lu_entries WHERE sent_at IS NULL",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(n, 1);
        });
    }

    #[test]
    fn draft_items_move_remove_and_follow_category_order() {
        with_db("order", |conn| {
            next_from_last(conn, "2026-10-01", "2026-10-01T09:00:00Z").unwrap();
            conn.execute(
                "INSERT INTO lu_categories (name, position) VALUES ('Later', 2)",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO lu_categories (name, position) VALUES ('Now', 1)",
                [],
            )
            .unwrap();
            let dup = conn.execute(
                "INSERT INTO lu_categories (name, position) VALUES ('Now', 9)",
                [],
            );
            assert!(dup.is_err(), "lu_categories name must be unique");
            let mut later = input("b", "Later item");
            later.category = "Later".into();
            later.position = 0;
            let mut now_late = input("a", "Now late");
            now_late.category = "Now".into();
            now_late.position = 5;
            let mut now_early = input("c", "Now early");
            now_early.category = "Now".into();
            now_early.position = 1;
            // No lu_categories row: sorts after every named category.
            let mut elsewhere = input("z", "Elsewhere");
            elsewhere.category = "Elsewhere".into();
            elsewhere.position = 0;
            upsert_item(conn, &later).unwrap();
            upsert_item(conn, &now_late).unwrap();
            upsert_item(conn, &now_early).unwrap();
            upsert_item(conn, &elsewhere).unwrap();
            let listed = list(conn, true).unwrap();
            let ids: Vec<_> = listed[0]
                .items
                .iter()
                .map(|item| item.item_id.as_str())
                .collect();
            assert_eq!(ids, vec!["c", "a", "b", "z"]);

            move_item(conn, "c", "Later", 4).unwrap();
            let moved = list(conn, true).unwrap();
            let c = moved[0]
                .items
                .iter()
                .find(|item| item.item_id == "c")
                .unwrap();
            assert_eq!(c.category, "Later");
            assert_eq!(c.position, 4);
            // Later/4 sorts after Later/0.
            let ids: Vec<_> = moved[0]
                .items
                .iter()
                .map(|item| item.item_id.as_str())
                .collect();
            assert_eq!(ids, vec!["a", "b", "c", "z"]);

            remove_item(conn, "b").unwrap();
            assert_eq!(
                remove_item(conn, "missing").unwrap_err(),
                UpdateError::NotFound
            );
            let left_entries = list(conn, true).unwrap();
            let left: Vec<_> = left_entries[0]
                .items
                .iter()
                .map(|item| item.item_id.as_str())
                .collect();
            assert_eq!(left, vec!["a", "c", "z"]);
        });
    }

    #[test]
    fn upsert_rejects_out_of_range_numbers() {
        with_db("range", |conn| {
            next_from_last(conn, "2026-10-01", "2026-10-01T09:00:00Z").unwrap();
            let mut item = input("pay-1", "Payments");
            item.pct = Some(101);
            assert_eq!(
                upsert_item(conn, &item).unwrap_err(),
                UpdateError::Invalid("Percent complete must be between 0 and 100.".into())
            );
            item.pct = Some(-1);
            assert_eq!(
                upsert_item(conn, &item).unwrap_err(),
                UpdateError::Invalid("Percent complete must be between 0 and 100.".into())
            );
            item.pct = None;
            item.capacity = Some(-1);
            assert_eq!(
                upsert_item(conn, &item).unwrap_err(),
                UpdateError::Invalid("Capacity can't be negative.".into())
            );

            item.pct = Some(0);
            item.capacity = Some(0);
            upsert_item(conn, &item).unwrap();
            let stored = list(conn, true).unwrap();
            assert_eq!(stored[0].items[0].pct, Some(0));
            assert_eq!(stored[0].items[0].capacity, Some(0));

            item.pct = Some(100);
            upsert_item(conn, &item).unwrap();
            let stored = list(conn, true).unwrap();
            assert_eq!(stored[0].items[0].pct, Some(100));
            assert_eq!(stored[0].items[0].capacity, Some(0));
        });
    }

    #[test]
    fn next_from_last_rejects_a_non_date() {
        with_db("bad_entry_date", |conn| {
            let invalid = UpdateError::Invalid("Entry date must be a date like 2029-03-16.".into());
            for bad in ["2026-1-2", "2026-99-99", "2026-13-01", "2026-02-00"] {
                assert_eq!(
                    next_from_last(conn, bad, "2026-01-02T09:00:00Z").unwrap_err(),
                    invalid
                );
            }
            next_from_last(conn, "2026-12-31", "2026-12-31T09:00:00Z").unwrap();
        });
    }

    #[test]
    fn upsert_item_checks_start_date_shape() {
        with_db("start_date", |conn| {
            next_from_last(conn, "2026-10-01", "2026-10-01T09:00:00Z").unwrap();
            let mut item = input("pay-1", "Payments");
            item.start_date = Some("oct 8".into());
            assert_eq!(
                upsert_item(conn, &item).unwrap_err(),
                UpdateError::Invalid("Start date must be a date like 2029-03-16.".into())
            );
            item.start_date = None;
            upsert_item(conn, &item).unwrap();
            assert_eq!(list(conn, true).unwrap()[0].items[0].start_date, None);
        });
    }

    fn categories(conn: &Connection) -> Vec<(String, i64)> {
        let mut stmt = conn
            .prepare("SELECT name, position FROM lu_categories ORDER BY position, name")
            .unwrap();
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .map(|row| row.unwrap())
            .collect()
    }

    #[test]
    fn set_settings_writes_the_open_draft_only() {
        with_db("settings", |conn| {
            let order = vec!["Now".to_string()];
            assert_eq!(
                set_settings(conn, Some("Weekly"), Some(&order)).unwrap_err(),
                UpdateError::NoDraft
            );
            assert!(categories(conn).is_empty());

            next_from_last(conn, "2026-10-01", "2026-10-01T09:00:00Z").unwrap();
            upsert_item(conn, &input("pay-1", "Payments")).unwrap();
            send(conn, "2026-10-01T18:00:00Z").unwrap();
            assert_eq!(
                set_settings(conn, Some("Nope"), None).unwrap_err(),
                UpdateError::NoDraft
            );
            assert_eq!(list(conn, false).unwrap()[0].programme, None);

            next_from_last(conn, "2026-10-08", "2026-10-08T09:00:00Z").unwrap();
            conn.execute(
                "INSERT INTO lu_categories (name, position) VALUES ('Old', 5)",
                [],
            )
            .unwrap();
            set_settings(
                conn,
                Some("  Weekly  "),
                Some(&[" Later ".into(), "Now".into()]),
            )
            .unwrap();
            let open = &list(conn, true).unwrap()[0];
            assert!(open.sent_at.is_none());
            assert_eq!(open.programme.as_deref(), Some("Weekly"));
            assert_eq!(list(conn, false).unwrap()[0].programme, None);
            assert_eq!(
                categories(conn),
                vec![
                    ("Later".into(), 0),
                    ("Now".into(), 1),
                    ("Old".into(), 5),
                ]
            );

            set_settings(conn, Some("  "), None).unwrap();
            assert_eq!(list(conn, true).unwrap()[0].programme, None);
            assert_eq!(categories(conn).len(), 3);

            set_settings(
                conn,
                None,
                Some(&["Now".into(), "Soon".into(), "Later".into()]),
            )
            .unwrap();
            assert_eq!(list(conn, true).unwrap()[0].programme, None);
            assert_eq!(
                categories(conn),
                vec![
                    ("Now".into(), 0),
                    ("Soon".into(), 1),
                    ("Later".into(), 2),
                    ("Old".into(), 5),
                ]
            );

            let err = set_settings(conn, Some("Changed"), Some(&["".into()])).unwrap_err();
            assert_eq!(
                err,
                UpdateError::Invalid("Category names can't be blank.".into())
            );
            assert_eq!(
                set_settings(conn, None, Some(&["   ".into()])).unwrap_err(),
                UpdateError::Invalid("Category names can't be blank.".into())
            );
            assert_eq!(
                set_settings(conn, None, Some(&["Now".into(), " Now ".into()])).unwrap_err(),
                UpdateError::Invalid("Category names can't repeat.".into())
            );
            assert_eq!(list(conn, true).unwrap()[0].programme, None);
            assert_eq!(categories(conn)[0].0, "Now");
        });
    }

    #[test]
    fn sent_item_with_bad_pct_is_sent_not_invalid() {
        with_db("sent_pct", |conn| {
            next_from_last(conn, "2026-10-01", "2026-10-01T09:00:00Z").unwrap();
            upsert_item(conn, &input("pay-1", "Payments")).unwrap();
            send(conn, "2026-10-01T18:00:00Z").unwrap();
            let mut edit = input("pay-1", "Changed");
            edit.pct = Some(101);
            assert_eq!(upsert_item(conn, &edit).unwrap_err(), UpdateError::Sent);
        });
    }

    #[test]
    fn correct_replaces_the_word_in_sent_and_draft_text_only() {
        with_db("correct", |conn| {
            next_from_last(conn, "2026-10-01", "2026-10-01T09:00:00Z").unwrap();
            let mut sent_item = input("pay-1", "vendor and vendor");
            sent_item.priority = 3;
            sent_item.pct = Some(40);
            sent_item.capacity = Some(25);
            sent_item.start_date = Some("2026-10-01".into());
            sent_item.end_date = Some("2026-10-31".into());
            sent_item.completed = vec!["he said \"vendor\" today".into()];
            sent_item.future = vec!["vendor later".into()];
            upsert_item(conn, &sent_item).unwrap();
            send(conn, "2026-10-01T18:00:00Z").unwrap();
            conn.execute(
                "UPDATE lu_entries SET programme='vendor weekly' WHERE entry_date='2026-10-01'",
                [],
            )
            .unwrap();

            next_from_last(conn, "2026-10-08", "2026-10-08T09:00:00Z").unwrap();
            let mut draft_item = input("pay-1", "Payments");
            draft_item.priority = 3;
            draft_item.pct = Some(40);
            draft_item.capacity = Some(25);
            draft_item.start_date = Some("2026-10-01".into());
            draft_item.end_date = Some("2026-10-31".into());
            draft_item.completed = vec!["keep \"quotes\"".into()];
            draft_item.next = vec!["call the vendor".into(), "vendor again".into()];
            upsert_item(conn, &draft_item).unwrap();
            upsert_item(conn, &input("pay-2", "Receipts 40")).unwrap();

            let before = list(conn, true).unwrap();
            assert_eq!(
                correct(conn, "zzzz", "partner", CorrectMode::Apply, "mina")
                    .unwrap()
                    .rows,
                0
            );
            assert_eq!(
                correct(conn, "Vendor", "partner", CorrectMode::Apply, "mina")
                    .unwrap()
                    .rows,
                0
            );
            assert_eq!(
                correct(conn, " vendor ", "partner", CorrectMode::Apply, "mina")
                    .unwrap()
                    .rows,
                0
            );
            assert_eq!(list(conn, true).unwrap(), before);
            assert!(correction_audit(conn).is_empty());
            assert_eq!(
                correct(conn, "", "partner", CorrectMode::Apply, "mina").unwrap_err(),
                UpdateError::Invalid("Type the text to find.".into())
            );
            assert_eq!(
                correct(conn, "   ", "partner", CorrectMode::Apply, "mina").unwrap_err(),
                UpdateError::Invalid("Type the text to find.".into())
            );
            assert_eq!(
                correct(conn, "vendor", "vendor", CorrectMode::Apply, "mina").unwrap_err(),
                UpdateError::Invalid("The replacement is the same text.".into())
            );
            assert_eq!(list(conn, true).unwrap(), before);
            assert!(correction_audit(conn).is_empty());

            let applied = correct(conn, "vendor", "partner", CorrectMode::Apply, "mina").unwrap();
            assert_eq!(applied.rows, 2);
            let after = list(conn, true).unwrap();
            let sent = after.iter().find(|entry| entry.sent_at.is_some()).unwrap();
            let draft = after.iter().find(|entry| entry.sent_at.is_none()).unwrap();
            assert_eq!(sent.programme.as_deref(), Some("vendor weekly"));
            assert_eq!(sent.entry_date, "2026-10-01");
            assert_eq!(sent.created_at, "2026-10-01T09:00:00Z");
            assert_eq!(sent.sent_at.as_deref(), Some("2026-10-01T18:00:00Z"));
            assert_eq!(draft.entry_date, "2026-10-08");
            assert_eq!(draft.programme.as_deref(), Some("vendor weekly"));
            let sent_pay = &sent.items[0];
            assert_eq!(sent_pay.title, "partner and partner");
            assert_eq!(sent_pay.completed, vec!["he said \"partner\" today"]);
            assert_eq!(sent_pay.future, vec!["partner later"]);
            assert!(sent_pay.next.is_empty());
            assert_eq!(sent_pay.pct, Some(40));
            assert_eq!(sent_pay.capacity, Some(25));
            assert_eq!(sent_pay.start_date.as_deref(), Some("2026-10-01"));
            assert_eq!(sent_pay.end_date.as_deref(), Some("2026-10-31"));
            assert_eq!(sent_pay.priority, 3);
            assert_eq!(sent_pay.category, "Now");
            assert!(sent_pay.carried.is_empty());

            let draft_pay = draft
                .items
                .iter()
                .find(|item| item.item_id == "pay-1")
                .unwrap();
            assert_eq!(draft_pay.title, "Payments");
            assert_eq!(
                draft_pay.next,
                vec!["call the partner", "partner again"]
            );
            assert_eq!(draft_pay.completed, vec!["keep \"quotes\""]);
            assert_eq!(draft_pay.pct, Some(40));
            assert_eq!(draft_pay.capacity, Some(25));
            assert_eq!(draft_pay.start_date.as_deref(), Some("2026-10-01"));
            assert_eq!(draft_pay.end_date.as_deref(), Some("2026-10-31"));
            assert_eq!(draft_pay.priority, 3);
            assert_eq!(
                draft_pay.carried,
                vec!["pct", "capacity", "start_date", "end_date"]
            );
            let other = draft
                .items
                .iter()
                .find(|item| item.item_id == "pay-2")
                .unwrap();
            assert_eq!(other.title, "Receipts 40");
            assert_eq!(other.pct, None);
            assert_eq!(other.start_date, None);
            assert_eq!(applied.entry_ids, vec![sent.id, draft.id]);
            assert!(sent.corrected);
            assert!(!draft.corrected);
            let audit = correction_audit(conn);
            assert_eq!(audit.len(), 1);
            assert_eq!(
                audit[0],
                (
                    "mina".to_string(),
                    "vendor".to_string(),
                    "partner".to_string(),
                    2,
                    serde_json::to_string(&applied.entry_ids).unwrap(),
                )
            );
        });
    }

    #[test]
    fn correct_preview_returns_the_apply_count_and_leaves_rows() {
        with_db("correct_preview", |conn| {
            next_from_last(conn, "2026-10-01", "2026-10-01T09:00:00Z").unwrap();
            upsert_item(conn, &input("pay-1", "vendor and vendor")).unwrap();
            send(conn, "2026-10-01T18:00:00Z").unwrap();
            next_from_last(conn, "2026-10-08", "2026-10-08T09:00:00Z").unwrap();
            let mut draft_item = input("pay-1", "Payments");
            draft_item.next = vec!["call the vendor".into(), "vendor again".into()];
            upsert_item(conn, &draft_item).unwrap();

            let before_items = stored_items(conn);
            let writes = conn.total_changes();
            let before = list(conn, true).unwrap();
            let preview = correct(conn, "vendor", "partner", CorrectMode::Preview, "mina").unwrap();
            assert_eq!(preview.rows, 2);
            assert_eq!(stored_items(conn), before_items);
            assert_eq!(conn.total_changes(), writes);
            assert!(correction_audit(conn).is_empty());
            assert_eq!(list(conn, true).unwrap(), before);

            let applied = correct(conn, "vendor", "partner", CorrectMode::Apply, "mina").unwrap();
            assert_eq!(applied.rows, preview.rows);
            assert_eq!(applied.entry_ids, preview.entry_ids);
            let after = list(conn, true).unwrap();
            assert_ne!(after, before);
            let sent = after.iter().find(|entry| entry.sent_at.is_some()).unwrap();
            let draft = after.iter().find(|entry| entry.sent_at.is_none()).unwrap();
            assert_eq!(sent.items[0].title, "partner and partner");
            assert_eq!(draft.items[0].title, "Payments");
            assert_eq!(
                draft.items[0].next,
                vec!["call the partner", "partner again"]
            );
            assert_eq!(applied.entry_ids, vec![sent.id, draft.id]);
            assert!(sent.corrected);
            assert!(!draft.corrected);
            let audit = correction_audit(conn);
            assert_eq!(audit.len(), 1);
            assert_eq!(audit[0].0, "mina");
            assert_eq!(audit[0].3, applied.rows as i64);
            assert_eq!(
                audit[0].4,
                serde_json::to_string(&applied.entry_ids).unwrap()
            );
        });
    }

    #[test]
    fn correct_apply_collapses_two_items_onto_one_entry_id() {
        with_db("correct_entry", |conn| {
            next_from_last(conn, "2026-10-01", "2026-10-01T09:00:00Z").unwrap();
            upsert_item(conn, &input("pay-1", "vendor one")).unwrap();
            upsert_item(conn, &input("pay-2", "vendor two")).unwrap();
            send(conn, "2026-10-01T18:00:00Z").unwrap();
            let id = list(conn, false).unwrap()[0].id;

            assert_eq!(
                correct(conn, "missing", "x", CorrectMode::Apply, "mina")
                    .unwrap()
                    .rows,
                0
            );
            assert!(correction_audit(conn).is_empty());

            let applied = correct(conn, "vendor", "partner", CorrectMode::Apply, "").unwrap();
            assert_eq!(applied.rows, 2);
            assert_eq!(applied.entry_ids, vec![id]);
            let audit = correction_audit(conn);
            assert_eq!(audit.len(), 1);
            assert_eq!(audit[0].0, "owner");
            assert_eq!(audit[0].1, "vendor");
            assert_eq!(audit[0].2, "partner");
            assert_eq!(audit[0].3, 2);
            assert_eq!(audit[0].4, serde_json::to_string(&vec![id]).unwrap());
            let entry = &list(conn, false).unwrap()[0];
            assert!(entry.corrected);
            assert_eq!(entry.items[0].title, "partner one");
            assert_eq!(entry.items[1].title, "partner two");

            assert_eq!(
                correct(conn, "missing", "x", CorrectMode::Apply, "mina")
                    .unwrap()
                    .rows,
                0
            );
            assert_eq!(correction_audit(conn).len(), 1);
        });
    }

    #[test]
    fn upsert_item_if_accepts_a_match_and_refuses_a_stale_expect() {
        with_db("cas", |conn| {
            next_from_last(conn, "2026-10-01", "2026-10-01T09:00:00Z").unwrap();
            let item = input("pay-1", "Payments");
            upsert_item_if(conn, &item, None).unwrap();
            let mut next = item.clone();
            next.pct = Some(40);
            upsert_item_if(conn, &next, Some(&item)).unwrap();
            let mut raced = next.clone();
            raced.title = "Raced".into();
            let mut stale = next.clone();
            stale.future = vec!["later".into()];
            assert_eq!(
                upsert_item_if(conn, &raced, Some(&stale)).unwrap_err(),
                UpdateError::Changed
            );
            let kept = &list(conn, true).unwrap()[0].items[0];
            assert_eq!(
                (kept.title.as_str(), kept.pct, kept.future.len()),
                ("Payments", Some(40), 0)
            );
        });
    }
}
