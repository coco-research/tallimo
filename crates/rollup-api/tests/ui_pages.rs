// Page contract for the app shell. A missing route name fails this check.
#[test]
fn ui_has_the_app_pages() {
    let html = include_str!("../../../ui/index.html");
    for needle in [
        "data-build: app1",
        "#/today",
        "#/inbox",
        "#/people",
        "#/projects",
        "#/activity",
        "#/digest",
        "#/settings",
        "#/room",
        "#/update",
        "#/desk",
        "#/connect",
        "function renderToday",
        "function renderInbox",
        "function renderPerson",
        "function renderProject",
        "function renderStory",
        "function renderActivity",
        "function renderDigest",
        "function renderSearch",
        "function renderSettings",
        "function renderRoom",
        "function renderUpdate",
        "function renderDesk",
        "function renderConnect",
        "function gate",
        "function renderOnboard",
        "function land",
        "function noteWindows",
        "function writeLetter",
        "rollup.range",
        "merged, no date.",
        "function addFlags",
        "function sendFlag",
        "postJson('/feedback'",
        "Open Tallimo from its private link",
        "data-access-copy",
        "./rollup-api open",
    ] {
        assert!(html.contains(needle), "missing {needle}");
    }
    let mut at = 0usize;
    for href in [
        "/signal/fonts.css",
        "/signal/layout.css",
        "/signal/signal.css",
        "/signal/color.css",
        "/signal/consult.css",
    ] {
        let found = html.find(href).unwrap_or_else(|| panic!("missing {href}"));
        assert!(found > at, "{href} out of order");
        at = found;
    }
    assert!(html.contains("data-theme"), "missing data-theme");
    assert!(html.contains("class=\"rail\""), "missing class=\"rail\"");
    assert!(!html.contains(".tile.lg"), "icon size must not have a second class");
    assert!(!html.contains("class=\"tile"));
}

// ppm, director and partner are readers. Sync is hidden for all three.
#[test]
fn ui_hides_sync_for_readers() {
    let html = include_str!("../../../ui/index.html");
    assert!(
        html.contains("const reader = ppm || role === 'director' || role === 'partner';"),
        "missing reader lock"
    );
    assert!(
        html.contains("$('sync').hidden = reader;"),
        "Sync is not hidden for ppm, director and partner"
    );
}

// Director lands on leadership updates. The partner lands on the brief.
#[test]
fn ui_lands_director_on_updates_and_partner_on_brief() {
    let html = include_str!("../../../ui/index.html");
    for needle in [
        "director:'#/updates'",
        "partner:'#/brief'",
        "'room','updates'",
        "href=\"#/updates\"",
        "href=\"#/brief\"",
        "if (p.role === 'director' && r.name !== 'updates') { mountBanner(); location.hash = '#/updates'; return; }",
        "if (p.role === 'partner' && r.name !== 'brief') { mountBanner(); location.hash = '#/brief'; return; }",
    ] {
        assert!(html.contains(needle), "missing {needle}");
    }
}

// The partner brief read view. A director is sent from #/brief back to updates.
#[test]
fn ui_brief_read_view() {
    let html = include_str!("../../../ui/index.html");
    for needle in [
        "function renderBrief(",
        "function briefCard(",
        "data-brief-retry",
        "function briefText(",
        "partner:'#/brief'",
        "if (p.role === 'director' && r.name !== 'updates') { mountBanner(); location.hash = '#/updates'; return; }",
        "/briefs?who=",
    ] {
        assert!(html.contains(needle), "missing {needle}");
    }
}

// The toast host must stay visible on the onboard screen.
#[test]
fn gate_leaves_the_toast_host_visible() {
    let html = include_str!("../../../ui/index.html");
    assert!(
        html.contains("['jobs', 'search-form', 'rail-projects']"),
        "missing gate hide list"
    );
    assert!(
        !html.contains("'search-form', 'note'"),
        "gate still hides the toast host"
    );
}

// UTC timestamps must be compared as local days.
#[test]
fn ui_compares_merged_at_as_local_days() {
    let html = include_str!("../../../ui/index.html");
    assert!(html.contains("function mergedDay(p){ return isoDay(new Date(p.merged_at)); }"), "missing mergedDay");
    assert!(!html.contains("merged_at.slice("), "merged_at.slice still compares UTC dates");
}

// The checks above only look for strings. Run the pure date helpers for real.
#[test]
fn ui_date_helpers_run_in_node() {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let output = match std::process::Command::new("node")
        .args(["--test", "--test-timeout=30000", "../../ui/helpers.test.mjs"])
        .current_dir(manifest_dir)
        .output()
    {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            panic!("node is not on PATH; install Node.js to run ui/helpers.test.mjs");
        }
        Err(error) => panic!("failed to spawn node: {error}"),
        Ok(output) => output,
    };
    assert!(
        output.status.success(),
        "node --test --test-timeout=30000 ../../ui/helpers.test.mjs failed: {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

// Class names in the stylesheet must not satisfy a check that the script renders them.
fn updates_body(html: &str) -> &str {
    let start = html.find("function renderUpdates").expect("renderUpdates");
    let rest = &html[start..];
    let end = rest[1..]
        .find("\nfunction ")
        .map(|i| i + 1)
        .unwrap_or(rest.len());
    &rest[..end]
}

// The read view copies one sent entry and asks for that person's list.
#[test]
fn ui_updates_read_view() {
    let html = include_str!("../../../ui/index.html");
    for needle in [
        "function entryText(",
        "LU_LABELS",
        "Team capacity",
    ] {
        assert!(html.contains(needle), "missing {needle}");
    }
    assert!(html.contains("Team capacity"), "missing Team capacity");
    assert!(
        updates_body(html).contains("/updates?who="),
        "renderUpdates does not call /updates?who="
    );
}

// The owner editor is pm and ppm only. Director and partner stay on the read view.
#[test]
fn ui_updates_editor_is_pm_or_ppm() {
    let html = include_str!("../../../ui/index.html");
    for needle in [
        "Send to the Director? Sent entries can't be edited.",
        "Start from the last entry",
        "/updates/correct",
        "list=\"lu-cats\"",
        "data-dirty",
    ] {
        assert!(html.contains(needle), "missing {needle}");
    }
    let body = updates_body(html);
    assert!(
        body.contains("p.role === 'pm' || p.role === 'ppm'"),
        "editor is not gated on role pm or ppm"
    );
    assert!(
        body.contains("luEditor"),
        "renderUpdates does not render the editor"
    );
}

// A correction previews the row count. A failed load shows a retry, not a blank page.
#[test]
fn ui_updates_preview_loading_and_retry() {
    let html = include_str!("../../../ui/index.html");
    assert!(html.contains("preview: true"), "missing preview: true");
    let body = updates_body(html);
    for needle in ["lu-loading", "lu-error", "data-lu-retry"] {
        assert!(body.contains(needle), "missing {needle}");
    }
}

// The AI page is a hash route with publish, run and resolve.
#[test]
fn ui_has_the_ai_page() {
    let html = include_str!("../../../ui/index.html");
    for needle in [
        "#/ai",
        "function renderAi(",
        "/ai/status",
        "data-ai-publish",
        "data-ai-run",
        "data-ai-resolve",
    ] {
        assert!(html.contains(needle), "missing {needle}");
    }
}
