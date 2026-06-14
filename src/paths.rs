use anyhow::{Context, Result};
use directories::BaseDirs;
use std::path::PathBuf;

/// Root of babysit's state (`meta.json`, `status.json`, `output.log`,
/// `control.sock` per session live under `<root>/sessions/<id>/`).
///
/// Defaults to `~/.babysit`, but `$BABYSIT_DIR` overrides it — handy for
/// tests, demos, and CI that shouldn't touch the real state dir. The override
/// must be an absolute path so a worker re-exec'd with a different cwd still
/// resolves the same location.
pub fn root() -> Result<PathBuf> {
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
        return Ok(path);
    }
    let base = BaseDirs::new().context("could not determine home directory")?;
    Ok(base.home_dir().join(".babysit"))
}

pub fn sessions_dir() -> Result<PathBuf> {
    Ok(root()?.join("sessions"))
}

pub fn session_dir(id: &str) -> Result<PathBuf> {
    Ok(sessions_dir()?.join(id))
}

pub fn meta_path(id: &str) -> Result<PathBuf> {
    Ok(session_dir(id)?.join("meta.json"))
}

pub fn status_path(id: &str) -> Result<PathBuf> {
    Ok(session_dir(id)?.join("status.json"))
}

pub fn output_log_path(id: &str) -> Result<PathBuf> {
    Ok(session_dir(id)?.join("output.log"))
}

pub fn control_socket_path(id: &str) -> Result<PathBuf> {
    Ok(session_dir(id)?.join("control.sock"))
}

/// Optional attention note set by `babysit flag`. Its presence means the
/// session is flagged for a human; its contents are the message.
pub fn note_path(id: &str) -> Result<PathBuf> {
    Ok(session_dir(id)?.join("note"))
}
