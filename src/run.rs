use crate::attach;
use crate::control::{self, Handle, LoopMessage};
use crate::pane::{ExitInfo, OutputHub, Pane};
use crate::paths::Babysit;
use crate::session::{self, Meta, State, Status};
use anyhow::{Context, Result, anyhow};
use chrono::Utc;
use std::io::{IsTerminal, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tokio::sync::{mpsc, watch};

impl Babysit {
    /// Entry point for `babysit run` / `babysit -- …` / `babysit -d -- …`.
    ///
    /// Architecture (tmux-style): the wrapped command always runs under a
    /// headless *worker* process that owns the PTY, the control socket, and the
    /// output fan-out. Foreground terminals are just *clients* attached over the
    /// socket. `run` spawns the worker and (unless `-d`) attaches to it; `-d`
    /// spawns the worker and returns immediately.
    #[allow(clippy::too_many_arguments)] // top-level entry; each arg is a distinct flag
    pub async fn run(
        &self,
        cmd: Vec<String>,
        id: Option<String>,
        detach: bool,
        detached_id: Option<String>,
        no_tty: bool,
        timeout: Option<String>,
        idle_timeout: Option<String>,
        size: Option<String>,
        view_cmd: Option<String>,
        json: bool,
    ) -> Result<i32> {
        // Parse the inputs up front so a bad value errors before we spawn.
        // Use parse_timeout (not parse_duration) so `0`/`none`/`off`/`never`
        // mean "no timeout" here too, consistent with wait/expect/wait-idle —
        // otherwise `--timeout 0s` would auto-kill the command immediately.
        let timeout = timeout.as_deref().map(parse_timeout).transpose()?.flatten();
        let idle_timeout = idle_timeout
            .as_deref()
            .map(parse_timeout)
            .transpose()?
            .flatten();
        let size = size.as_deref().map(parse_size).transpose()?;

        // We are the detached worker (re-exec'd with --detached-id): run the
        // headless server loop and never come back until the command exits.
        if let Some(worker_id) = detached_id {
            self.serve_worker(
                cmd,
                worker_id,
                !no_tty,
                timeout,
                idle_timeout,
                size,
                view_cmd,
            )
            .await?;
            return Ok(0);
        }

        // Parent: choose the id, announce it, spawn the worker.
        let session_id = session::make_id(self, id).await?;
        if json {
            // Machine-readable: an agent captures `.id` without scraping prose.
            println!("{}", serde_json::json!({ "id": session_id }));
            let _ = std::io::stdout().flush();
        } else if self.is_cli() {
            print_banner(&session_id, &cmd.join(" "));
        }
        // (library embedder, non-json: stay silent — the host owns its own UX)
        self.spawn_worker_process(
            &cmd,
            &session_id,
            no_tty,
            timeout,
            idle_timeout,
            size,
            view_cmd.as_deref(),
        )?;

        if detach {
            return Ok(0);
        }
        // Attached run: stream the session until it exits or we detach. Use the
        // id directly (skip resolution) since the worker may not have written
        // the session dir yet — connect_retry waits for its socket.
        attach::attach_to(self, session_id).await
    }

    /// The headless worker: owns the PTY + control socket, fans output out to
    /// attached clients, and supervises restarts until the command exits.
    #[allow(clippy::too_many_arguments)] // supervisor loop; each arg is a distinct run flag
    async fn serve_worker(
        &self,
        cmd: Vec<String>,
        id: String,
        tty: bool,
        timeout: Option<Duration>,
        idle_timeout: Option<Duration>,
        size: Option<(u16, u16)>,
        view_cmd: Option<String>,
    ) -> Result<()> {
        let meta = Meta {
            id: id.clone(),
            cmd: cmd.clone(),
            babysit_pid: std::process::id(),
            started_at: Utc::now(),
        };
        session::write_meta(self, &meta).await?;
        session::write_status(self, &id, &Status::starting()).await?;

        // No terminal here (stdio is /dev/null); start at the requested size or
        // a sane default. Attached clients send their real size via a resize
        // frame.
        let (cols, rows) = size.unwrap_or((80, 24));

        let log_path = self.output_log_path(&id);
        // Only the CLI exposes the session id to the wrapped command (so nested
        // `babysit` calls can omit -s). As a library, stay invisible: an
        // embedder's wrapped program (e.g. an LLM agent) must not see — and then
        // parrot — babysit's identity (`babysit attach -s …`).
        let env = if self.is_cli() {
            vec![("BABYSIT_SESSION_ID".into(), id.clone())]
        } else {
            vec![]
        };
        let hub = OutputHub::new();
        let pane = match Pane::spawn(&cmd, rows, cols, &env, Some(&log_path), hub.clone(), tty) {
            Ok(p) => Arc::new(p),
            Err(e) => {
                // Don't leave the session stuck in `starting` forever.
                let _ = session::write_status(
                    self,
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
            self,
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
            self.clone(),
            id.clone(),
            pane.clone(),
            action_tx,
            hub.clone(),
            exit_rx,
            detach_tx,
            attached.clone(),
            view_cmd,
        );
        if let Err(error) = control::serve(handle.clone()).await {
            // A bind/setup failure happens after the child and `running` status
            // exist. Finalize both explicitly instead of letting the detached
            // supervisor exit and leave a misleading stale-running session.
            // Child termination is best-effort here; the setup error remains
            // primary, but use the same escalation path so descendants do not
            // leak if they ignore SIGHUP.
            let _ = terminate_pane(&pane).await;
            session::write_status(
                self,
                &id,
                &Status {
                    state: State::Killed,
                    child_pid: None,
                    // This is a supervisor setup failure regardless of whether
                    // the wrapped command happened to exit 0 first.
                    exit_code: Some(1),
                    last_change: Utc::now(),
                },
            )
            .await?;
            return Err(error);
        }

        let mut current_pane = pane;
        let info: Option<ExitInfo>;
        let mut terminal_status_written = false;
        // Optional auto-kill deadline. Fires once; after that the branch is
        // disabled so we don't busy-loop re-killing.
        let timeout_at = timeout.map(|d| tokio::time::Instant::now() + d);

        // Optional inactivity watchdog: poll the pane's idle time and kill once
        // it exceeds the limit. Polled (rather than event-driven) since output
        // arrives on a blocking reader thread.
        let idle_limit_ms = idle_timeout.map(|d| d.as_millis() as u64);
        let mut idle_tick =
            idle_limit_ms.map(|_| tokio::time::interval(Duration::from_millis(500)));

        loop {
            let exit_notify = current_pane.exit_notify.clone();
            tokio::select! {
                _ = async {
                    match timeout_at {
                        Some(t) => tokio::time::sleep_until(t).await,
                        None => std::future::pending::<()>().await,
                    }
                } => {
                    info = Some(terminate_pane(&current_pane).await?);
                    break;
                }
                _ = async {
                    match idle_tick.as_mut() {
                        Some(t) => { t.tick().await; }
                        None => std::future::pending::<()>().await,
                    }
                } => {
                    if let Some(limit) = idle_limit_ms
                        && current_pane.idle_ms() >= limit
                    {
                        info = Some(terminate_pane(&current_pane).await?);
                        break;
                    }
                }
                Some(msg) = action_rx.recv() => match msg {
                    LoopMessage::Restart => {
                        terminate_pane(&current_pane).await?;
                        let new_pane = Arc::new(Pane::spawn(&cmd, rows, cols, &env, Some(&log_path), hub.clone(), tty)?);
                        handle.replace_cmd_pane(new_pane.clone()).await;
                        session::write_status(self, &id, &Status {
                            state: State::Running,
                            child_pid: new_pane.pid,
                            exit_code: None,
                            last_change: Utc::now(),
                        }).await?;
                        current_pane = new_pane;
                    }
                    LoopMessage::Kill { reply } => {
                        match terminate_pane(&current_pane).await {
                            Ok(exit) => {
                                let state = if exit.signaled { State::Killed } else { State::Exited };
                                session::write_status(self, &id, &Status {
                                    state,
                                    child_pid: None,
                                    exit_code: exit.code,
                                    last_change: Utc::now(),
                                }).await?;
                                terminal_status_written = true;
                                let _ = reply.send(Ok(serde_json::json!({
                                    "killed": true,
                                    "confirmed": true,
                                    "state": state,
                                    "exit_code": exit.code,
                                })));
                                info = Some(exit);
                                break;
                            }
                            Err(error) => {
                                let _ = reply.send(Err(format!("{error:#}")));
                            }
                        }
                    }
                },
                _ = exit_notify.notified() => {
                    info = current_pane.exit_info();
                    break;
                }
            }
        }

        if !terminal_status_written {
            let signaled = info.map(|i| i.signaled).unwrap_or(true);
            let state = if signaled {
                State::Killed
            } else {
                State::Exited
            };
            session::write_status(
                self,
                &id,
                &Status {
                    state,
                    child_pid: None,
                    exit_code: info.and_then(|i| i.code),
                    last_change: Utc::now(),
                },
            )
            .await?;
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
        control::cleanup(self, &id);
        Ok(())
    }

    /// Re-exec babysit as a detached worker that supervises `cmd` in the
    /// background. The worker gets its own session (setsid) so it survives the
    /// parent and the user's shell exiting, and its stdio is detached to
    /// /dev/null (output is captured to the log and fanned out to attached
    /// clients). The chosen `id` is handed down via `--detached-id`, and the
    /// state root via `--root`, so the worker reconstructs THIS context without
    /// reading the environment.
    #[allow(clippy::too_many_arguments)] // re-exec builder; mirrors run()'s flags
    fn spawn_worker_process(
        &self,
        cmd: &[String],
        id: &str,
        no_tty: bool,
        timeout: Option<Duration>,
        idle_timeout: Option<Duration>,
        size: Option<(u16, u16)>,
        view_cmd: Option<&str>,
    ) -> Result<()> {
        use std::process::{Command, Stdio};

        let exe = self.supervisor_exe()?;
        let mut command = Command::new(exe);
        command
            .arg("run")
            .arg("--detached-id")
            .arg(id)
            .arg("--root")
            .arg(self.root());
        if no_tty {
            command.arg("--no-tty");
        }
        if let Some(d) = timeout {
            // Pass milliseconds so a sub-second timeout isn't truncated to 0s
            // when re-exec'd into the worker.
            command.arg("--timeout").arg(format!("{}ms", d.as_millis()));
        }
        if let Some(d) = idle_timeout {
            command
                .arg("--idle-timeout")
                .arg(format!("{}ms", d.as_millis()));
        }
        if let Some((c, r)) = size {
            command.arg("--size").arg(format!("{c}x{r}"));
        }
        if let Some(v) = view_cmd {
            command.arg("--view-cmd").arg(v);
        }
        command.arg("--").args(cmd);
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());

        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            // New session: detach from the controlling terminal and the
            // parent's process group so the worker isn't killed when the shell
            // exits or sends Ctrl-C to the foreground group.
            unsafe {
                command.pre_exec(|| {
                    nix::unistd::setsid()
                        .map_err(|e| std::io::Error::from_raw_os_error(e as i32))?;
                    Ok(())
                });
            }
        }

        command
            .spawn()
            .context("spawning detached babysit worker")?;
        Ok(())
    }

    /// Resolve the executable to re-exec as the detached worker supervisor.
    ///
    /// Precedence:
    ///   1. an explicit [`with_supervisor_exe`](crate::Babysit::with_supervisor_exe)
    ///      override (the embedder knows a stable path);
    ///   2. on Linux, `/proc/self/exe` — the kernel keeps this pointing at the
    ///      running image's inode even after the on-disk binary is replaced
    ///      (upgrade) or unlinked (`nix gc`), so a long-lived supervisor can
    ///      still re-exec itself; and it execs the right inode for free;
    ///   3. `current_exe()` everywhere else (and if `/proc/self/exe` is absent,
    ///      e.g. /proc not mounted).
    fn supervisor_exe(&self) -> Result<PathBuf> {
        if let Some(p) = self.supervisor_override() {
            return Ok(p.to_path_buf());
        }
        #[cfg(target_os = "linux")]
        {
            let proc_self = PathBuf::from("/proc/self/exe");
            if proc_self.exists() {
                return Ok(proc_self);
            }
        }
        std::env::current_exe().context("locating the babysit executable")
    }
}

/// Terminate a pane with confirmation. A graceful SIGHUP keeps interactive
/// programs able to clean up; stubborn processes are escalated to SIGKILL.
/// Success means the wait thread observed the child exit, not merely that a
/// signal syscall accepted the request.
async fn terminate_pane(pane: &Arc<Pane>) -> Result<ExitInfo> {
    if let Some(info) = pane.exit_info() {
        return Ok(info);
    }

    pane.kill().context("requesting graceful termination")?;
    let graceful_info = wait_for_pane_exit(pane, Duration::from_millis(300)).await;
    if let Some(info) = graceful_info
        && !pane.command_tree_alive()?
    {
        return Ok(info);
    }

    // The direct child may already be gone while descendants remain in its
    // process group. Always escalate the still-live group before confirming.
    pane.force_kill().context("escalating termination")?;
    let info = match graceful_info {
        Some(info) => info,
        None => wait_for_pane_exit(pane, Duration::from_secs(2))
            .await
            .ok_or_else(|| anyhow!("process did not exit after SIGKILL"))?,
    };
    wait_for_command_tree_exit(pane, Duration::from_secs(2)).await?;
    Ok(info)
}

/// Wait for exit without losing a notification that races the initial status
/// check. The wait thread also leaves a Notify permit armed, but checking both
/// sides makes this safe if that implementation detail changes.
async fn wait_for_pane_exit(pane: &Arc<Pane>, timeout: Duration) -> Option<ExitInfo> {
    if let Some(info) = pane.exit_info() {
        return Some(info);
    }
    let notified = pane.exit_notify.notified();
    tokio::pin!(notified);
    if let Some(info) = pane.exit_info() {
        return Some(info);
    }
    let _ = tokio::time::timeout(timeout, &mut notified).await;
    pane.exit_info()
}

async fn wait_for_command_tree_exit(pane: &Arc<Pane>, timeout: Duration) -> Result<()> {
    let deadline = tokio::time::Instant::now() + timeout;
    while pane.command_tree_alive()? {
        if tokio::time::Instant::now() >= deadline {
            return Err(anyhow!("process group still exists after SIGKILL"));
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
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

/// Parse a `--timeout` value into an optional deadline. The sentinels `0`,
/// `none`, `off`, `never` (and the empty string) mean "no timeout" and yield
/// `None`; everything else parses as a normal duration. A zero duration
/// (e.g. `0s`) is also treated as "no timeout".
pub fn parse_timeout(s: &str) -> Result<Option<Duration>> {
    let t = s.trim();
    if t.is_empty() || matches!(t.to_ascii_lowercase().as_str(), "none" | "off" | "never") {
        return Ok(None);
    }
    let d = parse_duration(t)?;
    Ok(if d.is_zero() { None } else { Some(d) })
}

/// Parse a human duration like `500ms`, `30s`, `10m`, `2h`, `1d`, or a bare
/// number of seconds, into a `Duration`.
pub fn parse_duration(s: &str) -> Result<Duration> {
    let s = s.trim();
    if s.is_empty() {
        return Err(anyhow!("empty duration"));
    }
    // Milliseconds first, since `ms` ends in `s` and would otherwise be read
    // as seconds.
    if let Some(num) = s.strip_suffix("ms").or_else(|| s.strip_suffix("MS")) {
        let n: u64 = num
            .trim()
            .parse()
            .map_err(|_| anyhow!("invalid duration `{s}` (use e.g. 500ms, 30s, 10m, 2h)"))?;
        return Ok(Duration::from_millis(n));
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
        .map_err(|_| anyhow!("invalid duration `{s}` (use e.g. 500ms, 30s, 10m, 2h)"))?;
    Ok(Duration::from_secs(n * unit_secs))
}

/// Parse a `COLSxROWS` geometry string like `120x40` into `(cols, rows)`.
pub fn parse_size(s: &str) -> Result<(u16, u16)> {
    let (c, r) = s
        .split_once(['x', 'X'])
        .ok_or_else(|| anyhow!("invalid size `{s}` (use COLSxROWS, e.g. 120x40)"))?;
    let cols: u16 = c
        .trim()
        .parse()
        .map_err(|_| anyhow!("invalid columns in `{s}`"))?;
    let rows: u16 = r
        .trim()
        .parse()
        .map_err(|_| anyhow!("invalid rows in `{s}`"))?;
    if cols == 0 || rows == 0 {
        return Err(anyhow!("size must be non-zero (got `{s}`)"));
    }
    Ok((cols, rows))
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use super::terminate_pane;
    use super::{parse_duration, parse_timeout};
    #[cfg(unix)]
    use crate::pane::{OutputHub, Pane};
    #[cfg(unix)]
    use crate::session::is_pid_alive;
    #[cfg(unix)]
    use std::sync::Arc;
    use std::time::Duration;

    #[test]
    fn timeout_sentinels_mean_infinite() {
        // `0`, zero durations and the word forms => no deadline.
        assert_eq!(parse_timeout("0").unwrap(), None);
        assert_eq!(parse_timeout("0s").unwrap(), None);
        assert_eq!(parse_timeout("none").unwrap(), None);
        assert_eq!(parse_timeout("OFF").unwrap(), None);
        assert_eq!(parse_timeout("never").unwrap(), None);
        assert_eq!(parse_timeout("").unwrap(), None);
        // A real duration parses through.
        assert_eq!(parse_timeout("30s").unwrap(), Some(Duration::from_secs(30)));
        assert!(parse_timeout("abc").is_err());
    }

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

    #[test]
    fn parses_milliseconds() {
        assert_eq!(parse_duration("500ms").unwrap(), Duration::from_millis(500));
        assert_eq!(parse_duration("0ms").unwrap(), Duration::from_millis(0));
        // `ms` must win over the `s`-suffix branch.
        assert_ne!(parse_duration("500ms").unwrap(), Duration::from_secs(500));
        assert!(parse_duration("ms").is_err());
    }

    #[test]
    fn parses_size() {
        use super::parse_size;
        assert_eq!(parse_size("120x40").unwrap(), (120, 40));
        assert_eq!(parse_size("80X24").unwrap(), (80, 24));
        assert!(parse_size("120").is_err());
        assert!(parse_size("0x10").is_err());
        assert!(parse_size("axb").is_err());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn termination_escalates_and_kills_the_process_group() {
        let pid_file = std::env::temp_dir().join(format!(
            "babysit-kill-group-{}-{}.pid",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default(),
        ));
        let script = format!(
            "trap '' HUP; (trap '' HUP; while :; do sleep 1; done) & echo $! > {}; while :; do sleep 1; done",
            pid_file.display(),
        );
        let pane = Arc::new(
            Pane::spawn(
                &["sh".into(), "-c".into(), script],
                24,
                80,
                &[],
                None,
                OutputHub::new(),
                false,
            )
            .unwrap(),
        );
        let parent_pid = pane.pid.unwrap();

        let child_pid = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if let Ok(raw) = std::fs::read_to_string(&pid_file)
                    && let Ok(pid) = raw.trim().parse::<u32>()
                {
                    break pid;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("child pid was not written");

        let info = terminate_pane(&pane).await.unwrap();
        assert!(info.signaled, "ignored SIGHUP should require SIGKILL");

        tokio::time::timeout(Duration::from_secs(2), async {
            while is_pid_alive(parent_pid) || is_pid_alive(child_pid) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("parent or descendant survived confirmed termination");
        let _ = std::fs::remove_file(pid_file);
    }
}
