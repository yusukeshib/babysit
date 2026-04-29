//! A `Pane` wraps a PTY pair, the child process, and a `vt100::Parser` for rendering.
//!
//! The reader runs on its own std thread because `portable_pty`'s reader is
//! blocking and not `Send` across `await` points. The writer is also sync, but
//! we only call it from the main loop in small bursts, which is fine.

use anyhow::{Context, Result};
use portable_pty::{Child, CommandBuilder, MasterPty, NativePtySystem, PtySize, PtySystem};
use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};
use std::thread;

pub struct Pane {
    pub parser: Arc<RwLock<vt100::Parser>>,
    pub writer: Mutex<Box<dyn Write + Send>>,
    pub master: Mutex<Box<dyn MasterPty + Send>>,
    pub child: Arc<Mutex<Box<dyn Child + Send + Sync>>>,
    /// Latest known exit status, set by the wait thread when the child exits.
    pub exit_status: Arc<Mutex<Option<ExitInfo>>>,
    /// Notifier so the UI loop can redraw promptly when PTY output arrives.
    /// The reader and exit-wait threads hold their own clones; this field
    /// keeps the original alive for the lifetime of the pane.
    #[allow(dead_code)]
    pub redraw: Arc<tokio::sync::Notify>,
    /// Last (rows, cols) we resized to. We only call `resize` when this
    /// changes, since resizing has a syscall + SIGWINCH cost.
    last_size: Mutex<(u16, u16)>,
}

#[derive(Debug, Clone, Copy)]
pub struct ExitInfo {
    pub code: Option<i32>,
    /// True if the process was terminated by a signal.
    pub signaled: bool,
}

impl Pane {
    /// Spawn `cmd[0]` with `cmd[1..]` as arguments inside a fresh PTY of the
    /// given size. Extra environment variables are layered on top of the
    /// current process's env.
    pub fn spawn(
        cmd: &[String],
        rows: u16,
        cols: u16,
        extra_env: &[(String, String)],
        redraw: Arc<tokio::sync::Notify>,
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
        // Drop slave — child has it. Keeping it open in the parent prevents
        // EOF on master read when the child exits.
        drop(pair.slave);

        let parser = Arc::new(RwLock::new(vt100::Parser::new(rows, cols, 0)));
        let exit_status: Arc<Mutex<Option<ExitInfo>>> = Arc::new(Mutex::new(None));

        // Reader thread: pump bytes from PTY master into the vt100 parser
        // and (optionally) tee the same bytes to an output-log file.
        let mut reader = pair
            .master
            .try_clone_reader()
            .context("cloning PTY reader")?;
        let log_path: Option<PathBuf> = output_log.map(|p| p.to_path_buf());
        {
            let parser = parser.clone();
            let redraw = redraw.clone();
            thread::spawn(move || {
                let mut log_file = log_path
                    .and_then(|p| OpenOptions::new().create(true).append(true).open(&p).ok());
                let mut buf = [0u8; 8192];
                loop {
                    match reader.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => {
                            if let Ok(mut p) = parser.write() {
                                p.process(&buf[..n]);
                            }
                            if let Some(f) = log_file.as_mut() {
                                let _ = f.write_all(&buf[..n]);
                            }
                            redraw.notify_one();
                        }
                        Err(_) => break,
                    }
                }
                redraw.notify_one();
            });
        }

        let writer = pair.master.take_writer().context("taking PTY writer")?;
        let child = Arc::new(Mutex::new(child));

        // Wait thread: capture exit status when the child finishes.
        {
            let child = child.clone();
            let exit_status = exit_status.clone();
            let redraw = redraw.clone();
            thread::spawn(move || {
                // try_wait in a loop is wasteful; instead we wait once.
                // We have to take the child briefly to call wait().
                // We use a short loop because portable_pty's Child::wait()
                // takes &mut self and may block; we hold the lock during wait.
                let status = {
                    let mut guard = child.lock().unwrap();
                    guard.wait()
                };
                let info = match status {
                    Ok(s) => ExitInfo {
                        code: s.exit_code().try_into().ok(),
                        signaled: !s.success() && s.exit_code() == 0,
                    },
                    Err(_) => ExitInfo {
                        code: None,
                        signaled: true,
                    },
                };
                if let Ok(mut g) = exit_status.lock() {
                    *g = Some(info);
                }
                redraw.notify_one();
            });
        }

        Ok(Self {
            parser,
            writer: Mutex::new(writer),
            master: Mutex::new(pair.master),
            child,
            exit_status,
            redraw,
            last_size: Mutex::new((rows, cols)),
        })
    }

    /// Forward raw bytes (e.g. typed characters) to the PTY's stdin.
    pub fn write_input(&self, bytes: &[u8]) {
        if let Ok(mut w) = self.writer.lock() {
            let _ = w.write_all(bytes);
            let _ = w.flush();
        }
    }

    /// Resize the PTY (and the parser's screen) to match the body area.
    pub fn resize(&self, rows: u16, cols: u16) {
        let mut last = self.last_size.lock().unwrap();
        if *last == (rows, cols) || rows == 0 || cols == 0 {
            return;
        }
        *last = (rows, cols);
        drop(last);
        if let Ok(m) = self.master.lock() {
            let _ = m.resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            });
        }
        if let Ok(mut p) = self.parser.write() {
            p.screen_mut().set_size(rows, cols);
        }
    }

    /// `Some(_)` once the child has exited.
    pub fn exit_info(&self) -> Option<ExitInfo> {
        self.exit_status.lock().ok().and_then(|g| *g)
    }

    /// Send SIGTERM (best-effort kill).
    pub fn kill(&self) {
        if let Ok(mut g) = self.child.lock() {
            let _ = g.kill();
        }
    }
}
