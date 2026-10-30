// The binary reads TALLIMO_PORT: a bad value stops it before it opens a database,
// and a good one serves on that port with a fresh database.
use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Command, Stdio};

fn scratch(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("tallimo-port-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn api(dir: &std::path::Path, port: &str) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_rollup-api"));
    cmd.current_dir(dir)
        .env("TALLIMO_PORT", port)
        .env("ROLLUP_DB_PATH", dir.join("fresh.db"))
        .env("ROLLUP_SYNC_MODE", "fixture");
    cmd
}

#[test]
fn a_bad_tallimo_port_is_a_clear_startup_error() {
    let dir = scratch("bad");
    for bad in ["abc", "0", "70000"] {
        let out = api(&dir, bad).output().unwrap();
        assert_eq!(out.status.code(), Some(2), "{bad}");
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(err.contains("TALLIMO_PORT must be a port number from 1 to 65535"), "{bad}: {err}");
        assert!(!dir.join("fresh.db").exists(), "{bad}: a database was created");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn tallimo_port_serves_on_that_port_with_a_fresh_database() {
    let dir = scratch("good");
    let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let mut child = api(&dir, &port.to_string()).stdout(Stdio::piped()).spawn().unwrap();
    let mut first = String::new();
    // Keep the reader open: closing the pipe early would make the server's next line fail.
    let mut out = BufReader::new(child.stdout.take().unwrap());
    out.read_line(&mut first).unwrap();
    assert_eq!(first.trim(), format!("rollup-api listening on http://127.0.0.1:{port}"));
    let token = std::fs::read_to_string(dir.join("fresh.db.token")).unwrap();
    let token = token.trim().to_string();

    let status = |host: String| {
        let mut s = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
        write!(s, "GET /workspace HTTP/1.1\r\nHost: {host}\r\nX-Tallimo-Token: {token}\r\nConnection: close\r\n\r\n").unwrap();
        let mut buf = String::new();
        s.read_to_string(&mut buf).unwrap();
        buf.split_whitespace().nth(1).unwrap().to_string()
    };
    let served = status(format!("localhost:{port}"));
    let other_port = status("localhost:8787".into());
    child.kill().unwrap();
    child.wait().unwrap();
    assert!(dir.join("fresh.db").exists());
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(served, "200");
    assert_eq!(other_port, "421");
}

#[test]
fn live_mode_without_jira_boards_is_a_startup_error() {
    let dir = scratch("live");
    let mut child = api(&dir, "8787")
        .env("ROLLUP_SYNC_MODE", "live")
        .env("ROLLUP_JIRA_BOARDS", "")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut err_pipe = child.stderr.take().unwrap();
    // Keep stdout open so a listening line cannot fill the pipe and stall the child.
    let mut out = child.stdout.take().unwrap();
    // read_to_string blocks until the process closes the pipe. Wait first, or a
    // guard that fails to exit hangs this test.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if std::time::Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                panic!(
                    "rollup-api still running after 10s without ROLLUP_JIRA_BOARDS; killed it. Live mode should exit before it listens."
                );
            }
            Ok(None) => std::thread::sleep(std::time::Duration::from_millis(50)),
            Err(e) => panic!("try_wait failed: {e}"),
        }
    };
    let mut err = String::new();
    err_pipe.read_to_string(&mut err).unwrap();
    let mut sink = String::new();
    let _ = out.read_to_string(&mut sink);
    assert_eq!(status.code(), Some(2), "{err}");
    assert!(
        err.contains("ROLLUP_SYNC_MODE=live needs ROLLUP_JIRA_BOARDS: the Jira project keys to read, for example PROJ,OPS."),
        "{err}"
    );
    assert!(!dir.join("fresh.db").exists(), "{err}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn ai_run_on_a_fresh_database_prints_a_line_and_exits_0() {
    let dir = scratch("airun");
    let out = api(&dir, "8787")
        .env_remove("ROLLUP_AI")
        .arg("ai-run")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(out.status.code(), Some(0), "{stderr}");
    assert!(
        stdout.lines().any(|line| line.starts_with("ai-run: ")),
        "{stdout}"
    );
}
