#![cfg(unix)]

use serde_json::Value;
use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::Duration;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_babysit")
}

fn temp_root(label: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!(
        "babysit-remote-{label}-{}-{}",
        std::process::id(),
        chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default(),
    ));
    fs::create_dir_all(&root).unwrap();
    root
}

fn fake_ssh(root: &Path, disconnect_first_bridge: bool) -> PathBuf {
    let path = root.join("fake-ssh");
    let body = if disconnect_first_bridge {
        r#"#!/bin/sh
[ "$1" = "-T" ] && shift
[ "$1" = "--" ] && shift
shift
cmd=$1
if echo "$cmd" | grep -q __remote-bridge && mkdir "$FAKE_ONCE" 2>/dev/null; then
  sh -c "$cmd" <&0 >&1 2>&2 & p=$!
  sleep .20
  kill "$p" 2>/dev/null || true
  wait "$p" 2>/dev/null || true
  exit 255
fi
exec sh -c "$cmd"
"#
    } else {
        r#"#!/bin/sh
[ "$1" = "-T" ] && shift
[ "$1" = "--" ] && shift
shift
exec sh -c "$1"
"#
    };
    fs::write(&path, body).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
    path
}

fn fake_ssh_creation_drop(root: &Path) -> PathBuf {
    let path = root.join("fake-ssh-create-drop");
    fs::write(
        &path,
        r#"#!/bin/sh
[ "$1" = "-T" ] && shift
[ "$1" = "--" ] && shift
shift
cmd=$1
if echo "$cmd" | grep -q "'run'" && mkdir "$FAKE_ONCE" 2>/dev/null; then
  sh -c "$cmd" <&0 >&1 2>&2 || true
  exit 255
fi
exec sh -c "$cmd"
"#,
    )
    .unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
    path
}

fn cli_command(root: &Path, ssh: &Path) -> Command {
    let mut command = Command::new(bin());
    command
        .env("BABYSIT_CONFIG", root.join("machines.json"))
        .env("BABYSIT_DIR", root.join("remote-state"))
        .env("BABYSIT_SSH", ssh)
        .env("FAKE_ONCE", root.join("first-bridge"));
    command
}

fn cli(root: &Path, ssh: &Path, args: &[&str]) -> Output {
    cli_command(root, ssh).args(args).output().unwrap()
}

fn add_machine(root: &Path, ssh: &Path) {
    let output = cli(
        root,
        ssh,
        &["machine", "add", "dev", "fake", "--remote-command", bin()],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn machine_profiles_and_remote_detached_commands() {
    let root = temp_root("profiles");
    let ssh = fake_ssh(&root, false);
    add_machine(&root, &ssh);

    let listed = cli(&root, &ssh, &["machine", "list", "--json"]);
    let profiles: Value = serde_json::from_slice(&listed.stdout).unwrap();
    assert_eq!(profiles[0]["name"], "dev");
    assert_eq!(profiles[0]["target"], "fake");

    let started = cli(
        &root,
        &ssh,
        &[
            "--host",
            "dev",
            "run",
            "-d",
            "--json",
            "--",
            "sh",
            "-c",
            "printf remote-ok",
        ],
    );
    assert!(
        started.status.success(),
        "{}",
        String::from_utf8_lossy(&started.stderr)
    );
    let id = serde_json::from_slice::<Value>(&started.stdout).unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let waited = cli(&root, &ssh, &["--host", "dev", "wait", "-s", &id]);
    assert!(waited.status.success());
    let log = cli(&root, &ssh, &["--host", "dev", "log", "-s", &id]);
    assert_eq!(log.stdout, b"remote-ok");

    let removed = cli(&root, &ssh, &["machine", "remove", "dev"]);
    assert!(removed.status.success());
    let _ = fs::remove_dir_all(root);
}

#[test]
fn ambiguous_creation_recovers_the_generated_id_without_spawning_twice() {
    let root = temp_root("creation-drop");
    let ssh = fake_ssh_creation_drop(&root);
    add_machine(&root, &ssh);
    let starts = root.join("creation-starts");
    let script = format!("printf x >> '{}'; sleep .1", starts.display());
    let started = cli(
        &root,
        &ssh,
        &[
            "--host", "dev", "run", "-d", "--json", "--", "sh", "-c", &script,
        ],
    );
    assert!(
        started.status.success(),
        "{}",
        String::from_utf8_lossy(&started.stderr)
    );
    let id = serde_json::from_slice::<Value>(&started.stdout).unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let _ = cli(&root, &ssh, &["--host", "dev", "wait", "-s", &id]);
    assert_eq!(fs::read(&starts).unwrap(), b"x");
    let sessions = cli(&root, &ssh, &["--host", "dev", "list", "--json"]);
    let sessions: Value = serde_json::from_slice(&sessions.stdout).unwrap();
    assert_eq!(sessions.as_array().unwrap().len(), 1);
    let _ = fs::remove_dir_all(root);
}

#[test]
fn foreground_run_reconnects_without_duplicate_output_or_process_restart() {
    let root = temp_root("reconnect");
    let ssh = fake_ssh(&root, true);
    add_machine(&root, &ssh);

    let starts = root.join("starts");
    let script = format!(
        "printf x >> '{}'; printf A; sleep .12; printf B; sleep .35; printf C; stty size",
        starts.display()
    );
    let output = cli(
        &root,
        &ssh,
        &["--host", "dev", "run", "--", "sh", "-c", &script],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stdout.starts_with(b"ABC"));
    assert!(String::from_utf8_lossy(&output.stdout).contains("24 80"));
    assert!(String::from_utf8_lossy(&output.stderr).contains("reconnecting"));
    let sessions = cli(&root, &ssh, &["--host", "dev", "list", "--json"]);
    let sessions: Value = serde_json::from_slice(&sessions.stdout).unwrap();
    assert_eq!(sessions.as_array().unwrap().len(), 1);
    assert_eq!(
        fs::read(&starts).unwrap(),
        b"x",
        "wrapped command must run once"
    );
    let _ = fs::remove_dir_all(root);
}

#[test]
fn detach_sequence_works_while_waiting_to_reconnect() {
    let root = temp_root("detach-reconnect");
    let ssh = fake_ssh(&root, true);
    add_machine(&root, &ssh);
    let started = cli(
        &root,
        &ssh,
        &[
            "--host", "dev", "run", "-d", "--json", "--", "sh", "-c", "sleep 2",
        ],
    );
    let id = serde_json::from_slice::<Value>(&started.stdout).unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned();

    let mut child = cli_command(&root, &ssh)
        .args(["--host", "dev", "attach", "-s", &id])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_millis(300));
    child
        .stdin
        .take()
        .unwrap()
        .write_all(&[0x1c, 0x1c])
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stderr).contains("reconnecting"));
    let status = cli(
        &root,
        &ssh,
        &["--host", "dev", "status", "-s", &id, "--json"],
    );
    let status: Value = serde_json::from_slice(&status.stdout).unwrap();
    assert_eq!(
        status["status"]["state"], "running",
        "detach must not kill the session"
    );
    let _ = cli(&root, &ssh, &["--host", "dev", "wait", "-s", &id]);
    let _ = fs::remove_dir_all(root);
}

#[test]
fn no_reconnect_returns_on_transport_loss_and_leaves_session_running() {
    let root = temp_root("no-reconnect");
    let ssh = fake_ssh(&root, true);
    add_machine(&root, &ssh);
    let started = cli(
        &root,
        &ssh,
        &[
            "--host",
            "dev",
            "run",
            "-d",
            "--json",
            "--",
            "sh",
            "-c",
            "printf A; sleep 1; printf B",
        ],
    );
    let id = serde_json::from_slice::<Value>(&started.stdout).unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let attached = cli(
        &root,
        &ssh,
        &["--host", "dev", "attach", "-s", &id, "--no-reconnect"],
    );
    assert!(!attached.status.success());
    assert!(!String::from_utf8_lossy(&attached.stderr).contains("reconnecting"));
    let status = cli(
        &root,
        &ssh,
        &["--host", "dev", "status", "-s", &id, "--json"],
    );
    assert!(
        status.status.success(),
        "transport loss must not kill the worker"
    );
    let _ = cli(&root, &ssh, &["--host", "dev", "wait", "-s", &id]);
    let _ = fs::remove_dir_all(root);
}
