//! `babysit` subcommand handlers (the "API" surface that agents use).
//!
//! `list` is answered directly from disk. The other subcommands open a
//! short-lived connection to the session's control socket and forward the
//! request as a JSON line.

use crate::cli::ShotFormat;
use crate::control::{Request, Response, last_n_lines};
use crate::paths;
use crate::session::{self, Meta, State, Status};
use anyhow::{Context, Result, anyhow};
use chrono::{DateTime, Utc};
use regex::Regex;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

pub async fn list(json: bool) -> Result<()> {
    let ids = session::list_ids().await?;
    let mut entries = Vec::new();
    for id in &ids {
        let meta = match session::read_meta(id).await {
            Ok(m) => m,
            Err(_) => continue,
        };
        let status = session::read_status(id).await.unwrap_or(Status::starting());
        let note = session::read_note(id).await;
        entries.push((meta, status, note));
    }
    // Most-recently-active first.
    entries.sort_by_key(|e| std::cmp::Reverse(e.1.last_change));

    if json {
        let arr: Vec<serde_json::Value> = entries
            .iter()
            .map(|(m, s, note)| {
                serde_json::json!({
                    "id": m.id,
                    "cmd": m.cmd,
                    "state": s.state,
                    "alive": is_owner_alive(m, s),
                    "exit_code": s.exit_code,
                    "started_at": m.started_at,
                    "last_change": s.last_change,
                    "note": note,
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&arr)?);
    } else if entries.is_empty() {
        println!("(no sessions)");
    } else {
        println!("{:<10} {:<8} {:<10} CMD", "ID", "STATE", "AGE");
        for (m, s, note) in &entries {
            let age = format_age(m.started_at, Utc::now());
            let cmd = m.cmd.join(" ");
            // A flagged session is prefixed with ⚑ and its note appended, so a
            // human scanning `babysit ls` sees what needs attention.
            let suffix = match note {
                Some(n) if !n.is_empty() => format!("  ⚑ {n}"),
                Some(_) => "  ⚑".to_string(),
                None => String::new(),
            };
            println!(
                "{:<10} {:<8} {:<10} {}{}",
                m.id,
                state_label_for(Some(m), s),
                age,
                cmd,
                suffix,
            );
        }
    }
    Ok(())
}

pub async fn status(session: Option<String>, json: bool) -> Result<()> {
    let id = session::resolve(session).await?;
    // Prefer the live state via the control socket; fall back to disk if
    // the babysit process isn't running.
    let resp = request(&id, &Request::Status).await;
    let data = match resp {
        Ok(r) if r.ok => r.data,
        _ => serde_json::to_value(session::read_status(&id).await?)?,
    };
    if json {
        let mut out = serde_json::Map::new();
        out.insert("session".into(), serde_json::Value::String(id));
        out.insert("status".into(), data);
        println!("{}", serde_json::to_string_pretty(&out)?);
    } else {
        let s: Status = serde_json::from_value(data)?;
        let meta = session::read_meta(&id).await.ok();
        println!("session: {id}");
        if let Some(m) = meta.as_ref() {
            println!("cmd:     {}", m.cmd.join(" "));
        }
        println!("state:   {}", state_label_for(meta.as_ref(), &s));
        if let Some(c) = s.exit_code {
            println!("exit:    {c}");
        }
        if let Some(note) = session::read_note(&id).await {
            println!("flag:    ⚑ {note}");
        }
    }
    Ok(())
}

pub async fn log(
    session: Option<String>,
    tail: Option<usize>,
    grep: Option<String>,
    raw: bool,
    since: Option<u64>,
    follow: bool,
    json: bool,
) -> Result<()> {
    let id = session::resolve(session).await?;
    let path = paths::output_log_path(&id)?;
    let re = grep
        .as_deref()
        .map(Regex::new)
        .transpose()
        .context("invalid --grep regex")?;

    if follow {
        return follow_log(&id, &path, raw, since.unwrap_or(0), re.as_ref()).await;
    }

    if let Some(off) = since {
        // Incremental read straight from the (append-only) log file.
        let (text, offset) = read_slice(&path, off, raw).await?;
        emit_log(&id, grep_filter(text, re.as_ref()), offset, json).await
    } else {
        // Whole log (or --tail). Prefer the live socket; fall back to disk.
        // With --grep we fetch the full log and filter+tail client-side, so
        // the server-side tail is skipped.
        let server_tail = if re.is_some() { None } else { tail };
        let resp = request(
            &id,
            &Request::Log {
                tail: server_tail,
                raw,
            },
        )
        .await;
        let text = match resp {
            Ok(r) if r.ok => r
                .data
                .get("text")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            _ => {
                let bytes = tokio::fs::read(&path).await.unwrap_or_default();
                let processed = if raw {
                    bytes
                } else {
                    strip_ansi_escapes::strip(&bytes)
                };
                let text = String::from_utf8_lossy(&processed).into_owned();
                match server_tail {
                    Some(n) => last_n_lines(&text, n),
                    None => text,
                }
            }
        };
        // Apply --grep, then --tail to the matching lines.
        let text = match re.as_ref() {
            Some(re) => {
                let filtered = grep_filter(text, Some(re));
                match tail {
                    Some(n) => last_n_lines(&filtered, n),
                    None => filtered,
                }
            }
            None => text,
        };
        let offset = tokio::fs::metadata(&path)
            .await
            .map(|m| m.len())
            .unwrap_or(0);
        emit_log(&id, text, offset, json).await
    }
}

/// Keep only lines matching `re` (no-op when `re` is None). Each kept line is
/// terminated with a newline.
fn grep_filter(text: String, re: Option<&Regex>) -> String {
    let Some(re) = re else { return text };
    let mut out = String::new();
    for line in text.lines() {
        if re.is_match(line) {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

/// Capture the current visible screen of a session. Prefers the live worker
/// (via the control socket); if the worker is gone, falls back to replaying
/// the on-disk output log through a fresh virtual terminal.
pub async fn screenshot(session: Option<String>, format: ShotFormat, trim: bool) -> Result<()> {
    let id = session::resolve(session).await?;
    let req = Request::Screenshot { format, trim };
    let data = match request(&id, &req).await {
        Ok(r) if r.ok => r.data,
        _ => {
            // Worker not running: render from the log on disk.
            let path = paths::output_log_path(&id)?;
            let bytes = tokio::fs::read(&path).await.unwrap_or_default();
            crate::render::render_log(&bytes, format, trim)
        }
    };

    match format {
        // For text formats the rendered screen is the payload; print it raw.
        // JSON returns the full metadata object (size, cursor, cells).
        ShotFormat::Plain | ShotFormat::Ansi => {
            if let Some(text) = data.get("text").and_then(|v| v.as_str()) {
                println!("{text}");
            }
        }
        ShotFormat::Json => println!("{}", serde_json::to_string_pretty(&data)?),
    }
    Ok(())
}

/// Print log output, either as raw text or as JSON `{text, offset, done}`
/// (so a poller can resume from `offset` and stop when `done`).
async fn emit_log(id: &str, text: String, offset: u64, json: bool) -> Result<()> {
    if json {
        let done = is_finished(id).await;
        let obj = serde_json::json!({ "text": text, "offset": offset, "done": done });
        println!("{}", serde_json::to_string(&obj)?);
    } else {
        print!("{text}");
    }
    Ok(())
}

/// Read the raw log from byte `off` to EOF. Returns the (optionally
/// ANSI-stripped) text plus the new raw-byte offset to resume from.
async fn read_slice(path: &std::path::Path, off: u64, raw: bool) -> Result<(String, u64)> {
    use tokio::io::{AsyncReadExt, AsyncSeekExt};
    let mut f = match tokio::fs::File::open(path).await {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok((String::new(), off)),
        Err(e) => return Err(e.into()),
    };
    let len = f.metadata().await?.len();
    if off >= len {
        return Ok((String::new(), len));
    }
    f.seek(std::io::SeekFrom::Start(off)).await?;
    let mut bytes = Vec::new();
    f.read_to_end(&mut bytes).await?;
    let processed = if raw {
        bytes
    } else {
        strip_ansi_escapes::strip(&bytes)
    };
    Ok((String::from_utf8_lossy(&processed).into_owned(), len))
}

async fn is_finished(id: &str) -> bool {
    session::read_status(id)
        .await
        .map(|s| s.state.is_terminal())
        .unwrap_or(false)
}

/// Stream new log output to stdout until the session finishes (tail -f style).
async fn follow_log(
    id: &str,
    path: &std::path::Path,
    raw: bool,
    start: u64,
    re: Option<&Regex>,
) -> Result<()> {
    use std::io::Write as _;
    let mut off = start;
    let mut idle_after_done = 0u32;
    loop {
        let (text, new_off) = read_slice(path, off, raw).await?;
        let text = grep_filter(text, re);
        if !text.is_empty() {
            let mut out = std::io::stdout();
            let _ = out.write_all(text.as_bytes());
            let _ = out.flush();
        }
        let advanced = new_off > off;
        off = new_off;
        // The worker flips status to terminal slightly before its final
        // post-exit flush completes, so wait for a couple of idle polls after
        // `done` before stopping, to avoid cutting off the tail.
        if is_finished(id).await {
            if advanced {
                idle_after_done = 0;
            } else {
                idle_after_done += 1;
                if idle_after_done >= 2 {
                    break;
                }
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    Ok(())
}

pub async fn restart(session: Option<String>) -> Result<()> {
    let id = session::resolve(session).await?;
    let r = request(&id, &Request::Restart).await?;
    if !r.ok {
        return Err(anyhow!(r.error.unwrap_or_else(|| "restart failed".into())));
    }
    println!("restart queued for session {id}");
    Ok(())
}

pub async fn kill(session: Option<String>) -> Result<()> {
    let id = session::resolve(session).await?;
    let r = request(&id, &Request::Kill).await?;
    if !r.ok {
        return Err(anyhow!(r.error.unwrap_or_else(|| "kill failed".into())));
    }
    println!("killed session {id}");
    Ok(())
}

pub async fn send(session: Option<String>, text: String, newline: bool) -> Result<()> {
    let id = session::resolve(session).await?;
    let r = request(&id, &Request::Send { text, newline }).await?;
    if !r.ok {
        return Err(anyhow!(r.error.unwrap_or_else(|| "send failed".into())));
    }
    Ok(())
}

/// Send one or more named keys (e.g. `Down Down Enter`, `C-c`) to the wrapped
/// command by encoding them to their terminal byte sequences and writing them
/// raw (no trailing newline).
pub async fn key(session: Option<String>, keys: Vec<String>) -> Result<()> {
    let id = session::resolve(session).await?;
    let mut bytes = Vec::new();
    for name in &keys {
        let seq = key_to_bytes(name)
            .ok_or_else(|| anyhow!("unknown key `{name}` (try Enter, Tab, Esc, Up, C-c, F1, …)"))?;
        bytes.extend_from_slice(&seq);
    }
    // Key escape sequences are ASCII, so a lossless String round-trips them
    // over the JSON `send` op without a newline.
    let text = String::from_utf8(bytes).expect("key sequences are ASCII");
    let r = request(
        &id,
        &Request::Send {
            text,
            newline: false,
        },
    )
    .await?;
    if !r.ok {
        return Err(anyhow!(r.error.unwrap_or_else(|| "key failed".into())));
    }
    Ok(())
}

/// Resize a session's terminal from a `COLSxROWS` string.
pub async fn resize(session: Option<String>, size: String) -> Result<()> {
    let id = session::resolve(session).await?;
    let (cols, rows) = crate::run::parse_size(&size)?;
    let r = request(&id, &Request::Resize { cols, rows }).await?;
    if !r.ok {
        return Err(anyhow!(r.error.unwrap_or_else(|| "resize failed".into())));
    }
    println!("resized session {id} to {cols}x{rows}");
    Ok(())
}

/// Flag a session for human attention, with an optional note.
pub async fn flag(session: Option<String>, message: Option<String>) -> Result<()> {
    let id = session::resolve(session).await?;
    let msg = message.unwrap_or_else(|| "needs attention".to_string());
    session::write_note(&id, &msg).await?;
    println!("flagged session {id}: {msg}");
    Ok(())
}

/// Clear a session's attention flag.
pub async fn unflag(session: Option<String>) -> Result<()> {
    let id = session::resolve(session).await?;
    session::clear_note(&id).await?;
    println!("unflagged session {id}");
    Ok(())
}

/// Map a key name to the bytes a terminal sends for it. Names are
/// case-insensitive. `C-x` / `Ctrl-x` produce the corresponding control byte.
fn key_to_bytes(name: &str) -> Option<Vec<u8>> {
    let lower = name.to_ascii_lowercase();
    // Ctrl combinations: C-c, Ctrl-c, ^c → control byte for the letter.
    let ctrl = lower
        .strip_prefix("c-")
        .or_else(|| lower.strip_prefix("ctrl-"))
        .or_else(|| name.strip_prefix('^'));
    if let Some(rest) = ctrl
        && rest.len() == 1
    {
        let c = rest.as_bytes()[0];
        if c.is_ascii_alphabetic() {
            return Some(vec![c.to_ascii_uppercase() - b'@']); // Ctrl-A = 0x01
        }
    }
    let seq: &[u8] = match lower.as_str() {
        "enter" | "return" | "cr" => b"\r",
        "tab" => b"\t",
        "esc" | "escape" => b"\x1b",
        "space" => b" ",
        "backspace" | "bs" => b"\x7f",
        "up" => b"\x1b[A",
        "down" => b"\x1b[B",
        "right" => b"\x1b[C",
        "left" => b"\x1b[D",
        "home" => b"\x1b[H",
        "end" => b"\x1b[F",
        "insert" | "ins" => b"\x1b[2~",
        "delete" | "del" => b"\x1b[3~",
        "pageup" | "pgup" => b"\x1b[5~",
        "pagedown" | "pgdn" => b"\x1b[6~",
        "f1" => b"\x1bOP",
        "f2" => b"\x1bOQ",
        "f3" => b"\x1bOR",
        "f4" => b"\x1bOS",
        "f5" => b"\x1b[15~",
        "f6" => b"\x1b[17~",
        "f7" => b"\x1b[18~",
        "f8" => b"\x1b[19~",
        "f9" => b"\x1b[20~",
        "f10" => b"\x1b[21~",
        "f11" => b"\x1b[23~",
        "f12" => b"\x1b[24~",
        _ => return None,
    };
    Some(seq.to_vec())
}

/// Block until `pattern` (a regex) appears in the output, or `timeout`
/// elapses. Scans the raw log incrementally from `since` (or the current end,
/// unless `from_start`). Returns 0 on match, 124 on timeout, 1 if the session
/// ends before the pattern appears.
#[allow(clippy::too_many_arguments)]
pub async fn expect(
    session: Option<String>,
    pattern: String,
    timeout: Option<String>,
    since: Option<u64>,
    from_now: bool,
    raw: bool,
    json: bool,
) -> Result<i32> {
    let id = session::resolve(session).await?;
    let path = paths::output_log_path(&id)?;
    let re = Regex::new(&pattern).context("invalid expect regex")?;
    let timeout = timeout
        .as_deref()
        .map(crate::run::parse_duration)
        .transpose()?;
    let deadline = timeout.map(|d| std::time::Instant::now() + d);

    // Where to start scanning. Default: the whole log, so an already-printed
    // marker still matches (the send→expect response usually lands before this
    // call starts). `--from-now` opts into stream semantics; `--since` is the
    // race-free way to wait for output after a specific point.
    let mut off = match since {
        Some(o) => o,
        None if from_now => tokio::fs::metadata(&path)
            .await
            .map(|m| m.len())
            .unwrap_or(0),
        None => 0,
    };
    let mut buf = String::new();

    loop {
        let (text, new_off) = read_slice(&path, off, raw).await?;
        off = new_off;
        if !text.is_empty() {
            buf.push_str(&text);
            if let Some(m) = re.find(&buf) {
                if json {
                    let obj = serde_json::json!({
                        "matched": m.as_str(),
                        "offset": off,
                    });
                    println!("{}", serde_json::to_string(&obj)?);
                } else {
                    println!("{}", m.as_str());
                }
                return Ok(0);
            }
        }
        if is_finished(&id).await {
            // Drain any final bytes once more before giving up.
            let (tail, _) = read_slice(&path, off, raw).await?;
            buf.push_str(&tail);
            if re.is_match(&buf) {
                return Ok(0);
            }
            eprintln!("babysit: session {id} ended before matching /{pattern}/");
            return Ok(1);
        }
        if let Some(dl) = deadline
            && std::time::Instant::now() >= dl
        {
            eprintln!("babysit: timed out waiting for /{pattern}/ in session {id}");
            return Ok(124);
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

/// Block until the session's output has been quiet for `settle`. Returns
/// immediately (idle) if the session already finished; exits 124 on timeout.
pub async fn wait_idle(
    session: Option<String>,
    settle: String,
    timeout: Option<String>,
) -> Result<i32> {
    let id = session::resolve(session).await?;
    let path = paths::output_log_path(&id)?;
    let settle = crate::run::parse_duration(&settle)?;
    let timeout = timeout
        .as_deref()
        .map(crate::run::parse_duration)
        .transpose()?;
    let deadline = timeout.map(|d| std::time::Instant::now() + d);

    let mut last_size = tokio::fs::metadata(&path)
        .await
        .map(|m| m.len())
        .unwrap_or(0);
    let mut quiet_since = std::time::Instant::now();

    loop {
        let size = tokio::fs::metadata(&path)
            .await
            .map(|m| m.len())
            .unwrap_or(0);
        if size != last_size {
            last_size = size;
            quiet_since = std::time::Instant::now();
        } else if quiet_since.elapsed() >= settle {
            return Ok(0);
        }
        // A finished session is, by definition, idle.
        if is_finished(&id).await {
            return Ok(0);
        }
        if let Some(dl) = deadline
            && std::time::Instant::now() >= dl
        {
            eprintln!("babysit: timed out waiting for session {id} to settle");
            return Ok(124);
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

/// Block until the session's wrapped command exits, then return its exit
/// code. Polls the on-disk status (so it works regardless of who owns the
/// session) and gives up with exit 124 after `timeout`, mirroring coreutils
/// `timeout`. If the owning babysit process dies without recording a
/// terminal state, returns 137.
pub async fn wait(session: Option<String>, timeout: Option<String>) -> Result<i32> {
    let id = session::resolve(session).await?;
    let timeout = timeout
        .as_deref()
        .map(crate::run::parse_duration)
        .transpose()?;
    let deadline = timeout.map(|d| std::time::Instant::now() + d);

    loop {
        if let Ok(status) = session::read_status(&id).await {
            match status.state {
                State::Exited => return Ok(status.exit_code.unwrap_or(0)),
                State::Killed => return Ok(status.exit_code.unwrap_or(130)),
                State::Starting | State::Running => {
                    // Owner gone without a terminal state ⇒ it crashed.
                    if let Ok(meta) = session::read_meta(&id).await
                        && !session::is_pid_alive(meta.babysit_pid)
                    {
                        eprintln!("babysit: session {id} owner died before exiting");
                        return Ok(137);
                    }
                }
            }
        }
        if let Some(dl) = deadline
            && std::time::Instant::now() >= dl
        {
            eprintln!("babysit: timed out waiting for session {id}");
            return Ok(124);
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
}

/// Delete session directories for sessions that are in a terminal state
/// (exited / killed) or whose owning babysit process has died.
///
/// Live sessions (running, with a live owner) are never touched.
pub async fn prune(dry_run: bool) -> Result<()> {
    let ids = session::list_ids().await?;
    let mut targets: Vec<(String, Meta)> = Vec::new();
    for id in &ids {
        let meta = match session::read_meta(id).await {
            Ok(m) => m,
            // Unparseable meta — leave it alone rather than silently nuke it.
            Err(_) => continue,
        };
        let status = session::read_status(id).await.ok();
        let alive = session::is_pid_alive(meta.babysit_pid);
        let should_delete = match status.as_ref().map(|s| s.state) {
            Some(State::Exited | State::Killed) => true,
            // Starting/Running with a dead owner ⇒ "dead" in `babysit list`.
            Some(State::Starting | State::Running) if !alive => true,
            // No status file at all and no live owner ⇒ orphan.
            None if !alive => true,
            _ => false,
        };
        if should_delete {
            targets.push((id.clone(), meta));
        }
    }

    if targets.is_empty() {
        println!("(nothing to prune)");
        return Ok(());
    }

    for (id, meta) in &targets {
        let cmd = meta.cmd.join(" ");
        if dry_run {
            println!("would delete {id}  {cmd}");
        } else {
            let dir = paths::session_dir(id)?;
            if let Err(e) = tokio::fs::remove_dir_all(&dir).await {
                eprintln!("babysit: failed to remove {}: {e}", dir.display());
                continue;
            }
            println!("deleted {id}  {cmd}");
        }
    }
    Ok(())
}

/// Open a short-lived connection to the session's control socket, send a
/// single JSON request, and parse the JSON response.
async fn request(id: &str, req: &Request) -> Result<Response> {
    let path = paths::control_socket_path(id)?;
    let mut stream = UnixStream::connect(&path)
        .await
        .with_context(|| format!("connecting to control socket {}", path.display()))?;
    let mut bytes = serde_json::to_vec(req)?;
    bytes.push(b'\n');
    stream.write_all(&bytes).await?;
    stream.flush().await?;

    let mut br = BufReader::new(stream);
    let mut line = String::new();
    br.read_line(&mut line).await?;
    let resp: Response = serde_json::from_str(line.trim())?;
    Ok(resp)
}

fn state_label_for(meta: Option<&Meta>, s: &Status) -> String {
    // A persisted Starting/Running state only reflects reality while the
    // owning babysit process is still alive. If the process is gone (crash,
    // kill -9, reboot, or an early spawn failure that bailed before writing
    // a terminal state) the on-disk value is stale — surface that instead.
    if matches!(s.state, State::Starting | State::Running) && !is_owner_alive_meta(meta) {
        return "dead".into();
    }
    match s.state {
        State::Starting => "starting".into(),
        State::Running => "running".into(),
        State::Exited => match s.exit_code {
            Some(c) => format!("exit:{c}"),
            None => "exited".into(),
        },
        State::Killed => "killed".into(),
    }
}

fn is_owner_alive_meta(meta: Option<&Meta>) -> bool {
    meta.map(|m| session::is_pid_alive(m.babysit_pid))
        .unwrap_or(false)
}

fn is_owner_alive(meta: &Meta, s: &Status) -> bool {
    if !matches!(s.state, State::Starting | State::Running) {
        return false;
    }
    session::is_pid_alive(meta.babysit_pid)
}

fn format_age(then: DateTime<Utc>, now: DateTime<Utc>) -> String {
    let secs = (now - then).num_seconds().max(0);
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86400 {
        format!("{}h", secs / 3600)
    } else {
        format!("{}d", secs / 86400)
    }
}

#[cfg(test)]
mod tests {
    use super::{grep_filter, key_to_bytes};
    use regex::Regex;

    #[test]
    fn key_named_sequences() {
        assert_eq!(key_to_bytes("Enter").unwrap(), b"\r");
        assert_eq!(key_to_bytes("up").unwrap(), b"\x1b[A");
        assert_eq!(key_to_bytes("Esc").unwrap(), b"\x1b");
        assert_eq!(key_to_bytes("F5").unwrap(), b"\x1b[15~");
        assert!(key_to_bytes("nope").is_none());
    }

    #[test]
    fn key_ctrl_combinations() {
        assert_eq!(key_to_bytes("C-c").unwrap(), vec![0x03]);
        assert_eq!(key_to_bytes("Ctrl-d").unwrap(), vec![0x04]);
        assert_eq!(key_to_bytes("^a").unwrap(), vec![0x01]);
        // Case-insensitive: C-C is the same control byte as C-c.
        assert_eq!(key_to_bytes("C-C").unwrap(), vec![0x03]);
    }

    #[test]
    fn grep_filter_keeps_matching_lines() {
        let re = Regex::new("err").unwrap();
        let out = grep_filter("ok\nerror here\nfine\nerr2\n".into(), Some(&re));
        assert_eq!(out, "error here\nerr2\n");
    }

    #[test]
    fn grep_filter_none_is_passthrough() {
        assert_eq!(grep_filter("a\nb\n".into(), None), "a\nb\n");
    }
}
