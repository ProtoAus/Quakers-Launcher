//! Local install state: what we last put on disk, so a normal launch can skip
//! unchanged files by size without re-hashing 8 GB every time. `--verify` ignores
//! this and hashes everything.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Default, Serialize, Deserialize)]
pub struct State {
    #[serde(default)]
    pub version: String,
    /// install-root-relative path -> hash we installed.
    #[serde(default)]
    pub files: HashMap<String, String>,
}

pub fn state_path(install_dir: &Path) -> PathBuf {
    install_dir.join(".quakers-launcher").join("state.json")
}

pub fn load(install_dir: &Path) -> State {
    match std::fs::read_to_string(state_path(install_dir)) {
        Ok(s) => serde_json::from_str(&s).unwrap_or_default(),
        Err(_) => State::default(),
    }
}

pub fn save(install_dir: &Path, state: &State) -> anyhow::Result<()> {
    let p = state_path(install_dir);
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = p.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(state)?)?;
    // best-effort atomic replace
    let _ = std::fs::remove_file(&p);
    std::fs::rename(&tmp, &p)?;
    Ok(())
}
