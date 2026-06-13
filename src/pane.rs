//! A `Pane` wraps a PTY pair, the child process, and the threads that
//! ferry bytes between the master fd and the user's terminal.
//!
//! Output bytes from the PTY are written straight to stdout (and
//! optionally tee'd to a log file). There is no terminal-emulator parser
//! in babysit itself — the user's terminal renders the bytes directly.

use anyhow::{Context, Result};
use portable_pty::{ChildKiller, CommandBuilder, MasterPty, NativePtySystem, PtySize, PtySystem};
use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;

pub struct Pane {
    pub writer: Mutex<Box<dyn Write + Send>>,
    pub master: Mutex<Box<dyn MasterPty + Send>>,
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
    /// Spawn `cmd[0]` with `cmd[1..]` as arguments inside a fresh PTY of the
    /// given size. PTY output is streamed to stdout (and tee'd to
    /// `output_log` if provided).
    pub fn spawn(
        cmd: &[String],
        rows: u16,
        cols: u16,
        extra_env: &[(String, String)],
        output_log: Option<&Path>,
    ) -> Result<Self> {
        anyhow::ensure!(!cmd.is_empty(), "empty command");

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

        let child = pair
            .slave
            .spawn_command(builder)
            .with_context(|| format!("spawning {:?}", cmd))?;
        // Grab an independent killer + the pid up front, before `child` is
        // moved behind a mutex the wait thread will hold while blocked.
        let killer = child.clone_killer();
        let pid = child.process_id();
        // Drop slave — the child has it. Keeping it open in the parent
        // prevents EOF on master read when the child exits.
        drop(pair.slave);

        let exit_status: Arc<Mutex<Option<ExitInfo>>> = Arc::new(Mutex::new(None));
        let exit_notify = Arc::new(tokio::sync::Notify::new());

        // Reader thread: pump bytes from PTY master → stdout, tee'd to the
        // log file. Runs on its own std thread because portable_pty's reader
        // is blocking and not Send across `await` points.
        let mut reader = pair
            .master
            .try_clone_reader()
            .context("cloning PTY reader")?;
        let log_path: Option<PathBuf> = output_log.map(|p| p.to_path_buf());
        let reader_done = Arc::new(tokio::sync::Notify::new());
        {
            let reader_done = reader_done.clone();
            thread::spawn(move || {
                let mut log_file = log_path
                    .and_then(|p| OpenOptions::new().create(true).append(true).open(&p).ok());
                let stdout = std::io::stdout();
                let mut buf = [0u8; 8192];
                loop {
                    match reader.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => {
                            // Best-effort; if stdout is closed there's nothing to do.
                            let mut out = stdout.lock();
                            let _ = out.write_all(&buf[..n]);
                            let _ = out.flush();
                            if let Some(f) = log_file.as_mut() {
                                let _ = f.write_all(&buf[..n]);
                            }
                        }
                        Err(_) => break,
                    }
                }
                // All output flushed; wake any shutdown awaiter. notify_one
                // arms a permit so a late awaiter still observes completion.
                reader_done.notify_waiters();
                reader_done.notify_one();
            });
        }

        let writer = pair.master.take_writer().context("taking PTY writer")?;
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
            master: Mutex::new(pair.master),
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
    pub fn resize(&self, rows: u16, cols: u16) {
        if rows == 0 || cols == 0 {
            return;
        }
        if let Ok(m) = self.master.lock() {
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
