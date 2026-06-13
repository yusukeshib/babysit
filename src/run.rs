use crate::control::{self, Handle, LoopMessage};
use crate::pane::Pane;
use crate::paths;
use crate::session::{self, Meta, State, Status};
use anyhow::{Context, Result};
use chrono::Utc;
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use std::io::{IsTerminal, Read, Write};
use std::sync::{Arc, RwLock};
use std::thread;
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::mpsc;

pub async fn run(
    cmd: Vec<String>,
    name: Option<String>,
    detach: bool,
    detached_id: Option<String>,
) -> Result<i32> {
    let cmd_title = cmd.join(" ");

    // Parent side of `-d`: pick the id, print the banner, then re-exec a copy
    // of ourselves detached (own session, stdio → /dev/null) so the wrapped
    // command keeps running after this process and the user's shell return.
    // The worker copy is invoked with --detached-id and lands below.
    if detach && detached_id.is_none() {
        let id = session::new_unique_id().await;
        print_banner(&id, &cmd_title);
        spawn_detached(&cmd, name.as_deref(), &id)?;
        return Ok(0);
    }

    // Either an attached run, or the detached worker (which carries the id the
    // parent already announced).
    let id = match detached_id {
        Some(id) => id,
        None => session::new_unique_id().await,
    };

    let meta = Meta {
        id: id.clone(),
        name,
        cmd: cmd.clone(),
        babysit_pid: std::process::id(),
        started_at: Utc::now(),
    };
    session::write_meta(&meta).await?;
    session::write_status(&id, &Status::starting()).await?;

    // Print the session id banner *before* raw mode so it stays in the
    // user's scrollback. They can paste this id into a Claude / Codex
    // session running in another terminal. (For the detached worker this
    // goes to /dev/null; the parent already printed the visible banner.)
    print_banner(&id, &cmd_title);

    let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));

    let log_path = paths::output_log_path(&id)?;
    let env = vec![("BABYSIT_SESSION_ID".into(), id.clone())];
    let pane = match Pane::spawn(&cmd, rows, cols, &env, Some(&log_path)) {
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

    // Enable raw mode now that the child is up. The RawGuard restores
    // the terminal on drop, so any error past this point still leaves a
    // usable shell behind. Skipped when stdin isn't a tty (e.g. piped
    // input under tests), since enable_raw_mode requires one.
    let _raw = if std::io::stdin().is_terminal() {
        match RawGuard::enter() {
            Ok(g) => Some(g),
            Err(e) => {
                eprintln!("babysit: could not enter raw mode: {e}; continuing without it");
                None
            }
        }
    } else {
        None
    };

    // Stdin → PTY forwarder. Lives on a std thread because std::io::stdin
    // is blocking. Uses a shared slot so `restart` can swap target panes
    // without restarting the thread.
    let active: Arc<RwLock<Arc<Pane>>> = Arc::new(RwLock::new(pane.clone()));
    spawn_stdin_forwarder(active.clone());

    // Control socket so `babysit log/status/send/restart/kill` work.
    let (action_tx, mut action_rx) = mpsc::unbounded_channel::<LoopMessage>();
    let handle = Handle::new(id.clone(), pane.clone(), action_tx);
    control::serve(handle.clone()).await?;

    let mut winch = signal(SignalKind::window_change()).context("install SIGWINCH handler")?;

    let mut current_pane = pane;
    let exit_code: Option<i32>;
    let signaled: bool;

    loop {
        let exit_notify = current_pane.exit_notify.clone();
        tokio::select! {
            _ = winch.recv() => {
                if let Ok((cols, rows)) = crossterm::terminal::size() {
                    current_pane.resize(rows, cols);
                }
            }
            Some(msg) = action_rx.recv() => match msg {
                LoopMessage::Restart => {
                    current_pane.kill();
                    current_pane.exit_notify.notified().await;
                    let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
                    let new_pane = Arc::new(Pane::spawn(&cmd, rows, cols, &env, Some(&log_path))?);
                    *active.write().unwrap() = new_pane.clone();
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
                let info = current_pane.exit_info();
                exit_code = info.and_then(|i| i.code);
                signaled = info.map(|i| i.signaled).unwrap_or(true);
                let state = if signaled { State::Killed } else { State::Exited };
                session::write_status(&id, &Status {
                    state,
                    child_pid: None,
                    exit_code,
                    last_change: Utc::now(),
                }).await?;
                break;
            }
        }
    }

    // The child has been reaped, but the reader thread may still be flushing
    // the final bytes of PTY output to stdout and the log. Give it a brief
    // window to drain before we tear the process down, so we don't truncate
    // the tail of the output. Bounded so a wrapped command that leaves
    // background processes holding the PTY open can't wedge shutdown.
    let _ = tokio::time::timeout(
        std::time::Duration::from_millis(500),
        current_pane.reader_done.notified(),
    )
    .await;

    control::cleanup(&id);

    // Drop _raw → terminal restored before we return.
    drop(_raw);

    Ok(exit_code.unwrap_or(if signaled { 130 } else { 0 }))
}

/// Print the session-id banner. Goes to the user's terminal for an attached
/// run (and for the `-d` parent); for the detached worker stdout is
/// /dev/null, so it's a no-op there.
fn print_banner(id: &str, cmd_title: &str) {
    let (on, off) = if std::io::stdout().is_terminal() {
        ("\x1b[1;36m", "\x1b[0m")
    } else {
        ("", "")
    };
    println!("babysit session {on}{id}{off}: {cmd_title}");
    println!("  babysit log -s {on}{id}{off} --tail 200");
    println!("  babysit status -s {on}{id}{off}");
    let _ = std::io::stdout().flush();
}

/// Re-exec babysit as a detached worker that supervises `cmd` in the
/// background. The worker gets its own session (setsid) so it survives the
/// parent and the user's shell exiting, and its stdio is detached to
/// /dev/null (output is still captured to the session log). The chosen `id`
/// is handed down so the worker adopts the same session id the parent just
/// announced.
fn spawn_detached(cmd: &[String], name: Option<&str>, id: &str) -> Result<()> {
    use std::process::{Command, Stdio};

    let exe = std::env::current_exe().context("locating the babysit executable")?;
    let mut command = Command::new(exe);
    command.arg("run").arg("--detached-id").arg(id);
    if let Some(name) = name {
        command.arg("--name").arg(name);
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

fn spawn_stdin_forwarder(active: Arc<RwLock<Arc<Pane>>>) {
    thread::spawn(move || {
        let stdin = std::io::stdin();
        let mut lock = stdin.lock();
        let mut buf = [0u8; 4096];
        loop {
            match lock.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let pane = active.read().unwrap().clone();
                    pane.write_input(&buf[..n]);
                }
            }
        }
    });
}

/// RAII guard that puts the terminal in raw mode and restores it on drop.
struct RawGuard;

impl RawGuard {
    fn enter() -> Result<Self> {
        enable_raw_mode()?;
        Ok(Self)
    }
}

impl Drop for RawGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
    }
}
