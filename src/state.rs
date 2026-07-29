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
    install_dir.join("quakers-launcher.json")
}

/// Where the ledger lived until 2026-07-28: a whole directory holding one file. The leading dot
/// hides it on unix but not on Windows, where it just read as clutter next to the game binaries.
fn legacy_state_path(install_dir: &Path) -> PathBuf {
    install_dir.join(".quakers-launcher").join("state.json")
}

pub fn load(install_dir: &Path) -> State {
    // Fall back to the old location so an existing install keeps its ledger across the move.
    // Losing it is not fatal (fast mode would trust on-disk sizes) but it would make the next
    // run re-verify the whole tree for no reason.
    let text = std::fs::read_to_string(state_path(install_dir))
        .or_else(|_| std::fs::read_to_string(legacy_state_path(install_dir)));
    match text {
        Ok(s) => serde_json::from_str(&s).unwrap_or_default(),
        Err(_) => State::default(),
    }
}

pub fn save(install_dir: &Path, state: &State) -> anyhow::Result<()> {
    let p = state_path(install_dir);
    let tmp = p.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(state)?)?;
    // best-effort atomic replace
    let _ = std::fs::remove_file(&p);
    std::fs::rename(&tmp, &p)?;

    // Only once the new ledger is safely on disk: retire the old directory. remove_dir (not
    // remove_dir_all) so anything unexpected in there stops us rather than being deleted.
    let legacy = legacy_state_path(install_dir);
    if legacy.is_file() {
        let _ = std::fs::remove_file(&legacy);
        if let Some(dir) = legacy.parent() {
            let _ = std::fs::remove_dir(dir);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An install written by <=0.1.6 must keep its ledger, then end up with only the new file.
    #[test]
    fn legacy_ledger_is_read_then_retired() {
        let dir = std::env::temp_dir().join(format!("qkstate{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".quakers-launcher")).unwrap();
        std::fs::write(
            legacy_state_path(&dir),
            br#"{"version":"old","files":{"quakers/menu.dat":"abc"}}"#,
        )
        .unwrap();

        let st = load(&dir);
        assert_eq!(st.version, "old");
        assert_eq!(st.files.get("quakers/menu.dat").map(String::as_str), Some("abc"));

        save(&dir, &st).unwrap();
        assert!(state_path(&dir).is_file(), "new ledger must exist");
        assert!(!legacy_state_path(&dir).exists(), "old ledger must be gone");
        assert!(!dir.join(".quakers-launcher").exists(), "old dir must be gone");

        // and the migrated content survives a round trip
        assert_eq!(load(&dir).version, "old");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
