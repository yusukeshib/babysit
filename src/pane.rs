//! A `Pane` wraps a PTY pair, the child process, and the threads that
//! ferry bytes between the master fd and attached clients.
//!
//! Output bytes from the PTY are tee'd to a log file and fanned out through
//! an `OutputHub` to any attached clients. They are also fed into a `vt100`
//! virtual-terminal parser so `babysit screenshot` can render the current
//! on-screen grid (the client's own terminal still renders the live bytes
//! directly for `attach`).

use crate::cli::ShotFormat;
use anyhow::{Context, Result};
use portable_pty::{ChildKiller, CommandBuilder, MasterPty, NativePtySystem, PtySize, PtySystem};
use std::collections::VecDeque;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

/// Maximum bytes of recent PTY output retained for replay to a freshly
/// attached client, so attaching shows the current screen/context. Older
/// output is still on disk in the session log.
const BACKLOG_CAP: usize = 1 << 20; // 1 MiB

/// Fans PTY output out to attached clients and keeps a bounded backlog so a
/// newly attached client can be caught up. The backlog and client list share
/// one lock, so `subscribe` snapshots the backlog and registers atomically —
/// a client sees the backlog then live output with no gap and no duplicate.
#[derive(Default)]
pub struct OutputHub {
    inner: Mutex<HubInner>,
}

#[derive(Clone, Debug)]
pub struct OutputChunk {
    pub offset: u64,
    pub data: Vec<u8>,
}

pub struct ResumeSubscription {
    pub output: UnboundedReceiver<OutputChunk>,
    pub snapshot_end: u64,
    pub backlog: Option<OutputChunk>,
}

#[derive(Default)]
struct HubInner {
    backlog: VecDeque<u8>,
    backlog_start: u64,
    next_offset: u64,
    log: Option<File>,
    log_error: Option<String>,
    clients: Vec<UnboundedSender<Vec<u8>>>,
    resume_clients: Vec<UnboundedSender<OutputChunk>>,
}

impl OutputHub {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Configure the append-only log once. All output readers write through
    /// the hub so pipe-mode stdout/stderr have one total order and offsets map
    /// exactly to bytes in the persisted log.
    pub fn configure_log(&self, path: &Path) -> std::io::Result<()> {
        let mut g = self
            .inner
            .lock()
            .map_err(|_| std::io::Error::other("output hub poisoned"))?;
        if g.log.is_none() {
            let file = OpenOptions::new().create(true).append(true).open(path)?;
            g.next_offset = file.metadata()?.len();
            g.backlog_start = g.next_offset;
            g.log = Some(file);
            g.log_error = None;
        }
        Ok(())
    }

    /// Append a chunk to the log/backlog and push it to every attached client.
    pub fn broadcast(&self, data: &[u8]) {
        let Ok(mut g) = self.inner.lock() else {
            return;
        };
        if let Some(log) = g.log.as_mut()
            && let Err(error) = log.write_all(data)
        {
            g.log_error = Some(error.to_string());
            g.log = None;
        }
        let offset = g.next_offset;
        g.next_offset = g.next_offset.saturating_add(data.len() as u64);
        g.backlog.extend(data);
        let overflow = g.backlog.len().saturating_sub(BACKLOG_CAP);
        if overflow > 0 {
            g.backlog.drain(..overflow);
            g.backlog_start = g.backlog_start.saturating_add(overflow as u64);
        }
        if !g.clients.is_empty() {
            let bytes = data.to_vec();
            g.clients.retain(|tx| tx.send(bytes.clone()).is_ok());
        }
        if !g.resume_clients.is_empty() {
            let chunk = OutputChunk {
                offset,
                data: data.to_vec(),
            };
            g.resume_clients.retain(|tx| tx.send(chunk.clone()).is_ok());
        }
    }

    /// Register a legacy client. It receives bounded backlog then live bytes.
    pub fn subscribe(&self) -> UnboundedReceiver<Vec<u8>> {
        let (tx, rx) = unbounded_channel();
        if let Ok(mut g) = self.inner.lock() {
            if !g.backlog.is_empty() {
                let snapshot: Vec<u8> = g.backlog.iter().copied().collect();
                let _ = tx.send(snapshot);
            }
            g.clients.push(tx);
        }
        rx
    }

    /// Atomically capture the current end and subscribe to subsequent chunks.
    /// A fresh client also receives the bounded backlog with its true offset;
    /// resumed clients replay the older range from output.log before draining
    /// the queued live receiver.
    pub fn subscribe_resumable(&self, since: Option<u64>) -> Result<ResumeSubscription> {
        let (tx, rx) = unbounded_channel();
        let mut g = self
            .inner
            .lock()
            .map_err(|_| anyhow::anyhow!("output hub poisoned"))?;
        if let Some(error) = &g.log_error {
            anyhow::bail!("output log is unavailable for resume: {error}");
        }
        if since.is_some_and(|offset| offset > g.next_offset) {
            anyhow::bail!("resume offset is beyond output end");
        }
        let backlog = if since.is_none() && !g.backlog.is_empty() {
            Some(OutputChunk {
                offset: g.backlog_start,
                data: g.backlog.iter().copied().collect(),
            })
        } else {
            None
        };
        let snapshot_end = g.next_offset;
        g.resume_clients.push(tx);
        Ok(ResumeSubscription {
            output: rx,
            snapshot_end,
            backlog,
        })
    }
}

pub struct Pane {
    pub writer: Mutex<Box<dyn Write + Send>>,
    /// PTY master, used for resizing. `None` in no-tty (pipe) mode.
    master: Option<Mutex<Box<dyn MasterPty + Send>>>,
    /// Independent signaller for the child. Kept separate from the child
    /// handle (which the wait thread holds locked for the entire duration of
    /// its blocking `wait()`) so termination never has to contend with it.
    /// On Unix we prefer signaling the isolated child process group via `pid`;
    /// this remains the cross-platform fallback.
    killer: Mutex<Box<dyn ChildKiller + Send + Sync>>,
    /// OS process id of the child, if known. On Unix the child is also the
    /// leader of an isolated process group, so descendants can be terminated
    /// together instead of leaking after their parent exits.
    pub pid: Option<u32>,
    /// Latest known exit status, set by the wait thread when the child exits.
    pub exit_status: Arc<Mutex<Option<ExitInfo>>>,
    /// Notified once when the child exits, so async callers can `await` it.
    pub exit_notify: Arc<tokio::sync::Notify>,
    /// Notified once the reader thread has drained all PTY output (to stdout
    /// and the log) and seen EOF. Lets shutdown wait for the final bytes
    /// instead of racing `process::exit` against the last flush.
    pub reader_done: Arc<tokio::sync::Notify>,
    /// Virtual terminal: every output byte is fed here so we can render the
    /// current visible screen for `babysit screenshot`.
    screen: Arc<Mutex<vt100::Parser>>,
    /// Output activity counters, updated by the reader thread(s).
    activity: Arc<Activity>,
}

/// Cheap, lock-free probes for "has output changed?" and "how long since the
/// last output?", shared with the reader thread(s).
///
/// * `seq` increments on every output chunk, so an agent can poll `status`
///   and tell whether the screen moved without re-fetching a screenshot.
/// * `last_ms` is the epoch-millis timestamp of the most recent output (seeded
///   at spawn), so the worker can enforce an idle timeout.
pub struct Activity {
    pub seq: AtomicU64,
    pub last_ms: AtomicU64,
}

fn now_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[derive(Debug, Clone, Copy)]
pub struct ExitInfo {
    pub code: Option<i32>,
    /// True if the process was terminated by a signal.
    pub signaled: bool,
}

impl Pane {
    /// Spawn `cmd[0]` with `cmd[1..]` as arguments. With `tty` it runs inside
    /// a fresh PTY of the given size (so interactive programs behave); without
    /// it the process is run with plain pipes (so programs that detect a
    /// non-tty emit clean, line-oriented output). Output is fanned out through
    /// `hub` to attached clients and tee'd to `output_log` if provided.
    pub fn spawn(
        cmd: &[String],
        rows: u16,
        cols: u16,
        extra_env: &[(String, String)],
        output_log: Option<&Path>,
        hub: Arc<OutputHub>,
        tty: bool,
    ) -> Result<Self> {
        anyhow::ensure!(!cmd.is_empty(), "empty command");

        // Each backend yields: the child handle, a writer for its stdin, an
        // optional PTY master (for resize), and one or more output readers.
        let child: Box<dyn portable_pty::Child + Send + Sync>;
        let writer: Box<dyn Write + Send>;
        let master: Option<Mutex<Box<dyn MasterPty + Send>>>;
        let mut readers: Vec<Box<dyn Read + Send>> = Vec::new();

        if tty {
            let pty_system = NativePtySystem::default();
            let pair = pty_system
                .openpty(PtySize {
                    rows,
                    cols,
                    pixel_width: 0,
                    pixel_height: 0,
                })
                .context("openpty failed")?;

            let mut builder = CommandBuilder::new(&cmd[0]);
            for arg in &cmd[1..] {
                builder.arg(arg);
            }
            if let Ok(cwd) = std::env::current_dir() {
                builder.cwd(cwd);
            }
            for (k, v) in extra_env {
                builder.env(k, v);
            }

            let spawned = pair
                .slave
                .spawn_command(builder)
                .with_context(|| format!("spawning {:?}", cmd))?;
            // Drop slave — the child has it. Keeping it open in the parent
            // prevents EOF on master read when the child exits.
            drop(pair.slave);

            readers.push(
                pair.master
                    .try_clone_reader()
                    .context("cloning PTY reader")?,
            );
            writer = pair.master.take_writer().context("taking PTY writer")?;
            master = Some(Mutex::new(pair.master));
            child = spawned;
        } else {
            use std::process::{Command, Stdio};
            let mut c = Command::new(&cmd[0]);
            c.args(&cmd[1..]);
            if let Ok(cwd) = std::env::current_dir() {
                c.current_dir(cwd);
            }
            for (k, v) in extra_env {
                c.env(k, v);
            }
            c.stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            // PTY children call setsid() inside portable-pty. Pipe-mode
            // children need equivalent isolation before we can safely signal
            // their process group without also signaling this supervisor.
            #[cfg(unix)]
            {
                use std::os::unix::process::CommandExt;
                c.process_group(0);
            }
            let mut spawned = c.spawn().with_context(|| format!("spawning {:?}", cmd))?;
            writer = Box::new(spawned.stdin.take().context("taking child stdin")?);
            readers.push(Box::new(
                spawned.stdout.take().context("taking child stdout")?,
            ));
            readers.push(Box::new(
                spawned.stderr.take().context("taking child stderr")?,
            ));
            // portable_pty implements Child/ChildKiller for std::process::Child,
            // so the wait/kill machinery below is identical to the PTY path.
            master = None;
            child = Box::new(spawned);
        }

        // Grab an independent killer + the pid up front, before `child` is
        // moved behind a mutex the wait thread will hold while blocked.
        let killer = child.clone_killer();
        let pid = child.process_id();

        let exit_status: Arc<Mutex<Option<ExitInfo>>> = Arc::new(Mutex::new(None));
        let exit_notify = Arc::new(tokio::sync::Notify::new());
        let reader_done = Arc::new(tokio::sync::Notify::new());
        if let Some(path) = output_log {
            hub.configure_log(path)
                .with_context(|| format!("opening output log {}", path.display()))?;
        }
        // Virtual terminal sized to the PTY (no scrollback: a screenshot is a
        // single visible frame). Kept in sync with the PTY via `resize`.
        let screen = Arc::new(Mutex::new(vt100::Parser::new(rows, cols, 0)));
        // Seed `last_ms` at spawn so idle time is measured from start, not from
        // the first byte of output.
        let activity = Arc::new(Activity {
            seq: AtomicU64::new(0),
            last_ms: AtomicU64::new(now_ms()),
        });

        // One reader thread per output stream (PTY: 1; pipe: stdout + stderr).
        // `reader_done` fires when the last of them drains and sees EOF.
        let remaining = Arc::new(AtomicUsize::new(readers.len()));
        for reader in readers {
            spawn_output_reader(
                reader,
                hub.clone(),
                screen.clone(),
                activity.clone(),
                remaining.clone(),
                reader_done.clone(),
            );
        }

        let child = Arc::new(Mutex::new(child));

        // Wait thread: capture exit status when the child finishes and
        // wake any awaiter.
        {
            let child = child.clone();
            let exit_status = exit_status.clone();
            let exit_notify = exit_notify.clone();
            thread::spawn(move || {
                let status = {
                    let mut guard = child.lock().unwrap();
                    guard.wait()
                };
                let info = match status {
                    Ok(s) => {
                        // portable_pty reports signal termination via
                        // `signal()`; the numeric `exit_code()` is a
                        // placeholder (1) in that case, so don't surface it.
                        let signaled = s.signal().is_some();
                        ExitInfo {
                            code: if signaled {
                                None
                            } else {
                                s.exit_code().try_into().ok()
                            },
                            signaled,
                        }
                    }
                    Err(_) => ExitInfo {
                        code: None,
                        signaled: true,
                    },
                };
                if let Ok(mut g) = exit_status.lock() {
                    *g = Some(info);
                }
                exit_notify.notify_waiters();
                // Also notify any future awaiter (notify_one stays armed
                // until consumed, unlike notify_waiters).
                exit_notify.notify_one();
            });
        }

        Ok(Self {
            writer: Mutex::new(writer),
            master,
            killer: Mutex::new(killer),
            pid,
            exit_status,
            exit_notify,
            reader_done,
            screen,
            activity,
        })
    }

    /// Forward raw bytes (typed characters or text from `babysit send`) to
    /// the PTY's stdin.
    pub fn write_input(&self, bytes: &[u8]) {
        if let Ok(mut w) = self.writer.lock() {
            let _ = w.write_all(bytes);
            let _ = w.flush();
        }
    }

    /// Resize the PTY (and its line discipline) to the given dimensions.
    /// No-op in no-tty (pipe) mode, which has no PTY.
    pub fn resize(&self, rows: u16, cols: u16) {
        if rows == 0 || cols == 0 {
            return;
        }
        if let Some(master) = &self.master
            && let Ok(m) = master.lock()
        {
            let _ = m.resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            });
        }
        // Keep the virtual terminal in lock-step with the PTY so screenshots
        // reflect the dimensions the program is actually drawing for.
        if let Ok(mut s) = self.screen.lock() {
            s.screen_mut().set_size(rows, cols);
        }
    }

    /// Render the current visible screen of the virtual terminal in the
    /// requested `format`. See `render_screen` for the output shape.
    pub fn screenshot(&self, format: ShotFormat, trim: bool) -> serde_json::Value {
        match self.screen.lock() {
            Ok(p) => crate::render::render_screen(p.screen(), format, trim),
            Err(_) => serde_json::json!({ "error": "screen lock poisoned" }),
        }
    }

    /// `Some(_)` once the child has exited.
    pub fn exit_info(&self) -> Option<ExitInfo> {
        self.exit_status.lock().ok().and_then(|g| *g)
    }

    /// Monotonic counter of output chunks seen so far. An agent can compare it
    /// across `status` polls to cheaply tell whether the screen has moved.
    pub fn screen_seq(&self) -> u64 {
        self.activity.seq.load(Ordering::Relaxed)
    }

    /// Milliseconds since the most recent output (or since spawn if none yet).
    pub fn idle_ms(&self) -> u64 {
        now_ms().saturating_sub(self.activity.last_ms.load(Ordering::Relaxed))
    }

    /// Ask the isolated process group to terminate gracefully. On Unix portable-pty's
    /// cloned killer only sends SIGHUP to the direct child and never escalates;
    /// signal the isolated process group ourselves so descendants receive it.
    pub fn kill(&self) -> Result<()> {
        #[cfg(unix)]
        if let Some(pid) = self.pid {
            return signal_process_group(pid, nix::sys::signal::Signal::SIGHUP);
        }

        let mut killer = self
            .killer
            .lock()
            .map_err(|_| anyhow::anyhow!("child killer lock is poisoned"))?;
        killer.kill().context("signaling child")
    }

    /// True while any process remains in this command's isolated process
    /// group. A shell can exit after SIGHUP while another group member ignores
    /// it, so direct-child exit alone is not sufficient confirmation.
    pub fn process_group_alive(&self) -> Result<bool> {
        #[cfg(unix)]
        if let Some(pid) = self.pid {
            use nix::errno::Errno;
            use nix::sys::signal::kill;
            use nix::unistd::Pid;
            return match kill(Pid::from_raw(-(pid as i32)), None) {
                Ok(()) | Err(Errno::EPERM) => Ok(true),
                Err(Errno::ESRCH) => Ok(false),
                Err(error) => Err(error).with_context(|| format!("probing process group {pid}")),
            };
        }

        Ok(self.exit_info().is_none())
    }

    /// Force the isolated process group to stop after graceful termination did
    /// not work. The caller must still wait for both direct-child exit and
    /// process-group disappearance before reporting success.
    pub fn force_kill(&self) -> Result<()> {
        #[cfg(unix)]
        if let Some(pid) = self.pid {
            return signal_process_group(pid, nix::sys::signal::Signal::SIGKILL);
        }

        let mut killer = self
            .killer
            .lock()
            .map_err(|_| anyhow::anyhow!("child killer lock is poisoned"))?;
        killer.kill().context("force-killing child")
    }
}

#[cfg(unix)]
fn signal_process_group(pid: u32, signal: nix::sys::signal::Signal) -> Result<()> {
    use nix::errno::Errno;
    use nix::sys::signal::killpg;
    use nix::unistd::Pid;

    match killpg(Pid::from_raw(pid as i32), signal) {
        Ok(()) | Err(Errno::ESRCH) => Ok(()),
        Err(error) => {
            Err(error).with_context(|| format!("sending {signal:?} to process group {pid}"))
        }
    }
}

/// Pump one output stream to the hub + log on its own blocking thread. When
/// the last live reader (`remaining` reaching zero) sees EOF, fire
/// `reader_done` so shutdown can wait for the final bytes.
fn spawn_output_reader(
    mut reader: Box<dyn Read + Send>,
    hub: Arc<OutputHub>,
    screen: Arc<Mutex<vt100::Parser>>,
    activity: Arc<Activity>,
    remaining: Arc<AtomicUsize>,
    reader_done: Arc<tokio::sync::Notify>,
) {
    thread::spawn(move || {
        let mut buf = [0u8; 8192];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    activity.seq.fetch_add(1, Ordering::Relaxed);
                    activity.last_ms.store(now_ms(), Ordering::Relaxed);
                    if let Ok(mut p) = screen.lock() {
                        p.process(&buf[..n]);
                    }
                    hub.broadcast(&buf[..n]);
                }
                Err(_) => break,
            }
        }
        if remaining.fetch_sub(1, Ordering::SeqCst) == 1 {
            // notify_one arms a permit so a late awaiter still observes it.
            reader_done.notify_waiters();
            reader_done.notify_one();
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn resumable_subscription_uses_persisted_offsets_then_live_chunks() {
        let path = std::env::temp_dir().join(format!(
            "babysit-output-hub-{}-{}.log",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ));
        let hub = OutputHub::new();
        hub.configure_log(&path).unwrap();
        hub.broadcast(b"abc");
        let mut resumed = hub.subscribe_resumable(Some(1)).unwrap();
        assert_eq!(resumed.snapshot_end, 3);
        assert!(resumed.backlog.is_none());
        assert_eq!(std::fs::read(&path).unwrap(), b"abc");

        hub.broadcast(b"de");
        let live = resumed.output.recv().await.unwrap();
        assert_eq!(live.offset, 3);
        assert_eq!(live.data, b"de");

        let fresh = hub.subscribe_resumable(None).unwrap();
        let backlog = fresh.backlog.unwrap();
        assert_eq!(backlog.offset, 0);
        assert_eq!(backlog.data, b"abcde");
        assert!(hub.subscribe_resumable(Some(6)).is_err());
        let _ = std::fs::remove_file(path);
    }
}
