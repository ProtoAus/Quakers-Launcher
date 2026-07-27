//! Optional launcher.toml (next to the exe or in the install dir). Everything here
//! is overridable on the command line; the manifest also carries a mirror list.

use serde::Deserialize;
use std::path::Path;

#[derive(Debug, Default, Deserialize)]
pub struct Config {
    /// Mirror base URLs, tried before the manifest's own list.
    #[serde(default)]
    pub mirrors: Vec<String>,
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
