//! Control plane: a Unix domain socket per session that accepts JSON
//! requests and lets external callers (the `babysit` subcommands, plus the
//! sidecar agent) inspect and operate on the wrapped command.
//!
//! Wire protocol: one request per connection, newline-delimited JSON for
//! both directions:
//!
//!     →  {"op":"status"}
//!     ←  {"ok":true,"data":{...}}
//!
//! The connection closes after the response.

use crate::pane::Pane;
use crate::paths;
use crate::session;
use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{Mutex, mpsc};

/// Operations a client can request via the control socket.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    /// Read the current status (state, exit code, …) of the wrapped command.
    Status,
    /// Read the output log. `tail` returns only the last N lines; `raw`
    /// preserves ANSI escapes (otherwise they're stripped).
    Log {
        #[serde(default)]
        tail: Option<usize>,
        #[serde(default)]
        raw: bool,
    },
    /// Send text + newline to the wrapped command's stdin.
    Send { text: String },
    /// Restart the wrapped command (kill + respawn with the same argv).
    Restart,
    /// Terminate the wrapped command (SIGHUP).
    Kill,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Response {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default)]
    pub data: serde_json::Value,
}

impl Response {
    pub fn ok(data: serde_json::Value) -> Self {
        Self {
            ok: true,
            error: None,
            data,
        }
    }
    pub fn err(msg: impl Into<String>) -> Self {
        Self {
            ok: false,
            error: Some(msg.into()),
            data: serde_json::Value::Null,
        }
    }
}

/// Message from the control loop to the main loop, for actions that need
/// to mutate the App's state (i.e. restart, which replaces the pane).
pub enum LoopMessage {
    Restart,
}

/// Shared handle that the control socket task reads from. Includes a
/// `Mutex<Arc<Pane>>` so it can always see the current command pane,
/// even after a restart swaps it.
#[derive(Clone)]
pub struct Handle {
    pub session_id: String,
    pub cmd_pane: Arc<Mutex<Arc<Pane>>>,
    pub action_tx: mpsc::UnboundedSender<LoopMessage>,
}

impl Handle {
    pub fn new(
        session_id: String,
        cmd_pane: Arc<Pane>,
        action_tx: mpsc::UnboundedSender<LoopMessage>,
    ) -> Self {
        Self {
            session_id,
            cmd_pane: Arc::new(Mutex::new(cmd_pane)),
            action_tx,
        }
    }

    pub async fn replace_cmd_pane(&self, new_pane: Arc<Pane>) {
        let mut g = self.cmd_pane.lock().await;
        *g = new_pane;
    }
}

/// Bind a control socket and spawn a task that serves requests forever.
/// The task is detached; on shutdown the caller should call `cleanup()`.
pub async fn serve(handle: Handle) -> Result<()> {
    let path = paths::control_socket_path(&handle.session_id)?;
    // If a stale socket exists from a prior run with the same id, remove it.
    let _ = tokio::fs::remove_file(&path).await;
    let listener = UnixListener::bind(&path)
        .with_context(|| format!("binding control socket at {}", path.display()))?;
    tokio::spawn(async move {
        loop {
            let stream = match listener.accept().await {
                Ok((s, _)) => s,
                Err(_) => break,
            };
            let h = handle.clone();
            tokio::spawn(async move {
                let _ = handle_conn(stream, h).await;
            });
        }
    });
    Ok(())
}

async fn handle_conn(stream: UnixStream, handle: Handle) -> Result<()> {
    let (rd, mut wr) = stream.into_split();
    let mut br = BufReader::new(rd);
    let mut line = String::new();
    let n = br.read_line(&mut line).await?;
    if n == 0 {
        return Ok(());
    }

    let resp = match serde_json::from_str::<Request>(line.trim()) {
        Ok(req) => match dispatch(req, &handle).await {
            Ok(data) => Response::ok(data),
            Err(e) => Response::err(format!("{e}")),
        },
        Err(e) => Response::err(format!("invalid request: {e}")),
    };

    let mut bytes = serde_json::to_vec(&resp)?;
    bytes.push(b'\n');
    wr.write_all(&bytes).await?;
    wr.flush().await?;
    wr.shutdown().await?;
    Ok(())
}

async fn dispatch(req: Request, handle: &Handle) -> Result<serde_json::Value> {
    match req {
        Request::Status => {
            let status = session::read_status(&handle.session_id).await?;
            Ok(serde_json::to_value(status)?)
        }
        Request::Log { tail, raw } => {
            let path = paths::output_log_path(&handle.session_id)?;
            read_log(&path, tail, raw).await
        }
        Request::Send { text } => {
            let pane = handle.cmd_pane.lock().await.clone();
            pane.write_input(text.as_bytes());
            pane.write_input(b"\n");
            Ok(serde_json::json!({"sent": text.len() + 1}))
        }
        Request::Kill => {
            let pane = handle.cmd_pane.lock().await.clone();
            pane.kill();
            Ok(serde_json::json!({"killed": true}))
        }
        Request::Restart => {
            handle
                .action_tx
                .send(LoopMessage::Restart)
                .map_err(|_| anyhow!("main loop is gone"))?;
            Ok(serde_json::json!({"restart": "queued"}))
        }
    }
}

async fn read_log(path: &Path, tail: Option<usize>, raw: bool) -> Result<serde_json::Value> {
    let bytes = match tokio::fs::read(path).await {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(e) => return Err(e.into()),
    };
    let processed = if raw {
        bytes
    } else {
        strip_ansi_escapes::strip(&bytes)
    };
    let text = String::from_utf8_lossy(&processed).into_owned();
    let out = match tail {
        Some(n) => last_n_lines(&text, n),
        None => text,
    };
    Ok(serde_json::json!({"text": out}))
}

/// Return the last `n` lines of `text`, preserving the original bytes.
///
/// A single trailing newline terminates the final line rather than starting
/// an empty one, so `last_n_lines("a\nb\nc\n", 2)` is `"b\nc\n"` (two lines),
/// not `"c\n"`.
pub fn last_n_lines(text: &str, n: usize) -> String {
    if n == 0 {
        return String::new();
    }
    let trimmed = text.strip_suffix('\n').unwrap_or(text);
    let mut start = 0;
    for (seen, (i, _)) in trimmed.rmatch_indices('\n').enumerate() {
        if seen + 1 == n {
            start = i + 1;
            break;
        }
    }
    text[start..].to_string()
}

/// Best-effort cleanup: remove the socket file. Called on graceful shutdown.
pub fn cleanup(session_id: &str) {
    if let Ok(path) = paths::control_socket_path(session_id) {
        let _ = std::fs::remove_file(path);
    }
}

#[cfg(test)]
mod tests {
    use super::last_n_lines;

    #[test]
    fn tail_respects_trailing_newline() {
        // 3 logical lines + trailing newline: tail 2 keeps the last two.
        assert_eq!(last_n_lines("a\nb\nc\n", 2), "b\nc\n");
        assert_eq!(last_n_lines("a\nb\nc\n", 1), "c\n");
    }

    #[test]
    fn tail_without_trailing_newline() {
        assert_eq!(last_n_lines("a\nb\nc", 2), "b\nc");
    }

    #[test]
    fn tail_larger_than_available_returns_all() {
        assert_eq!(last_n_lines("a\nb\n", 10), "a\nb\n");
    }

    #[test]
    fn tail_zero_is_empty() {
        assert_eq!(last_n_lines("a\nb\n", 0), "");
    }

    #[test]
    fn tail_empty_input() {
        assert_eq!(last_n_lines("", 5), "");
    }
}
