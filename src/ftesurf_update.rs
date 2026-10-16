//! FTESurf's signed, small-install adapter. The Quakers transport/hashes are reused;
//! the disk policy and transactional install are deliberately separate from its 6 GB game.
use crate::{
    hashing::hash_file,
    manifest::{FileEntry, Manifest},
    state::State,
};
use anyhow::{ensure, Context, Result};
use ed25519_dalek::{Signature, VerifyingKey};
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
};

pub const PUBLIC_KEY: &str = "18c7c332f5bbc913a0fceef38eab9494ce79cf60771fdda093a2d5eabef86cb6";
pub const FEED_URL: &str = "https://proto.bar/ftesurf/update.json";
pub const META: &str = ".ftesurf-updater";
pub const MAX_FEED: usize = 2 * 1024 * 1024;

#[derive(Deserialize)]
struct Envelope {
    payload: String,
    signature: String,
}

/// Verify the EXACT decoded UTF-8 payload bytes, never a reserialized JSON object.
/// Public key is a parameter only for library tests; the shipped frontend pins PUBLIC_KEY.
pub fn decode(text: &str, public_key: &str, allow_insecure: bool) -> Result<Manifest> {
    ensure!(text.len() <= MAX_FEED, "update manifest is too large");
    let e: Envelope = serde_json::from_str(text).context("signed update envelope")?;
    ensure!(!e.payload.contains('\0'), "NUL in update payload");
    let key: [u8; 32] = hex::decode(public_key)?
        .try_into()
        .map_err(|_| anyhow::anyhow!("public key length"))?;
    let sig = Signature::from_slice(&hex::decode(e.signature)?)?;
    VerifyingKey::from_bytes(&key)?
        .verify_strict(e.payload.as_bytes(), &sig)
        .context("update manifest signature is invalid")?;
    let value: serde_json::Value = serde_json::from_str(&e.payload)?;
    ensure!(value["game"] == "ftesurf", "not an FTESurf update");
    let m: Manifest = serde_json::from_value(value)?;
    ensure!(
        m.schema == 2 && m.hash_algo == "blake2b-256",
        "unsupported update format"
    );
    version(&m.version)?;
    ensure!(
        m.channel == "stable" && m.object_layout == "objects/{h0h1}/{hash}",
        "unsupported channel/layout"
    );
    ensure!(
        m.removed.is_empty(),
        "this updater does not delete retired files"
    );
    ensure!(!m.mirrors.is_empty(), "update has no object mirror");
    for mirror in &m.mirrors {
        let u = reqwest::Url::parse(mirror)?;
        ensure!(
            u.scheme() == "https"
                || (allow_insecure
                    && u.scheme() == "http"
                    && matches!(u.host_str(), Some("127.0.0.1" | "localhost" | "[::1]"))),
            "object mirrors must use HTTPS (test HTTP is loopback only)"
        );
        ensure!(
            u.username().is_empty()
                && u.password().is_none()
                && u.query().is_none()
                && u.fragment().is_none(),
            "invalid object mirror"
        );
    }
    let mut paths = HashSet::new();
    let mut sizes = HashMap::new();
    let mut bytes = 0u64;
    for f in &m.files {
        ensure!(managed(&f.path), "unmanaged update path: {}", f.path);
        ensure!(
            matches!(f.platform.as_str(), "all" | "win64" | "linux64"),
            "unknown platform"
        );
        ensure!(
            paths.insert((f.path.to_ascii_lowercase(), f.platform.clone())),
            "duplicate update path"
        );
        ensure!(
            f.hash.len() == 64
                && f.hash
                    .bytes()
                    .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c)),
            "invalid object hash"
        );
        ensure!(f.size <= 32 * 1024 * 1024, "update file too large");
        if let Some(size) = sizes.insert(&f.hash, f.size) {
            ensure!(
                size == f.size,
                "inconsistent sizes for the same object hash"
            );
        }
        bytes = bytes.checked_add(f.size).context("update size overflow")?;
    }
    ensure!(
        m.files.len() <= 8192 && bytes <= 160 * 1024 * 1024,
        "update exceeds small-install limits"
    );
    // An 'all' entry and a native entry for the same path would install two versions.
    let mut native = HashSet::new();
    for f in m.files_for_platform() {
        ensure!(
            native.insert(f.path.to_ascii_lowercase()),
            "overlapping platform entries"
        );
    }
    let e = m.exec_for_platform();
    ensure!(
        e.cmd == default_exec() && e.args == ["-manifest", "default.fmf"],
        "unexpected launch command"
    );
    for must in [
        default_exec(),
        "default.fmf",
        "ftesurf/menu.dat",
        "ftesurf/csprogs.dat",
        "ftesurf/qwprogs.dat",
    ] {
        ensure!(
            m.files_for_platform().any(|f| f.path == must),
            "missing required update file: {must}"
        );
    }
    Ok(m)
}

pub fn default_exec() -> &'static str {
    if cfg!(windows) {
        "ftesurf64.exe"
    } else {
        "ftesurf64"
    }
}

pub fn version(v: &str) -> Result<(u32, u32, u32)> {
    let parts: Vec<_> = v.split('.').collect();
    ensure!(
        parts.len() == 3
            && parts
                .iter()
                .all(|p| !p.is_empty() && p.bytes().all(|c| c.is_ascii_digit())),
        "invalid release version"
    );
    Ok((parts[0].parse()?, parts[1].parse()?, parts[2].parse()?))
}

/// Kept in step with updatefeed.py and qkupdate.c; publisher input is a verified release
/// stage, NOT an install scan. This second boundary protects player-owned paths even from
/// a mistakenly authored release. No maps, Steam assets, identities, or arbitrary data.
pub fn managed(p: &str) -> bool {
    if p.is_empty()
        || !p.is_ascii()
        || p.len() >= 240
        || p.contains(['\\', ':', '"', '*', '<', '>', '?', '|'])
    {
        return false;
    }
    if p.split('/').any(|s| {
        s.is_empty()
            || s == "."
            || s == ".."
            || s.ends_with(['.', ' '])
            || s.bytes().any(|c| c < 32)
    }) {
        return false;
    }
    for segment in p.split('/') {
        let base = segment.split('.').next().unwrap().to_ascii_uppercase();
        if matches!(base.as_str(), "CON" | "PRN" | "AUX" | "NUL")
            || (base.len() == 4
                && (base.starts_with("COM") || base.starts_with("LPT"))
                && matches!(base.as_bytes()[3], b'1'..=b'9'))
        {
            return false;
        }
    }
    if let Some(tail) = p.strip_prefix("notices/") {
        return !tail.contains('/') && tail.ends_with(".txt");
    }
    if matches!(
        p,
        "default.fmf"
            | "ftesurf.bat"
            | "ftesurf.sh"
            | "ftesurf_debug.bat"
            | "LICENSE"
            | "INSTALL.txt"
            | "VERSION.txt"
            | "SOURCE.txt"
            | "THIRD-PARTY.txt"
            | "ftesurf64.exe"
            | "ftesurf64"
            | "fteplug_hl2_x64.dll"
            | "fteplug_ui_imgui_x64.dll"
            | "fteplug_hl2_amd64.so"
    ) {
        return true;
    }
    if matches!(
        p,
        "ftesurf/csprogs.dat"
            | "ftesurf/qwprogs.dat"
            | "ftesurf/menu.dat"
            | "ftesurf/fs_addons.default.txt"
            | "ftesurf/fs_addons.txt"
    ) {
        return true;
    }
    if matches!(
        p,
        "ftesurf/data/mapmeta.txt"
            | "ftesurf/data/mapdeps.txt"
            | "ftesurf/data/mapmeta_override.txt"
            | "ftesurf/data/mapparticles.txt"
            | "ftesurf/data/mapwr.txt"
            | "ftesurf/data/mapdl.txt"
            | "ftesurf/data/mapmanifest.txt"
            | "ftesurf/maps/tiers.txt"
    ) {
        return true;
    }
    if let Some(tail) = p.strip_prefix("ftesurf/maps/zones/local/") {
        return !tail.contains('/') && (tail.ends_with(".json") || tail == "manifest.txt");
    }
    // Only the shipped default config; arbitrary cfgs may be player-authored.
    if p == "ftesurf/cfg/default.cfg" {
        return true;
    }
    if let Some(tail) = p.strip_prefix("ftesurf/cfg/") {
        return tail.ends_with(".cfg")
            && !tail.starts_with("test/")
            && !tail.starts_with("testrun/")
            && !tail.contains("lobby_local")
            && !tail.starts_with("build")
            && !tail.starts_with("fs_")
            && tail != "settings.cfg"
            && (!tail.contains('/') || tail.starts_with("maps/") || tail.starts_with("lobby/"));
    }
    if p == "ftesurf/textures/no_texture.png" {
        return true;
    }
    [
        "ftesurf/gfx/",
        "ftesurf/glsl/",
        "ftesurf/particles/",
        "ftesurf/scripts/",
        "ftesurf/models/",
        "ftesurf/sound/",
    ]
    .iter()
    .any(|prefix| p.starts_with(prefix))
}

pub fn protected(p: &str) -> bool {
    p == "ftesurf/fs_addons.txt" || p.starts_with("ftesurf/maps/zones/local/")
}

/// Never follow an install symlink/junction into Steam/player data. Check every existing
/// ancestor, including the root and the final file, before reads, renames or chmod.
pub fn checked_path(root: &Path, rel: &str) -> Result<PathBuf> {
    ensure!(
        managed(rel) || rel.starts_with(".ftesurf-updater/"),
        "unmanaged local path"
    );
    ensure!(
        !rel.split('/')
            .any(|s| s.is_empty() || s == "." || s == ".."),
        "invalid local path"
    );
    for ancestor in root.ancestors() {
        if let Ok(md) = fs::symlink_metadata(ancestor) {
            #[cfg(windows)]
            {
                use std::os::windows::fs::MetadataExt;
                ensure!(
                    md.file_attributes() & 0x400 == 0,
                    "linked install ancestor: {}",
                    ancestor.display()
                );
            }
            #[cfg(not(windows))]
            ensure!(
                !md.file_type().is_symlink(),
                "linked install ancestor: {}",
                ancestor.display()
            );
        }
    }
    let mut p = root.to_path_buf();
    for part in std::iter::once("").chain(rel.split('/')) {
        if !part.is_empty() {
            p.push(part);
        }
        if let Ok(md) = fs::symlink_metadata(&p) {
            #[cfg(windows)]
            {
                use std::os::windows::fs::MetadataExt;
                ensure!(
                    md.file_attributes() & 0x400 == 0,
                    "linked install path: {}",
                    p.display()
                );
            }
            #[cfg(not(windows))]
            ensure!(
                !md.file_type().is_symlink(),
                "linked install path: {}",
                p.display()
            );
        }
    }
    Ok(p)
}

/// Staging companions are not managed install names. Reject links/reparse points and
/// non-file leaves, propagating inspection errors instead of treating them as absence.
/// Ancestors are rechecked at opens; this does not prevent concurrent ancestor swaps.
pub(crate) fn checked_download_path(path: &Path) -> Result<()> {
    for p in path.ancestors() {
        match fs::symlink_metadata(p) {
            Ok(md) => {
                #[cfg(windows)]
                {
                    use std::os::windows::fs::MetadataExt;
                    ensure!(
                        md.file_attributes() & 0x400 == 0,
                        "linked staging path: {}",
                        p.display()
                    );
                }
                #[cfg(not(windows))]
                ensure!(
                    !md.file_type().is_symlink(),
                    "linked staging path: {}",
                    p.display()
                );
                ensure!(
                    if p == path { md.is_file() } else { md.is_dir() },
                    "invalid staging file/ancestor: {}",
                    p.display()
                );
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(e)
                    .with_context(|| format!("cannot inspect staging path: {}", p.display()))
            }
        }
    }
    Ok(())
}

pub fn load_state(root: &Path) -> Result<State> {
    let p = checked_path(root, ".ftesurf-updater/state.json")?;
    match fs::read(p) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .context("install ledger is damaged; close FTESurf and inspect .ftesurf-updater/state.json before repair"),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(State::default()),
        Err(e) => Err(e.into()),
    }
}

pub async fn matches(root: &Path, f: &FileEntry) -> Result<bool> {
    let p = checked_path(root, &f.path)?;
    if !p.exists() {
        return Ok(false);
    }
    Ok(fs::metadata(&p)?.len() == f.size && hash_file(&p, "blake2b-256").await? == f.hash)
}

/// 32 MB is small enough to hash on EVERY launch. This also verifies archive adoption and
/// detects same-sized corruption; the Quakers 'trust a file by size' shortcut isn't used.
pub async fn plan(m: &Manifest, root: &Path, old: &State) -> Result<(Vec<FileEntry>, Vec<String>)> {
    let mut jobs = Vec::new();
    let mut kept = Vec::new();
    for f in m.files_for_platform() {
        if matches(root, f).await? {
            continue;
        }
        let p = checked_path(root, &f.path)?;
        if protected(&f.path) && p.exists() {
            let have = hash_file(&p, "blake2b-256").await?;
            if f.path == "ftesurf/fs_addons.txt" || old.files.get(&f.path) != Some(&have) {
                kept.push(f.path.clone());
                continue;
            }
        }
        jobs.push(f.clone());
    }
    Ok((jobs, kept))
}

#[derive(Serialize, Deserialize)]
struct Journal {
    envelope: String,
    paths: Vec<String>,
}

pub fn write_meta(root: &Path, name: &str, bytes: &[u8]) -> Result<()> {
    let p = checked_path(root, &format!("{META}/{name}"))?;
    let tmp = checked_path(root, &format!("{META}/{name}.tmp"))?;
    fs::create_dir_all(p.parent().unwrap())?;
    {
        use std::io::Write;
        let mut f = fs::File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    // Only metadata is removed here. A torn replacement cannot conceal pending game writes:
    // pending.json is created before applying and stays present until the whole set verifies.
    if p.exists() {
        fs::remove_file(&p)?;
    }
    fs::rename(tmp, p)?;
    Ok(())
}

pub fn begin(root: &Path, envelope: &str, jobs: &[FileEntry]) -> Result<()> {
    ensure!(
        !checked_path(root, ".ftesurf-updater/pending.json")?.exists(),
        "unfinished update exists"
    );
    write_meta(
        root,
        "pending.json",
        &serde_json::to_vec(&Journal {
            envelope: envelope.into(),
            paths: jobs.iter().map(|f| f.path.clone()).collect(),
        })?,
    )
}

/// Forward recovery, not a pretend multi-file atomic rename. A pending marker blocks the
/// game from booting a mixed set. One previous copy per managed file remains for rollback.
pub async fn recover(root: &Path, allow_insecure: bool) -> Result<()> {
    let pending = checked_path(root, ".ftesurf-updater/pending.json")?;
    if !pending.exists() {
        return Ok(());
    }
    let j: Journal = serde_json::from_slice(&fs::read(&pending)?)?;
    let m = decode(&j.envelope, PUBLIC_KEY, allow_insecure)?;
    let mut st = load_state(root)?;
    if !st.version.is_empty() {
        ensure!(
            version(&m.version)? >= version(&st.version)?,
            "refusing an older signed recovery release"
        );
    }
    let mut seen = HashSet::new();
    for path in &j.paths {
        ensure!(seen.insert(path.as_str()), "duplicate journal path");
        ensure!(
            m.files_for_platform().any(|f| &f.path == path),
            "journal path absent from signed manifest"
        );
    }
    // Reuse launch planning so a journal cannot overwrite locally edited protected files.
    // Validate the entire remaining set and metadata before the first live replacement.
    let (jobs, _) = plan(&m, root, &st).await?;
    for name in [
        "state.json",
        "state.json.tmp",
        "installed.json",
        "installed.json.tmp",
    ] {
        checked_path(root, &format!("{META}/{name}"))?;
    }
    let staged_root = checked_path(root, ".ftesurf-updater/download")?;
    let mut moves = Vec::new();
    for f in &jobs {
        ensure!(
            seen.contains(f.path.as_str()),
            "journal omits required repair: {}",
            f.path
        );
        ensure!(
            matches(&staged_root, f).await?,
            "staged file missing/corrupt: {} (rerun --verify after restoring staging)",
            f.path
        );
        let dest = checked_path(root, &f.path)?;
        let src = checked_path(&staged_root, &f.path)?;
        let backup = checked_path(root, &format!("{META}/previous/{}", f.path))?;
        ensure!(
            !dest.exists() || dest.is_file(),
            "live destination is not a file: {}",
            f.path
        );
        ensure!(
            !backup.exists() || backup.is_file(),
            "backup destination is not a file: {}",
            f.path
        );
        moves.push((f, dest, src, backup));
    }
    for (f, dest, src, backup) in moves {
        if dest.exists() {
            fs::create_dir_all(backup.parent().unwrap())?;
            // Fixed managed-file backup, never a recursive cleanup. If power fails
            // here the live old file remains and pending.json still blocks boot.
            if backup.exists() {
                fs::remove_file(&backup)?;
            }
            fs::rename(&dest, backup)
                .with_context(|| format!("{} is in use; close FTESurf and retry", f.path))?;
        }
        fs::create_dir_all(dest.parent().unwrap())?;
        fs::rename(src, &dest)?;
    }
    prepare_install(root, &m).await?;
    for f in m.files_for_platform() {
        if matches(root, f).await? {
            st.files.insert(f.path.clone(), f.hash.clone());
        }
    }
    st.version = m.version.clone();
    write_meta(root, "state.json", &serde_json::to_vec_pretty(&st)?)?;
    write_meta(root, "installed.json", j.envelope.as_bytes())?;
    fs::remove_file(pending)?;
    Ok(())
}

async fn check_contents(root: &Path, m: &Manifest) -> Result<()> {
    for f in m.files_for_platform() {
        if protected(&f.path) && checked_path(root, &f.path)?.is_file() {
            continue;
        }
        ensure!(matches(root, f).await?, "install needs repair: {}", f.path);
    }
    Ok(())
}

/// Byte-correct adoption/recovery still needs executable permissions. Verify the WHOLE
/// signed set before chmod, and finish this before publishing metadata/clearing pending.
/// Only existing readers gain execute access; never widen read/write or touch player files.
pub async fn prepare_install(root: &Path, m: &Manifest) -> Result<()> {
    check_contents(root, m).await?;
    #[cfg(unix)]
    for f in m
        .files_for_platform()
        .filter(|f| f.exec && !protected(&f.path))
    {
        use std::os::unix::fs::PermissionsExt;
        let p = checked_path(root, &f.path)?;
        let file = fs::File::open(&p)?;
        let mode = file.metadata()?.permissions().mode();
        let executable = mode | 0o100 | ((mode & 0o444) >> 2);
        if mode != executable {
            file.set_permissions(fs::Permissions::from_mode(executable))
                .with_context(|| format!("cannot enable execute permission: {}", f.path))?;
        }
    }
    check_complete(root, m).await
}

/// Read-only readiness check: valid hashes alone do not make a POSIX install runnable.
pub async fn check_complete(root: &Path, m: &Manifest) -> Result<()> {
    check_contents(root, m).await?;
    #[cfg(unix)]
    for f in m
        .files_for_platform()
        .filter(|f| f.exec && !protected(&f.path))
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(checked_path(root, &f.path)?)?
            .permissions()
            .mode();
        ensure!(
            mode & 0o100 != 0,
            "install needs executable permission: {} (rerun --verify)",
            f.path
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn disk_policy_protects_player_paths() {
        for p in [
            "ftesurf/data/consent.txt",
            "ftesurf/ftesurf.cfg",
            "qkey",
            "hl2/pak01_dir.vpk",
            "ftesurf/maps/a.bsp",
            "ftesurf/maps/tiers-other.txt",
            "ftesurf/maps/tiers.txt/extra",
            "ftesurf/maps/TIERS.txt",
            "ftesurf/cfg/settings.cfg",
            "../LICENSE",
            "ftesurf/gfx/../../data/consent.txt",
            "ftesurf/gfx/x.",
        ] {
            assert!(!managed(p), "{p}");
        }
        for p in [
            "ftesurf/menu.dat",
            "ftesurf/cfg/default.cfg",
            "ftesurf/maps/zones/local/surf_a.json",
            "ftesurf/fs_addons.txt",
            "ftesurf/data/mapdl.txt",
            "ftesurf/maps/tiers.txt",
        ] {
            assert!(managed(p), "{p}");
        }
    }
    #[test]
    fn versions_are_numeric_not_lexical() {
        assert!(version("0.1.100").unwrap() > version("0.1.99").unwrap());
        assert!(version("0.1.2/../../x").is_err());
    }
    #[test]
    fn signature_is_required() {
        assert!(decode(r#"{"payload":"{}","signature":"00"}"#, PUBLIC_KEY, false).is_err());
    }
}
