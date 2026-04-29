use crate::paths;
use anyhow::{Context, Result, anyhow};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Static metadata, written once at session start.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Meta {
    pub id: String,
    pub name: Option<String>,
    pub cmd: Vec<String>,
    pub agent: Option<String>,
    pub prompt: Option<String>,
    pub babysit_pid: u32,
    pub started_at: DateTime<Utc>,
}

/// Live state, updated as the wrapped command transitions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Status {
    pub state: State,
    pub child_pid: Option<u32>,
    pub exit_code: Option<i32>,
    pub last_change: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum State {
    Starting,
    Running,
    Exited,
    Killed,
}

impl Status {
    pub fn starting() -> Self {
        Self {
            state: State::Starting,
            child_pid: None,
            exit_code: None,
            last_change: Utc::now(),
        }
    }
}

/// True if `pid` corresponds to a process this user can see.
///
/// Used to distinguish a session whose babysit owner is still running from
/// one whose owner died (crash, kill -9, reboot) without writing a terminal
/// state. Subject to PID reuse, but in practice good enough for display.
pub fn is_pid_alive(pid: u32) -> bool {
    use nix::errno::Errno;
    use nix::sys::signal::kill;
    use nix::unistd::Pid;
    matches!(
        kill(Pid::from_raw(pid as i32), None),
        Ok(_) | Err(Errno::EPERM)
    )
}

/// Generate a short, human-friendly session id ("babysit-3a7f"-style).
pub fn new_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let pid = std::process::id() as u64;
    let mix = nanos.wrapping_mul(2862933555777941757).wrapping_add(pid);
    format!("{:04x}", (mix as u16))
}

pub async fn write_meta(meta: &Meta) -> Result<()> {
    let dir = paths::session_dir(&meta.id)?;
    tokio::fs::create_dir_all(&dir).await?;
    let path = paths::meta_path(&meta.id)?;
    let json = serde_json::to_vec_pretty(meta)?;
    tokio::fs::write(&path, json).await?;
    Ok(())
}

pub async fn write_status(id: &str, status: &Status) -> Result<()> {
    let path = paths::status_path(id)?;
    let json = serde_json::to_vec_pretty(status)?;
    // Write atomically via rename to avoid torn reads.
    let tmp = path.with_extension("json.tmp");
    tokio::fs::write(&tmp, json).await?;
    tokio::fs::rename(&tmp, &path).await?;
    Ok(())
}

pub async fn read_meta(id: &str) -> Result<Meta> {
    let path = paths::meta_path(id)?;
    let bytes = tokio::fs::read(&path)
        .await
        .with_context(|| format!("reading meta for {id}"))?;
    Ok(serde_json::from_slice(&bytes)?)
}

pub async fn read_status(id: &str) -> Result<Status> {
    let path = paths::status_path(id)?;
    let bytes = tokio::fs::read(&path)
        .await
        .with_context(|| format!("reading status for {id}"))?;
    Ok(serde_json::from_slice(&bytes)?)
}

/// Enumerate all session ids by listing ~/.babysit/sessions/.
pub async fn list_ids() -> Result<Vec<String>> {
    let dir = paths::sessions_dir()?;
    if !tokio::fs::try_exists(&dir).await.unwrap_or(false) {
        return Ok(Vec::new());
    }
    let mut rd = tokio::fs::read_dir(&dir).await?;
    let mut ids = Vec::new();
    while let Some(entry) = rd.next_entry().await? {
        if entry.file_type().await?.is_dir()
            && let Some(name) = entry.file_name().to_str()
        {
            ids.push(name.to_string());
        }
    }
    Ok(ids)
}

/// Resolve a user-supplied session reference into an id.
///
/// Resolution order:
/// 1. The explicit argument, if Some.
/// 2. `$BABYSIT_SESSION_ID`, if set.
/// 3. `latest` — the session whose status was modified most recently.
pub async fn resolve(session: Option<String>) -> Result<String> {
    if let Some(s) = session {
        return resolve_one(&s).await;
    }
    if let Ok(env_id) = std::env::var("BABYSIT_SESSION_ID")
        && !env_id.is_empty()
    {
        return resolve_one(&env_id).await;
    }
    resolve_latest().await
}

async fn resolve_one(s: &str) -> Result<String> {
    if s == "latest" {
        return resolve_latest().await;
    }
    // Match by id first, then by name.
    let ids = list_ids().await?;
    if ids.iter().any(|i| i == s) {
        return Ok(s.to_string());
    }
    for id in &ids {
        if let Ok(meta) = read_meta(id).await
            && meta.name.as_deref() == Some(s)
        {
            return Ok(id.clone());
        }
    }
    Err(anyhow!("no session matching `{s}`"))
}

async fn resolve_latest() -> Result<String> {
    let ids = list_ids().await?;
    if ids.is_empty() {
        return Err(anyhow!("no sessions found"));
    }
    let mut best: Option<(String, std::time::SystemTime)> = None;
    for id in ids {
        let path = paths::status_path(&id)?;
        if let Ok(meta) = tokio::fs::metadata(&path).await {
            let modified = meta.modified().unwrap_or(std::time::SystemTime::UNIX_EPOCH);
            if best.as_ref().map(|(_, t)| modified > *t).unwrap_or(true) {
                best = Some((id.clone(), modified));
            }
        }
    }
    best.map(|(id, _)| id)
        .ok_or_else(|| anyhow!("no sessions with status"))
}
