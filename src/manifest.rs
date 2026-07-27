//! The release manifest: what `publish.py` emits and the launcher consumes.
//!
//! Content-addressed: every file is stored on a mirror at `objects/<hh>/<hash>`
//! (hh = first two hex chars of the hash). A patch changes only a few objects.

use serde::Deserialize;
use std::collections::HashMap;

/// Which per-platform slice of the manifest this build wants.
///
/// The bulk of a release (~6.3 GB of maps/textures/models/progs) is identical on every
/// platform and is tagged `"all"`; only the ~15 MB engine binary set differs. One manifest
/// therefore describes every platform, and the content-addressed object tree dedupes the
/// shared blobs automatically.
pub const fn platform_key() -> &'static str {
    if cfg!(target_os = "windows") {
        "win64"
    } else if cfg!(target_os = "linux") {
        "linux64"
    } else if cfg!(target_os = "macos") {
        "macos64"
    } else {
        "unknown"
    }
}

/// Fallback game binary when we launch without consulting a manifest (`--launch-only`).
pub const fn default_exec_cmd() -> &'static str {
    if cfg!(target_os = "windows") {
        "fteqw64.exe"
    } else {
        "fteqw-gl64"
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Manifest {
    pub schema: u32,
    pub channel: String,
    pub version: String,
    #[serde(default)]
    pub created_utc: String,
    /// "blake2b-256" or "blake3" — the launcher hashes with whatever this says.
    pub hash_algo: String,
    #[serde(default)]
    pub launcher_version: String,
    #[serde(default)]
    pub install_root_name: String,
    /// Schema-1 field: the Windows launch command. Still emitted so an older launcher
    /// keeps working; `exec_by_platform` wins when it has an entry for us.
    pub exec: Exec,
    /// Schema-2: launch command per platform key ("win64" / "linux64" / ...).
    #[serde(default)]
    pub exec_by_platform: HashMap<String, Exec>,
    #[serde(default)]
    pub mirrors: Vec<String>,
    #[serde(default = "default_layout")]
    pub object_layout: String,
    #[serde(default)]
    pub total_files: u64,
    #[serde(default)]
    pub total_bytes: u64,
    pub files: Vec<FileEntry>,
}

fn default_layout() -> String {
    "objects/{h0h1}/{hash}".to_string()
}

#[derive(Debug, Clone, Deserialize)]
pub struct Exec {
    pub cmd: String,
    #[serde(default)]
    pub args: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct FileEntry {
    /// Install-root-relative path, forward slashes (e.g. "quakers/csprogs.dat").
    pub path: String,
    pub size: u64,
    pub hash: String,
    #[serde(default)]
    pub component: String,
    /// "all" (shared game content) or a platform key like "win64" / "linux64".
    /// Absent in schema-1 manifests, where every file was implicitly for everyone.
    #[serde(default = "default_platform")]
    pub platform: String,
    /// Needs the executable bit on unix (the engine binaries). Ignored on Windows.
    #[serde(default)]
    pub exec: bool,
}

fn default_platform() -> String {
    "all".to_string()
}

impl FileEntry {
    /// Should this build download the file at all?
    pub fn wanted(&self) -> bool {
        self.platform == "all" || self.platform == platform_key()
    }
}

impl FileEntry {
    /// Mirror-relative object path, e.g. "objects/37/373c05...".
    pub fn object_rel(&self) -> String {
        let hh = if self.hash.len() >= 2 { &self.hash[..2] } else { "00" };
        format!("objects/{}/{}", hh, self.hash)
    }
}

impl Manifest {
    /// Only the entries this platform installs.
    pub fn files_for_platform(&self) -> impl Iterator<Item = &FileEntry> {
        self.files.iter().filter(|f| f.wanted())
    }

    /// The launch command for this platform, falling back to the schema-1 `exec`.
    pub fn exec_for_platform(&self) -> &Exec {
        self.exec_by_platform
            .get(platform_key())
            .unwrap_or(&self.exec)
    }
}

/// GET and parse a manifest from a full URL.
pub async fn fetch_manifest(client: &reqwest::Client, url: &str) -> anyhow::Result<Manifest> {
    let resp = client.get(url).send().await?.error_for_status()?;
    let text = resp.text().await?;
    let m: Manifest = serde_json::from_str(&text)?;
    Ok(m)
}
