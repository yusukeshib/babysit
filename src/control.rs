//! Control plane: a Unix domain socket per session that accepts JSON
//! requests and lets external callers (the `babysit` subcommands, plus the
//! sidecar agent) inspect and operate on the wrapped command.
//!
//! Wire protocol: one request per connection, newline-delimited JSON for
//! both directions:
//!
//! ```text
//! →  {"op":"status"}
//! ←  {"ok":true,"data":{...}}
//! ```
//!
//! The connection closes after the response.

use crate::attach::{self, C_INPUT, C_RESIZE, S_DETACHED, S_EXIT, S_OUTPUT};
use crate::pane::{ExitInfo, OutputHub, Pane};
use crate::paths::Babysit;
use crate::session;
use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use std::io::ErrorKind;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{Mutex, Notify, mpsc, watch};

/// Operations a client can request via the control socket.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    /// Read the current status (state, exit code, …) of the wrapped command.
    Status,
    /// Read the output log. `tail` returns only the last N lines; `raw`
    /// preserves ANSI escapes (otherwise they're stripped).
    Log {
        #[serde(default)]
        tail: Option<usize>,
        #[serde(default)]
        raw: bool,
    },
    /// Render the current visible screen (virtual terminal grid).
    Screenshot {
        format: crate::cli::ShotFormat,
        #[serde(default)]
        trim: bool,
    },
    /// Send text to the wrapped command's stdin. A trailing newline is
    /// appended unless `newline` is false (default true, for back-compat with
    /// older clients that omit the field).
    Send {
        text: String,
        #[serde(default = "default_true")]
        newline: bool,
    },
    /// Resize the PTY (and the virtual terminal) to the given dimensions.
    Resize { cols: u16, rows: u16 },
    /// Restart the wrapped command (kill + respawn with the same argv).
    Restart,
    /// Terminate the wrapped command (SIGHUP).
    Kill,
    /// Attach this connection to the live PTY: stream output and accept
    /// input/resize frames. Upgrades the connection to the frame protocol.
    Attach {
        #[serde(default)]
        cols: u16,
        #[serde(default)]
        rows: u16,
    },
    /// Detach any currently-attached clients, leaving the command running.
    Detach,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Response {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default)]
    pub data: serde_json::Value,
}

impl Response {
    pub fn ok(data: serde_json::Value) -> Self {
        Self {
            ok: true,
            error: None,
            data,
        }
    }
    pub fn err(msg: impl Into<String>) -> Self {
        Self {
            ok: false,
            error: Some(msg.into()),
            data: serde_json::Value::Null,
        }
    }
}

/// Message from the control loop to the main loop, for actions that need
/// to mutate the App's state (i.e. restart, which replaces the pane).
pub enum LoopMessage {
    Restart,
}

/// Shared handle that the control socket task reads from. Includes a
/// `Mutex<Arc<Pane>>` so it can always see the current command pane,
/// even after a restart swaps it.
#[derive(Clone)]
pub struct Handle {
    /// The context this session lives in (its state root), so the control loop
    /// resolves paths without consulting the environment.
    pub bs: Babysit,
    pub session_id: String,
    pub cmd_pane: Arc<Mutex<Arc<Pane>>>,
    pub action_tx: mpsc::UnboundedSender<LoopMessage>,
    /// Live PTY output fan-out for attached clients (survives restarts).
    pub hub: Arc<OutputHub>,
    /// Set once when the session ends; carries the final exit info so
    /// attached clients can be told the exit code.
    pub exit_rx: watch::Receiver<Option<ExitInfo>>,
    /// Bumped to force-detach all currently-attached clients.
    pub detach_tx: Arc<watch::Sender<u64>>,
    /// Count of currently-attached clients, so shutdown can wait for them to
    /// drain the final output + exit frame before tearing the socket down.
    pub attached: Arc<AtomicUsize>,
    /// Optional shell command the live output is piped through per attached
    /// client (e.g. a JSONL→human formatter). `None` streams raw bytes. The
    /// recorded log and vt100 screenshot always stay raw regardless.
    pub view_cmd: Option<String>,
}

impl Handle {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        bs: Babysit,
        session_id: String,
        cmd_pane: Arc<Pane>,
        action_tx: mpsc::UnboundedSender<LoopMessage>,
        hub: Arc<OutputHub>,
        exit_rx: watch::Receiver<Option<ExitInfo>>,
        detach_tx: Arc<watch::Sender<u64>>,
        attached: Arc<AtomicUsize>,
        view_cmd: Option<String>,
    ) -> Self {
        Self {
            bs,
            session_id,
            cmd_pane: Arc::new(Mutex::new(cmd_pane)),
            action_tx,
            hub,
            exit_rx,
            detach_tx,
            attached,
            view_cmd,
        }
    }

    pub async fn replace_cmd_pane(&self, new_pane: Arc<Pane>) {
        let mut g = self.cmd_pane.lock().await;
        *g = new_pane;
    }
}

/// Bind a control socket and spawn a task that serves requests forever.
/// The task is detached; on shutdown the caller should call `cleanup()`.
pub async fn serve(handle: Handle) -> Result<()> {
    let path = handle.bs.control_socket_path(&handle.session_id);
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("control socket has no parent: {}", path.display()))?;
    let user_dir = parent.parent().ok_or_else(|| {
        anyhow!(
            "control socket directory has no parent: {}",
            parent.display()
        )
    })?;
    ensure_private_dir(user_dir)?;
    ensure_private_dir(parent)?;

    // If a stale socket exists from a prior run with the same id, remove it.
    let _ = tokio::fs::remove_file(&path).await;
    let listener = UnixListener::bind(&path)
        .with_context(|| format!("binding control socket at {}", path.display()))?;
    secure_socket(&path)?;
    spawn_listener(listener, handle.clone());

    // Best-effort reverse compatibility: when the legacy path fits, bind it as
    // well so a pre-upgrade client can still control this new worker. A path
    // length failure is expected for the exact sessions this layout fixes.
    let legacy = handle.bs.legacy_control_socket_path(&handle.session_id);
    let _ = tokio::fs::remove_file(&legacy).await;
    if let Ok(listener) = UnixListener::bind(&legacy) {
        if secure_socket(&legacy).is_ok() {
            spawn_listener(listener, handle);
        } else {
            drop(listener);
            let _ = std::fs::remove_file(&legacy);
        }
    }
    Ok(())
}

fn ensure_private_dir(path: &Path) -> Result<()> {
    match std::fs::DirBuilder::new().mode(0o700).create(path) {
        Ok(()) => {}
        Err(error) if error.kind() == ErrorKind::AlreadyExists => {}
        Err(error) => {
            return Err(error)
                .with_context(|| format!("creating control socket directory {}", path.display()));
        }
    }
    let metadata = std::fs::symlink_metadata(path)
        .with_context(|| format!("inspecting control socket directory {}", path.display()))?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        anyhow::bail!(
            "control socket directory is not a real directory: {}",
            path.display()
        );
    }
    let expected_uid = nix::unistd::Uid::effective().as_raw();
    if metadata.uid() != expected_uid {
        anyhow::bail!(
            "control socket directory {} is owned by uid {}, expected {}",
            path.display(),
            metadata.uid(),
            expected_uid
        );
    }
    let mut permissions = metadata.permissions();
    permissions.set_mode(0o700);
    std::fs::set_permissions(path, permissions)
        .with_context(|| format!("securing control socket directory {}", path.display()))?;
    Ok(())
}

fn secure_socket(path: &Path) -> Result<()> {
    let mut permissions = std::fs::metadata(path)
        .with_context(|| {
            format!(
                "inspecting control socket permissions at {}",
                path.display()
            )
        })?
        .permissions();
    permissions.set_mode(0o600);
    std::fs::set_permissions(path, permissions)
        .with_context(|| format!("securing control socket at {}", path.display()))
}

fn spawn_listener(listener: UnixListener, handle: Handle) {
    tokio::spawn(async move {
        loop {
            let stream = match listener.accept().await {
                Ok((s, _)) => s,
                Err(_) => break,
            };
            let h = handle.clone();
            tokio::spawn(async move {
                let _ = handle_conn(stream, h).await;
            });
        }
    });
}

async fn handle_conn(stream: UnixStream, handle: Handle) -> Result<()> {
    let (rd, mut wr) = stream.into_split();
    let mut br = BufReader::new(rd);
    let mut line = String::new();
    let n = br.read_line(&mut line).await?;
    if n == 0 {
        return Ok(());
    }

    let req = match serde_json::from_str::<Request>(line.trim()) {
        Ok(req) => req,
        Err(e) => {
            let resp = Response::err(format!("invalid request: {e}"));
            let mut bytes = serde_json::to_vec(&resp)?;
            bytes.push(b'\n');
            wr.write_all(&bytes).await?;
            wr.flush().await?;
            return Ok(());
        }
    };

    // Attach upgrades the connection to the frame protocol; it never sends a
    // JSON response, so it's handled before the one-shot path.
    if let Request::Attach { cols, rows } = req {
        return handle_attach(br.into_inner(), wr, handle, cols, rows).await;
    }

    let resp = match dispatch(req, &handle).await {
        Ok(data) => Response::ok(data),
        Err(e) => Response::err(format!("{e}")),
    };

    let mut bytes = serde_json::to_vec(&resp)?;
    bytes.push(b'\n');
    wr.write_all(&bytes).await?;
    wr.flush().await?;
    wr.shutdown().await?;
    Ok(())
}

async fn dispatch(req: Request, handle: &Handle) -> Result<serde_json::Value> {
    match req {
        Request::Status => {
            let status = session::read_status(&handle.bs, &handle.session_id).await?;
            let mut obj = serde_json::to_value(status)?;
            // Augment with cheap-to-poll liveness metrics so an agent can tell
            // whether output advanced without re-fetching a screenshot/log.
            if let serde_json::Value::Object(map) = &mut obj {
                let path = handle.bs.output_log_path(&handle.session_id);
                let output_bytes = tokio::fs::metadata(&path)
                    .await
                    .map(|m| m.len())
                    .unwrap_or(0);
                let screen_seq = handle.cmd_pane.lock().await.clone().screen_seq();
                map.insert("output_bytes".into(), output_bytes.into());
                map.insert("screen_seq".into(), screen_seq.into());
            }
            Ok(obj)
        }
        Request::Log { tail, raw } => {
            let path = handle.bs.output_log_path(&handle.session_id);
            read_log(&path, tail, raw).await
        }
        Request::Screenshot { format, trim } => {
            let pane = handle.cmd_pane.lock().await.clone();
            let mut data = pane.screenshot(format, trim);
            // Stamp the frame sequence so an agent can dedup screenshots
            // (skip re-rendering when `screen_seq` hasn't advanced).
            if let serde_json::Value::Object(map) = &mut data {
                map.insert("screen_seq".into(), pane.screen_seq().into());
            }
            Ok(data)
        }
        Request::Send { text, newline } => {
            // Capture the log size BEFORE injecting input: this is the
            // race-free offset to hand to `expect --since` so it scans only
            // the output the command produces in response.
            let path = handle.bs.output_log_path(&handle.session_id);
            let offset = tokio::fs::metadata(&path)
                .await
                .map(|m| m.len())
                .unwrap_or(0);
            let pane = handle.cmd_pane.lock().await.clone();
            pane.write_input(text.as_bytes());
            let mut sent = text.len();
            if newline {
                pane.write_input(b"\n");
                sent += 1;
            }
            Ok(serde_json::json!({ "sent": sent, "offset": offset }))
        }
        Request::Resize { cols, rows } => {
            let pane = handle.cmd_pane.lock().await.clone();
            pane.resize(rows, cols);
            Ok(serde_json::json!({ "cols": cols, "rows": rows }))
        }
        Request::Kill => {
            let pane = handle.cmd_pane.lock().await.clone();
            pane.kill();
            Ok(serde_json::json!({"killed": true}))
        }
        Request::Restart => {
            handle
                .action_tx
                .send(LoopMessage::Restart)
                .map_err(|_| anyhow!("main loop is gone"))?;
            Ok(serde_json::json!({"restart": "queued"}))
        }
        Request::Detach => {
            // Bump the generation so every attached client's writer wakes.
            let v = *handle.detach_tx.borrow();
            let _ = handle.detach_tx.send(v.wrapping_add(1));
            Ok(serde_json::json!({"detached": true}))
        }
        Request::Attach { .. } => unreachable!("attach handled before dispatch"),
    }
}

/// Serve an attached client: stream PTY output (plus the catch-up backlog)
/// out as frames, and apply the input/resize frames it sends back. Ends when
/// the client disconnects, the session exits, or a forced detach fires.
async fn handle_attach(
    rd: tokio::net::unix::OwnedReadHalf,
    mut wr: tokio::net::unix::OwnedWriteHalf,
    handle: Handle,
    cols: u16,
    rows: u16,
) -> Result<()> {
    // Track this client so worker shutdown can wait for it to drain.
    handle.attached.fetch_add(1, Ordering::SeqCst);
    let _attached_guard = AttachedGuard(handle.attached.clone());

    // Apply the client's terminal size to the PTY up front.
    if cols > 0 && rows > 0 {
        handle.cmd_pane.lock().await.clone().resize(rows, cols);
    }

    // When a view command is configured, pipe this client's byte stream
    // (backlog + live) through it before rendering; the recorded log and vt100
    // screenshot stay raw. The formatter is killed when `view_child` drops at
    // the end of this fn, so it never outlives the client.
    //
    // Note: under `--view-cmd` the *formatted* live view has no tail-delivery
    // guarantee — on session exit the formatter may still be flushing, so its
    // final bytes can be truncated. The raw recorded log is always complete,
    // so this only affects the transformed on-screen view, not the record.
    // Compute this before spawning the formatter: when the session has already
    // exited, the hub delivers its whole backlog as a single pre-queued
    // snapshot and will never broadcast again, so the formatter's feed task
    // must close stdin after that finite backlog (see `spawn_view_filter`).
    let mut exit_rx = handle.exit_rx.clone();
    let already_exited = exit_rx.borrow().is_some();

    // Subscribe lazily via a closure so the hub subscription only happens
    // once the formatter has actually spawned. If `spawn_view_filter` failed
    // eagerly (before subscribing) it would leave a dead sender parked in
    // `hub.clients` until the next broadcast — which may never come once the
    // session has exited, leaking across repeated attaches with a broken
    // `--view-cmd`.
    let (mut output, view_child) = match handle.view_cmd.as_deref() {
        Some(cmd) if !cmd.trim().is_empty() => {
            match spawn_view_filter(|| handle.hub.subscribe(), cmd, already_exited) {
                Ok((rx, guard)) => (rx, Some(guard)),
                Err(_) => (handle.hub.subscribe(), None),
            }
        }
        _ => (handle.hub.subscribe(), None),
    };
    // Whether output is routed through a formatter. The already-exited EXIT
    // fast-path below only holds for the raw hub stream, not the formatted one.
    let has_view = view_child.is_some();
    let mut detach_rx = handle.detach_tx.subscribe();

    // Grace period bounding the already-exited + `--view-cmd` drain. A
    // cooperative formatter exits on stdin EOF, closing stdout so
    // `output.recv()` returns `None` and we EXIT cleanly. A pathological
    // formatter that ignores EOF (or stalls) would otherwise never close the
    // channel, and since the session already exited `exit_rx.changed()` can
    // never fire — the client would hang forever. This idle timer resets on
    // every forwarded chunk, so a slow-but-streaming formatter is never cut
    // off; it only fires after the formatter goes silent, at which point we
    // deliver EXIT and detach.
    //
    // Tradeoff: the grace period is also the budget for a *buffer-until-EOF*
    // formatter to emit its first byte after we close its stdin. Such a
    // formatter reads the whole (finite) backlog, then does its work, and only
    // then flushes — so if that post-EOF processing exceeds the grace period
    // before any output appears, the idle guard fires and we EXIT early,
    // truncating (or entirely skipping) the transformed backlog. The raw
    // recorded log is always complete, so this only ever affects the on-screen
    // formatted view of an already-exited session. We keep the window generous
    // enough to accommodate a slow full-backlog transform while still bounding
    // a genuinely hung formatter.
    const VIEW_DRAIN_IDLE_GRACE: std::time::Duration = std::time::Duration::from_secs(15);

    // Reader half: client → PTY (input/resize). Runs as its own task so a
    // read mid-frame is never cancelled by the writer's select.
    let gone = Arc::new(Notify::new());
    let reader = {
        let gone = gone.clone();
        let handle = handle.clone();
        tokio::spawn(async move {
            let mut rd = rd;
            loop {
                match attach::read_frame(&mut rd).await {
                    Ok(Some((C_INPUT, payload))) => {
                        handle.cmd_pane.lock().await.clone().write_input(&payload);
                    }
                    Ok(Some((C_RESIZE, payload))) if payload.len() == 4 => {
                        let cols = u16::from_be_bytes([payload[0], payload[1]]);
                        let rows = u16::from_be_bytes([payload[2], payload[3]]);
                        handle.cmd_pane.lock().await.clone().resize(rows, cols);
                    }
                    Ok(Some(_)) => {}
                    Ok(None) | Err(_) => break,
                }
            }
            gone.notify_one();
        })
    };

    loop {
        // Arm the idle-drain timeout only for the already-exited view-cmd path;
        // otherwise wait forever so the timer never fires.
        let idle_guard = async {
            if already_exited && has_view {
                tokio::time::sleep(VIEW_DRAIN_IDLE_GRACE).await;
            } else {
                std::future::pending::<()>().await;
            }
        };
        tokio::select! {
            biased;
            // Drain queued output (backlog + live) before honoring exit, so
            // the client never loses the tail.
            data = output.recv() => match data {
                Some(bytes) => {
                    if attach::write_frame(&mut wr, S_OUTPUT, &bytes).await.is_err() {
                        break;
                    }
                }
                None => {
                    // Output stream closed. Re-check the live session state so
                    // we always send a well-formed terminal frame instead of
                    // just dropping the socket (which the client would misread
                    // as an unexpected worker death, `Ok(None) | Err(_)`).
                    let info = *exit_rx.borrow();
                    if info.is_some() {
                        // The session has exited: either the already-exited
                        // view-cmd drain completed (formatter saw EOF after the
                        // finite backlog and dropped its sender) or the session
                        // exited concurrently. Deliver EXIT so the formatted
                        // backlog's final frame is the recorded exit.
                        let _ = attach::write_frame(&mut wr, S_EXIT, &attach::exit_payload(info))
                            .await;
                    } else {
                        // The session is still running but the output channel
                        // closed — e.g. a `--view-cmd` formatter exited/crashed
                        // early. This is not worker death, so detach cleanly:
                        // the client restores the terminal and the session
                        // keeps running.
                        let _ = attach::write_frame(&mut wr, S_DETACHED, &[]).await;
                    }
                    break;
                }
            },
            _ = exit_rx.changed() => {
                let info = *exit_rx.borrow();
                if info.is_some() {
                    let _ = attach::write_frame(&mut wr, S_EXIT, &attach::exit_payload(info)).await;
                    break;
                }
            },
            _ = detach_rx.changed() => {
                let _ = attach::write_frame(&mut wr, S_DETACHED, &[]).await;
                break;
            },
            _ = gone.notified() => break,
            _ = idle_guard => {
                // Already-exited view-cmd formatter went idle without closing
                // stdout — assume the finite backlog is fully formatted (or the
                // formatter is stuck) and deliver EXIT instead of hanging.
                let info = *exit_rx.borrow();
                let _ = attach::write_frame(&mut wr, S_EXIT, &attach::exit_payload(info)).await;
                break;
            }
        }
        // Raw already-exited fast-path: the hub delivers its whole backlog as
        // one pre-queued chunk, so once the channel is drained we can EXIT
        // immediately. The view-cmd path must NOT use this heuristic — the
        // formatter's output can be momentarily empty while more formatted
        // bytes are still in flight (or buffered pending EOF), which would
        // prematurely EXIT and truncate the formatted backlog. That path
        // instead relies on the channel-close signal handled above.
        if already_exited && !has_view && output.is_empty() {
            let info = *exit_rx.borrow();
            let _ = attach::write_frame(&mut wr, S_EXIT, &attach::exit_payload(info)).await;
            break;
        }
    }

    reader.abort();
    Ok(())
}

/// Owns the per-client view-cmd formatter and its two pump tasks. On drop
/// (client detached/exited) it kills the formatter and aborts the pumps, so a
/// formatter process/task never outlives the client it was serving.
struct ViewChild {
    child: Option<tokio::process::Child>,
    pumps: [tokio::task::JoinHandle<()>; 2],
}

impl Drop for ViewChild {
    fn drop(&mut self) {
        for p in &self.pumps {
            p.abort();
        }
        if let Some(mut child) = self.child.take() {
            // Reap the formatter so it never lingers as a zombie. Prefer a
            // background task that both kills and awaits the child: `kill()`
            // issues the signal and then `wait()`s, so a transient
            // `start_kill` failure is retried and the child is always reaped
            // when a runtime is available. Fall back to `start_kill` +
            // `kill_on_drop` (set at spawn) reaping via tokio's orphan queue
            // only when no runtime handle exists (e.g. dropped outside an
            // async context).
            match tokio::runtime::Handle::try_current() {
                Ok(rt) => {
                    rt.spawn(async move {
                        let _ = child.kill().await;
                    });
                }
                Err(_) => {
                    let _ = child.start_kill();
                    drop(child);
                }
            }
        }
    }
}

/// Spawn `sh -c <view_cmd>` and splice it between the hub and an attached
/// client: hub bytes are written to the formatter's stdin, and its stdout is
/// forwarded to the client. Returns the display-byte receiver plus a
/// `ViewChild` guard that tears everything down when dropped.
///
/// `subscribe` is only invoked *after* the formatter has spawned and its
/// stdio pipes are wired, so a spawn failure never registers (and then
/// orphans) a client sender in the hub.
///
/// When `finite_backlog` is set the session has already exited: the hub
/// delivers its whole backlog as a single pre-queued snapshot and will never
/// broadcast again (its sender stays parked in the hub, so `recv` would block
/// forever). In that case the feed task drains only what is already queued and
/// then closes stdin, so the formatter sees EOF, flushes any buffered output,
/// and exits — letting the drain task close the client channel to signal that
/// the formatted backlog has been fully delivered.
fn spawn_view_filter(
    subscribe: impl FnOnce() -> mpsc::UnboundedReceiver<Vec<u8>>,
    view_cmd: &str,
    finite_backlog: bool,
) -> Result<(mpsc::UnboundedReceiver<Vec<u8>>, ViewChild)> {
    use tokio::io::AsyncReadExt;
    let mut child = tokio::process::Command::new("sh")
        .arg("-c")
        .arg(view_cmd)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .context("spawning view-cmd")?;
    let mut stdin = child.stdin.take().context("view-cmd stdin")?;
    let mut stdout = child.stdout.take().context("view-cmd stdout")?;
    // All fallible setup is done; now it is safe to register with the hub.
    let mut output = subscribe();
    let (tx, rx) = mpsc::unbounded_channel::<Vec<u8>>();

    // hub → formatter stdin. `write_all` already pushes each chunk into the
    // pipe, so no per-chunk `flush` is needed; dropping `stdin` at the end of
    // this task closes the pipe and delivers EOF to the formatter.
    let feed = tokio::spawn(async move {
        if finite_backlog {
            // Already-exited: drain only the pre-queued backlog snapshot
            // (non-blocking) then fall through to drop stdin, delivering EOF.
            // `recv().await` would block forever on the parked hub sender.
            while let Ok(chunk) = output.try_recv() {
                if stdin.write_all(&chunk).await.is_err() {
                    break;
                }
            }
        } else {
            while let Some(chunk) = output.recv().await {
                if stdin.write_all(&chunk).await.is_err() {
                    break;
                }
            }
        }
        // stdin dropped here → EOF to the formatter.
    });

    // formatter stdout → client. Ends when the formatter exits (EOF) or the
    // client's receiver is dropped.
    let drain = tokio::spawn(async move {
        let mut buf = [0u8; 8192];
        loop {
            match stdout.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if tx.send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
            }
        }
    });

    Ok((
        rx,
        ViewChild {
            child: Some(child),
            pumps: [feed, drain],
        },
    ))
}

async fn read_log(path: &Path, tail: Option<usize>, raw: bool) -> Result<serde_json::Value> {
    let bytes = match tokio::fs::read(path).await {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(e) => return Err(e.into()),
    };
    let processed = if raw {
        bytes
    } else {
        strip_ansi_escapes::strip(&bytes)
    };
    let text = String::from_utf8_lossy(&processed).into_owned();
    let out = match tail {
        Some(n) => last_n_lines(&text, n),
        None => text,
    };
    Ok(serde_json::json!({"text": out}))
}

/// Return the last `n` lines of `text`, preserving the original bytes.
///
/// A single trailing newline terminates the final line rather than starting
/// an empty one, so `last_n_lines("a\nb\nc\n", 2)` is `"b\nc\n"` (two lines),
/// not `"c\n"`.
pub fn last_n_lines(text: &str, n: usize) -> String {
    if n == 0 {
        return String::new();
    }
    let trimmed = text.strip_suffix('\n').unwrap_or(text);
    let mut start = 0;
    for (seen, (i, _)) in trimmed.rmatch_indices('\n').enumerate() {
        if seen + 1 == n {
            start = i + 1;
            break;
        }
    }
    text[start..].to_string()
}

/// Decrements the attached-client counter when an attach handler ends, on
/// any exit path.
struct AttachedGuard(Arc<AtomicUsize>);

impl Drop for AttachedGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Best-effort cleanup: remove the socket file. Called on graceful shutdown.
pub fn cleanup(bs: &Babysit, session_id: &str) {
    let _ = std::fs::remove_file(bs.control_socket_path(session_id));
    let _ = std::fs::remove_file(bs.legacy_control_socket_path(session_id));
}

#[cfg(test)]
mod tests {
    use super::{ensure_private_dir, last_n_lines, secure_socket, spawn_view_filter};
    use crate::paths::Babysit;
    use std::os::unix::fs::PermissionsExt;
    use tokio::net::UnixListener;
    use tokio::sync::mpsc;

    #[tokio::test]
    async fn hashed_socket_for_long_root_binds_in_private_directory() {
        let bs = Babysit::new(format!("/tmp/{}", "long-root-".repeat(20)));
        let path = bs.control_socket_path(&"x".repeat(64));
        let root_dir = path.parent().unwrap();
        let user_dir = root_dir.parent().unwrap();
        ensure_private_dir(user_dir).unwrap();
        ensure_private_dir(root_dir).unwrap();
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path).unwrap();
        secure_socket(&path).unwrap();
        assert!(path.as_os_str().as_encoded_bytes().len() < 100);
        assert_eq!(
            std::fs::metadata(root_dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        drop(listener);
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn view_filter_pipes_backlog_and_live_through_command() {
        let (tx, rx) = mpsc::unbounded_channel::<Vec<u8>>();
        // Queue a backlog chunk plus a "live" chunk before the filter drains.
        tx.send(b"abc".to_vec()).unwrap();
        tx.send(b"def".to_vec()).unwrap();
        // An uppercasing filter proves bytes actually flow through `sh -c`.
        let (mut out, guard) = spawn_view_filter(|| rx, "tr a-z A-Z", false).unwrap();
        // Close the hub side so the formatter sees EOF, flushes, and exits.
        drop(tx);
        let mut got = Vec::new();
        while let Some(chunk) = out.recv().await {
            got.extend_from_slice(&chunk);
        }
        assert_eq!(got, b"ABCDEF");
        drop(guard);
    }

    #[tokio::test]
    async fn view_filter_finite_backlog_closes_stdin_without_dropping_hub() {
        // Simulate an already-exited session: the hub pre-queues the whole
        // backlog as one snapshot and keeps its sender parked (never dropped).
        let (tx, rx) = mpsc::unbounded_channel::<Vec<u8>>();
        tx.send(b"hello".to_vec()).unwrap();
        // `cat` streams its input, but with stdin never reaching EOF it would
        // block open forever. It exits (closing stdout, ending the receiver
        // loop below) only once stdin is closed — proving the feed task closes
        // stdin after the finite backlog even though the hub sender is still
        // alive.
        let (mut out, guard) = spawn_view_filter(|| rx, "cat", true).unwrap();
        let mut got = Vec::new();
        while let Some(chunk) = out.recv().await {
            got.extend_from_slice(&chunk);
        }
        assert_eq!(got, b"hello");
        // The hub sender is deliberately kept alive the whole time.
        drop(tx);
        drop(guard);
    }

    #[test]
    fn tail_respects_trailing_newline() {
        // 3 logical lines + trailing newline: tail 2 keeps the last two.
        assert_eq!(last_n_lines("a\nb\nc\n", 2), "b\nc\n");
        assert_eq!(last_n_lines("a\nb\nc\n", 1), "c\n");
    }

    #[test]
    fn tail_without_trailing_newline() {
        assert_eq!(last_n_lines("a\nb\nc", 2), "b\nc");
    }

    #[test]
    fn tail_larger_than_available_returns_all() {
        assert_eq!(last_n_lines("a\nb\n", 10), "a\nb\n");
    }

    #[test]
    fn tail_zero_is_empty() {
        assert_eq!(last_n_lines("a\nb\n", 0), "");
    }

    #[test]
    fn tail_empty_input() {
        assert_eq!(last_n_lines("", 5), "");
    }
}
