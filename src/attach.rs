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
//! Enhanced keyboard protocols are decoded here too: applications such as pi
//! enable Kitty CSI-u reporting, so Ctrl-\ no longer arrives as byte 0x1c.

use crate::pane::ExitInfo;
use crate::paths::Babysit;
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

/// Legacy encoding of the Ctrl-\ detach hotkey (FS, 0x1c), which must be
/// pressed twice. Enhanced keyboard modes encode the same key as an escape
/// sequence; `DetachFilter` handles both forms.
const DETACH_KEY: u8 = 0x1c;
const ESCAPE_SEQUENCE_TIMEOUT: Duration = Duration::from_millis(10);

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
pub async fn detach(bs: &Babysit, session: Option<String>, json: bool) -> Result<()> {
    let id = session::resolve(bs, session).await?;
    let path = bs.control_socket_path(&id);
    let legacy = bs.legacy_control_socket_path(&id);
    let mut stream = match UnixStream::connect(&path).await {
        Ok(stream) => stream,
        Err(_) => UnixStream::connect(&legacy)
            .await
            .with_context(|| format!("connecting to session {id}"))?,
    };
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
pub async fn attach(bs: &Babysit, session: Option<String>) -> Result<i32> {
    let id = session::resolve(bs, session).await?;
    attach_to(bs, id).await
}

/// Attach the current terminal to the session with the exact id `id` and
/// stream until the wrapped command exits, the user detaches (Ctrl-\
/// Ctrl-\), or `babysit detach` kicks us off. Returns the wrapped command's
/// exit code on exit, else 0.
///
/// Does not pre-check that the session exists: `connect_retry` waits for the
/// worker to bind its socket, so this is safe to call right after spawning a
/// worker (the `babysit run` path) before the session dir is written.
pub async fn attach_to(bs: &Babysit, id: String) -> Result<i32> {
    let stream = match connect_retry(bs, &id).await? {
        Some(s) => s,
        // Session already finished before we could attach: print whatever the
        // log captured and report the recorded exit code.
        None => return fallback_finished(bs, &id).await,
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
    // Carries a first Ctrl-\ and partial enhanced-key escape sequences across
    // stdin chunks. A short timeout keeps a lone Escape responsive.
    let mut detach_filter = DetachFilter::default();
    let escape_timeout = tokio::time::sleep(Duration::from_secs(24 * 60 * 60));
    tokio::pin!(escape_timeout);
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
                    exit_code = recorded_exit_code(bs, &id).await;
                    restore_terminal = true;
                    break;
                }
            },
            chunk = stdin_rx.recv() => match chunk {
                Some(bytes) => {
                    let (forward, do_detach) = detach_filter.push(&bytes);
                    if !forward.is_empty() {
                        write_frame(&mut wr, C_INPUT, &forward).await?;
                    }
                    if do_detach { exit_code = 0; restore_terminal = true; break; }
                    if detach_filter.has_partial_escape() {
                        escape_timeout.as_mut().reset(
                            tokio::time::Instant::now() + ESCAPE_SEQUENCE_TIMEOUT,
                        );
                    }
                }
                None => { /* stdin closed; keep streaming output */ }
            },
            _ = &mut escape_timeout, if detach_filter.has_partial_escape() => {
                let forward = detach_filter.flush_partial_escape();
                if !forward.is_empty() {
                    write_frame(&mut wr, C_INPUT, &forward).await?;
                }
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
async fn connect_retry(bs: &Babysit, id: &str) -> Result<Option<UnixStream>> {
    let paths = [
        bs.control_socket_path(id),
        bs.legacy_control_socket_path(id),
    ];
    for _ in 0..75 {
        for path in &paths {
            if let Ok(stream) = UnixStream::connect(path).await {
                return Ok(Some(stream));
            }
        }
        if session_finished(bs, id).await {
            return Ok(None);
        }
        tokio::time::sleep(Duration::from_millis(40)).await;
    }
    if session_finished(bs, id).await {
        Ok(None)
    } else {
        Err(anyhow!("could not connect to session {id}"))
    }
}

async fn session_finished(bs: &Babysit, id: &str) -> bool {
    session::read_status(bs, id)
        .await
        .map(|s| s.state.is_terminal())
        .unwrap_or(false)
}

async fn recorded_exit_code(bs: &Babysit, id: &str) -> i32 {
    exit_code_from_status(session::read_status(bs, id).await.ok())
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
async fn fallback_finished(bs: &Babysit, id: &str) -> Result<i32> {
    if let Ok(bytes) = tokio::fs::read(bs.output_log_path(id)).await {
        use std::io::Write;
        let mut out = std::io::stdout();
        let _ = out.write_all(&bytes);
        let _ = out.flush();
    }
    Ok(recorded_exit_code(bs, id).await)
}

/// Streaming filter for the `Ctrl-\ Ctrl-\` detach sequence.
///
/// Most programs leave Ctrl-\ in its legacy one-byte form (0x1c), but TUIs can
/// ask the terminal for enhanced key reporting. In particular, pi requests
/// Kitty keyboard protocol flags 1+2+4, making the same key arrive as CSI-u
/// press/release events such as `ESC [ 92 ; 5 u` and `ESC [ 92 ; 5 : 3 u`.
/// xterm's modifyOtherKeys form is accepted as well.
#[derive(Default)]
struct DetachFilter {
    /// The first detach press and any repeat/release events belonging to it.
    withheld: Vec<u8>,
    /// A possibly incomplete CSI sequence split across stdin reads.
    escape: Vec<u8>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DetachEvent {
    Press,
    RepeatOrRelease,
}

impl DetachFilter {
    fn push(&mut self, chunk: &[u8]) -> (Vec<u8>, bool) {
        let mut out = Vec::with_capacity(self.withheld.len() + chunk.len());

        for &byte in chunk {
            if !self.escape.is_empty() {
                self.escape.push(byte);
                let not_csi = self.escape.len() == 2 && byte != b'[';
                let csi_complete = self.escape.len() >= 3 && (0x40..=0x7e).contains(&byte);
                let too_long = self.escape.len() > 128;
                if not_csi || csi_complete || too_long {
                    let sequence = std::mem::take(&mut self.escape);
                    if self.handle_token(&sequence, &mut out) {
                        return (out, true);
                    }
                }
            } else if byte == 0x1b {
                self.escape.push(byte);
            } else if self.handle_token(&[byte], &mut out) {
                return (out, true);
            }
        }

        (out, false)
    }

    fn has_partial_escape(&self) -> bool {
        !self.escape.is_empty()
    }

    fn flush_partial_escape(&mut self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.withheld.len() + self.escape.len());
        let sequence = std::mem::take(&mut self.escape);
        self.handle_other(&sequence, &mut out);
        out
    }

    fn handle_token(&mut self, token: &[u8], out: &mut Vec<u8>) -> bool {
        let event = if token == [DETACH_KEY] {
            Some(DetachEvent::Press)
        } else {
            enhanced_detach_event(token)
        };

        match event {
            Some(DetachEvent::Press) if !self.withheld.is_empty() => true,
            Some(DetachEvent::Press) => {
                self.withheld.extend_from_slice(token);
                false
            }
            Some(DetachEvent::RepeatOrRelease) if !self.withheld.is_empty() => {
                self.withheld.extend_from_slice(token);
                false
            }
            _ => {
                self.handle_other(token, out);
                false
            }
        }
    }

    fn handle_other(&mut self, token: &[u8], out: &mut Vec<u8>) {
        out.append(&mut self.withheld);
        out.extend_from_slice(token);
    }
}

fn enhanced_detach_event(sequence: &[u8]) -> Option<DetachEvent> {
    if !sequence.starts_with(b"\x1b[") {
        return None;
    }

    if sequence.ends_with(b"u") {
        let body = &sequence[2..sequence.len() - 1];
        let mut params = body.split(|&byte| byte == b';');
        let codepoint = parse_decimal(params.next()?.split(|&byte| byte == b':').next()?)?;
        let mut modifier_and_event = params.next()?.split(|&byte| byte == b':');
        let modifier = parse_decimal(modifier_and_event.next()?)?.checked_sub(1)?;
        let event = match modifier_and_event.next() {
            Some(value) => Some(parse_decimal(value)?),
            None => None,
        };

        if codepoint != b'\\' as u16 || modifier & !(64 | 128) != 4 {
            return None;
        }
        return match event.unwrap_or(1) {
            1 => Some(DetachEvent::Press),
            2 | 3 => Some(DetachEvent::RepeatOrRelease),
            _ => None,
        };
    }

    // xterm modifyOtherKeys: CSI 27 ; modifier ; codepoint ~
    if sequence.ends_with(b"~") {
        let body = &sequence[2..sequence.len() - 1];
        let mut params = body.split(|&byte| byte == b';');
        let prefix = parse_decimal(params.next()?)?;
        let modifier = parse_decimal(params.next()?)?.checked_sub(1)?;
        let codepoint = parse_decimal(params.next()?)?;
        if prefix == 27 && codepoint == b'\\' as u16 && modifier & !(64 | 128) == 4 {
            return Some(DetachEvent::Press);
        }
    }

    None
}

fn parse_decimal(bytes: &[u8]) -> Option<u16> {
    if bytes.is_empty() {
        return None;
    }
    let mut value = 0u16;
    for &byte in bytes {
        if !byte.is_ascii_digit() {
            return None;
        }
        value = value.checked_mul(10)?.checked_add((byte - b'0') as u16)?;
    }
    Some(value)
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
    use super::DetachFilter;

    const K: u8 = 0x1c; // Ctrl-\
    const KITTY_PRESS: &[u8] = b"\x1b[92;5u";
    const KITTY_REPEAT: &[u8] = b"\x1b[92;5:2u";
    const KITTY_RELEASE: &[u8] = b"\x1b[92;5:3u";

    #[test]
    fn passes_normal_input() {
        let mut filter = DetachFilter::default();
        assert_eq!(filter.push(b"hello"), (b"hello".to_vec(), false));
    }

    #[test]
    fn detects_legacy_detach_sequence_in_one_chunk() {
        let mut filter = DetachFilter::default();
        assert_eq!(filter.push(&[K, K]), (vec![], true));
    }

    #[test]
    fn detects_legacy_detach_sequence_across_chunks() {
        let mut filter = DetachFilter::default();
        assert_eq!(filter.push(&[K]), (vec![], false));
        assert_eq!(filter.push(&[K]), (vec![], true));
    }

    #[test]
    fn lone_legacy_ctrl_backslash_then_other_is_forwarded() {
        let mut filter = DetachFilter::default();
        assert_eq!(filter.push(&[K, b'a']), (vec![K, b'a'], false));
    }

    #[test]
    fn detects_kitty_detach_while_ignoring_repeat_and_release_events() {
        let mut filter = DetachFilter::default();
        let input = [KITTY_PRESS, KITTY_REPEAT, KITTY_RELEASE, KITTY_PRESS].concat();
        assert_eq!(filter.push(&input), (vec![], true));
    }

    #[test]
    fn detects_fragmented_kitty_detach_sequence() {
        let mut filter = DetachFilter::default();
        assert_eq!(filter.push(b"\x1b[92;"), (vec![], false));
        assert!(filter.has_partial_escape());
        assert_eq!(filter.push(b"5u"), (vec![], false));
        assert!(!filter.has_partial_escape());
        assert_eq!(filter.push(KITTY_RELEASE), (vec![], false));
        assert_eq!(filter.push(KITTY_PRESS), (vec![], true));
    }

    #[test]
    fn forwards_a_lone_kitty_ctrl_backslash_when_another_key_arrives() {
        let mut filter = DetachFilter::default();
        let input = [KITTY_PRESS, KITTY_RELEASE, b"a"].concat();
        assert_eq!(
            filter.push(&input),
            ([KITTY_PRESS, KITTY_RELEASE, b"a"].concat(), false)
        );
    }

    #[test]
    fn accepts_kitty_alternate_keys_locks_and_xterm_encoding() {
        let mut filter = DetachFilter::default();
        assert_eq!(filter.push(b"\x1b[92:124:92;69u"), (vec![], false));
        assert_eq!(filter.push(b"\x1b[27;5;92~"), (vec![], true));
    }

    #[test]
    fn forwards_non_matching_and_timed_out_escape_sequences() {
        let mut filter = DetachFilter::default();
        assert_eq!(filter.push(b"\x1b[97;5u"), (b"\x1b[97;5u".to_vec(), false));
        assert_eq!(filter.push(b"\x1b["), (vec![], false));
        assert_eq!(filter.flush_partial_escape(), b"\x1b[".to_vec());
    }
}
