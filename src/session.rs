use crate::paths::Babysit;
use anyhow::{Context, Result, anyhow};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Static metadata, written once at session start.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Meta {
    pub id: String,
    pub cmd: Vec<String>,
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

impl State {
    /// True once the wrapped command has finished (exited or killed). A
    /// terminal state is final: `status`/`log` then read from disk without
    /// needing the worker alive.
    pub fn is_terminal(self) -> bool {
        matches!(self, State::Exited | State::Killed)
    }
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

/// A single session, flattened to plain data (no printing, no exit codes). The
/// in-process equivalent of one row of `babysit ls --json`, returned by
/// [`Babysit::list_sessions`].
#[derive(Debug, Clone, Serialize)]
pub struct SessionInfo {
    pub id: String,
    pub cmd: Vec<String>,
    pub state: String,
    pub alive: bool,
    pub exit_code: Option<i32>,
    pub note: Option<String>,
    pub started_at: String,
    pub last_change: String,
}

fn state_str(s: State) -> &'static str {
    match s {
        State::Starting => "starting",
        State::Running => "running",
        State::Exited => "exited",
        State::Killed => "killed",
    }
}

impl Babysit {
    /// List all sessions in this root, most-recently-active first — the curated,
    /// data-first surface for embedding. Sessions whose metadata can't be read
    /// (mid-write, corrupt) are skipped, exactly like the CLI.
    pub async fn list_sessions(&self) -> Result<Vec<SessionInfo>> {
        let ids = list_ids(self).await?;
        let mut entries: Vec<(Meta, Status, Option<String>)> = Vec::new();
        for id in &ids {
            let Ok(meta) = read_meta(self, id).await else {
                continue;
            };
            let status = read_status(self, id)
                .await
                .unwrap_or_else(|_| Status::starting());
            let note = read_note(self, id).await;
            entries.push((meta, status, note));
        }
        entries.sort_by_key(|e| std::cmp::Reverse(e.1.last_change));

        Ok(entries
            .into_iter()
            .map(|(m, s, note)| SessionInfo {
                alive: is_owner_alive(&m, &s),
                state: state_str(s.state).to_string(),
                id: m.id,
                cmd: m.cmd,
                exit_code: s.exit_code,
                note,
                started_at: m.started_at.to_rfc3339(),
                last_change: s.last_change.to_rfc3339(),
            })
            .collect())
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

/// True while the wrapped command's babysit owner is still running: a terminal
/// state is never alive; otherwise the owner pid must exist. Canonical check
/// shared by the CLI (`sub::list`) and the library API (`list_sessions`) so the
/// two can never drift.
pub fn is_owner_alive(meta: &Meta, status: &Status) -> bool {
    matches!(status.state, State::Starting | State::Running) && is_pid_alive(meta.babysit_pid)
}

/// Resolve the session id for a new run: validate a user-supplied `--id`,
/// or auto-generate a unique one when none was given.
pub async fn make_id(bs: &Babysit, requested: Option<String>) -> Result<String> {
    match requested {
        Some(id) => {
            validate_id(&id)?;
            let dir = bs.session_dir(&id);
            if tokio::fs::try_exists(&dir).await.unwrap_or(false) {
                return Err(anyhow!(
                    "session `{id}` already exists; pick another --id or run `babysit prune`"
                ));
            }
            Ok(id)
        }
        None => Ok(new_unique_id(bs).await),
    }
}

/// Reject ids that aren't safe as a directory name. Keeps a user-supplied
/// `--id` from escaping the sessions directory (path traversal).
fn validate_id(id: &str) -> Result<()> {
    if id.is_empty() {
        return Err(anyhow!("session id must not be empty"));
    }
    if id.len() > 64 {
        return Err(anyhow!("session id too long (max 64 characters)"));
    }
    if id == "." || id == ".." {
        return Err(anyhow!("`.` and `..` are not valid session ids"));
    }
    if !id
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    {
        return Err(anyhow!(
            "session id may only contain ASCII letters, digits, `-`, `_`, `.`"
        ));
    }
    Ok(())
}

/// Generate a short, human-friendly session id ("3a7f"-style).
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

/// Generate a short id that doesn't collide with an existing session dir.
///
/// The 16-bit space behind `new_id` is small, so without a check two
/// concurrent sessions could hash to the same id and clobber each other's
/// directory (meta/status/socket). Retry until we find a free one; fall
/// back to a raw id if the space is somehow exhausted.
pub async fn new_unique_id(bs: &Babysit) -> String {
    for _ in 0..10_000 {
        let id = new_id();
        if !tokio::fs::try_exists(bs.session_dir(&id))
            .await
            .unwrap_or(false)
        {
            return id;
        }
    }
    new_id()
}

pub async fn write_meta(bs: &Babysit, meta: &Meta) -> Result<()> {
    let dir = bs.session_dir(&meta.id);
    tokio::fs::create_dir_all(&dir).await?;
    let json = serde_json::to_vec_pretty(meta)?;
    tokio::fs::write(bs.meta_path(&meta.id), json).await?;
    Ok(())
}

pub async fn write_status(bs: &Babysit, id: &str, status: &Status) -> Result<()> {
    let path = bs.status_path(id);
    let json = serde_json::to_vec_pretty(status)?;
    // Write atomically via rename to avoid torn reads.
    let tmp = path.with_extension("json.tmp");
    tokio::fs::write(&tmp, json).await?;
    tokio::fs::rename(&tmp, &path).await?;
    Ok(())
}

pub async fn read_meta(bs: &Babysit, id: &str) -> Result<Meta> {
    let bytes = tokio::fs::read(bs.meta_path(id))
        .await
        .with_context(|| format!("reading meta for {id}"))?;
    Ok(serde_json::from_slice(&bytes)?)
}

pub async fn read_status(bs: &Babysit, id: &str) -> Result<Status> {
    let bytes = tokio::fs::read(bs.status_path(id))
        .await
        .with_context(|| format!("reading status for {id}"))?;
    Ok(serde_json::from_slice(&bytes)?)
}

/// Write an attention note for a session (`babysit flag`). Creates the
/// session dir if needed so it works regardless of the worker's state.
pub async fn write_note(bs: &Babysit, id: &str, message: &str) -> Result<()> {
    tokio::fs::create_dir_all(bs.session_dir(id)).await?;
    tokio::fs::write(bs.note_path(id), message.as_bytes()).await?;
    Ok(())
}

/// Clear a session's attention note (`babysit unflag`). Missing note is fine.
pub async fn clear_note(bs: &Babysit, id: &str) -> Result<()> {
    match tokio::fs::remove_file(bs.note_path(id)).await {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}

/// Read a session's attention note, if flagged. A present-but-empty file
/// still counts as flagged and yields an empty string.
pub async fn read_note(bs: &Babysit, id: &str) -> Option<String> {
    let bytes = tokio::fs::read(bs.note_path(id)).await.ok()?;
    Some(String::from_utf8_lossy(&bytes).trim().to_string())
}

/// Enumerate all session ids by listing `<root>/sessions/`.
pub async fn list_ids(bs: &Babysit) -> Result<Vec<String>> {
    let dir = bs.sessions_dir();
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
/// 2. `$BABYSIT_SESSION_ID`, if set (the session-selector env babysit exports
///    INTO the wrapped command, so nested `babysit` calls can omit `-s`). This
///    is a runtime *selector*, not the state-root config — the root always
///    comes from the `Babysit` context.
///
/// There is intentionally no "most recently active" fallback: an agent that
/// drives several sessions must name the one it means, so a forgotten `-s`
/// fails loudly instead of silently operating on the wrong session.
pub async fn resolve(bs: &Babysit, session: Option<String>) -> Result<String> {
    if let Some(s) = session {
        return resolve_one(bs, &s).await;
    }
    if let Ok(env_id) = std::env::var("BABYSIT_SESSION_ID")
        && !env_id.is_empty()
    {
        return resolve_one(bs, &env_id).await;
    }
    Err(anyhow!(
        "no session selected: pass -s <id> or set $BABYSIT_SESSION_ID (list ids with `babysit ls`)"
    ))
}

async fn resolve_one(bs: &Babysit, s: &str) -> Result<String> {
    let ids = list_ids(bs).await?;
    if ids.iter().any(|i| i == s) {
        return Ok(s.to_string());
    }
    Err(anyhow!("no session matching `{s}`"))
}
