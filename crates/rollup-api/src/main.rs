//! Rollup API. P0 sync/read endpoints + P1 match/queue/rollup endpoints + UI.

use axum::{
    extract::{Path as AxPath, State},
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use rollup_core::{
    ai::{self, AiError},
    airun,
    briefs::{self, BriefError},
    draft::{FakeModel, Model},
    matcher,
    rollup::build_rollups,
    store,
    updates::{self, UpdateError},
};
use rollup_ingest::run_sync;
use serde_json::json;
use std::sync::{Arc, Mutex};
use subtle::ConstantTimeEq;

#[derive(Clone)]
struct AppState {
    conn: Arc<Mutex<store::Conn>>,
    settings: Arc<rollup_core::Settings>,
    write_token: String,
}

type ApiResult = Result<Json<serde_json::Value>, (axum::http::StatusCode, Json<serde_json::Value>)>;

fn err(status: axum::http::StatusCode, msg: impl Into<String>) -> ApiResult {
    Err((status, Json(json!({"error": msg.into()}))))
}

/// A refused connect write: role or owner -> 403, bad input -> 400, storage fault -> 500.
fn refused(e: rollup_core::connect::ConnectError) -> ApiResult {
    match e {
        rollup_core::connect::ConnectError::Forbidden(m) => err(axum::http::StatusCode::FORBIDDEN, m),
        rollup_core::connect::ConnectError::Invalid(m) => err(axum::http::StatusCode::BAD_REQUEST, m),
        rollup_core::connect::ConnectError::Internal(m) => {
            eprintln!("connect storage fault: {m}");
            err(
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                "Storage error. Nothing was written.",
            )
        }
    }
}

/// A cross-site page can only send a simple (text/plain) POST without a
/// preflight. Requiring JSON keeps other sites from driving these writes.
fn is_json(headers: &axum::http::HeaderMap) -> bool {
    headers
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.trim_start().to_ascii_lowercase().starts_with("application/json"))
        .unwrap_or(false)
}

async fn sync_now(State(state): State<AppState>) -> ApiResult {
    let conn = Arc::clone(&state.conn);
    let settings = Arc::clone(&state.settings);
    let result = tokio::task::spawn_blocking(move || {
        let conn = conn.lock().unwrap();
        run_sync(&conn, &settings).map(|r| {
            let m = matcher::run_match(&conn, settings.t_high, settings.t_low)
                .map_err(|e| e.to_string());
            (r, m)
        })
    })
    .await
    .unwrap_or_else(|_| Err("sync task panicked".into()));

    match result {
        Ok((sync, Ok(m))) => Ok(Json(json!({
            "sync": sync,
            "match": {"auto": m.auto, "candidates": m.candidates, "unmatched": m.unmatched}
        }))),
        Ok((sync, Err(e))) => Ok(Json(json!({"sync": sync, "match_error": e}))),
        Err(e) => err(axum::http::StatusCode::INTERNAL_SERVER_ERROR, e),
    }
}

async fn match_now(State(state): State<AppState>) -> ApiResult {
    let conn = Arc::clone(&state.conn);
    let t = Arc::clone(&state.settings);
    let result = tokio::task::spawn_blocking(move || {
        let conn = conn.lock().unwrap();
        matcher::run_match(&conn, t.t_high, t.t_low).map_err(|e| e.to_string())
    })
    .await
    .unwrap_or_else(|_| Err("match task panicked".into()));
    match result {
        Ok(m) => Ok(Json(json!({"auto": m.auto, "candidates": m.candidates, "unmatched": m.unmatched}))),
        Err(e) => err(axum::http::StatusCode::INTERNAL_SERVER_ERROR, e),
    }
}

async fn freshness(State(state): State<AppState>) -> ApiResult {
    let conn = state.conn.lock().unwrap();
    let mut stmt = conn
        .prepare("SELECT source, resource, cursor, last_success_at, status FROM sync_state")
        .map_err(|e| internal(e))?;
    let rows: Vec<serde_json::Value> = stmt
        .query_map([], |r| {
            Ok(json!({
                "source": r.get::<_, String>(0)?,
                "resource": r.get::<_, String>(1)?,
                "last_success_at": r.get::<_, Option<String>>(3)?,
                "status": r.get::<_, String>(4)?,
            }))
        })
        .map_err(|e| internal(e))?
        .filter_map(|r| r.ok())
        .collect();
    Ok(Json(json!({ "sources": rows, "matcher": matcher::MODEL_VERSION })))
}

fn internal(e: rusqlite::Error) -> (axum::http::StatusCode, Json<serde_json::Value>) {
    (axum::http::StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string()})))
}

async fn epics(State(state): State<AppState>) -> ApiResult {
    let conn = state.conn.lock().unwrap();
    let mut stmt = conn
        .prepare("SELECT epic_key, board, summary, status FROM epics ORDER BY epic_key")
        .map_err(internal)?;
    let rows: Vec<serde_json::Value> = stmt
        .query_map([], |r| {
            Ok(json!({
                "epic_key": r.get::<_, String>(0)?,
                "board": r.get::<_, String>(1)?,
                "summary": r.get::<_, String>(2)?,
                "status": r.get::<_, Option<String>>(3)?,
            }))
        })
        .map_err(internal)?
        .filter_map(|r| r.ok())
        .collect();
    Ok(Json(json!({ "epics": rows })))
}

async fn rollup(State(state): State<AppState>) -> ApiResult {
    let conn = state.conn.lock().unwrap();
    let rollups = build_rollups(&conn).map_err(internal)?;
    Ok(Json(json!({ "generated_at": store::utcnow(), "epics": rollups })))
}

async fn queue(State(state): State<AppState>) -> ApiResult {
    let conn = state.conn.lock().unwrap();
    let mut stmt = conn
        .prepare(
            "SELECT q.pr_id, p.repo, p.number, p.title, p.author, q.status,
                    q.suggested_story, q.suggested_score, q.notes
             FROM review_queue q JOIN pull_requests p ON p.id = q.pr_id
             WHERE q.status IN ('unmatched','candidate')
             ORDER BY q.updated_at",
        )
        .map_err(internal)?;
    let rows: Vec<serde_json::Value> = stmt
        .query_map([], |r| {
            Ok(json!({
                "pr_id": r.get::<_, String>(0)?,
                "repo": r.get::<_, String>(1)?,
                "pr_number": r.get::<_, i64>(2)?,
                "title": r.get::<_, String>(3)?,
                "author": r.get::<_, Option<String>>(4)?,
                "status": r.get::<_, String>(5)?,
                "suggested_story": r.get::<_, Option<String>>(6)?,
                "suggested_score": r.get::<_, Option<f64>>(7)?,
                "notes": r.get::<_, Option<String>>(8)?,
            }))
        })
        .map_err(internal)?
        .filter_map(|r| r.ok())
        .collect();
    Ok(Json(json!({ "queue": rows })))
}

async fn summary(State(state): State<AppState>) -> ApiResult {
    let conn = state.conn.lock().unwrap();
    let s = rollup_core::leadership::build_summary(&conn).map_err(internal)?;
    Ok(Json(s))
}

async fn forecast(State(state): State<AppState>) -> ApiResult {
    let conn = state.conn.lock().unwrap();
    let f = rollup_core::leadership::build_forecast(&conn).map_err(internal)?;
    Ok(Json(json!({"generated_at": store::utcnow(), "forecast": f})))
}

async fn queue_action(
    State(state): State<AppState>,
    AxPath(pr_id): AxPath<String>,
    headers: axum::http::HeaderMap,
    body: String,
) -> ApiResult {
    if !is_json(&headers) {
        return err(axum::http::StatusCode::UNSUPPORTED_MEDIA_TYPE, "Send application/json.");
    }
    // body: {"action":"confirm"|"ignore","story_key":"PAY-101"} (ignore needs no story)
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&body) else {
        return err(axum::http::StatusCode::BAD_REQUEST, "invalid JSON");
    };
    let action = v["action"].as_str().unwrap_or("").to_string();
    let story_key = v["story_key"].as_str().unwrap_or("").to_string();
    let conn = Arc::clone(&state.conn);
    let pr = pr_id.clone();
    let result = tokio::task::spawn_blocking(move || {
        let conn = conn.lock().unwrap();
        match action.as_str() {
            "confirm" if !story_key.is_empty() => {
                // One transaction: a refused confirm keeps the link the PR has.
                let tx = conn.unchecked_transaction().map_err(|e| e.to_string())?;
                let stored = |sql: &str, key: &str| -> Result<bool, String> {
                    tx.query_row(sql, [key], |r| r.get::<_, i64>(0))
                        .map(|n| n > 0)
                        .map_err(|e| e.to_string())
                };
                if !stored("SELECT COUNT(*) FROM pull_requests WHERE id = ?1", &pr)? {
                    return Err("That pull request is not stored. Nothing was linked.".to_string());
                }
                if !stored("SELECT COUNT(*) FROM stories WHERE story_key = ?1", &story_key)? {
                    return Err(format!("That story is not stored: {story_key}. Nothing was linked."));
                }
                tx.execute(
                    "UPDATE pr_story_links SET active=0 WHERE pr_id=?1",
                    [&pr],
                )
                .map_err(|e| e.to_string())?;
                tx.execute(
                    "INSERT INTO pr_story_links (pr_id, story_key, score, method, status, created_at)
                     VALUES (?1, ?2, 1.0, 'confirmed', 'confirmed', ?3)
                     ON CONFLICT(pr_id, story_key) DO UPDATE SET status='confirmed', active=1",
                    rusqlite::params![pr, story_key, store::utcnow()],
                )
                .map_err(|e| e.to_string())?;
                tx.execute(
                    "UPDATE review_queue SET status='linked', suggested_story=?2, updated_at=?3 WHERE pr_id=?1",
                    rusqlite::params![pr, story_key, store::utcnow()],
                )
                .map_err(|e| e.to_string())?;
                tx.commit().map_err(|e| e.to_string())?;
                Ok(json!({"ok": true, "action": "confirmed", "story": story_key}))
            }
            "ignore" => {
                conn.execute(
                    "UPDATE review_queue SET status='ignored', updated_at=?2 WHERE pr_id=?1",
                    rusqlite::params![pr, store::utcnow()],
                )
                .map_err(|e| e.to_string())?;
                Ok(json!({"ok": true, "action": "ignored"}))
            }
            _ => Err("action must be confirm (with story_key) or ignore".to_string()),
        }
    })
    .await
    .unwrap_or_else(|_| Err("task panicked".into()));

    match result {
        Ok(v) => Ok(Json(v)),
        Err(e) => err(axum::http::StatusCode::BAD_REQUEST, e),
    }
}

async fn workspace_now(State(state): State<AppState>) -> ApiResult {
    let conn = Arc::clone(&state.conn);
    let mode = state.settings.sync_mode.clone();
    let result = tokio::task::spawn_blocking(move || {
        let conn = conn.lock().unwrap();
        rollup_core::connect::workspace(&conn)
    })
    .await;
    let Ok(result) = result else {
        return err(axum::http::StatusCode::INTERNAL_SERVER_ERROR, "task panicked");
    };
    let mut v = result.map_err(internal)?;
    if let Some(obj) = v.as_object_mut() {
        obj.insert("mode".into(), json!(mode));
    }
    Ok(Json(v))
}

fn key_has_many(raw: &str) -> bool {
    raw.split(|c: char| c == ',' || c.is_whitespace())
        .filter(|s| !s.trim().is_empty())
        .nth(1)
        .is_some()
}

async fn select_projects(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    body: String,
) -> ApiResult {
    if !is_json(&headers) {
        return err(axum::http::StatusCode::UNSUPPORTED_MEDIA_TYPE, "Send application/json.");
    }
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&body) else {
        return err(axum::http::StatusCode::BAD_REQUEST, "invalid JSON");
    };
    let owner = v["owner"].as_str().unwrap_or("").to_string();
    let keys: Vec<String> = match v.get("epic_keys") {
        Some(serde_json::Value::Array(a)) => a
            .iter()
            .filter_map(|x| x.as_str().map(|s| s.to_string()))
            .collect(),
        _ => return err(axum::http::StatusCode::BAD_REQUEST, "epic_keys must be a list. An empty list clears the pick."),
    };
    let conn = Arc::clone(&state.conn);
    let result = tokio::task::spawn_blocking(move || {
        let conn = conn.lock().unwrap();
        rollup_core::connect::set_projects(&conn, &owner, &keys)
    })
    .await;
    let Ok(result) = result else {
        return err(axum::http::StatusCode::INTERNAL_SERVER_ERROR, "task panicked");
    };
    match result {
        Ok(saved) => Ok(Json(json!({"ok": true, "epic_keys": saved}))),
        Err(e) => refused(e),
    }
}

async fn attach_now(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    body: String,
) -> ApiResult {
    if !is_json(&headers) {
        return err(axum::http::StatusCode::UNSUPPORTED_MEDIA_TYPE, "Send application/json.");
    }
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&body) else {
        return err(axum::http::StatusCode::BAD_REQUEST, "invalid JSON");
    };
    let kind = v["kind"].as_str().unwrap_or("").to_string();
    let key = v["key"].as_str().unwrap_or("").to_string();
    let owner = v["owner"].as_str().unwrap_or("").to_string();
    let conn = Arc::clone(&state.conn);
    let result = tokio::task::spawn_blocking(move || {
        let conn = conn.lock().unwrap();
        if kind == "jira_board" && key_has_many(&key) {
            let attached = rollup_core::connect::attach_boards(&conn, &key, &owner)?;
            return Ok(json!({"ok": true, "attached": attached}));
        }
        if kind == "github_repo" && key_has_many(&key) {
            let attached = rollup_core::connect::attach_repos(&conn, &key, &owner)?;
            return Ok(json!({"ok": true, "attached": attached}));
        }
        rollup_core::connect::attach(&conn, &kind, &key, &owner)
    })
    .await;
    let Ok(result) = result else {
        return err(axum::http::StatusCode::INTERNAL_SERVER_ERROR, "task panicked");
    };
    match result {
        Ok(v) => Ok(Json(v)),
        Err(e) => refused(e),
    }
}

async fn map_now(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    body: String,
) -> ApiResult {
    if !is_json(&headers) {
        return err(axum::http::StatusCode::UNSUPPORTED_MEDIA_TYPE, "Send application/json.");
    }
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&body) else {
        return err(axum::http::StatusCode::BAD_REQUEST, "invalid JSON");
    };
    let repo = v["repo"].as_str().unwrap_or("").to_string();
    let epic_key = v["epic_key"].as_str().unwrap_or("").to_string();
    // A missing updated_by is blank (403), not the lead. The page always sends it.
    let updated_by = v["updated_by"].as_str().unwrap_or("").to_string();
    let conn = Arc::clone(&state.conn);
    let settings = Arc::clone(&state.settings);
    let result = tokio::task::spawn_blocking(move || {
        let conn = conn.lock().unwrap();
        rollup_core::connect::map_repo(&conn, &repo, &epic_key, &updated_by)?;
        let matched = match matcher::run_match(&conn, settings.t_high, settings.t_low) {
            Ok(m) => json!({"auto": m.auto, "candidates": m.candidates, "unmatched": m.unmatched}),
            Err(e) => json!({"error": e.to_string()}),
        };
        Ok::<_, rollup_core::connect::ConnectError>(json!({
            "ok": true,
            "repo": repo,
            "epic_key": epic_key,
            "match": matched,
        }))
    })
    .await;
    let Ok(result) = result else {
        return err(axum::http::StatusCode::INTERNAL_SERVER_ERROR, "task panicked");
    };
    match result {
        Ok(v) => Ok(Json(v)),
        Err(e) => refused(e),
    }
}

/// The Host header must name this server: `127.0.0.1`, `localhost` or
/// `[::1]`, with no port or with the port it listens on. Anything else is
/// refused (421) before a handler runs, so a page on a rebinding domain
/// (attacker.example resolving to 127.0.0.1) can neither read nor write.
/// Other routes also require the local token: see `only_with_token`.
fn host_allowed(host: &str, port: u16) -> bool {
    let host = host.trim().to_ascii_lowercase();
    ["127.0.0.1", "localhost", "[::1]"]
        .iter()
        .any(|name| host == *name || host == format!("{name}:{port}"))
}

async fn only_local_host(
    State(port): State<u16>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    // Exactly one Host header, valid visible ASCII, naming this server. The
    // authority of an absolute-form request line is never used instead.
    let mut hosts = req.headers().get_all(axum::http::header::HOST).iter();
    let host = match (hosts.next(), hosts.next()) {
        (Some(v), None) => v.to_str().ok(),
        _ => None,
    };
    if host.is_some_and(|h| host_allowed(h, port)) {
        return next.run(req).await;
    }
    (
        axum::http::StatusCode::MISDIRECTED_REQUEST,
        [(axum::http::header::CACHE_CONTROL, "no-store")],
        Json(json!({
            "error": "Open this page at 127.0.0.1, localhost or [::1] on this port. Nothing was read or written."
        })),
    )
        .into_response()
}

/// An Origin header, when present, must be this server over http. A browser
/// leaves the port out of the Origin only for 80 (TALLIMO_PORT=80).
fn origin_allowed(origin: &str, port: u16) -> bool {
    let origin = origin.trim().to_ascii_lowercase();
    ["127.0.0.1", "localhost", "[::1]"].iter().any(|name| {
        origin == format!("http://{name}:{port}") || (port == 80 && origin == format!("http://{name}"))
    })
}

#[derive(Clone)]
struct WriteGate {
    port: u16,
    token: String,
}

/// Every route except `/`, `/signal/{*path}` and `/health/freshness` is refused
/// before a handler runs.
///
/// An `Origin` that is not this server, or a `Sec-Fetch-Site` other than
/// `same-origin` or `none`, is a 403. Those headers can only add refusals.
/// `X-Tallimo-Token` must then match the local token, or the call is a 401.
/// A same-origin header never grants access.
async fn only_with_token(
    State(gate): State<WriteGate>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    // ponytail: the token is the only way in. It stops other websites (they cannot read
    // the token, and Origin/Sec-Fetch-Site only add refusals) and other local user accounts
    // (the token file is mode 0600). It does not stop code running as this user, which can
    // read that file. Per-user isolation needs a unix socket or file-permission auth (#102).
    let foreign_origin = req.headers().get_all(axum::http::header::ORIGIN).iter().any(|origin| {
        !origin.to_str().is_ok_and(|value| origin_allowed(value, gate.port))
    });
    let foreign_site = req.headers().get_all("sec-fetch-site").iter().any(|site| {
        !site.to_str().is_ok_and(|value| value == "same-origin" || value == "none")
    });
    if foreign_origin || foreign_site {
        return (
            axum::http::StatusCode::FORBIDDEN,
            [(axum::http::header::CACHE_CONTROL, "no-store")],
            Json(json!({
                "error": "This request came from a page on another site, so it was refused. Use the page at 127.0.0.1, localhost or [::1] on this port. Nothing was read or written."
            })),
        )
            .into_response();
    }
    let presented = req
        .headers()
        .get("x-tallimo-token")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if token_eq(presented, &gate.token) {
        return next.run(req).await;
    }
    (
        axum::http::StatusCode::UNAUTHORIZED,
        [(axum::http::header::CACHE_CONTROL, "no-store")],
        Json(json!({"error": "Missing or wrong Tallimo token."})),
    )
        .into_response()
}

/// Constant-time compare. An empty server token or an empty presented token
/// never matches, so a missing file cannot authorize a missing header.
fn token_eq(a: &str, b: &str) -> bool {
    if a.is_empty() || b.is_empty() {
        return false;
    }
    a.as_bytes().ct_eq(b.as_bytes()).into()
}

/// Largest request body /feedback accepts; anything bigger is refused (413) unread.
const FEEDBACK_MAX_BODY: usize = 4096;
/// Largest request body a leadership-update, partner-brief, or AI write accepts.
const UPDATES_MAX_BODY: usize = 64 * 1024;
/// One writer at a time, so two flags never interleave in feedback.jsonl.
static FEEDBACK_LOCK: Mutex<()> = Mutex::new(());

/// Operator flag on a Today or Inbox card (human loop). Appends one line to the
/// local feedback.jsonl next to the database; nothing is sent anywhere. The
/// Host and Origin checks in `app` run first, and the body is capped.
async fn feedback_now(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    body: String,
) -> ApiResult {
    if !is_json(&headers) {
        return err(axum::http::StatusCode::UNSUPPORTED_MEDIA_TYPE, "Send application/json.");
    }
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&body) else {
        return err(axum::http::StatusCode::BAD_REQUEST, "invalid JSON");
    };
    let field = |k: &str| v[k].as_str().unwrap_or("").to_string();
    let (page, card, verdict, note) = (field("page"), field("card"), field("verdict"), field("note"));
    let path = rollup_core::feedback::path_for(&state.settings.db_path);
    let result = tokio::task::spawn_blocking(move || {
        let _guard = FEEDBACK_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        rollup_core::feedback::append(&path, &page, &card, &verdict, &note)
    })
    .await;
    match result {
        Ok(Ok(())) => Ok(Json(json!({"ok": true}))),
        Ok(Err(rollup_core::feedback::FeedbackError::Invalid(m))) => err(axum::http::StatusCode::BAD_REQUEST, m),
        Ok(Err(rollup_core::feedback::FeedbackError::Io(m))) => {
            eprintln!("feedback write fault: {m}");
            err(axum::http::StatusCode::INTERNAL_SERVER_ERROR, "Could not save the flag. Nothing was written.")
        }
        Err(_) => err(axum::http::StatusCode::INTERNAL_SERVER_ERROR, "task panicked"),
    }
}

fn update_refused(e: UpdateError) -> ApiResult {
    match e {
        UpdateError::Sent => err(
            axum::http::StatusCode::CONFLICT,
            "This entry was sent. It is a record of what the Director saw.",
        ),
        UpdateError::DraftOpen => err(axum::http::StatusCode::CONFLICT, "A draft is already open."),
        UpdateError::NoDraft => err(axum::http::StatusCode::CONFLICT, "There is no open draft."),
        UpdateError::Changed => err(
            axum::http::StatusCode::CONFLICT,
            "This item changed while you were editing. Reload and try again.",
        ),
        UpdateError::NotFound => err(axum::http::StatusCode::NOT_FOUND, "No such item on the draft."),
        UpdateError::Invalid(m) => err(axum::http::StatusCode::BAD_REQUEST, m),
        UpdateError::Db(m) => {
            eprintln!("leadership update storage fault: {m}");
            err(
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                "Storage error. Nothing was written.",
            )
        }
    }
}

fn ai_err(e: AiError) -> UpdateError {
    match e {
        AiError::Invalid(m) => UpdateError::Invalid(m),
        AiError::Db(m) => UpdateError::Db(m),
    }
}

async fn with_editor(
    state: AppState,
    headers: axum::http::HeaderMap,
    body: String,
    run: impl FnOnce(&store::Conn, &serde_json::Value) -> Result<serde_json::Value, UpdateError> + Send + 'static,
) -> ApiResult {
    if !is_json(&headers) {
        return err(axum::http::StatusCode::UNSUPPORTED_MEDIA_TYPE, "Send application/json.");
    }
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&body) else {
        return err(axum::http::StatusCode::BAD_REQUEST, "invalid JSON");
    };
    let conn = Arc::clone(&state.conn);
    let joined = tokio::task::spawn_blocking(move || {
        let conn = conn.lock().unwrap();
        let owner = v["owner"].as_str().unwrap_or("");
        if !matches!(rollup_core::connect::owner_role(&conn, owner), Some("pm" | "ppm")) {
            return Err(None);
        }
        run(&conn, &v).map_err(Some)
    })
    .await;
    match joined {
        Ok(Ok(body)) => Ok(Json(body)),
        Ok(Err(None)) => err(
            axum::http::StatusCode::FORBIDDEN,
            "Only the owner can edit leadership updates.",
        ),
        Ok(Err(Some(e))) => update_refused(e),
        Err(_) => err(axum::http::StatusCode::INTERNAL_SERVER_ERROR, "task panicked"),
    }
}

async fn ai_read(
    state: AppState,
    who: Option<String>,
    run: impl FnOnce(&store::Conn) -> Result<serde_json::Value, UpdateError> + Send + 'static,
) -> ApiResult {
    let conn = Arc::clone(&state.conn);
    let joined = tokio::task::spawn_blocking(move || {
        let conn = conn.lock().unwrap();
        if !matches!(
            who.as_deref().and_then(|owner| rollup_core::connect::owner_role(&conn, owner)),
            Some("pm" | "ppm")
        ) {
            return Err(None);
        }
        run(&conn).map_err(Some)
    })
    .await;
    match joined {
        Ok(Ok(body)) => Ok(Json(body)),
        Ok(Err(None)) => err(
            axum::http::StatusCode::FORBIDDEN,
            "Only the owner can read AI notes and sources.",
        ),
        Ok(Err(Some(e))) => update_refused(e),
        Err(_) => err(axum::http::StatusCode::INTERNAL_SERVER_ERROR, "task panicked"),
    }
}

fn item_json(item: &updates::Item) -> serde_json::Value {
    json!({
        "id": item.id,
        "entry_id": item.entry_id,
        "item_id": item.item_id,
        "category": item.category,
        "position": item.position,
        "title": item.title,
        "priority": item.priority,
        "start_date": item.start_date,
        "end_date": item.end_date,
        "pct": item.pct,
        "capacity": item.capacity,
        "carried": item.carried,
        "completed": item.completed,
        "next": item.next,
        "future": item.future,
        "is_new": item.is_new,
    })
}

fn entry_json(entry: &updates::Entry) -> serde_json::Value {
    let mut value = json!({
        "id": entry.id,
        "entry_date": entry.entry_date,
        "created_at": entry.created_at,
        "sent_at": entry.sent_at,
        "programme": entry.programme,
        "items": entry.items.iter().map(item_json).collect::<Vec<_>>(),
        "total": entry.total_capacity,
        "label": entry.capacity_label,
    });
    if entry.corrected {
        value["corrected"] = json!(true);
    }
    value
}

fn read_item_input(v: &serde_json::Value) -> Result<updates::ItemInput, UpdateError> {
    if !v.is_object() {
        return Err(UpdateError::Invalid("item must be an object.".into()));
    }
    let text = |key: &str| -> Result<String, UpdateError> {
        v[key]
            .as_str()
            .map(|s| s.to_string())
            .ok_or_else(|| UpdateError::Invalid(format!("{key} is required.")))
    };
    let whole = |key: &str| -> Result<i64, UpdateError> {
        v[key]
            .as_i64()
            .ok_or_else(|| UpdateError::Invalid(format!("{key} must be a whole number.")))
    };
    let opt_whole = |key: &str| -> Result<Option<i64>, UpdateError> {
        match v.get(key) {
            None | Some(serde_json::Value::Null) => Ok(None),
            Some(n) => n
                .as_i64()
                .map(Some)
                .ok_or_else(|| UpdateError::Invalid(format!("{key} must be a whole number."))),
        }
    };
    let opt_text = |key: &str| -> Result<Option<String>, UpdateError> {
        match v.get(key) {
            None | Some(serde_json::Value::Null) => Ok(None),
            Some(serde_json::Value::String(s)) => Ok(Some(s.clone())),
            Some(_) => Err(UpdateError::Invalid(format!("{key} must be text."))),
        }
    };
    let lines = |key: &str| -> Result<Vec<String>, UpdateError> {
        match v.get(key) {
            None | Some(serde_json::Value::Null) => Ok(Vec::new()),
            Some(serde_json::Value::Array(rows)) => rows
                .iter()
                .map(|row| {
                    row.as_str().map(|s| s.to_string()).ok_or_else(|| {
                        UpdateError::Invalid(format!("{key} must be a list of text."))
                    })
                })
                .collect(),
            Some(_) => Err(UpdateError::Invalid(format!("{key} must be a list of text."))),
        }
    };
    Ok(updates::ItemInput {
        item_id: text("item_id")?,
        category: text("category")?,
        position: whole("position")?,
        title: text("title")?,
        priority: whole("priority")?,
        start_date: opt_text("start_date")?,
        end_date: opt_text("end_date")?,
        pct: opt_whole("pct")?,
        capacity: opt_whole("capacity")?,
        completed: lines("completed")?,
        next: lines("next")?,
        future: lines("future")?,
    })
}

async fn updates_list(
    State(state): State<AppState>,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> ApiResult {
    let who = q.get("who").cloned();
    let conn = Arc::clone(&state.conn);
    let mode = state.settings.ai_publish.clone();
    let result = tokio::task::spawn_blocking(move || -> Result<serde_json::Value, UpdateError> {
        let conn = conn.lock().unwrap();
        let include = matches!(
            who.as_deref().and_then(|owner| rollup_core::connect::owner_role(&conn, owner)),
            Some("pm" | "ppm")
        );
        let entries = updates::list(&conn, include)?;
        let published = if entries.iter().any(|entry| entry.sent_at.is_none()) {
            Some(airun::open_ai_published(&conn, &mode)?)
        } else {
            None
        };
        Ok(json!({
            "entries": entries.iter().map(|entry| {
                let mut value = entry_json(entry);
                if entry.sent_at.is_none() {
                    value["ai_published"] = json!(published.unwrap_or(false));
                }
                value
            }).collect::<Vec<_>>()
        }))
    })
    .await;
    match result {
        Ok(Ok(body)) => Ok(Json(body)),
        Ok(Err(e)) => update_refused(e),
        Err(_) => err(axum::http::StatusCode::INTERNAL_SERVER_ERROR, "task panicked"),
    }
}

async fn updates_next(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    body: String,
) -> ApiResult {
    with_editor(state, headers, body, |conn, v| {
        let date = v["entry_date"].as_str().unwrap_or("");
        let id = updates::next_from_last(conn, date, &store::utcnow())?;
        Ok(json!({"ok": true, "id": id}))
    })
    .await
}

async fn updates_item(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    body: String,
) -> ApiResult {
    with_editor(state, headers, body, |conn, v| {
        let Some(item) = v.get("item") else {
            return Err(UpdateError::Invalid("item is required.".into()));
        };
        let item = read_item_input(item)?;
        let expect = match v.get("expect") {
            None | Some(serde_json::Value::Null) => None,
            Some(raw) if raw.is_object() => Some(read_item_input(raw)?),
            Some(_) => {
                return Err(UpdateError::Invalid(
                    "expect must be an item object.".into(),
                ))
            }
        };
        updates::upsert_item_if(conn, &item, expect.as_ref())?;
        Ok(json!({"ok": true}))
    })
    .await
}

async fn updates_item_remove(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    body: String,
) -> ApiResult {
    with_editor(state, headers, body, |conn, v| {
        let Some(item_id) = v["item_id"].as_str() else {
            return Err(UpdateError::Invalid("item_id is required.".into()));
        };
        updates::remove_item(conn, item_id)?;
        Ok(json!({"ok": true}))
    })
    .await
}

async fn updates_item_move(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    body: String,
) -> ApiResult {
    with_editor(state, headers, body, |conn, v| {
        let Some(item_id) = v["item_id"].as_str() else {
            return Err(UpdateError::Invalid("item_id is required.".into()));
        };
        let Some(category) = v["category"].as_str() else {
            return Err(UpdateError::Invalid("category is required.".into()));
        };
        let Some(position) = v["position"].as_i64() else {
            return Err(UpdateError::Invalid("position must be a whole number.".into()));
        };
        updates::move_item(conn, item_id, category, position)?;
        Ok(json!({"ok": true}))
    })
    .await
}

async fn updates_settings(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    body: String,
) -> ApiResult {
    with_editor(state, headers, body, |conn, v| {
        let programme = match v.get("programme") {
            None => None,
            Some(serde_json::Value::Null) => Some(""),
            Some(serde_json::Value::String(s)) => Some(s.as_str()),
            Some(_) => return Err(UpdateError::Invalid("programme must be text.".into())),
        };
        let order = match v.get("category_order") {
            None => None,
            Some(serde_json::Value::Array(rows)) => {
                let mut names = Vec::with_capacity(rows.len());
                for row in rows {
                    let Some(name) = row.as_str() else {
                        return Err(UpdateError::Invalid(
                            "category_order must be a list of names.".into(),
                        ));
                    };
                    names.push(name.to_string());
                }
                Some(names)
            }
            Some(_) => {
                return Err(UpdateError::Invalid(
                    "category_order must be a list of names.".into(),
                ))
            }
        };
        updates::set_settings(conn, programme, order.as_deref())?;
        Ok(json!({"ok": true}))
    })
    .await
}

async fn updates_send(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    body: String,
) -> ApiResult {
    with_editor(state, headers, body, |conn, _v| {
        updates::send(conn, &store::utcnow())?;
        Ok(json!({"ok": true}))
    })
    .await
}

async fn updates_discard(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    body: String,
) -> ApiResult {
    with_editor(state, headers, body, |conn, _v| {
        updates::discard(conn)?;
        Ok(json!({"ok": true}))
    })
    .await
}

async fn updates_correct(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    body: String,
) -> ApiResult {
    with_editor(state, headers, body, |conn, v| {
        // Missing used to become "" and delete the phrase on confirm.
        let Some(find) = v["find"].as_str() else {
            return Err(UpdateError::Invalid("Type the text to find.".into()));
        };
        let Some(replace) = v["replace"].as_str() else {
            return Err(UpdateError::Invalid("Type the replacement text.".into()));
        };
        let preview = v["preview"].as_bool() == Some(true);
        if !preview && v["confirm"].as_bool() != Some(true) {
            return Err(UpdateError::Invalid(
                "Confirm the correction first.".into(),
            ));
        }
        // owner is the person this request names. A body with no person records "owner".
        let by = v["owner"]
            .as_str()
            .filter(|name| !name.is_empty())
            .unwrap_or("owner");
        let mode = if preview {
            updates::CorrectMode::Preview
        } else {
            updates::CorrectMode::Apply
        };
        let n = updates::correct(conn, find, replace, mode, by)?.rows;
        if preview {
            Ok(json!({"would_change": n}))
        } else {
            Ok(json!({"changed": n}))
        }
    })
    .await
}

async fn ai_publish_now(State(state): State<AppState>) -> ApiResult {
    let conn = Arc::clone(&state.conn);
    let joined = tokio::task::spawn_blocking(move || {
        let conn = conn.lock().unwrap();
        let at = airun::publish_open(&conn, &store::utcnow())?;
        Ok(json!({"published_at": at}))
    })
    .await;
    match joined {
        Ok(Ok(body)) => Ok(Json(body)),
        Ok(Err(e)) => update_refused(e),
        Err(_) => err(axum::http::StatusCode::INTERNAL_SERVER_ERROR, "task panicked"),
    }
}

const AI_MODEL: &str = "fake";

async fn ai_run_now(State(state): State<AppState>) -> ApiResult {
    let conn = Arc::clone(&state.conn);
    let joined = tokio::task::spawn_blocking(move || {
        let conn = conn.lock().unwrap();
        let now = store::utcnow();
        let run = airun::run_once(&conn, &rollup_core::draft::FakeModel, AI_MODEL, &now).map_err(ai_err)?;
        Ok(json!({
            "outcome": run.outcome,
            "facts": run.facts,
            "accepted": run.accepted,
            "rejected": run.rejected,
        }))
    })
    .await;
    match joined {
        Ok(Ok(body)) => Ok(Json(body)),
        Ok(Err(e)) => update_refused(e),
        Err(_) => err(axum::http::StatusCode::INTERNAL_SERVER_ERROR, "task panicked"),
    }
}

async fn ai_notes_add(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    body: String,
) -> ApiResult {
    with_editor(state, headers, body, |conn, v| {
        let id = ai::add_note(
            conn,
            v["kind"].as_str().unwrap_or(""),
            v["body"].as_str().unwrap_or(""),
            &store::utcnow(),
        )
        .map_err(ai_err)?;
        Ok(json!({"id": id}))
    })
    .await
}

async fn ai_notes_resolve(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    body: String,
) -> ApiResult {
    with_editor(state, headers, body, |conn, v| {
        let Some(id) = v["id"].as_i64() else {
            return Err(UpdateError::Invalid("id must be a whole number.".into()));
        };
        let resolved = ai::resolve_note(conn, id, &store::utcnow()).map_err(ai_err)?;
        Ok(json!({"resolved": resolved}))
    })
    .await
}

async fn ai_map(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    body: String,
) -> ApiResult {
    with_editor(state, headers, body, |conn, v| {
        ai::map_source(
            conn,
            v["item_id"].as_str().unwrap_or(""),
            v["kind"].as_str().unwrap_or(""),
            v["key"].as_str().unwrap_or(""),
            &store::utcnow(),
        )
        .map_err(ai_err)?;
        Ok(json!({"ok": true}))
    })
    .await
}

async fn ai_unmap(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    body: String,
) -> ApiResult {
    with_editor(state, headers, body, |conn, v| {
        let removed = ai::unmap_source(
            conn,
            v["item_id"].as_str().unwrap_or(""),
            v["kind"].as_str().unwrap_or(""),
            v["key"].as_str().unwrap_or(""),
        )
        .map_err(ai_err)?;
        Ok(json!({"removed": removed}))
    })
    .await
}

async fn ai_notes_list(
    State(state): State<AppState>,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> ApiResult {
    let who = q.get("who").cloned();
    let kind = q.get("kind").cloned().filter(|k| !k.trim().is_empty());
    ai_read(state, who, move |conn| {
        let notes = ai::open_notes(conn, kind.as_deref()).map_err(ai_err)?;
        Ok(json!({
            "notes": notes.iter().map(|note| json!({
                "id": note.id,
                "kind": note.kind,
                "body": note.body,
                "created_at": note.created_at,
            })).collect::<Vec<_>>()
        }))
    })
    .await
}

async fn ai_sources_list(
    State(state): State<AppState>,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> ApiResult {
    let who = q.get("who").cloned();
    let item_id = q.get("item_id").cloned();
    ai_read(state, who, move |conn| {
        let Some(item_id) = item_id.as_deref().map(str::trim).filter(|s| !s.is_empty()) else {
            return Err(UpdateError::Invalid("item_id is required.".into()));
        };
        let rows = ai::sources(conn, item_id).map_err(ai_err)?;
        Ok(json!({
            "sources": rows.iter().map(|(kind, key)| json!({"kind": kind, "key": key})).collect::<Vec<_>>()
        }))
    })
    .await
}

async fn ai_brief_run(State(state): State<AppState>) -> ApiResult {
    let conn = Arc::clone(&state.conn);
    let result = tokio::task::spawn_blocking(move || {
        let conn = conn.lock().unwrap();
        let now = store::utcnow();
        let model: &dyn Model = &FakeModel;
        rollup_core::abrief::brief_run_once(&conn, model, "fake", &now)
    })
    .await;
    match result {
        Ok(Ok(run)) => Ok(Json(json!({
            "outcome": run.outcome,
            "facts": run.facts,
            "accepted": run.accepted,
            "rejected": run.rejected,
        }))),
        Ok(Err(AiError::Invalid(msg))) => err(axum::http::StatusCode::BAD_REQUEST, msg),
        Ok(Err(AiError::Db(msg))) => {
            eprintln!("brief run storage fault: {msg}");
            err(
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                "Storage error. Nothing was written.",
            )
        }
        Err(_) => err(axum::http::StatusCode::INTERNAL_SERVER_ERROR, "task panicked"),
    }
}

fn runs_limit(raw: Option<&str>) -> Result<i64, UpdateError> {
    let Some(raw) = raw.map(str::trim).filter(|text| !text.is_empty()) else {
        return Ok(20);
    };
    match raw.parse::<i64>() {
        Ok(n) if (1..=100).contains(&n) => Ok(n),
        Ok(n) if n > 100 => Ok(100),
        _ => Err(UpdateError::Invalid(
            "limit must be a whole number from 1 to 100.".into(),
        )),
    }
}

async fn ai_runs_list(
    State(state): State<AppState>,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> ApiResult {
    let who = q.get("who").cloned();
    let limit = q.get("limit").cloned();
    ai_read(state, who, move |conn| {
        let limit = runs_limit(limit.as_deref())?;
        let runs = ai::recent_runs(conn, limit).map_err(ai_err)?;
        Ok(json!({
            "runs": runs.iter().map(ai::RunRow::to_json).collect::<Vec<_>>()
        }))
    })
    .await
}

fn ai_status_json(
    scheduled_off: bool,
    publish_mode: &str,
    model: &str,
    due: Option<&str>,
) -> serde_json::Value {
    json!({
        "scheduled": if scheduled_off { "off" } else { "on" },
        "publish_mode": publish_mode,
        "model": model,
        "due": due,
    })
}

async fn ai_status(
    State(state): State<AppState>,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> ApiResult {
    let who = q.get("who").cloned();
    let publish_mode = state.settings.ai_publish.clone();
    ai_read(state, who, move |conn| {
        // Same check as airun::is_due: ROLLUP_AI=off, anything else is on.
        let scheduled_off = std::env::var("ROLLUP_AI").ok().as_deref() == Some("off");
        let due = match ny_clock() {
            Ok((weekday, minute)) => {
                airun::is_due(conn, weekday, minute, &store::utcnow()).map_err(ai_err)?
            }
            Err(err) => {
                eprintln!("ai status: {err}");
                Some("clock unavailable")
            }
        };
        Ok(ai_status_json(scheduled_off, &publish_mode, AI_MODEL, due))
    })
    .await
}

async fn ai_changes_list(
    State(state): State<AppState>,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> ApiResult {
    let who = q.get("who").cloned();
    let since = q.get("since").cloned().unwrap_or_default();
    ai_read(state, who, move |conn| {
        let rows = ai::changes_since(conn, &since).map_err(ai_err)?;
        Ok(json!({
            "changes": rows.iter().map(ai::ChangeRow::to_json).collect::<Vec<_>>()
        }))
    })
    .await
}

async fn ai_flags_list(
    State(state): State<AppState>,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> ApiResult {
    let who = q.get("who").cloned();
    ai_read(state, who, |conn| {
        let flags = ai::open_flags(conn).map_err(ai_err)?;
        Ok(json!({
            "flags": flags.iter().map(|flag| json!({
                "id": flag.id,
                "item_id": flag.item_id,
                "field": flag.field,
                "reason": flag.reason,
                "raised_at": flag.raised_at,
            })).collect::<Vec<_>>()
        }))
    })
    .await
}

async fn ai_flags_resolve(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    body: String,
) -> ApiResult {
    let result = with_editor(state, headers, body, |conn, v| {
        let Some(id) = v["id"].as_i64() else {
            return Err(UpdateError::Invalid("id must be a whole number.".into()));
        };
        let resolved = ai::resolve_flag(conn, id, &store::utcnow()).map_err(ai_err)?;
        Ok(json!({"resolved": resolved}))
    })
    .await;
    match result {
        Ok(Json(body)) if body["resolved"].as_bool() == Some(false) => {
            err(axum::http::StatusCode::NOT_FOUND, "No such open flag.")
        }
        other => other,
    }
}

fn brief_refused(e: BriefError) -> ApiResult {
    match e {
        BriefError::Sent => err(
            axum::http::StatusCode::CONFLICT,
            "This brief was sent. It is a record of what the Senior Partner saw.",
        ),
        BriefError::DraftOpen => err(axum::http::StatusCode::CONFLICT, "A draft is already open."),
        BriefError::NoDraft => err(axum::http::StatusCode::CONFLICT, "There is no open draft."),
        BriefError::NotFound => err(axum::http::StatusCode::NOT_FOUND, "No such item on the draft."),
        BriefError::Invalid(m) => err(axum::http::StatusCode::BAD_REQUEST, m),
        BriefError::Db(m) => {
            eprintln!("partner brief storage fault: {m}");
            err(
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                "Storage error. Nothing was written.",
            )
        }
    }
}

async fn with_brief_editor(
    state: AppState,
    headers: axum::http::HeaderMap,
    body: String,
    run: impl FnOnce(&store::Conn, &serde_json::Value) -> Result<serde_json::Value, BriefError> + Send + 'static,
) -> ApiResult {
    if !is_json(&headers) {
        return err(axum::http::StatusCode::UNSUPPORTED_MEDIA_TYPE, "Send application/json.");
    }
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&body) else {
        return err(axum::http::StatusCode::BAD_REQUEST, "invalid JSON");
    };
    let conn = Arc::clone(&state.conn);
    let joined = tokio::task::spawn_blocking(move || {
        let conn = conn.lock().unwrap();
        let owner = v["owner"].as_str().unwrap_or("");
        if !matches!(rollup_core::connect::owner_role(&conn, owner), Some("pm" | "ppm")) {
            return Err(None);
        }
        run(&conn, &v).map_err(Some)
    })
    .await;
    match joined {
        Ok(Ok(body)) => Ok(Json(body)),
        Ok(Err(None)) => err(
            axum::http::StatusCode::FORBIDDEN,
            "Only the owner can edit the partner brief.",
        ),
        Ok(Err(Some(e))) => brief_refused(e),
        Err(_) => err(axum::http::StatusCode::INTERNAL_SERVER_ERROR, "task panicked"),
    }
}

fn brief_item_json(item: &briefs::BriefItem) -> serde_json::Value {
    json!({
        "item_id": item.item_id,
        "kind": item.kind,
        "position": item.position,
        "headline": item.headline,
        "what": item.what,
        "why": item.why,
        "action": item.action,
        "at_this_stage": item.at_this_stage,
    })
}

fn brief_json(brief: &briefs::Brief) -> serde_json::Value {
    json!({
        "id": brief.id,
        "week_ending": brief.week_ending,
        "created_at": brief.created_at,
        "sent_at": brief.sent_at,
        "org_unit": brief.org_unit,
        "partner_name": brief.partner_name,
        "prepared_by": brief.prepared_by,
        "bottom_line": brief.bottom_line,
        "items": brief.items.iter().map(brief_item_json).collect::<Vec<_>>(),
    })
}

fn read_brief_item(v: &serde_json::Value) -> Result<briefs::BriefItemInput, BriefError> {
    if !v.is_object() {
        return Err(BriefError::Invalid("item must be an object.".into()));
    }
    let text = |key: &str| -> Result<String, BriefError> {
        v[key]
            .as_str()
            .map(|s| s.to_string())
            .ok_or_else(|| BriefError::Invalid(format!("{key} is required.")))
    };
    let whole = |key: &str| -> Result<i64, BriefError> {
        v[key]
            .as_i64()
            .ok_or_else(|| BriefError::Invalid(format!("{key} must be a whole number.")))
    };
    Ok(briefs::BriefItemInput {
        item_id: text("item_id")?,
        kind: text("kind")?,
        position: whole("position")?,
        headline: text("headline")?,
        what: text("what")?,
        why: text("why")?,
        action: text("action")?,
        at_this_stage: whole("at_this_stage")?,
    })
}

async fn briefs_list(
    State(state): State<AppState>,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> ApiResult {
    let who = q.get("who").cloned();
    let conn = Arc::clone(&state.conn);
    let result = tokio::task::spawn_blocking(move || {
        let conn = conn.lock().unwrap();
        let role = who
            .as_deref()
            .and_then(|owner| rollup_core::connect::owner_role(&conn, owner));
        if role == Some("director") {
            return Err(None);
        }
        let include = matches!(role, Some("pm" | "ppm"));
        briefs::list(&conn, include)
            .map(|rows| json!({ "briefs": rows.iter().map(brief_json).collect::<Vec<_>>() }))
            .map_err(Some)
    })
    .await;
    match result {
        Ok(Ok(body)) => Ok(Json(body)),
        Ok(Err(None)) => err(
            axum::http::StatusCode::FORBIDDEN,
            "The Director reads leadership updates, not the partner brief.",
        ),
        Ok(Err(Some(e))) => brief_refused(e),
        Err(_) => err(axum::http::StatusCode::INTERNAL_SERVER_ERROR, "task panicked"),
    }
}

async fn briefs_next(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    body: String,
) -> ApiResult {
    with_brief_editor(state, headers, body, |conn, v| {
        let week = v["week_ending"].as_str().unwrap_or("");
        let id = briefs::next_from_last(conn, week, &store::utcnow())?;
        Ok(json!({"ok": true, "id": id}))
    })
    .await
}

async fn briefs_header(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    body: String,
) -> ApiResult {
    with_brief_editor(state, headers, body, |conn, v| {
        let text = |key: &str| {
            v[key]
                .as_str()
                .ok_or_else(|| BriefError::Invalid(format!("{key} is required.")))
        };
        briefs::set_header(
            conn,
            text("org_unit")?,
            text("partner_name")?,
            text("prepared_by")?,
            text("bottom_line")?,
        )?;
        Ok(json!({"ok": true}))
    })
    .await
}

async fn briefs_item(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    body: String,
) -> ApiResult {
    with_brief_editor(state, headers, body, |conn, v| {
        let Some(item) = v.get("item") else {
            return Err(BriefError::Invalid("item is required.".into()));
        };
        briefs::upsert_item(conn, &read_brief_item(item)?)?;
        Ok(json!({"ok": true}))
    })
    .await
}

async fn briefs_item_remove(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    body: String,
) -> ApiResult {
    with_brief_editor(state, headers, body, |conn, v| {
        let Some(item_id) = v["item_id"].as_str() else {
            return Err(BriefError::Invalid("item_id is required.".into()));
        };
        briefs::remove_item(conn, item_id)?;
        Ok(json!({"ok": true}))
    })
    .await
}

async fn briefs_item_move(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    body: String,
) -> ApiResult {
    with_brief_editor(state, headers, body, |conn, v| {
        let Some(item_id) = v["item_id"].as_str() else {
            return Err(BriefError::Invalid("item_id is required.".into()));
        };
        let Some(position) = v["position"].as_i64() else {
            return Err(BriefError::Invalid("position must be a whole number.".into()));
        };
        briefs::move_item(conn, item_id, position)?;
        Ok(json!({"ok": true}))
    })
    .await
}

async fn briefs_send(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    body: String,
) -> ApiResult {
    with_brief_editor(state, headers, body, |conn, _v| {
        briefs::send(conn, &store::utcnow())?;
        Ok(json!({"ok": true}))
    })
    .await
}

#[cfg(any(test, feature = "test-util"))]
fn fixture_settings(db_path: &std::path::Path) -> rollup_core::Settings {
    rollup_core::Settings {
        sync_mode: "fixture".into(),
        db_path: db_path.to_path_buf(),
        jira_base_url: String::new(),
        jira_email: String::new(),
        jira_token: String::new(),
        jira_boards: Vec::new(),
        github_token: String::new(),
        github_org: String::new(),
        github_repos: Vec::new(),
        tracker_url: rollup_core::config::DEFAULT_TRACKER_URL.into(),
        t_high: 0.55,
        t_low: 0.35,
        ai_publish: "review".into(),
    }
}

/// Serve the router on 127.0.0.1:0 until the process exits. Returns the port and write token.
/// The runtime thread stays up for the caller (a test in another crate).
#[cfg(any(test, feature = "test-util"))]
pub fn serve_temp(db_path: &std::path::Path) -> (u16, String) {
    store::init_db(db_path).expect("init db");
    let write_token = rollup_core::token::ensure(db_path).expect("write token");
    let state = AppState {
        conn: Arc::new(Mutex::new(store::connect(db_path).expect("open db"))),
        settings: Arc::new(fixture_settings(db_path)),
        write_token: write_token.clone(),
    };
    let (tx, rx) = std::sync::mpsc::sync_channel::<Result<u16, String>>(1);
    std::thread::spawn(move || {
        let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
            Ok(rt) => rt,
            Err(e) => {
                let _ = tx.send(Err(e.to_string()));
                return;
            }
        };
        rt.block_on(async move {
            let listener = match tokio::net::TcpListener::bind("127.0.0.1:0").await {
                Ok(listener) => listener,
                Err(e) => {
                    let _ = tx.send(Err(e.to_string()));
                    return;
                }
            };
            let port = listener.local_addr().unwrap().port();
            let router = app(state, port);
            let _ = tx.send(Ok(port));
            axum::serve(listener, router).await.unwrap();
        });
    });
    let port = match rx.recv() {
        Ok(Ok(port)) => port,
        Ok(Err(err)) => panic!("serve_temp: cannot start server on 127.0.0.1:0: {err}"),
        Err(_) => panic!("server thread exited before reporting a port"),
    };
    (port, write_token)
}

fn app(state: AppState, port: u16) -> Router {
    let gate = WriteGate { port, token: state.write_token.clone() };
    // only_with_token is a route_layer: it wraps only the API routes registered before it.
    // `public` is merged after it on purpose, so those three stay open.
    let public = Router::new()
        .route("/", get(index))
        .route("/signal/{*path}", get(signal_asset))
        .route("/health/freshness", get(freshness));
    Router::new()
        .route("/sync", post(sync_now))
        .route("/match", post(match_now))
        .route("/epics", get(epics))
        .route("/rollup", get(rollup))
        .route("/queue", get(queue))
        .route("/summary", get(summary))
        .route("/forecast", get(forecast))
        .route("/queue/{pr_id}", post(queue_action))
        .route("/workspace", get(workspace_now))
        .route("/attach", post(attach_now))
        .route("/map", post(map_now))
        .route("/projects/select", post(select_projects))
        .route(
            "/feedback",
            post(feedback_now).layer(axum::extract::DefaultBodyLimit::max(FEEDBACK_MAX_BODY)),
        )
        .route("/updates", get(updates_list))
        .route(
            "/updates/next",
            post(updates_next).layer(axum::extract::DefaultBodyLimit::max(UPDATES_MAX_BODY)),
        )
        .route(
            "/updates/item",
            post(updates_item).layer(axum::extract::DefaultBodyLimit::max(UPDATES_MAX_BODY)),
        )
        .route(
            "/updates/item/remove",
            post(updates_item_remove).layer(axum::extract::DefaultBodyLimit::max(UPDATES_MAX_BODY)),
        )
        .route(
            "/updates/item/move",
            post(updates_item_move).layer(axum::extract::DefaultBodyLimit::max(UPDATES_MAX_BODY)),
        )
        .route(
            "/updates/settings",
            post(updates_settings).layer(axum::extract::DefaultBodyLimit::max(UPDATES_MAX_BODY)),
        )
        .route(
            "/updates/send",
            post(updates_send).layer(axum::extract::DefaultBodyLimit::max(UPDATES_MAX_BODY)),
        )
        .route(
            "/updates/discard",
            post(updates_discard).layer(axum::extract::DefaultBodyLimit::max(UPDATES_MAX_BODY)),
        )
        .route(
            "/updates/correct",
            post(updates_correct).layer(axum::extract::DefaultBodyLimit::max(UPDATES_MAX_BODY)),
        )
        .route(
            "/ai/notes",
            get(ai_notes_list)
                .post(ai_notes_add)
                .layer(axum::extract::DefaultBodyLimit::max(UPDATES_MAX_BODY)),
        )
        .route(
            "/ai/notes/resolve",
            post(ai_notes_resolve).layer(axum::extract::DefaultBodyLimit::max(UPDATES_MAX_BODY)),
        )
        .route(
            "/ai/map",
            post(ai_map).layer(axum::extract::DefaultBodyLimit::max(UPDATES_MAX_BODY)),
        )
        .route(
            "/ai/unmap",
            post(ai_unmap).layer(axum::extract::DefaultBodyLimit::max(UPDATES_MAX_BODY)),
        )
        .route(
            "/ai/sources",
            get(ai_sources_list)
                .layer(axum::extract::DefaultBodyLimit::max(UPDATES_MAX_BODY)),
        )
        .route("/ai/brief/run", post(ai_brief_run))
        .route(
            "/ai/publish",
            post(ai_publish_now).layer(axum::extract::DefaultBodyLimit::max(UPDATES_MAX_BODY)),
        )
        .route(
            "/ai/run",
            post(ai_run_now).layer(axum::extract::DefaultBodyLimit::max(UPDATES_MAX_BODY)),
        )
        .route("/ai/runs", get(ai_runs_list))
        .route("/ai/status", get(ai_status))
        .route("/ai/changes", get(ai_changes_list))
        .route("/ai/flags", get(ai_flags_list))
        .route(
            "/ai/flags/resolve",
            post(ai_flags_resolve).layer(axum::extract::DefaultBodyLimit::max(UPDATES_MAX_BODY)),
        )
        .route("/briefs", get(briefs_list))
        .route(
            "/briefs/next",
            post(briefs_next).layer(axum::extract::DefaultBodyLimit::max(UPDATES_MAX_BODY)),
        )
        .route(
            "/briefs/header",
            post(briefs_header).layer(axum::extract::DefaultBodyLimit::max(UPDATES_MAX_BODY)),
        )
        .route(
            "/briefs/item",
            post(briefs_item).layer(axum::extract::DefaultBodyLimit::max(UPDATES_MAX_BODY)),
        )
        .route(
            "/briefs/item/remove",
            post(briefs_item_remove).layer(axum::extract::DefaultBodyLimit::max(UPDATES_MAX_BODY)),
        )
        .route(
            "/briefs/item/move",
            post(briefs_item_move).layer(axum::extract::DefaultBodyLimit::max(UPDATES_MAX_BODY)),
        )
        .route(
            "/briefs/send",
            post(briefs_send).layer(axum::extract::DefaultBodyLimit::max(UPDATES_MAX_BODY)),
        )
        .route_layer(axum::middleware::from_fn_with_state(gate, only_with_token))
        .merge(public)
        .layer(axum::middleware::from_fn_with_state(port, only_local_host))
        .with_state(state)
}

// The page is opened from a link that carries the token, so neither it nor its assets may send a Referer.
async fn index() -> impl IntoResponse {
    (
        [
            (axum::http::header::CONTENT_TYPE, "text/html; charset=utf-8"),
            (axum::http::header::CACHE_CONTROL, "no-store"),
            (axum::http::header::REFERRER_POLICY, "no-referrer"),
        ],
        include_str!("../../../ui/index.html"),
    )
}

async fn signal_asset(AxPath(path): AxPath<String>) -> axum::response::Response {
    let css = |body: &'static str| -> axum::response::Response {
        (
            [
                (axum::http::header::CONTENT_TYPE, "text/css; charset=utf-8"),
                (axum::http::header::CACHE_CONTROL, "no-store"),
                (axum::http::header::REFERRER_POLICY, "no-referrer"),
            ],
            body,
        )
            .into_response()
    };
    let font = |body: &'static [u8]| -> axum::response::Response {
        (
            [
                (axum::http::header::CONTENT_TYPE, "font/woff2"),
                (axum::http::header::CACHE_CONTROL, "no-store"),
                (axum::http::header::REFERRER_POLICY, "no-referrer"),
            ],
            body,
        )
            .into_response()
    };
    match path.as_str() {
        "fonts.css" => css(include_str!("../../../ui/signal/fonts.css")),
        "layout.css" => css(include_str!("../../../ui/signal/layout.css")),
        "signal.css" => css(include_str!("../../../ui/signal/signal.css")),
        "color.css" => css(include_str!("../../../ui/signal/color.css")),
        "consult.css" => css(include_str!("../../../ui/signal/consult.css")),
        "fonts/geist-mono-latin-wght-normal.woff2" => {
            font(include_bytes!(
                "../../../ui/signal/fonts/geist-mono-latin-wght-normal.woff2"
            ))
        }
        "fonts/schibsted-grotesk-latin-wght-normal.woff2" => {
            font(include_bytes!(
                "../../../ui/signal/fonts/schibsted-grotesk-latin-wght-normal.woff2"
            ))
        }
        _ => (
            axum::http::StatusCode::NOT_FOUND,
            [
                (axum::http::header::CACHE_CONTROL, "no-store"),
                (axum::http::header::REFERRER_POLICY, "no-referrer"),
            ],
        )
            .into_response(),
    }
}

fn fail_line(msg: &str) -> ! {
    eprintln!("ai-run: failed: {msg}");
    std::process::exit(1);
}

fn ai_text(err: &AiError) -> &str {
    match err {
        AiError::Invalid(msg) | AiError::Db(msg) => msg,
    }
}

/// Local weekday (1 = Mon .. 7 = Sun) and minute of day in America/New_York.
/// `date` is the clock. No timezone crate.
fn ny_clock() -> Result<(u8, u16), String> {
    let out = std::process::Command::new("date")
        .args(["+%u %H %M"])
        .env("TZ", "America/New_York")
        .output()
        .map_err(|err| format!("cannot read America/New_York clock: {err}"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        return Err(format!("cannot read America/New_York clock: {}", err.trim()));
    }
    let text = String::from_utf8(out.stdout)
        .map_err(|err| format!("cannot read America/New_York clock: {err}"))?;
    let mut parts = text.split_whitespace();
    let parse = |part: Option<&str>| part.and_then(|s| s.parse::<u16>().ok());
    let (Some(weekday), Some(hour), Some(minute)) =
        (parse(parts.next()), parse(parts.next()), parse(parts.next()))
    else {
        return Err(format!("cannot read America/New_York clock: {}", text.trim()));
    };
    if !(1..=7).contains(&weekday) || hour > 23 || minute > 59 {
        return Err(format!("cannot read America/New_York clock: {}", text.trim()));
    }
    Ok((weekday as u8, hour * 60 + minute))
}

/// `rollup-api ai-run [--if-due]`. One pass, then exit. Does not listen.
fn ai_run() -> ! {
    let if_due = std::env::args().skip(2).any(|arg| arg == "--if-due");
    let settings = rollup_core::Settings::load();
    store::init_db(&settings.db_path).unwrap_or_else(|err| fail_line(&format!("init db: {err}")));
    let conn = store::connect(&settings.db_path)
        .unwrap_or_else(|err| fail_line(&format!("open db: {err}")));
    let now = store::utcnow();
    if if_due {
        let (weekday, minute_of_day) = ny_clock().unwrap_or_else(|err| fail_line(&err));
        match rollup_core::airun::is_due(&conn, weekday, minute_of_day, &now) {
            Ok(Some(reason)) => {
                println!("ai-run: skipped ({reason})");
                std::process::exit(0);
            }
            Ok(None) => {}
            Err(err) => fail_line(ai_text(&err)),
        }
    } else if std::env::var("ROLLUP_AI").ok().as_deref() == Some("off") {
        // No window check on a manual run. The off switch still wins.
        println!("ai-run: skipped (ROLLUP_AI is off)");
        std::process::exit(0);
    }
    // ponytail: a real model behind the `Model` trait would replace FakeModel here
    match rollup_core::airun::run_once(&conn, &rollup_core::draft::FakeModel, AI_MODEL, &now) {
        Ok(run) => {
            println!(
                "ai-run: {} facts={} accepted={} rejected={}",
                run.outcome, run.facts, run.accepted, run.rejected
            );
            std::process::exit(0);
        }
        Err(err) => fail_line(ai_text(&err)),
    }
}

fn xml_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(c),
        }
    }
    out
}

/// `rollup-api schedule print --dir <DIR>`.
/// Prints a launchd plist for `ai-run --if-due`. Writes nothing. Does not call launchctl.
fn schedule_print() -> ! {
    let args: Vec<String> = std::env::args().skip(2).collect();
    let dir = match args.as_slice() {
        [cmd, flag, dir] if cmd == "print" && flag == "--dir" => dir.as_str(),
        _ => {
            eprintln!("rollup-api: usage: rollup-api schedule print --dir <DIR>");
            std::process::exit(2);
        }
    };
    let path = std::path::Path::new(dir);
    if !path.is_absolute() {
        eprintln!("rollup-api: --dir must be an absolute path, got {dir}");
        std::process::exit(2);
    }
    if !path.is_dir() {
        eprintln!("rollup-api: --dir is not an existing directory: {dir}");
        std::process::exit(2);
    }
    let bin = std::env::current_exe().and_then(|p| p.canonicalize()).unwrap_or_else(|err| {
        eprintln!("rollup-api: cannot resolve the running executable: {err}");
        std::process::exit(2);
    });
    let Some(bin) = bin.to_str() else {
        eprintln!("rollup-api: executable path is not valid utf-8");
        std::process::exit(2);
    };
    let plist = include_str!("../../../deploy/launchd/tallimo-ai-run.plist.template")
        .replace("<BIN>", &xml_escape(bin))
        .replace("<DIR>", &xml_escape(dir));
    print!("{plist}");
    let _ = std::io::Write::flush(&mut std::io::stdout());
    std::process::exit(0);
}

/// The port to listen on: TALLIMO_PORT, or 8787 when it is unset or empty.
/// Anything else that is not a port from 1 to 65535 is an error.
fn port_from(raw: Option<&std::ffi::OsStr>) -> Result<u16, String> {
    let Some(raw) = raw.filter(|v| !v.is_empty()) else {
        return Ok(8787);
    };
    match raw.to_str().map(|v| v.trim().parse::<u16>()) {
        Some(Ok(port)) if port != 0 => Ok(port),
        _ => Err(format!(
            "TALLIMO_PORT must be a port number from 1 to 65535, got {raw:?}. Unset it to use 8787."
        )),
    }
}

fn run_eval() -> Result<(), String> {
    use rollup_core::draft::FakeModel;
    let args: Vec<String> = std::env::args().skip(1).collect();
    let dir = match args.as_slice() {
        [cmd, flag, dir] if cmd == "eval" && flag == "--cases" => dir.as_str(),
        _ => return Err("usage: rollup-api eval --cases <dir>".into()),
    };
    let cases = rollup_core::eval::load_cases(std::path::Path::new(dir))?;
    let scored = rollup_core::eval::score(&cases, &FakeModel)?;
    let text = serde_json::to_string(&scored.to_value()).map_err(|err| err.to_string())?;
    println!("{text}");
    Ok(())
}

/// The link `open` hands to the browser. The token sits in the fragment,
/// where a server log never sees it.
fn access_url(port: u16, token: &str) -> String {
    format!("http://127.0.0.1:{port}/#t={token}")
}

/// The pure decision of `open` from the token file's raw contents: the trimmed
/// token, or the line that names the dead end when the file is missing or empty.
fn open_decision(raw: Option<&str>) -> Result<String, &'static str> {
    match raw.map(str::trim).filter(|token| !token.is_empty()) {
        Some(token) => Ok(token.to_string()),
        None => Err("Tallimo hasn't been started in this folder yet. Start it first: ./rollup-api"),
    }
}

/// `rollup-api open`. Reads the token the server minted, checks that the server
/// answers, then opens the link. Reads only: it never mints or rewrites the
/// token file, and it never prints the token.
fn run_open() -> ! {
    // The same settings the server reads, so `open` finds the same folder.
    let port = port_from(std::env::var_os("TALLIMO_PORT").as_deref()).unwrap_or_else(|e| {
        eprintln!("rollup-api: {e}");
        std::process::exit(2);
    });
    let settings = rollup_core::Settings::load();
    let token_file = rollup_core::token::path_for(&settings.db_path);
    let token = open_decision(std::fs::read_to_string(&token_file).ok().as_deref()).unwrap_or_else(|msg| {
        eprintln!("{msg}");
        std::process::exit(2);
    });
    let addr: std::net::SocketAddr = format!("127.0.0.1:{port}")
        .parse()
        .expect("127.0.0.1 plus a valid port is a socket address");
    // A plain connect, no HTTP client: enough to know the server is up.
    if std::net::TcpStream::connect_timeout(&addr, std::time::Duration::from_secs(1)).is_err() {
        eprintln!("Tallimo isn't running. Start it first: ./rollup-api");
        std::process::exit(1);
    }
    let url = access_url(port, &token);
    println!("Opening Tallimo in your browser.");
    // ponytail: macOS `open` only, because this repo's desks are Macs; add xdg-open for Linux desks
    #[cfg(target_os = "macos")]
    {
        let opened = std::process::Command::new("open").arg(&url).status().map(|s| s.success()).unwrap_or(false);
        if !opened {
            eprintln!("Couldn't open your browser. Open the link Tallimo printed when it started.");
            std::process::exit(1);
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = url; // Print only. The URL holds the token, so it is never printed.
    }
    std::process::exit(0);
}

#[tokio::main]
pub async fn main() {
    if std::env::args().nth(1).as_deref() == Some("ai-run") {
        ai_run();
    }
    if std::env::args().nth(1).as_deref() == Some("schedule") {
        schedule_print();
    }
    if std::env::args().nth(1).as_deref() == Some("eval") {
        if let Err(err) = run_eval() {
            eprintln!("rollup-api eval: {err}");
            std::process::exit(1);
        }
        return;
    }
    if std::env::args().nth(1).as_deref() == Some("open") {
        run_open();
    }
    // Read the port before touching the database, so a bad value changes nothing.
    let port = port_from(std::env::var_os("TALLIMO_PORT").as_deref()).unwrap_or_else(|e| {
        eprintln!("rollup-api: {e}");
        std::process::exit(2);
    });
    let settings = rollup_core::Settings::load();
    // Before the database and the port: live mode with no boards must not start.
    settings.live_ready().unwrap_or_else(|e| {
        eprintln!("{e}");
        std::process::exit(2);
    });
    let settings = Arc::new(settings);
    store::init_db(&settings.db_path).expect("init db");
    let write_token = rollup_core::token::ensure(&settings.db_path).unwrap_or_else(|e| {
        eprintln!("rollup-api: cannot prepare the write token: {e}");
        std::process::exit(1);
    });
    let conn = Arc::new(Mutex::new(store::connect(&settings.db_path).expect("open db")));
    let state = AppState { conn, settings, write_token };

    let addr = format!("127.0.0.1:{port}");
    let listener = tokio::net::TcpListener::bind(&addr).await.unwrap_or_else(|e| {
        eprintln!("rollup-api: cannot listen on {addr}: {e}. Set TALLIMO_PORT to a free port.");
        std::process::exit(1);
    });
    println!("rollup-api listening on http://{addr}");
    // The link carries the token, so it goes only to a terminal, never into a log file.
    // It is printed, never opened: nothing here starts a browser.
    use std::io::Write as _;
    if std::io::IsTerminal::is_terminal(&std::io::stdout()) {
        let _ = writeln!(std::io::stdout(), "Tallimo is running. Open this link in your browser (it is printed, not opened): http://{addr}/#t={}", state.write_token);
        let _ = writeln!(std::io::stdout(), "Next time, run ./rollup-api open to open it again.");
    } else {
        let token_file = rollup_core::token::path_for(&state.settings.db_path);
        let _ = writeln!(std::io::stdout(), "Tallimo is running. Open http://{addr}/#t= followed by the token in {} (the link is printed only to a terminal).", token_file.display());
    }
    axum::serve(listener, app(state, port)).await.unwrap();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn json_headers() -> axum::http::HeaderMap {
        let mut headers = axum::http::HeaderMap::new();
        headers.insert(
            axum::http::header::CONTENT_TYPE,
            "application/json".parse().unwrap(),
        );
        headers
    }

    fn status_of(r: ApiResult) -> axum::http::StatusCode {
        match r {
            Ok(_) => axum::http::StatusCode::OK,
            Err((status, _)) => status,
        }
    }

    fn count(conn: &store::Conn, table: &str) -> i64 {
        let sql = match table {
            "attachments" | "repos" | "person_projects" | "repo_epic_map" => {
                format!("SELECT COUNT(*) FROM {table}")
            }
            _ => panic!("count: unknown table {table}"),
        };
        conn.query_row(&sql, [], |r| r.get(0)).unwrap()
    }

    fn counts(conn: &store::Conn) -> (i64, i64, i64, i64) {
        (
            count(conn, "attachments"),
            count(conn, "repos"),
            count(conn, "person_projects"),
            count(conn, "repo_epic_map"),
        )
    }

    fn cleanup(path: &std::path::Path) {
        for suffix in ["", "-wal", "-shm", ".token"] {
            let mut p = path.as_os_str().to_owned();
            p.push(suffix);
            let _ = std::fs::remove_file(std::path::PathBuf::from(p));
        }
    }

    struct TempDb(std::path::PathBuf);

    impl Drop for TempDb {
        fn drop(&mut self) {
            cleanup(&self.0);
        }
    }

    fn state() -> (AppState, TempDb) {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static N: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "rollup_api_{}_{}.db",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        cleanup(&path);
        store::init_db(&path).unwrap();
        let conn = store::connect(&path).unwrap();
        for (id, login, name, role) in [
            ("nora@firm.example", Some("nora"), "Nora", "dev"),
            ("mina@firm.example", None, "Mina", "pm"),
            ("rosa@firm.example", Some("rosa"), "Rosa", "tl"),
            ("iris@firm.example", None, "Iris", "ppm"),
        ] {
            conn.execute(
                "INSERT INTO identity_map (jira_account_id, github_login, display_name, confirmed, role)
                 VALUES (?1, ?2, ?3, 1, ?4)",
                rusqlite::params![id, login, name, role],
            )
            .unwrap();
        }
        for (key, board, summary) in [
            ("PAY-100", "PAY", "Payments"),
            ("AUD-200", "AUD", "Audit"),
        ] {
            conn.execute(
                "INSERT INTO epics (epic_key, board, summary) VALUES (?1, ?2, ?3)",
                rusqlite::params![key, board, summary],
            )
            .unwrap();
        }
        let state = AppState {
            conn: Arc::new(Mutex::new(conn)),
            // Fixture settings, built here so a test never reads .env or tokens.
            settings: Arc::new(fixture_settings(&path)),
            write_token: rollup_core::token::ensure(&path).unwrap(),
        };
        (state, TempDb(path))
    }

    async fn attach_status(state: &AppState, body: serde_json::Value) -> axum::http::StatusCode {
        status_of(attach_now(State(state.clone()), json_headers(), body.to_string()).await)
    }

    async fn select_status(state: &AppState, body: serde_json::Value) -> axum::http::StatusCode {
        status_of(select_projects(State(state.clone()), json_headers(), body.to_string()).await)
    }

    async fn map_status(state: &AppState, body: serde_json::Value) -> axum::http::StatusCode {
        status_of(map_now(State(state.clone()), json_headers(), body.to_string()).await)
    }

    fn snapshot(state: &AppState) -> (i64, i64, i64, i64) {
        counts(&state.conn.lock().unwrap())
    }

    /// Serve the real router on a free port and send one raw HTTP request with
    /// the Host line given. Returns the status code.
    async fn raw_status(port: u16, request: impl AsRef<[u8]>) -> u16 {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        stream.write_all(request.as_ref()).await.unwrap();
        let mut buf = Vec::new();
        stream.read_to_end(&mut buf).await.unwrap();
        let head = String::from_utf8_lossy(&buf);
        head.split_whitespace().nth(1).unwrap().parse().unwrap()
    }

    async fn serve(state: &AppState) -> u16 {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let router = app(state.clone(), port);
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        port
    }

    /// An empty `token` omits `X-Tallimo-Token`. Gated routes need the real token.
    fn get_req(path: &str, host: &str, token: &str) -> String {
        let auth = if token.is_empty() {
            String::new()
        } else {
            format!("X-Tallimo-Token: {token}\r\n")
        };
        format!("GET {path} HTTP/1.1\r\nHost: {host}\r\n{auth}Connection: close\r\n\r\n")
    }

    fn post_bare(path: &str, host: &str, body: &serde_json::Value) -> String {
        let body = body.to_string();
        format!(
            "POST {path} HTTP/1.1\r\nHost: {host}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    fn post_req(path: &str, host: &str, token: &str, body: &serde_json::Value) -> String {
        let body = body.to_string();
        format!(
            "POST {path} HTTP/1.1\r\nHost: {host}\r\nX-Tallimo-Token: {token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    #[test]
    fn tallimo_port_defaults_to_8787_and_refuses_junk() {
        use std::ffi::OsStr;
        assert_eq!(port_from(None), Ok(8787));
        assert_eq!(port_from(Some(OsStr::new(""))), Ok(8787));
        assert_eq!(port_from(Some(OsStr::new("8788"))), Ok(8788));
        assert_eq!(port_from(Some(OsStr::new(" 9000 "))), Ok(9000));
        for bad in ["0", "65536", "-1", "abc", "8787x", "80 80"] {
            let e = port_from(Some(OsStr::new(bad))).unwrap_err();
            assert!(e.contains("TALLIMO_PORT must be a port number from 1 to 65535"), "{bad}: {e}");
        }
    }

    // `open` is tested where it is pure: the URL, and the missing-token dead end.
    // No test spawns a browser.
    #[test]
    fn access_url_carries_the_token_in_the_fragment() {
        let token = "ab".repeat(32);
        assert_eq!(access_url(8787, &token), format!("http://127.0.0.1:8787/#t={token}"));
        assert_eq!(access_url(9000, &token), format!("http://127.0.0.1:9000/#t={token}"));
    }

    #[test]
    fn open_decision_names_the_dead_end_for_a_missing_or_empty_token() {
        let msg = open_decision(None).unwrap_err();
        assert_eq!(
            msg,
            "Tallimo hasn't been started in this folder yet. Start it first: ./rollup-api"
        );
        assert_eq!(open_decision(Some("")).unwrap_err(), msg);
        assert_eq!(open_decision(Some("  \n")).unwrap_err(), msg);
        let token = "cd".repeat(32);
        assert_eq!(open_decision(Some(&format!("  {token}\n"))).unwrap(), token);
    }

    #[tokio::test]
    async fn the_host_check_uses_the_port_the_server_listens_on() {
        let (state, _db) = state();
        let port = serve(&state).await;
        assert_ne!(port, 8787);
        assert_eq!(raw_status(port, get_req("/workspace", &format!("127.0.0.1:{port}"), &state.write_token)).await, 200);
        assert_eq!(raw_status(port, get_req("/workspace", "127.0.0.1:8787", "")).await, 421);
    }

    fn post_from(path: &str, host: &str, origin: &str, body: &serde_json::Value) -> String {
        post_bare(path, host, body).replacen("\r\n", &format!("\r\nOrigin: {origin}\r\n"), 1)
    }

    #[test]
    fn only_this_server_is_a_good_origin() {
        for good in ["http://127.0.0.1:8787", "http://localhost:8787", "http://[::1]:8787", "HTTP://LOCALHOST:8787"] {
            assert!(origin_allowed(good, 8787), "{good}");
        }
        for bad in ["https://attacker.example", "http://attacker.example:8787", "null", "http://127.0.0.1:9999", "https://127.0.0.1:8787", "http://localhost.attacker.example:8787", "http://localhost", ""] {
            assert!(!origin_allowed(bad, 8787), "{bad}");
        }
        // On port 80 the browser sends the Origin without a port.
        assert!(origin_allowed("http://localhost", 80) && origin_allowed("http://localhost:80", 80));
        assert!(!origin_allowed("http://localhost", 8788));
    }

    #[tokio::test]
    async fn a_write_from_another_origin_is_refused() {
        let (state, _db) = state();
        let port = serve(&state).await;
        let host = format!("127.0.0.1:{port}");
        let claim = json!({"kind": "github_repo", "key": "payments-core", "owner": "nora@firm.example"});
        let before = snapshot(&state);

        // A page on another site posts to this server: refused, nothing written.
        for evil in ["https://attacker.example", "null", "http://127.0.0.1:9"] {
            for path in ["/attach", "/sync", "/match", "/map"] {
                assert_eq!(raw_status(port, post_from(path, &host, evil, &claim)).await, 403, "{evil} {path}");
            }
        }
        assert_eq!(snapshot(&state), before);

        // A foreign Origin refuses a read too. The header only adds a refusal.
        let get = get_req("/workspace", &host, "").replacen("\r\n", "\r\nOrigin: https://attacker.example\r\n", 1);
        assert_eq!(raw_status(port, get).await, 403);

        // No Origin and no token.
        let (status, body) = raw_parts(port, post_bare("/attach", &host, &claim)).await;
        assert_eq!(status, 401);
        assert_eq!(error_text(&body), "Missing or wrong Tallimo token.");
        assert_eq!(snapshot(&state), before);

        // Wrong token, including one the same length as the real token.
        let wrong = "0".repeat(state.write_token.len());
        let (status, body) = raw_parts(port, post_req("/attach", &host, &wrong, &claim)).await;
        assert_eq!(status, 401);
        assert_eq!(error_text(&body), "Missing or wrong Tallimo token.");
        assert_eq!(snapshot(&state), before);

        // This server's own page, with no token, is still refused.
        let own = format!("http://localhost:{port}");
        let (status, body) = raw_parts(port, post_from("/attach", &host, &own, &claim)).await;
        assert_eq!(status, 401);
        assert_eq!(error_text(&body), "Missing or wrong Tallimo token.");
        assert_eq!(count(&state.conn.lock().unwrap(), "attachments"), 0);

        // No Origin, with the right token, may write.
        let other = json!({"kind": "github_repo", "key": "ledger-api", "owner": "nora@firm.example"});
        assert_eq!(raw_status(port, post_req("/attach", &host, &state.write_token, &other)).await, 200);
        assert_eq!(count(&state.conn.lock().unwrap(), "attachments"), 1);
    }

    async fn token_denied(port: u16, request: impl AsRef<[u8]>, path: &str) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        stream.write_all(request.as_ref()).await.unwrap();
        let mut buf = Vec::new();
        stream.read_to_end(&mut buf).await.unwrap();
        let text = String::from_utf8_lossy(&buf);
        let status: u16 = text.split_whitespace().nth(1).unwrap().parse().unwrap();
        assert_eq!(status, 401, "{path}");
        let (head, body) = text.split_once("\r\n\r\n").unwrap();
        assert!(
            head.to_ascii_lowercase().contains("cache-control: no-store"),
            "{path} {head}"
        );
        assert_eq!(error_text(body), "Missing or wrong Tallimo token.");
    }

    async fn origin_denied(port: u16, request: impl AsRef<[u8]>, path: &str) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        stream.write_all(request.as_ref()).await.unwrap();
        let mut buf = Vec::new();
        stream.read_to_end(&mut buf).await.unwrap();
        let text = String::from_utf8_lossy(&buf);
        let status: u16 = text.split_whitespace().nth(1).unwrap().parse().unwrap();
        assert_eq!(status, 403, "{path}");
        let (head, body) = text.split_once("\r\n\r\n").unwrap();
        assert!(
            head.to_ascii_lowercase().contains("cache-control: no-store"),
            "{path} {head}"
        );
        assert_eq!(
            error_text(body),
            "This request came from a page on another site, so it was refused. Use the page at 127.0.0.1, localhost or [::1] on this port. Nothing was read or written."
        );
    }

    #[tokio::test]
    async fn the_local_token_is_the_only_way_in() {
        let (state, _db) = state();
        let port = serve(&state).await;
        let host = format!("127.0.0.1:{port}");
        let token = state.write_token.clone();
        let own = format!("http://127.0.0.1:{port}");
        let reads = [
            "/updates?who=mina@firm.example",
            "/briefs?who=mina@firm.example",
            "/ai/notes?who=mina@firm.example",
            "/ai/sources?who=mina@firm.example",
            "/ai/runs?who=mina@firm.example",
            "/ai/changes?who=mina@firm.example&since=2026-10-19T08:00:00Z",
            "/ai/flags?who=mina@firm.example",
            "/rollup",
            "/epics",
            "/queue",
            "/summary",
            "/forecast",
            "/workspace",
        ];
        for path in reads {
            token_denied(port, get_req(path, &host, ""), path).await;
            let same = get_req(path, &host, "").replacen("\r\n", "\r\nSec-Fetch-Site: same-origin\r\n", 1);
            token_denied(port, same, path).await;
            let local = get_req(path, &host, "").replacen("\r\n", &format!("\r\nOrigin: {own}\r\n"), 1);
            token_denied(port, local, path).await;
            let foreign = get_req(path, &host, &token).replacen("\r\n", "\r\nOrigin: https://attacker.example\r\n", 1);
            origin_denied(port, foreign, path).await;
            let cross = get_req(path, &host, &token).replacen("\r\n", "\r\nSec-Fetch-Site: cross-site\r\n", 1);
            origin_denied(port, cross, path).await;
            let status = raw_status(port, get_req(path, &host, &token)).await;
            assert!(status != 401 && status != 403, "{path} {status}");
        }
        let body = json!({});
        for path in ["/sync", "/updates/next", "/ai/notes", "/ai/brief/run", "/ai/flags/resolve"] {
            token_denied(port, post_bare(path, &host, &body), path).await;
            let same = post_bare(path, &host, &body).replacen("\r\n", "\r\nSec-Fetch-Site: same-origin\r\n", 1);
            token_denied(port, same, path).await;
            let local = post_bare(path, &host, &body).replacen("\r\n", &format!("\r\nOrigin: {own}\r\n"), 1);
            token_denied(port, local, path).await;
            let foreign = post_req(path, &host, &token, &body).replacen("\r\n", "\r\nOrigin: https://attacker.example\r\n", 1);
            origin_denied(port, foreign, path).await;
            let cross = post_req(path, &host, &token, &body).replacen("\r\n", "\r\nSec-Fetch-Site: cross-site\r\n", 1);
            origin_denied(port, cross, path).await;
            // Past the gate, the handler may still refuse (for example a 403 for a non-owner), but never with the gate's 401.
            let (status, text) = raw_parts(port, post_req(path, &host, &token, &body)).await;
            assert!(status != 401, "{path} {status}");
            let refused = serde_json::from_str::<serde_json::Value>(&text).ok().and_then(|v| v["error"].as_str().map(str::to_string));
            assert_ne!(refused.as_deref(), Some("Missing or wrong Tallimo token."), "{path}");
        }
        assert_eq!(raw_status(port, get_req("/", &host, "")).await, 200);
        assert_eq!(raw_status(port, get_req("/health/freshness", &host, "")).await, 200);
        assert_eq!(raw_status(port, get_req("/signal/fonts.css", &host, "")).await, 200);
        assert_eq!(raw_status(port, get_req("/signal/missing.css", &host, "")).await, 404);
    }

    #[tokio::test]
    async fn a_brief_run_returns_counts_only() {
        let (state, _db) = state();
        let port = serve(&state).await;
        let host = format!("127.0.0.1:{port}");
        let (status, text) = raw_parts(port, post_req("/ai/brief/run", &host, &state.write_token, &json!({}))).await;
        assert_eq!(status, 200, "{text}");
        let body: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(body["outcome"].as_str(), Some("unchanged"));
        assert_eq!(body["facts"].as_u64(), Some(0));
        assert_eq!(body["accepted"].as_u64(), Some(0));
        assert_eq!(body["rejected"].as_u64(), Some(0));
        assert!(body.get("detail").is_none());
        let mode: String = state
            .conn
            .lock()
            .unwrap()
            .query_row("SELECT mode FROM ai_runs", [], |row| row.get(0))
            .unwrap();
        assert_eq!(mode, "brief");
    }

    #[tokio::test]
    async fn the_page_and_its_assets_send_no_referrer() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let (state, _db) = state();
        let port = serve(&state).await;
        let host = format!("127.0.0.1:{port}");
        // Public routes, so no token. The page is opened from a link that carries the token.
        for path in ["/", "/signal/fonts.css", "/signal/fonts/geist-mono-latin-wght-normal.woff2", "/signal/missing.css"] {
            let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port)).await.unwrap();
            stream.write_all(get_req(path, &host, "").as_bytes()).await.unwrap();
            let mut buf = Vec::new();
            stream.read_to_end(&mut buf).await.unwrap();
            let text = String::from_utf8_lossy(&buf);
            let (head, body) = text.split_once("\r\n\r\n").unwrap();
            assert!(head.to_ascii_lowercase().contains("referrer-policy: no-referrer"), "{path} {head}");
            if path == "/" {
                assert!(body.contains(r#"<meta name="referrer" content="no-referrer">"#), "the page has no referrer meta");
            }
        }
    }

    #[test]
    fn an_empty_presented_token_never_matches_even_if_the_server_token_is_empty() {
        // token_eq is the gate in only_with_token. False is the 401.
        assert!(!token_eq("", ""));
        assert!(!token_eq("", "aa"));
        assert!(!token_eq("aa", ""));
        assert!(token_eq("aa", "aa"));
        assert!(!token_eq("aa", "ab"));
    }

    #[test]
    fn the_write_token_file_is_created_with_mode_0600() {
        use std::os::unix::fs::PermissionsExt;
        let (state, db) = state();
        let path = rollup_core::token::path_for(&db.0);
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text.trim(), state.write_token);
        assert_eq!(state.write_token.len(), 64);
        assert!(state.write_token.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
        assert_eq!(rollup_core::token::ensure(&db.0).unwrap(), state.write_token);
    }

    #[test]
    fn only_this_server_is_a_good_host() {
        for good in [
            "127.0.0.1:8787",
            "localhost:8787",
            "[::1]:8787",
            "LOCALHOST:8787",
            " 127.0.0.1:8787 ",
            "127.0.0.1",
            "localhost",
            "[::1]",
            "LocalHost",
        ] {
            assert!(host_allowed(good, 8787), "{good}");
        }
        for bad in [
            "attacker.example",
            "attacker.example:8787",
            "::1",
            "localhost.",
            "localhost.:8787",
            "127.0.0.1:",
            "127.0.0.1:9999",
            "localhost:9999",
            "127.0.0.1.attacker.example:8787",
            "localhost.attacker.example:8787",
            "0.0.0.0:8787",
            "",
        ] {
            assert!(!host_allowed(bad, 8787), "{bad}");
        }
    }

    #[tokio::test]
    async fn a_good_host_is_served_and_any_other_host_is_refused() {
        let (state, _db) = state();
        let port = serve(&state).await;

        let token = &state.write_token;
        for good in [format!("127.0.0.1:{port}"), format!("localhost:{port}"), format!("[::1]:{port}")] {
            assert_eq!(raw_status(port, get_req("/workspace", &good, token)).await, 200, "{good}");
        }
        assert_eq!(raw_status(port, get_req("/", &format!("localhost:{port}"), "")).await, 200);
        // The three local names with no port at all.
        for good in ["127.0.0.1", "localhost", "[::1]"] {
            assert_eq!(raw_status(port, get_req("/workspace", good, token)).await, 200, "{good}");
        }

        // DNS rebinding: attacker.example now resolves to 127.0.0.1, so the
        // browser sends its own name as the Host, with or without the port.
        for evil in ["attacker.example".to_string(), format!("attacker.example:{port}"), "127.0.0.1:9".to_string()] {
            assert_eq!(raw_status(port, get_req("/workspace", &evil, "")).await, 421, "{evil}");
            assert_eq!(raw_status(port, get_req("/", &evil, "")).await, 421, "{evil}");
        }
        // HTTP/1.0 with no Host line at all.
        let no_host = "GET /workspace HTTP/1.0\r\n\r\n".to_string();
        assert_eq!(raw_status(port, no_host).await, 421);
    }

    #[tokio::test]
    async fn a_host_that_is_not_text_is_refused_and_the_uri_authority_is_never_used() {
        let (state, _db) = state();
        let port = serve(&state).await;
        let absolute = format!("GET http://127.0.0.1:{port}/workspace HTTP/1.1\r\n");

        // A Host header that is not valid text, with a good absolute-form URI.
        let mut junk = absolute.clone().into_bytes();
        junk.extend_from_slice(b"Host: \xff\xfe\r\nConnection: close\r\n\r\n");
        assert_eq!(raw_status(port, junk).await, 421);

        // No Host header at all: the URI authority does not stand in for it.
        assert_eq!(raw_status(port, format!("{absolute}Connection: close\r\n\r\n")).await, 421);

        // Two Host headers, even if one of them is good.
        let two = format!("{absolute}Host: 127.0.0.1:{port}\r\nHost: attacker.example\r\nConnection: close\r\n\r\n");
        assert_eq!(raw_status(port, two).await, 421);

        // The same absolute-form request with a good Host header is served.
        let good = format!(
            "{absolute}Host: 127.0.0.1:{port}\r\nX-Tallimo-Token: {}\r\nConnection: close\r\n\r\n",
            state.write_token
        );
        assert_eq!(raw_status(port, good).await, 200);
    }

    #[tokio::test]
    async fn a_rebinding_host_cannot_write() {
        let (state, _db) = state();
        let port = serve(&state).await;
        let before = snapshot(&state);
        let claim = json!({"kind": "github_repo", "key": "payments-core", "owner": "nora@firm.example"});

        for evil in ["attacker.example".to_string(), format!("attacker.example:{port}")] {
            assert_eq!(raw_status(port, post_req("/attach", &evil, &state.write_token, &claim)).await, 421, "{evil}");
            assert_eq!(raw_status(port, post_req("/sync", &evil, &state.write_token, &json!({}))).await, 421, "{evil}");
        }
        assert_eq!(snapshot(&state), before);

        // The same write on this server's own Host goes through.
        let good = format!("127.0.0.1:{port}");
        assert_eq!(raw_status(port, post_req("/attach", &good, &state.write_token, &claim)).await, 200);
        assert_eq!(count(&state.conn.lock().unwrap(), "attachments"), 1);
    }

    #[tokio::test]
    async fn dev_may_not_attach_a_board() {
        let (state, _db) = state();
        let before = snapshot(&state);

        for key in ["PAY", "PAY, AUD"] {
            let status = attach_status(
                &state,
                json!({"kind": "jira_board", "key": key, "owner": "nora@firm.example"}),
            )
            .await;
            assert_eq!(status, axum::http::StatusCode::FORBIDDEN);
        }
        assert_eq!(snapshot(&state), before);

        let status = attach_status(
            &state,
            json!({"kind": "github_repo", "key": "payments-core", "owner": "nora@firm.example"}),
        )
        .await;
        assert_eq!(status, axum::http::StatusCode::OK);
        assert_eq!(count(&state.conn.lock().unwrap(), "attachments"), 1);
    }

    #[tokio::test]
    async fn pm_may_not_attach_a_repo() {
        let (state, _db) = state();
        let before = snapshot(&state);

        for key in ["payments-core", "payments-core, audit-trail"] {
            let status = attach_status(
                &state,
                json!({"kind": "github_repo", "key": key, "owner": "mina@firm.example"}),
            )
            .await;
            assert_eq!(status, axum::http::StatusCode::FORBIDDEN);
        }
        assert_eq!(snapshot(&state), before);
        assert_eq!(count(&state.conn.lock().unwrap(), "repos"), 0);

        let status = attach_status(
            &state,
            json!({"kind": "jira_board", "key": "PAY", "owner": "mina@firm.example"}),
        )
        .await;
        assert_eq!(status, axum::http::StatusCode::OK);
    }

    #[tokio::test]
    async fn tl_may_not_attach_a_board() {
        let (state, _db) = state();
        let before = snapshot(&state);

        for key in ["PAY", "PAY, AUD"] {
            let status = attach_status(
                &state,
                json!({"kind": "jira_board", "key": key, "owner": "rosa@firm.example"}),
            )
            .await;
            assert_eq!(status, axum::http::StatusCode::FORBIDDEN);
        }
        assert_eq!(snapshot(&state), before);
    }

    #[tokio::test]
    async fn ppm_may_not_attach_a_repo() {
        let (state, _db) = state();
        let before = snapshot(&state);

        for key in ["payments-core", "payments-core, audit-trail"] {
            let status = attach_status(
                &state,
                json!({"kind": "github_repo", "key": key, "owner": "iris@firm.example"}),
            )
            .await;
            assert_eq!(status, axum::http::StatusCode::FORBIDDEN);
        }
        assert_eq!(snapshot(&state), before);
        assert_eq!(count(&state.conn.lock().unwrap(), "repos"), 0);
    }

    #[tokio::test]
    async fn unknown_owner_is_refused_everywhere() {
        let (state, _db) = state();
        state
            .conn
            .lock()
            .unwrap()
            .execute(
                "INSERT INTO repos (org, name) VALUES ('example-org', 'payments-core')",
                [],
            )
            .unwrap();
        let before = snapshot(&state);

        for (kind, key) in [
            ("github_repo", "payments-core"),
            ("github_repo", "payments-core, audit-trail"),
            ("jira_board", "PAY"),
            ("jira_board", "PAY, AUD"),
        ] {
            let status = attach_status(
                &state,
                json!({"kind": kind, "key": key, "owner": "nobody@firm.example"}),
            )
            .await;
            assert_eq!(status, axum::http::StatusCode::FORBIDDEN);
        }

        let status = select_status(
            &state,
            json!({"owner": "nobody@firm.example", "epic_keys": ["PAY-100"]}),
        )
        .await;
        assert_eq!(status, axum::http::StatusCode::FORBIDDEN);

        let status = map_status(
            &state,
            json!({
                "repo": "payments-core",
                "epic_key": "PAY-100",
                "updated_by": "nobody@firm.example"
            }),
        )
        .await;
        assert_eq!(status, axum::http::StatusCode::FORBIDDEN);

        let status = map_status(
            &state,
            json!({"repo": "payments-core", "epic_key": "PAY-100", "updated_by": ""}),
        )
        .await;
        assert_eq!(status, axum::http::StatusCode::FORBIDDEN);

        let status = map_status(
            &state,
            json!({"repo": "payments-core", "epic_key": "PAY-100", "updated_by": "   "}),
        )
        .await;
        assert_eq!(status, axum::http::StatusCode::FORBIDDEN);

        let status = attach_status(
            &state,
            json!({"kind": "github_repo", "key": "payments-core", "owner": ""}),
        )
        .await;
        assert_eq!(status, axum::http::StatusCode::FORBIDDEN);

        let status = select_status(&state, json!({"owner": "", "epic_keys": ["PAY-100"]})).await;
        assert_eq!(status, axum::http::StatusCode::FORBIDDEN);

        assert_eq!(snapshot(&state), before);
        assert_eq!(count(&state.conn.lock().unwrap(), "repo_epic_map"), 0);

        let status = map_status(
            &state,
            json!({"repo": "payments-core", "epic_key": "PAY-100", "updated_by": "lead"}),
        )
        .await;
        assert_eq!(status, axum::http::StatusCode::OK);
        assert_eq!(count(&state.conn.lock().unwrap(), "repo_epic_map"), 1);
    }

    #[tokio::test]
    async fn roleless_or_missing_actor_is_refused_on_map_and_select() {
        let (state, _db) = state();
        {
            let conn = state.conn.lock().unwrap();
            conn.execute(
                "INSERT INTO identity_map (jira_account_id, github_login, display_name, confirmed, role)
                 VALUES ('blank@firm.example', 'blank', 'Blank', 1, NULL)",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO repos (org, name) VALUES ('example-org', 'payments-core')",
                [],
            )
            .unwrap();
        }
        let before = snapshot(&state);

        // A stored person with no role: by account id and by GitHub login.
        for who in ["blank@firm.example", "blank"] {
            let status = select_status(&state, json!({"owner": who, "epic_keys": ["PAY-100"]})).await;
            assert_eq!(status, axum::http::StatusCode::FORBIDDEN, "select {who}");
            let status = map_status(
                &state,
                json!({"repo": "payments-core", "epic_key": "PAY-100", "updated_by": who}),
            )
            .await;
            assert_eq!(status, axum::http::StatusCode::FORBIDDEN, "map {who}");
        }

        // No updated_by at all is blank, not the lead.
        let status = map_status(&state, json!({"repo": "payments-core", "epic_key": "PAY-100"})).await;
        assert_eq!(status, axum::http::StatusCode::FORBIDDEN);
        let status = map_status(
            &state,
            json!({"repo": "payments-core", "epic_key": "PAY-100", "updated_by": null}),
        )
        .await;
        assert_eq!(status, axum::http::StatusCode::FORBIDDEN);

        assert_eq!(snapshot(&state), before);

        // Each known role may still pick projects and map.
        for who in ["nora@firm.example", "mina@firm.example", "rosa@firm.example", "iris@firm.example"] {
            let status = select_status(&state, json!({"owner": who, "epic_keys": ["PAY-100"]})).await;
            assert_eq!(status, axum::http::StatusCode::OK, "select {who}");
            let status = map_status(
                &state,
                json!({"repo": "payments-core", "epic_key": "PAY-100", "updated_by": who}),
            )
            .await;
            assert_eq!(status, axum::http::StatusCode::OK, "map {who}");
        }
    }

    #[tokio::test]
    async fn wrong_kind_for_the_role_writes_nothing() {
        let (state, _db) = state();
        state
            .conn
            .lock()
            .unwrap()
            .execute(
                "INSERT INTO identity_map (jira_account_id, github_login, display_name, confirmed, role)
                 VALUES ('blank@firm.example', 'blank', 'Blank', 1, NULL)",
                [],
            )
            .unwrap();
        let before = snapshot(&state);

        for (kind, key, owner) in [
            ("jira_board", "PAY", "blank@firm.example"),
            ("github_repo", "payments-core", "mina@firm.example"),
            ("jira_board", "AUD", "rosa@firm.example"),
            ("github_repo", "audit-trail", "iris@firm.example"),
            ("github_repo", "payments-core", "blank@firm.example"),
            ("jira_board", "PAY", "blank@firm.example"),
        ] {
            let status = attach_status(&state, json!({"kind": kind, "key": key, "owner": owner})).await;
            assert_eq!(status, axum::http::StatusCode::FORBIDDEN, "{kind} {key} {owner}");
        }
        assert_eq!(snapshot(&state), before);

        let status = attach_status(
            &state,
            json!({"kind": "slack_channel", "key": "general", "owner": "nora@firm.example"}),
        )
        .await;
        assert_eq!(status, axum::http::StatusCode::BAD_REQUEST);
        assert_eq!(snapshot(&state), before);
    }

    #[tokio::test]
    async fn one_bad_item_rolls_back_the_batch() {
        let (state, _db) = state();
        let before = snapshot(&state);

        let status = attach_status(
            &state,
            json!({"kind": "jira_board", "key": "PAY, not a board!", "owner": "mina@firm.example"}),
        )
        .await;
        assert_eq!(status, axum::http::StatusCode::BAD_REQUEST);
        assert_eq!(snapshot(&state), before);

        let status = attach_status(
            &state,
            json!({"kind": "github_repo", "key": "payments-core, bad repo name!", "owner": "nora@firm.example"}),
        )
        .await;
        assert_eq!(status, axum::http::StatusCode::BAD_REQUEST);
        assert_eq!(snapshot(&state), before);
    }

    async fn queue_status(state: &AppState, pr_id: &str, body: serde_json::Value) -> axum::http::StatusCode {
        status_of(
            queue_action(
                State(state.clone()),
                AxPath(pr_id.to_string()),
                json_headers(),
                body.to_string(),
            )
            .await,
        )
    }

    /// The status and the "error" text of a refused queue action.
    async fn queue_refusal(state: &AppState, pr_id: &str, body: serde_json::Value) -> (axum::http::StatusCode, String) {
        match queue_action(State(state.clone()), AxPath(pr_id.to_string()), json_headers(), body.to_string()).await {
            Ok(_) => panic!("expected a refusal"),
            Err((status, Json(v))) => (status, v["error"].as_str().unwrap_or_default().to_string()),
        }
    }

    /// The error a person reads names no storage internals.
    fn assert_plain(msg: &str) {
        let lower = msg.to_lowercase();
        for word in ["sqlite", "sql", "constraint", "foreign key", "unique", "rusqlite", "no such"] {
            assert!(!lower.contains(word), "storage wording {word:?} in {msg:?}");
        }
        assert!(msg.ends_with("Nothing was linked."), "{msg:?}");
    }

    fn active_links(state: &AppState, pr_id: &str) -> i64 {
        state
            .conn
            .lock()
            .unwrap()
            .query_row("SELECT COUNT(*) FROM pr_story_links WHERE pr_id = ?1 AND active = 1", [pr_id], |r| r.get(0))
            .unwrap()
    }

    fn active_link(state: &AppState, pr_id: &str) -> Option<String> {
        state
            .conn
            .lock()
            .unwrap()
            .query_row(
                "SELECT story_key FROM pr_story_links WHERE pr_id = ?1 AND active = 1",
                [pr_id],
                |r| r.get(0),
            )
            .ok()
    }

    #[tokio::test]
    async fn a_refused_confirm_keeps_the_current_link() {
        let (state, _db) = state();
        {
            let conn = state.conn.lock().unwrap();
            for (key, summary) in [("PAY-101", "Refunds"), ("PAY-102", "Receipts")] {
                conn.execute(
                    "INSERT INTO stories (story_key, epic_key, summary) VALUES (?1, 'PAY-100', ?2)",
                    rusqlite::params![key, summary],
                )
                .unwrap();
            }
            conn.execute(
                "INSERT INTO pull_requests (id, repo, number, title, state)
                 VALUES ('PR_1', 'payments-core', 7, 'Refund flow', 'open')",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO review_queue (pr_id, status) VALUES ('PR_1', 'candidate')",
                [],
            )
            .unwrap();
        }

        let ok = json!({"action": "confirm", "story_key": "PAY-101"});
        assert_eq!(queue_status(&state, "PR_1", ok).await, axum::http::StatusCode::OK);
        assert_eq!(active_link(&state, "PR_1").as_deref(), Some("PAY-101"));
        assert_eq!(active_links(&state, "PR_1"), 1);

        // A story that is not stored is refused in plain words, and the link stays.
        let typo = json!({"action": "confirm", "story_key": "PAY-1O1"});
        let (status, msg) = queue_refusal(&state, "PR_1", typo).await;
        assert_eq!(status, axum::http::StatusCode::BAD_REQUEST);
        assert_plain(&msg);
        assert_eq!(active_link(&state, "PR_1").as_deref(), Some("PAY-101"));
        assert_eq!(active_links(&state, "PR_1"), 1);

        // A pull request that is not stored is refused in plain words and writes nothing.
        let ghost = json!({"action": "confirm", "story_key": "PAY-102"});
        let (status, msg) = queue_refusal(&state, "PR_ghost", ghost).await;
        assert_eq!(status, axum::http::StatusCode::BAD_REQUEST);
        assert_plain(&msg);
        let n: i64 = state
            .conn
            .lock()
            .unwrap()
            .query_row("SELECT COUNT(*) FROM pr_story_links WHERE pr_id = 'PR_ghost'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 0);

        // Relinking to another stored story moves the one active link.
        let move_it = json!({"action": "confirm", "story_key": "PAY-102"});
        assert_eq!(queue_status(&state, "PR_1", move_it).await, axum::http::StatusCode::OK);
        assert_eq!(active_link(&state, "PR_1").as_deref(), Some("PAY-102"));
        assert_eq!(active_links(&state, "PR_1"), 1);
    }
    /// A state whose database (and so feedback.jsonl) is in its own temp folder.
    fn feedback_state(name: &str) -> (AppState, TempDb, std::path::PathBuf) {
        let (state, db) = state();
        let dir = std::env::temp_dir().join(format!("tallimo_fbapi_{}_{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut settings = (*state.settings).clone();
        settings.db_path = dir.join("rollup.db");
        let state = AppState {
            conn: state.conn,
            settings: Arc::new(settings),
            write_token: state.write_token,
        };
        (state, db, dir)
    }

    fn feedback_lines(dir: &std::path::Path) -> Vec<serde_json::Value> {
        std::fs::read_to_string(dir.join("feedback.jsonl"))
            .unwrap_or_default()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    #[tokio::test]
    async fn a_flag_appends_exactly_one_schema_v1_line() {
        let (state, _db, dir) = feedback_state("ok");
        let port = serve(&state).await;
        let host = format!("127.0.0.1:{port}");
        let body = json!({"page": "today", "card": "pr-7", "verdict": "wrong", "note": "count is off"});
        assert_eq!(raw_status(port, post_req("/feedback", &host, &state.write_token, &body)).await, 200);
        let lines = feedback_lines(&dir);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0]["v"], 1);
        assert_eq!(lines[0]["product"], "tallimo");
        assert_eq!(lines[0]["loop"], "human");
        assert_eq!(lines[0]["ref"], "today:pr-7");
        assert_eq!(lines[0]["detail"], "wrong: count is off");
        let origin = format!("http://127.0.0.1:{port}");
        let from_page = post_req("/feedback", &host, &state.write_token, &json!({"page": "inbox", "card": "c2", "verdict": "dismiss"}))
            .replacen("\r\n", &format!("\r\nOrigin: {origin}\r\n"), 1);
        assert_eq!(raw_status(port, from_page).await, 200);
        assert_eq!(feedback_lines(&dir).len(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn feedback_refuses_foreign_hosts_origins_big_bodies_and_bad_input() {
        let (state, _db, dir) = feedback_state("refuse");
        let port = serve(&state).await;
        let host = format!("127.0.0.1:{port}");
        let good = json!({"page": "today", "card": "pr-7", "verdict": "wrong"});
        // DNS rebinding: a Host that is not this server.
        assert_eq!(raw_status(port, post_req("/feedback", "attacker.example", &state.write_token, &good)).await, 421);
        assert_eq!(raw_status(port, post_req("/feedback", &format!("attacker.example:{port}"), &state.write_token, &good)).await, 421);
        // A page on another site.
        assert_eq!(raw_status(port, post_from("/feedback", &host, "https://attacker.example", &good)).await, 403);
        // Over the size cap.
        let big = json!({"page": "today", "card": "pr-7", "verdict": "wrong", "note": "x".repeat(FEEDBACK_MAX_BODY)});
        assert_eq!(raw_status(port, post_req("/feedback", &host, &state.write_token, &big)).await, 413);
        // Not JSON, bad JSON, bad fields, out-of-scope page.
        let plain = post_req("/feedback", &host, &state.write_token, &good).replace("application/json", "text/plain");
        assert_eq!(raw_status(port, plain).await, 415);
        let broken = format!(
            "POST /feedback HTTP/1.1\r\nHost: {host}\r\nX-Tallimo-Token: {}\r\nContent-Type: application/json\r\nContent-Length: 3\r\nConnection: close\r\n\r\n{{{{x",
            state.write_token
        );
        assert_eq!(raw_status(port, broken).await, 400);
        for bad in [
            json!({"page": "leadership", "card": "c", "verdict": "wrong"}),
            json!({"page": "today", "card": "../x", "verdict": "wrong"}),
            json!({"page": "today", "card": "c", "verdict": "maybe"}),
            json!({}),
        ] {
            assert_eq!(raw_status(port, post_req("/feedback", &host, &state.write_token, &bad)).await, 400, "{bad}");
        }
        // GET is not a route for writes.
        assert_eq!(raw_status(port, get_req("/feedback", &host, &state.write_token)).await, 405);
        assert!(feedback_lines(&dir).is_empty(), "a refused request wrote a line");
        let _ = std::fs::remove_dir_all(&dir);
    }

    async fn raw_parts(port: u16, request: impl AsRef<[u8]>) -> (u16, String) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        stream.write_all(request.as_ref()).await.unwrap();
        let mut buf = Vec::new();
        stream.read_to_end(&mut buf).await.unwrap();
        let text = String::from_utf8_lossy(&buf).into_owned();
        let status = text.split_whitespace().nth(1).unwrap().parse().unwrap();
        let body = text.split_once("\r\n\r\n").map(|(_, body)| body.to_string()).unwrap_or_default();
        (status, body)
    }

    fn error_text(body: &str) -> String {
        serde_json::from_str::<serde_json::Value>(body).unwrap()["error"]
            .as_str()
            .unwrap()
            .to_string()
    }

    fn has_draft(body: &str) -> bool {
        let v: serde_json::Value = serde_json::from_str(body).unwrap();
        v["entries"].as_array().unwrap().iter().any(|entry| entry["sent_at"].is_null())
    }

    #[tokio::test]
    async fn leadership_updates_follow_the_owner_and_the_draft() {
        let (state, _db) = state();
        state
            .conn
            .lock()
            .unwrap()
            .execute(
                "INSERT INTO identity_map (jira_account_id, github_login, display_name, confirmed, role)
                 VALUES ('dir@firm.example', NULL, 'Dee', 1, 'director')",
                [],
            )
            .unwrap();
        let port = serve(&state).await;
        let host = format!("127.0.0.1:{port}");
        let pm = "mina@firm.example";

        let (status, _) = raw_parts(port, post_req("/updates/next", &host, &state.write_token, &json!({"owner": pm, "entry_date": "2026-10-01"}))).await;
        assert_eq!(status, 200);
        let (status, body) = raw_parts(
            port,
            post_req("/updates/next", &host, &state.write_token, &json!({"owner": pm, "entry_date": "2026-10-08"})),
        )
        .await;
        assert_eq!(status, 409);
        assert_eq!(error_text(&body), "A draft is already open.");

        let item = json!({
            "owner": pm,
            "item": {
                "item_id": "pay-1",
                "category": "Now",
                "position": 0,
                "title": "Payments",
                "priority": 1,
                "capacity": 25,
                "completed": ["shipped"],
                "next": ["roll out"],
                "future": []
            }
        });
        assert_eq!(raw_parts(port, post_req("/updates/item", &host, &state.write_token, &item)).await.0, 200);
        assert_eq!(
            raw_parts(
                port,
                post_req("/updates/item/move", &host, &state.write_token, &json!({"owner": pm, "item_id": "pay-1", "category": "Later", "position": 2})),
            )
            .await
            .0,
            200
        );
        let extra = json!({
            "owner": pm,
            "item": {"item_id": "pay-2", "category": "Now", "position": 1, "title": "Receipts", "priority": 0}
        });
        assert_eq!(raw_parts(port, post_req("/updates/item", &host, &state.write_token, &extra)).await.0, 200);
        assert_eq!(
            raw_parts(port, post_req("/updates/item/remove", &host, &state.write_token, &json!({"owner": pm, "item_id": "pay-2"}))).await.0,
            200
        );
        assert_eq!(
            raw_parts(
                port,
                post_req("/updates/settings", &host, &state.write_token, &json!({"owner": pm, "programme": "Weekly", "category_order": ["Later", "Now"]})),
            )
            .await
            .0,
            200
        );

        let (status, body) = raw_parts(
            port,
            post_req(
                "/updates/item",
                &host,
                &state.write_token,
                &json!({"owner": "nora@firm.example", "item": {"item_id": "pay-9", "category": "Now", "position": 0, "title": "Nope", "priority": 0}}),
            ),
        )
        .await;
        assert_eq!(status, 403);
        assert_eq!(error_text(&body), "Only the owner can edit leadership updates.");

        assert_eq!(raw_parts(port, post_req("/updates/send", &host, &state.write_token, &json!({"owner": pm}))).await.0, 200);
        let (status, body) = raw_parts(port, post_req("/updates/item", &host, &state.write_token, &item)).await;
        assert_eq!(status, 409);
        assert_eq!(
            error_text(&body),
            "This entry was sent. It is a record of what the Director saw."
        );

        assert_eq!(
            raw_parts(port, post_req("/updates/next", &host, &state.write_token, &json!({"owner": pm, "entry_date": "2026-10-08"}))).await.0,
            200
        );
        let (_, pm_body) = raw_parts(port, get_req(&format!("/updates?who={pm}"), &host, &state.write_token)).await;
        let pm_json: serde_json::Value = serde_json::from_str(&pm_body).unwrap();
        let entries = pm_json["entries"].as_array().unwrap();
        assert_eq!(entries.len(), 2);
        let draft = entries.iter().find(|entry| entry["sent_at"].is_null()).unwrap();
        assert_eq!(draft["programme"], "Weekly");
        assert_eq!(draft["total"], 25);
        assert_eq!(draft["label"], "75% unallocated");
        assert_eq!(draft["items"][0]["item_id"], "pay-1");
        assert_eq!(draft["items"][0]["category"], "Later");
        assert_eq!(draft["items"][0]["is_new"], false);
        assert_eq!(draft["items"].as_array().unwrap().len(), 1);
        let sent = entries.iter().find(|entry| entry["sent_at"].is_string()).unwrap();
        assert_eq!(sent["items"][0]["is_new"], true);

        for who in ["dir@firm.example", "nobody@firm.example", "nora@firm.example"] {
            let (status, body) = raw_parts(port, get_req(&format!("/updates?who={who}"), &host, &state.write_token)).await;
            assert_eq!(status, 200, "{who}");
            assert!(!has_draft(&body), "{who} saw the draft");
            let rows: serde_json::Value = serde_json::from_str(&body).unwrap();
            assert_eq!(rows["entries"].as_array().unwrap().len(), 1, "{who}");
        }
        let (_, missing) = raw_parts(port, get_req("/updates", &host, &state.write_token)).await;
        assert!(!has_draft(&missing));

        let (_, ppm) = raw_parts(port, get_req("/updates?who=iris@firm.example", &host, &state.write_token)).await;
        assert!(has_draft(&ppm));

        let big = json!({"owner": pm, "entry_date": "2026-10-15", "pad": "x".repeat(UPDATES_MAX_BODY)});
        assert_eq!(raw_status(port, post_req("/updates/next", &host, &state.write_token, &big)).await, 413);
        let plain = post_req("/updates/next", &host, &state.write_token, &json!({"owner": pm, "entry_date": "2026-10-15"})).replace("application/json", "text/plain");
        assert_eq!(raw_status(port, plain).await, 415);
    }

    #[tokio::test]
    async fn ai_notes_and_sources_follow_the_owner() {
        let (state, _db) = state();
        let port = serve(&state).await;
        let host = format!("127.0.0.1:{port}");
        let token = &state.write_token;
        let pm = "mina@firm.example";
        let dev = "nora@firm.example";
        let note = json!({"owner": pm, "kind": "glossary", "body": "Pelix => Felix"});

        let (status, body) = raw_parts(port, post_req("/ai/notes", &host, token, &note)).await;
        assert_eq!(status, 200);
        let id = serde_json::from_str::<serde_json::Value>(&body).unwrap()["id"].as_i64().unwrap();
        let (status, again) = raw_parts(port, post_req("/ai/notes", &host, token, &note)).await;
        assert_eq!(status, 200);
        assert_eq!(serde_json::from_str::<serde_json::Value>(&again).unwrap()["id"], id);

        let (status, listed) = raw_parts(port, get_req(&format!("/ai/notes?who={pm}"), &host, token)).await;
        assert_eq!(status, 200);
        let listed: serde_json::Value = serde_json::from_str(&listed).unwrap();
        assert_eq!(listed["notes"].as_array().unwrap().len(), 1);
        assert_eq!(listed["notes"][0]["id"], id);
        assert_eq!(listed["notes"][0]["kind"], "glossary");
        assert_eq!(listed["notes"][0]["body"], "Pelix => Felix");
        assert!(listed["notes"][0]["created_at"].as_str().unwrap().ends_with('Z'));
        let (status, ppm) = raw_parts(port, get_req("/ai/notes?who=iris@firm.example", &host, token)).await;
        assert_eq!(status, 200);
        assert_eq!(serde_json::from_str::<serde_json::Value>(&ppm).unwrap()["notes"][0]["id"], id);

        let (status, body) = raw_parts(
            port,
            post_req("/ai/notes", &host, token, &json!({"owner": pm, "kind": "rule", "body": "keep:ITEM-1"})),
        )
        .await;
        assert_eq!(status, 400);
        assert_eq!(
            error_text(&body),
            "A rule looks like drop:<item_id>, no_end_date:<item_id>, or never_say:<phrase>."
        );

        let (status, body) = raw_parts(
            port,
            post_req("/ai/notes", &host, token, &json!({"owner": dev, "kind": "fact", "body": "no"})),
        )
        .await;
        assert_eq!(status, 403);
        assert_eq!(error_text(&body), "Only the owner can edit leadership updates.");
        let (status, body) = raw_parts(port, get_req(&format!("/ai/notes?who={dev}"), &host, token)).await;
        assert_eq!(status, 403);
        assert_eq!(error_text(&body), "Only the owner can read AI notes and sources.");
        let (status, body) = raw_parts(port, get_req(&format!("/ai/sources?who={dev}&item_id=pay-1"), &host, token)).await;
        assert_eq!(status, 403);
        assert_eq!(error_text(&body), "Only the owner can read AI notes and sources.");

        let resolve = json!({"owner": pm, "id": id});
        let (status, body) = raw_parts(port, post_req("/ai/notes/resolve", &host, token, &resolve)).await;
        assert_eq!(status, 200);
        assert_eq!(serde_json::from_str::<serde_json::Value>(&body).unwrap()["resolved"], true);
        let (status, body) = raw_parts(port, post_req("/ai/notes/resolve", &host, token, &resolve)).await;
        assert_eq!(status, 200);
        assert_eq!(serde_json::from_str::<serde_json::Value>(&body).unwrap()["resolved"], false);
        let (status, body) = raw_parts(
            port,
            post_req("/ai/notes/resolve", &host, token, &json!({"owner": pm, "id": "1"})),
        )
        .await;
        assert_eq!(status, 400);
        assert_eq!(error_text(&body), "id must be a whole number.");

        let link = json!({"owner": pm, "item_id": "pay-1", "kind": "story", "key": "PAY-100"});
        let (status, body) = raw_parts(port, post_req("/ai/map", &host, token, &link)).await;
        assert_eq!(status, 200);
        assert_eq!(serde_json::from_str::<serde_json::Value>(&body).unwrap()["ok"], true);
        let (status, body) = raw_parts(port, get_req(&format!("/ai/sources?who={pm}&item_id=pay-1"), &host, token)).await;
        assert_eq!(status, 200);
        let sources: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(sources["sources"], json!([{"kind": "story", "key": "PAY-100"}]));
        let (status, body) = raw_parts(port, post_req("/ai/unmap", &host, token, &link)).await;
        assert_eq!(status, 200);
        assert_eq!(serde_json::from_str::<serde_json::Value>(&body).unwrap()["removed"], true);
        let (status, body) = raw_parts(
            port,
            post_req(
                "/ai/map",
                &host,
                token,
                &json!({"owner": pm, "item_id": "pay-1", "kind": "ticket", "key": "PAY-100"}),
            ),
        )
        .await;
        assert_eq!(status, 400);
        assert_eq!(error_text(&body), "Source kind must be epic, story, or repo.");
        let (status, body) = raw_parts(port, get_req(&format!("/ai/sources?who={pm}"), &host, token)).await;
        assert_eq!(status, 400);
        assert_eq!(error_text(&body), "item_id is required.");
    }

    #[tokio::test]
    async fn leadership_update_discard_removes_the_open_draft() {
        let (state, _db) = state();
        let port = serve(&state).await;
        let host = format!("127.0.0.1:{port}");
        let pm = "mina@firm.example";
        let dev = "nora@firm.example";

        let (status, body) = raw_parts(
            port,
            post_req("/updates/discard", &host, &state.write_token, &json!({"owner": pm})),
        )
        .await;
        assert_eq!(status, 409);
        assert_eq!(error_text(&body), "There is no open draft.");

        assert_eq!(
            raw_parts(
                port,
                post_req(
                    "/updates/next",
                    &host,
                    &state.write_token,
                    &json!({"owner": pm, "entry_date": "2026-10-01"}),
                ),
            )
            .await
            .0,
            200
        );
        let item = json!({
            "owner": pm,
            "item": {"item_id": "pay-1", "category": "Now", "position": 0, "title": "Payments", "priority": 0}
        });
        assert_eq!(
            raw_parts(port, post_req("/updates/item", &host, &state.write_token, &item)).await.0,
            200
        );
        assert_eq!(
            raw_parts(port, post_req("/updates/send", &host, &state.write_token, &json!({"owner": pm}))).await.0,
            200
        );
        assert_eq!(
            raw_parts(
                port,
                post_req(
                    "/updates/next",
                    &host,
                    &state.write_token,
                    &json!({"owner": pm, "entry_date": "2026-10-08"}),
                ),
            )
            .await
            .0,
            200
        );

        let (status, body) = raw_parts(
            port,
            post_req("/updates/discard", &host, &state.write_token, &json!({"owner": dev})),
        )
        .await;
        assert_eq!(status, 403);
        assert_eq!(error_text(&body), "Only the owner can edit leadership updates.");

        assert_eq!(
            raw_parts(port, post_req("/updates/discard", &host, &state.write_token, &json!({"owner": pm}))).await.0,
            200
        );
        let (_, got) = raw_parts(port, get_req(&format!("/updates?who={pm}"), &host, &state.write_token)).await;
        assert!(!has_draft(&got));
        let got: serde_json::Value = serde_json::from_str(&got).unwrap();
        let entries = got["entries"].as_array().unwrap();
        assert_eq!(entries.len(), 1);
        assert!(entries[0]["sent_at"].is_string());
        assert_eq!(entries[0]["items"][0]["item_id"], "pay-1");
        assert_eq!(entries[0]["items"][0]["title"], "Payments");
    }

    #[tokio::test]
    async fn leadership_update_correction_needs_the_owner_and_a_confirm() {
        let (state, _db) = state();
        let port = serve(&state).await;
        let host = format!("127.0.0.1:{port}");
        let pm = "mina@firm.example";
        assert_eq!(
            raw_parts(
                port,
                post_req("/updates/next", &host, &state.write_token, &json!({"owner": pm, "entry_date": "2026-10-01"})),
            )
            .await
            .0,
            200
        );
        let item = json!({
            "owner": pm,
            "item": {
                "item_id": "pay-1",
                "category": "Now",
                "position": 0,
                "title": "Payments vendor",
                "priority": 1,
                "pct": 40,
                "start_date": "2026-10-01",
                "end_date": "2026-10-31"
            }
        });
        assert_eq!(raw_parts(port, post_req("/updates/item", &host, &state.write_token, &item)).await.0, 200);
        assert_eq!(
            raw_parts(port, post_req("/updates/send", &host, &state.write_token, &json!({"owner": pm}))).await.0,
            200
        );

        let (status, body) = raw_parts(
            port,
            post_req(
                "/updates/correct",
                &host,
                &state.write_token,
                &json!({"owner": pm, "find": "vendor", "replace": "partner"}),
            ),
        )
        .await;
        assert_eq!(status, 400);
        assert_eq!(error_text(&body), "Confirm the correction first.");
        let (status, body) = raw_parts(
            port,
            post_req(
                "/updates/correct",
                &host,
                &state.write_token,
                &json!({"owner": pm, "find": "vendor", "replace": "partner", "confirm": false}),
            ),
        )
        .await;
        assert_eq!(status, 400);
        assert_eq!(error_text(&body), "Confirm the correction first.");

        let (status, body) = raw_parts(
            port,
            post_req(
                "/updates/correct",
                &host,
                &state.write_token,
                &json!({"owner": "nora@firm.example", "find": "vendor", "replace": "partner", "confirm": true}),
            ),
        )
        .await;
        assert_eq!(status, 403);
        assert_eq!(error_text(&body), "Only the owner can edit leadership updates.");

        let (_, still) = raw_parts(port, get_req(&format!("/updates?who={pm}"), &host, &state.write_token)).await;
        let still: serde_json::Value = serde_json::from_str(&still).unwrap();
        assert_eq!(still["entries"][0]["items"][0]["title"], "Payments vendor");

        let (status, body) = raw_parts(
            port,
            post_req(
                "/updates/correct",
                &host,
                &state.write_token,
                &json!({"owner": pm, "find": "vendor", "replace": "partner", "confirm": true}),
            ),
        )
        .await;
        assert_eq!(status, 200);
        let changed: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(changed["changed"], 1);

        let (_, got) = raw_parts(port, get_req(&format!("/updates?who={pm}"), &host, &state.write_token)).await;
        let got: serde_json::Value = serde_json::from_str(&got).unwrap();
        assert_eq!(got["entries"][0]["items"][0]["title"], "Payments partner");
        assert_eq!(got["entries"][0]["items"][0]["pct"], 40);
        assert_eq!(got["entries"][0]["items"][0]["start_date"], "2026-10-01");
        assert_eq!(got["entries"][0]["items"][0]["end_date"], "2026-10-31");
        assert_eq!(got["entries"][0]["entry_date"], "2026-10-01");
        assert!(got["entries"][0]["sent_at"].is_string());
        assert_eq!(got["entries"][0]["corrected"], true);
    }

    async fn first_title(port: u16, host: &str, token: &str, pm: &str) -> String {
        let (_, body) = raw_parts(port, get_req(&format!("/updates?who={pm}"), host, token)).await;
        let got: serde_json::Value = serde_json::from_str(&body).unwrap();
        got["entries"][0]["items"][0]["title"]
            .as_str()
            .unwrap()
            .to_string()
    }

    #[tokio::test]
    async fn leadership_update_correction_requires_both_strings_and_can_preview() {
        let (state, _db) = state();
        let port = serve(&state).await;
        let host = format!("127.0.0.1:{port}");
        let pm = "mina@firm.example";
        assert_eq!(
            raw_parts(
                port,
                post_req("/updates/next", &host, &state.write_token, &json!({"owner": pm, "entry_date": "2026-10-01"})),
            )
            .await
            .0,
            200
        );
        let item = json!({
            "owner": pm,
            "item": {
                "item_id": "pay-1",
                "category": "Now",
                "position": 0,
                "title": "Payments vendor",
                "priority": 1,
                "pct": 40,
                "start_date": "2026-10-01",
                "end_date": "2026-10-31"
            }
        });
        assert_eq!(raw_parts(port, post_req("/updates/item", &host, &state.write_token, &item)).await.0, 200);
        assert_eq!(
            raw_parts(port, post_req("/updates/send", &host, &state.write_token, &json!({"owner": pm}))).await.0,
            200
        );

        let (status, body) = raw_parts(
            port,
            post_req(
                "/updates/correct",
                &host,
                &state.write_token,
                &json!({"owner": pm, "find": "vendor", "confirm": true}),
            ),
        )
        .await;
        assert_eq!(status, 400);
        assert_eq!(error_text(&body), "Type the replacement text.");
        assert_eq!(first_title(port, &host, &state.write_token, pm).await, "Payments vendor");

        let (status, body) = raw_parts(
            port,
            post_req(
                "/updates/correct",
                &host,
                &state.write_token,
                &json!({"owner": pm, "find": 1, "replace": "partner", "confirm": true}),
            ),
        )
        .await;
        assert_eq!(status, 400);
        assert_eq!(error_text(&body), "Type the text to find.");
        assert_eq!(first_title(port, &host, &state.write_token, pm).await, "Payments vendor");

        let (status, body) = raw_parts(
            port,
            post_req(
                "/updates/correct",
                &host,
                &state.write_token,
                &json!({"owner": pm, "find": "vendor", "replace": "partner", "preview": true}),
            ),
        )
        .await;
        assert_eq!(status, 200);
        let preview: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(preview["would_change"], 1);
        assert!(preview.get("changed").is_none());
        assert_eq!(first_title(port, &host, &state.write_token, pm).await, "Payments vendor");

        let (status, body) = raw_parts(
            port,
            post_req(
                "/updates/correct",
                &host,
                &state.write_token,
                &json!({"owner": pm, "find": "vendor", "replace": "", "confirm": true}),
            ),
        )
        .await;
        assert_eq!(status, 200);
        let changed: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(changed["changed"], 1);
        assert_eq!(first_title(port, &host, &state.write_token, pm).await, "Payments ");
    }

    #[tokio::test]
    async fn leadership_update_correction_flags_only_the_sent_entry_it_changed() {
        let (state, _db) = state();
        let port = serve(&state).await;
        let host = format!("127.0.0.1:{port}");
        let pm = "mina@firm.example";
        let item = |title: &str| {
            json!({
                "owner": pm,
                "item": {
                    "item_id": "pay-1",
                    "category": "Now",
                    "position": 0,
                    "title": title,
                    "priority": 1,
                    "pct": 40,
                    "start_date": "2026-10-01",
                    "end_date": "2026-10-31"
                }
            })
        };
        assert_eq!(
            raw_parts(
                port,
                post_req("/updates/next", &host, &state.write_token, &json!({"owner": pm, "entry_date": "2026-10-01"})),
            )
            .await
            .0,
            200
        );
        assert_eq!(
            raw_parts(port, post_req("/updates/item", &host, &state.write_token, &item("Payments vendor"))).await.0,
            200
        );
        assert_eq!(
            raw_parts(port, post_req("/updates/send", &host, &state.write_token, &json!({"owner": pm}))).await.0,
            200
        );
        assert_eq!(
            raw_parts(
                port,
                post_req("/updates/next", &host, &state.write_token, &json!({"owner": pm, "entry_date": "2026-10-08"})),
            )
            .await
            .0,
            200
        );
        assert_eq!(
            raw_parts(port, post_req("/updates/item", &host, &state.write_token, &item("Receipts"))).await.0,
            200
        );
        assert_eq!(
            raw_parts(port, post_req("/updates/send", &host, &state.write_token, &json!({"owner": pm}))).await.0,
            200
        );

        let (status, body) = raw_parts(
            port,
            post_req(
                "/updates/correct",
                &host,
                &state.write_token,
                &json!({"owner": pm, "find": "vendor", "replace": "partner", "preview": true}),
            ),
        )
        .await;
        assert_eq!(status, 200);
        assert_eq!(serde_json::from_str::<serde_json::Value>(&body).unwrap()["would_change"], 1);
        let none: i64 = state
            .conn
            .lock()
            .unwrap()
            .query_row("SELECT COUNT(*) FROM lu_corrections", [], |row| row.get(0))
            .unwrap();
        assert_eq!(none, 0);

        let (status, body) = raw_parts(
            port,
            post_req(
                "/updates/correct",
                &host,
                &state.write_token,
                &json!({"owner": pm, "find": "zzzz", "replace": "partner", "confirm": true}),
            ),
        )
        .await;
        assert_eq!(status, 200);
        assert_eq!(serde_json::from_str::<serde_json::Value>(&body).unwrap()["changed"], 0);
        let still_none: i64 = state
            .conn
            .lock()
            .unwrap()
            .query_row("SELECT COUNT(*) FROM lu_corrections", [], |row| row.get(0))
            .unwrap();
        assert_eq!(still_none, 0);

        let (status, body) = raw_parts(
            port,
            post_req(
                "/updates/correct",
                &host,
                &state.write_token,
                &json!({"owner": pm, "find": "vendor", "replace": "partner", "confirm": true}),
            ),
        )
        .await;
        assert_eq!(status, 200);
        assert_eq!(serde_json::from_str::<serde_json::Value>(&body).unwrap()["changed"], 1);

        let (_, got) = raw_parts(port, get_req(&format!("/updates?who={pm}"), &host, &state.write_token)).await;
        let got: serde_json::Value = serde_json::from_str(&got).unwrap();
        let entries = got["entries"].as_array().unwrap();
        let fixed = entries
            .iter()
            .find(|entry| entry["items"][0]["title"] == "Payments partner")
            .unwrap();
        let untouched = entries
            .iter()
            .find(|entry| entry["items"][0]["title"] == "Receipts")
            .unwrap();
        assert_eq!(fixed["corrected"], true);
        assert!(fixed["sent_at"].is_string());
        assert_ne!(untouched.get("corrected").and_then(|flag| flag.as_bool()), Some(true));
        assert!(untouched["sent_at"].is_string());

        let (by, rows, entry_ids): (String, i64, String) = state
            .conn
            .lock()
            .unwrap()
            .query_row(
                "SELECT \"by\", rows, entry_ids FROM lu_corrections",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(by, pm);
        assert_eq!(rows, 1);
        assert_eq!(
            entry_ids,
            serde_json::to_string(&vec![fixed["id"].as_i64().unwrap()]).unwrap()
        );
    }

    #[tokio::test]
    async fn partner_briefs_follow_the_owner_and_the_draft() {
        let (state, _db) = state();
        state
            .conn
            .lock()
            .unwrap()
            .execute(
                "INSERT INTO identity_map (jira_account_id, github_login, display_name, confirmed, role)
                 VALUES ('dir@firm.example', NULL, 'Dee', 1, 'director'),
                        ('sam@firm.example', NULL, 'Sam', 1, 'partner')",
                [],
            )
            .unwrap();
        let port = serve(&state).await;
        let host = format!("127.0.0.1:{port}");
        let pm = "mina@firm.example";

        let (status, body) = raw_parts(
            port,
            post_req("/briefs/send", &host, &state.write_token, &json!({"owner": pm})),
        )
        .await;
        assert_eq!(status, 409);
        assert_eq!(error_text(&body), "There is no open draft.");

        assert_eq!(
            raw_parts(
                port,
                post_req(
                    "/briefs/next",
                    &host,
                    &state.write_token,
                    &json!({"owner": pm, "week_ending": "2026-10-02"}),
                ),
            )
            .await
            .0,
            200
        );
        let (status, body) = raw_parts(
            port,
            post_req(
                "/briefs/next",
                &host,
                &state.write_token,
                &json!({"owner": pm, "week_ending": "2026-10-09"}),
            ),
        )
        .await;
        assert_eq!(status, 409);
        assert_eq!(error_text(&body), "A draft is already open.");

        assert_eq!(
            raw_parts(
                port,
                post_req(
                    "/briefs/header",
                    &host,
                    &state.write_token,
                    &json!({
                        "owner": pm,
                        "org_unit": "Payments",
                        "partner_name": "Sam Sample",
                        "prepared_by": "Alex",
                        "bottom_line": "Ship the parser fix."
                    }),
                ),
            )
            .await
            .0,
            200
        );

        let missing_action = json!({
            "owner": pm,
            "item": {
                "item_id": "esc-1",
                "kind": "needs",
                "position": 0,
                "headline": "Parser is blocked",
                "what": "Nested tables stop the parser.",
                "why": "The partner cannot sign off.",
                "action": "",
                "at_this_stage": 0
            }
        });
        let (status, body) = raw_parts(
            port,
            post_req("/briefs/item", &host, &state.write_token, &missing_action),
        )
        .await;
        assert_eq!(status, 400);
        assert_eq!(error_text(&body), "Every item needs its action line.");

        let item = json!({
            "owner": pm,
            "item": {
                "item_id": "esc-1",
                "kind": "needs",
                "position": 0,
                "headline": "Parser is blocked",
                "what": "Nested tables stop the parser.",
                "why": "The partner cannot sign off.",
                "action": "Ask for a decision.",
                "at_this_stage": 1
            }
        });
        assert_eq!(
            raw_parts(port, post_req("/briefs/item", &host, &state.write_token, &item)).await.0,
            200
        );
        assert_eq!(
            raw_parts(
                port,
                post_req(
                    "/briefs/item/move",
                    &host,
                    &state.write_token,
                    &json!({"owner": pm, "item_id": "esc-1", "position": 2}),
                ),
            )
            .await
            .0,
            200
        );
        let extra = json!({
            "owner": pm,
            "item": {
                "item_id": "esc-2",
                "kind": "fyi",
                "position": 1,
                "headline": "Shipped the queue",
                "what": "The queue is live.",
                "why": "Worth a note.",
                "action": "Milestone only.",
                "at_this_stage": 1
            }
        });
        assert_eq!(
            raw_parts(port, post_req("/briefs/item", &host, &state.write_token, &extra)).await.0,
            200
        );
        assert_eq!(
            raw_parts(
                port,
                post_req(
                    "/briefs/item/remove",
                    &host,
                    &state.write_token,
                    &json!({"owner": pm, "item_id": "esc-2"}),
                ),
            )
            .await
            .0,
            200
        );
        let (status, body) = raw_parts(
            port,
            post_req(
                "/briefs/item/remove",
                &host,
                &state.write_token,
                &json!({"owner": pm, "item_id": "missing"}),
            ),
        )
        .await;
        assert_eq!(status, 404);
        assert_eq!(error_text(&body), "No such item on the draft.");

        let (status, body) = raw_parts(
            port,
            post_req(
                "/briefs/item",
                &host,
                &state.write_token,
                &json!({
                    "owner": "nora@firm.example",
                    "item": {
                        "item_id": "esc-9",
                        "kind": "needs",
                        "position": 0,
                        "headline": "Nope",
                        "what": "",
                        "why": "",
                        "action": "Ask.",
                        "at_this_stage": 0
                    }
                }),
            ),
        )
        .await;
        assert_eq!(status, 403);
        assert_eq!(error_text(&body), "Only the owner can edit the partner brief.");

        assert_eq!(
            raw_parts(
                port,
                post_req("/briefs/send", &host, &state.write_token, &json!({"owner": pm})),
            )
            .await
            .0,
            200
        );
        let (status, body) = raw_parts(port, post_req("/briefs/item", &host, &state.write_token, &item)).await;
        assert_eq!(status, 409);
        assert_eq!(
            error_text(&body),
            "This brief was sent. It is a record of what the Senior Partner saw."
        );

        assert_eq!(
            raw_parts(
                port,
                post_req(
                    "/briefs/next",
                    &host,
                    &state.write_token,
                    &json!({"owner": pm, "week_ending": "2026-10-09"}),
                ),
            )
            .await
            .0,
            200
        );

        let (_, pm_body) = raw_parts(port, get_req(&format!("/briefs?who={pm}"), &host, &state.write_token)).await;
        let pm_json: serde_json::Value = serde_json::from_str(&pm_body).unwrap();
        let briefs = pm_json["briefs"].as_array().unwrap();
        assert_eq!(briefs.len(), 2);
        let draft = briefs.iter().find(|row| row["sent_at"].is_null()).unwrap();
        assert_eq!(draft["week_ending"], "2026-10-09");
        assert_eq!(draft["org_unit"], "Payments");
        assert_eq!(draft["partner_name"], "Sam Sample");
        assert_eq!(draft["prepared_by"], "Alex");
        assert!(draft["bottom_line"].is_null());
        assert!(draft["items"].as_array().unwrap().is_empty());
        let sent = briefs.iter().find(|row| row["sent_at"].is_string()).unwrap();
        assert_eq!(sent["week_ending"], "2026-10-02");
        assert_eq!(sent["bottom_line"], "Ship the parser fix.");
        let sent_items = sent["items"].as_array().unwrap();
        assert_eq!(sent_items.len(), 1);
        assert_eq!(sent_items[0]["item_id"], "esc-1");
        assert_eq!(sent_items[0]["kind"], "needs");
        assert_eq!(sent_items[0]["position"], 2);
        assert_eq!(sent_items[0]["headline"], "Parser is blocked");
        assert_eq!(sent_items[0]["what"], "Nested tables stop the parser.");
        assert_eq!(sent_items[0]["why"], "The partner cannot sign off.");
        assert_eq!(sent_items[0]["action"], "Ask for a decision.");
        // Needs items always store 0.
        assert_eq!(sent_items[0]["at_this_stage"], 0);

        let (status, body) = raw_parts(port, get_req("/briefs?who=dir@firm.example", &host, &state.write_token)).await;
        assert_eq!(status, 403);
        assert_eq!(
            error_text(&body),
            "The Director reads leadership updates, not the partner brief."
        );

        let (status, body) = raw_parts(port, get_req("/briefs?who=sam@firm.example", &host, &state.write_token)).await;
        assert_eq!(status, 200);
        let partner: serde_json::Value = serde_json::from_str(&body).unwrap();
        let rows = partner["briefs"].as_array().unwrap();
        assert_eq!(rows.len(), 1);
        assert!(rows.iter().all(|row| row["sent_at"].is_string()));
        assert_eq!(rows[0]["items"][0]["item_id"], "esc-1");

        let (_, ppm) = raw_parts(port, get_req("/briefs?who=iris@firm.example", &host, &state.write_token)).await;
        let ppm_json: serde_json::Value = serde_json::from_str(&ppm).unwrap();
        assert!(ppm_json["briefs"].as_array().unwrap().iter().any(|row| row["sent_at"].is_null()));

        let big = json!({"owner": pm, "week_ending": "2026-10-16", "pad": "x".repeat(UPDATES_MAX_BODY)});
        assert_eq!(raw_status(port, post_req("/briefs/next", &host, &state.write_token, &big)).await, 413);
        let plain = post_req(
            "/briefs/next",
            &host,
            &state.write_token,
            &json!({"owner": pm, "week_ending": "2026-10-16"}),
        )
        .replace("application/json", "text/plain");
        assert_eq!(raw_status(port, plain).await, 415);
    }

    #[tokio::test]
    async fn stale_item_expect_is_a_conflict() {
        let (state, _db) = state();
        let port = serve(&state).await;
        let host = format!("127.0.0.1:{port}");
        let pm = "mina@firm.example";
        let token = &state.write_token;
        let item = json!({"item_id": "pay-1", "category": "Now", "position": 0, "title": "Payments", "priority": 1});
        assert_eq!(
            raw_parts(port, post_req("/updates/next", &host, token, &json!({"owner": pm, "entry_date": "2026-10-01"}))).await.0,
            200
        );
        assert_eq!(
            raw_parts(port, post_req("/updates/item", &host, token, &json!({"owner": pm, "item": item}))).await.0,
            200
        );
        let mut other = item.clone();
        other["capacity"] = json!(10);
        let stale = json!({"item_id": "pay-1", "category": "Now", "position": 0, "title": "Stale", "priority": 1});
        let (status, body) = raw_parts(
            port,
            post_req("/updates/item", &host, token, &json!({"owner": pm, "item": other, "expect": stale})),
        )
        .await;
        assert_eq!(status, 409);
        assert_eq!(
            error_text(&body),
            "This item changed while you were editing. Reload and try again."
        );
        let (bad, bad_body) = raw_parts(
            port,
            post_req("/updates/item", &host, token, &json!({"owner": pm, "item": other, "expect": "nope"})),
        )
        .await;
        assert_eq!(bad, 400);
        assert_eq!(error_text(&bad_body), "expect must be an item object.");
        let foreign = json!({"item_id": "other-1", "category": "Now", "position": 0, "title": "Payments", "priority": 1});
        let (wrong, wrong_body) = raw_parts(
            port,
            post_req("/updates/item", &host, token, &json!({"owner": pm, "item": other, "expect": foreign})),
        )
        .await;
        assert_eq!(wrong, 400);
        assert_eq!(error_text(&wrong_body), "expect must describe the same item.");
        let (_, listed) = raw_parts(port, get_req(&format!("/updates?who={pm}"), &host, &state.write_token)).await;
        let listed: serde_json::Value = serde_json::from_str(&listed).unwrap();
        assert_eq!(listed["entries"][0]["items"][0]["title"], "Payments");
        assert_eq!(listed["entries"][0]["items"][0]["capacity"], serde_json::Value::Null);
    }

    fn ai_published_of(body: &str) -> bool {
        let v: serde_json::Value = serde_json::from_str(body).unwrap();
        let draft = v["entries"]
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["sent_at"].is_null())
            .unwrap();
        draft["ai_published"].as_bool().unwrap()
    }

    #[tokio::test]
    async fn ai_publish_and_run_follow_the_token_and_the_draft() {
        let (review, _db) = state();
        let port = serve(&review).await;
        let host = format!("127.0.0.1:{port}");
        let token = &review.write_token;
        let pm = "mina@firm.example";

        let (status, body) = raw_parts(port, post_req("/ai/publish", &host, token, &json!({}))).await;
        assert_eq!(status, 409);
        assert_eq!(error_text(&body), "There is no open draft.");

        let (status, _) = raw_parts(port, post_bare("/ai/run", &host, &json!({}))).await;
        assert_eq!(status, 401);
        let (status, body) = raw_parts(port, post_req("/ai/run", &host, token, &json!({}))).await;
        assert_eq!(status, 200);
        let run: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert!(run["outcome"].as_str().is_some_and(|outcome| !outcome.is_empty()));
        assert!(run["facts"].is_number() && run["accepted"].is_number() && run["rejected"].is_number());

        assert_eq!(
            raw_parts(port, post_req("/updates/next", &host, token, &json!({"owner": pm, "entry_date": "2026-10-01"}))).await.0,
            200
        );
        let (_, listed) = raw_parts(port, get_req(&format!("/updates?who={pm}"), &host, token)).await;
        assert!(!ai_published_of(&listed));

        let (status, body) = raw_parts(port, post_req("/ai/publish", &host, token, &json!({}))).await;
        assert_eq!(status, 200);
        let published = serde_json::from_str::<serde_json::Value>(&body).unwrap()["published_at"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(published.ends_with('Z'));
        let stored: String = review
            .conn
            .lock()
            .unwrap()
            .query_row(
                "SELECT published_at FROM lu_entries WHERE sent_at IS NULL",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(stored, published);
        let (_, listed) = raw_parts(port, get_req(&format!("/updates?who={pm}"), &host, token)).await;
        assert!(ai_published_of(&listed));

        {
            let conn = review.conn.lock().unwrap();
            conn.execute(
                "UPDATE lu_entries SET published_at='2020-01-01T00:00:00Z' WHERE sent_at IS NULL",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO ai_runs (started_at, mode, outcome) VALUES ('2024-01-01T00:00:00Z', 'leadership', 'drafted')",
                [],
            )
            .unwrap();
        }
        let (_, listed) = raw_parts(port, get_req(&format!("/updates?who={pm}"), &host, token)).await;
        assert!(!ai_published_of(&listed));
        let (status, body) = raw_parts(port, post_req("/ai/publish", &host, token, &json!({}))).await;
        assert_eq!(status, 200);
        let again = serde_json::from_str::<serde_json::Value>(&body).unwrap()["published_at"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(again.as_str() >= "2024-01-01T00:00:00Z");
        let (_, listed) = raw_parts(port, get_req(&format!("/updates?who={pm}"), &host, token)).await;
        assert!(ai_published_of(&listed));
        let rows: serde_json::Value = serde_json::from_str(&listed).unwrap();
        assert!(rows["entries"].as_array().unwrap().iter().any(|entry| entry["sent_at"].is_null()));

        let (mut auto_state, _auto_db) = state();
        let mut settings = (*auto_state.settings).clone();
        settings.ai_publish = "auto".into();
        auto_state.settings = Arc::new(settings);
        auto_state
            .conn
            .lock()
            .unwrap()
            .execute(
                "INSERT INTO lu_entries (entry_date, created_at, sent_at) VALUES ('2026-10-01', '2026-10-01T00:00:00Z', NULL)",
                [],
            )
            .unwrap();
        auto_state
            .conn
            .lock()
            .unwrap()
            .execute(
                "INSERT INTO ai_runs (started_at, mode, outcome) VALUES ('2999-01-01T00:00:00Z', 'leadership', 'drafted')",
                [],
            )
            .unwrap();
        let auto_port = serve(&auto_state).await;
        let auto_host = format!("127.0.0.1:{auto_port}");
        let (_, auto_body) = raw_parts(
            auto_port,
            get_req(&format!("/updates?who={pm}"), &auto_host, &auto_state.write_token),
        )
        .await;
        assert!(ai_published_of(&auto_body));
    }

    #[tokio::test]
    async fn ai_page_reads_runs_changes_and_open_flags() {
        let (state, _db) = state();
        let port = serve(&state).await;
        let host = format!("127.0.0.1:{port}");
        let token = &state.write_token;
        let pm = "mina@firm.example";
        let since = "2026-10-19T08:00:00Z";
        let changes_path = format!("/ai/changes?since={since}");

        for path in ["/ai/runs", changes_path.as_str(), "/ai/flags"] {
            token_denied(port, get_req(path, &host, ""), path).await;
        }
        token_denied(
            port,
            post_bare("/ai/flags/resolve", &host, &json!({"id": 1})),
            "/ai/flags/resolve",
        )
        .await;

        let (status, body) = raw_parts(
            port,
            get_req(&format!("/ai/changes?who={pm}&since=yesterday"), &host, token),
        )
        .await;
        assert_eq!(status, 400);
        assert_eq!(
            error_text(&body),
            "since must be a UTC time like 2026-10-19T08:00:00Z."
        );
        let (status, body) = raw_parts(port, get_req(&format!("/ai/changes?who={pm}"), &host, token)).await;
        assert_eq!(status, 400);
        assert_eq!(
            error_text(&body),
            "since must be a UTC time like 2026-10-19T08:00:00Z."
        );

        let flag_id: i64 = {
            let conn = state.conn.lock().unwrap();
            conn.execute_batch(
                "INSERT INTO stories (story_key, epic_key, summary, status) VALUES
                   ('S-1', 'PAY-100', 'Ship', 'todo'),
                   ('S-REJ', 'AUD-200', 'Nope', 'todo');
                 INSERT INTO pull_requests (id, repo, number, title, state) VALUES
                   ('pr-9', 'org/app', 9, 'Merge', 'merged');
                 INSERT INTO pr_story_links (pr_id, story_key, score, method, status, active) VALUES
                   ('pr-9', 'S-1', 1, 'manual', 'accepted', 1),
                   ('pr-9', 'S-REJ', 1, 'manual', 'rejected', 1);
                 INSERT INTO lu_item_sources (item_id, source_kind, source_key, created_at) VALUES
                   ('item-epic', 'epic', 'PAY-100', 't'),
                   ('item-story', 'story', 'S-1', 't'),
                   ('item-repo', 'repo', 'org/app', 't'),
                   ('item-rej', 'story', 'S-REJ', 't');
                 INSERT INTO activity_events (source, type, ref, occurred_at, payload) VALUES
                   ('detector', 'story_status', 'S-1', '2026-10-19T07:00:00Z', '{\"items\":[\"nope\"],\"title\":\"Early\"}'),
                   ('detector', 'story_status', 'S-1', '2026-10-19T08:00:00Z', '{\"items\":[\"nope\"],\"title\":\"Ship\"}'),
                   ('detector', 'pr_merged', 'pr-9', '2026-10-19T09:00:00Z', '{\"items\":[\"nope\"],\"title\":\"Merge\"}'),
                   ('github', 'push', 'pr-9', '2026-10-19T10:00:00Z', '{}');
                 INSERT INTO ai_flags (item_id, field, reason, raised_at, resolved_at) VALUES
                   ('pay-1', 'end_date', 'missing', '2026-10-19T08:00:00Z', NULL),
                   ('pay-2', 'title', 'old', '2026-10-19T07:00:00Z', '2026-10-19T07:30:00Z');",
            )
            .unwrap();
            for i in 0..101i64 {
                let at = format!("2026-10-19T{:02}:{:02}:00Z", i / 60, i % 60);
                conn.execute(
                    "INSERT INTO ai_runs (started_at, finished_at, mode, outcome, facts, accepted, rejected, model, detail)
                     VALUES (?1, ?1, 'leadership', 'drafted', ?2, 1, 0, 'm', 'do-not-leak')",
                    rusqlite::params![at, i],
                )
                .unwrap();
            }
            conn.query_row("SELECT id FROM ai_flags WHERE item_id='pay-1'", [], |row| row.get(0))
                .unwrap()
        };

        let (status, body) = raw_parts(port, get_req("/ai/runs?who=nora@firm.example", &host, token)).await;
        assert_eq!(status, 403);
        assert_eq!(error_text(&body), "Only the owner can read AI notes and sources.");

        let (status, body) = raw_parts(port, get_req(&format!("/ai/runs?who={pm}&limit=2"), &host, token)).await;
        assert_eq!(status, 200);
        assert!(!body.contains("do-not-leak"));
        let runs: serde_json::Value = serde_json::from_str(&body).unwrap();
        let runs = runs["runs"].as_array().unwrap();
        assert_eq!(runs.len(), 2);
        assert!(runs[0].get("detail").is_none());
        assert_eq!(runs[0]["facts"], 100);
        assert_eq!(runs[0]["started_at"], "2026-10-19T01:40:00Z");
        assert_eq!(runs[0]["mode"], "leadership");
        assert_eq!(runs[0]["outcome"], "drafted");
        assert_eq!(runs[0]["accepted"], 1);
        assert_eq!(runs[0]["rejected"], 0);
        assert_eq!(runs[0]["model"], "m");
        assert_eq!(runs[1]["facts"], 99);
        assert_eq!(runs[1]["started_at"], "2026-10-19T01:39:00Z");

        let (status, body) = raw_parts(port, get_req(&format!("/ai/runs?who={pm}"), &host, token)).await;
        assert_eq!(status, 200);
        let runs: serde_json::Value = serde_json::from_str(&body).unwrap();
        let runs = runs["runs"].as_array().unwrap();
        assert_eq!(runs.len(), 20);
        assert_eq!(runs[0]["facts"], 100);
        assert_eq!(runs[19]["facts"], 81);

        let (status, body) = raw_parts(port, get_req(&format!("/ai/runs?who={pm}&limit=500"), &host, token)).await;
        assert_eq!(status, 200);
        let runs: serde_json::Value = serde_json::from_str(&body).unwrap();
        let runs = runs["runs"].as_array().unwrap();
        assert_eq!(runs.len(), 100);
        assert_eq!(runs[0]["facts"], 100);
        assert_eq!(runs[99]["facts"], 1);

        let (status, body) = raw_parts(port, get_req(&format!("/ai/runs?who={pm}&limit=0"), &host, token)).await;
        assert_eq!(status, 400);
        assert_eq!(error_text(&body), "limit must be a whole number from 1 to 100.");

        let (status, body) = raw_parts(
            port,
            get_req(&format!("/ai/changes?who={pm}&since={since}"), &host, token),
        )
        .await;
        assert_eq!(status, 200);
        let changes: serde_json::Value = serde_json::from_str(&body).unwrap();
        let changes = changes["changes"].as_array().unwrap();
        assert_eq!(changes.len(), 2);
        assert_eq!(changes[0]["type"], "pr_merged");
        assert_eq!(changes[0]["ref"], "pr-9");
        assert_eq!(changes[0]["occurred_at"], "2026-10-19T09:00:00Z");
        assert!(changes[0]["payload"].is_object());
        assert_eq!(changes[0]["payload"]["title"], "Merge");
        assert_eq!(changes[0]["payload"]["items"], json!(["nope"]));
        assert_eq!(
            changes[0]["item_ids"],
            json!(["item-epic", "item-repo", "item-story"])
        );
        assert_eq!(changes[1]["type"], "story_status");
        assert_eq!(changes[1]["ref"], "S-1");
        assert_eq!(changes[1]["occurred_at"], since);
        assert_eq!(changes[1]["item_ids"], json!(["item-epic", "item-story"]));

        let (status, body) = raw_parts(port, get_req(&format!("/ai/flags?who={pm}"), &host, token)).await;
        assert_eq!(status, 200);
        let flags: serde_json::Value = serde_json::from_str(&body).unwrap();
        let flags = flags["flags"].as_array().unwrap();
        assert_eq!(flags.len(), 1);
        assert_eq!(flags[0]["id"], flag_id);
        assert_eq!(flags[0]["item_id"], "pay-1");
        assert_eq!(flags[0]["field"], "end_date");
        assert_eq!(flags[0]["reason"], "missing");
        assert_eq!(flags[0]["raised_at"], "2026-10-19T08:00:00Z");

        let resolve = json!({"owner": pm, "id": flag_id});
        let (status, body) = raw_parts(port, post_req("/ai/flags/resolve", &host, token, &resolve)).await;
        assert_eq!(status, 200);
        assert_eq!(serde_json::from_str::<serde_json::Value>(&body).unwrap()["resolved"], true);
        let (status, body) = raw_parts(port, get_req(&format!("/ai/flags?who={pm}"), &host, token)).await;
        assert_eq!(status, 200);
        assert!(serde_json::from_str::<serde_json::Value>(&body).unwrap()["flags"]
            .as_array()
            .unwrap()
            .is_empty());
        let (status, body) = raw_parts(port, post_req("/ai/flags/resolve", &host, token, &resolve)).await;
        assert_eq!(status, 404);
        assert_eq!(error_text(&body), "No such open flag.");
        let (status, body) = raw_parts(
            port,
            post_req("/ai/flags/resolve", &host, token, &json!({"owner": pm, "id": 999})),
        )
        .await;
        assert_eq!(status, 404);
        assert_eq!(error_text(&body), "No such open flag.");
        let (status, body) = raw_parts(
            port,
            post_req("/ai/flags/resolve", &host, token, &json!({"owner": pm, "id": "1"})),
        )
        .await;
        assert_eq!(status, 400);
        assert_eq!(error_text(&body), "id must be a whole number.");
    }

    #[test]
    fn ai_status_json_maps_every_field() {
        assert_eq!(
            ai_status_json(true, "review", "fake", None),
            json!({"scheduled": "off", "publish_mode": "review", "model": "fake", "due": null})
        );
        assert_eq!(
            ai_status_json(false, "auto", "fake", Some("weekend")),
            json!({"scheduled": "on", "publish_mode": "auto", "model": "fake", "due": "weekend"})
        );
    }

    #[tokio::test]
    async fn ai_status_route_needs_the_token_and_the_owner() {
        let (state, _db) = state();
        let port = serve(&state).await;
        let host = format!("127.0.0.1:{port}");
        let token = &state.write_token;

        token_denied(port, get_req("/ai/status", &host, ""), "/ai/status").await;

        let (status, body) =
            raw_parts(port, get_req("/ai/status?who=nora@firm.example", &host, token)).await;
        assert_eq!(status, 403);
        assert_eq!(error_text(&body), "Only the owner can read AI notes and sources.");

        let (status, body) =
            raw_parts(port, get_req("/ai/status?who=mina@firm.example", &host, token)).await;
        assert_eq!(status, 200);
        let value: serde_json::Value = serde_json::from_str(&body).unwrap();
        for key in ["scheduled", "publish_mode", "model", "due"] {
            assert!(value.get(key).is_some(), "{key}");
        }
        assert_eq!(value["model"], "fake");
        assert_eq!(value["publish_mode"], "review");
    }

}
