//! Client side of attach/detach plus the small framed wire protocol used on
//! the control socket once a connection upgrades to an attach stream.
//!
//! After a client sends `{"op":"attach","cols":C,"rows":R}\n`, both ends
//! switch to length-prefixed frames: `[tag: u8][len: u32 BE][payload]`.
//!
//!   server → client:  OUTPUT(bytes) · EXIT(signaled u8, code i32 BE) · DETACHED
//!   client → server:  INPUT(bytes) · RESIZE(cols u16 BE, rows u16 BE)
//!
//! The detach hotkey (Ctrl-\ pressed twice) is handled entirely client-side:
//! the client just closes the connection and the worker keeps running.
//! `babysit detach` is the out-of-band equivalent driven from another
//! terminal. Ctrl-\ is used instead of a flow-control key like Ctrl-Q
//! (XON, often swallowed by the terminal) or Ctrl-P (commonly bound by
//! TUIs/shells, e.g. history) so it doesn't collide with the wrapped app.

use crate::pane::ExitInfo;
use crate::paths;
use crate::session::{self, State, Status};
use anyhow::{Context, Result, anyhow};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use std::io::IsTerminal;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::mpsc;

// server → client
pub const S_OUTPUT: u8 = 1;
pub const S_EXIT: u8 = 2;
pub const S_DETACHED: u8 = 3;
// client → server
pub const C_INPUT: u8 = 1;
pub const C_RESIZE: u8 = 2;

/// Detach hotkey: Ctrl-\ (FS, 0x1c) pressed twice in a row. Chosen because
/// it's not a flow-control key and is rarely bound by interactive programs;
/// in raw mode ISIG is off, so it arrives as a byte rather than SIGQUIT.
const DETACH_KEY: u8 = 0x1c;

/// Write one frame: `[tag][len: u32 BE][payload]`.
pub async fn write_frame<W: AsyncWriteExt + Unpin>(
    w: &mut W,
    tag: u8,
    payload: &[u8],
) -> std::io::Result<()> {
    let mut hdr = [0u8; 5];
    hdr[0] = tag;
    hdr[1..].copy_from_slice(&(payload.len() as u32).to_be_bytes());
    w.write_all(&hdr).await?;
    if !payload.is_empty() {
        w.write_all(payload).await?;
    }
    w.flush().await
}

/// Read one frame. `Ok(None)` on a clean EOF (peer closed the connection).
pub async fn read_frame<R: AsyncReadExt + Unpin>(
    r: &mut R,
) -> std::io::Result<Option<(u8, Vec<u8>)>> {
    let mut hdr = [0u8; 5];
    match r.read_exact(&mut hdr).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let tag = hdr[0];
    let len = u32::from_be_bytes([hdr[1], hdr[2], hdr[3], hdr[4]]) as usize;
    let mut payload = vec![0u8; len];
    if len > 0 {
        r.read_exact(&mut payload).await?;
    }
    Ok(Some((tag, payload)))
}

pub fn exit_payload(info: Option<ExitInfo>) -> Vec<u8> {
    let (signaled, code) = match info {
        Some(i) => (
            i.signaled,
            i.code.unwrap_or(if i.signaled { 130 } else { 0 }),
        ),
        None => (true, 130),
    };
    let mut p = Vec::with_capacity(5);
    p.push(signaled as u8);
    p.extend_from_slice(&code.to_be_bytes());
    p
}

fn parse_exit(payload: &[u8]) -> i32 {
    if payload.len() == 5 {
        i32::from_be_bytes([payload[1], payload[2], payload[3], payload[4]])
    } else {
        0
    }
}

fn resize_payload(cols: u16, rows: u16) -> Vec<u8> {
    let mut p = Vec::with_capacity(4);
    p.extend_from_slice(&cols.to_be_bytes());
    p.extend_from_slice(&rows.to_be_bytes());
    p
}

/// Detach an attached terminal from session `id` (the `babysit detach`
/// subcommand). Tells the worker to drop its currently-attached clients.
pub async fn detach(session: Option<String>, json: bool) -> Result<()> {
    let id = session::resolve(session).await?;
    let path = paths::control_socket_path(&id)?;
    let mut stream = UnixStream::connect(&path)
        .await
        .with_context(|| format!("connecting to session {id}"))?;
    stream.write_all(b"{\"op\":\"detach\"}\n").await?;
    stream.flush().await?;
    // Best-effort: drain the one-line response.
    let mut buf = [0u8; 256];
    let _ = stream.read(&mut buf).await;
    if json {
        println!("{}", serde_json::json!({ "detached": true }));
    } else {
        println!("detached clients of session {id}");
    }
    Ok(())
}

/// Resolve a user-supplied selector (id or $BABYSIT_SESSION_ID) and attach to
/// it. Errors if no such session exists — used by `babysit attach`.
pub async fn attach(session: Option<String>) -> Result<i32> {
    let id = session::resolve(session).await?;
    attach_to(id).await
}

/// Attach the current terminal to the session with the exact id `id` and
/// stream until the wrapped command exits, the user detaches (Ctrl-P
/// Ctrl-Q), or `babysit detach` kicks us off. Returns the wrapped command's
/// exit code on exit, else 0.
///
/// Does not pre-check that the session exists: `connect_retry` waits for the
/// worker to bind its socket, so this is safe to call right after spawning a
/// worker (the `babysit run` path) before the session dir is written.
pub async fn attach_to(id: String) -> Result<i32> {
    let path = paths::control_socket_path(&id)?;

    let stream = match connect_retry(&path, &id).await? {
        Some(s) => s,
        // Session already finished before we could attach: print whatever the
        // log captured and report the recorded exit code.
        None => return fallback_finished(&id).await,
    };

    let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
    let mut stream = stream;
    let hello = format!("{{\"op\":\"attach\",\"cols\":{cols},\"rows\":{rows}}}\n");
    stream.write_all(hello.as_bytes()).await?;
    stream.flush().await?;

    // Raw mode only when stdin is a real terminal (skipped under pipes/tests).
    let _raw = if std::io::stdin().is_terminal() {
        RawGuard::enter().ok()
    } else {
        None
    };

    let (mut rd, mut wr) = stream.into_split();

    // Blocking stdin lives on a std thread; it streams chunks to the async
    // side over a channel.
    let (stdin_tx, mut stdin_rx) = mpsc::unbounded_channel::<Vec<u8>>();
    std::thread::spawn(move || {
        use std::io::Read;
        let stdin = std::io::stdin();
        let mut lock = stdin.lock();
        let mut buf = [0u8; 4096];
        loop {
            match lock.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if stdin_tx.send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
            }
        }
    });

    let mut winch = signal(SignalKind::window_change())?;
    let mut saw_ctrl_p = false;
    let exit_code: i32;
    // Whether we left the session running (detached / worker vanished) vs the
    // command actually exiting. On the former the wrapped program is still
    // holding terminal modes (alt-screen, enhanced keyboard, …) that it never
    // tore down for us, so we clean the terminal ourselves afterwards.
    let mut restore_terminal = false;

    loop {
        tokio::select! {
            frame = read_frame(&mut rd) => match frame {
                Ok(Some((S_OUTPUT, payload))) => {
                    use std::io::Write as _;
                    let mut out = std::io::stdout();
                    let _ = out.write_all(&payload);
                    let _ = out.flush();
                }
                Ok(Some((S_EXIT, payload))) => { exit_code = parse_exit(&payload); break; }
                Ok(Some((S_DETACHED, _))) => { exit_code = 0; restore_terminal = true; break; }
                Ok(Some(_)) => {}
                Ok(None) | Err(_) => {
                    // Worker closed unexpectedly; fall back to the recorded code.
                    exit_code = recorded_exit_code(&id).await;
                    restore_terminal = true;
                    break;
                }
            },
            chunk = stdin_rx.recv() => match chunk {
                Some(bytes) => {
                    let (forward, do_detach) = filter_detach(&mut saw_ctrl_p, &bytes);
                    if !forward.is_empty() {
                        write_frame(&mut wr, C_INPUT, &forward).await?;
                    }
                    if do_detach { exit_code = 0; restore_terminal = true; break; }
                }
                None => { /* stdin closed; keep streaming output */ }
            },
            _ = winch.recv() => {
                if let Ok((c, r)) = crossterm::terminal::size() {
                    write_frame(&mut wr, C_RESIZE, &resize_payload(c, r)).await?;
                }
            }
        }
    }

    if restore_terminal {
        restore_terminal_modes();
    }
    Ok(exit_code)
}

/// After detaching (or the worker vanishing), the wrapped program is still
/// running and never reset the terminal modes it had enabled, so the shell we
/// return to would be left in alt-screen / mouse / bracketed-paste /
/// enhanced-keyboard mode. Emit a best-effort cleanup, like tmux does on
/// detach. Harmless if the program hadn't enabled these.
fn restore_terminal_modes() {
    use std::io::Write as _;
    // exit alt screens; show cursor; disable mouse (1000/1002/1003/1006/1015);
    // disable bracketed paste (2004) and focus reporting (1004); pop the kitty
    // keyboard protocol stack; reset SGR; carriage return.
    const CLEANUP: &[u8] = b"\x1b[?1049l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1015l\x1b[?2004l\x1b[?1004l\x1b[<u\x1b[0m\r";
    let mut out = std::io::stdout();
    let _ = out.write_all(CLEANUP);
    let _ = out.flush();
}

/// Connect to the worker's socket, retrying briefly while it binds. Returns
/// `Ok(None)` if the session has already reached a terminal state (so the
/// caller should fall back to the on-disk log + status).
async fn connect_retry(path: &std::path::Path, id: &str) -> Result<Option<UnixStream>> {
    for _ in 0..75 {
        match UnixStream::connect(path).await {
            Ok(s) => return Ok(Some(s)),
            Err(_) => {
                if session_finished(id).await {
                    return Ok(None);
                }
                tokio::time::sleep(Duration::from_millis(40)).await;
            }
        }
    }
    if session_finished(id).await {
        Ok(None)
    } else {
        Err(anyhow!("could not connect to session {id}"))
    }
}

async fn session_finished(id: &str) -> bool {
    session::read_status(id)
        .await
        .map(|s| s.state.is_terminal())
        .unwrap_or(false)
}

async fn recorded_exit_code(id: &str) -> i32 {
    exit_code_from_status(session::read_status(id).await.ok())
}

fn exit_code_from_status(status: Option<Status>) -> i32 {
    match status {
        Some(s) => s
            .exit_code
            .unwrap_or(if s.state == State::Killed { 130 } else { 0 }),
        None => 0,
    }
}

/// The session finished before we attached: dump the captured log and return
/// the recorded exit code, so `babysit run -- <quick cmd>` still behaves.
async fn fallback_finished(id: &str) -> Result<i32> {
    if let Ok(path) = paths::output_log_path(id)
        && let Ok(bytes) = tokio::fs::read(&path).await
    {
        use std::io::Write;
        let mut out = std::io::stdout();
        let _ = out.write_all(&bytes);
        let _ = out.flush();
    }
    Ok(recorded_exit_code(id).await)
}

/// Strip the `Ctrl-\ Ctrl-\` detach sequence from a stdin chunk. Returns the
/// bytes to forward to the PTY and whether the detach sequence completed. A
/// lone `Ctrl-\` is withheld until the next byte (state carried in `saw`);
/// if the next byte isn't another `Ctrl-\`, both are forwarded.
fn filter_detach(saw: &mut bool, chunk: &[u8]) -> (Vec<u8>, bool) {
    let mut out = Vec::with_capacity(chunk.len() + 1);
    for &b in chunk {
        if *saw {
            *saw = false;
            if b == DETACH_KEY {
                // Two in a row → detach; drop both bytes.
                return (out, true);
            }
            // Not the sequence: emit the withheld key, then this byte.
            out.push(DETACH_KEY);
            out.push(b);
        } else if b == DETACH_KEY {
            *saw = true;
        } else {
            out.push(b);
        }
    }
    (out, false)
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

#[cfg(test)]
mod tests {
    use super::filter_detach;

    const K: u8 = 0x1c; // Ctrl-\

    #[test]
    fn passes_normal_input() {
        let mut p = false;
        assert_eq!(filter_detach(&mut p, b"hello"), (b"hello".to_vec(), false));
        assert!(!p);
    }

    #[test]
    fn detects_detach_sequence_in_one_chunk() {
        let mut p = false;
        assert_eq!(filter_detach(&mut p, &[K, K]), (vec![], true));
    }

    #[test]
    fn detects_detach_sequence_across_chunks() {
        let mut p = false;
        assert_eq!(filter_detach(&mut p, &[K]), (vec![], false));
        assert!(p);
        assert_eq!(filter_detach(&mut p, &[K]), (vec![], true));
    }

    #[test]
    fn lone_ctrl_backslash_then_other_is_forwarded() {
        let mut p = false;
        // Ctrl-\ then 'a' → both forwarded, no detach.
        assert_eq!(filter_detach(&mut p, &[K, b'a']), (vec![K, b'a'], false));
        assert!(!p);
    }
}
