//! The `Babysit` context — an explicit handle to a state root.
//!
//! Every operation in the library goes through a `Babysit` value, which owns the
//! root directory. Session files live under
//! `<root>/sessions/<id>/{meta.json,status.json,output.log,note}` and short,
//! hashed control-socket paths live under the user's private
//! `~/.babysit-sockets/` directory. The library NEVER reads the environment to
//! discover its state root: the embedder passes it in via
//! [`Babysit::new`]. The only place the
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
    sockets: PathBuf,
    /// True only for the `babysit` CLI binary. The CLI exposes its session id to
    /// wrapped commands (BABYSIT_SESSION_ID, so nested `babysit` calls can omit
    /// -s) and prints the attach banner. Library embedders (e.g. `looop`) leave
    /// this off so babysit stays INVISIBLE to the wrapped program — otherwise an
    /// embedder's child (e.g. an LLM agent) sees babysit's identity and parrots
    /// `babysit attach -s …` guidance the human can't use.
    cli: bool,
    /// Explicit path to the executable re-exec'd as the detached worker
    /// supervisor. `None` = resolve at spawn time (Linux: `/proc/self/exe`, which
    /// survives the binary being replaced/deleted mid-run; elsewhere
    /// `current_exe()`). An embedder that re-execs itself (e.g. `looop`, which
    /// routes `run --detached-id` back into its own supervisor) can pin a stable
    /// path here so a long-lived process isn't broken by an upgrade/move of its
    /// own binary. See [`Babysit::with_supervisor_exe`].
    supervisor: Option<PathBuf>,
}

impl Babysit {
    /// Open a context rooted at `root`. Per-session state lives under
    /// `<root>/sessions/<id>/`. This is the explicit, env-free constructor that
    /// embedders use.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        let uid = nix::unistd::Uid::effective().as_raw();
        let sockets = BaseDirs::new()
            .map(|base| base.home_dir().join(".babysit-sockets"))
            .unwrap_or_else(|| PathBuf::from(format!("/tmp/babysit-{uid}")));
        Self {
            root: root.into(),
            sockets,
            cli: false,
            supervisor: None,
        }
    }

    /// Pin the executable that babysit re-execs as the detached worker
    /// supervisor (it is invoked as `<exe> run --detached-id <id> --root <dir>
    /// -- <cmd…>`). Use this when the embedder re-execs ITSELF and wants a stable
    /// path that outlives an upgrade/move of the running binary. When unset,
    /// babysit resolves it at spawn time, preferring `/proc/self/exe` on Linux
    /// (which stays valid even after the on-disk binary is replaced or unlinked).
    pub fn with_supervisor_exe(mut self, exe: impl Into<PathBuf>) -> Self {
        self.supervisor = Some(exe.into());
        self
    }

    /// The pinned supervisor exe override, if any (see
    /// [`with_supervisor_exe`](Self::with_supervisor_exe)).
    pub fn supervisor_override(&self) -> Option<&Path> {
        self.supervisor.as_deref()
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

    /// `~/.babysit-sockets/<root-hash>/<id-hash>` — the per-session control
    /// socket.
    ///
    /// Unix-domain socket paths are limited to roughly 104–108 bytes. Hashing
    /// both the root and session id keeps the socket path bounded even when the
    /// state root or valid 64-character session id is long. The control server
    /// creates the per-user directory privately (0700) before binding.
    pub fn control_socket_path(&self, id: &str) -> PathBuf {
        self.sockets
            .join(format!(
                "{:016x}",
                stable_hash(self.root.as_os_str().as_encoded_bytes())
            ))
            .join(format!("{:016x}", stable_hash(id.as_bytes())))
    }

    /// Socket location used before 0.13. Clients probe this as a fallback so an
    /// upgraded CLI can still control workers started by an older binary.
    pub fn legacy_control_socket_path(&self, id: &str) -> PathBuf {
        self.session_dir(id).join("control.sock")
    }

    /// `<session_dir>/note` — optional attention note set by `flag`. Its
    /// presence means the session is flagged for a human; its contents are the
    /// message.
    pub fn note_path(&self, id: &str) -> PathBuf {
        self.session_dir(id).join("note")
    }
}

/// Stable, dependency-free FNV-1a hash for compact socket names. This is not a
/// security primitive; it only needs deterministic, negligible-collision names
/// within one babysit root.
fn stable_hash(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf29ce484222325_u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
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

    #[test]
    fn supervisor_exe_override_defaults_off_and_is_settable() {
        assert_eq!(
            Babysit::new("/tmp/x").supervisor_override(),
            None,
            "no override by default — babysit resolves the exe at spawn time"
        );
        let b = Babysit::new("/tmp/x").with_supervisor_exe("/usr/local/bin/looop");
        assert_eq!(
            b.supervisor_override(),
            Some(Path::new("/usr/local/bin/looop")),
            "an embedder can pin a stable supervisor path"
        );
    }

    #[test]
    fn control_socket_path_is_short_and_stable_for_long_roots_and_session_ids() {
        let root = format!("/tmp/{}", "root".repeat(40));
        let bs = Babysit::new(&root);
        let id = "x".repeat(64);
        let path = bs.control_socket_path(&id);
        assert_eq!(path, bs.control_socket_path(&id));
        assert!(path.starts_with(BaseDirs::new().unwrap().home_dir().join(".babysit-sockets")));
        assert_eq!(path.file_name().unwrap().to_string_lossy().len(), 16);
        assert!(path.as_os_str().as_encoded_bytes().len() < 100);
        assert_ne!(path, bs.control_socket_path(&"y".repeat(64)));
        assert_ne!(path, Babysit::new("/tmp/other").control_socket_path(&id));
        assert_eq!(
            bs.legacy_control_socket_path("worker"),
            Path::new(&root).join("sessions/worker/control.sock")
        );
    }
}
