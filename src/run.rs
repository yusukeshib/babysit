use crate::attach;
use crate::control::{self, Handle, LoopMessage};
use crate::pane::{ExitInfo, OutputHub, Pane};
use crate::paths;
use crate::session::{self, Meta, State, Status};
use anyhow::{Context, Result};
use chrono::Utc;
use std::io::{IsTerminal, Write};
use std::sync::Arc;
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
) -> Result<i32> {
    // We are the detached worker (re-exec'd with --detached-id): run the
    // headless server loop and never come back until the command exits.
    if let Some(worker_id) = detached_id {
        serve_worker(cmd, worker_id).await?;
        return Ok(0);
    }

    // Parent: choose the id, announce it, spawn the worker.
    let session_id = session::make_id(id).await?;
    print_banner(&session_id, &cmd.join(" "));
    spawn_worker_process(&cmd, &session_id)?;

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
async fn serve_worker(cmd: Vec<String>, id: String) -> Result<()> {
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
    let pane = match Pane::spawn(&cmd, rows, cols, &env, Some(&log_path), hub.clone()) {
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
    let handle = Handle::new(
        id.clone(),
        pane.clone(),
        action_tx,
        hub.clone(),
        exit_rx,
        detach_tx,
    );
    control::serve(handle.clone()).await?;

    let mut current_pane = pane;
    let info: Option<ExitInfo>;

    loop {
        let exit_notify = current_pane.exit_notify.clone();
        tokio::select! {
            Some(msg) = action_rx.recv() => match msg {
                LoopMessage::Restart => {
                    current_pane.kill();
                    current_pane.exit_notify.notified().await;
                    let new_pane = Arc::new(Pane::spawn(&cmd, rows, cols, &env, Some(&log_path), hub.clone())?);
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

    // Give attached clients a beat to flush queued output + the exit frame
    // before we tear the socket down.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
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
fn spawn_worker_process(cmd: &[String], id: &str) -> Result<()> {
    use std::process::{Command, Stdio};

    let exe = std::env::current_exe().context("locating the babysit executable")?;
    let mut command = Command::new(exe);
    command.arg("run").arg("--detached-id").arg(id);
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
