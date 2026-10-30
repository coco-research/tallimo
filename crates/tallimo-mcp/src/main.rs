//! Tallimo over stdio: one JSON-RPC 2.0 message per line.
//!
//! ponytail: hand-rolled for four methods. Move to the official Rust MCP SDK
//! if we need resources, prompts or streaming.

use serde_json::{json, Value};
use std::io::{BufRead, Write};

struct Server {
    base: String,
    owner: String,
    token: String,
    token_gap: Option<String>,
    client: reqwest::blocking::Client,
    #[cfg(test)]
    before_reread: std::cell::RefCell<Option<Box<dyn FnOnce()>>>,
}

impl Server {
    fn new(base: impl Into<String>, owner: impl Into<String>, token: impl Into<String>) -> Self {
        let client = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(20))
            // Never follow a redirect: reqwest would carry X-Tallimo-Token to the new host.
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("http client");
        Self {
            base: base.into().trim().trim_end_matches('/').to_string(),
            owner: owner.into(),
            token: token.into(),
            token_gap: None,
            client,
            #[cfg(test)]
            before_reread: std::cell::RefCell::new(None),
        }
    }

    fn from_env() -> Self {
        let base = match std::env::var("TALLIMO_URL") {
            Ok(url) if !url.is_empty() => url,
            _ => match std::env::var("TALLIMO_PORT") {
                Ok(port) if !port.is_empty() => format!("http://127.0.0.1:{port}"),
                _ => "http://127.0.0.1:8787".into(),
            },
        };
        let owner = std::env::var("TALLIMO_OWNER").unwrap_or_default();
        let path = rollup_core::token::path_for(&rollup_core::Settings::load().db_path);
        let (token, token_gap) = match std::fs::read_to_string(&path) {
            Ok(raw) => (raw.trim().to_string(), None),
            Err(e) => {
                eprintln!("{}: {e}", path.display());
                (String::new(), Some(format!(
                    "Tallimo write token not found at {}. Start the server once, or set ROLLUP_DB_PATH.",
                    path.display()
                )))
            }
        };
        let mut server = Self::new(base, owner, token);
        server.token_gap = token_gap;
        server
    }

    fn send(&self, req: reqwest::blocking::RequestBuilder) -> Result<Value, String> {
        if !url_keeps_token_home(&self.base) {
            return Err("TALLIMO_URL must be a loopback address or https.".into());
        }
        let resp = req
            .header("X-Tallimo-Token", &self.token)
            .send()
            .map_err(|e| e.to_string())?;
        let ok = resp.status().is_success();
        let text = resp.text().map_err(|e| e.to_string())?;
        if !ok {
            let msg = serde_json::from_str::<Value>(&text)
                .ok()
                .and_then(|v| v["error"].as_str().map(str::to_string))
                .unwrap_or(text);
            return Err(msg);
        }
        serde_json::from_str(&text).map_err(|e| e.to_string())
    }

    fn updates(&self) -> Result<Value, String> {
        self.send(
            self.client
                .get(format!("{}/updates", self.base))
                .query(&[("who", &self.owner)]),
        )
    }

    fn post(&self, path: &str, body: &Value) -> Result<Value, String> {
        if let Some(msg) = &self.token_gap {
            return Err(msg.clone());
        }
        self.send(self.client.post(format!("{}{path}", self.base)).json(body))
    }

    fn post_item(&self, item: &Value, expect: &Value) -> Result<(), String> {
        self.post(
            "/updates/item",
            &json!({"owner": &self.owner, "item": item, "expect": expect}),
        )
        .map(|_| ())
    }
}

fn url_keeps_token_home(url: &str) -> bool {
    let url = url.trim();
    if url.get(..8).is_some_and(|head| head.eq_ignore_ascii_case("https://")) {
        let rest = &url[8..];
        return !rest.is_empty() && !rest.starts_with(['/', ':']);
    }
    if !url.get(..7).is_some_and(|head| head.eq_ignore_ascii_case("http://")) {
        return false;
    }
    let rest = url[7..].to_ascii_lowercase();
    ["127.0.0.1", "localhost", "[::1]"].into_iter().any(|host| {
        let Some(after) = rest.strip_prefix(host) else { return false };
        after.is_empty() || after.starts_with(['/', '?', '#']) || http_port(after)
    })
}

fn http_port(after: &str) -> bool {
    let Some(tail) = after.strip_prefix(':') else { return false };
    let port = tail.split(['/', '?', '#']).next().unwrap_or("");
    (1..=5).contains(&port.len())
        && port.bytes().all(|b| b.is_ascii_digit())
        && port.parse::<u16>().is_ok_and(|n| n > 0)
}

fn dispatch_line(line: &str, server: &Server) -> Option<String> {
    let v: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(_) => {
            return Some(err_line(&Value::Null, -32700, "Parse error"));
        }
    };
    // A notification has no id. Nothing is written back.
    if !v.as_object().is_some_and(|o| o.contains_key("id")) || v["id"].is_null() {
        return None;
    }
    let id = v["id"].clone();
    let result = match v["method"].as_str().unwrap_or("") {
        "initialize" => json!({
            "protocolVersion": v["params"]["protocolVersion"].as_str().unwrap_or(""),
            "serverInfo": {"name": "tallimo", "version": env!("CARGO_PKG_VERSION")},
            "capabilities": {"tools": {}}
        }),
        "ping" => json!({}),
        "tools/list" => json!({"tools": tools()}),
        "tools/call" => call_tool(server, &v["params"]),
        _ => return Some(err_line(&id, -32601, "Method not found")),
    };
    Some(json!({"jsonrpc": "2.0", "id": id, "result": result}).to_string())
}

fn err_line(id: &Value, code: i64, message: &str) -> String {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}}).to_string()
}

fn tools() -> Vec<Value> {
    let field = json!({
        "type": "string",
        "enum": ["title", "pct", "capacity", "start_date", "end_date", "completed", "next", "future"]
    });
    vec![
        json!({
            "name": "tallimo_today",
            "description": "Show the live draft and the last sent update.",
            "inputSchema": {"type": "object", "properties": {}, "additionalProperties": false}
        }),
        json!({
            "name": "tallimo_edit_item",
            "description": "Change one field on an item in the open draft.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "item_id": {"type": "string"},
                    "field": field,
                    "value": {"type": "string"}
                },
                "required": ["item_id", "field", "value"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "tallimo_note",
            "description": "Save a note for this owner. A glossary note uses \"heard => correct\". A rule uses drop:<item_id>, no_end_date:<item_id>, never_say:<phrase>. An escalate note asks for an item on the partner brief: say what needs the Senior Partner and why.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "kind": {"type": "string", "enum": ["fact", "glossary", "rule", "escalate"]},
                    "body": {"type": "string"}
                },
                "required": ["kind", "body"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "tallimo_map",
            "description": "Link one leadership item to an epic, story, or repo.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "item_id": {"type": "string"},
                    "kind": {"type": "string", "enum": ["epic", "story", "repo"]},
                    "key": {"type": "string"}
                },
                "required": ["item_id", "kind", "key"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "tallimo_publish",
            "description": "Publish the open draft.",
            "inputSchema": {"type": "object", "properties": {}, "additionalProperties": false}
        }),
        json!({
            "name": "tallimo_run_now",
            "description": "Run one AI pass now.",
            "inputSchema": {"type": "object", "properties": {}, "additionalProperties": false}
        }),
        json!({
            "name": "tallimo_flags",
            "description": "List open flags.",
            "inputSchema": {"type": "object", "properties": {}, "additionalProperties": false}
        }),
        json!({
            "name": "tallimo_resolve_flag",
            "description": "Resolve one open flag.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "id": {"type": "integer"}
                },
                "required": ["id"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "tallimo_changes",
            "description": "Detector changes since a UTC time; default: the last 24 hours.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "since": {"type": "string"}
                },
                "additionalProperties": false
            }
        }),
    ]
}

fn call_tool(server: &Server, params: &Value) -> Value {
    let name = params["name"].as_str().unwrap_or("");
    let args = params.get("arguments").cloned().unwrap_or_else(|| json!({}));
    let outcome = match name {
        "tallimo_today" => today(server),
        "tallimo_edit_item" => edit_item(server, &args),
        "tallimo_note" => note(server, &args),
        "tallimo_map" => map_item(server, &args),
        "tallimo_publish" => publish_draft(server),
        "tallimo_run_now" => run_now(server),
        "tallimo_flags" => flags(server),
        "tallimo_resolve_flag" => resolve_flag(server, &args),
        "tallimo_changes" => changes(server, &args),
        _ => Err("Unknown tool.".into()),
    };
    match outcome {
        Ok(text) => tool_result(text, false),
        Err(text) => tool_result(text, true),
    }
}

fn tool_result(text: String, is_error: bool) -> Value {
    json!({"content": [{"type": "text", "text": text}], "isError": is_error})
}

fn today(server: &Server) -> Result<String, String> {
    if server.owner.is_empty() {
        return Err("Set TALLIMO_OWNER to your Tallimo id.".into());
    }
    Ok(format_today(&server.updates()?))
}

fn edit_item(server: &Server, args: &Value) -> Result<String, String> {
    if server.owner.is_empty() {
        return Err("Set TALLIMO_OWNER to your Tallimo id.".into());
    }
    let item_id = args["item_id"].as_str().unwrap_or("");
    let field = args["field"].as_str().unwrap_or("");
    let value = match args.get("value") {
        Some(Value::String(s)) => s.as_str(),
        Some(Value::Null) | None => "",
        Some(_) => return Err("value must be text.".into()),
    };
    if item_id.is_empty() {
        return Err("item_id is required.".into());
    }
    let mut item = open_item(&server.updates()?, item_id)?;
    let seen = item.clone();
    apply_field(&mut item, field, value)?;
    #[cfg(test)]
    if let Some(hook) = server.before_reread.borrow_mut().take() {
        hook();
    }
    server.post_item(&item, &seen)?;
    Ok(format!("Saved {field} on {item_id}."))
}

fn note(server: &Server, args: &Value) -> Result<String, String> {
    if server.owner.is_empty() {
        return Err("Set TALLIMO_OWNER to your Tallimo id.".into());
    }
    let kind = args["kind"].as_str().unwrap_or("");
    let body = args["body"].as_str().unwrap_or("");
    let saved = server.post(
        "/ai/notes",
        &json!({"owner": &server.owner, "kind": kind, "body": body}),
    )?;
    let Some(id) = saved["id"].as_i64() else {
        return Err("The server returned an unexpected response for the note.".into());
    };
    Ok(format!("Noted ({kind} #{id})."))
}

fn map_item(server: &Server, args: &Value) -> Result<String, String> {
    if server.owner.is_empty() {
        return Err("Set TALLIMO_OWNER to your Tallimo id.".into());
    }
    let item_id = args["item_id"].as_str().unwrap_or("");
    let kind = args["kind"].as_str().unwrap_or("");
    let key = args["key"].as_str().unwrap_or("");
    server.post(
        "/ai/map",
        &json!({"owner": &server.owner, "item_id": item_id, "kind": kind, "key": key}),
    )?;
    Ok(format!("Linked {item_id} to {kind} {key}."))
}

fn publish_draft(server: &Server) -> Result<String, String> {
    if server.owner.is_empty() {
        return Err("Set TALLIMO_OWNER to your Tallimo id.".into());
    }
    let saved = server.post("/ai/publish", &json!({"owner": &server.owner}))?;
    let Some(at) = saved["published_at"].as_str() else {
        return Err("The server returned an unexpected response for publish.".into());
    };
    Ok(format!("Published at {at}."))
}

fn run_now(server: &Server) -> Result<String, String> {
    if server.owner.is_empty() {
        return Err("Set TALLIMO_OWNER to your Tallimo id.".into());
    }
    let run = server.post("/ai/run", &json!({"owner": &server.owner}))?;
    let Some(outcome) = run["outcome"].as_str() else {
        return Err("The server returned an unexpected response for the run.".into());
    };
    let count = |key: &str| run[key].as_u64().unwrap_or(0);
    Ok(format!(
        "{outcome} facts={} accepted={} rejected={}",
        count("facts"),
        count("accepted"),
        count("rejected"),
    ))
}

fn flags(server: &Server) -> Result<String, String> {
    if server.owner.is_empty() {
        return Err("Set TALLIMO_OWNER to your Tallimo id.".into());
    }
    let body = server.send(
        server
            .client
            .get(format!("{}/ai/flags", server.base))
            .query(&[("who", &server.owner)]),
    )?;
    format_flags(&body)
}

fn resolve_flag(server: &Server, args: &Value) -> Result<String, String> {
    if server.owner.is_empty() {
        return Err("Set TALLIMO_OWNER to your Tallimo id.".into());
    }
    let Some(id) = args["id"].as_i64() else {
        return Err("id must be a whole number.".into());
    };
    server.post(
        "/ai/flags/resolve",
        &json!({"owner": &server.owner, "id": id}),
    )?;
    Ok(format!("Resolved flag {id}."))
}

fn changes(server: &Server, args: &Value) -> Result<String, String> {
    if server.owner.is_empty() {
        return Err("Set TALLIMO_OWNER to your Tallimo id.".into());
    }
    let since_arg = args.get("since");
    if let Some(value) = since_arg {
        if !value.is_null() && !value.is_string() {
            return Err("since must be a UTC time like 2026-10-19T08:00:00Z.".into());
        }
    }
    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock before epoch")
        .as_secs();
    let since = changes_since(since_arg, now_secs);
    let body = server.send(
        server
            .client
            .get(format!("{}/ai/changes", server.base))
            .query(&[("who", server.owner.as_str()), ("since", since.as_str())]),
    )?;
    format_changes(&body)
}

fn changes_since(arg: Option<&Value>, now_secs: u64) -> String {
    if let Some(Value::String(text)) = arg {
        if !text.is_empty() {
            return text.clone();
        }
    }
    // ponytail: std has no local offset, so the default is now minus 24h in UTC. Use a TZ crate if the host offset becomes available.
    rollup_core::store::format_epoch(now_secs.saturating_sub(86_400))
}

fn format_flags(body: &Value) -> Result<String, String> {
    let Some(flags) = body["flags"].as_array() else {
        return Err("The server returned an unexpected response for flags.".into());
    };
    if flags.is_empty() {
        return Ok("No open flags.".into());
    }
    let lines = flags
        .iter()
        .map(|flag| {
            format!(
                "{} · {} · {} · {} · {}",
                blank_num(&flag["id"]),
                flag["item_id"].as_str().unwrap_or(""),
                flag["field"].as_str().unwrap_or(""),
                flag["reason"].as_str().unwrap_or(""),
                flag["raised_at"].as_str().unwrap_or("")
            )
        })
        .collect::<Vec<_>>();
    Ok(lines.join("\n"))
}

fn format_changes(body: &Value) -> Result<String, String> {
    let Some(changes) = body["changes"].as_array() else {
        return Err("The server returned an unexpected response for changes.".into());
    };
    if changes.is_empty() {
        return Ok("No changes.".into());
    }
    let lines = changes
        .iter()
        .map(|change| {
            let items = change["item_ids"]
                .as_array()
                .map(|ids| {
                    ids.iter()
                        .filter_map(|id| id.as_str())
                        .collect::<Vec<_>>()
                        .join(",")
                })
                .unwrap_or_default();
            format!(
                "{} · {} · {} · items: {}",
                change["occurred_at"].as_str().unwrap_or(""),
                change["type"].as_str().unwrap_or(""),
                change["ref"].as_str().unwrap_or(""),
                items
            )
        })
        .collect::<Vec<_>>();
    Ok(lines.join("\n"))
}

fn open_item(body: &Value, item_id: &str) -> Result<Value, String> {
    let entries = body["entries"].as_array().ok_or("There is no open draft.")?;
    let draft = entries
        .iter()
        .find(|entry| entry["sent_at"].is_null())
        .ok_or("There is no open draft.")?;
    draft["items"]
        .as_array()
        .and_then(|items| items.iter().find(|item| item["item_id"].as_str() == Some(item_id)))
        .cloned()
        .ok_or("No such item on the draft.".into())
}

fn apply_field(item: &mut Value, field: &str, value: &str) -> Result<(), String> {
    match field {
        "title" => item["title"] = json!(value),
        "pct" | "capacity" => {
            item[field] = if value.is_empty() {
                Value::Null
            } else {
                let n: i64 = value.parse().map_err(|_| format!("{field} must be a whole number."))?;
                json!(n)
            };
        }
        "start_date" | "end_date" => {
            item[field] = if value.is_empty() { Value::Null } else { json!(value) };
        }
        "completed" | "next" | "future" => {
            let rows: Vec<&str> = value.lines().map(str::trim).filter(|line| !line.is_empty()).collect();
            item[field] = json!(rows);
        }
        _ => {
            return Err(
                "field must be title, pct, capacity, start_date, end_date, completed, next or future.".into(),
            )
        }
    }
    Ok(())
}

fn format_today(body: &Value) -> String {
    let empty = Vec::new();
    let entries = body["entries"].as_array().unwrap_or(&empty);
    let mut blocks = Vec::new();
    if let Some(draft) = entries.iter().find(|entry| entry["sent_at"].is_null()) {
        blocks.push(format_entry("Live draft", draft));
    }
    if let Some(sent) = entries.iter().find(|entry| entry["sent_at"].is_string()) {
        blocks.push(format_entry("Last sent", sent));
    }
    if blocks.is_empty() { "No updates.".into() } else { blocks.join("\n") }
}

fn format_entry(label: &str, entry: &Value) -> String {
    let cap = format!(
        "{} {}",
        blank_num(&entry["total"]),
        entry["label"].as_str().unwrap_or("")
    );
    let mut lines = vec![
        label.to_string(),
        entry["entry_date"].as_str().unwrap_or("").to_string(),
        cap.trim().to_string(),
    ];
    for item in entry["items"].as_array().into_iter().flatten() {
        lines.push(format!(
            "{} · {}: {}% complete, capacity {}%",
            item["category"].as_str().unwrap_or(""),
            item["title"].as_str().unwrap_or(""),
            blank_num(&item["pct"]),
            blank_num(&item["capacity"])
        ));
        for (key, prefix) in [("completed", "done"), ("next", "next"), ("future", "future")] {
            for row in item[key].as_array().into_iter().flatten() {
                if let Some(text) = row.as_str().filter(|text| !text.is_empty()) {
                    lines.push(format!("{prefix}: {text}"));
                }
            }
        }
    }
    lines.join("\n")
}

fn blank_num(v: &Value) -> String {
    match v {
        Value::Number(n) => n.to_string(),
        _ => String::new(),
    }
}

fn main() {
    let server = Server::from_env();
    let stdin = std::io::stdin();
    let mut out = std::io::stdout().lock();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        if let Some(reply) = dispatch_line(&line, &server) {
            if writeln!(out, "{reply}").is_err() || out.flush().is_err() {
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server() -> Server {
        Server::new("http://127.0.0.1:9", "", "token")
    }

    fn rpc(server: &Server, line: &str) -> Option<Value> {
        dispatch_line(line, server).map(|text| serde_json::from_str(&text).unwrap())
    }

    #[test]
    fn dispatch_covers_initialize_tools_unknown_and_notifications() {
        let server = server();
        let init = rpc(
            &server,
            r#"{"jsonrpc":"2.0","id":7,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}"#,
        )
        .unwrap();
        assert_eq!(init["id"], 7);
        assert_eq!(init["result"]["protocolVersion"], "2025-06-18");
        assert_eq!(init["result"]["serverInfo"]["name"], "tallimo");
        assert_eq!(init["result"]["serverInfo"]["version"], env!("CARGO_PKG_VERSION"));
        assert!(init["result"]["capabilities"]["tools"].is_object());

        let list = rpc(&server, r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#).unwrap();
        let names: Vec<_> = list["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| tool["name"].as_str().unwrap())
            .collect();
        assert_eq!(
            names,
            vec![
                "tallimo_today",
                "tallimo_edit_item",
                "tallimo_note",
                "tallimo_map",
                "tallimo_publish",
                "tallimo_run_now",
                "tallimo_flags",
                "tallimo_resolve_flag",
                "tallimo_changes",
            ]
        );
        let changes_tool = list["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|tool| tool["name"] == "tallimo_changes")
            .unwrap();
        assert_eq!(
            changes_tool["description"],
            "Detector changes since a UTC time; default: the last 24 hours."
        );
        let note = list["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|tool| tool["name"] == "tallimo_note")
            .unwrap();
        let desc = note["description"].as_str().unwrap();
        assert!(desc.contains("heard => correct"), "{desc}");
        assert!(
            desc.contains("drop:<item_id>, no_end_date:<item_id>, never_say:<phrase>"),
            "{desc}"
        );

        let unknown = rpc(&server, r#"{"jsonrpc":"2.0","id":"x","method":"resources/list"}"#).unwrap();
        assert_eq!(unknown["error"]["code"], -32601);
        assert_eq!(unknown["id"], "x");

        assert!(rpc(&server, r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#).is_none());

        let missing = rpc(
            &server,
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"tallimo_edit_item","arguments":{"item_id":"demo-vendor","field":"pct","value":"55"}}}"#,
        )
        .unwrap();
        assert_eq!(missing["result"]["isError"], true);
        assert_eq!(
            missing["result"]["content"][0]["text"],
            "Set TALLIMO_OWNER to your Tallimo id."
        );

        let today = rpc(&server, r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"tallimo_today","arguments":{}}}"#).unwrap();
        assert_eq!(today["result"]["content"][0]["text"], "Set TALLIMO_OWNER to your Tallimo id.");
    }

    #[test]
    fn changes_since_uses_the_argument_or_now_minus_24h() {
        let now = 1_760_832_000;
        let day_ago = rollup_core::store::format_epoch(now - 86_400);
        assert_eq!(changes_since(None, now), day_ago);
        assert_eq!(changes_since(Some(&Value::Null), now), day_ago);
        assert_eq!(changes_since(Some(&json!("")), now), day_ago);
        assert_eq!(
            changes_since(Some(&json!("2026-10-19T08:00:00Z")), now),
            "2026-10-19T08:00:00Z"
        );
    }

    #[test]
    fn cleartext_tallimo_url_must_be_loopback() {
        for url in ["http://127.0.0.1", "http://127.0.0.1:8787", "http://localhost:9", "http://[::1]:8787", "https://example.com", "https://example.com:443/updates"] {
            assert!(url_keeps_token_home(url), "{url}");
        }
        for url in ["http://example.com", "http://example.com:8787", "http://10.0.0.8", "http://127.0.0.1.evil"] {
            assert!(!url_keeps_token_home(url), "{url}");
        }
        let refused = rpc(&Server::new("http://example.com", "mina@firm.example", "secret"), r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"tallimo_today","arguments":{}}}"#).unwrap();
        assert_eq!(refused["result"]["isError"], true);
        assert_eq!(refused["result"]["content"][0]["text"], "TALLIMO_URL must be a loopback address or https.");
    }

    struct Tmp(std::path::PathBuf);

    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn edit_item_sets_pct_on_the_open_draft() {
        let dir = std::env::temp_dir().join(format!("tallimo-mcp-it-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let _tmp = Tmp(dir.clone());
        let db = dir.join("rollup.db");
        let (port, token) = rollup_api::serve_temp(&db);
        let base = format!("http://127.0.0.1:{port}");
        let http = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(20))
            .build()
            .unwrap();
        let authed = |builder: reqwest::blocking::RequestBuilder| builder.header("X-Tallimo-Token", &token);

        let synced = authed(http.post(format!("{base}/sync")).json(&json!({})))
            .send()
            .unwrap();
        assert!(synced.status().is_success(), "{}", synced.text().unwrap());

        let server = Server::new(&base, "mina@firm.example", &token);
        let early = rpc(
            &server,
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"tallimo_edit_item","arguments":{"item_id":"demo-vendor","field":"pct","value":"55"}}}"#,
        )
        .unwrap();
        assert_eq!(early["result"]["content"][0]["text"], "There is no open draft.");

        let opened = authed(
            http.post(format!("{base}/updates/next"))
                .json(&json!({"owner": "mina@firm.example", "entry_date": "2029-04-06"})),
        )
        .send()
        .unwrap();
        assert!(opened.status().is_success(), "{}", opened.text().unwrap());

        let saved = rpc(
            &server,
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"tallimo_edit_item","arguments":{"item_id":"demo-vendor","field":"pct","value":"55"}}}"#,
        )
        .unwrap();
        assert_eq!(saved["result"]["isError"], false);
        assert_eq!(saved["result"]["content"][0]["text"], "Saved pct on demo-vendor.");

        let listed: Value = authed(http.get(format!("{base}/updates")).query(&[("who", "mina@firm.example")]))
            .send()
            .unwrap()
            .json()
            .unwrap();
        let draft = listed["entries"]
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["sent_at"].is_null())
            .unwrap();
        let item = draft["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["item_id"] == "demo-vendor")
            .unwrap();
        assert_eq!(item["pct"], 55);

        let text = rpc(&server, r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"tallimo_today","arguments":{}}}"#)
            .unwrap();
        let text = text["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("Live draft"), "{text}");
        assert!(text.contains("55% complete"), "{text}");
        assert!(text.contains("Last sent"), "{text}");

        let raced_item = item.clone();
        let (hook_http, hook_base, hook_token) = (http.clone(), base.clone(), token.clone());
        *server.before_reread.borrow_mut() = Some(Box::new(move || {
            let mut raced = raced_item;
            raced["title"] = json!("Atlas raced");
            hook_http.post(format!("{hook_base}/updates/item")).header("X-Tallimo-Token", &hook_token)
                .json(&json!({"owner": "mina@firm.example", "item": raced})).send().unwrap();
        }));
        let raced = rpc(&server, r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"tallimo_edit_item","arguments":{"item_id":"demo-vendor","field":"capacity","value":"12"}}}"#).unwrap();
        assert_eq!(raced["result"]["content"][0]["text"], "This item changed while you were editing. Reload and try again.");
        let after: Value = authed(http.get(format!("{base}/updates")).query(&[("who", "mina@firm.example")])).send().unwrap().json().unwrap();
        let after = after["entries"].as_array().unwrap().iter().find(|e| e["sent_at"].is_null()).unwrap()["items"].as_array().unwrap().iter().find(|r| r["item_id"] == "demo-vendor").unwrap();
        assert_eq!((after["title"].as_str(), after["pct"].as_i64(), after["capacity"].as_i64()), (Some("Atlas raced"), Some(55), Some(25)));
    }

    #[test]
    fn note_and_map_against_the_api() {
        let dir = std::env::temp_dir().join(format!("tallimo-mcp-note-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let _tmp = Tmp(dir.clone());
        let db = dir.join("rollup.db");
        let (port, token) = rollup_api::serve_temp(&db);
        let base = format!("http://127.0.0.1:{port}");
        let http = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(20))
            .build()
            .unwrap();
        let synced = http
            .post(format!("{base}/sync"))
            .header("X-Tallimo-Token", &token)
            .json(&json!({}))
            .send()
            .unwrap();
        assert!(synced.status().is_success(), "{}", synced.text().unwrap());

        let server = Server::new(&base, "mina@firm.example", &token);
        let noted = rpc(
            &server,
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"tallimo_note","arguments":{"kind":"glossary","body":"Pelix => Felix"}}}"#,
        )
        .unwrap();
        assert_eq!(noted["result"]["isError"], false);
        assert_eq!(noted["result"]["content"][0]["text"], "Noted (glossary #1).");

        let linked = rpc(
            &server,
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"tallimo_map","arguments":{"item_id":"pay-1","kind":"story","key":"PAY-100"}}}"#,
        )
        .unwrap();
        assert_eq!(linked["result"]["isError"], false);
        assert_eq!(linked["result"]["content"][0]["text"], "Linked pay-1 to story PAY-100.");

        let bad = rpc(
            &server,
            r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"tallimo_note","arguments":{"kind":"glossary","body":"Pelix"}}}"#,
        )
        .unwrap();
        assert_eq!(bad["result"]["isError"], true);
        assert_eq!(
            bad["result"]["content"][0]["text"],
            "A glossary note looks like \"heard => correct\" with both sides filled in."
        );
    }

    #[test]
    fn flags_list_then_resolve() {
        let dir = std::env::temp_dir().join(format!("tallimo-mcp-flags-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let _tmp = Tmp(dir.clone());
        let db = dir.join("rollup.db");
        let (port, token) = rollup_api::serve_temp(&db);
        let base = format!("http://127.0.0.1:{port}");
        let http = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(20))
            .build()
            .unwrap();
        let synced = http
            .post(format!("{base}/sync"))
            .header("X-Tallimo-Token", &token)
            .json(&json!({}))
            .send()
            .unwrap();
        assert!(synced.status().is_success(), "{}", synced.text().unwrap());

        rollup_core::store::connect(&db)
            .unwrap()
            .execute_batch(
                "INSERT INTO ai_flags (item_id, field, reason, raised_at) VALUES ('pay-1', 'end_date', 'missing', '2026-10-19T08:00:00Z')",
            )
            .unwrap();

        let server = Server::new(&base, "mina@firm.example", &token);
        let listed = rpc(
            &server,
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"tallimo_flags","arguments":{}}}"#,
        )
        .unwrap();
        assert_eq!(listed["result"]["isError"], false);
        let listed_text = listed["result"]["content"][0]["text"].as_str().unwrap();
        let id: i64 = listed_text.split(" · ").next().unwrap().parse().unwrap();
        assert_eq!(
            listed_text,
            format!("{id} · pay-1 · end_date · missing · 2026-10-19T08:00:00Z")
        );

        let resolved = rpc(
            &server,
            &json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "tools/call",
                "params": {"name": "tallimo_resolve_flag", "arguments": {"id": id}}
            })
            .to_string(),
        )
        .unwrap();
        assert_eq!(resolved["result"]["isError"], false);
        assert_eq!(resolved["result"]["content"][0]["text"], format!("Resolved flag {id}."));

        let gone = rpc(
            &server,
            r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"tallimo_flags","arguments":{}}}"#,
        )
        .unwrap();
        assert_eq!(gone["result"]["isError"], false);
        assert_eq!(gone["result"]["content"][0]["text"], "No open flags.");

        let missing = rpc(
            &server,
            &json!({
                "jsonrpc": "2.0",
                "id": 4,
                "method": "tools/call",
                "params": {"name": "tallimo_resolve_flag", "arguments": {"id": id}}
            })
            .to_string(),
        )
        .unwrap();
        assert_eq!(missing["result"]["isError"], true);
        assert_eq!(missing["result"]["content"][0]["text"], "No such open flag.");
    }
}
