//! The `Babysit` context — an explicit handle to a state root.
//!
//! Every operation in the library goes through a `Babysit` value, which owns the
//! root directory (`<root>/sessions/<id>/{meta.json,status.json,output.log,
//! control.sock,note}`). The library NEVER reads the environment to discover its
//! root: the embedder passes it in via [`Babysit::new`]. The only place the
//! environment is consulted is [`Babysit::from_env`], a convenience for the
//! `babysit` binary itself — embedders (e.g. `looop`) compute their own root and
//! call `new`.

use anyhow::{Context, Result};
use directories::BaseDirs;
use std::path::{Path, PathBuf};

/// A handle to one babysit state root. Cheap to clone (a single `PathBuf`).
#[derive(Debug, Clone)]
pub struct Babysit {
    root: PathBuf,
    /// True only for the `babysit` CLI binary. The CLI exposes its session id to
    /// wrapped commands (BABYSIT_SESSION_ID, so nested `babysit` calls can omit
    /// -s) and prints the attach banner. Library embedders (e.g. `looop`) leave
    /// this off so babysit stays INVISIBLE to the wrapped program — otherwise an
    /// embedder's child (e.g. an LLM agent) sees babysit's identity and parrots
    /// `babysit attach -s …` guidance the human can't use.
    cli: bool,
}

impl Babysit {
    /// Open a context rooted at `root`. Per-session state lives under
    /// `<root>/sessions/<id>/`. This is the explicit, env-free constructor that
    /// embedders use.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            cli: false,
        }
    }

    /// Mark this context as the `babysit` CLI binary (not a library embedder).
    /// Enables exposing BABYSIT_SESSION_ID to wrapped commands and the attach
    /// banner. Library embedders never call this. Default: off (library-safe).
    pub fn cli_mode(mut self) -> Self {
        self.cli = true;
        self
    }

    /// Whether this is the CLI context (see [`cli_mode`](Self::cli_mode)).
    pub fn is_cli(&self) -> bool {
        self.cli
    }

    /// Binary-boundary convenience: derive the root from `$BABYSIT_DIR` (which
    /// must be absolute), falling back to `~/.babysit`. This is the ONE place
    /// the environment is read, and only the `babysit` CLI calls it; library
    /// embedders use [`Babysit::new`] with a path they computed themselves.
    pub fn from_env() -> Result<Self> {
        if let Some(dir) = std::env::var_os("BABYSIT_DIR")
            && !dir.is_empty()
        {
            let path = PathBuf::from(dir);
            if !path.is_absolute() {
                anyhow::bail!(
                    "$BABYSIT_DIR must be an absolute path (got `{}`)",
                    path.display()
                );
            }
            return Ok(Self::new(path).cli_mode());
        }
        let base = BaseDirs::new().context("could not determine home directory")?;
        Ok(Self::new(base.home_dir().join(".babysit")).cli_mode())
    }

    /// The state root (`<root>/sessions/<id>/...` live under it).
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// `<root>/sessions`.
    pub fn sessions_dir(&self) -> PathBuf {
        self.root.join("sessions")
    }

    /// `<root>/sessions/<id>`.
    pub fn session_dir(&self, id: &str) -> PathBuf {
        self.sessions_dir().join(id)
    }

    /// `<session_dir>/meta.json` — static metadata, written once at start.
    pub fn meta_path(&self, id: &str) -> PathBuf {
        self.session_dir(id).join("meta.json")
    }

    /// `<session_dir>/status.json` — live state (updated on transitions).
    pub fn status_path(&self, id: &str) -> PathBuf {
        self.session_dir(id).join("status.json")
    }

    /// `<session_dir>/output.log` — the captured PTY output stream.
    pub fn output_log_path(&self, id: &str) -> PathBuf {
        self.session_dir(id).join("output.log")
    }

    /// `<session_dir>/control.sock` — the per-session control socket.
    pub fn control_socket_path(&self, id: &str) -> PathBuf {
        self.session_dir(id).join("control.sock")
    }

    /// `<session_dir>/note` — optional attention note set by `flag`. Its
    /// presence means the session is flagged for a human; its contents are the
    /// message.
    pub fn note_path(&self, id: &str) -> PathBuf {
        self.session_dir(id).join("note")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_is_library_mode_cli_mode_opts_in() {
        // Library embedders (Babysit::new) stay invisible: no BABYSIT_SESSION_ID
        // injected into wrapped commands, no attach banner. Only the CLI opts in.
        assert!(
            !Babysit::new("/tmp/x").is_cli(),
            "library default must be off"
        );
        assert!(
            Babysit::new("/tmp/x").cli_mode().is_cli(),
            "cli_mode opts in"
        );
    }
}
