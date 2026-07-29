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

    // Fetch the small, functionally important files before the multi-hundred-MB texture packs,
    // so a fresh install becomes runnable and inspectable early instead of after 6 GB.
    // Stable: ties break on path, and the manifest itself is emitted path-sorted.
    to_download.sort_by(|a, b| priority(a).cmp(&priority(b)).then_with(|| a.path.cmp(&b.path)));

    Ok(Plan {
        to_download,
        up_to_date,
        bytes_to_download: bytes,
    })
}

/// Download-order tier: lower goes first.
///
/// Kept as a free function over `&FileEntry` (rather than a field on the entry, or logic inlined
/// into `build`) for two reasons: `build` is `async` and does real filesystem work, so it is
/// awkward to unit-test, and adding a field to `FileEntry` would break the literal in
/// `download.rs`'s test helper. Same shape as `ui::category_of`.
pub fn priority(entry: &FileEntry) -> u8 {
    // Tier 0 needs no path matching: publish.py emits exactly two component values, "engine"
    // (the 12 root binaries + default.fmf) and "game" (everything else).
    if entry.component == "engine" {
        return 0;
    }

    let Some((_gamedir, rest)) = entry.path.split_once('/') else {
        return 0; // any other root-level file is engine-adjacent; keep it up front
    };

    if rest.contains('/') {
        // Config directory. `data/` is the historical name, `cfg/` the current one — accept
        // both so this stays correct either side of the rename.
        if rest.starts_with("cfg/") || rest.starts_with("data/") {
            return 2;
        }
        return 3;
    }

    // Top level of the gamedir. MUST be extension-matched, not depth-matched: this same bucket
    // holds ambientcg_01.pk3 and ambientcg_02.pk3, the two largest objects in the release, so a
    // depth rule would drag 677 MB to the very front — the exact opposite of the intent.
    match rest.rsplit('.').next() {
        Some("dat") | Some("fgd") | Some("cfg") | Some("txt") => 1,
        _ => 3,
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn e(path: &str, component: &str) -> FileEntry {
        FileEntry {
            path: path.to_string(),
            size: 1,
            hash: String::new(),
            component: component.to_string(),
            platform: "all".to_string(),
            exec: false,
        }
    }

    #[test]
    fn tiers_match_the_intended_order() {
        // 0: the engine set, identified by component rather than by path
        assert_eq!(priority(&e("fteqw64.exe", "engine")), 0);
        assert_eq!(priority(&e("sqlite3.dll", "engine")), 0);
        assert_eq!(priority(&e("default.fmf", "engine")), 0);
        assert_eq!(priority(&e("fteplug_box3d_x64.dll", "engine")), 0);

        // 1: gamedir-root game data, by extension
        assert_eq!(priority(&e("quakers/qwprogs.dat", "game")), 1);
        assert_eq!(priority(&e("quakers/menu.dat", "game")), 1);
        assert_eq!(priority(&e("quakers/nettest.fgd", "game")), 1);
        assert_eq!(priority(&e("quakers/default.cfg", "game")), 1);
        assert_eq!(priority(&e("quakers/fs_addons.txt", "game")), 1);

        // 2: the config directory, under either name
        assert_eq!(priority(&e("quakers/cfg/default.cfg", "game")), 2);
        assert_eq!(priority(&e("quakers/cfg/scripts/sprays.shader", "game")), 2);
        assert_eq!(priority(&e("quakers/data/server.cfg", "game")), 2);

        // 3: bulk content
        assert_eq!(priority(&e("quakers/gfx/blobshadow.png", "game")), 3);
        assert_eq!(priority(&e("quakers/models/props/x.iqm", "game")), 3);
    }

    /// The regression that actually matters. The two ambientcg packs sit at the top level of the
    /// gamedir alongside the .dat/.cfg files, so a "depth == 1" priority rule would pull 677 MB
    /// to the front of the queue — the precise opposite of what this ordering is for.
    #[test]
    fn top_level_packs_are_not_prioritised() {
        for p in [
            "quakers/ambientcg_01.pk3",
            "quakers/ambientcg_02.pk3",
            "quakers/polyhaven_props_04.pk3",
            "quakers/quakers_csprogs.pk3",
        ] {
            assert_eq!(priority(&e(p, "game")), 3, "{p} must not be prioritised");
        }
    }

    #[test]
    fn sort_puts_the_small_important_files_first() {
        let mut v = vec![
            e("quakers/ambientcg_01.pk3", "game"),
            e("quakers/gfx/a.png", "game"),
            e("quakers/cfg/server.cfg", "game"),
            e("quakers/qwprogs.dat", "game"),
            e("fteqw64.exe", "engine"),
        ];
        v.sort_by(|a, b| priority(a).cmp(&priority(b)).then_with(|| a.path.cmp(&b.path)));
        let order: Vec<&str> = v.iter().map(|x| x.path.as_str()).collect();
        assert_eq!(
            order,
            vec![
                "fteqw64.exe",
                "quakers/qwprogs.dat",
                "quakers/cfg/server.cfg",
                "quakers/ambientcg_01.pk3",
                "quakers/gfx/a.png",
            ]
        );
    }

    /// Equal-tier entries keep manifest (path) order, so a re-run queues identically — which is
    /// what makes a resumed install pick up where it left off rather than reshuffling.
    #[test]
    fn ties_break_on_path() {
        let mut v = vec![
            e("quakers/zz.dat", "game"),
            e("quakers/aa.cfg", "game"),
            e("quakers/mm.fgd", "game"),
        ];
        v.sort_by(|a, b| priority(a).cmp(&priority(b)).then_with(|| a.path.cmp(&b.path)));
        let order: Vec<&str> = v.iter().map(|x| x.path.as_str()).collect();
        assert_eq!(order, vec!["quakers/aa.cfg", "quakers/mm.fgd", "quakers/zz.dat"]);
    }
}
