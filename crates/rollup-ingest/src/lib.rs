//! Ingestion: fixture mode (bundled sample), live mode (Jira + GitHub REST),
//! and tracker mode (the local tracker state + GitHub via the gh CLI, read only).
//!
//! Both paths land in the same idempotent upserts and record sync_state per
//! (source, resource). Live clients are deliberately thin: correct endpoints,
//! auth, and pagination; field names get confirmed against the real
//! instances when credentials exist (PRD-1 Q1/Q4).

use rollup_core::{store, Settings};
use serde_json::Value;

const FIXTURE: &str = include_str!("../../../fixtures/sample.json");

// ------------------------------------------------------------- upserts

pub fn upsert_epic(conn: &store::Conn, epic: &Value) -> rusqlite::Result<usize> {
    conn.execute(
        "INSERT INTO epics (epic_key, board, summary, status) VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(epic_key) DO UPDATE SET
           board=excluded.board, summary=excluded.summary, status=excluded.status",
        rusqlite::params![
            epic["epic_key"].as_str().unwrap_or_default(),
            epic["board"].as_str().unwrap_or_default(),
            epic["summary"].as_str().unwrap_or_default(),
            epic["status"].as_str(),
        ],
    )
}

pub fn upsert_story(conn: &store::Conn, story: &Value) -> rusqlite::Result<usize> {
    conn.execute(
        "INSERT INTO stories (story_key, epic_key, summary, description, status,
                              assignee, sprint, updated_at, raw)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
         ON CONFLICT(story_key) DO UPDATE SET
           epic_key=excluded.epic_key, summary=excluded.summary,
           description=excluded.description, status=excluded.status,
           assignee=excluded.assignee, sprint=excluded.sprint,
           updated_at=excluded.updated_at, raw=excluded.raw",
        rusqlite::params![
            story["story_key"].as_str().unwrap_or_default(),
            story["epic_key"].as_str().unwrap_or_default(),
            story["summary"].as_str().unwrap_or_default(),
            story["description"].as_str().unwrap_or(""),
            story["status"].as_str(),
            story["assignee"].as_str(),
            story["sprint"].as_str(),
            store::utcnow(),
            story.to_string(),
        ],
    )
}

pub fn upsert_pr(conn: &store::Conn, pr: &Value) -> rusqlite::Result<usize> {
    // Fixture entries may carry age_days or merged_days_ago so demo data never goes stale.
    let created_at = match (pr["created_at"].as_str(), pr["age_days"].as_i64()) {
        (Some(ts), _) => Some(ts.to_string()),
        (None, Some(days)) => Some(store::utc_days_ago(days)),
        _ => None,
    };
    let merged_at = match (pr["merged_at"].as_str(), pr["merged_days_ago"].as_i64()) {
        (Some(ts), _) => Some(ts.to_string()),
        (None, Some(days)) => Some(store::utc_days_ago(days)),
        _ => None,
    };
    conn.execute(
        "INSERT INTO pull_requests (id, repo, number, title, body, author, state,
                                    created_at, merged_at, head_branch, base_branch, raw)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
         ON CONFLICT(id) DO UPDATE SET
           title=excluded.title, body=excluded.body, state=excluded.state,
           merged_at=excluded.merged_at, head_branch=excluded.head_branch, raw=excluded.raw",
        rusqlite::params![
            pr["id"].as_str().unwrap_or_default(),
            pr["repo"].as_str().unwrap_or_default(),
            pr["number"].as_i64().unwrap_or_default(),
            pr["title"].as_str().unwrap_or_default(),
            pr["body"].as_str().unwrap_or(""),
            pr["author"].as_str(),
            pr["state"].as_str().unwrap_or("open"),
            created_at,
            merged_at,
            pr["head_branch"].as_str(),
            pr["base_branch"].as_str(),
            pr.to_string(),
        ],
    )
}

pub fn upsert_identity(conn: &store::Conn, person: &Value) -> rusqlite::Result<usize> {
    conn.execute(
        "INSERT INTO identity_map (jira_account_id, github_login, slack_member_id,
                                   display_name, confirmed, role)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT(jira_account_id) DO UPDATE SET
           github_login=excluded.github_login,
           slack_member_id=excluded.slack_member_id,
           display_name=excluded.display_name,
           role=COALESCE(excluded.role, identity_map.role)",
        rusqlite::params![
            person["jira_account_id"].as_str(),
            person["github_login"].as_str(),
            person["slack_member_id"].as_str(),
            person["display_name"].as_str().unwrap_or_default(),
            person["confirmed"].as_bool().map(|b| b as i64).unwrap_or(0),
            rollup_core::connect::known_role(person["role"].as_str()),
        ],
    )
}

fn ensure_repo(conn: &store::Conn, org: &str, repo: &str) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT OR IGNORE INTO repos (org, name) VALUES (?1, ?2)",
        rusqlite::params![org, repo],
    )?;
    Ok(())
}

fn seed_leadership_updates(conn: &store::Conn, data: &Value) -> Result<(), String> {
    let block = &data["leadership_updates"];
    // One transaction: a failed seed leaves nothing half written.
    let tx = conn.unchecked_transaction().map_err(|e| e.to_string())?;
    if let Some(categories) = block["categories"].as_array() {
        for (index, name) in categories.iter().enumerate() {
            let Some(name) = name.as_str() else {
                continue;
            };
            tx.execute(
                "INSERT OR IGNORE INTO lu_categories (name, position) VALUES (?1, ?2)",
                rusqlite::params![name, index as i64],
            )
            .map_err(|e| e.to_string())?;
        }
    }
    for entry in block["entries"].as_array().into_iter().flatten() {
        seed_lu_entry(&tx, entry)?;
    }
    tx.commit().map_err(|e| e.to_string())
}

fn seed_lu_entry(conn: &store::Conn, entry: &Value) -> Result<(), String> {
    use rusqlite::OptionalExtension;
    let (Some(entry_date), Some(created_at)) = (
        entry["entry_date"].as_str(),
        entry["created_at"].as_str(),
    ) else {
        return Ok(());
    };
    let existing: Option<i64> = conn
        .query_row(
            "SELECT id FROM lu_entries WHERE entry_date = ?1 AND created_at = ?2",
            rusqlite::params![entry_date, created_at],
            |row| row.get(0),
        )
        .optional()
        .map_err(|e| e.to_string())?;
    let entry_id = match existing {
        Some(id) => id,
        None => {
            conn.execute(
                "INSERT INTO lu_entries (entry_date, created_at, sent_at, programme)
                 VALUES (?1, ?2, ?3, ?4)",
                rusqlite::params![
                    entry_date,
                    created_at,
                    entry["sent_at"].as_str(),
                    entry["programme"].as_str(),
                ],
            )
            .map_err(|e| e.to_string())?;
            conn.last_insert_rowid()
        }
    };
    let Some(items) = entry["items"].as_array() else {
        return Ok(());
    };
    for item in items {
        let Some(item_id) = item["item_id"].as_str() else {
            return Err(format!("leadership_updates item without item_id in entry {entry_date}"));
        };
        conn.execute(
            "INSERT OR IGNORE INTO lu_items (
                entry_id, item_id, category, position, title, priority,
                start_date, end_date, pct, capacity, carried, completed, next, future
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
            rusqlite::params![
                entry_id,
                item_id,
                item["category"].as_str().unwrap_or_default(),
                item["position"].as_i64().unwrap_or(0),
                item["title"].as_str().unwrap_or_default(),
                item["priority"].as_i64().unwrap_or(0),
                item["start_date"].as_str(),
                item["end_date"].as_str(),
                item["pct"].as_i64(),
                item["capacity"].as_i64(),
                json_list(&item["carried"]),
                json_list(&item["completed"]),
                json_list(&item["next"]),
                json_list(&item["future"]),
            ],
        )
        .map_err(|e| e.to_string())?;
    }
    Ok(())
}

fn json_list(value: &Value) -> String {
    if value.as_array().is_some() {
        value.to_string()
    } else {
        "[]".to_string()
    }
}

// ------------------------------------------------------------- fixture dates

/// Days since 1970-01-01 for a strict real `YYYY-MM-DD`. None for anything
/// else, including a day that does not exist in its month.
fn ymd_days(raw: &str) -> Option<i64> {
    let bytes = raw.as_bytes();
    if bytes.len() != 10
        || bytes[4] != b'-'
        || bytes[7] != b'-'
        || !bytes
            .iter()
            .enumerate()
            .all(|(i, b)| matches!(i, 4 | 7) || b.is_ascii_digit())
    {
        return None;
    }
    let year: i64 = raw[0..4].parse().ok()?;
    let month: u32 = raw[5..7].parse().ok()?;
    let day: u32 = raw[8..10].parse().ok()?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let days = days_from_civil(year, month, day);
    (civil_from_days(days) == (year, month, day)).then_some(days)
}

/// Civil date for days since 1970-01-01 (Howard Hinnant), the inverse of
/// [`days_from_civil`].
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    (if month <= 2 { year + 1 } else { year }, month as u32, day as u32)
}

/// The most recent Friday on or before epoch day `days`. 1970-01-02 (day 1)
/// was a Friday, so Fridays are the days congruent to 1 mod 7.
fn last_friday(days: i64) -> i64 {
    days - (days - 1).rem_euclid(7)
}

/// A strict `YYYY-MM-DD` moved `n` days. None when the input is not a real
/// date; the caller decides what stays unchanged.
fn shift_ymd(raw: &str, n: i64) -> Option<String> {
    let (y, m, d) = civil_from_days(ymd_days(raw)? + n);
    Some(format!("{y:04}-{m:02}-{d:02}"))
}

/// [`shift_ymd`] leaving an unparseable date exactly as written.
fn ymd_shifted(raw: &str, n: i64) -> String {
    shift_ymd(raw, n).unwrap_or_else(|| raw.to_string())
}

/// An RFC3339 string with its date part moved `n` days. The clock and offset
/// stay as written; a date part that is not a real date leaves it unchanged.
fn rfc3339_shifted(raw: &str, n: i64) -> String {
    match raw.get(..10).and_then(|date| shift_ymd(date, n)) {
        Some(shifted) => format!("{shifted}{}", &raw[10..]),
        None => raw.to_string(),
    }
}

/// Replace a string field with `shift(raw)`. Nulls, numbers, and missing
/// fields are left alone.
fn shift_field(row: &mut Value, field: &str, shift: impl Fn(&str) -> String) {
    if let Some(raw) = row[field].as_str() {
        let shifted = shift(raw);
        row[field] = Value::String(shifted);
    }
}

/// Move a fixture section onto the current week. The newest anchor date
/// (`entry_date` for updates, `week_ending` for briefs) lands on the most
/// recent Friday on or before the sync day, and every other date in the
/// section moves by the same whole-day offset, so weekday and spacing
/// relationships hold. A date that does not parse is left exactly as written.
/// Rows an earlier sync seeded on an earlier Friday stay: they read as history.
fn align_section_dates(section: &mut Value, rows: &str, anchor: &str, now_days: i64) {
    let Some(entries) = section[rows].as_array_mut() else {
        return;
    };
    let Some(newest) = entries
        .iter()
        .filter_map(|entry| entry[anchor].as_str().and_then(ymd_days))
        .max()
    else {
        return;
    };
    let offset = last_friday(now_days) - newest;
    for entry in entries.iter_mut() {
        shift_field(entry, anchor, |raw| ymd_shifted(raw, offset));
        for field in ["created_at", "sent_at"] {
            shift_field(entry, field, |raw| rfc3339_shifted(raw, offset));
        }
        for item in entry["items"].as_array_mut().into_iter().flatten() {
            for field in ["start_date", "end_date"] {
                shift_field(item, field, |raw| ymd_shifted(raw, offset));
            }
        }
    }
}

fn seed_partner_briefs(conn: &store::Conn, data: &Value) -> Result<(), String> {
    let block = &data["partner_briefs"];
    // One transaction: a failed seed leaves nothing half written.
    let tx = conn.unchecked_transaction().map_err(|e| e.to_string())?;
    for brief in block["briefs"].as_array().into_iter().flatten() {
        seed_partner_brief(&tx, brief)?;
    }
    tx.commit().map_err(|e| e.to_string())
}

fn seed_partner_brief(conn: &store::Conn, brief: &Value) -> Result<(), String> {
    use rusqlite::OptionalExtension;
    let (Some(week_ending), Some(created_at)) = (
        brief["week_ending"].as_str(),
        brief["created_at"].as_str(),
    ) else {
        return Ok(());
    };
    let existing: Option<i64> = conn
        .query_row(
            "SELECT id FROM a1_entries WHERE week_ending = ?1 AND created_at = ?2",
            rusqlite::params![week_ending, created_at],
            |row| row.get(0),
        )
        .optional()
        .map_err(|e| e.to_string())?;
    let entry_id = match existing {
        Some(id) => id,
        None => {
            conn.execute(
                "INSERT INTO a1_entries (
                    week_ending, created_at, sent_at, org_unit, partner_name, prepared_by, bottom_line
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                rusqlite::params![
                    week_ending,
                    created_at,
                    brief["sent_at"].as_str(),
                    brief["org_unit"].as_str(),
                    brief["partner_name"].as_str(),
                    brief["prepared_by"].as_str(),
                    brief["bottom_line"].as_str(),
                ],
            )
            .map_err(|e| e.to_string())?;
            conn.last_insert_rowid()
        }
    };
    let Some(items) = brief["items"].as_array() else {
        return Ok(());
    };
    for item in items {
        let Some(item_id) = item["item_id"].as_str() else {
            return Err(format!(
                "partner_briefs item without item_id in entry {week_ending}"
            ));
        };
        // OR IGNORE would also swallow the kind CHECK and drop the item silently.
        let kind = item["kind"].as_str().unwrap_or_default();
        if kind != "needs" && kind != "fyi" {
            return Err(format!("partner_briefs item {item_id} has kind {kind:?}, not needs or fyi"));
        }
        conn.execute(
            "INSERT OR IGNORE INTO a1_items (
                entry_id, item_id, kind, position, headline, what, why, action, at_this_stage
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            rusqlite::params![
                entry_id,
                item_id,
                kind,
                item["position"].as_i64().unwrap_or(0),
                item["headline"].as_str().unwrap_or_default(),
                item["what"].as_str().unwrap_or_default(),
                item["why"].as_str().unwrap_or_default(),
                item["action"].as_str().unwrap_or_default(),
                item["at_this_stage"].as_bool().map(|flag| flag as i64).unwrap_or(0),
            ],
        )
        .map_err(|e| e.to_string())?;
    }
    Ok(())
}

// ------------------------------------------------------------- fixture sync

pub fn sync_fixture(conn: &store::Conn, settings: &Settings) -> Result<serde_json::Value, String> {
    let mut data: Value = serde_json::from_str(FIXTURE).map_err(|e| e.to_string())?;
    let org = if settings.github_org.is_empty() {
        "example-org"
    } else {
        &settings.github_org
    };
    for person in data["identity"].as_array().unwrap_or(&vec![]) {
        upsert_identity(conn, person).map_err(|e| e.to_string())?;
    }
    // Mina is the PM persona. Her Today is empty until boards sit on her name,
    // so the seed attaches PAY and PLAT through the same path the Connect page
    // uses: the role rule applies, and a repeat sync attaches nothing new.
    rollup_core::connect::attach_boards(conn, "PAY, PLAT", "mina@firm.example")
        .map_err(|e| e.to_string())?;
    // The rail shows the real current week, so the fixture's frozen dates move
    // onto it: the newest update and the newest brief land on the most recent
    // Friday on or before the sync day (UTC).
    let now = store::utcnow();
    let now_days = ymd_days(now.get(..10).unwrap_or("")).unwrap_or(0);
    align_section_dates(&mut data["leadership_updates"], "entries", "entry_date", now_days);
    align_section_dates(&mut data["partner_briefs"], "briefs", "week_ending", now_days);
    seed_leadership_updates(conn, &data)?;
    seed_partner_briefs(conn, &data)?;
    for epic in data["epics"].as_array().unwrap_or(&vec![]) {
        upsert_epic(conn, epic).map_err(|e| e.to_string())?;
    }
    for story in data["stories"].as_array().unwrap_or(&vec![]) {
        upsert_story(conn, story).map_err(|e| e.to_string())?;
    }
    let mut repos: Vec<String> = settings.github_repos.clone();
    for m in data["repo_epic_map"].as_array().unwrap_or(&vec![]) {
        rollup_core::connect::upsert_fixture_map(
            conn,
            m["repo"].as_str().unwrap_or_default(),
            m["epic_key"].as_str().unwrap_or_default(),
            &store::utcnow(),
        )
        .map_err(|e| e.to_string())?;
    }
    for pr in data["pull_requests"].as_array().unwrap_or(&vec![]) {
        if let Some(r) = pr["repo"].as_str() {
            if !repos.iter().any(|x| x == r) {
                repos.push(r.to_string());
            }
        }
        upsert_pr(conn, pr).map_err(|e| e.to_string())?;
    }
    for repo in &repos {
        ensure_repo(conn, org, repo).map_err(|e| e.to_string())?;
    }
    store::record_sync(conn, "jira", &jira_boards_resource(&settings.jira_boards), "fixture")
        .map_err(|e| e.to_string())?;
    store::record_sync(conn, "github", &format!("repos:{}", repos.join(",")), "fixture")
        .map_err(|e| e.to_string())?;
    
    Ok(serde_json::json!({
        "mode": "fixture",
        "epics": data["epics"].as_array().map(|a| a.len()).unwrap_or(0),
        "stories": data["stories"].as_array().map(|a| a.len()).unwrap_or(0),
        "prs": data["pull_requests"].as_array().map(|a| a.len()).unwrap_or(0),
    }))
}

// ------------------------------------------------------------- live sync

/// A stalled Jira or GitHub socket must not hold the database mutex that /sync keeps across the call.
fn http_client() -> Result<reqwest::blocking::Client, String> {
    reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(20))
        .build()
        .map_err(|e| e.to_string())
}

const JIRA_KEY_RE: &str = "^[A-Z][A-Z0-9_]{0,19}$";
const JIRA_FIELDS: &str = "summary,description,status,assignee,sprint,parent,issuetype,updated";
// ponytail: 20 pages per run (50 issues each). Past that, resume from the newest issue written.
const JIRA_PAGE_CAP: usize = 20;

fn jira_project_key(key: &str) -> bool {
    let bytes = key.as_bytes();
    (1..=20).contains(&bytes.len())
        && bytes[0].is_ascii_uppercase()
        && bytes[1..]
            .iter()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || *c == b'_')
}

/// `-<digits>m`, or exactly `-30d`. Anything else is refused.
fn jira_since_token(since: &str) -> bool {
    if since == "-30d" {
        return true;
    }
    let Some(digits) = since.strip_prefix('-').and_then(|s| s.strip_suffix('m')) else {
        return false;
    };
    !digits.is_empty() && digits.bytes().all(|c| c.is_ascii_digit())
}

/// JQL for validated project keys. `since` is `-<digits>m` or `-30d`.
/// Oldest-first so a capped run's newest written issue is a safe resume point.
fn jira_jql(boards: &[String], since: &str) -> Result<String, String> {
    if boards.is_empty() {
        return Err("Set ROLLUP_JIRA_BOARDS to the Jira project keys to read.".into());
    }
    for key in boards {
        if !jira_project_key(key) {
            return Err(format!(
                "Jira project key \"{key}\" is not valid (expected {JIRA_KEY_RE})"
            ));
        }
    }
    if !jira_since_token(since) {
        return Err(format!(
            "jira since {since:?} is not -<digits>m or -30d"
        ));
    }
    let projects = boards.join(", ");
    Ok(format!(
        "updated >= {since} AND project in ({projects}) AND issuetype in (Epic, Story) \
         ORDER BY updated ASC"
    ))
}

/// Days since 1970-01-01 (Howard Hinnant). `month` is 1-12.
fn days_from_civil(mut year: i64, month: u32, day: u32) -> i64 {
    if month <= 2 {
        year -= 1;
    }
    let era = year.div_euclid(400);
    let yoe = year - era * 400;
    let mp = if month > 2 { month - 3 } else { month + 9 };
    let doy = (153 * mp as i64 + 2) / 5 + day as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Clock body and its offset in seconds east of UTC.
/// `+HH:MM`, `-HH:MM`, `+HHMM`, and `-HHMM` (Jira omits the colon). `Z` is zero.
/// The offset is taken before any fractional seconds so `.123+02:00` is not dropped.
fn split_jira_offset(time: &str) -> Result<(&str, i64), ()> {
    let time = time.strip_suffix('Z').unwrap_or(time);
    let Some(sign_at) = time.as_bytes().iter().rposition(|c| *c == b'+' || *c == b'-') else {
        return Ok((time, 0));
    };
    let (clock, offset) = time.split_at(sign_at);
    if clock.is_empty() {
        return Err(());
    }
    let sign: i64 = if offset.as_bytes()[0] == b'+' { 1 } else { -1 };
    let digits = &offset[1..];
    let (hh, mm) = if let Some((hour, minute)) = digits.split_once(':') {
        if minute.contains(':') {
            return Err(());
        }
        (hour, minute)
    } else if digits.len() == 4 && digits.bytes().all(|b| b.is_ascii_digit()) {
        // `+a€` is 4 bytes and not a char boundary at index 2. ASCII digits only.
        (&digits[..2], &digits[2..])
    } else {
        return Err(());
    };
    if hh.len() != 2
        || mm.len() != 2
        || !hh.bytes().all(|c| c.is_ascii_digit())
        || !mm.bytes().all(|c| c.is_ascii_digit())
    {
        return Err(());
    }
    let hour: i64 = hh.parse().map_err(|_| ())?;
    let minute: i64 = mm.parse().map_err(|_| ())?;
    if hour > 23 || minute > 59 {
        return Err(());
    }
    Ok((clock, sign * (hour * 3_600 + minute * 60)))
}

fn parse_rfc3339_utc(ts: &str) -> Result<i64, String> {
    let raw = ts.trim();
    let (date, time) = raw
        .split_once('T')
        .or_else(|| raw.split_once(' '))
        .ok_or_else(|| format!("jira sync time {raw:?} is not RFC3339 UTC"))?;
    let (clock_body, offset_secs) = split_jira_offset(time)
        .map_err(|_| format!("jira sync time {raw:?} has a bad offset"))?;
    let hms = clock_body
        .split_once('.')
        .map(|(head, _)| head)
        .unwrap_or(clock_body);
    if hms.bytes().any(|c| c == b'+' || c == b'-') {
        return Err(format!("jira sync time {raw:?} has a bad offset"));
    }
    let mut date_parts = date.split('-');
    let year: i64 = date_parts
        .next()
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| format!("jira sync time {raw:?} has no year"))?;
    // days_from_civil's era multiply overflows on a huge i64 year. Reject first.
    if !(1970..=9999).contains(&year) {
        return Err(format!("jira sync time {raw:?} is out of range"));
    }
    let month: u32 = date_parts
        .next()
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| format!("jira sync time {raw:?} has no month"))?;
    let day: u32 = date_parts
        .next()
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| format!("jira sync time {raw:?} has no day"))?;
    if date_parts.next().is_some() {
        return Err(format!("jira sync time {raw:?} has a bad date"));
    }
    let mut clock = hms.split(':');
    let hour: u32 = clock
        .next()
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| format!("jira sync time {raw:?} has no hour"))?;
    let minute: u32 = clock
        .next()
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| format!("jira sync time {raw:?} has no minute"))?;
    let second: u32 = match clock.next() {
        Some(s) if !s.is_empty() => s
            .parse()
            .map_err(|_| format!("jira sync time {raw:?} has a bad second"))?,
        _ => 0,
    };
    if clock.next().is_some()
        || !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || second > 59
    {
        return Err(format!("jira sync time {raw:?} is out of range"));
    }
    let local = days_from_civil(year, month, day)
        .checked_mul(86_400)
        .and_then(|days| days.checked_add(hour as i64 * 3_600 + minute as i64 * 60 + second as i64))
        .ok_or_else(|| format!("jira sync time {raw:?} is out of range"))?;
    local
        .checked_sub(offset_secs)
        .ok_or_else(|| format!("jira sync time {raw:?} is out of range"))
}

/// Minutes from `last_success_at` to `now`, plus a 10-minute overlap, as `-<N>m`.
/// Jira reads an absolute date in the user's zone, so the window stays relative.
/// `N` is at least 10. An old watermark stays `-<N>m` (Jira accepts large minute
/// values). `-30d` is only for a boards resource with no successful sync yet.
fn jira_since_bound(last_success_at: &str, now: &str) -> Result<String, String> {
    let elapsed = parse_rfc3339_utc(now)?.saturating_sub(parse_rfc3339_utc(last_success_at)?);
    let whole = elapsed.div_euclid(60);
    let minutes = if elapsed.rem_euclid(60) == 0 {
        whole
    } else {
        whole + 1
    };
    let n = minutes.saturating_add(10).max(10);
    Ok(format!("-{n}m"))
}

/// Watermark resource for this board set. The lookup and both writes use this
/// string, so a different set does not inherit another set's window.
fn jira_boards_resource(boards: &[String]) -> String {
    format!("boards:{}", boards.join(","))
}

fn last_jira_success_at(conn: &store::Conn, resource: &str) -> Result<Option<String>, String> {
    conn.query_row(
        "SELECT MAX(last_success_at) FROM sync_state
         WHERE source = 'jira' AND status = 'ok' AND resource = ?1",
        [resource],
        |row| row.get(0),
    )
    .map_err(|e| e.to_string())
}

/// Plain strings pass through. ADF yields `text` nodes, with a newline between
/// paragraph-level blocks (paragraph, heading, code block).
fn adf_text(value: &Value) -> String {
    if let Some(text) = value.as_str() {
        return text.to_string();
    }
    let mut out = String::new();
    adf_append(value, &mut out);
    while out.ends_with('\n') {
        out.pop();
    }
    out
}

fn adf_append(value: &Value, out: &mut String) {
    match value {
        Value::String(text) => out.push_str(text),
        Value::Array(nodes) => {
            for node in nodes {
                adf_append(node, out);
            }
        }
        Value::Object(node) => {
            let kind = node.get("type").and_then(Value::as_str).unwrap_or("");
            if kind == "hardBreak" {
                out.push('\n');
                return;
            }
            if let Some(text) = node.get("text").and_then(Value::as_str) {
                out.push_str(text);
            }
            if let Some(content) = node.get("content") {
                adf_append(content, out);
            }
            // After each paragraph-level block. adf_text strips the trailing
            // newline so adjacent blocks are separated by one newline.
            if matches!(kind, "paragraph" | "heading" | "codeBlock") {
                out.push('\n');
            }
        }
        _ => {}
    }
}

struct JiraPage {
    issues: Vec<Value>,
    next_page_token: Option<String>,
    /// True only when the payload says `isLast: true`. Omitted is false.
    is_last: bool,
}

/// One `/search/jql` page. A token is kept only when `isLast` is not true.
/// No token does not by itself mean the walk finished.
fn parse_jira_page(body: &Value) -> JiraPage {
    let issues = body
        .get("issues")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let token = body
        .get("nextPageToken")
        .and_then(Value::as_str)
        .filter(|token| !token.is_empty())
        .map(str::to_string);
    let is_last = body.get("isLast").and_then(Value::as_bool) == Some(true);
    JiraPage {
        issues,
        next_page_token: if is_last { None } else { token },
        is_last,
    }
}

fn write_jira_issue(conn: &store::Conn, issue: &Value) -> Result<usize, String> {
    let Some(key) = issue["key"].as_str().filter(|key| !key.is_empty()) else {
        return Ok(0);
    };
    let fields = &issue["fields"];
    let issuetype = fields["issuetype"]["name"].as_str().unwrap_or("");
    if issuetype.eq_ignore_ascii_case("epic") {
        upsert_epic(
            conn,
            &serde_json::json!({
                "epic_key": key,
                "board": key.split('-').next().unwrap_or(""),
                "summary": fields["summary"].as_str().unwrap_or(""),
                "status": fields["status"]["name"].as_str(),
            }),
        )
        .map_err(|e| e.to_string())?;
        return Ok(1);
    }
    let Some(epic_key) = fields["parent"]["key"].as_str() else {
        return Ok(0);
    };
    upsert_story(
        conn,
        &serde_json::json!({
            "story_key": key,
            "epic_key": epic_key,
            "summary": fields["summary"].as_str().unwrap_or(""),
            "description": adf_text(&fields["description"]),
            "status": fields["status"]["name"].as_str(),
            "assignee": fields["assignee"]["accountId"].as_str(),
            "sprint": Value::Null,
        }),
    )
    .map_err(|e| e.to_string())?;
    Ok(1)
}

#[derive(Debug, PartialEq, Eq)]
enum PageLoop {
    Continue,
    Finished,
    Capped,
}

/// `page_index` is 0-based. `Capped` keeps what was written and resumes from
/// the newest of those times. That is the page cap, and also a page that still
/// has issues when Jira did not set `isLast: true` and sent no next token.
/// Finished is only `is_last`, or a page with no issues and no token.
fn page_loop_done(
    page_index: usize,
    cap: usize,
    next_token: Option<&str>,
    is_last: bool,
    has_issues: bool,
) -> PageLoop {
    let more = matches!(next_token, Some(token) if !token.is_empty());
    if is_last || (!has_issues && !more) {
        PageLoop::Finished
    } else if !more || page_index.saturating_add(1) >= cap {
        PageLoop::Capped
    } else {
        PageLoop::Continue
    }
}

/// One fetched page, reduced to the issues this run actually stored.
/// `has_issues` is the payload array, not how many rows were written.
struct JiraWrittenPage {
    written_updated: Vec<String>,
    next_token: Option<String>,
    is_last: bool,
    has_issues: bool,
}

/// Where `last_success_at` should sit after a bounded page walk.
#[derive(Debug, PartialEq, Eq)]
enum JiraWatermark {
    /// The walk finished. The caller stamps the clock.
    Now,
    /// Later pages were not read. Resume from this UTC instant.
    Resume(String),
}

/// `pages` are in fetch order, each holding `fields.updated` of issues actually
/// written. When the walk finishes, the watermark is [`JiraWatermark::Now`].
/// When it stops early (page cap, or issues with no `isLast` and no next
/// token), the watermark is the newest written time.
fn jira_watermark(pages: &[JiraWrittenPage], cap: usize) -> Result<JiraWatermark, String> {
    let mut seen = 0usize;
    let mut outcome = PageLoop::Finished;
    for (index, page) in pages.iter().enumerate() {
        if index >= cap {
            break;
        }
        seen = index + 1;
        outcome = page_loop_done(
            index,
            cap,
            page.next_token.as_deref(),
            page.is_last,
            page.has_issues,
        );
        if outcome != PageLoop::Continue {
            break;
        }
    }
    match outcome {
        // A finished walk stamps the clock. Issue timestamps are not consulted.
        PageLoop::Finished => Ok(JiraWatermark::Now),
        PageLoop::Continue => {
            Err("jira: stopped while another page of changes was still pending".into())
        }
        PageLoop::Capped => {
            let mut newest: Option<i64> = None;
            for page in pages.iter().take(seen) {
                for ts in &page.written_updated {
                    let epoch = parse_rfc3339_utc(ts)?;
                    newest = Some(newest.map_or(epoch, |have| have.max(epoch)));
                }
            }
            let Some(epoch) = newest else {
                return Err(
                    "jira: stopped before the last page, and no written issue had an updated time to resume from"
                        .into(),
                );
            };
            if epoch < 0 {
                return Err("jira updated time is before the unix epoch".into());
            }
            Ok(JiraWatermark::Resume(store::format_epoch(epoch as u64)))
        }
    }
}

fn sync_jira_live(conn: &store::Conn, settings: &Settings) -> Result<usize, String> {
    let resource = jira_boards_resource(&settings.jira_boards);
    let since = match last_jira_success_at(conn, &resource)? {
        Some(ts) if !ts.trim().is_empty() => jira_since_bound(ts.trim(), &store::utcnow())?,
        _ => "-30d".to_string(),
    };
    let jql = jira_jql(&settings.jira_boards, &since)?;
    let client = http_client()?;
    let url = format!(
        "{}/rest/api/3/search/jql",
        settings.jira_base_url.trim_end_matches('/')
    );
    let mut count = 0usize;
    let mut page_token: Option<String> = None;
    let mut seen = Vec::new();
    for page_index in 0..JIRA_PAGE_CAP {
        let body = {
            let mut params = vec![
                ("jql", jql.as_str()),
                ("maxResults", "50"),
                ("fields", JIRA_FIELDS),
            ];
            if let Some(token) = page_token.as_deref() {
                params.push(("nextPageToken", token));
            }
            let resp = client
                .get(&url)
                .basic_auth(&settings.jira_email, Some(&settings.jira_token))
                .query(&params)
                .send()
                .map_err(|e| e.to_string())?
                .error_for_status()
                .map_err(|e| format!("jira: {e}"))?;
            resp.json().map_err(|e| e.to_string())?
        };
        let page = parse_jira_page(&body);
        let mut written_updated = Vec::new();
        for issue in &page.issues {
            let wrote = write_jira_issue(conn, issue)?;
            count += wrote;
            if wrote > 0 {
                if let Some(updated) = issue["fields"]["updated"].as_str() {
                    written_updated.push(updated.to_string());
                }
            }
        }
        let has_issues = !page.issues.is_empty();
        let next = page.next_page_token.clone();
        let step = page_loop_done(
            page_index,
            JIRA_PAGE_CAP,
            next.as_deref(),
            page.is_last,
            has_issues,
        );
        seen.push(JiraWrittenPage {
            written_updated,
            next_token: next.clone(),
            is_last: page.is_last,
            has_issues,
        });
        match step {
            PageLoop::Continue => page_token = next,
            PageLoop::Finished | PageLoop::Capped => break,
        }
    }
    // Capped resumes at the newest issue written, not the clock. The next
    // `-<N>m` window starts 10 minutes earlier, and upserts make that overlap safe.
    match jira_watermark(&seen, JIRA_PAGE_CAP)? {
        JiraWatermark::Now => {
            store::record_sync(conn, "jira", &resource, &since).map_err(|e| e.to_string())?;
        }
        JiraWatermark::Resume(at) => {
            store::record_sync_at(conn, "jira", &resource, &since, &at)
                .map_err(|e| e.to_string())?;
        }
    }
    Ok(count)
}

fn sync_github_live(conn: &store::Conn, settings: &Settings) -> Result<usize, String> {
    let client = http_client()?;
    let mut count = 0;
    for repo in &settings.github_repos {
        ensure_repo(conn, &settings.github_org, repo).map_err(|e| e.to_string())?;
        let url = format!(
            "https://api.github.com/repos/{}/{}/pulls",
            settings.github_org, repo
        );
        let resp = client
            .get(&url)
            .header("Authorization", format!("Bearer {}", settings.github_token))
            .header("Accept", "application/vnd.github+json")
            .header("User-Agent", "rollup")
            .query(&[
                ("state", "all"),
                ("sort", "updated"),
                ("direction", "desc"),
                ("per_page", "50"),
            ])
            .send()
            
            .map_err(|e| e.to_string())?
            .error_for_status()
            .map_err(|e| format!("github {repo}: {e}"))?;

        let prs: Value = resp.json().map_err(|e| e.to_string())?;
        for pr in prs.as_array().unwrap_or(&vec![]) {
            let state = if pr["merged_at"].is_null() {
                pr["state"].as_str().unwrap_or("open")
            } else {
                "merged"
            };
            upsert_pr(
                conn,
                &serde_json::json!({
                    "id": format!("gh_{repo}_{}", pr["number"].as_i64().unwrap_or(0)),
                    "repo": repo,
                    "number": pr["number"],
                    "title": pr["title"].as_str().unwrap_or(""),
                    "body": pr["body"].as_str().unwrap_or(""),
                    "author": pr["user"]["login"].as_str(),
                    "state": state,
                    "merged_at": pr["merged_at"].clone(),
                    "head_branch": pr["head"]["ref"].as_str(),
                    "base_branch": pr["base"]["ref"].as_str(),
                }),
            )
            .map_err(|e| e.to_string())?;
            count += 1;
        }
        store::record_sync(conn, "github", &format!("repo:{repo}"), &store::utcnow())
            .map_err(|e| e.to_string())?;
    }
    Ok(count)
}

// ------------------------------------------------------------- tracker sync

const GH_SIGN_IN: &str = "GitHub CLI is not signed in. Run gh auth login, then sync again.";
const GH_MISSING: &str = "GitHub CLI (gh) is not installed.";
const GH_TIMEOUT: &str = "gh api timed out after 30 seconds.";
const GH_LIMIT: std::time::Duration = std::time::Duration::from_secs(30);

/// Uppercase slug: letters and digits kept, every other run becomes one hyphen.
fn project_slug(project: &str) -> String {
    let mut slug = String::new();
    let mut dash = false;
    for c in project.chars() {
        if c.is_ascii_alphanumeric() {
            slug.push(c.to_ascii_uppercase());
            dash = false;
        } else if !dash && !slug.is_empty() {
            slug.push('-');
            dash = true;
        }
    }
    if slug.ends_with('-') {
        slug.pop();
    }
    slug
}

fn story_id(value: &Value) -> Option<String> {
    if let Some(n) = value.as_i64() {
        Some(n.to_string())
    } else if let Some(n) = value.as_u64() {
        Some(n.to_string())
    } else {
        let s = value.as_str().unwrap_or("").trim();
        if s.is_empty() { None } else { Some(s.to_string()) }
    }
}

/// done wins, then work in progress, then blocked. Anything else is to do.
fn story_status(row: &Value) -> &'static str {
    let work = row["workStatus"].as_str().unwrap_or("");
    if row["status"].as_str() == Some("done") {
        "Done"
    } else if work == "in-progress" {
        "In Progress"
    } else if work == "blocked" {
        "Blocked"
    } else {
        "To Do"
    }
}

/// All done, else any in progress, else any blocked, else to do.
fn epic_status(group: &[&Value]) -> &'static str {
    if !group.is_empty() && group.iter().all(|row| story_status(row) == "Done") {
        "Done"
    } else if group.iter().any(|row| story_status(row) == "In Progress") {
        "In Progress"
    } else if group.iter().any(|row| story_status(row) == "Blocked") {
        "Blocked"
    } else {
        "To Do"
    }
}

/// Map a local tracker `/api/state` payload to epic and story rows.
/// `added` is not written: the stories table has no created_at column.
fn map_tracker(payload: &Value) -> Result<(Vec<Value>, Vec<Value>), String> {
    let rows = payload["stories"]
        .as_array()
        .ok_or("tracker payload has no stories")?;
    let mut slugs: Vec<String> = Vec::new();
    let mut names: Vec<String> = Vec::new();
    let mut members: Vec<Vec<&Value>> = Vec::new();
    for row in rows {
        if story_id(&row["id"]).is_none() {
            continue;
        }
        let project = row["project"].as_str().unwrap_or("").trim();
        let slug = project_slug(project);
        if slug.is_empty() {
            return Err(format!("The local tracker project '{project}' yields an empty slug."));
        }
        if let Some(i) = slugs.iter().position(|s| s == &slug) {
            if names[i] != project {
                return Err(format!(
                    "The local tracker projects '{}' and '{}' yield the same slug.",
                    names[i], project
                ));
            }
            members[i].push(row);
        } else {
            slugs.push(slug);
            names.push(project.to_string());
            members.push(vec![row]);
        }
    }
    let mut epics = Vec::new();
    let mut stories = Vec::new();
    let mut seen = std::collections::HashSet::<String>::new();
    for ((slug, name), group) in slugs.iter().zip(&names).zip(&members) {
        epics.push(serde_json::json!({
            "epic_key": format!("SD-{slug}"),
            "board": "SD",
            "summary": name,
            "status": epic_status(group),
        }));
        for row in group {
            let Some(id) = story_id(&row["id"]) else { continue };
            let key = format!("SD-{id}");
            if !seen.insert(key.clone()) {
                return Err(format!("duplicate tracker id {key}"));
            }
            stories.push(serde_json::json!({
                "story_key": key,
                "epic_key": format!("SD-{slug}"),
                "summary": row["task"].as_str().unwrap_or(""),
                "description": row["note"].as_str().unwrap_or(""),
                "status": story_status(row),
                "assignee": "",
                "sprint": "",
            }));
        }
    }
    Ok((epics, stories))
}

/// First `story #NN` or `of #NN)` (as in `(6b of #85)`) in the text, as `SD-NN`.
/// A bare `#NN` is not a citation. The keyword matcher still links only when
/// the repo's epic map contains the story.
fn story_key_from_mention(text: &str) -> Option<String> {
    let chars: Vec<char> = text.chars().collect();
    let needle = ['s', 't', 'o', 'r', 'y'];
    let mut i = 0;
    while i < chars.len() {
        if let Some(key) = of_hash_paren(&chars, i) {
            return Some(key);
        }
        let starts = i + needle.len() <= chars.len()
            && (i == 0 || !chars[i - 1].is_ascii_alphanumeric())
            && chars[i..i + needle.len()]
                .iter()
                .zip(needle)
                .all(|(c, n)| c.eq_ignore_ascii_case(&n));
        if starts {
            let mut j = i + needle.len();
            if j < chars.len() && chars[j].is_ascii_whitespace() {
                while j < chars.len() && chars[j].is_ascii_whitespace() {
                    j += 1;
                }
                if j < chars.len() && chars[j] == '#' {
                    j += 1;
                    let start = j;
                    while j < chars.len() && chars[j].is_ascii_digit() {
                        j += 1;
                    }
                    if j > start && (j == chars.len() || !chars[j].is_ascii_alphanumeric()) {
                        let num: String = chars[start..j].iter().collect();
                        return Some(format!("SD-{num}"));
                    }
                }
            }
        }
        i += 1;
    }
    None
}

/// `of #NN)` starting at `i`, word-bounded. `(6b of #85)` is a citation; `of #85` is not.
fn of_hash_paren(chars: &[char], i: usize) -> Option<String> {
    if i + 2 > chars.len() || (i > 0 && chars[i - 1].is_ascii_alphanumeric()) {
        return None;
    }
    if !chars[i].eq_ignore_ascii_case(&'o') || !chars[i + 1].eq_ignore_ascii_case(&'f') {
        return None;
    }
    if i + 2 < chars.len() && chars[i + 2].is_ascii_alphanumeric() {
        return None;
    }
    let mut j = i + 2;
    while j < chars.len() && chars[j].is_ascii_whitespace() {
        j += 1;
    }
    if j == i + 2 || j >= chars.len() || chars[j] != '#' {
        return None;
    }
    j += 1;
    let start = j;
    while j < chars.len() && chars[j].is_ascii_digit() {
        j += 1;
    }
    if j > start && j < chars.len() && chars[j] == ')' {
        let num: String = chars[start..j].iter().collect();
        return Some(format!("SD-{num}"));
    }
    None
}

fn contains_key(text: &str, key: &str) -> bool {
    text.match_indices(key).any(|(i, _)| {
        let bytes = text.as_bytes();
        let before_ok = i == 0 || !bytes[i - 1].is_ascii_alphanumeric();
        let end = i + key.len();
        let after_ok = end >= bytes.len() || !bytes[end].is_ascii_alphanumeric();
        before_ok && after_ok
    })
}

/// Copy `SD-NN` onto the body so the existing keyword pass can see a `story #NN` citation.
fn body_with_story_key(title: &str, body: &str) -> String {
    let Some(key) = story_key_from_mention(&format!("{title}\n{body}")) else {
        return body.to_string();
    };
    if contains_key(&format!("{title}\n{body}"), &key) {
        return body.to_string();
    }
    if body.is_empty() { key } else { format!("{body}\n{key}") }
}

fn github_pr_value(repo: &str, pr: &Value) -> Value {
    let title = pr["title"].as_str().unwrap_or("");
    let body = pr["body"].as_str().unwrap_or("");
    let state = if pr["merged_at"].is_null() {
        pr["state"].as_str().unwrap_or("open")
    } else {
        "merged"
    };
    serde_json::json!({
        "id": format!("gh_{repo}_{}", pr["number"].as_i64().unwrap_or(0)),
        "repo": repo,
        "number": pr["number"].as_i64().unwrap_or(0),
        "title": title,
        "body": body_with_story_key(title, body),
        "author": pr["user"]["login"].as_str(),
        "state": state,
        "created_at": pr["created_at"].as_str(),
        "merged_at": pr["merged_at"].as_str(),
        "head_branch": pr["head"]["ref"].as_str(),
        "base_branch": pr["base"]["ref"].as_str(),
    })
}

/// `gh api` defaults to GET. No method flag, no field flag, no token.
fn gh_get(path: &str) -> Vec<String> {
    vec!["api".to_string(), path.to_string()]
}

/// Missing binary, sign in, timeout, or the first stderr line. Never tokens or the environment.
fn gh_failure(err: &str) -> String {
    if err == GH_TIMEOUT {
        return GH_TIMEOUT.to_string();
    }
    if err == GH_MISSING || err == "gh missing" {
        return GH_MISSING.to_string();
    }
    let lower = err.to_ascii_lowercase();
    if lower.contains("auth") || lower.contains("login") {
        return GH_SIGN_IN.to_string();
    }
    let first = err.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("command failed");
    format!("gh api failed: {first}")
}

/// Token and host overrides the `gh` child must not inherit. Same list `run_gh` scrubs.
const GH_SCRUBBED_ENV: &[&str] = &[
    "GH_TOKEN",
    "GITHUB_TOKEN",
    "GITHUB_PAT",
    "GH_ENTERPRISE_TOKEN",
    "GITHUB_ENTERPRISE_TOKEN",
    "GH_HOST",
    "GH_CONFIG_DIR",
];

/// `gh` invocation with inherited token and host overrides removed.
fn gh_command(args: &[String]) -> std::process::Command {
    let mut command = std::process::Command::new("gh");
    command.args(args);
    for key in GH_SCRUBBED_ENV {
        command.env_remove(key);
    }
    command
}

fn run_gh(args: &[String]) -> Result<String, String> {
    // Drop inherited token and host overrides so gh uses the default host
    // and the login stored by `gh auth login`.
    let output = spawn_bounded(gh_command(args), GH_LIMIT)?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).into_owned());
    }
    String::from_utf8(output.stdout).map_err(|e| e.to_string())
}

/// Piped stdout and stderr, drained on separate threads while the child runs.
/// A full pipe buffer would stall the child so it never exits. On timeout the
/// child is killed and reaped.
fn spawn_bounded(
    mut command: std::process::Command,
    timeout: std::time::Duration,
) -> Result<std::process::Output, String> {
    command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let mut child = command.spawn().map_err(|err| {
        if err.kind() == std::io::ErrorKind::NotFound { "gh missing".to_string() } else { "spawn failed".to_string() }
    })?;
    // Both were set to piped above, so take() always returns Some.
    let (mut stdout, mut stderr) = (child.stdout.take().expect("piped"), child.stderr.take().expect("piped"));
    let started = std::time::Instant::now();
    std::thread::scope(|scope| {
        let out_handle = scope.spawn(move || {
            let mut buf = Vec::new();
            std::io::Read::read_to_end(&mut stdout, &mut buf).map(|_| buf)
        });
        let err_handle = scope.spawn(move || {
            let mut buf = Vec::new();
            std::io::Read::read_to_end(&mut stderr, &mut buf).map(|_| buf)
        });
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) if started.elapsed() >= timeout => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(GH_TIMEOUT.to_string());
                }
                Ok(None) => std::thread::sleep(std::time::Duration::from_millis(20)),
                Err(err) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(err.to_string());
                }
            }
        };
        let stdout = out_handle.join().map_err(|_| "spawn failed".to_string())?.map_err(|e| e.to_string())?;
        let stderr = err_handle.join().map_err(|_| "spawn failed".to_string())?.map_err(|e| e.to_string())?;
        Ok(std::process::Output { status, stdout, stderr })
    })
}

fn gh_call(
    run: &mut impl FnMut(&[String]) -> Result<String, String>,
    args: &[String],
) -> Result<Value, String> {
    let stdout = run(args).map_err(|err| gh_failure(&err))?;
    serde_json::from_str(&stdout).map_err(|e| e.to_string())
}

/// ponytail: one page (100 repos, 50 pulls). Add `gh api --paginate` when a board exceeds that.
fn fetch_github_prs(
    settings: &Settings,
    run: &mut impl FnMut(&[String]) -> Result<String, String>,
) -> Result<(Vec<String>, Vec<Value>), String> {
    if settings.github_org.is_empty() {
        return Err("GitHub organisation is not set.".into());
    }
    let repos = if settings.github_repos.is_empty() {
        let args = gh_get(&format!(
            "orgs/{}/repos?per_page=100&type=all",
            settings.github_org
        ));
        let body = gh_call(run, &args)?;
        let rows = body
            .as_array()
            .ok_or("GitHub returned an unexpected payload.")?;
        rows.iter()
            .filter_map(|row| row["name"].as_str().filter(|name| !name.is_empty()))
            .map(str::to_string)
            .collect()
    } else {
        settings.github_repos.clone()
    };
    let mut prs = Vec::new();
    for repo in &repos {
        let args = gh_get(&format!(
            "repos/{}/{}/pulls?state=all&sort=updated&direction=desc&per_page=50",
            settings.github_org, repo
        ));
        let body = gh_call(run, &args)?;
        let rows = body
            .as_array()
            .ok_or("GitHub returned an unexpected payload.")?;
        for pr in rows {
            prs.push(github_pr_value(repo, pr));
        }
    }
    Ok((repos, prs))
}

/// Direct `cited` link for each PR title or body that names a story which exists.
/// Status `auto` is the matcher's automatically accepted link. Owner decisions win,
/// using the same status strings as `matcher.rs`: skip a PR whose `review_queue`
/// row is `ignored`, and skip a `(pr_id, story_key)` row already `rejected`.
fn link_cited_prs(conn: &store::Conn) -> rusqlite::Result<usize> {
    let rows = {
        let mut stmt = conn.prepare("SELECT id, title, COALESCE(body, '') FROM pull_requests")?;
        let rows = stmt
            .query_map([], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        rows
    };
    let mut n = 0;
    for (id, title, body) in rows {
        let Some(key) = story_key_from_mention(&format!("{title}\n{body}")) else {
            continue;
        };
        n += conn.execute(
            "INSERT INTO pr_story_links
               (pr_id, story_key, score, method, status, runner_ups, created_at)
             SELECT ?1, story_key, 1.0, 'cited', 'auto', '[]', ?3
             FROM stories
             WHERE story_key = ?2
               AND NOT EXISTS (
                 SELECT 1 FROM pr_story_links WHERE pr_id = ?1 AND active = 1
               )
               AND NOT EXISTS (
                 SELECT 1 FROM review_queue WHERE pr_id = ?1 AND status = 'ignored'
               )
               AND NOT EXISTS (
                 SELECT 1 FROM pr_story_links
                 WHERE pr_id = ?1 AND story_key = ?2 AND status = 'rejected'
               )
             ON CONFLICT(pr_id, story_key) DO NOTHING",
            rusqlite::params![id, key, store::utcnow()],
        )?;
    }
    Ok(n)
}

fn fetch_tracker(settings: &Settings) -> Result<Value, String> {
    let client = http_client()?;
    let url = format!(
        "{}/api/state",
        settings.tracker_url.trim_end_matches('/')
    );
    let resp = client
        .get(&url)
        .send()
        .map_err(|e| e.to_string())?
        .error_for_status()
        .map_err(|e| format!("tracker: {e}"))?;
    resp.json().map_err(|e| e.to_string())
}

fn sync_tracker(
    conn: &store::Conn,
    settings: &Settings,
    mut run: impl FnMut(&[String]) -> Result<String, String>,
) -> Result<serde_json::Value, String> {
    if settings.tracker_url.trim().is_empty() || settings.github_org.trim().is_empty() {
        return Err("The local tracker sync needs ROLLUP_TRACKER_URL and ROLLUP_GITHUB_ORG.".into());
    }
    let payload = fetch_tracker(settings)?;
    finish_tracker(conn, settings, &payload, &mut run)
}

fn finish_tracker(
    conn: &store::Conn,
    settings: &Settings,
    payload: &Value,
    run: &mut impl FnMut(&[String]) -> Result<String, String>,
) -> Result<serde_json::Value, String> {
    let (epics, stories) = map_tracker(payload)?;
    for epic in &epics {
        upsert_epic(conn, epic).map_err(|e| e.to_string())?;
    }
    for story in &stories {
        upsert_story(conn, story).map_err(|e| e.to_string())?;
    }
    store::record_sync(conn, "tracker", "state", "tracker").map_err(|e| e.to_string())?;
    let (repos, prs) = fetch_github_prs(settings, run)?;
    for repo in &repos {
        ensure_repo(conn, &settings.github_org, repo).map_err(|e| e.to_string())?;
        for pr in prs.iter().filter(|pr| pr["repo"].as_str() == Some(repo.as_str())) {
            upsert_pr(conn, pr).map_err(|e| e.to_string())?;
        }
        store::record_sync(conn, "github", &format!("repo:{repo}"), &store::utcnow())
            .map_err(|e| e.to_string())?;
    }
    // Same matcher as the sync endpoint. It links SD-NN only when repo_epic_map allows.
    rollup_core::matcher::run_match(conn, settings.t_high, settings.t_low)
        .map_err(|e| e.to_string())?;
    // After that pass: DO NOTHING keeps a keyword row. An unmapped repo never
    // gets one, so a citation still links. One Lab repo serves many projects.
    link_cited_prs(conn).map_err(|e| e.to_string())?;
    Ok(serde_json::json!({
        "mode": "tracker",
        "epics": epics.len(),
        "stories": stories.len(),
        "prs": prs.len(),
    }))
}

pub fn run_sync(conn: &store::Conn, settings: &Settings) -> Result<serde_json::Value, String> {
    if settings.sync_mode == "live" {
        if !(settings.jira_live() && settings.github_live()) {
            return Err("live mode requires Jira and GitHub credentials".into());
        }
        let jira_n = sync_jira_live(conn, settings)?;
        let gh_n = sync_github_live(conn, settings)?;
        return Ok(serde_json::json!({"mode": "live", "jira_issues": jira_n, "prs": gh_n}));
    }
    if settings.sync_mode == "tracker" {
        return sync_tracker(conn, settings, run_gh);
    }
    sync_fixture(conn, settings)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixture_sync_is_idempotent() {
        let dir = std::env::temp_dir().join(format!("rollup_ing_{}", std::process::id()));
        let db = dir.join("t.db");
        store::init_db(&db).unwrap();
        let conn = store::connect(&db).unwrap();
        let settings = Settings::load();
        let a = sync_fixture(&conn, &settings).unwrap();
        let b = sync_fixture(&conn, &settings).unwrap();
        assert_eq!(a["stories"], b["stories"]);
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM stories", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n as u64, b["stories"].as_u64().unwrap());
        let _ = std::fs::remove_file(&db);
    }

    #[test]
    fn fixture_sync_seeds_one_director_update() {
        let dir = std::env::temp_dir().join(format!("rollup_ing_lu_{}", std::process::id()));
        let db = dir.join("t.db");
        for name in ["t.db", "t.db-wal", "t.db-shm"] {
            let _ = std::fs::remove_file(dir.join(name));
        }
        store::init_db(&db).unwrap();
        let conn = store::connect(&db).unwrap();
        let settings = Settings::load();
        sync_fixture(&conn, &settings).unwrap();
        let friday = current_friday();
        let sent_at = format!("{friday}T17:00:00Z");
        rollup_core::updates::next_from_last(&conn, "2029-04-06", "2029-04-06T08:00:00Z").unwrap();
        sync_fixture(&conn, &settings).unwrap();
        let count = |sql: &str| -> i64 { conn.query_row(sql, [], |r| r.get(0)).unwrap() };
        assert_eq!(
            count(&format!(
                "SELECT COUNT(*) FROM lu_entries WHERE entry_date = '{friday}'"
            )),
            1
        );
        assert_eq!(
            count(&format!(
                "SELECT COUNT(*) FROM lu_items i JOIN lu_entries e ON e.id = i.entry_id WHERE e.entry_date = '{friday}'"
            )),
            5
        );
        let listed = rollup_core::updates::list(&conn, false).unwrap();
        let sample = listed.iter().find(|entry| entry.entry_date == friday).unwrap();
        assert_eq!(sample.total_capacity, Some(83));
        assert_eq!(sample.capacity_label.as_deref(), Some("17% unallocated"));
        assert!(sample.items.iter().all(|item| item.is_new));
        assert_eq!(sample.sent_at.as_deref(), Some(sent_at.as_str()));
        assert_eq!(
            sample.items.iter().map(|item| item.priority).collect::<Vec<_>>(),
            vec![1, 1, 0, 0, 0]
        );
        assert_eq!(sample.items[1].start_date, None);
        assert_eq!(sample.items[4].pct, None);
        assert_eq!(sample.items[4].capacity, None);
        assert_eq!(
            sample.items[1].completed[2].as_str(),
            "The UAT completion date has moved from Apr 1 to Apr 15, because a weekly cadence with the partner team has not been possible."
        );
        assert_eq!(
            sample.items[3].next[0].as_str(),
            "Meet Taylor next week to finalise the contract, expected to take about a month."
        );
        let role = |id: &str| -> String {
            conn.query_row(
                "SELECT role FROM identity_map WHERE jira_account_id = ?1",
                [id],
                |r| r.get(0),
            )
            .unwrap()
        };
        assert_eq!(role("dana@firm.example"), "director");
        assert_eq!(role("alex@firm.example"), "partner");
        conn.execute(
            "UPDATE lu_items SET title = 'Owner title' WHERE entry_id = ?1 AND item_id = 'demo-vendor'",
            [sample.id],
        )
        .unwrap();
        sync_fixture(&conn, &settings).unwrap();
        let title: String = conn
            .query_row(
                "SELECT title FROM lu_items WHERE entry_id = ?1 AND item_id = 'demo-vendor'",
                [sample.id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(title, "Owner title");
        assert_eq!(
            count(&format!(
                "SELECT COUNT(*) FROM lu_entries WHERE entry_date = '{friday}'"
            )),
            1
        );
        let _ = std::fs::remove_file(&db);
    }

    #[test]
    fn fixture_sync_seeds_one_partner_brief() {
        let dir = std::env::temp_dir().join(format!("rollup_ing_a1_{}", std::process::id()));
        let db = dir.join("t.db");
        for name in ["t.db", "t.db-wal", "t.db-shm"] {
            let _ = std::fs::remove_file(dir.join(name));
        }
        store::init_db(&db).unwrap();
        let conn = store::connect(&db).unwrap();
        let settings = Settings::load();
        sync_fixture(&conn, &settings).unwrap();
        sync_fixture(&conn, &settings).unwrap();
        let friday = current_friday();
        let count = |sql: &str| -> i64 { conn.query_row(sql, [], |r| r.get(0)).unwrap() };
        assert_eq!(
            count(&format!(
                "SELECT COUNT(*) FROM a1_entries WHERE week_ending = '{friday}'"
            )),
            1
        );
        assert_eq!(
            count(&format!(
                "SELECT COUNT(*) FROM a1_items i JOIN a1_entries e ON e.id = i.entry_id
                 WHERE e.week_ending = '{friday}' AND i.kind = 'needs'"
            )),
            1
        );
        assert_eq!(
            count(&format!(
                "SELECT COUNT(*) FROM a1_items i JOIN a1_entries e ON e.id = i.entry_id
                 WHERE e.week_ending = '{friday}' AND i.kind = 'fyi'"
            )),
            3
        );
        let listed = rollup_core::briefs::list(&conn, false).unwrap();
        assert_eq!(listed.len(), 1);
        let brief = &listed[0];
        assert_eq!(brief.week_ending, friday);
        assert_eq!(brief.partner_name.as_deref(), Some("Alex"));
        assert_eq!(
            brief.bottom_line.as_deref(),
            Some(
                "Two March launches remain on track, and the Example Corp renegotiation is expected to save approximately $50,000 a year. The principal risk is the loss of contractor capacity, which will push delivery dates until replacement staff are confirmed."
            )
        );
        let example_corp = brief
            .items
            .iter()
            .find(|item| item.item_id == "example-corp-savings")
            .unwrap();
        assert_eq!(example_corp.at_this_stage, 1);
        assert_eq!(example_corp.kind, "fyi");
        assert!(brief
            .items
            .iter()
            .filter(|item| item.item_id != "example-corp-savings")
            .all(|item| item.at_this_stage == 0));
        let name: String = conn
            .query_row(
                "SELECT display_name FROM identity_map WHERE jira_account_id = 'alex@firm.example'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(name, "Alex Example");
        let _ = std::fs::remove_file(&db);
    }

    /// The most recent Friday on or before today (UTC), as `YYYY-MM-DD`.
    fn current_friday() -> String {
        let now = store::utcnow();
        let days = ymd_days(now.get(..10).unwrap_or("")).unwrap_or(0);
        let (y, m, d) = civil_from_days(last_friday(days));
        format!("{y:04}-{m:02}-{d:02}")
    }

    #[test]
    fn last_friday_is_the_newest_friday_on_or_before() {
        let ymd = |days: i64| {
            let (y, m, d) = civil_from_days(days);
            format!("{y:04}-{m:02}-{d:02}")
        };
        // 2029-03-09 and 2029-03-16 are the Fridays the fixture is written around.
        for ((year, month, day), expected) in [
            ((2029, 3, 16), "2029-03-16"), // Friday itself
            ((2029, 3, 17), "2029-03-16"), // Saturday
            ((2029, 3, 18), "2029-03-16"), // Sunday
            ((2029, 3, 12), "2029-03-09"), // Monday
            ((1970, 1, 1), "1969-12-26"), // Thursday before the first epoch Friday
        ] {
            let days = days_from_civil(year, month, day);
            assert_eq!(ymd(last_friday(days)), expected);
        }
    }

    #[test]
    fn shift_ymd_moves_real_dates_and_refuses_the_rest() {
        assert_eq!(shift_ymd("2029-03-16", 7).as_deref(), Some("2029-03-23"));
        assert_eq!(shift_ymd("2029-03-16", -7).as_deref(), Some("2029-03-09"));
        assert_eq!(shift_ymd("2029-01-01", -1).as_deref(), Some("2028-12-31"));
        assert_eq!(shift_ymd("2028-02-28", 1).as_deref(), Some("2028-02-29"));
        for bad in [
            "",
            "2029-3-16",
            "2029-02-31",
            "2029-13-01",
            "2029-03-00",
            "16-03-2029",
            "2029-03-16T09",
        ] {
            assert_eq!(shift_ymd(bad, 3), None, "{bad}");
        }
        assert_eq!(ymd_shifted("undated", 3), "undated");
    }

    #[test]
    fn rfc3339_shifted_moves_only_the_date_part() {
        assert_eq!(
            rfc3339_shifted("2029-03-16T09:00:00Z", 7),
            "2029-03-23T09:00:00Z"
        );
        assert_eq!(
            rfc3339_shifted("2029-03-16T17:00:00Z", 0),
            "2029-03-16T17:00:00Z"
        );
        assert_eq!(
            rfc3339_shifted("2029-03-16T17:30:00+02:00", -14),
            "2029-03-02T17:30:00+02:00"
        );
        assert_eq!(rfc3339_shifted("undated", 7), "undated");
        // February 31 is not a real day, so the timestamp is left as written.
        assert_eq!(
            rfc3339_shifted("2029-02-31T09:00:00Z", 7),
            "2029-02-31T09:00:00Z"
        );
    }

    #[test]
    fn fixture_dates_land_on_the_most_recent_friday() {
        let dir = std::env::temp_dir().join(format!("rollup_ing_friday_{}", std::process::id()));
        let db = dir.join("t.db");
        for name in ["t.db", "t.db-wal", "t.db-shm"] {
            let _ = std::fs::remove_file(dir.join(name));
        }
        store::init_db(&db).unwrap();
        let conn = store::connect(&db).unwrap();
        let settings = Settings::load();
        sync_fixture(&conn, &settings).unwrap();
        sync_fixture(&conn, &settings).unwrap();
        let friday = current_friday();
        let count = |sql: &str| -> i64 { conn.query_row(sql, [], |r| r.get(0)).unwrap() };
        assert_eq!(
            count(&format!(
                "SELECT COUNT(*) FROM lu_entries WHERE entry_date = '{friday}'"
            )),
            1
        );
        assert_eq!(
            count(&format!(
                "SELECT COUNT(*) FROM a1_entries WHERE week_ending = '{friday}'"
            )),
            1
        );
        // The clock of a shifted timestamp is kept as written.
        let created: String = conn
            .query_row(
                "SELECT created_at FROM lu_entries WHERE entry_date = ?1",
                [&friday],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(created, format!("{friday}T09:00:00Z"));
        // Item dates move by the same offset: demo-vendor ran 2029-01-08 to
        // 2029-03-16, and its end date is the section's anchor Friday.
        let (start, end): (String, String) = conn
            .query_row(
                "SELECT i.start_date, i.end_date FROM lu_items i
                 JOIN lu_entries e ON e.id = i.entry_id
                 WHERE e.entry_date = ?1 AND i.item_id = 'demo-vendor'",
                [&friday],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(end, friday);
        assert_eq!(start, shift_ymd(&friday, -67).unwrap());
        // A null date stays null.
        assert_eq!(
            count(&format!(
                "SELECT COUNT(*) FROM lu_items i JOIN lu_entries e ON e.id = i.entry_id
                 WHERE e.entry_date = '{friday}' AND i.item_id = 'sample-advisor'
                   AND i.start_date IS NULL AND i.end_date IS NULL"
            )),
            1
        );
        let _ = std::fs::remove_file(&db);
    }

    #[test]
    fn fixture_sync_puts_two_boards_on_mina() {
        let dir = std::env::temp_dir().join(format!("rollup_ing_mina_{}", std::process::id()));
        let db = dir.join("t.db");
        for name in ["t.db", "t.db-wal", "t.db-shm"] {
            let _ = std::fs::remove_file(dir.join(name));
        }
        store::init_db(&db).unwrap();
        let conn = store::connect(&db).unwrap();
        let settings = Settings::load();
        sync_fixture(&conn, &settings).unwrap();
        let boards = || -> Vec<String> {
            let mut stmt = conn
                .prepare(
                    "SELECT key FROM attachments
                     WHERE kind = 'jira_board' AND owner = 'mina@firm.example' ORDER BY key",
                )
                .unwrap();
            let rows = stmt.query_map([], |r| r.get(0)).unwrap();
            rows.collect::<Result<Vec<_>, _>>().unwrap()
        };
        assert_eq!(boards(), vec!["PAY".to_string(), "PLAT".to_string()]);
        sync_fixture(&conn, &settings).unwrap();
        assert_eq!(boards(), vec!["PAY".to_string(), "PLAT".to_string()]);
        let _ = std::fs::remove_file(&db);
    }

    #[test]
    fn merged_at_is_stored_when_the_pr_has_one() {
        let dir = std::env::temp_dir().join(format!("rollup_ing_merge_{}", std::process::id()));
        let db = dir.join("t.db");
        store::init_db(&db).unwrap();
        let conn = store::connect(&db).unwrap();
        upsert_pr(
            &conn,
            &serde_json::json!({
                "id": "PR_dated",
                "repo": "payments-core",
                "number": 9,
                "title": "Dated merge",
                "state": "merged",
                "merged_at": "2026-09-22T10:00:00Z",
            }),
        )
        .unwrap();
        upsert_pr(
            &conn,
            &serde_json::json!({
                "id": "PR_undated",
                "repo": "payments-core",
                "number": 10,
                "title": "Undated merge",
                "state": "merged",
            }),
        )
        .unwrap();
        let dated: Option<String> = conn
            .query_row(
                "SELECT merged_at FROM pull_requests WHERE id = 'PR_dated'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let undated: Option<String> = conn
            .query_row(
                "SELECT merged_at FROM pull_requests WHERE id = 'PR_undated'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(dated.as_deref(), Some("2026-09-22T10:00:00Z"));
        assert_eq!(undated, None);
        let _ = std::fs::remove_file(&db);
    }

    #[test]
    fn merged_days_ago_sets_merged_at_unless_merged_at_is_set() {
        let dir =
            std::env::temp_dir().join(format!("rollup_ing_merge_days_{}", std::process::id()));
        let db = dir.join("t.db");
        let _ = std::fs::remove_file(&db);
        store::init_db(&db).unwrap();
        let conn = store::connect(&db).unwrap();
        upsert_pr(
            &conn,
            &serde_json::json!({
                "id": "PR_days",
                "repo": "payments-core",
                "number": 11,
                "title": "Relative merge",
                "state": "merged",
                "merged_days_ago": 2,
            }),
        )
        .unwrap();
        upsert_pr(
            &conn,
            &serde_json::json!({
                "id": "PR_both",
                "repo": "payments-core",
                "number": 12,
                "title": "Explicit merge",
                "state": "merged",
                "merged_at": "2026-09-22T10:00:00Z",
                "merged_days_ago": 2,
            }),
        )
        .unwrap();
        let days: Option<String> = conn
            .query_row(
                "SELECT merged_at FROM pull_requests WHERE id = 'PR_days'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let both: Option<String> = conn
            .query_row(
                "SELECT merged_at FROM pull_requests WHERE id = 'PR_both'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let expected = store::utc_days_ago(2);
        assert_eq!(
            days.as_deref().and_then(|s| s.split('T').next()),
            expected.split('T').next()
        );
        assert_eq!(both.as_deref(), Some("2026-09-22T10:00:00Z"));
        let _ = std::fs::remove_file(&db);
    }

    #[test]
    fn run_sync_live_mode_requires_credentials() {
        let dir = std::env::temp_dir().join(format!("rollup_ing_live_{}", std::process::id()));
        let db = dir.join("t.db");
        let _ = std::fs::remove_file(&db);
        store::init_db(&db).unwrap();
        let conn = store::connect(&db).unwrap();
        let mut settings = Settings::load();
        settings.sync_mode = "live".into();
        settings.jira_base_url = String::new();
        settings.jira_token = String::new();
        settings.github_token = String::new();
        settings.github_org = String::new();
        let res = run_sync(&conn, &settings);
        assert_eq!(
            res.unwrap_err(),
            "live mode requires Jira and GitHub credentials"
        );
        let _ = std::fs::remove_file(&db);
    }

    const TRACKER_SAMPLE: &str = r#"{
        "title": "Sample board",
        "user": "sam",
        "coreEpics": ["Alpha"],
        "stories": [
            {"id": 1, "task": "Sample story 1", "project": "Alpha", "status": "done", "workStatus": null, "points": 1, "position": 1, "added": "2026-01-01", "comments": "[]", "note": "Done note"},
            {"id": 2, "task": "Sample story 2", "project": "Beta", "status": "pending", "workStatus": "in-progress", "points": "2", "position": 2, "added": "2026-01-02", "comments": "[]", "note": ""},
            {"id": "3", "task": "Sample story 3", "project": "Gamma", "status": "pending", "workStatus": "blocked", "points": 3, "position": 3, "added": "2026-01-03", "comments": "[]", "note": "Waiting"},
            {"id": 4, "task": "Sample story 4", "project": "Gamma", "status": "pending", "workStatus": null, "points": "1", "position": 4, "added": "2026-01-04", "comments": "[]", "note": ""}
        ]
    }"#;

    fn tracker_payload() -> Value {
        serde_json::from_str(TRACKER_SAMPLE).unwrap()
    }

    #[test]
    fn tracker_maps_epic_rollup_and_story_status() {
        let (epics, stories) = map_tracker(&tracker_payload()).unwrap();
        assert!(map_tracker(&serde_json::json!({})).is_err());
        assert_eq!(epics.len(), 3);
        assert_eq!(stories.len(), 4);

        let epic = |key: &str| epics.iter().find(|row| row["epic_key"] == key).unwrap();
        assert_eq!(epic("SD-ALPHA")["board"], "SD");
        assert_eq!(epic("SD-ALPHA")["summary"], "Alpha");
        assert_eq!(epic("SD-ALPHA")["status"], "Done");
        assert_eq!(epic("SD-BETA")["status"], "In Progress");
        assert_eq!(epic("SD-BETA")["summary"], "Beta");
        assert_eq!(epic("SD-GAMMA")["status"], "Blocked");
        assert_eq!(epic("SD-GAMMA")["summary"], "Gamma");

        let story = |key: &str| stories.iter().find(|row| row["story_key"] == key).unwrap();
        assert_eq!(story("SD-1")["status"], "Done");
        assert_eq!(story("SD-1")["epic_key"], "SD-ALPHA");
        assert_eq!(story("SD-1")["summary"], "Sample story 1");
        assert_eq!(story("SD-1")["description"], "Done note");
        assert_eq!(story("SD-1")["assignee"], "");
        assert_eq!(story("SD-1")["sprint"], "");
        assert_eq!(story("SD-2")["status"], "In Progress");
        assert_eq!(story("SD-2")["epic_key"], "SD-BETA");
        assert_eq!(story("SD-3")["status"], "Blocked");
        assert_eq!(story("SD-3")["epic_key"], "SD-GAMMA");
        assert_eq!(story("SD-4")["status"], "To Do");
        assert_eq!(story("SD-4")["epic_key"], "SD-GAMMA");
    }

    #[test]
    fn epic_rollup_blocked_when_none_in_progress() {
        let done = serde_json::json!({"status": "done", "workStatus": null});
        let doing = serde_json::json!({"status": "pending", "workStatus": "in-progress"});
        let blocked = serde_json::json!({"status": "pending", "workStatus": "blocked"});
        let todo = serde_json::json!({"status": "pending", "workStatus": null});
        assert_eq!(epic_status(&[&done, &done]), "Done");
        assert_eq!(epic_status(&[&doing, &blocked]), "In Progress");
        assert_eq!(epic_status(&[&blocked, &todo]), "Blocked");
        assert_eq!(epic_status(&[&todo]), "To Do");
    }

    #[test]
    fn duplicate_tracker_id_names_the_key() {
        let mut payload = tracker_payload();
        payload["stories"].as_array_mut().unwrap().push(serde_json::json!(
            {"id": 1, "task": "Again", "project": "Alpha", "status": "pending"}
        ));
        let err = map_tracker(&payload).unwrap_err();
        assert_eq!(err, "duplicate tracker id SD-1");
    }

    #[test]
    fn empty_slug_names_the_project() {
        let payload = serde_json::json!({"stories": [
            {"id": 1, "task": "kept", "project": "Alpha", "status": "pending"},
            {"id": 2, "task": "lost", "project": "!!!", "status": "pending"}
        ]});
        let err = map_tracker(&payload).unwrap_err();
        assert!(err.contains("!!!"), "{err}");
        assert!(err.contains("empty slug"), "{err}");
    }

    #[test]
    fn shared_slug_names_both_projects() {
        let payload = serde_json::json!({"stories": [
            {"id": 1, "task": "a", "project": "Foo Bar", "status": "pending"},
            {"id": 2, "task": "b", "project": "Foo-Bar", "status": "pending"}
        ]});
        let err = map_tracker(&payload).unwrap_err();
        assert!(err.contains("Foo Bar"), "{err}");
        assert!(err.contains("Foo-Bar"), "{err}");
    }

    #[test]
    fn story_mention_becomes_sd_key() {
        assert_eq!(story_key_from_mention("Sample story #12"), Some("SD-12".into()));
        assert_eq!(story_key_from_mention("Story #7 is next"), Some("SD-7".into()));
        assert_eq!(
            story_key_from_mention("story #12 then story #3"),
            Some("SD-12".into())
        );
        assert_eq!(story_key_from_mention("no citation"), None);
        assert_eq!(story_key_from_mention("history #4"), None);
        assert_eq!(story_key_from_mention("story #12a"), None);
        assert_eq!(story_key_from_mention("story #85"), Some("SD-85".into()));
        assert_eq!(story_key_from_mention("(6b of #85)"), Some("SD-85".into()));
        assert_eq!(story_key_from_mention("fixes #56"), None);
        assert_eq!(story_key_from_mention("#56"), None);
    }

    #[test]
    fn cited_link_writes_existing_story_once() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(store::SCHEMA).unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();
        conn.execute_batch(
            "INSERT INTO epics (epic_key, board, summary) VALUES ('SD-X', 'SD', 'X');
             INSERT INTO stories (story_key, epic_key, summary) VALUES ('SD-85', 'SD-X', 'Exists');
             INSERT INTO pull_requests (id, repo, number, title, body, state) VALUES
               ('pr_yes', 'lab', 1, 'feat: x (6b of #85)', '', 'open'),
               ('pr_no', 'lab', 2, 'story #56', '', 'open');",
        )
        .unwrap();
        assert_eq!(link_cited_prs(&conn).unwrap(), 1);
        assert_eq!(link_cited_prs(&conn).unwrap(), 0);
        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM pr_story_links
                 WHERE pr_id = 'pr_yes' AND story_key = 'SD-85' AND score = 1.0
                   AND method = 'cited' AND status = 'auto' AND runner_ups = '[]'
                   AND created_at IS NOT NULL",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 1);
        let missing: i64 = conn
            .query_row("SELECT COUNT(*) FROM pr_story_links WHERE pr_id = 'pr_no'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(missing, 0);
    }

    #[test]
    fn cited_link_keeps_a_single_matcher_link() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(store::SCHEMA).unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();
        conn.execute_batch(
            "INSERT INTO epics (epic_key, board, summary) VALUES ('SD-X', 'SD', 'X');
             INSERT INTO stories (story_key, epic_key, summary) VALUES
               ('SD-85', 'SD-X', 'Exists'), ('SD-1', 'SD-X', 'Other');
             INSERT INTO pull_requests (id, repo, number, title, body, state) VALUES
               ('pr_linked', 'lab', 1, 'feat: x (6b of #85)', '', 'open');
             INSERT INTO pr_story_links (pr_id, story_key, score, method, status, active)
               VALUES ('pr_linked', 'SD-1', 0.9, 'keyword', 'auto', 1);",
        )
        .unwrap();
        assert_eq!(link_cited_prs(&conn).unwrap(), 0);
        let (n, method): (i64, String) = conn
            .query_row(
                "SELECT COUNT(*), method FROM pr_story_links WHERE pr_id = 'pr_linked' AND active = 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(n, 1);
        assert_eq!(method, "keyword");
    }

    #[test]
    fn cited_link_respects_ignored_pr_and_rejected_pair() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(store::SCHEMA).unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();
        conn.execute_batch(
            "INSERT INTO epics (epic_key, board, summary) VALUES ('SD-X', 'SD', 'X');
             INSERT INTO stories (story_key, epic_key, summary) VALUES ('SD-85', 'SD-X', 'Exists');
             INSERT INTO pull_requests (id, repo, number, title, body, state) VALUES
               ('pr_ignored', 'lab', 1, 'feat: x (6b of #85)', '', 'open'),
               ('pr_rejected', 'lab', 2, 'feat: y (6b of #85)', '', 'open'),
               ('pr_ok', 'lab', 3, 'feat: z (6b of #85)', '', 'open');
             INSERT INTO review_queue (pr_id, status) VALUES ('pr_ignored', 'ignored');
             INSERT INTO pr_story_links (pr_id, story_key, score, method, status, active)
               VALUES ('pr_rejected', 'SD-85', 1.0, 'cited', 'rejected', 0);",
        )
        .unwrap();
        assert_eq!(link_cited_prs(&conn).unwrap(), 1);
        let ignored: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM pr_story_links WHERE pr_id = 'pr_ignored'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(ignored, 0);
        let (rejected, active): (String, i64) = conn
            .query_row(
                "SELECT status, active FROM pr_story_links
                 WHERE pr_id = 'pr_rejected' AND story_key = 'SD-85'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(rejected, "rejected");
        assert_eq!(active, 0);
        let method: String = conn
            .query_row(
                "SELECT method FROM pr_story_links
                 WHERE pr_id = 'pr_ok' AND story_key = 'SD-85' AND status = 'auto' AND active = 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(method, "cited");
    }

    #[test]
    fn gh_command_scrubs_token_and_host_env() {
        let command = gh_command(&["api".into(), "user".into()]);
        let removed: std::collections::BTreeSet<String> = command
            .get_envs()
            .map(|(key, value)| {
                assert!(value.is_none(), "{}", key.to_string_lossy());
                key.to_string_lossy().into_owned()
            })
            .collect();
        let expect: std::collections::BTreeSet<String> = [
            "GH_TOKEN",
            "GITHUB_TOKEN",
            "GITHUB_PAT",
            "GH_ENTERPRISE_TOKEN",
            "GITHUB_ENTERPRISE_TOKEN",
            "GH_HOST",
            "GH_CONFIG_DIR",
        ]
        .into_iter()
        .map(str::to_string)
        .collect();
        assert_eq!(removed, expect);
    }

    fn gh_settings() -> Settings {
        let mut settings = Settings::load();
        settings.github_org = "example-org".into();
        settings.github_repos = vec!["sample-repo".into()];
        settings
    }

    #[test]
    fn failing_gh_command_reports_sign_in() {
        let settings = gh_settings();
        let mut seen = Vec::new();
        let err = fetch_github_prs(&settings, &mut |args| {
            seen = args.to_vec();
            Err("please run gh auth login\nGH_TOKEN=ghp_secret".into())
        })
        .unwrap_err();
        assert_eq!(err, "GitHub CLI is not signed in. Run gh auth login, then sync again.");
        assert!(!err.contains("ghp_secret"));
        assert_eq!(seen.first().map(String::as_str), Some("api"));
        assert!(seen.iter().all(|arg| arg != "-X" && arg != "-f" && !arg.contains("ghp_")));
        assert!(seen.iter().any(|arg| arg.contains("repos/example-org/sample-repo/pulls")));
    }

    #[test]
    fn missing_gh_binary_reports_not_installed() {
        let settings = gh_settings();
        let err = fetch_github_prs(&settings, &mut |_| Err("gh missing".into())).unwrap_err();
        assert_eq!(err, "GitHub CLI (gh) is not installed.");
    }

    #[test]
    fn other_gh_failure_reports_first_stderr_line() {
        let settings = gh_settings();
        let err = fetch_github_prs(&settings, &mut |_| {
            Err("HTTP 502 from api\nGH_TOKEN=ghp_secret\nPATH=/tmp".into())
        })
        .unwrap_err();
        assert_eq!(err, "gh api failed: HTTP 502 from api");
        assert!(!err.contains("ghp_secret"));
        assert!(!err.contains("PATH="));
    }

    #[test]
    fn gh_timeout_maps_to_the_limit_message() {
        let settings = gh_settings();
        let err = fetch_github_prs(&settings, &mut |_| Err(GH_TIMEOUT.into())).unwrap_err();
        assert_eq!(err, "gh api timed out after 30 seconds.");
    }

    #[test]
    fn bounded_spawn_reads_more_than_the_pipe_buffer() {
        let mut command = std::process::Command::new("sh");
        command.args(["-c", "head -c 300000 /dev/zero | tr '\\0' x"]);
        let output = spawn_bounded(command, GH_LIMIT).expect("bounded command");
        assert!(output.status.success());
        assert!(output.stdout.len() > 200_000);
    }

    #[test]
    fn bounded_spawn_stops_when_the_deadline_passes() {
        let mut command = std::process::Command::new("sleep");
        command.arg("30");
        let started = std::time::Instant::now();
        let err = spawn_bounded(command, std::time::Duration::from_millis(300)).unwrap_err();
        assert_eq!(err, GH_TIMEOUT);
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
    }

    #[test]
    fn tracker_mode_requires_url_and_org_before_write() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(store::SCHEMA).unwrap();
        let mut settings = Settings::load();
        settings.sync_mode = "tracker".into();
        let msg = "The local tracker sync needs ROLLUP_TRACKER_URL and ROLLUP_GITHUB_ORG.";
        for (url, org) in [(" ", "example-org"), ("http://127.0.0.1:9", " ")] {
            settings.tracker_url = url.into();
            settings.github_org = org.into();
            assert_eq!(run_sync(&conn, &settings).unwrap_err(), msg);
        }
        let n: i64 = conn.query_row("SELECT COUNT(*) FROM epics", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    fn tracker_links_mention_when_repo_map_allows() {
        let dir = std::env::temp_dir().join(format!("rollup_ing_sd_{}", std::process::id()));
        let db = dir.join("t.db");
        let _ = std::fs::remove_file(&db);
        store::init_db(&db).unwrap();
        let conn = store::connect(&db).unwrap();
        conn.execute(
            "INSERT INTO repo_epic_map (repo, epic_key) VALUES ('sample-repo', 'SD-BETA')",
            [],
        )
        .unwrap();
        let mut settings = Settings::load();
        settings.github_org = "example-org".into();
        settings.github_repos = vec!["sample-repo".into()];
        settings.t_high = 0.55;
        settings.t_low = 0.35;
        let pulls = r#"[{
            "number": 8,
            "title": "Sample story #2",
            "body": "Notes for the sample.",
            "user": {"login": "sam"},
            "state": "open",
            "merged_at": null,
            "created_at": "2026-03-03T00:00:00Z",
            "head": {"ref": "sample-branch"},
            "base": {"ref": "main"}
        }]"#;
        let summary = finish_tracker(&conn, &settings, &tracker_payload(), &mut |_| Ok(pulls.into()))
            .unwrap();
        assert_eq!(summary["mode"], "tracker");
        assert_eq!(summary["epics"], 3);
        assert_eq!(summary["stories"], 4);
        assert_eq!(summary["prs"], 1);
        let (story, method): (String, String) = conn
            .query_row(
                "SELECT story_key, method FROM pr_story_links WHERE pr_id = 'gh_sample-repo_8' AND active = 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(story, "SD-2");
        assert_eq!(method, "keyword");
        let body: String = conn
            .query_row(
                "SELECT body FROM pull_requests WHERE id = 'gh_sample-repo_8'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(body.contains("SD-2"));
        assert!(body.contains("Notes for the sample."));
        let _ = std::fs::remove_file(&db);
    }

    #[test]
    fn jira_jql_two_boards_and_since() {
        let boards = vec!["PAY".to_string(), "CORE".to_string()];
        let jql = jira_jql(&boards, "-30m").unwrap();
        assert_eq!(
            jql,
            "updated >= -30m AND project in (PAY, CORE) AND issuetype in (Epic, Story) ORDER BY updated ASC"
        );
        let fresh = jira_jql(&boards, "-30d").unwrap();
        assert_eq!(
            fresh,
            "updated >= -30d AND project in (PAY, CORE) AND issuetype in (Epic, Story) ORDER BY updated ASC"
        );
        let err = jira_jql(&boards, "2026-10-19 14:20").unwrap_err();
        assert!(err.contains("2026-10-19 14:20"), "{err}");
        assert!(!err.contains("issuetype"), "{err}");
    }

    #[test]
    fn jira_jql_refuses_bad_key() {
        let bad = "pay\") OR project = PAY OR summary ~ \"";
        let err = jira_jql(&[bad.to_string()], "-30m").unwrap_err();
        assert!(err.contains(bad), "{err}");
        assert!(!err.contains("issuetype"), "{err}");
    }

    #[test]
    fn jira_jql_refuses_empty_boards() {
        let err = jira_jql(&[], "-30d").unwrap_err();
        assert_eq!(
            err,
            "Set ROLLUP_JIRA_BOARDS to the Jira project keys to read."
        );
    }

    #[test]
    fn adf_text_paragraphs_and_plain_string() {
        let doc = serde_json::json!({
            "type": "doc",
            "version": 1,
            "content": [
                {"type": "paragraph", "content": [
                    {"type": "text", "text": "First "},
                    {"type": "text", "text": "line", "marks": [{"type": "strong"}]}
                ]},
                {"type": "paragraph", "content": [
                    {"type": "text", "text": "Second"}
                ]}
            ]
        });
        assert_eq!(adf_text(&doc), "First line\nSecond");
        assert_eq!(adf_text(&serde_json::json!("plain")), "plain");
    }

    #[test]
    fn jira_two_page_parse() {
        let page1 = serde_json::json!({
            "issues": [{"key": "PAY-1"}],
            "nextPageToken": "tok-2",
            "isLast": false
        });
        let page2 = serde_json::json!({
            "issues": [{"key": "CORE-2"}],
            "nextPageToken": "tok-3",
            "isLast": true
        });
        let first = parse_jira_page(&page1);
        assert!(!first.is_last);
        assert_eq!(first.next_page_token.as_deref(), Some("tok-2"));
        assert_eq!(first.issues[0]["key"], "PAY-1");
        let second = parse_jira_page(&page2);
        assert!(second.is_last);
        assert_eq!(second.next_page_token, None);
        assert_eq!(second.issues[0]["key"], "CORE-2");
        let absent = parse_jira_page(&serde_json::json!({
            "issues": [{"key": "PAY-3"}],
            "isLast": false
        }));
        assert!(!absent.is_last);
        assert_eq!(absent.next_page_token, None);
        assert_eq!(absent.issues[0]["key"], "PAY-3");
        assert_eq!(
            page_loop_done(
                0,
                20,
                absent.next_page_token.as_deref(),
                absent.is_last,
                !absent.issues.is_empty()
            ),
            PageLoop::Capped
        );
        // Omitted isLast is the same incomplete read: issues, no token.
        let omitted = parse_jira_page(&serde_json::json!({
            "issues": [{"key": "PAY-4"}]
        }));
        assert!(!omitted.is_last);
        assert!(omitted.next_page_token.is_none());
        assert_eq!(
            page_loop_done(
                0,
                20,
                omitted.next_page_token.as_deref(),
                omitted.is_last,
                !omitted.issues.is_empty()
            ),
            PageLoop::Capped
        );
        let empty = parse_jira_page(&serde_json::json!({"isLast": false}));
        assert_eq!(
            page_loop_done(
                0,
                20,
                empty.next_page_token.as_deref(),
                empty.is_last,
                !empty.issues.is_empty()
            ),
            PageLoop::Finished
        );
    }

    #[test]
    fn jira_since_bound_is_relative_with_overlap() {
        // 20 minutes ago -> ceil(20) + 10.
        assert_eq!(
            jira_since_bound("2026-10-19T14:10:00Z", "2026-10-19T14:30:00Z").unwrap(),
            "-30m"
        );
        // 31 days ago stays in minutes: 31 * 1440 + 10 overlap.
        assert_eq!(
            jira_since_bound("2026-01-01T00:00:00Z", "2026-02-01T00:00:00Z").unwrap(),
            "-44650m"
        );
    }

    #[test]
    fn parse_rfc3339_utc_applies_offset_before_fraction() {
        // Fraction sits in front of the offset. Stripping it first would drop +02:00.
        assert_eq!(
            parse_rfc3339_utc("2026-10-19T14:20:31.123+02:00").unwrap(),
            parse_rfc3339_utc("2026-10-19T12:20:31Z").unwrap()
        );
        // Jira writes the offset without a colon.
        assert_eq!(
            parse_rfc3339_utc("2026-10-19T14:20:31.123-0400").unwrap(),
            parse_rfc3339_utc("2026-10-19T18:20:31Z").unwrap()
        );
    }

    #[test]
    fn split_jira_offset_rejects_non_ascii_four_byte_suffix() {
        // `a` + U+20AC is 4 bytes. Slicing at byte 2 lands inside the euro.
        assert_eq!(split_jira_offset("14:20:31+a\u{20ac}"), Err(()));
        assert!(parse_rfc3339_utc("2026-10-19T14:20:31+a\u{20ac}").is_err());
    }

    #[test]
    fn parse_rfc3339_utc_rejects_year_outside_1970_9999() {
        // Parses as i64, then days_from_civil's era multiply would overflow.
        assert!(parse_rfc3339_utc("999999999999999999-01-01T00:00:00Z").is_err());
        assert!(parse_rfc3339_utc("1969-12-31T23:59:59Z").is_err());
        assert!(parse_rfc3339_utc("10000-01-01T00:00:00Z").is_err());
        assert_eq!(parse_rfc3339_utc("1970-01-01T00:00:00Z").unwrap(), 0);
    }

    #[test]
    fn jira_watermark_resumes_when_capped_and_is_now_when_complete() {
        // +02:00 is 12:20Z. -0400 is 18:20Z. String order would pick the wrong one.
        let earlier = "2026-10-19T14:20:31.123+02:00";
        let written = "2026-10-19T14:20:31.123-0400";
        let unread = "2026-10-19T20:00:00Z";
        let pages = vec![
            JiraWrittenPage {
                written_updated: vec![earlier.into(), written.into()],
                next_token: Some("p2".into()),
                is_last: false,
                has_issues: true,
            },
            JiraWrittenPage {
                written_updated: vec![unread.into()],
                next_token: Some("p3".into()),
                is_last: false,
                has_issues: true,
            },
        ];
        assert_eq!(
            jira_watermark(&pages, 1).unwrap(),
            JiraWatermark::Resume("2026-10-19T18:20:31Z".into())
        );
        assert_eq!(
            jira_watermark(&pages, 2).unwrap(),
            JiraWatermark::Resume("2026-10-19T20:00:00Z".into())
        );
        let done = vec![
            JiraWrittenPage {
                written_updated: vec![earlier.into(), written.into()],
                next_token: Some("p2".into()),
                is_last: false,
                has_issues: true,
            },
            JiraWrittenPage {
                written_updated: vec![unread.into()],
                next_token: None,
                is_last: true,
                has_issues: true,
            },
        ];
        assert_eq!(jira_watermark(&done, 20).unwrap(), JiraWatermark::Now);
        // isLast false, issues present, no token: same as the page cap. Not "now".
        let incomplete = vec![JiraWrittenPage {
            written_updated: vec![written.into()],
            next_token: None,
            is_last: false,
            has_issues: true,
        }];
        assert_eq!(
            jira_watermark(&incomplete, 20).unwrap(),
            JiraWatermark::Resume("2026-10-19T18:20:31Z".into())
        );
        let empty = vec![JiraWrittenPage {
            written_updated: vec![],
            next_token: None,
            is_last: false,
            has_issues: false,
        }];
        assert_eq!(jira_watermark(&empty, 20).unwrap(), JiraWatermark::Now);
        let nothing_written = vec![JiraWrittenPage {
            written_updated: vec![],
            next_token: Some("p2".into()),
            is_last: false,
            has_issues: true,
        }];
        assert!(jira_watermark(&nothing_written, 1).is_err());
    }

    #[test]
    fn page_loop_caps_without_finishing() {
        assert_eq!(
            page_loop_done(0, 20, Some("tok"), false, true),
            PageLoop::Continue
        );
        assert_eq!(
            page_loop_done(18, 20, Some("tok"), false, true),
            PageLoop::Continue
        );
        assert_eq!(
            page_loop_done(19, 20, Some("tok"), false, true),
            PageLoop::Capped
        );
        assert_eq!(
            page_loop_done(19, 20, None, true, true),
            PageLoop::Finished
        );
        assert_eq!(page_loop_done(0, 20, None, true, true), PageLoop::Finished);
        assert_eq!(
            page_loop_done(19, 20, Some(""), true, true),
            PageLoop::Finished
        );
        // isLast false or omitted: issues and no token is incomplete, not finished.
        assert_eq!(page_loop_done(0, 20, None, false, true), PageLoop::Capped);
        assert_eq!(page_loop_done(5, 20, None, false, true), PageLoop::Capped);
        assert_eq!(
            page_loop_done(0, 20, Some(""), false, true),
            PageLoop::Capped
        );
        // No issues and no token is finished even when isLast is false.
        assert_eq!(
            page_loop_done(0, 20, None, false, false),
            PageLoop::Finished
        );
        assert_eq!(
            page_loop_done(0, 20, Some(""), false, false),
            PageLoop::Finished
        );
        // isLast wins over a token that is also present.
        assert_eq!(
            page_loop_done(0, 20, Some("tok"), true, true),
            PageLoop::Finished
        );
    }

    #[test]
    fn last_jira_success_at_is_scoped_to_boards_resource() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(store::SCHEMA).unwrap();
        let pay = jira_boards_resource(&["PAY".to_string()]);
        let pay_ops = jira_boards_resource(&["PAY".to_string(), "OPS".to_string()]);
        assert_eq!(pay, "boards:PAY");
        assert_eq!(pay_ops, "boards:PAY,OPS");
        assert_eq!(last_jira_success_at(&conn, &pay_ops).unwrap(), None);
        conn.execute(
            "INSERT INTO sync_state (source, resource, cursor, last_success_at, status)
             VALUES ('jira', 'boards:PAY', 'x', '2026-10-19T14:00:00Z', 'ok')",
            [],
        )
        .unwrap();
        // A success for boards:PAY does not open the window for boards:PAY,OPS.
        assert_eq!(last_jira_success_at(&conn, &pay_ops).unwrap(), None);
        assert_eq!(
            last_jira_success_at(&conn, &pay).unwrap().as_deref(),
            Some("2026-10-19T14:00:00Z")
        );
        conn.execute(
            "INSERT INTO sync_state (source, resource, cursor, last_success_at, status)
             VALUES ('jira', 'boards:PAY,OPS', 'x', '2026-10-18T14:00:00Z', 'ok')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO sync_state (source, resource, cursor, last_success_at, status)
             VALUES ('github', 'repo:x', 'x', '2026-10-19T18:00:00Z', 'ok')",
            [],
        )
        .unwrap();
        assert_eq!(
            last_jira_success_at(&conn, &pay_ops).unwrap().as_deref(),
            Some("2026-10-18T14:00:00Z")
        );
    }

    #[test]
    fn write_jira_issue_skips_missing_or_empty_key() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(store::SCHEMA).unwrap();
        let epic_fields = serde_json::json!({"issuetype": {"name": "Epic"}, "summary": "Pay"});
        assert_eq!(
            write_jira_issue(&conn, &serde_json::json!({"fields": epic_fields})).unwrap(),
            0
        );
        assert_eq!(
            write_jira_issue(
                &conn,
                &serde_json::json!({"key": "", "fields": {"issuetype": {"name": "Epic"}, "summary": "Pay"}})
            )
            .unwrap(),
            0
        );
        let epics: i64 = conn
            .query_row("SELECT COUNT(*) FROM epics", [], |row| row.get(0))
            .unwrap();
        assert_eq!(epics, 0);
        assert_eq!(
            write_jira_issue(
                &conn,
                &serde_json::json!({"key": "PAY-1", "fields": {"issuetype": {"name": "Epic"}, "summary": "Pay"}})
            )
            .unwrap(),
            1
        );
        let story = serde_json::json!({
            "key": "",
            "fields": {
                "issuetype": {"name": "Story"},
                "summary": "No key",
                "parent": {"key": "PAY-1"}
            }
        });
        assert_eq!(write_jira_issue(&conn, &story).unwrap(), 0);
        let missing_story = serde_json::json!({
            "fields": {
                "issuetype": {"name": "Story"},
                "summary": "No key",
                "parent": {"key": "PAY-1"}
            }
        });
        assert_eq!(write_jira_issue(&conn, &missing_story).unwrap(), 0);
        let stories: i64 = conn
            .query_row("SELECT COUNT(*) FROM stories", [], |row| row.get(0))
            .unwrap();
        assert_eq!(stories, 0);
        let epics: i64 = conn
            .query_row("SELECT COUNT(*) FROM epics", [], |row| row.get(0))
            .unwrap();
        assert_eq!(epics, 1);
    }
}
