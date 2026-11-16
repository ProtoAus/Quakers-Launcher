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
/// P1593: the worldofsurf feed, a sibling of FEED_URL; qkupdate.c's identity table names the same URL.
pub const WOS_FEED_URL: &str = "https://proto.bar/ftesurf/worldofsurf.json";
/// Each identity's required game files, at the root of its gamedir (feeds and install_identity).
pub const GAME_REQUIRED: [&str; 3] = ["menu.dat", "csprogs.dat", "qwprogs.dat"];
pub const META: &str = ".ftesurf-updater";
pub const MAX_FEED: usize = 2 * 1024 * 1024;

/// P1593: the two install identities; must agree with updatefeed.py and qkupdate.c. A feed is
/// wholly one. WorldOfSurf's allowlist is Legacy's with the gamedir and exe swapped
/// (`legacy_spelling`), so the two cannot drift.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Identity {
    Legacy,
    WorldOfSurf,
}

impl Identity {
    pub fn game(self) -> &'static str {
        match self {
            Identity::Legacy => "ftesurf",
            Identity::WorldOfSurf => "worldofsurf",
        }
    }
    /// With the trailing slash, as feed paths spell it.
    pub fn gamedir(self) -> &'static str {
        match self {
            Identity::Legacy => "ftesurf/",
            Identity::WorldOfSurf => "worldofsurf/",
        }
    }
    /// The win64 and linux64 executables.
    fn exes(self) -> [&'static str; 2] {
        match self {
            Identity::Legacy => ["ftesurf64.exe", "ftesurf64"],
            Identity::WorldOfSurf => ["WorldofSurf.exe", "worldofsurf64"],
        }
    }
    /// This platform's executable, the feed's exec cmd and a required file.
    pub fn exec(self) -> &'static str {
        self.exes()[usize::from(!cfg!(windows))]
    }
    /// The signed feed this identity updates from (`--manifest-url` overrides it for harnesses).
    pub fn feed_url(self) -> &'static str {
        match self {
            Identity::Legacy => FEED_URL,
            Identity::WorldOfSurf => WOS_FEED_URL,
        }
    }
}

/// What an install with neither gamedir gets; the feed URL follows. Stays Legacy until a
/// worldofsurf feed is published (stage 3 flips it).
pub const FRESH_INSTALL: Identity = Identity::Legacy;

fn inspect(p: &Path) -> Result<Option<fs::Metadata>> {
    match fs::symlink_metadata(p) {
        Ok(md) => Ok(Some(md)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("cannot inspect {}", p.display())),
    }
}

fn is_link(md: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        md.file_attributes() & 0x400 != 0
    }
    #[cfg(not(windows))]
    md.file_type().is_symlink()
}

/// A gamedir counts when it is a real directory holding at least two GAME_REQUIRED files as
/// regular files, no links. Two, not three: an update replaces one file at a time (recover()
/// here, the engine's in-game apply), so an install interrupted there is short exactly one.
pub fn gamedir_counts(root: &Path, id: Identity) -> Result<bool> {
    let dir = root.join(id.gamedir().trim_end_matches('/'));
    if !inspect(&dir)?.is_some_and(|md| md.is_dir() && !is_link(&md)) {
        return Ok(false);
    }
    let mut have = 0;
    for leaf in GAME_REQUIRED {
        if inspect(&dir.join(leaf))?.is_some_and(|md| md.is_file() && !is_link(&md)) {
            have += 1;
        }
    }
    Ok(have >= 2)
}

/// The launcher's durable identity record, outside both gamedirs; qkupdate.c reads it too.
pub const MARKER: &str = ".ftesurf-updater/identity";

/// None when absent. Anything but a regular file holding one identity word and an optional
/// newline is an error: a damaged marker is refused, never guessed past.
pub fn read_marker(root: &Path) -> Result<Option<Identity>> {
    let p = checked_path(root, MARKER, FRESH_INSTALL)?;
    let Some(md) = inspect(&p)? else {
        return Ok(None);
    };
    ensure!(
        md.is_file() && !is_link(&md) && md.len() <= 64,
        "install identity marker is not a regular file: {MARKER}"
    );
    let text = fs::read(&p)?;
    let word = text.strip_suffix(b"\n").unwrap_or(&text);
    [Identity::Legacy, Identity::WorldOfSurf]
        .into_iter()
        .find(|id| id.game().as_bytes() == word)
        .map(Some)
        .with_context(|| format!("damaged install identity marker: {MARKER}"))
}

/// Record `id` once an install is verified under it (fresh install or first verified update;
/// stage 2's folder move rewrites it). Call under the install lock.
pub fn establish_identity(root: &Path, id: Identity) -> Result<()> {
    match read_marker(root)? {
        Some(have) => {
            ensure!(
                have == id,
                "install identity marker says {}, not {}",
                have.game(),
                id.game()
            );
            Ok(())
        }
        None => write_meta(root, "identity", format!("{}\n", id.game()).as_bytes(), id),
    }
}

/// The one identity rule (P1593): the marker when there is one; otherwise the gamedir on disk.
/// Only a gamedir that counts (gamedir_counts) takes part, so a stray folder cannot lock the
/// launcher out; both counting is a half-moved install and is refused.
pub fn install_identity(root: &Path) -> Result<Identity> {
    if let Some(id) = read_marker(root)? {
        return Ok(id);
    }
    match (
        gamedir_counts(root, Identity::Legacy)?,
        gamedir_counts(root, Identity::WorldOfSurf)?,
    ) {
        (true, true) => anyhow::bail!(
            "both ftesurf/ and worldofsurf/ exist here; refusing to guess which install this is"
        ),
        (true, false) => Ok(Identity::Legacy),
        (false, true) => Ok(Identity::WorldOfSurf),
        (false, false) => Ok(FRESH_INSTALL),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Envelope<'a> {
    payload: String,
    /// Borrowed: serde_json cannot borrow a string with escapes, so the signature must be the raw
    /// 128 characters, as the engine reads it.
    #[serde(borrow)]
    signature: &'a str,
}

/// Any JSON value, refusing an object that repeats a key: serde keeps the last, the engine's
/// reader the first, so a repeated "game" would be read two ways.
struct UniqueKeys;

impl<'de> Deserialize<'de> for UniqueKeys {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        d.deserialize_any(UniqueKeysVisitor)
    }
}

struct UniqueKeysVisitor;

impl<'de> serde::de::Visitor<'de> for UniqueKeysVisitor {
    type Value = UniqueKeys;
    fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str("JSON")
    }
    fn visit_bool<E>(self, _: bool) -> std::result::Result<UniqueKeys, E> {
        Ok(UniqueKeys)
    }
    fn visit_i64<E>(self, _: i64) -> std::result::Result<UniqueKeys, E> {
        Ok(UniqueKeys)
    }
    fn visit_u64<E>(self, _: u64) -> std::result::Result<UniqueKeys, E> {
        Ok(UniqueKeys)
    }
    fn visit_f64<E>(self, _: f64) -> std::result::Result<UniqueKeys, E> {
        Ok(UniqueKeys)
    }
    fn visit_str<E>(self, _: &str) -> std::result::Result<UniqueKeys, E> {
        Ok(UniqueKeys)
    }
    fn visit_unit<E>(self) -> std::result::Result<UniqueKeys, E> {
        Ok(UniqueKeys)
    }
    fn visit_seq<A: serde::de::SeqAccess<'de>>(
        self,
        mut a: A,
    ) -> std::result::Result<UniqueKeys, A::Error> {
        while a.next_element::<UniqueKeys>()?.is_some() {}
        Ok(UniqueKeys)
    }
    fn visit_map<A: serde::de::MapAccess<'de>>(
        self,
        mut a: A,
    ) -> std::result::Result<UniqueKeys, A::Error> {
        let mut seen = HashSet::new();
        // A borrowed key has no escapes: serde_json refuses to borrow one that has, as the engine
        // (which compares raw names) refuses it.
        while let Some(k) = a.next_key::<&'de str>()? {
            // JSON_FindChild also lets "name." match "name"
            if k.contains('.') || !seen.insert(k) {
                return Err(serde::de::Error::custom(format!(
                    "repeated or dotted key \"{k}\""
                )));
            }
            a.next_value::<UniqueKeys>()?;
        }
        Ok(UniqueKeys)
    }
}

fn unique_keys(text: &str) -> Result<()> {
    serde_json::from_str::<UniqueKeys>(text).context("signed update repeats a key")?;
    Ok(())
}

/// Game arguments after `--`, an allowlist (P1593): what the docs (`-- -window +map <map>`) and
/// ftesurf.sh (`<map>`, `--debug`) pass. Windows re-splits a command line on quotes and
/// whitespace, so no element may hold either; a value never starts with `-` or `+`, since the
/// engine scans every position for its switches (-basegame, -basedir, ...).
pub fn checked_game_args(args: &[String]) -> Result<()> {
    ensure!(
        !args
            .iter()
            .any(|a| a.contains('"') || a.chars().any(char::is_whitespace)),
        "game arguments may not contain quotes or whitespace"
    );
    let value = |v: Option<&String>| {
        v.is_some_and(|v| {
            v.len() <= 64
                && v.bytes()
                    .next()
                    .is_some_and(|c| c.is_ascii_alphanumeric() || c == b'_')
                && v.bytes()
                    .all(|c| c.is_ascii_alphanumeric() || b"_.:-".contains(&c))
        })
    };
    let number = |v: Option<&String>| {
        v.is_some_and(|v| (1..=5).contains(&v.len()) && v.bytes().all(|c| c.is_ascii_digit()))
    };
    let mut i = 0;
    while i < args.len() {
        let (next, after) = (args.get(i + 1), args.get(i + 2).map(String::as_str));
        i += match args[i].as_str() {
            "-window" | "-fullscreen" | "-condebug" => 1,
            "+map" | "+connect" if value(next) => 2,
            "-width" | "-height" if number(next) => 2,
            "+set" if next.is_some_and(|n| n == "log_dir") && after == Some("logs") => 3,
            "+set" if next.is_some_and(|n| n == "developer") && after == Some("1") => 3,
            a => anyhow::bail!(
                "game argument not allowed: {a} (allowed: +map <map>, +connect <host[:port]>, \
                 -window, -fullscreen, -width N, -height N)"
            ),
        };
    }
    Ok(())
}

/// Verify the EXACT decoded UTF-8 payload bytes, never a reserialized JSON object.
/// Public key is a parameter only for library tests; the shipped frontend pins PUBLIC_KEY.
/// `id` is the install's identity: a feed for the other one is refused, never adopted.
pub fn decode(
    text: &str,
    public_key: &str,
    allow_insecure: bool,
    id: Identity,
) -> Result<Manifest> {
    ensure!(text.len() <= MAX_FEED, "update manifest is too large");
    unique_keys(text)?;
    // unique_keys parsed it, so the first non-blank byte says what it is
    ensure!(
        text.trim_start_matches([' ', '\t', '\n', '\r'])
            .starts_with('{'),
        "signed update envelope is not a JSON object"
    );
    let e: Envelope = serde_json::from_str(text).context("signed update envelope")?;
    ensure!(
        e.signature.len() == 128
            && e.signature
                .bytes()
                .all(|c| matches!(c, b'0'..=b'9' | b'a'..=b'f')),
        "update signature must be 128 lowercase hex digits"
    );
    ensure!(!e.payload.contains('\0'), "NUL in update payload");
    let key: [u8; 32] = hex::decode(public_key)?
        .try_into()
        .map_err(|_| anyhow::anyhow!("public key length"))?;
    let sig = Signature::from_slice(&hex::decode(e.signature)?)?;
    VerifyingKey::from_bytes(&key)?
        .verify_strict(e.payload.as_bytes(), &sig)
        .context("update manifest signature is invalid")?;
    unique_keys(&e.payload)?;
    let value: serde_json::Value = serde_json::from_str(&e.payload)?;
    ensure!(
        value["game"] == id.game(),
        "not an update for this install (expected game \"{}\")",
        id.game()
    );
    let removed_empty = value["removed"].as_array().is_some_and(|r| r.is_empty());
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
        removed_empty,
        "this updater needs an empty \"removed\" list (it does not delete retired files)"
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
        ensure!(managed(&f.path, id), "unmanaged update path: {}", f.path);
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
        e.cmd == id.exec() && e.args == ["-manifest", "default.fmf"],
        "unexpected launch command"
    );
    let game = GAME_REQUIRED.map(|leaf| format!("{}{leaf}", id.gamedir()));
    for must in [id.exec(), "default.fmf"]
        .into_iter()
        .chain(game.iter().map(String::as_str))
    {
        ensure!(
            m.files_for_platform().any(|f| f.path == must),
            "missing required update file: {must}"
        );
    }
    Ok(m)
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

/// `p` as the Legacy policy spells it, or None when it belongs to the other identity.
pub fn legacy_spelling(p: &str, id: Identity) -> Option<String> {
    if id == Identity::Legacy {
        return Some(p.to_string());
    }
    let (old, new) = (Identity::Legacy, id);
    if old.exes().iter().any(|e| *e == p) || p.starts_with(old.gamedir()) {
        return None;
    }
    if let Some(i) = new.exes().iter().position(|e| *e == p) {
        return Some(old.exes()[i].to_string());
    }
    Some(match p.strip_prefix(new.gamedir()) {
        Some(tail) => format!("{}{tail}", old.gamedir()),
        None => p.to_string(),
    })
}

/// This install's disk policy. The 240 limit is on the real path, before the respelling.
pub fn managed(p: &str, id: Identity) -> bool {
    p.len() < 240 && legacy_spelling(p, id).is_some_and(|q| managed_legacy(&q))
}

/// Kept in step with updatefeed.py and qkupdate.c; publisher input is a verified release
/// stage, NOT an install scan. This second boundary protects player-owned paths even from
/// a mistakenly authored release. No maps, Steam assets, identities, or arbitrary data.
fn managed_legacy(p: &str) -> bool {
    if p.is_empty()
        || p.bytes().any(|c| c >= 0x7f)
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

pub fn protected(p: &str, id: Identity) -> bool {
    legacy_spelling(p, id)
        .is_some_and(|p| p == "ftesurf/fs_addons.txt" || p.starts_with("ftesurf/maps/zones/local/"))
}

/// Never follow an install symlink/junction into Steam/player data. Check every existing
/// ancestor, including the root and the final file, before reads, renames or chmod.
pub fn checked_path(root: &Path, rel: &str, id: Identity) -> Result<PathBuf> {
    ensure!(
        managed(rel, id) || rel.starts_with(".ftesurf-updater/"),
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

pub fn load_state(root: &Path, id: Identity) -> Result<State> {
    let p = checked_path(root, ".ftesurf-updater/state.json", id)?;
    match fs::read(p) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .context("install ledger is damaged; close FTESurf and inspect .ftesurf-updater/state.json before repair"),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(State::default()),
        Err(e) => Err(e.into()),
    }
}

pub async fn matches(root: &Path, f: &FileEntry, id: Identity) -> Result<bool> {
    let p = checked_path(root, &f.path, id)?;
    if !p.exists() {
        return Ok(false);
    }
    Ok(fs::metadata(&p)?.len() == f.size && hash_file(&p, "blake2b-256").await? == f.hash)
}

/// 32 MB is small enough to hash on EVERY launch. This also verifies archive adoption and
/// detects same-sized corruption; the Quakers 'trust a file by size' shortcut isn't used.
pub async fn plan(
    m: &Manifest,
    root: &Path,
    old: &State,
    id: Identity,
) -> Result<(Vec<FileEntry>, Vec<String>)> {
    let mut jobs = Vec::new();
    let mut kept = Vec::new();
    for f in m.files_for_platform() {
        if matches(root, f, id).await? {
            continue;
        }
        let p = checked_path(root, &f.path, id)?;
        if protected(&f.path, id) && p.exists() {
            let have = hash_file(&p, "blake2b-256").await?;
            if f.path == format!("{}fs_addons.txt", id.gamedir())
                || old.files.get(&f.path) != Some(&have)
            {
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

pub fn write_meta(root: &Path, name: &str, bytes: &[u8], id: Identity) -> Result<()> {
    let p = checked_path(root, &format!("{META}/{name}"), id)?;
    let tmp = checked_path(root, &format!("{META}/{name}.tmp"), id)?;
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

pub fn begin(root: &Path, envelope: &str, jobs: &[FileEntry], id: Identity) -> Result<()> {
    ensure!(
        !checked_path(root, ".ftesurf-updater/pending.json", id)?.exists(),
        "unfinished update exists"
    );
    write_meta(
        root,
        "pending.json",
        &serde_json::to_vec(&Journal {
            envelope: envelope.into(),
            paths: jobs.iter().map(|f| f.path.clone()).collect(),
        })?,
        id,
    )
}

/// Forward recovery, not a pretend multi-file atomic rename. A pending marker blocks the
/// game from booting a mixed set. One previous copy per managed file remains for rollback.
pub async fn recover(root: &Path, allow_insecure: bool, id: Identity) -> Result<()> {
    let pending = checked_path(root, ".ftesurf-updater/pending.json", id)?;
    if !pending.exists() {
        return Ok(());
    }
    let j: Journal = serde_json::from_slice(&fs::read(&pending)?)?;
    let m = decode(&j.envelope, PUBLIC_KEY, allow_insecure, id)?;
    let mut st = load_state(root, id)?;
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
    let (jobs, _) = plan(&m, root, &st, id).await?;
    for name in [
        "state.json",
        "state.json.tmp",
        "installed.json",
        "installed.json.tmp",
    ] {
        checked_path(root, &format!("{META}/{name}"), id)?;
    }
    let staged_root = checked_path(root, ".ftesurf-updater/download", id)?;
    let mut moves = Vec::new();
    for f in &jobs {
        ensure!(
            seen.contains(f.path.as_str()),
            "journal omits required repair: {}",
            f.path
        );
        ensure!(
            matches(&staged_root, f, id).await?,
            "staged file missing/corrupt: {} (rerun --verify after restoring staging)",
            f.path
        );
        let dest = checked_path(root, &f.path, id)?;
        let src = checked_path(&staged_root, &f.path, id)?;
        let backup = checked_path(root, &format!("{META}/previous/{}", f.path), id)?;
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
    prepare_install(root, &m, id).await?;
    for f in m.files_for_platform() {
        if matches(root, f, id).await? {
            st.files.insert(f.path.clone(), f.hash.clone());
        }
    }
    st.version = m.version.clone();
    write_meta(root, "state.json", &serde_json::to_vec_pretty(&st)?, id)?;
    write_meta(root, "installed.json", j.envelope.as_bytes(), id)?;
    fs::remove_file(pending)?;
    Ok(())
}

async fn check_contents(root: &Path, m: &Manifest, id: Identity) -> Result<()> {
    for f in m.files_for_platform() {
        if protected(&f.path, id) && checked_path(root, &f.path, id)?.is_file() {
            continue;
        }
        ensure!(
            matches(root, f, id).await?,
            "install needs repair: {}",
            f.path
        );
    }
    Ok(())
}

/// Byte-correct adoption/recovery still needs executable permissions. Verify the WHOLE
/// signed set before chmod, and finish this before publishing metadata/clearing pending.
/// Only existing readers gain execute access; never widen read/write or touch player files.
pub async fn prepare_install(root: &Path, m: &Manifest, id: Identity) -> Result<()> {
    check_contents(root, m, id).await?;
    #[cfg(unix)]
    for f in m
        .files_for_platform()
        .filter(|f| f.exec && !protected(&f.path, id))
    {
        use std::os::unix::fs::PermissionsExt;
        let p = checked_path(root, &f.path, id)?;
        let file = fs::File::open(&p)?;
        let mode = file.metadata()?.permissions().mode();
        let executable = mode | 0o100 | ((mode & 0o444) >> 2);
        if mode != executable {
            file.set_permissions(fs::Permissions::from_mode(executable))
                .with_context(|| format!("cannot enable execute permission: {}", f.path))?;
        }
    }
    check_complete(root, m, id).await
}

/// Read-only readiness check: valid hashes alone do not make a POSIX install runnable.
pub async fn check_complete(root: &Path, m: &Manifest, id: Identity) -> Result<()> {
    check_contents(root, m, id).await?;
    #[cfg(unix)]
    for f in m
        .files_for_platform()
        .filter(|f| f.exec && !protected(&f.path, id))
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(checked_path(root, &f.path, id)?)?
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
            assert!(!managed(p, Identity::Legacy), "{p}");
        }
        for p in [
            "ftesurf/menu.dat",
            "ftesurf/cfg/default.cfg",
            "ftesurf/maps/zones/local/surf_a.json",
            "ftesurf/fs_addons.txt",
            "ftesurf/data/mapdl.txt",
            "ftesurf/maps/tiers.txt",
        ] {
            assert!(managed(p, Identity::Legacy), "{p}");
        }
    }
    #[test]
    fn generated_map_tables_use_existing_asset_namespace() {
        for name in ["matcolours", "unshipped"] {
            let path = format!("ftesurf/scripts/{name}.txt");
            assert!(managed(&path, Identity::Legacy), "{path}");
            for bad in [
                path.to_ascii_uppercase(),
                format!("{path}."),
                format!("{path} "),
                format!("{path}:stream"),
                format!("ftesurf/scripts/../data/{name}.txt"),
                format!("ftesurf/data/{name}.txt"),
            ] {
                assert!(!managed(&bad, Identity::Legacy), "{bad}");
            }
        }
        for bad in [
            "ftesurf/data/consent.txt",
            "ftesurf/data/player.txt",
            "ftesurf/data/settings.txt",
        ] {
            assert!(!managed(bad, Identity::Legacy), "{bad}");
        }
    }

    #[test]
    fn versions_are_numeric_not_lexical() {
        assert!(version("0.1.100").unwrap() > version("0.1.99").unwrap());
        assert!(version("0.1.2/../../x").is_err());
    }
    #[test]
    fn signature_is_required() {
        assert!(decode(
            r#"{"payload":"{}","signature":"00"}"#,
            PUBLIC_KEY,
            false,
            Identity::Legacy
        )
        .is_err());
    }

    // P1593: a test key (RFC 8032 vector 1), never the release credential.
    const TEST_SEED: [u8; 32] = [
        0x9d, 0x61, 0xb1, 0x9d, 0xef, 0xfd, 0x5a, 0x60, 0xba, 0x84, 0x4a, 0xf4, 0x92, 0xec, 0x2c,
        0xc4, 0x44, 0x49, 0xc5, 0x69, 0x7b, 0x32, 0x69, 0x19, 0x70, 0x3b, 0xac, 0x03, 0x1c, 0xae,
        0x7f, 0x60,
    ];
    const HASH: &str = "0000000000000000000000000000000000000000000000000000000000000000";

    fn signed(payload: &serde_json::Value) -> (String, String) {
        signed_text(&payload.to_string())
    }

    fn signed_text(text: &str) -> (String, String) {
        use ed25519_dalek::{Signer, SigningKey};
        let key = SigningKey::from_bytes(&TEST_SEED);
        let envelope = serde_json::json!({
            "payload": text,
            "signature": hex::encode(key.sign(text.as_bytes()).to_bytes()),
        });
        (
            envelope.to_string(),
            hex::encode(key.verifying_key().to_bytes()),
        )
    }

    /// A complete two-platform feed for `id`, as updatefeed.py builds it.
    fn feed_for(id: Identity) -> serde_json::Value {
        let [win, linux] = id.exes();
        let mut files = vec![
            serde_json::json!({"path": win, "hash": HASH, "size": 1, "platform": "win64", "exec": false}),
            serde_json::json!({"path": linux, "hash": HASH, "size": 1, "platform": "linux64", "exec": true}),
            serde_json::json!({"path": "default.fmf", "hash": HASH, "size": 1, "platform": "all", "exec": false}),
        ];
        for leaf in ["menu.dat", "csprogs.dat", "qwprogs.dat"] {
            files.push(
                serde_json::json!({"path": format!("{}{leaf}", id.gamedir()), "hash": HASH,
                "size": 1, "platform": "all", "exec": false}),
            );
        }
        serde_json::json!({
            "schema": 2, "hash_algo": "blake2b-256", "game": id.game(), "channel": "stable",
            "version": "0.1.25", "object_layout": "objects/{h0h1}/{hash}",
            "mirrors": ["https://dl.proto.bar/ftesurf/update"], "files": files, "removed": [],
            "exec": {"cmd": win, "args": ["-manifest", "default.fmf"]},
            "exec_by_platform": {
                "win64": {"cmd": win, "args": ["-manifest", "default.fmf"]},
                "linux64": {"cmd": linux, "args": ["-manifest", "default.fmf"]},
            },
        })
    }

    fn accepts(payload: &serde_json::Value, id: Identity) -> bool {
        let (text, key) = signed(payload);
        decode(&text, &key, false, id).is_ok()
    }

    fn with_file(mut payload: serde_json::Value, path: &str, platform: &str) -> serde_json::Value {
        payload["files"].as_array_mut().unwrap().push(
            serde_json::json!({"path": path, "hash": HASH, "size": 1, "platform": platform, "exec": false}),
        );
        payload
    }

    #[test]
    fn each_identity_accepts_its_own_feed_only() {
        use Identity::*;
        assert!(accepts(&feed_for(Legacy), Legacy));
        assert!(accepts(&feed_for(WorldOfSurf), WorldOfSurf));
        assert!(
            !accepts(&feed_for(WorldOfSurf), Legacy),
            "new feed, legacy install"
        );
        assert!(
            !accepts(&feed_for(Legacy), WorldOfSurf),
            "legacy feed, new install"
        );
    }

    #[test]
    fn mixed_and_outside_feeds_are_refused() {
        use Identity::*;
        let here = crate::manifest::platform_key();
        let mut relabelled = feed_for(WorldOfSurf);
        relabelled["game"] = "ftesurf".into();
        // only the game check refuses these two: the contents fit the install
        let mut legacy_as_new = feed_for(Legacy);
        legacy_as_new["game"] = "worldofsurf".into();
        let mut legacy_exec = feed_for(WorldOfSurf);
        legacy_exec["exec_by_platform"][here]["cmd"] = Legacy.exec().into();
        let mut unknown = feed_for(WorldOfSurf);
        unknown["game"] = "WorldofSurf".into();
        let cases = [
            ("relabelled", relabelled.clone(), Legacy),
            (
                "worldofsurf contents labelled ftesurf",
                relabelled,
                WorldOfSurf,
            ),
            (
                "ftesurf contents labelled worldofsurf",
                legacy_as_new,
                Legacy,
            ),
            (
                "legacy gamedir in a new feed",
                with_file(feed_for(WorldOfSurf), "ftesurf/gfx/x.png", "all"),
                WorldOfSurf,
            ),
            (
                "legacy exe in a new feed",
                with_file(feed_for(WorldOfSurf), Legacy.exec(), here),
                WorldOfSurf,
            ),
            ("legacy exec in a new feed", legacy_exec, WorldOfSurf),
            (
                "new gamedir in a legacy feed",
                with_file(feed_for(Legacy), "worldofsurf/gfx/x.png", "all"),
                Legacy,
            ),
            (
                "outside the gamedir",
                with_file(feed_for(WorldOfSurf), "worldofsurf/data/consent.txt", "all"),
                WorldOfSurf,
            ),
            ("unknown identity", unknown, WorldOfSurf),
        ];
        for (name, payload, id) in cases {
            assert!(!accepts(&payload, id), "{name}");
        }
    }

    #[test]
    fn new_allowlist_is_the_legacy_one_with_the_gamedir_swapped() {
        use Identity::*;
        let long = format!("ftesurf/gfx/{}.png", "a".repeat(216));
        let probes = [
            "ftesurf/menu.dat",
            "ftesurf/maps/tiers.txt",
            "ftesurf/maps/zones/local/a.json",
            "ftesurf/maps/zones/local/x/a.json",
            "ftesurf/cfg/maps/a.cfg",
            "ftesurf/cfg/settings.cfg",
            "ftesurf/cfg/test/a.cfg",
            "ftesurf/data/mapwr.txt",
            "ftesurf/data/consent.txt",
            "ftesurf/gfx/a.png",
            "ftesurf/gfx/../../x",
            "ftesurf/maps/a.bsp",
            "ftesurf/ftesurf.cfg",
            "ftesurf/textures/no_texture.png",
            "ftesurf/fs_addons.txt",
            "ftesurf/MENU.DAT",
            "ftesurf/menu.dat.",
            long.as_str(),
        ];
        for p in probes {
            let swapped = format!("worldofsurf/{}", &p["ftesurf/".len()..]);
            assert_eq!(managed(&swapped, WorldOfSurf), managed(p, Legacy), "{p}");
            assert!(!managed(p, WorldOfSurf), "{p}");
            assert!(!managed(&swapped, Legacy), "{swapped}");
            assert_eq!(
                protected(&swapped, WorldOfSurf),
                protected(p, Legacy),
                "{p}"
            );
            assert!(!protected(p, WorldOfSurf), "{p}");
        }
        for p in [
            "default.fmf",
            "LICENSE",
            "notices/a.txt",
            "ftesurf.sh",
            "fteplug_hl2_x64.dll",
        ] {
            assert!(managed(p, Legacy) && managed(p, WorldOfSurf), "{p}");
        }
        for (old, new) in Legacy.exes().into_iter().zip(WorldOfSurf.exes()) {
            assert!(managed(old, Legacy) && managed(new, WorldOfSurf));
            assert!(!managed(new, Legacy) && !managed(old, WorldOfSurf));
        }
        for p in [
            "worldofsurf",
            "worldofsurf.exe",
            "WORLDOFSURF/menu.dat",
            "WorldofSurf/menu.dat",
            "quakers/menu.dat",
        ] {
            assert!(!managed(p, WorldOfSurf), "{p}");
        }
        // The 240 limit is on the real path: the longer gamedir never buys a longer on-disk path.
        assert!(managed(
            &format!("worldofsurf/gfx/{}.png", "a".repeat(219)),
            WorldOfSurf
        ));
        assert!(!managed(
            &format!("worldofsurf/gfx/{}.png", "a".repeat(220)),
            WorldOfSurf
        ));
        assert!(managed(
            &format!("ftesurf/gfx/{}.png", "a".repeat(220)),
            Legacy
        ));
    }

    struct Root(PathBuf);
    impl Root {
        fn new(name: &str) -> Self {
            let p =
                std::env::temp_dir().join(format!("wos-identity-{name}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&p);
            fs::create_dir_all(&p).unwrap();
            Self(p)
        }
    }
    impl Drop for Root {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).unwrap();
        }
    }

    fn game_files(dir: &Path, leaves: &[&str]) {
        fs::create_dir_all(dir).unwrap();
        for leaf in leaves {
            fs::write(dir.join(leaf), leaf.as_bytes()).unwrap();
        }
    }

    fn dir_link(target: &Path, link: &Path) {
        #[cfg(windows)]
        assert!(std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(link)
            .arg(target)
            .output()
            .unwrap()
            .status
            .success());
        #[cfg(not(windows))]
        std::os::unix::fs::symlink(target, link).unwrap();
    }

    #[test]
    fn install_identity_follows_the_gamedir_on_disk() {
        let r = Root::new("select");
        assert_eq!(install_identity(&r.0).unwrap(), FRESH_INSTALL);
        fs::create_dir(r.0.join("ftesurf")).unwrap();
        assert_eq!(
            install_identity(&r.0).unwrap(),
            FRESH_INSTALL,
            "empty gamedir"
        );
        game_files(&r.0.join("ftesurf"), &GAME_REQUIRED);
        assert_eq!(install_identity(&r.0).unwrap(), Identity::Legacy);
        // stray folders do not lock a legacy install out
        fs::write(r.0.join("worldofsurf"), b"a file at the name").unwrap();
        assert_eq!(
            install_identity(&r.0).unwrap(),
            Identity::Legacy,
            "stray file"
        );
        fs::remove_file(r.0.join("worldofsurf")).unwrap();
        game_files(&r.0.join("worldofsurf"), &["menu.dat"]);
        assert_eq!(
            install_identity(&r.0).unwrap(),
            Identity::Legacy,
            "stray folder, one file"
        );
        game_files(&r.0.join("worldofsurf"), &["csprogs.dat"]);
        assert!(
            install_identity(&r.0).is_err(),
            "half-moved install must not be guessed"
        );
        fs::remove_dir_all(r.0.join("ftesurf")).unwrap();
        assert_eq!(install_identity(&r.0).unwrap(), Identity::WorldOfSurf);
        // paths of the other identity never reach the disk
        assert!(checked_path(&r.0, "ftesurf/menu.dat", Identity::WorldOfSurf).is_err());
        assert!(checked_path(&r.0, "worldofsurf/menu.dat", Identity::Legacy).is_err());
        assert!(checked_path(&r.0, ".ftesurf-updater/state.json", Identity::WorldOfSurf).is_ok());
    }

    #[test]
    fn an_update_interrupted_mid_replacement_keeps_its_identity() {
        // recover() renames the live file aside before the staged one in: one required file absent
        let r = Root::new("midupdate");
        game_files(&r.0.join("worldofsurf"), &["csprogs.dat", "qwprogs.dat"]);
        assert_eq!(install_identity(&r.0).unwrap(), Identity::WorldOfSurf);
    }

    #[test]
    fn linked_gamedirs_and_files_do_not_count() {
        let r = Root::new("links");
        let elsewhere = r.0.join("elsewhere");
        game_files(&elsewhere, &GAME_REQUIRED);
        dir_link(&elsewhere, &r.0.join("worldofsurf"));
        game_files(&r.0.join("ftesurf"), &GAME_REQUIRED);
        assert_eq!(
            install_identity(&r.0).unwrap(),
            Identity::Legacy,
            "linked gamedir"
        );
        fs::remove_dir_all(r.0.join("ftesurf")).unwrap();
        assert_eq!(install_identity(&r.0).unwrap(), FRESH_INSTALL);
    }

    fn strings(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn game_arguments_are_an_allowlist() {
        for ok in [
            &["-window", "+map", "surf_utopia"][..],
            &["+connect", "play.proto.bar:27500", "-fullscreen"],
            &["-width", "1920", "-height", "1080"],
            &[
                "-condebug",
                "+set",
                "log_dir",
                "logs",
                "+set",
                "developer",
                "1",
                "+map",
                "surf_x",
            ],
            &[],
        ] {
            assert!(checked_game_args(&strings(ok)).is_ok(), "{ok:?}");
        }
        for bad in [
            &["-basegame", "ftesurf"][..],
            &["+set", "sensitivity", "2"],
            &["+exec", "x.cfg"],
            &["+map", "surf x"],
            &["+map", "surf\tx"],
            &["+map", "a\"b"],
            &["+map", "-basegame"],
            &["+connect", "+gamedir"],
            &["+map", "../x"],
            &["+map"],
            &["-width", "wide"],
            &["+set", "developer", "2"],
            &["+MAP", "surf_x"],
            &["-window", "+gamedir", "x"],
        ] {
            assert!(checked_game_args(&strings(bad)).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn the_marker_names_the_identity_first() {
        let r = Root::new("marker");
        game_files(&r.0.join("ftesurf"), &GAME_REQUIRED);
        establish_identity(&r.0, Identity::WorldOfSurf).unwrap();
        assert_eq!(fs::read(r.0.join(MARKER)).unwrap(), b"worldofsurf\n");
        assert_eq!(
            install_identity(&r.0).unwrap(),
            Identity::WorldOfSurf,
            "marker over files"
        );
        game_files(&r.0.join("worldofsurf"), &GAME_REQUIRED);
        assert_eq!(
            install_identity(&r.0).unwrap(),
            Identity::WorldOfSurf,
            "marker over half-moved"
        );
        assert!(
            establish_identity(&r.0, Identity::Legacy).is_err(),
            "marker is not rewritten"
        );
        fs::write(r.0.join(MARKER), b"ftesurf").unwrap();
        assert_eq!(
            install_identity(&r.0).unwrap(),
            Identity::Legacy,
            "no trailing newline"
        );
        for bad in [
            &b"WorldofSurf\n"[..],
            b"worldofsurf\r\n",
            b"worldofsurf\n\n",
            b" ftesurf",
            b"",
            b"quakers\n",
        ] {
            fs::write(r.0.join(MARKER), bad).unwrap();
            assert!(install_identity(&r.0).is_err(), "{bad:?}");
        }
        fs::remove_file(r.0.join(MARKER)).unwrap();
        fs::create_dir(r.0.join(MARKER)).unwrap();
        assert!(install_identity(&r.0).is_err(), "a directory is no marker");
        fs::remove_dir(r.0.join(MARKER)).unwrap();
        let elsewhere = r.0.join("elsewhere");
        fs::create_dir(&elsewhere).unwrap();
        dir_link(&elsewhere, &r.0.join(".ftesurf-updater").join("identity")); // mklink wants backslashes
        assert!(install_identity(&r.0).is_err(), "a linked marker");
    }

    #[test]
    fn envelope_shape_matches_the_engine() {
        let text = feed_for(Identity::Legacy).to_string();
        let (envelope, key) = signed_text(&text);
        assert!(decode(&envelope, &key, false, Identity::Legacy).is_ok());
        // an escaped key name, in the payload and in the envelope
        let (escaped, key2) = signed_text(&text.replacen("\"schema\"", "\"sch\\u0065ma\"", 1));
        assert!(
            decode(&escaped, &key2, false, Identity::Legacy).is_err(),
            "escaped payload key"
        );
        let env_escaped = envelope.replacen("\"payload\"", "\"pay\\u006coad\"", 1);
        assert!(
            decode(&env_escaped, &key, false, Identity::Legacy).is_err(),
            "escaped envelope key"
        );
        // the envelope as an array (serde would map it onto the struct)
        let v: serde_json::Value = serde_json::from_str(&envelope).unwrap();
        let array = serde_json::json!([v["payload"], v["signature"]]).to_string();
        assert!(
            decode(&array, &key, false, Identity::Legacy).is_err(),
            "array envelope"
        );
        // the signature in upper case, and one hex digit short
        let sig = v["signature"].as_str().unwrap();
        let upper = envelope.replace(sig, &sig.to_ascii_uppercase());
        assert!(
            decode(&upper, &key, false, Identity::Legacy).is_err(),
            "upper-case signature"
        );
        let short = envelope.replace(sig, &sig[1..]);
        assert!(
            decode(&short, &key, false, Identity::Legacy).is_err(),
            "127-digit signature"
        );
        // the same signature with one digit written as an escape
        let escaped =
            envelope.replace(sig, &format!("\\u00{:02x}{}", sig.as_bytes()[0], &sig[1..]));
        assert!(
            decode(&escaped, &key, false, Identity::Legacy).is_err(),
            "escaped signature"
        );
    }

    #[test]
    fn removed_must_be_present_and_empty() {
        let mut absent = feed_for(Identity::Legacy);
        absent.as_object_mut().unwrap().remove("removed");
        assert!(!accepts(&absent, Identity::Legacy));
        let mut listed = feed_for(Identity::Legacy);
        listed["removed"] = serde_json::json!(["ftesurf/gfx/old.png"]);
        assert!(!accepts(&listed, Identity::Legacy));
    }

    #[test]
    fn dotted_and_extra_envelope_keys_are_refused() {
        let text = feed_for(Identity::Legacy).to_string();
        let (envelope, key) = signed_text(&format!("{{\"game.\":\"worldofsurf\",{}", &text[1..]));
        assert!(
            decode(&envelope, &key, false, Identity::Legacy).is_err(),
            "dotted key"
        );
        let (envelope, key) = signed_text(&text);
        let mut extra: serde_json::Value = serde_json::from_str(&envelope).unwrap();
        extra["note"] = "x".into();
        assert!(
            decode(&extra.to_string(), &key, false, Identity::Legacy).is_err(),
            "extra envelope key"
        );
        assert!(decode(&envelope, &key, false, Identity::Legacy).is_ok());
    }

    #[test]
    fn each_identity_has_its_own_feed() {
        assert_eq!(Identity::Legacy.feed_url(), FEED_URL);
        assert_eq!(
            Identity::WorldOfSurf.feed_url(),
            "https://proto.bar/ftesurf/worldofsurf.json"
        );
    }

    #[test]
    fn repeated_keys_are_refused() {
        // serde keeps the last "game": a legacy feed that also says worldofsurf first
        let text = feed_for(Identity::Legacy).to_string();
        let doubled = format!("{{\"game\":\"worldofsurf\",{}", &text[1..]);
        let (envelope, key) = signed_text(&doubled);
        assert!(decode(&envelope, &key, false, Identity::Legacy).is_err());
        let nested = text.replacen("\"path\":", "\"path\":\"ftesurf/gfx/x.png\",\"path\":", 1);
        let (envelope, key) = signed_text(&nested);
        assert!(decode(&envelope, &key, false, Identity::Legacy).is_err());
    }

    #[test]
    fn delete_byte_is_refused_in_paths() {
        assert!(!managed("ftesurf/gfx/a\x7f.png", Identity::Legacy));
        assert!(!managed("worldofsurf/gfx/a\x7f.png", Identity::WorldOfSurf));
        assert!(managed("ftesurf/gfx/a~.png", Identity::Legacy));
    }
}
