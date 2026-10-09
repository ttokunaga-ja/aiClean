use anyhow::{Context, Result, bail};
use clap::ValueEnum;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum Provider {
    Claude,
    Codex,
}
impl Provider {
    pub fn name(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Roots {
    pub codex: PathBuf,
    pub claude: PathBuf,
}
impl Roots {
    pub fn from_env() -> Result<Self> {
        let home = dirs::home_dir().context("Home directory unavailable")?;
        let absolute = |p: PathBuf| -> Result<PathBuf> {
            Ok(if p.is_absolute() {
                p
            } else {
                std::env::current_dir()?.join(p)
            })
        };
        Ok(Self {
            codex: absolute(
                std::env::var_os("CODEX_HOME")
                    .map(PathBuf::from)
                    .unwrap_or_else(|| home.join(".codex")),
            )?,
            claude: absolute(
                std::env::var_os("CLAUDE_CONFIG_DIR")
                    .map(PathBuf::from)
                    .unwrap_or_else(|| home.join(".claude")),
            )?,
        })
    }
    pub fn get(&self, p: Provider) -> &Path {
        match p {
            Provider::Claude => &self.claude,
            Provider::Codex => &self.codex,
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TableRef {
    pub name: String,
    pub id_columns: Vec<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DatabaseRef {
    pub path: PathBuf,
    pub tables: Vec<TableRef>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub provider: Provider,
    pub root: PathBuf,
    pub id: String,
    pub last_message_ms: Option<i64>,
    pub files: Vec<PathBuf>,
    pub databases: Vec<DatabaseRef>,
    pub skip_reason: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Inventory {
    pub provider: Provider,
    pub sessions: Vec<Session>,
    pub warnings: Vec<String>,
}

pub fn validate_providers(providers: &[Provider]) -> Result<()> {
    if providers.is_empty() {
        bail!("Specify claude, codex, or claude codex");
    }
    let mut seen = std::collections::BTreeSet::new();
    for p in providers {
        if !seen.insert(*p) {
            bail!("Duplicate provider: {}", p.name());
        }
    }
    Ok(())
}

/// Validate before any command creates state. Receipts must survive provider cleanup.
pub fn validate_state_location(roots: &Roots, state: &Path) -> Result<()> {
    use std::path::Component;
    if !state.is_absolute()
        || state
            .components()
            .any(|c| matches!(c, Component::ParentDir | Component::CurDir))
    {
        bail!("State directory must be absolute without traversal");
    }
    for path in state.ancestors() {
        match std::fs::symlink_metadata(path) {
            Ok(meta) if meta.file_type().is_symlink() => {
                #[cfg(target_os = "macos")]
                if [Path::new("/var"), Path::new("/tmp"), Path::new("/etc")].contains(&path)
                    && std::fs::read_link(path)?
                        == Path::new("private")
                            .join(path.file_name().context("Alias name missing")?)
                {
                    continue;
                }
                bail!("State directory has a symlink component");
            }
            #[cfg(windows)]
            Ok(meta) => {
                use std::os::windows::fs::MetadataExt;
                if meta.file_attributes() & 0x400 != 0 {
                    bail!("State directory has a reparse component");
                }
            }
            #[cfg(not(windows))]
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e).context("Cannot verify state path"),
        }
    }
    fn prefix(path: &Path) -> Result<PathBuf> {
        let mut parent = path;
        let mut suffix = Vec::new();
        while !parent.exists() {
            if std::fs::symlink_metadata(parent).is_ok() {
                bail!("Inaccessible path component");
            }
            suffix.push(
                parent
                    .file_name()
                    .context("No existing path ancestor")?
                    .to_owned(),
            );
            parent = parent.parent().context("No path parent")?;
        }
        let mut canonical = parent.canonicalize()?;
        for part in suffix.into_iter().rev() {
            canonical.push(part);
        }
        Ok(canonical)
    }
    let state = prefix(state)?;
    for root in [&roots.claude, &roots.codex] {
        let root = prefix(root)?;
        if state.starts_with(&root) || root.starts_with(&state) {
            bail!("State directory must be separate from Claude and Codex data");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn state_cannot_be_inside_or_contain_provider_data() {
        let temp = tempfile::tempdir().unwrap();
        let base = temp.path().canonicalize().unwrap();
        let roots = Roots {
            claude: base.join("claude"),
            codex: base.join("codex"),
        };
        assert!(validate_state_location(&roots, &base.join("codex/state")).is_err());
        assert!(validate_state_location(&roots, &base).is_err());
        assert!(validate_state_location(&roots, &base.join("own-state")).is_ok());
        assert!(!base.join("own-state").exists());
    }
}
