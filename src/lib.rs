//! babysit as a library.
//!
//! Historically babysit was a binary-only crate; the CLI in `main.rs` dispatched
//! straight into these modules. Exposing them here lets other Rust programs
//! (notably `looop`) drive the worker fleet IN-PROCESS instead of shelling out
//! to the `babysit` binary and re-parsing its JSON. The bin (`main.rs`) now
//! consumes this same library, so there is a single source of truth.
//!
//! The low-level modules are re-exported as-is; `api` is the curated, data-first
//! surface meant for external consumers (it returns values, never prints).

pub mod attach;
pub mod cli;
pub mod control;
pub mod pane;
pub mod paths;
pub mod render;
pub mod run;
pub mod session;
pub mod sub;
#[cfg(feature = "upgrade")]
pub mod upgrade;

/// Curated, data-first public API for embedding babysit in another program.
pub mod api {
    use crate::session::{self, State};
    use anyhow::Result;
    use serde::Serialize;

    /// A single session, flattened to plain data (no printing, no exit codes).
    /// This is the in-process equivalent of one row of `babysit ls --json`.
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

    /// List all sessions, most-recently-active first — the in-process
    /// replacement for `babysit ls --json`. Sessions whose metadata can't be
    /// read (mid-write, corrupt) are skipped, exactly like the CLI.
    pub async fn list_sessions() -> Result<Vec<SessionInfo>> {
        let ids = session::list_ids().await?;
        let mut entries: Vec<(session::Meta, session::Status, Option<String>)> = Vec::new();
        for id in &ids {
            let Ok(meta) = session::read_meta(id).await else {
                continue;
            };
            let status = session::read_status(id)
                .await
                .unwrap_or_else(|_| session::Status::starting());
            let note = session::read_note(id).await;
            entries.push((meta, status, note));
        }
        entries.sort_by_key(|e| std::cmp::Reverse(e.1.last_change));

        Ok(entries
            .into_iter()
            .map(|(m, s, note)| SessionInfo {
                alive: session::is_owner_alive(&m, &s),
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

    /// looop-owned sessions only (id prefixed `looop-`).
    pub async fn list_looop_sessions() -> Result<Vec<SessionInfo>> {
        Ok(list_sessions()
            .await?
            .into_iter()
            .filter(|s| s.id.starts_with("looop-"))
            .collect())
    }
}
