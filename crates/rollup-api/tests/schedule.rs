// `rollup-api schedule print --dir <DIR>` prints a launchd plist and writes nothing.
use std::process::Command;

fn scratch(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("tallimo-sched-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn print_schedule(dir: &std::path::Path) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_rollup-api"))
        .args(["schedule", "print", "--dir", dir.to_str().unwrap()])
        .output()
        .unwrap()
}

#[test]
fn schedule_print_fills_the_plist_and_escapes_ampersand() {
    let plain = scratch("plain");
    let out = print_schedule(&plain);
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    let plain_s = plain.to_str().unwrap().to_string();
    let wrote = std::fs::read_dir(&plain).unwrap().next().is_some();
    let _ = std::fs::remove_dir_all(&plain);
    assert_eq!(out.status.code(), Some(0), "{stderr}");
    assert!(!wrote, "schedule print wrote into {plain_s}");
    assert!(stdout.contains("local.tallimo.ai-run"), "{stdout}");
    assert!(stdout.contains("ai-run"), "{stdout}");
    assert!(stdout.contains("--if-due"), "{stdout}");
    assert!(stdout.contains("1800"), "{stdout}");
    assert!(stdout.contains(&plain_s), "{stdout}");
    assert!(stdout.contains(&format!("{plain_s}/ai-run.log")), "{stdout}");
    assert!(!stdout.contains("<DIR>"), "{stdout}");
    assert!(!stdout.contains("<BIN>"), "{stdout}");

    let amp = scratch("a&b");
    let out = print_schedule(&amp);
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    let amp_s = amp.to_str().unwrap().to_string();
    let _ = std::fs::remove_dir_all(&amp);
    assert_eq!(out.status.code(), Some(0), "{stderr}");
    assert!(stdout.contains(&amp_s.replace('&', "&amp;")), "{stdout}");
    assert!(!stdout.contains(&amp_s), "{stdout}");
}

#[test]
fn schedule_print_refuses_a_relative_dir() {
    let out = Command::new(env!("CARGO_BIN_EXE_rollup-api"))
        .args(["schedule", "print", "--dir", "relative/dir"])
        .output()
        .unwrap();
    let err = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(2), "{err}");
    assert!(err.contains("absolute"), "{err}");
    assert!(!String::from_utf8_lossy(&out.stdout).contains("local.tallimo.ai-run"));

    let missing = std::env::temp_dir().join(format!("tallimo-sched-missing-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&missing);
    let out = Command::new(env!("CARGO_BIN_EXE_rollup-api"))
        .args(["schedule", "print", "--dir", missing.to_str().unwrap()])
        .output()
        .unwrap();
    let err = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(2), "{err}");
    assert!(err.contains("not an existing directory"), "{err}");
}
