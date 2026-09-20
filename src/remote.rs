use crate::attach::{
    self, C_INPUT, C_RESIZE, DetachFilter, RawGuard, S_DETACHED, S_ERROR, S_EXIT, S_OUTPUT,
    S_OUTPUT_OFFSET, S_READY,
};
use crate::control::Request;
use crate::machine::Machine;
use crate::pane::ExitInfo;
use crate::paths::Babysit;
use crate::session::{self, State};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::io::{IsTerminal, Write};
use std::process::Stdio;
use std::time::Duration;
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

fn remote_command(machine: &Machine, args: &[String]) -> String {
    std::iter::once(machine.remote_command.as_str())
        .chain(args.iter().map(String::as_str))
        .map(shell_quote)
        .collect::<Vec<_>>()
        .join(" ")
}

fn ssh_command(machine: &Machine, args: &[String]) -> Command {
    let ssh = std::env::var_os("BABYSIT_SSH").unwrap_or_else(|| "ssh".into());
    let mut command = Command::new(ssh);
    command
        .arg("-T")
        .arg("--")
        .arg(&machine.target)
        .arg(remote_command(machine, args));
    command
}

pub async fn verify(machine: &Machine) -> Result<RemoteInfo> {
    let mut command = ssh_command(machine, &["__remote-info".into()]);
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

pub async fn proxy(machine: &Machine, args: &[String]) -> Result<i32> {
    let status = ssh_command(machine, args)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .await
        .context("starting ssh")?;
    Ok(status.code().unwrap_or(255))
}

pub async fn capture(machine: &Machine, args: &[String]) -> Result<std::process::Output> {
    ssh_command(machine, args)
        .stdin(Stdio::null())
        .output()
        .await
        .context("starting ssh")
}

/// After an ambiguous create transport failure, query the caller-chosen ID for
/// a bounded period. This never retries `run`, so it cannot duplicate work.
pub async fn confirm_session(machine: &Machine, id: &str) -> Result<bool> {
    for attempt in 0..10 {
        let output = capture(
            machine,
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

    match attach::connect_retry(bs, &id).await {
        Ok(Some(mut socket)) => {
            socket.write_all(hello.as_bytes()).await?;
            socket.flush().await?;
            let (mut socket_rd, mut socket_wr) = socket.into_split();
            let mut stdout = tokio::io::stdout();
            let input =
                tokio::spawn(async move { tokio::io::copy(&mut stdin, &mut socket_wr).await });
            let output = tokio::io::copy(&mut socket_rd, &mut stdout).await;
            input.abort();
            output?;
            stdout.flush().await?;
            Ok(())
        }
        Ok(None) => {
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
                            attach::write_frame(&mut stdout, S_READY, &[]).await?;
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
                attach::write_frame(&mut stdout, S_READY, &[]).await?;
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
        Err(error) if protocol >= 1 => {
            let mut stdout = tokio::io::stdout();
            attach::write_frame(&mut stdout, S_READY, &[]).await?;
            attach::write_frame(&mut stdout, S_ERROR, error.to_string().as_bytes()).await?;
            Ok(())
        }
        Err(error) => Err(error),
    }
}

fn spawn_bridge(machine: &Machine, id: &str) -> Result<Child> {
    ssh_command(
        machine,
        &["__remote-bridge".into(), "--session".into(), id.into()],
    )
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .stderr(Stdio::inherit())
    .kill_on_drop(true)
    .spawn()
    .context("starting ssh attach bridge")
}

pub async fn attach(machine: &Machine, id: String, reconnect: bool) -> Result<i32> {
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
    let mut established_once = false;
    let mut delay = Duration::from_millis(250);
    let mut cleanup = TerminalCleanup(false);

    'reconnect: loop {
        let mut child = spawn_bridge(machine, &id)?;
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
                        Some((S_READY, _)) => return Ok::<u8, std::io::Error>(1),
                        Some(_) => continue,
                        None => return Ok(0),
                    },
                    chunk = stdin_rx.recv() => {
                        if let Some(bytes) = chunk {
                            let (_, detach) = filter.push(&bytes);
                            if detach { return Ok(2); }
                        }
                    }
                }
            }
        })
        .await;

        match ready {
            Ok(Ok(1)) => {
                established_once = true;
                cleanup.0 = true;
                delay = Duration::from_millis(250);
                // Input typed while disconnected/authenticating is intentionally
                // discarded and must not leak into the resumed program.
                filter.discard_pending();
            }
            Ok(Ok(2)) => {
                let _ = child.kill().await;
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
                reconnect_notice(machine, delay);
                if wait_reconnect(delay, &mut stdin_rx, &mut filter).await? {
                    attach::restore_terminal_modes();
                    return Ok(0);
                }
                delay = (delay * 2).min(Duration::from_secs(5));
                continue;
            }
            Ok(Err(error)) => {
                let _ = child.kill().await;
                if !established_once || !reconnect {
                    return Err(error.into());
                }
                reconnect_notice(machine, delay);
                if wait_reconnect(delay, &mut stdin_rx, &mut filter).await? {
                    attach::restore_terminal_modes();
                    return Ok(0);
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
                reconnect_notice(machine, delay);
                if wait_reconnect(delay, &mut stdin_rx, &mut filter).await? {
                    attach::restore_terminal_modes();
                    return Ok(0);
                }
                delay = (delay * 2).min(Duration::from_secs(5));
                continue;
            }
        }

        loop {
            tokio::select! {
                frame = attach::read_frame(&mut reader) => match frame {
                    Ok(Some((S_OUTPUT_OFFSET, payload))) if payload.len() >= 8 => {
                        let start = u64::from_be_bytes(payload[..8].try_into().unwrap());
                        let bytes = &payload[8..];
                        let expected = cursor.unwrap_or(start);
                        if start > expected {
                            let _ = child.kill().await;
                            bail!("remote output gap: expected offset {expected}, received {start}");
                        }
                        let skip = usize::try_from(expected.saturating_sub(start)).unwrap_or(usize::MAX).min(bytes.len());
                        std::io::stdout().write_all(&bytes[skip..])?;
                        std::io::stdout().flush()?;
                        cursor = Some(start + bytes.len() as u64);
                    }
                    Ok(Some((S_OUTPUT, payload))) => {
                        std::io::stdout().write_all(&payload)?;
                        std::io::stdout().flush()?;
                        cursor = None;
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
                    Ok(None) | Err(_) => {
                        let _ = child.kill().await;
                        if !reconnect { bail!("remote attach transport disconnected"); }
                        reconnect_notice(machine, delay);
                        if wait_reconnect(delay, &mut stdin_rx, &mut filter).await? {
                            attach::restore_terminal_modes();
                            return Ok(0);
                        }
                        delay = (delay * 2).min(Duration::from_secs(5));
                        continue 'reconnect;
                    }
                },
                chunk = stdin_rx.recv() => if let Some(bytes) = chunk {
                    let (forward, detach) = filter.push(&bytes);
                    if !forward.is_empty()
                        && attach::write_frame(&mut child_in, C_INPUT, &forward).await.is_err()
                    {
                        if !reconnect { bail!("remote attach transport disconnected"); }
                        continue 'reconnect;
                    }
                    if detach {
                        let _ = child.kill().await;
                        attach::restore_terminal_modes();
                        cleanup.0 = false;
                        return Ok(0);
                    }
                },
                _ = winch.recv() => if let Ok((cols, rows)) = crossterm::terminal::size()
                    && attach::write_frame(&mut child_in, C_RESIZE, &attach::resize_payload(cols, rows)).await.is_err()
                {
                    if !reconnect { bail!("remote attach transport disconnected"); }
                    continue 'reconnect;
                }
            }
        }
    }
}

fn reconnect_notice(machine: &Machine, delay: Duration) {
    eprintln!(
        "\r\nbabysit: connection to {} lost; reconnecting in {:.2}s (detach: Ctrl-\\ Ctrl-\\)",
        machine.target,
        delay.as_secs_f32()
    );
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
