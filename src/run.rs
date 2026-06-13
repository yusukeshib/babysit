use crate::attach;
use crate::control::{self, Handle, LoopMessage};
use crate::pane::{ExitInfo, OutputHub, Pane};
use crate::paths;
use crate::session::{self, Meta, State, Status};
use anyhow::{Context, Result, anyhow};
use chrono::Utc;
use std::io::{IsTerminal, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tokio::sync::{mpsc, watch};

/// Entry point for `babysit run` / `babysit -- …` / `babysit -d -- …`.
///
/// Architecture (tmux-style): the wrapped command always runs under a
/// headless *worker* process that owns the PTY, the control socket, and the
/// output fan-out. Foreground terminals are just *clients* attached over the
/// socket. `run` spawns the worker and (unless `-d`) attaches to it; `-d`
/// spawns the worker and returns immediately.
pub async fn run(
    cmd: Vec<String>,
    id: Option<String>,
    detach: bool,
    detached_id: Option<String>,
    no_tty: bool,
    timeout: Option<String>,
) -> Result<i32> {
    // Parse the timeout up front so a bad value errors before we spawn.
    let timeout = timeout.as_deref().map(parse_duration).transpose()?;

    // We are the detached worker (re-exec'd with --detached-id): run the
    // headless server loop and never come back until the command exits.
    if let Some(worker_id) = detached_id {
        serve_worker(cmd, worker_id, !no_tty, timeout).await?;
        return Ok(0);
    }

    // Parent: choose the id, announce it, spawn the worker.
    let session_id = session::make_id(id).await?;
    print_banner(&session_id, &cmd.join(" "));
    spawn_worker_process(&cmd, &session_id, no_tty, timeout)?;

    if detach {
        return Ok(0);
    }
    // Attached run: stream the session until it exits or we detach. Use the
    // id directly (skip resolution) since the worker may not have written the
    // session dir yet — connect_retry waits for its socket.
    attach::attach_to(session_id).await
}

/// The headless worker: owns the PTY + control socket, fans output out to
/// attached clients, and supervises restarts until the command exits.
async fn serve_worker(
    cmd: Vec<String>,
    id: String,
    tty: bool,
    timeout: Option<Duration>,
) -> Result<()> {
    let meta = Meta {
        id: id.clone(),
        cmd: cmd.clone(),
        babysit_pid: std::process::id(),
        started_at: Utc::now(),
    };
    session::write_meta(&meta).await?;
    session::write_status(&id, &Status::starting()).await?;

    // No terminal here (stdio is /dev/null); start at a sane default. Attached
    // clients send their real size via a resize frame.
    let (cols, rows) = (80u16, 24u16);

    let log_path = paths::output_log_path(&id)?;
    let env = vec![("BABYSIT_SESSION_ID".into(), id.clone())];
    let hub = OutputHub::new();
    let pane = match Pane::spawn(&cmd, rows, cols, &env, Some(&log_path), hub.clone(), tty) {
        Ok(p) => Arc::new(p),
        Err(e) => {
            // Don't leave the session stuck in `starting` forever.
            let _ = session::write_status(
                &id,
                &Status {
                    state: State::Exited,
                    child_pid: None,
                    exit_code: None,
                    last_change: Utc::now(),
                },
            )
            .await;
            return Err(e);
        }
    };

    session::write_status(
        &id,
        &Status {
            state: State::Running,
            child_pid: pane.pid,
            exit_code: None,
            last_change: Utc::now(),
        },
    )
    .await?;

    let (action_tx, mut action_rx) = mpsc::unbounded_channel::<LoopMessage>();
    let (exit_tx, exit_rx) = watch::channel::<Option<ExitInfo>>(None);
    let (detach_tx, _detach_rx0) = watch::channel::<u64>(0);
    let detach_tx = Arc::new(detach_tx);
    let attached = Arc::new(AtomicUsize::new(0));
    let handle = Handle::new(
        id.clone(),
        pane.clone(),
        action_tx,
        hub.clone(),
        exit_rx,
        detach_tx,
        attached.clone(),
    );
    control::serve(handle.clone()).await?;

    let mut current_pane = pane;
    let info: Option<ExitInfo>;
    // Optional auto-kill deadline. Fires once; after that the branch is
    // disabled so we don't busy-loop re-killing.
    let timeout_at = timeout.map(|d| tokio::time::Instant::now() + d);
    let mut timed_out = false;

    loop {
        let exit_notify = current_pane.exit_notify.clone();
        tokio::select! {
            _ = async {
                match timeout_at {
                    Some(t) => tokio::time::sleep_until(t).await,
                    None => std::future::pending::<()>().await,
                }
            }, if !timed_out => {
                timed_out = true;
                current_pane.kill();
            }
            Some(msg) = action_rx.recv() => match msg {
                LoopMessage::Restart => {
                    current_pane.kill();
                    current_pane.exit_notify.notified().await;
                    let new_pane = Arc::new(Pane::spawn(&cmd, rows, cols, &env, Some(&log_path), hub.clone(), tty)?);
                    handle.replace_cmd_pane(new_pane.clone()).await;
                    session::write_status(&id, &Status {
                        state: State::Running,
                        child_pid: new_pane.pid,
                        exit_code: None,
                        last_change: Utc::now(),
                    }).await?;
                    current_pane = new_pane;
                }
            },
            _ = exit_notify.notified() => {
                info = current_pane.exit_info();
                let signaled = info.map(|i| i.signaled).unwrap_or(true);
                let state = if signaled { State::Killed } else { State::Exited };
                session::write_status(&id, &Status {
                    state,
                    child_pid: None,
                    exit_code: info.and_then(|i| i.code),
                    last_change: Utc::now(),
                }).await?;
                break;
            }
        }
    }

    // Let the reader thread drain the final PTY output to the log and to any
    // attached clients' queues (bounded so lingering PTY holders can't wedge
    // shutdown), then tell attached clients the exit code.
    let _ = tokio::time::timeout(
        std::time::Duration::from_millis(500),
        current_pane.reader_done.notified(),
    )
    .await;
    let _ = exit_tx.send(Some(info.unwrap_or(ExitInfo {
        code: None,
        signaled: true,
    })));

    // Wait (bounded) for attached clients to flush the remaining output and
    // the exit frame and disconnect, so the live view isn't truncated. The
    // on-disk log already has everything regardless.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    while attached.load(Ordering::SeqCst) > 0 && std::time::Instant::now() < deadline {
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    control::cleanup(&id);
    Ok(())
}

/// Print the session-id banner to the user's terminal.
fn print_banner(id: &str, cmd_title: &str) {
    let (on, off) = if std::io::stdout().is_terminal() {
        ("\x1b[1;36m", "\x1b[0m")
    } else {
        ("", "")
    };
    println!("babysit session {on}{id}{off}: {cmd_title}");
    println!("  babysit log -s {on}{id}{off} --tail 200");
    println!("  babysit attach -s {on}{id}{off}");
    let _ = std::io::stdout().flush();
}

/// Re-exec babysit as a detached worker that supervises `cmd` in the
/// background. The worker gets its own session (setsid) so it survives the
/// parent and the user's shell exiting, and its stdio is detached to
/// /dev/null (output is captured to the log and fanned out to attached
/// clients). The chosen `id` is handed down via --detached-id.
fn spawn_worker_process(
    cmd: &[String],
    id: &str,
    no_tty: bool,
    timeout: Option<Duration>,
) -> Result<()> {
    use std::process::{Command, Stdio};

    let exe = std::env::current_exe().context("locating the babysit executable")?;
    let mut command = Command::new(exe);
    command.arg("run").arg("--detached-id").arg(id);
    if no_tty {
        command.arg("--no-tty");
    }
    if let Some(d) = timeout {
        command.arg("--timeout").arg(format!("{}s", d.as_secs()));
    }
    command.arg("--").args(cmd);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // New session: detach from the controlling terminal and the parent's
        // process group so the worker isn't killed when the shell exits or
        // sends Ctrl-C to the foreground group.
        unsafe {
            command.pre_exec(|| {
                nix::unistd::setsid().map_err(|e| std::io::Error::from_raw_os_error(e as i32))?;
                Ok(())
            });
        }
    }

    command
        .spawn()
        .context("spawning detached babysit worker")?;
    Ok(())
}

/// Parse a human duration like `30s`, `10m`, `2h`, `1d`, or a bare number of
/// seconds, into a `Duration`.
pub fn parse_duration(s: &str) -> Result<Duration> {
    let s = s.trim();
    if s.is_empty() {
        return Err(anyhow!("empty duration"));
    }
    let (num, unit_secs) = match s.as_bytes()[s.len() - 1] {
        b's' | b'S' => (&s[..s.len() - 1], 1u64),
        b'm' | b'M' => (&s[..s.len() - 1], 60),
        b'h' | b'H' => (&s[..s.len() - 1], 3600),
        b'd' | b'D' => (&s[..s.len() - 1], 86400),
        _ => (s, 1),
    };
    let n: u64 = num
        .trim()
        .parse()
        .map_err(|_| anyhow!("invalid duration `{s}` (use e.g. 30s, 10m, 2h)"))?;
    Ok(Duration::from_secs(n * unit_secs))
}

#[cfg(test)]
mod tests {
    use super::parse_duration;
    use std::time::Duration;

    #[test]
    fn parses_units_and_bare_seconds() {
        assert_eq!(parse_duration("30s").unwrap(), Duration::from_secs(30));
        assert_eq!(parse_duration("10m").unwrap(), Duration::from_secs(600));
        assert_eq!(parse_duration("2h").unwrap(), Duration::from_secs(7200));
        assert_eq!(parse_duration("1d").unwrap(), Duration::from_secs(86400));
        assert_eq!(parse_duration("45").unwrap(), Duration::from_secs(45));
    }

    #[test]
    fn rejects_garbage() {
        assert!(parse_duration("").is_err());
        assert!(parse_duration("abc").is_err());
        assert!(parse_duration("10x").is_err());
    }
}
