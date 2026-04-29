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

pub async fn run(cmd: Vec<String>) -> Result<i32> {
    let id = session::new_id();
    let cmd_title = cmd.join(" ");

    let meta = Meta {
        id: id.clone(),
        name: None,
        cmd: cmd.clone(),
        babysit_pid: std::process::id(),
        started_at: Utc::now(),
    };
    session::write_meta(&meta).await?;
    session::write_status(&id, &Status::starting()).await?;

    // Print the session id banner *before* raw mode so it stays in the
    // user's scrollback. They can paste this id into a Claude / Codex
    // session running in another terminal.
    println!("babysit session {id}: {cmd_title}");
    println!("  babysit log -s {id} --tail 200");
    println!("  babysit status -s {id}");
    let _ = std::io::stdout().flush();

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
            child_pid: None,
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
                        child_pid: None,
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

    control::cleanup(&id);

    // Drop _raw → terminal restored before we return.
    drop(_raw);

    Ok(exit_code.unwrap_or(if signaled { 130 } else { 0 }))
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
