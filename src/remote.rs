use crate::attach::{
    self, C_INPUT, C_RESIZE, DetachFilter, RawGuard, S_DETACHED, S_ERROR, S_EXIT, S_OUTPUT,
    S_OUTPUT_OFFSET, S_READY,
};
use crate::control::Request;
use crate::pane::ExitInfo;
use crate::paths::Babysit;
use crate::session::{self, State};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::io::{IsTerminal, Write};
use std::process::Stdio;
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::mpsc;

pub const REMOTE_PROTOCOL: u32 = 1;

#[derive(Debug, Serialize, Deserialize)]
pub struct RemoteInfo {
    pub protocol: u32,
    pub version: String,
}

pub fn print_info() -> Result<()> {
    println!(
        "{}",
        serde_json::to_string(&RemoteInfo {
            protocol: REMOTE_PROTOCOL,
            version: env!("CARGO_PKG_VERSION").into(),
        })?
    );
    Ok(())
}

pub fn shell_quote(value: &str) -> String {
    if value.is_empty() {
        return "''".into();
    }
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn remote_command(args: &[String]) -> String {
    remote_command_with_term(args, std::env::var("TERM").ok().as_deref())
}

fn remote_command_with_term(args: &[String], term: Option<&str>) -> String {
    let command = std::iter::once("babysit")
        .chain(args.iter().map(String::as_str))
        .map(shell_quote)
        .collect::<Vec<_>>()
        .join(" ");
    match term.filter(|value| !value.is_empty()) {
        Some(value) => format!("TERM={} {command}", shell_quote(value)),
        None => command,
    }
}

fn ssh_command(host: &str, args: &[String]) -> Result<Command> {
    validate_host(host)?;
    let ssh = std::env::var_os("BABYSIT_SSH").unwrap_or_else(|| "ssh".into());
    let mut command = Command::new(ssh);
    command
        .arg("-T")
        .arg("-o")
        .arg("ServerAliveInterval=3")
        .arg("-o")
        .arg("ServerAliveCountMax=2")
        .arg("-o")
        .arg("ConnectTimeout=5")
        .arg("--")
        .arg(host)
        .arg(remote_command(args));
    Ok(command)
}

fn validate_host(host: &str) -> Result<()> {
    if host.is_empty() || host.starts_with('-') {
        bail!("SSH host must be non-empty and must not begin with `-`");
    }
    Ok(())
}

pub async fn verify(host: &str) -> Result<RemoteInfo> {
    let mut command = ssh_command(host, &["__remote-info".into()])?;
    command.stdin(Stdio::null()).kill_on_drop(true);
    let output = tokio::time::timeout(Duration::from_secs(20), command.output())
        .await
        .context("timed out checking remote babysit compatibility")?
        .context("starting ssh")?;
    if !output.status.success() {
        bail!(
            "remote compatibility check failed (ssh exit {}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let info: RemoteInfo = serde_json::from_slice(&output.stdout)
        .context("remote babysit returned an invalid compatibility response")?;
    if info.protocol != REMOTE_PROTOCOL {
        bail!(
            "remote babysit protocol {} is incompatible with local protocol {}; upgrade babysit on the remote host",
            info.protocol,
            REMOTE_PROTOCOL
        );
    }
    Ok(info)
}

pub async fn proxy(host: &str, args: &[String]) -> Result<i32> {
    let status = ssh_command(host, args)?
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .await
        .context("starting ssh")?;
    Ok(status.code().unwrap_or(255))
}

pub async fn capture(host: &str, args: &[String]) -> Result<std::process::Output> {
    let mut command = ssh_command(host, args)?;
    command.stdin(Stdio::null()).kill_on_drop(true);
    tokio::time::timeout(Duration::from_secs(20), command.output())
        .await
        .context("timed out waiting for remote babysit")?
        .context("starting ssh")
}

/// After an ambiguous create transport failure, query the caller-chosen ID for
/// a bounded period. This never retries `run`, so it cannot duplicate work.
pub async fn confirm_session(host: &str, id: &str) -> Result<bool> {
    for attempt in 0..10 {
        let output = capture(
            host,
            &[
                "status".into(),
                "--session".into(),
                id.into(),
                "--json".into(),
            ],
        )
        .await?;
        if output.status.success() {
            return Ok(true);
        }
        if attempt < 9 {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
    Ok(false)
}

/// Remove the global host selector while preserving every argument after the
/// wrapped command's `--` verbatim.
pub fn strip_host_args(raw: &[String]) -> Result<Vec<String>> {
    let mut result = Vec::with_capacity(raw.len());
    let mut index = 0;
    let mut after_separator = false;
    while index < raw.len() {
        let arg = &raw[index];
        if after_separator {
            result.push(arg.clone());
        } else if arg == "--" {
            after_separator = true;
            result.push(arg.clone());
        } else if arg == "--host" {
            index += 1;
            if index >= raw.len() {
                bail!("--host requires a value");
            }
        } else if arg.starts_with("--host=") {
            // omit
        } else {
            result.push(arg.clone());
        }
        index += 1;
    }
    Ok(result)
}

pub async fn bridge(bs: &Babysit, selected: Option<String>) -> Result<()> {
    let id = session::resolve(bs, selected).await?;
    let mut hello = String::new();
    let mut stdin = BufReader::new(tokio::io::stdin());
    stdin.read_line(&mut hello).await?;
    let request: Request = serde_json::from_str(hello.trim()).context("invalid attach request")?;
    let (since, protocol) = match request {
        Request::Attach {
            since, protocol, ..
        } => (since, protocol),
        _ => bail!("remote bridge accepts only attach requests"),
    };

    let connection = match attach::connect_retry(bs, &id).await {
        Ok(connection) => connection,
        Err(error) if protocol >= 1 => {
            let mut stdout = tokio::io::stdout();
            attach::write_frame(&mut stdout, S_ERROR, error.to_string().as_bytes()).await?;
            return Ok(());
        }
        Err(error) => return Err(error),
    };
    let tty = match session::read_tty(bs, &id).await {
        Ok(tty) => tty,
        Err(error) if protocol >= 1 => {
            let mut stdout = tokio::io::stdout();
            attach::write_frame(&mut stdout, S_ERROR, error.to_string().as_bytes()).await?;
            return Ok(());
        }
        Err(error) => return Err(error),
    };
    let ready_payload = [u8::from(tty)];

    match connection {
        Some(mut socket) => {
            socket.write_all(hello.as_bytes()).await?;
            socket.flush().await?;
            let (mut socket_rd, mut socket_wr) = socket.into_split();
            let mut stdout = tokio::io::stdout();
            // The bridge itself defines transport readiness. New workers also
            // send S_READY (harmlessly ignored after the handshake), while this
            // lets a new bridge attach once to a pre-protocol worker.
            if protocol >= 1 {
                attach::write_frame(&mut stdout, S_READY, &ready_payload).await?;
            }
            let input =
                tokio::spawn(async move { tokio::io::copy(&mut stdin, &mut socket_wr).await });
            let output = tokio::io::copy(&mut socket_rd, &mut stdout).await;
            input.abort();
            output?;
            stdout.flush().await?;
            Ok(())
        }
        None => {
            let status = session::read_status(bs, &id).await?;
            let bytes = tokio::fs::read(bs.output_log_path(&id))
                .await
                .unwrap_or_default();
            let start = match since {
                Some(offset) => {
                    let offset = usize::try_from(offset).context("resume offset is too large")?;
                    if offset > bytes.len() {
                        let message = format!(
                            "resume offset {offset} is beyond output end {}",
                            bytes.len()
                        );
                        if protocol >= 1 {
                            let mut stdout = tokio::io::stdout();
                            attach::write_frame(&mut stdout, S_READY, &ready_payload).await?;
                            attach::write_frame(&mut stdout, S_ERROR, message.as_bytes()).await?;
                            return Ok(());
                        }
                        bail!(message);
                    }
                    offset
                }
                None => bytes.len().saturating_sub(1 << 20),
            };
            let mut stdout = tokio::io::stdout();
            if protocol >= 1 {
                attach::write_frame(&mut stdout, S_READY, &ready_payload).await?;
                if start < bytes.len() {
                    let mut payload = Vec::with_capacity(8 + bytes.len() - start);
                    payload.extend_from_slice(&(start as u64).to_be_bytes());
                    payload.extend_from_slice(&bytes[start..]);
                    attach::write_frame(&mut stdout, S_OUTPUT_OFFSET, &payload).await?;
                }
            } else if start < bytes.len() {
                attach::write_frame(&mut stdout, S_OUTPUT, &bytes[start..]).await?;
            }
            let info = ExitInfo {
                code: status.exit_code,
                signaled: status.state == State::Killed,
            };
            attach::write_frame(&mut stdout, S_EXIT, &attach::exit_payload(Some(info))).await?;
            Ok(())
        }
    }
}

fn spawn_bridge(host: &str, id: &str) -> Result<Child> {
    ssh_command(
        host,
        &["__remote-bridge".into(), "--session".into(), id.into()],
    )?
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .stderr(Stdio::inherit())
    .kill_on_drop(true)
    .spawn()
    .context("starting ssh attach bridge")
}

#[derive(Clone, Copy)]
struct ReconnectTarget<'a> {
    host: &'a str,
    id: &'a str,
    visual: bool,
}

struct ReconnectStatus {
    attempt: u32,
    disconnected_at: Instant,
    initial_reason: String,
    latest_reason: String,
    last_offset: Option<u64>,
}

impl ReconnectStatus {
    fn new(reason: String, last_offset: Option<u64>) -> Self {
        Self {
            attempt: 0,
            disconnected_at: Instant::now(),
            initial_reason: reason.clone(),
            latest_reason: reason,
            last_offset,
        }
    }

    fn update(&mut self, reason: String, last_offset: Option<u64>) {
        self.latest_reason = reason;
        if last_offset.is_some() {
            self.last_offset = last_offset;
        }
    }

    fn render(&self, host: &str, id: &str, phase: &str) -> String {
        let elapsed = self.disconnected_at.elapsed().as_secs_f32();
        let offset = self
            .last_offset
            .map_or_else(|| "unknown".into(), |value| value.to_string());
        let latest = if self.latest_reason == self.initial_reason {
            String::new()
        } else {
            format!("\r\nLatest failure: {}", self.latest_reason)
        };
        format!(
            "\x1b[2J\x1b[H\
             babysit: remote connection lost\r\n\r\n\
             Host:            {host}\r\n\
             Session:         {id}\r\n\
             Offline:         {elapsed:.1}s\r\n\
             Last raw offset: {offset}\r\n\
             Reason:          {}{latest}\r\n\r\n\
             {phase}\r\n\
             Input while offline is discarded. Detach: Ctrl-\\ Ctrl-\\\r\n",
            self.initial_reason
        )
    }

    fn show_waiting(&self, host: &str, id: &str, delay: Duration, visual: bool) {
        let phase = format!(
            "Reconnecting:    attempt {} in {:.2}s",
            self.attempt.saturating_add(1),
            delay.as_secs_f32()
        );
        if visual {
            write_reconnect_display(&self.render(host, id, &phase));
        } else {
            eprintln!(
                "babysit: connection to {host} lost ({}); reconnecting attempt {} in {:.2}s",
                self.latest_reason,
                self.attempt.saturating_add(1),
                delay.as_secs_f32()
            );
        }
    }

    fn show_connecting(&mut self, host: &str, id: &str, visual: bool) {
        self.attempt = self.attempt.saturating_add(1);
        if visual {
            let phase = format!("Reconnecting:    attempt {} in progress...", self.attempt);
            write_reconnect_display(&self.render(host, id, &phase));
        }
    }
}

fn write_reconnect_display(text: &str) {
    let mut out = std::io::stdout();
    let _ = out.write_all(text.as_bytes());
    let _ = out.flush();
}

fn clear_reconnect_display() {
    write_reconnect_display("\x1b[2J\x1b[H");
}

fn ready_uses_tty(payload: &[u8]) -> bool {
    payload.first().copied().unwrap_or(1) != 0
}

async fn reconnect_after_loss(
    status: &mut Option<ReconnectStatus>,
    target: ReconnectTarget<'_>,
    reason: String,
    last_offset: Option<u64>,
    delay: Duration,
    stdin_rx: &mut mpsc::UnboundedReceiver<Vec<u8>>,
    filter: &mut DetachFilter,
) -> Result<bool> {
    match status {
        Some(status) => status.update(reason, last_offset),
        None => *status = Some(ReconnectStatus::new(reason, last_offset)),
    }
    let status = status.as_mut().expect("reconnect status was initialized");
    status.show_waiting(target.host, target.id, delay, target.visual);
    if wait_reconnect(delay, stdin_rx, filter).await? {
        if target.visual {
            clear_reconnect_display();
        }
        return Ok(true);
    }
    status.show_connecting(target.host, target.id, target.visual);
    Ok(false)
}

pub async fn attach(host: &str, id: String, reconnect: bool) -> Result<i32> {
    struct TerminalCleanup(bool);
    impl Drop for TerminalCleanup {
        fn drop(&mut self) {
            if self.0 {
                attach::restore_terminal_modes();
            }
        }
    }

    let (stdin_tx, mut stdin_rx) = mpsc::unbounded_channel::<Vec<u8>>();
    std::thread::spawn(move || {
        use std::io::Read;
        let mut input = std::io::stdin().lock();
        let mut buf = [0u8; 4096];
        loop {
            match input.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) if stdin_tx.send(buf[..n].to_vec()).is_err() => break,
                Ok(_) => {}
            }
        }
    });

    let _raw = if std::io::stdin().is_terminal() {
        RawGuard::enter().ok()
    } else {
        None
    };
    let mut winch = signal(SignalKind::window_change())?;
    let mut filter = DetachFilter::default();
    let mut cursor: Option<u64> = None;
    let mut resumable = true;
    let mut established_once = false;
    let mut delay = Duration::from_millis(250);
    let local_stdout_is_terminal = std::io::stdout().is_terminal();
    let mut visual_reconnect = false;
    let mut reconnect_status = None;
    let mut cleanup = TerminalCleanup(false);

    'reconnect: loop {
        let mut child = spawn_bridge(host, &id)?;
        let mut child_in = child.stdin.take().context("ssh stdin unavailable")?;
        let child_out = child.stdout.take().context("ssh stdout unavailable")?;
        let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
        let hello = serde_json::to_vec(&Request::Attach {
            cols,
            rows,
            since: cursor,
            protocol: REMOTE_PROTOCOL as u8,
        })?;
        child_in.write_all(&hello).await?;
        child_in.write_all(b"\n").await?;
        child_in.flush().await?;
        let mut reader = BufReader::new(child_out);

        let ready = tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                tokio::select! {
                    frame = attach::read_frame(&mut reader) => match frame? {
                        Some((S_READY, payload)) => {
                            return Ok::<(u8, bool), std::io::Error>((1, ready_uses_tty(&payload)));
                        }
                        Some((S_ERROR, payload)) => {
                            return Err(std::io::Error::other(format!(
                                "remote attach failed: {}",
                                String::from_utf8_lossy(&payload)
                            )));
                        }
                        Some(_) => continue,
                        None => return Ok((0, true)),
                    },
                    chunk = stdin_rx.recv() => {
                        if let Some(bytes) = chunk {
                            let (_, detach) = filter.push(&bytes);
                            if detach { return Ok((2, true)); }
                        }
                    }
                }
            }
        })
        .await;

        match ready {
            Ok(Ok((1, remote_tty))) => {
                established_once = true;
                cleanup.0 = true;
                delay = Duration::from_millis(250);
                visual_reconnect = local_stdout_is_terminal && remote_tty;
                if reconnect_status.take().is_some() && visual_reconnect {
                    clear_reconnect_display();
                }
                // The ready frame can win select while stdin chunks are already
                // queued. Drain them too: nothing typed before readiness may
                // leak into the resumed program, but detach must still work.
                if discard_queued_input(&mut stdin_rx, &mut filter) {
                    let _ = child.kill().await;
                    attach::restore_terminal_modes();
                    cleanup.0 = false;
                    return Ok(0);
                }
                filter.discard_pending();
            }
            Ok(Ok((2, _))) => {
                let _ = child.kill().await;
                if reconnect_status.take().is_some() && visual_reconnect {
                    clear_reconnect_display();
                }
                attach::restore_terminal_modes();
                cleanup.0 = false;
                return Ok(0);
            }
            Ok(Ok(_)) => {
                let _ = child.kill().await;
                if !established_once {
                    bail!("SSH attach bridge closed before the remote session was ready");
                }
                if !reconnect {
                    bail!("remote attach transport disconnected");
                }
                if reconnect_after_loss(
                    &mut reconnect_status,
                    ReconnectTarget {
                        host,
                        id: &id,
                        visual: visual_reconnect,
                    },
                    "SSH bridge closed before the attach handshake completed".into(),
                    cursor,
                    delay,
                    &mut stdin_rx,
                    &mut filter,
                )
                .await?
                {
                    attach::restore_terminal_modes();
                    cleanup.0 = false;
                    return Ok(0);
                }
                if visual_reconnect {
                    cursor = None;
                }
                delay = (delay * 2).min(Duration::from_secs(5));
                continue;
            }
            Ok(Err(error)) => {
                let _ = child.kill().await;
                if !established_once || !reconnect {
                    return Err(error.into());
                }
                if reconnect_after_loss(
                    &mut reconnect_status,
                    ReconnectTarget {
                        host,
                        id: &id,
                        visual: visual_reconnect,
                    },
                    format!("attach handshake failed: {error}"),
                    cursor,
                    delay,
                    &mut stdin_rx,
                    &mut filter,
                )
                .await?
                {
                    attach::restore_terminal_modes();
                    cleanup.0 = false;
                    return Ok(0);
                }
                if visual_reconnect {
                    cursor = None;
                }
                delay = (delay * 2).min(Duration::from_secs(5));
                continue;
            }
            Err(_) => {
                let _ = child.kill().await;
                if !established_once {
                    bail!("timed out waiting for remote attach handshake");
                }
                if !reconnect {
                    bail!("remote attach transport timed out");
                }
                if reconnect_after_loss(
                    &mut reconnect_status,
                    ReconnectTarget {
                        host,
                        id: &id,
                        visual: visual_reconnect,
                    },
                    "timed out waiting for the attach handshake".into(),
                    cursor,
                    delay,
                    &mut stdin_rx,
                    &mut filter,
                )
                .await?
                {
                    attach::restore_terminal_modes();
                    cleanup.0 = false;
                    return Ok(0);
                }
                if visual_reconnect {
                    cursor = None;
                }
                delay = (delay * 2).min(Duration::from_secs(5));
                continue;
            }
        }

        let escape_timeout = tokio::time::sleep(Duration::from_secs(24 * 60 * 60));
        tokio::pin!(escape_timeout);
        loop {
            tokio::select! {
                frame = attach::read_frame(&mut reader) => match frame {
                    Ok(Some((S_OUTPUT_OFFSET, payload))) if payload.len() >= 8 => {
                        let start = u64::from_be_bytes(payload[..8].try_into().unwrap());
                        let bytes = &payload[8..];
                        let (skip, next) = match offset_frame_progress(cursor, start, bytes.len()) {
                            Ok(progress) => progress,
                            Err((expected, received)) => {
                                let _ = child.kill().await;
                                if !reconnect {
                                    bail!("remote output gap: expected offset {expected}, received {received}");
                                }
                                if reconnect_after_loss(
                                    &mut reconnect_status,
                                    ReconnectTarget {
                                        host,
                                        id: &id,
                                        visual: visual_reconnect,
                                    },
                                    format!(
                                        "remote output gap: expected offset {expected}, received {received}"
                                    ),
                                    cursor,
                                    delay,
                                    &mut stdin_rx,
                                    &mut filter,
                                )
                                .await?
                                {
                                    attach::restore_terminal_modes();
                                    cleanup.0 = false;
                                    return Ok(0);
                                }
                                if visual_reconnect {
                                    cursor = None;
                                }
                                delay = (delay * 2).min(Duration::from_secs(5));
                                continue 'reconnect;
                            }
                        };
                        std::io::stdout().write_all(&bytes[skip..])?;
                        std::io::stdout().flush()?;
                        cursor = Some(next);
                    }
                    Ok(Some((S_OUTPUT, payload))) => {
                        std::io::stdout().write_all(&payload)?;
                        std::io::stdout().flush()?;
                        cursor = None;
                        resumable = false;
                    }
                    Ok(Some((S_ERROR, payload))) => {
                        let _ = child.kill().await;
                        bail!("remote attach failed: {}", String::from_utf8_lossy(&payload));
                    }
                    Ok(Some((S_EXIT, payload))) => {
                        let _ = child.kill().await;
                        cleanup.0 = false;
                        return Ok(attach::parse_exit(&payload));
                    }
                    Ok(Some((S_DETACHED, _))) => {
                        let _ = child.kill().await;
                        attach::restore_terminal_modes();
                        cleanup.0 = false;
                        return Ok(0);
                    }
                    Ok(Some((S_READY, _))) | Ok(Some(_)) => {}
                    result @ (Ok(None) | Err(_)) => {
                        let _ = child.kill().await;
                        if !reconnect { bail!("remote attach transport disconnected"); }
                        if !resumable {
                            bail!("remote reconnect is unavailable for --view-cmd streams");
                        }
                        let reason = match result {
                            Ok(None) => "remote attach stream closed".into(),
                            Err(error) => format!("remote attach stream failed: {error}"),
                            _ => unreachable!(),
                        };
                        if reconnect_after_loss(
                            &mut reconnect_status,
                            ReconnectTarget {
                                host,
                                id: &id,
                                visual: visual_reconnect,
                            },
                            reason,
                            cursor,
                            delay,
                            &mut stdin_rx,
                            &mut filter,
                        )
                        .await?
                        {
                            attach::restore_terminal_modes();
                            cleanup.0 = false;
                            return Ok(0);
                        }
                        if visual_reconnect {
                            cursor = None;
                        }
                        delay = (delay * 2).min(Duration::from_secs(5));
                        continue 'reconnect;
                    }
                },
                chunk = stdin_rx.recv() => if let Some(bytes) = chunk {
                    let (forward, detach) = filter.push(&bytes);
                    if !forward.is_empty()
                        && let Err(error) = attach::write_frame(&mut child_in, C_INPUT, &forward).await
                    {
                        let _ = child.kill().await;
                        if !reconnect { bail!("remote attach transport disconnected"); }
                        if !resumable {
                            bail!("remote reconnect is unavailable for --view-cmd streams");
                        }
                        if reconnect_after_loss(
                            &mut reconnect_status,
                            ReconnectTarget {
                                host,
                                id: &id,
                                visual: visual_reconnect,
                            },
                            format!("failed to send remote input: {error}"),
                            cursor,
                            delay,
                            &mut stdin_rx,
                            &mut filter,
                        )
                        .await?
                        {
                            attach::restore_terminal_modes();
                            cleanup.0 = false;
                            return Ok(0);
                        }
                        if visual_reconnect {
                            cursor = None;
                        }
                        delay = (delay * 2).min(Duration::from_secs(5));
                        continue 'reconnect;
                    }
                    if detach {
                        let _ = child.kill().await;
                        attach::restore_terminal_modes();
                        cleanup.0 = false;
                        return Ok(0);
                    }
                    if filter.has_partial_escape() {
                        escape_timeout.as_mut().reset(
                            tokio::time::Instant::now() + Duration::from_millis(10),
                        );
                    }
                },
                _ = &mut escape_timeout, if filter.has_partial_escape() => {
                    let forward = filter.flush_partial_escape();
                    if !forward.is_empty()
                        && let Err(error) = attach::write_frame(&mut child_in, C_INPUT, &forward).await
                    {
                        let _ = child.kill().await;
                        if !reconnect { bail!("remote attach transport disconnected"); }
                        if !resumable {
                            bail!("remote reconnect is unavailable for --view-cmd streams");
                        }
                        if reconnect_after_loss(
                            &mut reconnect_status,
                            ReconnectTarget {
                                host,
                                id: &id,
                                visual: visual_reconnect,
                            },
                            format!("failed to flush remote input: {error}"),
                            cursor,
                            delay,
                            &mut stdin_rx,
                            &mut filter,
                        )
                        .await?
                        {
                            attach::restore_terminal_modes();
                            cleanup.0 = false;
                            return Ok(0);
                        }
                        if visual_reconnect {
                            cursor = None;
                        }
                        delay = (delay * 2).min(Duration::from_secs(5));
                        continue 'reconnect;
                    }
                },
                _ = winch.recv() => if let Ok((cols, rows)) = crossterm::terminal::size()
                    && let Err(error) = attach::write_frame(
                        &mut child_in,
                        C_RESIZE,
                        &attach::resize_payload(cols, rows),
                    )
                    .await
                {
                    let _ = child.kill().await;
                    if !reconnect { bail!("remote attach transport disconnected"); }
                    if !resumable {
                        bail!("remote reconnect is unavailable for --view-cmd streams");
                    }
                    if reconnect_after_loss(
                        &mut reconnect_status,
                        ReconnectTarget {
                            host,
                            id: &id,
                            visual: visual_reconnect,
                        },
                        format!("failed to resize the remote terminal: {error}"),
                        cursor,
                        delay,
                        &mut stdin_rx,
                        &mut filter,
                    )
                    .await?
                    {
                        attach::restore_terminal_modes();
                        cleanup.0 = false;
                        return Ok(0);
                    }
                    if visual_reconnect {
                        cursor = None;
                    }
                    delay = (delay * 2).min(Duration::from_secs(5));
                    continue 'reconnect;
                }
            }
        }
    }
}

fn discard_queued_input(
    stdin_rx: &mut mpsc::UnboundedReceiver<Vec<u8>>,
    filter: &mut DetachFilter,
) -> bool {
    while let Ok(bytes) = stdin_rx.try_recv() {
        let (_, detach) = filter.push(&bytes);
        if detach {
            return true;
        }
    }
    false
}

fn offset_frame_progress(
    cursor: Option<u64>,
    start: u64,
    len: usize,
) -> std::result::Result<(usize, u64), (u64, u64)> {
    let expected = cursor.unwrap_or(start);
    if start > expected {
        return Err((expected, start));
    }
    let skip = usize::try_from(expected.saturating_sub(start))
        .unwrap_or(usize::MAX)
        .min(len);
    let end = start.saturating_add(len as u64);
    Ok((skip, expected.max(end)))
}

async fn wait_reconnect(
    delay: Duration,
    stdin_rx: &mut mpsc::UnboundedReceiver<Vec<u8>>,
    filter: &mut DetachFilter,
) -> Result<bool> {
    let sleep = tokio::time::sleep(delay);
    tokio::pin!(sleep);
    loop {
        tokio::select! {
            _ = &mut sleep => {
                filter.discard_pending();
                return Ok(false);
            }
            chunk = stdin_rx.recv() => match chunk {
                Some(bytes) => {
                    let (_, detach) = filter.push(&bytes);
                    if detach { return Ok(true); }
                }
                None => return Ok(false),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quotes_shell_arguments() {
        assert_eq!(shell_quote(""), "''");
        assert_eq!(shell_quote("hello world"), "'hello world'");
        assert_eq!(shell_quote("a'b;$HOME"), "'a'\\''b;$HOME'");
    }

    #[test]
    fn forwards_terminal_type_to_remote_commands() {
        let args = vec!["run".into(), "echo $TERM".into()];
        assert_eq!(
            remote_command_with_term(&args, Some("xterm-256color")),
            "TERM='xterm-256color' 'babysit' 'run' 'echo $TERM'"
        );
        assert_eq!(
            remote_command_with_term(&args, Some("x'; touch /tmp/nope; echo '")),
            "TERM='x'\\''; touch /tmp/nope; echo '\\''' 'babysit' 'run' 'echo $TERM'"
        );
        assert_eq!(
            remote_command_with_term(&args, None),
            "'babysit' 'run' 'echo $TERM'"
        );
        assert_eq!(
            remote_command_with_term(&args, Some("")),
            "'babysit' 'run' 'echo $TERM'"
        );
    }

    #[test]
    fn ssh_commands_bound_silent_connections_and_connection_attempts() {
        let command = ssh_command("devbox", &["status".into()]).unwrap();
        let args = command
            .as_std()
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();

        assert_eq!(
            &args[..7],
            [
                "-T",
                "-o",
                "ServerAliveInterval=3",
                "-o",
                "ServerAliveCountMax=2",
                "-o",
                "ConnectTimeout=5",
            ]
        );
    }

    #[test]
    fn validates_direct_ssh_destinations() {
        assert!(validate_host("user@host").is_ok());
        assert!(validate_host("ssh-alias").is_ok());
        assert!(validate_host("").is_err());
        assert!(validate_host("-oProxyCommand=bad").is_err());
    }

    #[test]
    fn strips_only_global_host_before_separator() {
        let raw = vec![
            "--host".into(),
            "dev".into(),
            "run".into(),
            "--".into(),
            "cmd".into(),
            "--host".into(),
            "inner".into(),
        ];
        assert_eq!(
            strip_host_args(&raw).unwrap(),
            vec!["run", "--", "cmd", "--host", "inner"]
        );
        let raw = vec![
            "status".into(),
            "--host=dev".into(),
            "-s".into(),
            "x".into(),
        ];
        assert_eq!(strip_host_args(&raw).unwrap(), vec!["status", "-s", "x"]);
    }

    #[test]
    fn handshake_drain_discards_queued_input_and_honors_detach() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut filter = DetachFilter::default();
        tx.send(b"offline input".to_vec()).unwrap();
        assert!(!discard_queued_input(&mut rx, &mut filter));
        filter.discard_pending();
        let (forward, detached) = filter.push(b"x");
        assert_eq!(forward, b"x");
        assert!(!detached);

        tx.send(vec![0x1c, 0x1c]).unwrap();
        assert!(discard_queued_input(&mut rx, &mut filter));
    }

    #[test]
    fn overlapping_offset_frames_never_move_the_cursor_backward() {
        assert_eq!(offset_frame_progress(Some(100), 80, 10), Ok((10, 100)));
        assert_eq!(offset_frame_progress(Some(100), 90, 20), Ok((10, 110)));
        assert_eq!(offset_frame_progress(Some(100), 101, 5), Err((100, 101)));
        assert_eq!(offset_frame_progress(None, 42, 5), Ok((0, 47)));
    }

    #[test]
    fn ready_payload_reports_worker_tty_mode_and_defaults_old_bridges_to_tty() {
        assert!(ready_uses_tty(&[]));
        assert!(ready_uses_tty(&[1]));
        assert!(!ready_uses_tty(&[0]));
    }

    #[test]
    fn reconnect_status_clears_stale_display_and_shows_details() {
        let status = ReconnectStatus::new("remote attach stream closed".into(), Some(42));
        let screen = status.render("devbox", "ab12", "Reconnecting:    attempt 1 in 0.25s");

        assert!(screen.starts_with("\x1b[2J\x1b[H"));
        assert!(screen.contains("Host:            devbox"));
        assert!(screen.contains("Session:         ab12"));
        assert!(screen.contains("Last raw offset: 42"));
        assert!(screen.contains("Reason:          remote attach stream closed"));
        assert!(screen.contains("Reconnecting:    attempt 1 in 0.25s"));
        assert!(screen.contains("Input while offline is discarded"));
        assert!(screen.contains("Detach: Ctrl-\\ Ctrl-\\"));
    }

    #[test]
    fn reconnect_status_tracks_repeated_attempts_and_latest_failure() {
        let mut status = ReconnectStatus::new("connection reset".into(), Some(99));
        status.attempt = 2;
        status.update("attach handshake failed: timeout".into(), Some(123));
        status.update("attach handshake failed again".into(), None);
        let screen = status.render("devbox", "ab12", "Reconnecting:    attempt 3 in 1.00s");

        assert!(screen.contains("Reason:          connection reset"));
        assert!(screen.contains("Latest failure: attach handshake failed again"));
        assert!(screen.contains("Last raw offset: 123"));
        assert!(screen.contains("Reconnecting:    attempt 3 in 1.00s"));
    }

    #[tokio::test]
    async fn reconnect_wait_discards_input_and_recognizes_detach() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut filter = DetachFilter::default();
        tx.send(b"offline input".to_vec()).unwrap();
        assert!(
            !wait_reconnect(Duration::from_millis(1), &mut rx, &mut filter)
                .await
                .unwrap()
        );
        let (forward, detached) = filter.push(b"x");
        assert_eq!(forward, b"x");
        assert!(!detached);

        tx.send(vec![0x1c, 0x1c]).unwrap();
        assert!(
            wait_reconnect(Duration::from_secs(1), &mut rx, &mut filter)
                .await
                .unwrap()
        );
    }
}
