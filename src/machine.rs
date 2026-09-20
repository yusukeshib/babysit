use anyhow::{Context, Result, anyhow, bail};
use directories::BaseDirs;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Machine {
    pub target: String,
    #[serde(default = "default_remote_command")]
    pub remote_command: String,
}

fn default_remote_command() -> String {
    "babysit".into()
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct MachineFile {
    #[serde(default)]
    machines: BTreeMap<String, Machine>,
}

#[derive(Debug, Clone)]
pub struct MachineStore {
    path: PathBuf,
}

impl MachineStore {
    pub fn from_env() -> Result<Self> {
        let path = if let Some(path) = std::env::var_os("BABYSIT_CONFIG")
            && !path.is_empty()
        {
            PathBuf::from(path)
        } else {
            let base = BaseDirs::new().context("could not determine config directory")?;
            base.config_dir().join("babysit/machines.json")
        };
        if !path.is_absolute() {
            bail!(
                "$BABYSIT_CONFIG must be an absolute path (got `{}`)",
                path.display()
            );
        }
        Ok(Self { path })
    }

    #[cfg(test)]
    fn at(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn get(&self, name: &str) -> Result<Machine> {
        validate_name(name)?;
        self.load()?.machines.remove(name).ok_or_else(|| {
            anyhow!("unknown host `{name}`; add it with `babysit machine add {name} <ssh-target>`")
        })
    }

    pub fn list(&self) -> Result<Vec<(String, Machine)>> {
        Ok(self.load()?.machines.into_iter().collect())
    }

    pub fn add(&self, name: String, machine: Machine) -> Result<()> {
        validate_name(&name)?;
        validate_machine(&machine)?;
        let mut file = self.load()?;
        file.machines.insert(name, machine);
        self.save(&file)
    }

    pub fn remove(&self, name: &str) -> Result<Machine> {
        validate_name(name)?;
        let mut file = self.load()?;
        let removed = file
            .machines
            .remove(name)
            .ok_or_else(|| anyhow!("machine `{name}` is not configured"))?;
        self.save(&file)?;
        Ok(removed)
    }

    fn load(&self) -> Result<MachineFile> {
        let bytes = match std::fs::read(&self.path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(MachineFile::default());
            }
            Err(error) => {
                return Err(error).with_context(|| format!("reading {}", self.path.display()));
            }
        };
        serde_json::from_slice(&bytes).with_context(|| format!("parsing {}", self.path.display()))
    }

    fn save(&self, file: &MachineFile) -> Result<()> {
        let parent = self
            .path
            .parent()
            .context("machine config has no parent directory")?;
        create_private_dir(parent)?;
        let tmp = parent.join(format!(".machines.json.{}.tmp", std::process::id()));
        let mut bytes = serde_json::to_vec_pretty(file)?;
        bytes.push(b'\n');
        write_private(&tmp, &bytes)?;
        std::fs::rename(&tmp, &self.path)
            .with_context(|| format!("replacing {}", self.path.display()))?;
        Ok(())
    }
}

pub fn validate_name(name: &str) -> Result<()> {
    if name == "local" {
        bail!("`local` is reserved for this machine");
    }
    if name.is_empty() || name.len() > 64 {
        bail!("machine name must be 1 to 64 characters");
    }
    if !name
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
    {
        bail!("machine name may contain only ASCII letters, digits, `-`, `_`, and `.`");
    }
    Ok(())
}

pub fn validate_machine(machine: &Machine) -> Result<()> {
    if machine.target.is_empty() || machine.target.starts_with('-') {
        bail!("SSH target must be non-empty and must not begin with `-`");
    }
    if machine.remote_command.trim().is_empty() {
        bail!("remote command must not be empty");
    }
    Ok(())
}

#[cfg(unix)]
fn create_private_dir(path: &Path) -> Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true).mode(0o700);
    builder
        .create(path)
        .with_context(|| format!("creating {}", path.display()))?;
    Ok(())
}

#[cfg(not(unix))]
fn create_private_dir(path: &Path) -> Result<()> {
    std::fs::create_dir_all(path).with_context(|| format!("creating {}", path.display()))
}

#[cfg(unix)]
fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

#[cfg(not(unix))]
fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    std::fs::write(path, bytes)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_store(label: &str) -> MachineStore {
        let dir = std::env::temp_dir().join(format!(
            "babysit-machine-{label}-{}-{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ));
        MachineStore::at(dir.join("machines.json"))
    }

    #[test]
    fn validates_names_and_targets() {
        assert!(validate_name("dev-1.example").is_ok());
        assert!(validate_name("local").is_err());
        assert!(validate_name("../dev").is_err());
        assert!(
            validate_machine(&Machine {
                target: "user@host".into(),
                remote_command: "babysit".into()
            })
            .is_ok()
        );
        assert!(
            validate_machine(&Machine {
                target: "-oProxyCommand=bad".into(),
                remote_command: "babysit".into()
            })
            .is_err()
        );
    }

    #[test]
    fn round_trips_and_removes_profiles() {
        let store = temp_store("roundtrip");
        let machine = Machine {
            target: "user@dev".into(),
            remote_command: "/opt/bin/babysit".into(),
        };
        store.add("dev".into(), machine.clone()).unwrap();
        assert_eq!(store.get("dev").unwrap(), machine);
        assert_eq!(store.list().unwrap().len(), 1);
        assert_eq!(store.remove("dev").unwrap(), machine);
        assert!(store.get("dev").is_err());
        let _ = std::fs::remove_dir_all(store.path().parent().unwrap());
    }
}
