use crate::agent;
use crate::control::{self, Handle, LoopMessage};
use crate::pane::Pane;
use crate::paths;
use crate::session::{self, Meta, State, Status};
use crate::tui::{App, TerminalGuard};
use anyhow::Result;
use chrono::Utc;
use crossterm::event::{Event, EventStream, KeyEventKind};
use futures::StreamExt;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;

pub async fn run(
    prompt: Option<String>,
    agent_arg: Option<String>,
    cmd: Vec<String>,
) -> Result<()> {
    let id = session::new_id();
    let cmd_title = cmd.join(" ");

    // Resolve the agent up-front so we fail before opening the alt-screen.
    let agent_spec = match agent::resolve(agent_arg.as_deref()) {
        Ok((name, bin)) => Some(agent::build_spec(name, bin, prompt.clone(), &id)),
        Err(e) => {
            eprintln!("babysit: {e}");
            eprintln!("note: continuing without an agent. Tab 2 will be empty.");
            None
        }
    };
    let agent_title = agent_spec
        .as_ref()
        .map(|s| s.name.clone())
        .unwrap_or_else(|| "no agent".to_string());

    // Persist meta + initial status before opening the TUI.
    let meta = Meta {
        id: id.clone(),
        name: None,
        cmd: cmd.clone(),
        agent: agent_spec.as_ref().map(|s| s.name.clone()),
        prompt: prompt.clone(),
        babysit_pid: std::process::id(),
        started_at: Utc::now(),
    };
    session::write_meta(&meta).await?;
    session::write_status(&id, &Status::starting()).await?;

    let mut guard = TerminalGuard::enter()?;
    let mut app = App::new(id.clone(), cmd_title.clone(), agent_title);

    // Initial PTY size; corrected on first draw.
    let initial_rows = 24;
    let initial_cols = 80;

    // Tab 1: wrapped command.
    let log_path = paths::output_log_path(&id)?;
    let cmd_pane = match Pane::spawn(
        &cmd,
        initial_rows,
        initial_cols,
        &[("BABYSIT_SESSION_ID".into(), id.clone())],
        app.redraw.clone(),
        Some(&log_path),
    ) {
        Ok(p) => Arc::new(p),
        Err(e) => {
            // Don't leave the session stuck in "starting" forever.
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
    app.tabs[0].pane = Some(cmd_pane.clone());

    // Mark the session running as soon as the wrapped command is spawned.
    // Doing this before the agent pane means an agent-spawn failure can
    // never leave the session stuck in `starting`.
    let mut current_state = State::Running;
    session::write_status(
        &id,
        &Status {
            state: current_state,
            child_pid: None,
            exit_code: None,
            last_change: Utc::now(),
        },
    )
    .await?;

    // Tab 2: agent (if available).
    if let Some(spec) = agent_spec {
        let env = build_agent_env(&id);
        let mut argv = vec![spec.bin.to_string_lossy().into_owned()];
        argv.extend(spec.args.clone());
        let agent_pane = Arc::new(Pane::spawn(
            &argv,
            initial_rows,
            initial_cols,
            &env,
            app.redraw.clone(),
            None, // agent output isn't logged to disk
        )?);
        app.tabs[1].pane = Some(agent_pane.clone());

        let initial = spec.initial_message.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(800)).await;
            agent_pane.write_input(initial.as_bytes());
            tokio::time::sleep(Duration::from_millis(100)).await;
            agent_pane.write_input(b"\r");
        });
    }

    // Action channel for control-plane → main-loop messages.
    let (action_tx, mut action_rx) = mpsc::unbounded_channel::<LoopMessage>();
    let handle = Handle::new(id.clone(), cmd_pane.clone(), action_tx);
    control::serve(handle.clone()).await?;

    // Run the merged event loop here (instead of in tui::run_app) because
    // the loop now also drives action processing and child-exit detection.
    let result = drive_loop(
        guard.terminal(),
        &mut app,
        &handle,
        &mut action_rx,
        &id,
        &cmd,
        &mut current_state,
    )
    .await;

    // Best-effort cleanup of the control socket file.
    control::cleanup(&id);

    drop(guard);
    result
}

async fn drive_loop(
    terminal: &mut crate::tui::Term,
    app: &mut App,
    handle: &Handle,
    action_rx: &mut mpsc::UnboundedReceiver<LoopMessage>,
    id: &str,
    cmd: &[String],
    current_state: &mut State,
) -> Result<()> {
    use crate::tui::{forward_key_to_active, render};

    let mut events = EventStream::new();
    let redraw = app.redraw.clone();

    loop {
        terminal.draw(|f| render(f, app))?;

        tokio::select! {
            _ = redraw.notified() => {}
            maybe_ev = events.next() => match maybe_ev {
                Some(Ok(Event::Key(k))) => {
                    if k.kind == KeyEventKind::Press {
                        let consumed = app.handle_key(k);
                        if !consumed {
                            forward_key_to_active(app, k);
                        }
                    }
                }
                Some(Ok(Event::Mouse(m))) => { let _ = app.handle_mouse(m); }
                Some(Ok(Event::Resize(_, _))) => {}
                Some(Ok(_)) => {}
                Some(Err(e)) => return Err(e.into()),
                None => break,
            },
            Some(msg) = action_rx.recv() => {
                match msg {
                    LoopMessage::Restart => {
                        do_restart(app, handle, id, cmd).await?;
                        *current_state = State::Running;
                        session::write_status(id, &Status {
                            state: *current_state,
                            child_pid: None,
                            exit_code: None,
                            last_change: Utc::now(),
                        }).await?;
                    }
                }
            }
        }

        // Detect child exit transitions and persist them.
        if let Some(pane) = app.tabs[0].pane.as_ref()
            && let Some(info) = pane.exit_info()
            && *current_state == State::Running
        {
            *current_state = if info.signaled {
                State::Killed
            } else {
                State::Exited
            };
            session::write_status(
                id,
                &Status {
                    state: *current_state,
                    child_pid: None,
                    exit_code: info.code,
                    last_change: Utc::now(),
                },
            )
            .await?;
        }

        // Honor the `r` keybind from the TUI.
        if app.pending_restart_cmd {
            app.pending_restart_cmd = false;
            do_restart(app, handle, id, cmd).await?;
            *current_state = State::Running;
            session::write_status(
                id,
                &Status {
                    state: *current_state,
                    child_pid: None,
                    exit_code: None,
                    last_change: Utc::now(),
                },
            )
            .await?;
        }

        if app.should_quit {
            break;
        }
    }
    Ok(())
}

async fn do_restart(app: &mut App, handle: &Handle, id: &str, cmd: &[String]) -> Result<()> {
    if let Some(old) = app.tabs[0].pane.take() {
        old.kill();
    }
    let log_path = paths::output_log_path(id)?;
    let new_pane = Arc::new(Pane::spawn(
        cmd,
        24,
        80,
        &[("BABYSIT_SESSION_ID".into(), id.to_string())],
        app.redraw.clone(),
        Some(&log_path),
    )?);
    app.tabs[0].pane = Some(new_pane.clone());
    handle.replace_cmd_pane(new_pane).await;
    Ok(())
}

fn build_agent_env(id: &str) -> Vec<(String, String)> {
    let mut env = vec![("BABYSIT_SESSION_ID".to_string(), id.to_string())];
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        let dir = dir.to_string_lossy().into_owned();
        let existing = std::env::var("PATH").unwrap_or_default();
        let new_path = if existing.is_empty() {
            dir
        } else {
            format!("{dir}:{existing}")
        };
        env.push(("PATH".to_string(), new_path));
    }
    env
}
