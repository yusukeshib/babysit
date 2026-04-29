use anyhow::{Context, Result};
use directories::BaseDirs;
use std::path::PathBuf;

pub fn root() -> Result<PathBuf> {
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
