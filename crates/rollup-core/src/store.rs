//! SQLite store implementing the PRD-1 schema (dev build of the Postgres shape),
//! plus leadership updates, partner briefs, and AI drafting inputs.

use rusqlite::Connection;
use std::path::Path;

pub const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS repos (
  id INTEGER PRIMARY KEY,
  org TEXT NOT NULL,
  name TEXT NOT NULL,
  default_branch TEXT DEFAULT 'main',
  archived INTEGER DEFAULT 0,
  UNIQUE(org, name)
);

CREATE TABLE IF NOT EXISTS repo_epic_map (
  id INTEGER PRIMARY KEY,
  repo TEXT NOT NULL UNIQUE,
  epic_key TEXT NOT NULL,
  source TEXT NOT NULL DEFAULT 'config',
  allowed_secondary_epics TEXT NOT NULL DEFAULT '[]',
  active INTEGER NOT NULL DEFAULT 1,
  updated_by TEXT,
  updated_at TEXT
);

CREATE TABLE IF NOT EXISTS identity_map (
  id INTEGER PRIMARY KEY,
  jira_account_id TEXT UNIQUE,
  github_login TEXT UNIQUE,
  slack_member_id TEXT UNIQUE,
  display_name TEXT NOT NULL,
  confirmed INTEGER NOT NULL DEFAULT 0,
  role TEXT
);

CREATE TABLE IF NOT EXISTS person_projects (
  owner TEXT NOT NULL,
  epic_key TEXT NOT NULL,
  created_at TEXT NOT NULL,
  PRIMARY KEY (owner, epic_key)
);

CREATE TABLE IF NOT EXISTS epics (
  epic_key TEXT PRIMARY KEY,
  board TEXT NOT NULL,
  summary TEXT NOT NULL,
  status TEXT,
  shape TEXT,
  shape_confirmed_by TEXT
);

CREATE TABLE IF NOT EXISTS stories (
  story_key TEXT PRIMARY KEY,
  epic_key TEXT NOT NULL REFERENCES epics(epic_key),
  summary TEXT NOT NULL,
  description TEXT DEFAULT '',
  status TEXT,
  assignee TEXT,
  sprint TEXT,
  updated_at TEXT,
  raw TEXT NOT NULL DEFAULT '{}'
);
CREATE INDEX IF NOT EXISTS idx_stories_epic ON stories(epic_key);

CREATE TABLE IF NOT EXISTS pull_requests (
  id TEXT PRIMARY KEY,
  repo TEXT NOT NULL,
  number INTEGER NOT NULL,
  title TEXT NOT NULL,
  body TEXT DEFAULT '',
  author TEXT,
  state TEXT NOT NULL,
  created_at TEXT,
  merged_at TEXT,
  closed_at TEXT,
  head_branch TEXT,
  base_branch TEXT,
  raw TEXT NOT NULL DEFAULT '{}'
);
CREATE INDEX IF NOT EXISTS idx_pr_repo ON pull_requests(repo);

CREATE TABLE IF NOT EXISTS pr_story_links (
  id INTEGER PRIMARY KEY,
  pr_id TEXT NOT NULL REFERENCES pull_requests(id),
  story_key TEXT NOT NULL REFERENCES stories(story_key),
  score REAL NOT NULL,
  method TEXT NOT NULL,
  status TEXT NOT NULL,
  runner_ups TEXT NOT NULL DEFAULT '[]',
  model_version TEXT,
  created_at TEXT,
  decided_by TEXT,
  active INTEGER NOT NULL DEFAULT 1,
  UNIQUE(pr_id, story_key)
);
CREATE INDEX IF NOT EXISTS idx_links_story ON pr_story_links(story_key);

CREATE TABLE IF NOT EXISTS activity_events (
  id INTEGER PRIMARY KEY,
  source TEXT NOT NULL,
  type TEXT NOT NULL,
  ref TEXT NOT NULL,
  actor TEXT,
  occurred_at TEXT NOT NULL,
  payload TEXT NOT NULL DEFAULT '{}'
);
CREATE INDEX IF NOT EXISTS idx_events_ref ON activity_events(ref, occurred_at);

CREATE TABLE IF NOT EXISTS sync_state (
  source TEXT NOT NULL,
  resource TEXT NOT NULL,
  cursor TEXT,
  last_success_at TEXT,
  status TEXT NOT NULL DEFAULT 'ok',
  PRIMARY KEY (source, resource)
);

CREATE TABLE IF NOT EXISTS review_queue (
  id INTEGER PRIMARY KEY,
  pr_id TEXT NOT NULL UNIQUE REFERENCES pull_requests(id),
  status TEXT NOT NULL DEFAULT 'unmatched',
  suggested_story TEXT,
  suggested_score REAL,
  notes TEXT DEFAULT '',
  updated_at TEXT
);

CREATE TABLE IF NOT EXISTS attachments (
  id INTEGER PRIMARY KEY,
  kind TEXT NOT NULL,
  key TEXT NOT NULL,
  owner TEXT NOT NULL,
  created_at TEXT NOT NULL,
  UNIQUE(kind, key, owner)
);

CREATE TABLE IF NOT EXISTS lu_categories (
  id INTEGER PRIMARY KEY,
  name TEXT NOT NULL,
  position INTEGER NOT NULL
);
CREATE UNIQUE INDEX IF NOT EXISTS lu_categories_name ON lu_categories(name);

CREATE TABLE IF NOT EXISTS lu_entries (
  id INTEGER PRIMARY KEY,
  entry_date TEXT NOT NULL,
  created_at TEXT NOT NULL,
  sent_at TEXT,
  programme TEXT,
  published_at TEXT
);
-- One open draft: every unsent row indexes the same key.
CREATE UNIQUE INDEX IF NOT EXISTS lu_one_draft ON lu_entries((sent_at IS NULL)) WHERE sent_at IS NULL;

CREATE TABLE IF NOT EXISTS lu_items (
  id INTEGER PRIMARY KEY,
  entry_id INTEGER NOT NULL REFERENCES lu_entries(id),
  item_id TEXT NOT NULL,
  category TEXT NOT NULL,
  position INTEGER NOT NULL,
  title TEXT NOT NULL,
  priority INTEGER NOT NULL DEFAULT 0,
  start_date TEXT,
  end_date TEXT,
  pct INTEGER,
  capacity INTEGER,
  carried TEXT NOT NULL DEFAULT '[]',
  completed TEXT NOT NULL DEFAULT '[]',
  next TEXT NOT NULL DEFAULT '[]',
  future TEXT NOT NULL DEFAULT '[]',
  UNIQUE(entry_id, item_id)
);

-- One applied text correction. entry_ids is a JSON array of lu_entries ids.
CREATE TABLE IF NOT EXISTS lu_corrections (
  id INTEGER PRIMARY KEY,
  at TEXT NOT NULL,
  by TEXT NOT NULL,
  find TEXT NOT NULL,
  replace TEXT NOT NULL,
  rows INTEGER NOT NULL,
  entry_ids TEXT NOT NULL DEFAULT '[]'
);

CREATE TABLE IF NOT EXISTS a1_entries (
  id INTEGER PRIMARY KEY,
  week_ending TEXT NOT NULL,
  created_at TEXT NOT NULL,
  sent_at TEXT,
  org_unit TEXT,
  partner_name TEXT,
  prepared_by TEXT,
  bottom_line TEXT
);
-- One open draft: every unsent row indexes the same key.
CREATE UNIQUE INDEX IF NOT EXISTS a1_one_draft ON a1_entries((sent_at IS NULL)) WHERE sent_at IS NULL;

CREATE TABLE IF NOT EXISTS a1_items (
  id INTEGER PRIMARY KEY,
  entry_id INTEGER NOT NULL REFERENCES a1_entries(id),
  item_id TEXT NOT NULL,
  kind TEXT NOT NULL CHECK(kind IN ('needs','fyi')),
  position INTEGER NOT NULL,
  headline TEXT NOT NULL,
  what TEXT NOT NULL DEFAULT '',
  why TEXT NOT NULL DEFAULT '',
  action TEXT NOT NULL DEFAULT '',
  at_this_stage INTEGER NOT NULL DEFAULT 0,
  UNIQUE(entry_id, item_id)
);

CREATE TABLE IF NOT EXISTS lu_item_sources (
  item_id TEXT NOT NULL,
  source_kind TEXT NOT NULL CHECK(source_kind IN ('epic','story','repo')),
  source_key TEXT NOT NULL,
  created_at TEXT NOT NULL,
  PRIMARY KEY(item_id, source_kind, source_key)
);

CREATE TABLE IF NOT EXISTS ai_notes (
  id INTEGER PRIMARY KEY,
  kind TEXT NOT NULL CHECK(kind IN ('fact','glossary','rule','escalate')),
  body TEXT NOT NULL,
  created_at TEXT NOT NULL,
  resolved_at TEXT
);
-- One open note per kind and body. An older database may hold repeats: settle them
-- (the oldest stays open) so the index can be built. Resolve a note to add it again.
UPDATE ai_notes SET resolved_at = created_at
  WHERE resolved_at IS NULL
    AND id NOT IN (SELECT MIN(id) FROM ai_notes WHERE resolved_at IS NULL GROUP BY kind, body);
CREATE UNIQUE INDEX IF NOT EXISTS ai_notes_open
  ON ai_notes(kind, body) WHERE resolved_at IS NULL;

CREATE TABLE IF NOT EXISTS ai_flags (
  id INTEGER PRIMARY KEY,
  item_id TEXT NOT NULL,
  field TEXT NOT NULL,
  reason TEXT NOT NULL,
  raised_at TEXT NOT NULL,
  resolved_at TEXT
);
-- One open flag per item, field, and reason. Resolve it to raise the same flag again.
CREATE UNIQUE INDEX IF NOT EXISTS ai_flags_open
  ON ai_flags(item_id, field, reason) WHERE resolved_at IS NULL;

CREATE TABLE IF NOT EXISTS ai_snapshot (
  kind TEXT NOT NULL CHECK(kind IN ('story','pr')),
  key TEXT NOT NULL,
  state TEXT NOT NULL,
  PRIMARY KEY(kind, key)
);

-- One row per ai run: ids and counts only, never prompt text.
-- outcome is unchanged, drafted, no_draft or failed. No CHECK: SQLite cannot alter one later.
-- last_event is the newest detector activity_events id a drafted or unchanged run covered.
CREATE TABLE IF NOT EXISTS ai_runs (
  id INTEGER PRIMARY KEY,
  started_at TEXT NOT NULL,
  finished_at TEXT,
  mode TEXT NOT NULL,
  outcome TEXT NOT NULL,
  facts INTEGER NOT NULL DEFAULT 0,
  accepted INTEGER NOT NULL DEFAULT 0,
  rejected INTEGER NOT NULL DEFAULT 0,
  model TEXT,
  detail TEXT,
  last_event INTEGER
);
"#;

/// Shared connection type. rusqlite Connection is the store handle for P0;
/// the API layer serialises access behind a Mutex.
pub type Conn = Connection;

/// Format a unix-epoch-seconds timestamp as RFC 3339 (UTC).
pub fn format_epoch(secs: u64) -> String {
    // RFC 3339 from epoch seconds; no chrono dependency needed for second precision.
    let days = secs / 86_400;
    let rem = secs % 86_400;
    let (h, m, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    // Civil-from-days algorithm (Howard Hinnant) for Y-M-D.
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { y + 1 } else { y };
    format!("{year:04}-{month:02}-{d:02}T{h:02}:{m:02}:{s:02}Z")
}

pub fn utcnow() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock before epoch");
    format_epoch(now.as_secs())
}

/// UTC timestamp `days` ago. Fixture data stays perpetually fresh.
pub fn utc_days_ago(days: i64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock before epoch");
    let secs = now.as_secs() as i64 - days * 86_400;
    format_epoch(secs.max(0) as u64)
}

/// The DB holds leadership updates and briefs: keep it, and its -wal and -shm,
/// readable by this user only. SQLite gives -wal and -shm the mode of the main
/// file, so creating that file 0600 first covers all three. Files left looser by
/// an older build are tightened. Symlinks are left alone. Best effort: SQLite
/// reports any real open error.
#[cfg(unix)]
fn private_db_files(db_path: &Path) {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let _ = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(db_path);
    for suffix in ["", "-wal", "-shm"] {
        let mut name = db_path.as_os_str().to_os_string();
        name.push(suffix);
        let path = std::path::PathBuf::from(name);
        // O_NOFOLLOW binds the check and the chmod to one inode. NotFound and ELOOP
        // (a symlink) are skipped; any other error is ignored.
        let file = match std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(&path)
        {
            Ok(file) => file,
            Err(err)
                if err.kind() == std::io::ErrorKind::NotFound
                    || err.raw_os_error() == Some(libc::ELOOP) =>
            {
                continue;
            }
            Err(_) => continue,
        };
        if let Ok(meta) = file.metadata() {
            if meta.is_file() && meta.permissions().mode() & 0o077 != 0 {
                let _ = file.set_permissions(std::fs::Permissions::from_mode(0o600));
            }
        }
    }
}

#[cfg(not(unix))]
fn private_db_files(_db_path: &Path) {}

pub fn connect(db_path: &Path) -> rusqlite::Result<Connection> {
    if let Some(parent) = db_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    private_db_files(db_path);
    let conn = Connection::open(db_path)?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    Ok(conn)
}

pub fn init_db(db_path: &Path) -> rusqlite::Result<()> {
    let conn = connect(db_path)?;
    conn.execute_batch(SCHEMA)?;
    ensure_role_column(&conn)?;
    ensure_last_event_column(&conn)?;
    ensure_published_at_column(&conn)
}

fn ensure_role_column(conn: &Connection) -> rusqlite::Result<()> {
    let mut stmt = conn.prepare("PRAGMA table_info(identity_map)")?;
    let cols = stmt
        .query_map([], |r| r.get::<_, String>(1))?
        .collect::<rusqlite::Result<Vec<String>>>()?;
    let has_role = cols.iter().any(|c| c == "role");
    if !has_role {
        conn.execute("ALTER TABLE identity_map ADD COLUMN role TEXT", [])?;
    }
    Ok(())
}

pub(crate) fn ensure_last_event_column(conn: &Connection) -> rusqlite::Result<()> {
    let mut stmt = conn.prepare("PRAGMA table_info(ai_runs)")?;
    let cols = stmt
        .query_map([], |r| r.get::<_, String>(1))?
        .collect::<rusqlite::Result<Vec<String>>>()?;
    if !cols.iter().any(|c| c == "last_event") {
        conn.execute("ALTER TABLE ai_runs ADD COLUMN last_event INTEGER", [])?;
        // before this column a pass never re-offered old facts, so history keeps that behaviour and only new events are offered.
        conn.execute(
            "UPDATE ai_runs SET last_event = (SELECT MAX(id) FROM activity_events WHERE source='detector') WHERE outcome IN ('drafted','unchanged')",
            [],
        )?;
    }
    Ok(())
}

pub(crate) fn ensure_published_at_column(conn: &Connection) -> rusqlite::Result<()> {
    let mut stmt = conn.prepare("PRAGMA table_info(lu_entries)")?;
    let cols = stmt
        .query_map([], |r| r.get::<_, String>(1))?
        .collect::<rusqlite::Result<Vec<String>>>()?;
    if !cols.iter().any(|c| c == "published_at") {
        conn.execute("ALTER TABLE lu_entries ADD COLUMN published_at TEXT", [])?;
    }
    Ok(())
}

pub fn record_sync(
    conn: &Connection,
    source: &str,
    resource: &str,
    cursor: &str,
) -> rusqlite::Result<()> {
    record_sync_at(conn, source, resource, cursor, &utcnow())
}

/// Same row as [`record_sync`], but `last_success_at` is `at` instead of the clock.
/// A capped Jira run resumes from the newest issue it wrote.
pub fn record_sync_at(
    conn: &Connection,
    source: &str,
    resource: &str,
    cursor: &str,
    at: &str,
) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO sync_state (source, resource, cursor, last_success_at, status)
         VALUES (?1, ?2, ?3, ?4, 'ok')
         ON CONFLICT(source, resource) DO UPDATE SET
           cursor=excluded.cursor, last_success_at=excluded.last_success_at,
           status=excluded.status",
        rusqlite::params![source, resource, cursor, at],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utcnow_format() {
        let ts = utcnow();
        assert_eq!(ts.len(), 20);
        assert!(ts.ends_with('Z'));
        assert_eq!(&ts[4..5], "-");
    }

    #[cfg(unix)]
    #[test]
    fn db_wal_and_shm_are_0600() {
        use std::os::unix::fs::PermissionsExt;
        let mode = |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        let dir = std::env::temp_dir().join(format!("rollup_perms_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("t.db");
        let side = |s: &str| dir.join(format!("t.db{s}"));

        // Fresh DB: WAL is forced by connect; a write keeps -wal and -shm open.
        init_db(&db).unwrap();
        let conn = connect(&db).unwrap();
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS perm_probe(x); INSERT INTO perm_probe VALUES (1);",
        )
        .unwrap();
        for p in [db.clone(), side("-wal"), side("-shm")] {
            assert!(p.exists(), "{} missing", p.display());
            assert_eq!(mode(&p), 0o600, "{}", p.display());
        }

        // A DB left 0644 by an older build is tightened on the next open.
        for p in [db.clone(), side("-wal"), side("-shm")] {
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o644)).unwrap();
        }
        let again = connect(&db).unwrap();
        again
            .execute("INSERT INTO perm_probe VALUES (2)", [])
            .unwrap();
        for p in [db.clone(), side("-wal"), side("-shm")] {
            assert_eq!(mode(&p), 0o600, "{} not tightened", p.display());
        }
        drop((conn, again));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn wal_symlink_does_not_chmod_victim() {
        use std::os::unix::fs::PermissionsExt;
        let mode = |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        let dir = std::env::temp_dir().join(format!("rollup_nofollow_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("t.db");
        let victim = dir.join("victim");
        std::fs::File::create(&victim).unwrap();
        std::fs::set_permissions(&victim, std::fs::Permissions::from_mode(0o644)).unwrap();
        std::os::unix::fs::symlink(&victim, dir.join("t.db-wal")).unwrap();
        connect(&db).unwrap();
        assert_eq!(mode(&victim), 0o644);
        assert_eq!(mode(&db), 0o600);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn schema_applies_twice() {
        let dir = std::env::temp_dir().join(format!("rollup_test_{}", std::process::id()));
        let db = dir.join("t.db");
        init_db(&db).unwrap();
        init_db(&db).unwrap(); // idempotent
        let _ = std::fs::remove_file(&db);
    }

    #[test]
    fn format_epoch_edges() {
        // Epoch 0
        assert_eq!(format_epoch(0), "1970-01-01T00:00:00Z");
        // Leap day 2024-02-29 (19782 days * 86400)
        assert_eq!(format_epoch(1_709_164_800), "2024-02-29T00:00:00Z");
        // Year boundary 2025-12-31T23:59:59Z then +1s
        assert_eq!(format_epoch(1_767_225_599), "2025-12-31T23:59:59Z");
        assert_eq!(format_epoch(1_767_225_600), "2026-01-01T00:00:00Z");
    }

    #[test]
    fn record_sync_upsert() {
        let dir = std::env::temp_dir().join(format!("rollup_test_sync_{}", std::process::id()));
        let db = dir.join("t.db");
        let _ = std::fs::remove_file(&db);
        init_db(&db).unwrap();
        let conn = connect(&db).unwrap();
        record_sync(&conn, "github", "repos:payments-core", "cur-1").unwrap();
        record_sync(&conn, "github", "repos:payments-core", "cur-2").unwrap();
        let (cursor, status): (String, String) = conn
            .query_row(
                "SELECT cursor, status FROM sync_state WHERE source='github' AND resource='repos:payments-core'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(cursor, "cur-2");
        assert_eq!(status, "ok");
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM sync_state", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1);
        record_sync_at(&conn, "jira", "boards:PAY", "-30m", "2026-10-19T18:20:31Z").unwrap();
        let at: String = conn
            .query_row(
                "SELECT last_success_at FROM sync_state WHERE source='jira' AND resource='boards:PAY'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(at, "2026-10-19T18:20:31Z");
        let _ = std::fs::remove_file(&db);
    }

    #[test]
    fn published_at_column_is_added_only_when_missing() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE lu_entries (
               id INTEGER PRIMARY KEY,
               entry_date TEXT NOT NULL,
               created_at TEXT NOT NULL,
               sent_at TEXT,
               programme TEXT
             )",
        )
        .unwrap();
        ensure_published_at_column(&conn).unwrap();
        ensure_published_at_column(&conn).unwrap();
        let mut stmt = conn.prepare("PRAGMA table_info(lu_entries)").unwrap();
        let cols: Vec<String> = stmt
            .query_map([], |r| r.get::<_, String>(1))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(cols.iter().filter(|c| *c == "published_at").count(), 1);
    }
}
