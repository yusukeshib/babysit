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
}

impl Babysit {
    /// Open a context rooted at `root`. Per-session state lives under
    /// `<root>/sessions/<id>/`. This is the explicit, env-free constructor that
    /// embedders use.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
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
            return Ok(Self::new(path));
        }
        let base = BaseDirs::new().context("could not determine home directory")?;
        Ok(Self::new(base.home_dir().join(".babysit")))
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
