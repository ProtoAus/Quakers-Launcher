//! Optional `launcher.toml`, looked for next to the exe and then in the install dir.
//!
//! Optional is the operative word: the launcher ships as a **single file**. Every setting here
//! has a compiled-in default (see `DEFAULT_MIRRORS` and the clap defaults in `main.rs`), so a
//! bare `quakers-launcher.exe` dropped in an empty folder installs the game with no companion
//! file. The toml exists to repoint a tester at a test mirror, or to pin a channel, without
//! handing them a new build.
//!
//! Precedence, highest first: **CLI flag > launcher.toml > compiled-in default**. The manifest's
//! own `mirrors` list is consulted after all of these, in `main.rs`.

use serde::Deserialize;
use std::path::Path;

#[derive(Debug, Default, Deserialize)]
pub struct Config {
    /// Mirror base URLs, tried before the manifest's own list.
    #[serde(default)]
    pub mirrors: Vec<String>,

    /// Release channel, selecting `manifests/<channel>.json`.
    #[serde(default)]
    pub channel: Option<String>,

    /// Where to install. Relative paths resolve against the current directory, so the usual
    /// value is "." — alongside the launcher.
    #[serde(default)]
    pub install_dir: Option<String>,

    /// Parallel download workers.
    #[serde(default)]
    pub concurrency: Option<usize>,
}

pub fn load(install_dir: &Path) -> Config {
    let candidates = [
        std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|d| d.join("launcher.toml"))),
        Some(install_dir.join("launcher.toml")),
    ];
    for c in candidates.into_iter().flatten() {
        if let Ok(s) = std::fs::read_to_string(&c) {
            if let Ok(cfg) = toml::from_str::<Config>(&s) {
                return cfg;
            }
        }
    }
    Config::default()
}

/// Read `install_dir` before the main config load, because it decides *where* the second
/// candidate config path even is. Only the exe-adjacent file can set it — a value inside the
/// install dir's own toml would be circular.
pub fn install_dir_override() -> Option<String> {
    let p = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("launcher.toml")))?;
    let s = std::fs::read_to_string(p).ok()?;
    toml::from_str::<Config>(&s).ok()?.install_dir
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A missing file must be a silent default, not an error: that is what makes the launcher a
    /// single self-contained binary.
    #[test]
    fn absent_config_is_all_defaults() {
        let cfg = load(Path::new("\u{0}definitely-not-a-real-directory"));
        assert!(cfg.mirrors.is_empty());
        assert!(cfg.channel.is_none());
        assert!(cfg.install_dir.is_none());
        assert!(cfg.concurrency.is_none());
    }

    /// Every key the shipped sample toml documents must actually deserialize. These were
    /// documented but absent from the struct, so serde silently dropped them.
    #[test]
    fn every_documented_key_is_honoured() {
        let cfg: Config = toml::from_str(
            r#"
            channel     = "beta"
            install_dir = "."
            concurrency = 4
            mirrors     = ["https://example.invalid"]
            "#,
        )
        .unwrap();
        assert_eq!(cfg.channel.as_deref(), Some("beta"));
        assert_eq!(cfg.install_dir.as_deref(), Some("."));
        assert_eq!(cfg.concurrency, Some(4));
        assert_eq!(cfg.mirrors, vec!["https://example.invalid"]);
    }

    /// Unknown keys must not fail the whole file, or a newer toml would brick an older launcher.
    #[test]
    fn unknown_keys_are_ignored() {
        let cfg: Config = toml::from_str("mirrors = []\nfuture_option = 3\n").unwrap();
        assert!(cfg.mirrors.is_empty());
    }
}
