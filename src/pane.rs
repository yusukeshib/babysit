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
    /// Virtual terminal: every output byte is fed here so we can render the
    /// current visible screen for `babysit screenshot`.
    screen: Arc<Mutex<vt100::Parser>>,
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
        // Virtual terminal sized to the PTY (no scrollback: a screenshot is a
        // single visible frame). Kept in sync with the PTY via `resize`.
        let screen = Arc::new(Mutex::new(vt100::Parser::new(rows, cols, 0)));

        // One reader thread per output stream (PTY: 1; pipe: stdout + stderr).
        // `reader_done` fires when the last of them drains and sees EOF.
        let remaining = Arc::new(AtomicUsize::new(readers.len()));
        for reader in readers {
            spawn_output_reader(
                reader,
                log_path.clone(),
                hub.clone(),
                screen.clone(),
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
            Ok(p) => render_screen(p.screen(), format, trim),
            Err(_) => serde_json::json!({ "error": "screen lock poisoned" }),
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
    screen: Arc<Mutex<vt100::Parser>>,
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
                    if let Ok(mut p) = screen.lock() {
                        p.process(&buf[..n]);
                    }
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

/// Fallback size used when rendering a screenshot for a session whose worker
/// is no longer running (we replay the on-disk log through a fresh parser and
/// have no record of the final PTY dimensions).
pub const DEFAULT_SCREENSHOT_SIZE: (u16, u16) = (24, 80);

/// Render a finished session's screen by replaying its raw output log through
/// a fresh virtual terminal. Imperfect (the final PTY size is unknown, so we
/// assume a default), but lets `screenshot` work after the command exits.
pub fn render_log(bytes: &[u8], format: ShotFormat, trim: bool) -> serde_json::Value {
    let (rows, cols) = DEFAULT_SCREENSHOT_SIZE;
    let mut parser = vt100::Parser::new(rows, cols, 0);
    parser.process(bytes);
    render_screen(parser.screen(), format, trim)
}

/// Serialize a vt100 color to a compact, agent-friendly string:
/// `"default"`, `"idxN"` (palette index), or `"#rrggbb"` (true color).
fn color_str(c: vt100::Color) -> String {
    match c {
        vt100::Color::Default => "default".to_string(),
        vt100::Color::Idx(i) => format!("idx{i}"),
        vt100::Color::Rgb(r, g, b) => format!("#{r:02x}{g:02x}{b:02x}"),
    }
}

/// Index of the last row with any visible content (for trimming trailing
/// blank lines). Returns 0 when the whole screen is blank.
fn last_nonblank(lines: &[String]) -> usize {
    lines
        .iter()
        .rposition(|l| !l.trim().is_empty())
        .map_or(0, |i| i + 1)
}

/// Render the visible grid of `screen` in the requested format. All variants
/// carry `rows`, `cols`, and `cursor` so an agent knows the geometry and
/// where focus currently is.
fn render_screen(screen: &vt100::Screen, format: ShotFormat, trim: bool) -> serde_json::Value {
    let (rows, cols) = screen.size();
    let (cur_row, cur_col) = screen.cursor_position();
    let meta = serde_json::json!({
        "rows": rows,
        "cols": cols,
        "cursor": { "row": cur_row, "col": cur_col, "hidden": screen.hide_cursor() },
        "alternate_screen": screen.alternate_screen(),
    });

    match format {
        ShotFormat::Plain => {
            let mut lines: Vec<String> = screen.rows(0, cols).collect();
            if trim {
                lines.truncate(last_nonblank(&lines));
            }
            let mut out = meta;
            out["format"] = "plain".into();
            out["text"] = lines.join("\n").into();
            out
        }
        ShotFormat::Ansi => {
            let formatted: Vec<Vec<u8>> = screen.rows_formatted(0, cols).collect();
            let mut lines: Vec<String> = formatted
                .iter()
                .map(|b| String::from_utf8_lossy(b).into_owned())
                .collect();
            if trim {
                // Decide what to keep from the plain rows (escape codes make a
                // formatted row never look "blank").
                let plain: Vec<String> = screen.rows(0, cols).collect();
                lines.truncate(last_nonblank(&plain));
            }
            // Each formatted row carries its own SGR state but no cursor
            // movement, so a plain newline joins them cleanly; reset at the end
            // so the caller's terminal isn't left in a stray attribute.
            let text = format!("{}\x1b[0m", lines.join("\n"));
            let mut out = meta;
            out["format"] = "ansi".into();
            out["text"] = text.into();
            out
        }
        ShotFormat::Json => {
            // Only emit cells that carry content or non-default styling, so the
            // payload stays small for an agent to read.
            let mut cells = Vec::new();
            for r in 0..rows {
                for c in 0..cols {
                    let Some(cell) = screen.cell(r, c) else {
                        continue;
                    };
                    if cell.is_wide_continuation() {
                        continue;
                    }
                    let styled = cell.bold()
                        || cell.italic()
                        || cell.underline()
                        || cell.inverse()
                        || !matches!(cell.fgcolor(), vt100::Color::Default)
                        || !matches!(cell.bgcolor(), vt100::Color::Default);
                    if !cell.has_contents() && !styled {
                        continue;
                    }
                    let mut obj = serde_json::Map::new();
                    obj.insert("row".into(), r.into());
                    obj.insert("col".into(), c.into());
                    obj.insert("char".into(), cell.contents().into());
                    if !matches!(cell.fgcolor(), vt100::Color::Default) {
                        obj.insert("fg".into(), color_str(cell.fgcolor()).into());
                    }
                    if !matches!(cell.bgcolor(), vt100::Color::Default) {
                        obj.insert("bg".into(), color_str(cell.bgcolor()).into());
                    }
                    for (k, v) in [
                        ("bold", cell.bold()),
                        ("italic", cell.italic()),
                        ("underline", cell.underline()),
                        ("inverse", cell.inverse()),
                    ] {
                        if v {
                            obj.insert(k.into(), true.into());
                        }
                    }
                    cells.push(serde_json::Value::Object(obj));
                }
            }
            let mut out = meta;
            out["format"] = "json".into();
            out["cells"] = cells.into();
            out
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a screen by feeding `bytes` into a fresh parser.
    fn screen_of(bytes: &[u8]) -> vt100::Parser {
        let mut p = vt100::Parser::new(24, 80, 0);
        p.process(bytes);
        p
    }

    #[test]
    fn plain_reflects_in_place_redraw_not_the_raw_stream() {
        // Print 3 lines, move up 2 and overwrite the middle one. The raw
        // stream has 4 lines; the *screen* has 3. (CRLF models the bytes a PTY
        // actually logs, after the line discipline's \n→\r\n translation.)
        let p = screen_of(b"A\r\nB\r\nC\r\n\x1b[2A\x1b[2KB2\r\n");
        let out = render_screen(p.screen(), ShotFormat::Plain, true);
        assert_eq!(out["text"], "A\nB2\nC");
        assert_eq!(out["format"], "plain");
    }

    #[test]
    fn trim_drops_trailing_blank_lines() {
        let p = screen_of(b"hi\r\n");
        let trimmed = render_screen(p.screen(), ShotFormat::Plain, true);
        assert_eq!(trimmed["text"], "hi");
        // Untrimmed keeps the full grid height (24 rows = 23 separators).
        let full = render_screen(p.screen(), ShotFormat::Plain, false);
        let text = full["text"].as_str().unwrap();
        assert!(text.starts_with("hi\n"));
        assert_eq!(text.matches('\n').count(), 23);
    }

    #[test]
    fn json_records_inverse_and_color_for_a_selected_row() {
        // An inverse-video red 'X' — the shape a TUI uses to mark a selection.
        let p = screen_of(b"\x1b[31;7mX\x1b[0m");
        let out = render_screen(p.screen(), ShotFormat::Json, true);
        let cells = out["cells"].as_array().unwrap();
        let cell = &cells[0];
        assert_eq!(cell["char"], "X");
        assert_eq!(cell["fg"], "idx1");
        assert_eq!(cell["inverse"], true);
        // Default-styled blank cells are omitted to keep the payload small.
        assert_eq!(cells.len(), 1);
    }

    #[test]
    fn ansi_preserves_escapes_and_resets_at_end() {
        let p = screen_of(b"\x1b[31mred\x1b[0m");
        let out = render_screen(p.screen(), ShotFormat::Ansi, true);
        let text = out["text"].as_str().unwrap();
        assert!(text.contains('\x1b'), "escapes should be preserved");
        assert!(text.ends_with("\x1b[0m"), "should reset SGR at the end");
    }

    #[test]
    fn render_log_replays_a_finished_session() {
        let out = render_log(b"done\n", ShotFormat::Plain, true);
        assert_eq!(out["text"], "done");
    }
}
