//! Who attached a repo or a board, and which repo points at which project.
//!
//! Fixture sync must not clobber a map a person saved. That is
//! `upsert_fixture_map`.

use rusqlite::{params, Connection};
use serde_json::{json, Value};

use crate::store;

/// Why a write was refused. `Forbidden` is the role or owner rule (403): a
/// missing, blank or unknown person, or a kind the role may not attach.
/// `Invalid` is bad input (400). `Internal` is a storage fault (500); its
/// text is logged, not sent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectError {
    Forbidden(String),
    Invalid(String),
    Internal(String),
}

impl ConnectError {
    /// Bad input (400).
    pub fn invalid(m: impl Into<String>) -> Self {
        ConnectError::Invalid(m.into())
    }

    /// The role or owner rule (403).
    pub fn forbidden(m: impl Into<String>) -> Self {
        ConnectError::Forbidden(m.into())
    }
}

impl std::fmt::Display for ConnectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConnectError::Forbidden(m) | ConnectError::Invalid(m) | ConnectError::Internal(m) => {
                f.write_str(m)
            }
        }
    }
}

impl std::error::Error for ConnectError {}

impl From<rusqlite::Error> for ConnectError {
    fn from(e: rusqlite::Error) -> Self {
        ConnectError::Internal(e.to_string())
    }
}

/// Seed or refresh a fixture map. A row with `updated_by` set is a person's
/// decision and is left alone.
pub fn upsert_fixture_map(
    conn: &Connection,
    repo: &str,
    epic_key: &str,
    updated_at: &str,
) -> rusqlite::Result<usize> {
    conn.execute(
        "INSERT INTO repo_epic_map (repo, epic_key, source, updated_at)
         VALUES (?1, ?2, 'config', ?3)
         ON CONFLICT(repo) DO UPDATE SET
           epic_key = excluded.epic_key,
           source = 'config',
           updated_at = excluded.updated_at
         WHERE repo_epic_map.updated_by IS NULL",
        params![repo, epic_key, updated_at],
    )
}

pub fn workspace(conn: &Connection) -> rusqlite::Result<Value> {
    let people = query_people(conn)?;
    let repos = query_repos(conn)?;
    let maps = query_maps(conn)?;
    let attachments = query_attachments(conn)?;
    let prs = query_prs(conn)?;
    let stories = query_stories(conn)?;
    let epics = query_epics(conn)?;
    Ok(json!({
        "people": people,
        "repos": repos,
        "maps": maps,
        "attachments": attachments,
        "prs": prs,
        "stories": stories,
        "epics": epics,
        "memberships": query_memberships(conn)?,
    }))
}

/// `kind` is `github_repo` or `jira_board`. `owner` is a stored Jira account
/// or GitHub login. A developer or tech lead claims a repo. A PM or principal
/// PM claims a board.
///
/// Slice A rule (PM decision 2026-09-28): a developer attaches repos only, a
/// PM boards only, a principal PM may attach boards, a tech lead may attach
/// repos. An unknown, blank or roleless owner is refused (403).
/// ponytail: whether a principal PM or tech lead should attach anything is an
/// open product question for the Behavior PR. Change `attach_refusal` then.
/// ponytail: a missing role is not guessed from a GitHub login; that person
/// cannot attach until a role is stored. Repo name is unique; a second org
/// with the same name is ignored. Upgrade path: key attachments by org/name.
pub fn attach(conn: &Connection, kind: &str, raw_key: &str, raw_owner: &str) -> Result<Value, ConnectError> {
    let tx = conn.unchecked_transaction()?;
    let out = attach_in(&tx, kind, raw_key, raw_owner)?;
    tx.commit()?;
    Ok(out)
}

fn attach_in(conn: &Connection, kind: &str, raw_key: &str, raw_owner: &str) -> Result<Value, ConnectError> {
    let (account, login, name) = resolve_owner(conn, raw_owner)?;
    let stored = stored_role(conn, &account)?;
    let role = role_of(stored.as_deref());
    if let Some(m) = attach_refusal(kind, role) {
        return Err(ConnectError::forbidden(m));
    }
    match kind {
        "github_repo" => {
            if login.is_empty() {
                return Err(ConnectError::invalid(
                    "A developer attaches a repo. This person has no GitHub login.",
                ));
            }
            let (org, repo) = split_repo(raw_key).map_err(ConnectError::invalid)?;
            conn.execute(
                "INSERT INTO repos (org, name)
                 SELECT ?1, ?2 WHERE NOT EXISTS (SELECT 1 FROM repos WHERE name = ?2)",
                params![org, repo],
            )?;
            let n = conn
                .execute(
                    "INSERT INTO attachments (kind, key, owner, created_at)
                     VALUES ('github_repo', ?1, ?2, ?3)
                     ON CONFLICT(kind, key, owner) DO NOTHING",
                    params![repo, account, store::utcnow()],
                )?;
            Ok(json!({
                "ok": true,
                "already": n == 0,
                "kind": "github_repo",
                "key": repo,
                "owner": account,
                "name": name,
            }))
        }
        "jira_board" => {
            let board = normalize_board(raw_key).map_err(ConnectError::invalid)?;
            let n = conn
                .execute(
                    "INSERT INTO attachments (kind, key, owner, created_at)
                     VALUES ('jira_board', ?1, ?2, ?3)
                     ON CONFLICT(kind, key, owner) DO NOTHING",
                    params![board, account, store::utcnow()],
                )?;
            Ok(json!({
                "ok": true,
                "already": n == 0,
                "kind": "jira_board",
                "key": board,
                "owner": account,
                "name": name,
            }))
        }
        _ => Err(ConnectError::invalid("kind must be github_repo or jira_board")),
    }
}

/// Attach several boards in one submit. `PAY, AUD` or `PAY AUD`.
/// One failure rolls the whole submit back.
pub fn attach_boards(conn: &Connection, raw_keys: &str, owner: &str) -> Result<Vec<Value>, ConnectError> {
    let keys = split_board_keys(raw_keys);
    if keys.is_empty() {
        return Err(ConnectError::invalid("Name at least one board."));
    }
    let tx = conn.unchecked_transaction()?;
    let mut out = Vec::new();
    for key in &keys {
        out.push(attach_in(&tx, "jira_board", key, owner)?);
    }
    tx.commit()?;
    Ok(out)
}

/// Attach several repos in one submit. `payments-core, audit-trail`.
/// One failure rolls the whole submit back.
pub fn attach_repos(conn: &Connection, raw_keys: &str, owner: &str) -> Result<Vec<Value>, ConnectError> {
    let keys = split_board_keys(raw_keys);
    if keys.is_empty() {
        return Err(ConnectError::invalid("Name at least one repo."));
    }
    let tx = conn.unchecked_transaction()?;
    let mut out = Vec::new();
    for key in &keys {
        out.push(attach_in(&tx, "github_repo", key, owner)?);
    }
    tx.commit()?;
    Ok(out)
}

/// Replace the projects this person is on. Empty clears the pick.
/// A later fixture sync does not touch this table. The owner must be a stored
/// person with a stored role; a missing, blank, unknown or roleless owner is
/// refused (403) before any write.
///
/// ponytail: there is no session identity. The owner / updated_by comes from
/// the request, and the server binds 127.0.0.1 only. Upgrade path: server-side
/// identity and role gating (P2), then check the actor before this write.
pub fn set_projects(conn: &Connection, raw_owner: &str, keys: &[String]) -> Result<Vec<String>, ConnectError> {
    let account = resolve_actor(conn, raw_owner)?;
    let mut clean = Vec::new();
    for key in keys {
        let key = key.trim();
        if key.is_empty() {
            continue;
        }
        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM epics WHERE epic_key = ?1",
                [key],
                |r| r.get(0),
            )?;
        if n == 0 {
            return Err(ConnectError::invalid(format!(
                "That project is not stored: {key}"
            )));
        }
        if !clean.iter().any(|k| k == key) {
            clean.push(key.to_string());
        }
    }
    let tx = conn.unchecked_transaction()?;
    tx.execute("DELETE FROM person_projects WHERE owner = ?1", [&account])?;
    let now = store::utcnow();
    for key in &clean {
        tx.execute(
            "INSERT INTO person_projects (owner, epic_key, created_at) VALUES (?1, ?2, ?3)",
            params![account, key, now],
        )?;
    }
    tx.commit()?;
    Ok(clean)
}

/// The six stored roles. Anything else is not a role. Seeding and
/// `role_of` both use this, so the vocabulary lives in one place.
pub fn known_role(raw: Option<&str>) -> Option<&'static str> {
    match raw.map(str::trim) {
        Some("dev") => Some("dev"),
        Some("tl") => Some("tl"),
        Some("pm") => Some("pm"),
        Some("ppm") => Some("ppm"),
        Some("director") => Some("director"),
        Some("partner") => Some("partner"),
        _ => None,
    }
}

/// Stored role for `raw_owner`, using the same account lookup as [`attach`].
/// `None` when the person is missing, blank, or has no known role.
pub fn owner_role(conn: &Connection, raw_owner: &str) -> Option<&'static str> {
    let (account, _, _) = resolve_owner(conn, raw_owner).ok()?;
    known_role(stored_role(conn, &account).ok()?.as_deref())
}

/// A stored role is the only role. A missing one is `none`, never guessed
/// from a GitHub login.
fn role_of(stored: Option<&str>) -> &'static str {
    known_role(stored).unwrap_or("none")
}

/// The attach rule by role. `None` means allowed. The message says why not.
/// tl -> github_repo and ppm -> jira_board are allowed by the slice A rule
/// (see `attach`).
fn attach_refusal(kind: &str, role: &str) -> Option<&'static str> {
    match (kind, role) {
        ("github_repo", "dev" | "tl") | ("jira_board", "pm" | "ppm") => None,
        ("github_repo", "pm") => Some("A PM attaches boards, not repos. Nothing was attached."),
        ("github_repo", "ppm") => {
            Some("A principal PM does not attach repos. Nothing was attached.")
        }
        ("jira_board", "dev") => {
            Some("A developer attaches repos, not boards. Nothing was attached.")
        }
        ("jira_board", "tl") => Some("A tech lead does not attach boards. Nothing was attached."),
        ("github_repo" | "jira_board", "none") => Some("No role stored. Nothing was attached."),
        ("github_repo" | "jira_board", "director") => {
            Some("A director reads leadership updates and does not attach. Nothing was attached.")
        }
        ("github_repo" | "jira_board", "partner") => Some(
            "A senior partner reads leadership updates and does not attach. Nothing was attached.",
        ),
        ("github_repo" | "jira_board", _) => {
            Some("This role may not attach this. Nothing was attached.")
        }
        // Unknown kind: attach_in refuses it as bad input (400). Add any new kind to both matches.
        _ => None,
    }
}

fn stored_role(conn: &Connection, account: &str) -> rusqlite::Result<Option<String>> {
    conn.query_row(
        "SELECT role FROM identity_map WHERE jira_account_id = ?1",
        [account],
        |r| r.get(0),
    )
}

fn split_board_keys(raw: &str) -> Vec<String> {
    let mut out = Vec::new();
    for part in raw.split(|c: char| c == ',' || c.is_whitespace()) {
        let part = part.trim();
        if part.is_empty() || out.iter().any(|k| k == part) {
            continue;
        }
        out.push(part.to_string());
    }
    out
}

/// Point a stored repo at one project. `updated_by` is `lead` or a stored
/// person with a stored role. A blank, unknown or roleless actor is refused
/// (403). A later fixture sync will not overwrite this row.
///
/// ponytail: there is no session identity. The owner / updated_by comes from
/// the request, and the server binds 127.0.0.1 only. Upgrade path: server-side
/// identity and role gating (P2), then check the actor before this write.
pub fn map_repo(
    conn: &Connection,
    repo: &str,
    epic_key: &str,
    updated_by: &str,
) -> Result<Value, ConnectError> {
    // The actor is checked first: only the literal "lead" means the lead. A
    // blank, unknown or roleless updated_by is refused inside resolve_actor
    // (403), before any input check or write.
    let by = if updated_by.trim() == "lead" {
        "lead".to_string()
    } else {
        resolve_actor(conn, updated_by)?
    };
    let repo = repo.trim();
    let epic_key = epic_key.trim();
    if repo.is_empty() || epic_key.is_empty() {
        return Err(ConnectError::invalid("Pick a repo and a project."));
    }
    let epic_ok: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM epics WHERE epic_key = ?1",
            [epic_key],
            |r| r.get(0),
        )?;
    if epic_ok == 0 {
        return Err(ConnectError::invalid(
            "That project is not stored. Attach the board and sync first.",
        ));
    }
    let known: i64 = conn
        .query_row(
            "SELECT
               (SELECT COUNT(*) FROM repos WHERE name = ?1)
               + (SELECT COUNT(*) FROM pull_requests WHERE repo = ?1)",
            [repo],
            |r| r.get(0),
        )?;
    if known == 0 {
        return Err(ConnectError::invalid(
            "Attach the repo first. Mapping only points a stored repo at a project.",
        ));
    }
    conn.execute(
        "INSERT INTO repo_epic_map (repo, epic_key, source, updated_by, updated_at, active)
         VALUES (?1, ?2, 'person', ?3, ?4, 1)
         ON CONFLICT(repo) DO UPDATE SET
           epic_key = excluded.epic_key,
           source = 'person',
           updated_by = excluded.updated_by,
           updated_at = excluded.updated_at,
           active = 1",
        params![repo, epic_key, by, store::utcnow()],
    )?;
    Ok(json!({"ok": true, "repo": repo, "epic_key": epic_key, "updated_by": by}))
}

fn resolve_owner(conn: &Connection, raw: &str) -> Result<(String, String, String), ConnectError> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err(ConnectError::forbidden("Pick a person. Nothing was written."));
    }
    match conn.query_row(
        "SELECT jira_account_id, COALESCE(github_login, ''), display_name
         FROM identity_map
         WHERE (jira_account_id = ?1 OR github_login = ?1) AND jira_account_id IS NOT NULL
         ORDER BY CASE WHEN jira_account_id = ?1 THEN 0 ELSE 1 END LIMIT 1",
        [raw],
        |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?)),
    ) {
        Ok(v) => Ok(v),
        Err(rusqlite::Error::QueryReturnedNoRows) => Err(ConnectError::forbidden(
            "That person is not stored. Nothing was written.",
        )),
        Err(e) => Err(e.into()),
    }
}

/// The person behind a `/map` or `/projects/select` write: stored, and with
/// a stored role. Returns the Jira account id. A roleless person is refused
/// (403) like an unknown one; the role is never guessed.
fn resolve_actor(conn: &Connection, raw: &str) -> Result<String, ConnectError> {
    let (account, _, _) = resolve_owner(conn, raw)?;
    match known_role(stored_role(conn, &account)?.as_deref()) {
        None => Err(ConnectError::forbidden("No role stored. Nothing was written.")),
        Some("director" | "partner") => Err(ConnectError::forbidden("This role reads only. Nothing was written.")),
        Some(_) => Ok(account),
    }
}

fn split_repo(raw: &str) -> Result<(String, String), String> {
    let raw = raw.trim();
    if raw.is_empty() || raw.chars().any(|c| c.is_whitespace()) {
        return Err("Repo name is required, with no spaces.".into());
    }
    let (org, name) = match raw.split_once('/') {
        Some((org, name)) if !org.is_empty() && !name.is_empty() && !name.contains('/') => {
            (org, name)
        }
        Some(_) => return Err("Use one repo name, or org/name.".into()),
        None => ("local", raw),
    };
    if !valid_token(name) || !valid_token(org) {
        return Err("Repo names use letters, numbers, dots, dashes, and underscores.".into());
    }
    Ok((org.to_string(), name.to_string()))
}

fn normalize_board(raw: &str) -> Result<String, String> {
    let board = raw.trim().to_ascii_uppercase();
    let ok = board.len() >= 2
        && board.len() <= 12
        && board.chars().next().is_some_and(|c| c.is_ascii_uppercase())
        && board.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit());
    if !ok {
        return Err("Board key looks like PAY or AUD.".into());
    }
    Ok(board)
}

fn valid_token(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_')
}

fn query_people(conn: &Connection) -> rusqlite::Result<Vec<Value>> {
    // A person is picked by account id. A GitHub-only identity has none, so it is not listed.
    let mut stmt = conn.prepare(
        "SELECT display_name, jira_account_id, github_login, confirmed, role
         FROM identity_map WHERE jira_account_id IS NOT NULL ORDER BY display_name",
    )?;
    let rows = stmt.query_map([], |r| {
        let login: Option<String> = r.get(2)?;
        let stored: Option<String> = r.get(4)?;
        let role = role_of(stored.as_deref());
        Ok(json!({
            "display_name": r.get::<_, String>(0)?,
            "jira_account_id": r.get::<_, String>(1)?,
            "github_login": login,
            "confirmed": r.get::<_, i64>(3)? == 1,
            "role": role,
        }))
    })?;
    rows.collect()
}

fn query_repos(conn: &Connection) -> rusqlite::Result<Vec<Value>> {
    let mut stmt = conn.prepare("SELECT org, name FROM repos ORDER BY name")?;
    let rows = stmt.query_map([], |r| {
        Ok(json!({"org": r.get::<_, String>(0)?, "name": r.get::<_, String>(1)?}))
    })?;
    rows.collect()
}

fn query_maps(conn: &Connection) -> rusqlite::Result<Vec<Value>> {
    let mut stmt = conn.prepare(
        "SELECT repo, epic_key, source, updated_by, updated_at, active
         FROM repo_epic_map ORDER BY repo",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok(json!({
            "repo": r.get::<_, String>(0)?,
            "epic_key": r.get::<_, String>(1)?,
            "source": r.get::<_, String>(2)?,
            "updated_by": r.get::<_, Option<String>>(3)?,
            "updated_at": r.get::<_, Option<String>>(4)?,
            "active": r.get::<_, i64>(5)? == 1,
        }))
    })?;
    rows.collect()
}

fn query_memberships(conn: &Connection) -> rusqlite::Result<Vec<Value>> {
    let mut stmt = conn.prepare(
        "SELECT owner, epic_key FROM person_projects ORDER BY owner, epic_key",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok(json!({
            "owner": r.get::<_, String>(0)?,
            "epic_key": r.get::<_, String>(1)?,
        }))
    })?;
    rows.collect()
}

fn query_attachments(conn: &Connection) -> rusqlite::Result<Vec<Value>> {
    let mut stmt = conn.prepare(
        "SELECT kind, key, owner, created_at FROM attachments ORDER BY created_at, key",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok(json!({
            "kind": r.get::<_, String>(0)?,
            "key": r.get::<_, String>(1)?,
            "owner": r.get::<_, String>(2)?,
            "created_at": r.get::<_, String>(3)?,
        }))
    })?;
    rows.collect()
}

fn query_prs(conn: &Connection) -> rusqlite::Result<Vec<Value>> {
    let mut stmt = conn.prepare(
        "SELECT p.id, p.repo, p.number, p.title, p.author, p.state, p.merged_at,
                (SELECT l.story_key FROM pr_story_links l
                  WHERE l.pr_id = p.id AND l.active = 1 ORDER BY l.id LIMIT 1),
                (SELECT l.status FROM pr_story_links l
                  WHERE l.pr_id = p.id AND l.active = 1 ORDER BY l.id LIMIT 1)
         FROM pull_requests p ORDER BY p.repo, p.number",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok(json!({
            "pr_id": r.get::<_, String>(0)?,
            "repo": r.get::<_, String>(1)?,
            "pr_number": r.get::<_, i64>(2)?,
            "title": r.get::<_, String>(3)?,
            "author": r.get::<_, Option<String>>(4)?,
            "state": r.get::<_, String>(5)?,
            "merged_at": r.get::<_, Option<String>>(6)?,
            "story_key": r.get::<_, Option<String>>(7)?,
            "link_status": r.get::<_, Option<String>>(8)?,
        }))
    })?;
    rows.collect()
}

fn query_stories(conn: &Connection) -> rusqlite::Result<Vec<Value>> {
    let mut stmt = conn.prepare(
        "SELECT story_key, epic_key, summary, status, assignee FROM stories ORDER BY story_key",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok(json!({
            "story_key": r.get::<_, String>(0)?,
            "epic_key": r.get::<_, String>(1)?,
            "summary": r.get::<_, String>(2)?,
            "status": r.get::<_, Option<String>>(3)?,
            "assignee": r.get::<_, Option<String>>(4)?,
        }))
    })?;
    rows.collect()
}

fn query_epics(conn: &Connection) -> rusqlite::Result<Vec<Value>> {
    let mut stmt = conn.prepare(
        "SELECT epic_key, board, summary, status FROM epics ORDER BY epic_key",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok(json!({
            "epic_key": r.get::<_, String>(0)?,
            "board": r.get::<_, String>(1)?,
            "summary": r.get::<_, String>(2)?,
            "status": r.get::<_, Option<String>>(3)?,
        }))
    })?;
    rows.collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store;

    fn db() -> Connection {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static N: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "rollup_connect_{}_{}.db",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_file(&path);
        store::init_db(&path).unwrap();
        store::connect(&path).unwrap()
    }

    #[test]
    fn known_role_is_the_six() {
        for role in ["dev", "tl", "pm", "ppm", "director", "partner"] {
            assert_eq!(known_role(Some(role)), Some(role));
        }
        assert_eq!(known_role(Some(" director ")), Some("director"));
        assert_eq!(known_role(Some(" partner ")), Some("partner"));
        assert_eq!(known_role(Some("chief")), None);
        assert_eq!(known_role(None), None);
    }

    #[test]
    fn attach_rule_is_one_table() {
        for (kind, role, expected) in [
            ("github_repo", "dev", None),
            ("github_repo", "tl", None),
            (
                "github_repo",
                "pm",
                Some("A PM attaches boards, not repos. Nothing was attached."),
            ),
            (
                "github_repo",
                "ppm",
                Some("A principal PM does not attach repos. Nothing was attached."),
            ),
            ("github_repo", "none", Some("No role stored. Nothing was attached.")),
            ("jira_board", "pm", None),
            ("jira_board", "ppm", None),
            (
                "jira_board",
                "dev",
                Some("A developer attaches repos, not boards. Nothing was attached."),
            ),
            (
                "jira_board",
                "tl",
                Some("A tech lead does not attach boards. Nothing was attached."),
            ),
            ("jira_board", "none", Some("No role stored. Nothing was attached.")),
            (
                "github_repo",
                "director",
                Some("A director reads leadership updates and does not attach. Nothing was attached."),
            ),
            (
                "jira_board",
                "director",
                Some("A director reads leadership updates and does not attach. Nothing was attached."),
            ),
            (
                "github_repo",
                "partner",
                Some("A senior partner reads leadership updates and does not attach. Nothing was attached."),
            ),
            (
                "jira_board",
                "partner",
                Some("A senior partner reads leadership updates and does not attach. Nothing was attached."),
            ),
        ] {
            assert_eq!(attach_refusal(kind, role), expected, "{kind} {role}");
        }
        assert_eq!(attach_refusal("slack_channel", "dev"), None);
    }

    #[test]
    fn attach_once_and_person_map_survives_fixture_seed() {
        let conn = db();
        conn.execute(
            "INSERT INTO identity_map (jira_account_id, github_login, display_name, confirmed, role)
             VALUES ('nora@firm.example', 'nora', 'Nora', 1, 'dev')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO identity_map (jira_account_id, github_login, display_name, confirmed, role)
             VALUES ('mina@firm.example', NULL, 'Mina', 1, 'pm')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO epics (epic_key, board, summary) VALUES ('PAY-100', 'PAY', 'Payments')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO epics (epic_key, board, summary) VALUES ('AUD-200', 'AUD', 'Audit')",
            [],
        )
        .unwrap();

        let first = attach(&conn, "github_repo", "example-org/payments-core", "nora").unwrap();
        let second = attach(&conn, "github_repo", "payments-core", "nora").unwrap();
        assert_eq!(first["already"], false);
        assert_eq!(second["already"], true);
        assert!(attach(&conn, "jira_board", "PAY", "nora").is_err());
        assert!(attach(&conn, "jira_board", "PAY", "mina@firm.example").is_ok());

        map_repo(&conn, "payments-core", "PAY-100", "lead").unwrap();
        upsert_fixture_map(&conn, "payments-core", "AUD-200", "2026-01-01T00:00:00Z").unwrap();
        let epic: String = conn
            .query_row(
                "SELECT epic_key FROM repo_epic_map WHERE repo = 'payments-core'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(epic, "PAY-100");

        upsert_fixture_map(&conn, "audit-trail", "AUD-200", "2026-01-01T00:00:00Z").unwrap();
        upsert_fixture_map(&conn, "audit-trail", "PAY-100", "2026-01-02T00:00:00Z").unwrap();
        let seeded: String = conn
            .query_row(
                "SELECT epic_key FROM repo_epic_map WHERE repo = 'audit-trail'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(seeded, "PAY-100");
    }

    #[test]
    fn a_missing_role_is_not_guessed() {
        let path = std::env::temp_dir().join(format!(
            "rollup_connect_role_guess_{}.db",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        store::init_db(&path).unwrap();
        let conn = store::connect(&path).unwrap();
        // Has a GitHub login, no stored role. The old guess made this person a dev.
        conn.execute(
            "INSERT INTO identity_map (jira_account_id, github_login, display_name, confirmed)
             VALUES ('nobody@firm.example', 'nobody', 'Nobody', 1)",
            [],
        )
        .unwrap();
        // Theo: a role is stored, but they are not confirmed. That does not block.
        conn.execute(
            "INSERT INTO identity_map (jira_account_id, github_login, display_name, confirmed, role)
             VALUES ('theo@firm.example', 'theo', 'Theo', 0, 'dev')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO epics (epic_key, board, summary) VALUES ('PAY-100', 'PAY', 'Payments')",
            [],
        )
        .unwrap();

        let ws = workspace(&conn).unwrap();
        let role = |id: &str| {
            ws["people"]
                .as_array()
                .unwrap()
                .iter()
                .find(|p| p["jira_account_id"] == id)
                .unwrap()["role"]
                .as_str()
                .unwrap()
                .to_string()
        };
        assert_eq!(role("nobody@firm.example"), "none");
        assert_eq!(role("theo@firm.example"), "dev");
        assert!(attach(&conn, "github_repo", "payments-core", "nobody@firm.example").is_err());
        assert!(attach(&conn, "jira_board", "PAY", "nobody@firm.example").is_err());
        assert!(attach(&conn, "github_repo", "payments-core", "theo@firm.example").is_ok());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn readers_cannot_write_project_picks() {
        let conn = db();
        conn.execute(
            "INSERT INTO identity_map (jira_account_id, github_login, display_name, confirmed, role)
             VALUES ('dana@firm.example', 'dana-w', 'Dana', 1, 'director')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO epics (epic_key, board, summary) VALUES ('PAY-100', 'PAY', 'Payments')",
            [],
        )
        .unwrap();
        let refused = set_projects(&conn, "dana@firm.example", &["PAY-100".to_string()]).unwrap_err();
        assert_eq!(refused.to_string(), "This role reads only. Nothing was written.");
    }

    #[test]
    fn roles_boards_and_project_picks_survive_fixture_seed() {
        let path = std::env::temp_dir().join(format!(
            "rollup_connect_roles_{}.db",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        store::init_db(&path).unwrap();
        let conn = store::connect(&path).unwrap();
        conn.execute(
            "INSERT INTO identity_map (jira_account_id, github_login, display_name, confirmed, role)
             VALUES ('rosa@firm.example', 'rosa', 'Rosa', 1, 'tl')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO identity_map (jira_account_id, github_login, display_name, confirmed, role)
             VALUES ('iris@firm.example', NULL, 'Iris', 1, 'ppm')",
            [],
        )
        .unwrap();
        for (key, board, summary) in [
            ("PAY-100", "PAY", "Payments"),
            ("AUD-200", "AUD", "Audit"),
            ("PLAT-300", "PLAT", "Platform"),
        ] {
            conn.execute(
                "INSERT INTO epics (epic_key, board, summary) VALUES (?1, ?2, ?3)",
                params![key, board, summary],
            )
            .unwrap();
        }

        conn.execute(
            "INSERT INTO identity_map (jira_account_id, github_login, display_name, confirmed, role)
             VALUES ('nora@firm.example', 'nora', 'Nora', 1, 'dev')",
            [],
        )
        .unwrap();
        let repos = attach_repos(&conn, "payments-core, audit-trail", "nora@firm.example").unwrap();
        assert_eq!(repos.len(), 2);
        assert!(attach_repos(&conn, "payments-core, audit-trail", "iris@firm.example").is_err());

        assert!(attach(&conn, "github_repo", "payments-core", "rosa@firm.example").is_ok());
        assert!(attach(&conn, "jira_board", "PAY", "rosa@firm.example").is_err());
        let boards = attach_boards(&conn, "PAY, AUD, PLAT", "iris@firm.example").unwrap();
        assert_eq!(boards.len(), 3);
        assert_eq!(boards[2]["key"], "PLAT");

        let saved = set_projects(
            &conn,
            "iris@firm.example",
            &["PAY-100".into(), "PLAT-300".into()],
        )
        .unwrap();
        assert_eq!(saved, vec!["PAY-100".to_string(), "PLAT-300".to_string()]);
        upsert_fixture_map(&conn, "payments-core", "AUD-200", "2026-01-01T00:00:00Z").unwrap();
        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM person_projects WHERE owner = 'iris@firm.example'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 2);
        let ws = workspace(&conn).unwrap();
        let iris = ws["people"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["jira_account_id"] == "iris@firm.example")
            .unwrap();
        assert_eq!(iris["role"], "ppm");
        assert_eq!(ws["memberships"].as_array().unwrap().len(), 2);
        assert!(set_projects(&conn, "iris@firm.example", &["NOPE-1".into()]).is_err());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn workspace_carries_the_merge_date_or_null() {
        let conn = db();
        conn.execute(
            "INSERT INTO pull_requests (id, repo, number, title, author, state, merged_at)
             VALUES ('PR_dated', 'payments-core', 41, 'Dated merge', 'nora', 'merged', '2026-09-22T10:00:00Z')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO pull_requests (id, repo, number, title, author, state, merged_at)
             VALUES ('PR_undated', 'payments-core', 43, 'Undated merge', 'nora', 'merged', NULL)",
            [],
        )
        .unwrap();
        let ws = workspace(&conn).unwrap();
        let prs = ws["prs"].as_array().unwrap();
        let dated = prs.iter().find(|p| p["pr_id"] == "PR_dated").unwrap();
        let undated = prs.iter().find(|p| p["pr_id"] == "PR_undated").unwrap();
        assert_eq!(dated["merged_at"].as_str(), Some("2026-09-22T10:00:00Z"));
        assert!(undated["merged_at"].is_null());
    }
}
