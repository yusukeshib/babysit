#![cfg(unix)]

use babysit::session::is_pid_alive;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant};

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_babysit")
}

fn temp_root(label: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!(
        "babysit-{label}-{}-{}",
        std::process::id(),
        chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default(),
    ));
    std::fs::create_dir_all(&root).unwrap();
    root
}

fn cli(root: &Path, args: &[&str]) -> Output {
    Command::new(bin())
        .env("BABYSIT_DIR", root)
        .args(args)
        .output()
        .unwrap()
}

fn json(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "invalid JSON ({error}): stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        )
    })
}

fn wait_until(timeout: Duration, mut predicate: impl FnMut() -> bool) {
    let deadline = Instant::now() + timeout;
    while !predicate() {
        assert!(Instant::now() < deadline, "condition timed out");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn kill_confirms_state_and_removes_stubborn_process_group() {
    for no_tty in [false, true] {
        let root = temp_root(if no_tty { "kill-pipe" } else { "kill-pty" });
        let pid_file = root.join("descendant.pid");
        let script = format!(
            "trap '' HUP; (trap '' HUP; while :; do sleep 1; done) & echo $! > {}; while :; do sleep 1; done",
            pid_file.display(),
        );
        let mut args = vec!["run", "-d", "--json"];
        if no_tty {
            args.push("--no-tty");
        }
        args.extend(["--", "sh", "-c", &script]);
        let started = cli(&root, &args);
        assert!(
            started.status.success(),
            "{}",
            String::from_utf8_lossy(&started.stderr)
        );
        let id = json(&started)["id"].as_str().unwrap().to_string();

        wait_until(Duration::from_secs(2), || {
            pid_file.metadata().is_ok_and(|m| m.len() > 0)
        });
        let descendant_pid: u32 = std::fs::read_to_string(&pid_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let before = cli(&root, &["status", "-s", &id, "--json"]);
        let parent_pid = json(&before)["status"]["child_pid"].as_u64().unwrap() as u32;

        let killed = cli(&root, &["kill", "-s", &id, "--json"]);
        assert!(
            killed.status.success(),
            "{}",
            String::from_utf8_lossy(&killed.stderr)
        );
        let killed_json = json(&killed);
        assert_eq!(killed_json["killed"], true);
        assert_eq!(killed_json["confirmed"], true);

        let after = json(&cli(&root, &["status", "-s", &id, "--json"]));
        assert_eq!(after["status"]["state"], "killed");
        assert!(after["status"]["child_pid"].is_null());
        wait_until(Duration::from_secs(2), || {
            !is_pid_alive(parent_pid) && !is_pid_alive(descendant_pid)
        });
        std::fs::remove_dir_all(root).unwrap();
    }
}

#[test]
fn escalated_kill_is_persisted_as_killed_even_when_leader_exits_zero() {
    let root = temp_root("kill-escalated-state");
    let pid_file = root.join("descendant.pid");
    let script = format!(
        "trap 'exit 0' HUP; (trap '' HUP; while :; do sleep 1; done) & echo $! > {}; wait",
        pid_file.display(),
    );
    let started = cli(&root, &["run", "-d", "--json", "--", "sh", "-c", &script]);
    assert!(started.status.success());
    let id = json(&started)["id"].as_str().unwrap().to_string();
    wait_until(Duration::from_secs(2), || {
        pid_file.metadata().is_ok_and(|m| m.len() > 0)
    });

    let killed = cli(&root, &["kill", "-s", &id, "--json"]);
    assert!(
        killed.status.success(),
        "{}",
        String::from_utf8_lossy(&killed.stderr)
    );
    let result = json(&killed);
    assert_eq!(result["confirmed"], true);
    assert_eq!(result["escalated"], true);
    assert_eq!(result["state"], "killed");
    let status = json(&cli(&root, &["status", "-s", &id, "--json"]));
    assert_eq!(status["status"]["state"], "killed");
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn kill_escalates_when_only_a_descendant_ignores_sighup() {
    let root = temp_root("kill-descendant");
    let pid_file = root.join("descendant.pid");
    let script = format!(
        "(trap '' HUP; while :; do sleep 1; done) & echo $! > {}; wait",
        pid_file.display(),
    );
    let started = cli(&root, &["run", "-d", "--json", "--", "sh", "-c", &script]);
    assert!(started.status.success());
    let id = json(&started)["id"].as_str().unwrap().to_string();
    wait_until(Duration::from_secs(2), || {
        pid_file.metadata().is_ok_and(|m| m.len() > 0)
    });
    let descendant_pid: u32 = std::fs::read_to_string(&pid_file)
        .unwrap()
        .trim()
        .parse()
        .unwrap();

    let killed = cli(&root, &["kill", "-s", &id, "--json"]);
    assert!(
        killed.status.success(),
        "{}",
        String::from_utf8_lossy(&killed.stderr)
    );
    assert_eq!(json(&killed)["confirmed"], true);
    wait_until(Duration::from_secs(2), || !is_pid_alive(descendant_pid));
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn kill_does_not_claim_an_already_finished_session() {
    let root = temp_root("kill-finished");
    let started = cli(
        &root,
        &[
            "run", "-d", "--json", "--no-tty", "--", "sh", "-c", "exit 0",
        ],
    );
    assert!(started.status.success());
    let id = json(&started)["id"].as_str().unwrap().to_string();
    wait_until(Duration::from_secs(2), || {
        let status = cli(&root, &["status", "-s", &id, "--json"]);
        status.status.success() && json(&status)["status"]["state"] == "exited"
    });

    let killed = cli(&root, &["kill", "-s", &id, "--json"]);
    assert!(!killed.status.success());
    assert!(!String::from_utf8_lossy(&killed.stdout).contains("\"killed\":true"));
    let after = json(&cli(&root, &["status", "-s", &id, "--json"]));
    assert_eq!(after["status"]["state"], "exited");
    std::fs::remove_dir_all(root).unwrap();
}
