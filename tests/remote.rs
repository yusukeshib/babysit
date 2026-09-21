#![cfg(unix)]

use serde_json::Value;
use std::fs;
use std::io::{BufRead, BufReader, Write};
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
        let proxy = root.join("drop-first-bridge.py");
        fs::write(
            &proxy,
            r#"import subprocess, sys

cmd = sys.argv[1]
child = subprocess.Popen(
    ["sh", "-c", cmd],
    stdin=subprocess.PIPE,
    stdout=subprocess.PIPE,
)
hello = sys.stdin.buffer.readline()
child.stdin.write(hello)
child.stdin.flush()

def read_exact(size):
    data = b""
    while len(data) < size:
        chunk = child.stdout.read(size - len(data))
        if not chunk:
            return None
        data += chunk
    return data

while True:
    header = read_exact(5)
    if header is None:
        raise SystemExit(child.wait())
    length = int.from_bytes(header[1:], "big")
    payload = read_exact(length)
    if payload is None:
        raise SystemExit(child.wait())
    sys.stdout.buffer.write(header + payload)
    sys.stdout.buffer.flush()
    if header[0] in (1, 4):
        child.kill()
        child.wait()
        raise SystemExit(255)
"#,
        )
        .unwrap();
        r#"#!/bin/sh
[ "$1" = "-T" ] && shift
while [ "$1" = "-o" ]; do shift 2; done
[ "$1" = "--" ] && shift
shift
cmd=$1
if echo "$cmd" | grep -q __remote-bridge && mkdir "$FAKE_ONCE" 2>/dev/null; then
  exec python3 "$FAKE_PROXY" "$cmd"
fi
exec sh -c "$cmd"
"#
    } else {
        r#"#!/bin/sh
[ "$1" = "-T" ] && shift
while [ "$1" = "-o" ]; do shift 2; done
[ "$1" = "--" ] && shift
shift
exec sh -c "$1"
"#
    };
    fs::write(&path, body).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
    path
}

fn fake_ssh_incompatible(root: &Path) -> PathBuf {
    let path = root.join("fake-ssh-incompatible");
    fs::write(
        &path,
        "#!/bin/sh\nprintf '%s\\n' '{\"protocol\":0,\"version\":\"old\"}'\n",
    )
    .unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
    path
}

fn fake_ssh_creation_drop(root: &Path) -> PathBuf {
    let path = root.join("fake-ssh-create-drop");
    fs::write(
        &path,
        r#"#!/bin/sh
[ "$1" = "-T" ] && shift
while [ "$1" = "-o" ]; do shift 2; done
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
    let mut paths = vec![Path::new(bin()).parent().unwrap().to_path_buf()];
    paths.extend(std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    ));
    let mut command = Command::new(bin());
    command
        .env("PATH", std::env::join_paths(paths).unwrap())
        .env("BABYSIT_DIR", root.join("remote-state"))
        .env("BABYSIT_SSH", ssh)
        .env("FAKE_ONCE", root.join("first-bridge"))
        .env("FAKE_PROXY", root.join("drop-first-bridge.py"));
    command
}

fn cli(root: &Path, ssh: &Path, args: &[&str]) -> Output {
    cli_command(root, ssh).args(args).output().unwrap()
}

#[test]
fn direct_ssh_destination_runs_remote_commands() {
    let root = temp_root("direct-host");
    let ssh = fake_ssh(&root, false);

    let started = cli(
        &root,
        &ssh,
        &[
            "--host",
            "user@fake",
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
    let waited = cli(&root, &ssh, &["--host", "user@fake", "wait", "-s", &id]);
    assert!(waited.status.success());
    let log = cli(&root, &ssh, &["--host", "user@fake", "log", "-s", &id]);
    assert_eq!(log.stdout, b"remote-ok");

    let _ = fs::remove_dir_all(root);
}

#[test]
fn incompatible_remote_is_rejected_before_foreground_run_creation() {
    let root = temp_root("incompatible");
    let ssh = fake_ssh_incompatible(&root);
    let output = cli(
        &root,
        &ssh,
        &[
            "--host",
            "old-host",
            "run",
            "--",
            "sh",
            "-c",
            "touch should-not-run",
        ],
    );
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("protocol 0 is incompatible"));
    assert!(!root.join("remote-state/sessions").exists());
    let _ = fs::remove_dir_all(root);
}

#[test]
fn standalone_escape_is_forwarded_without_waiting_for_another_key() {
    let root = temp_root("escape-timeout");
    let ssh = fake_ssh(&root, false);
    let started = cli(
        &root,
        &ssh,
        &[
            "--host",
            "user@fake",
            "run",
            "-d",
            "--json",
            "--no-tty",
            "--",
            "sh",
            "-c",
            "dd bs=1 count=1 2>/dev/null | od -An -tx1",
        ],
    );
    let id = serde_json::from_slice::<Value>(&started.stdout).unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let mut child = cli_command(&root, &ssh)
        .args(["--host", "user@fake", "attach", "-s", &id])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_millis(200));
    child.stdin.take().unwrap().write_all(&[0x1b]).unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("1b"));
    let _ = fs::remove_dir_all(root);
}

#[test]
fn ambiguous_creation_recovers_the_generated_id_without_spawning_twice() {
    let root = temp_root("creation-drop");
    let ssh = fake_ssh_creation_drop(&root);
    let starts = root.join("creation-starts");
    let script = format!("printf x >> '{}'; sleep .1", starts.display());
    let started = cli(
        &root,
        &ssh,
        &[
            "--host",
            "user@fake",
            "run",
            "-d",
            "--json",
            "--",
            "sh",
            "-c",
            &script,
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
    let _ = cli(&root, &ssh, &["--host", "user@fake", "wait", "-s", &id]);
    assert_eq!(fs::read(&starts).unwrap(), b"x");
    let sessions = cli(&root, &ssh, &["--host", "user@fake", "list", "--json"]);
    let sessions: Value = serde_json::from_slice(&sessions.stdout).unwrap();
    assert_eq!(sessions.as_array().unwrap().len(), 1);
    let _ = fs::remove_dir_all(root);
}

#[test]
fn foreground_run_reconnects_without_duplicate_output_or_process_restart() {
    let root = temp_root("reconnect");
    let ssh = fake_ssh(&root, true);

    let starts = root.join("starts");
    let script = format!(
        "printf x >> '{}'; printf A; sleep .20; printf B; sleep .80; printf C; stty size",
        starts.display()
    );
    let output = cli(
        &root,
        &ssh,
        &["--host", "user@fake", "run", "--", "sh", "-c", &script],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stdout.starts_with(b"ABC"));
    assert!(String::from_utf8_lossy(&output.stdout).contains("24 80"));
    assert!(String::from_utf8_lossy(&output.stderr).contains("reconnecting"));
    let sessions = cli(&root, &ssh, &["--host", "user@fake", "list", "--json"]);
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
    let started = cli(
        &root,
        &ssh,
        &[
            "--host",
            "user@fake",
            "run",
            "-d",
            "--json",
            "--",
            "sh",
            "-c",
            "printf A; sleep 2",
        ],
    );
    let id = serde_json::from_slice::<Value>(&started.stdout).unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned();

    let mut child = cli_command(&root, &ssh)
        .args(["--host", "user@fake", "attach", "-s", &id])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let stderr = child.stderr.take().unwrap();
    let (notice_tx, notice_rx) = std::sync::mpsc::channel();
    let stderr_reader = std::thread::spawn(move || {
        let mut text = String::new();
        for line in BufReader::new(stderr).lines() {
            let line = line.unwrap();
            if line.contains("reconnecting") {
                let _ = notice_tx.send(());
            }
            text.push_str(&line);
            text.push('\n');
        }
        text
    });
    notice_rx.recv_timeout(Duration::from_secs(3)).unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(&[0x1c, 0x1c])
        .unwrap();
    let output = child.wait_with_output().unwrap();
    let stderr = stderr_reader.join().unwrap();
    assert!(output.status.success(), "{stderr}");
    assert!(stderr.contains("reconnecting"));
    let status = cli(
        &root,
        &ssh,
        &["--host", "user@fake", "status", "-s", &id, "--json"],
    );
    let status: Value = serde_json::from_slice(&status.stdout).unwrap();
    assert_eq!(
        status["status"]["state"], "running",
        "detach must not kill the session"
    );
    let _ = cli(&root, &ssh, &["--host", "user@fake", "wait", "-s", &id]);
    let _ = fs::remove_dir_all(root);
}

#[test]
fn transformed_view_fails_instead_of_replaying_duplicate_output() {
    let root = temp_root("view-reconnect");
    let ssh = fake_ssh(&root, true);
    let started = cli(
        &root,
        &ssh,
        &[
            "--host",
            "user@fake",
            "run",
            "-d",
            "--json",
            "--view-cmd",
            "cat",
            "--",
            "sh",
            "-c",
            "printf A; sleep 2; printf B",
        ],
    );
    let id = serde_json::from_slice::<Value>(&started.stdout).unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let output = cli(&root, &ssh, &["--host", "user@fake", "attach", "-s", &id]);
    assert!(!output.status.success());
    assert!(output.stdout.starts_with(b"A"));
    assert_eq!(
        output.stdout.iter().filter(|&&byte| byte == b'A').count(),
        1
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("reconnect is unavailable for --view-cmd")
    );
    let _ = cli(&root, &ssh, &["--host", "user@fake", "kill", "-s", &id]);
    let _ = fs::remove_dir_all(root);
}

#[test]
fn no_reconnect_returns_on_transport_loss_and_leaves_session_running() {
    let root = temp_root("no-reconnect");
    let ssh = fake_ssh(&root, true);
    let started = cli(
        &root,
        &ssh,
        &[
            "--host",
            "user@fake",
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
        &["--host", "user@fake", "attach", "-s", &id, "--no-reconnect"],
    );
    assert!(!attached.status.success());
    assert!(!String::from_utf8_lossy(&attached.stderr).contains("reconnecting"));
    let status = cli(
        &root,
        &ssh,
        &["--host", "user@fake", "status", "-s", &id, "--json"],
    );
    assert!(
        status.status.success(),
        "transport loss must not kill the worker"
    );
    let _ = cli(&root, &ssh, &["--host", "user@fake", "wait", "-s", &id]);
    let _ = fs::remove_dir_all(root);
}
