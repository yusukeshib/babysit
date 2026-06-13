//! A `Pane` wraps a PTY pair, the child process, and the threads that
//! ferry bytes between the master fd and attached clients.
//!
//! Output bytes from the PTY are tee'd to a log file and fanned out through
//! an `OutputHub` to any attached clients. There is no terminal-emulator
//! parser in babysit itself — the client's terminal renders the bytes
//! directly.

use anyhow::{Context, Result};
use portable_pty::{ChildKiller, CommandBuilder, MasterPty, NativePtySystem, PtySize, PtySystem};
use std::collections::VecDeque;
use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
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

#[derive(Default)]
struct HubInner {
    backlog: VecDeque<u8>,
    clients: Vec<UnboundedSender<Vec<u8>>>,
}

impl OutputHub {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Append a chunk to the backlog and push it to every attached client,
    /// dropping any client whose receiver has gone away.
    pub fn broadcast(&self, data: &[u8]) {
        let Ok(mut g) = self.inner.lock() else {
            return;
        };
        g.backlog.extend(data);
        let overflow = g.backlog.len().saturating_sub(BACKLOG_CAP);
        if overflow > 0 {
            g.backlog.drain(..overflow);
        }
        if !g.clients.is_empty() {
            let chunk = data.to_vec();
            g.clients.retain(|tx| tx.send(chunk.clone()).is_ok());
        }
    }

    /// Register a client. Returns a receiver that first yields the current
    /// backlog (if any), then live output.
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
}

pub struct Pane {
    pub writer: Mutex<Box<dyn Write + Send>>,
    /// PTY master, used for resizing. `None` in no-tty (pipe) mode.
    master: Option<Mutex<Box<dyn MasterPty + Send>>>,
    /// Independent signaller for the child. Kept separate from the child
    /// handle (which the wait thread holds locked for the entire duration of
    /// its blocking `wait()`) so `kill()` never has to contend with it.
    killer: Mutex<Box<dyn ChildKiller + Send + Sync>>,
    /// OS process id of the child, if known.
    pub pid: Option<u32>,
    /// Latest known exit status, set by the wait thread when the child exits.
    pub exit_status: Arc<Mutex<Option<ExitInfo>>>,
    /// Notified once when the child exits, so async callers can `await` it.
    pub exit_notify: Arc<tokio::sync::Notify>,
    /// Notified once the reader thread has drained all PTY output (to stdout
    /// and the log) and seen EOF. Lets shutdown wait for the final bytes
    /// instead of racing `process::exit` against the last flush.
    pub reader_done: Arc<tokio::sync::Notify>,
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
        let log_path: Option<PathBuf> = output_log.map(|p| p.to_path_buf());

        // One reader thread per output stream (PTY: 1; pipe: stdout + stderr).
        // `reader_done` fires when the last of them drains and sees EOF.
        let remaining = Arc::new(AtomicUsize::new(readers.len()));
        for reader in readers {
            spawn_output_reader(
                reader,
                log_path.clone(),
                hub.clone(),
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
    }

    /// `Some(_)` once the child has exited.
    pub fn exit_info(&self) -> Option<ExitInfo> {
        self.exit_status.lock().ok().and_then(|g| *g)
    }

    /// Signal the child to terminate (best-effort). Uses the independent
    /// killer so it works even while the wait thread is blocked in `wait()`.
    pub fn kill(&self) {
        if let Ok(mut k) = self.killer.lock() {
            let _ = k.kill();
        }
    }
}

/// Pump one output stream to the hub + log on its own blocking thread. When
/// the last live reader (`remaining` reaching zero) sees EOF, fire
/// `reader_done` so shutdown can wait for the final bytes.
fn spawn_output_reader(
    mut reader: Box<dyn Read + Send>,
    log_path: Option<PathBuf>,
    hub: Arc<OutputHub>,
    remaining: Arc<AtomicUsize>,
    reader_done: Arc<tokio::sync::Notify>,
) {
    thread::spawn(move || {
        // O_APPEND makes concurrent appends (stdout + stderr) safe without a
        // shared lock.
        let mut log_file =
            log_path.and_then(|p| OpenOptions::new().create(true).append(true).open(&p).ok());
        let mut buf = [0u8; 8192];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    hub.broadcast(&buf[..n]);
                    if let Some(f) = log_file.as_mut() {
                        let _ = f.write_all(&buf[..n]);
                    }
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
