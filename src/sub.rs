//! `babysit` subcommand handlers (the "API" surface that agents use).
//!
//! `list` is answered directly from disk. The other subcommands open a
//! short-lived connection to the session's control socket and forward the
//! request as a JSON line.

use crate::control::{Request, Response};
use crate::paths;
use crate::session::{self, State, Status};
use anyhow::{Context, Result, anyhow};
use chrono::{DateTime, Utc};
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
        entries.push((meta, status));
    }
    // Most-recently-active first.
    entries.sort_by_key(|e| std::cmp::Reverse(e.1.last_change));

    if json {
        let arr: Vec<serde_json::Value> = entries
            .iter()
            .map(|(m, s)| {
                serde_json::json!({
                    "id": m.id,
                    "name": m.name,
                    "cmd": m.cmd,
                    "agent": m.agent,
                    "state": s.state,
                    "exit_code": s.exit_code,
                    "started_at": m.started_at,
                    "last_change": s.last_change,
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&arr)?);
    } else if entries.is_empty() {
        println!("(no sessions)");
    } else {
        println!(
            "{:<10} {:<14} {:<10} {:<8} {:<10} CMD",
            "ID", "NAME", "AGENT", "STATE", "AGE"
        );
        for (m, s) in &entries {
            let age = format_age(m.started_at, Utc::now());
            println!(
                "{:<10} {:<14} {:<10} {:<8} {:<10} {}",
                m.id,
                m.name.as_deref().unwrap_or("-"),
                m.agent.as_deref().unwrap_or("-"),
                state_label(s),
                age,
                m.cmd.join(" "),
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
            if let Some(name) = m.name.as_deref() {
                println!("name:    {name}");
            }
            if let Some(agent) = m.agent.as_deref() {
                println!("agent:   {agent}");
            }
        }
        println!("state:   {}", state_label(&s));
        if let Some(c) = s.exit_code {
            println!("exit:    {c}");
        }
    }
    Ok(())
}

pub async fn log(session: Option<String>, tail: Option<usize>, raw: bool) -> Result<()> {
    let id = session::resolve(session).await?;
    let resp = request(&id, &Request::Log { tail, raw }).await;
    let text = match resp {
        Ok(r) if r.ok => r
            .data
            .get("text")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        _ => {
            // Fallback: read the log file directly (handles dead session).
            let path = paths::output_log_path(&id)?;
            let bytes = tokio::fs::read(&path).await.unwrap_or_default();
            let processed = if raw {
                bytes
            } else {
                strip_ansi_escapes::strip(&bytes)
            };
            let text = String::from_utf8_lossy(&processed).into_owned();
            match tail {
                Some(n) => last_n_lines(&text, n),
                None => text,
            }
        }
    };
    print!("{text}");
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

pub async fn send(session: Option<String>, text: String) -> Result<()> {
    let id = session::resolve(session).await?;
    let r = request(&id, &Request::Send { text: text.clone() }).await?;
    if !r.ok {
        return Err(anyhow!(r.error.unwrap_or_else(|| "send failed".into())));
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

fn state_label(s: &Status) -> String {
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

fn last_n_lines(text: &str, n: usize) -> String {
    if n == 0 {
        return String::new();
    }
    let mut starts: Vec<usize> = text.match_indices('\n').map(|(i, _)| i + 1).collect();
    starts.insert(0, 0);
    let start = if starts.len() > n {
        starts[starts.len() - n]
    } else {
        0
    };
    text[start..].to_string()
}
