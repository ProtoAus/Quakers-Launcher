//! Diff the manifest against the local install to decide what to fetch.
//!
//! Fast mode (default): a file needs downloading if it's missing or the size
//! differs, or if our state file records a different hash for it. Files whose
//! size matches and that we have no state for are trusted (Minecraft-launcher
//! behaviour) — `--verify` forces a full hash of every file instead.

use crate::hashing::hash_file;
use crate::manifest::{FileEntry, Manifest};
use crate::state::State;
use std::path::{Path, PathBuf};

pub struct Plan {
    pub to_download: Vec<FileEntry>,
    pub up_to_date: u64,
    pub bytes_to_download: u64,
}

pub async fn build(
    manifest: &Manifest,
    install_dir: &Path,
    state: &State,
    verify: bool,
) -> anyhow::Result<Plan> {
    let mut to_download = Vec::new();
    let mut up_to_date = 0u64;
    let mut bytes = 0u64;

    for entry in manifest.files_for_platform() {
        let local: PathBuf = install_dir.join(&entry.path);
        let need = match std::fs::metadata(&local) {
            Err(_) => true, // missing
            Ok(md) => {
                if md.len() != entry.size {
                    true
                } else if verify {
                    hash_file(&local, &manifest.hash_algo).await? != entry.hash
                } else {
                    match state.files.get(&entry.path) {
                        Some(h) => h != &entry.hash, // content changed since last install
                        None => false,               // trust size in fast mode
                    }
                }
            }
        };
        if need {
            bytes += entry.size;
            to_download.push(entry.clone());
        } else {
            up_to_date += 1;
        }
    }

    Ok(Plan {
        to_download,
        up_to_date,
        bytes_to_download: bytes,
    })
}

/// Make sure every `exec` file the manifest lists for this platform is actually runnable.
///
/// The downloader chmods what it writes, but a file that was already present (right size,
/// so never re-fetched) can still be missing +x — e.g. it was unpacked from a zip, restored
/// from a backup, or copied off a FAT/NTFS volume. Without this the engine is downloaded
/// perfectly and then refuses to start with "Permission denied". No-op on Windows.
#[cfg(unix)]
pub fn ensure_exec_bits(manifest: &Manifest, install_dir: &Path) {
    use std::os::unix::fs::PermissionsExt;
    for entry in manifest.files_for_platform().filter(|e| e.exec) {
        let p = install_dir.join(&entry.path);
        if let Ok(md) = std::fs::metadata(&p) {
            let mode = md.permissions().mode();
            if mode & 0o111 == 0 {
                let mut perm = md.permissions();
                perm.set_mode(mode | 0o755);
                let _ = std::fs::set_permissions(&p, perm);
            }
        }
    }
}

#[cfg(not(unix))]
pub fn ensure_exec_bits(_manifest: &Manifest, _install_dir: &Path) {}
