//! quakers launcher — terminal front-end.
//!
//! Default run: fetch the manifest, diff against the local install, download whatever
//! is missing/changed (resumable, parallel, hash-verified), then launch the game.

use anyhow::{anyhow, Context, Result};
use clap::Parser;
use quakers_launcher::{config, download, manifest, plan, state, ui};
use ratatui::crossterm::style::Stylize;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Baked-in fallback so a lone launcher.exe works with no launcher.toml / CLI args.
const DEFAULT_MIRRORS: &[&str] = &["https://dl.proto.bar"];

#[derive(Parser, Debug)]
#[command(name = "quakers-launcher", version, about = "Download, verify, and launch the Quakers game")]
struct Args {
    /// Install directory (defaults to the launcher's own folder).
    #[arg(long)]
    install_dir: Option<PathBuf>,

    /// Release channel.
    #[arg(long, default_value = "beta")]
    channel: String,

    /// Override mirror base URL(s); repeatable. Falls back to launcher.toml, then the manifest.
    #[arg(long = "mirror")]
    mirrors: Vec<String>,

    /// Full manifest URL override (handy for local testing).
    #[arg(long)]
    manifest_url: Option<String>,

    /// Parallel download workers.
    #[arg(long, default_value_t = 8)]
    concurrency: usize,

    /// Hash every file (full integrity check) and repair any mismatch.
    #[arg(long)]
    verify: bool,

    /// Show what would be downloaded, then exit.
    #[arg(long)]
    dry_run: bool,

    /// Sync but do not launch the game.
    #[arg(long)]
    no_launch: bool,

    /// Skip the update check and just launch the game.
    #[arg(long)]
    launch_only: bool,

    /// Render a sample progress frame and exit (preview the UI layout).
    #[arg(long, hide = true)]
    ui_preview: bool,

    /// Install straight into the current folder even if it's a Desktop/Downloads/drive root.
    #[arg(long)]
    allow_unsafe_dir: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    // Enable ANSI/VT processing on Windows so colours + box-drawing render on any console.
    // crossterm gates this module to #[cfg(windows)]; unix terminals need no such nudge.
    #[cfg(windows)]
    let _ = ratatui::crossterm::ansi_support::supports_ansi();
    if args.ui_preview {
        print!("{}", ui::preview());
        return Ok(());
    }
    if let Err(e) = run(args).await {
        eprintln!("\n{} {e:#}", "error:".red().bold());
        pause_on_exit();
        std::process::exit(1);
    }
    Ok(())
}

/// Keep a double-clicked console window open so the user can read output/errors.
/// No-op when stdin isn't a console (piped/CI): read_line returns immediately on EOF.
fn pause_on_exit() {
    use std::io::Write;
    eprint!("\nPress Enter to close this window...");
    let _ = std::io::stderr().flush();
    let mut s = String::new();
    let _ = std::io::stdin().read_line(&mut s);
}

async fn run(args: Args) -> Result<()> {
    print_banner();
    let requested = args.install_dir.clone().unwrap_or_else(default_install_dir);
    let (install_dir, redirected) = resolve_install_dir(requested, args.allow_unsafe_dir);
    if let Some(why) = &redirected {
        println!();
        println!(
            "  {} This looks like {}, not a place to unpack a 6 GB game.",
            "!".yellow().bold(),
            why.clone().yellow()
        );
        println!("    Installing into {} instead.", fwd(&install_dir).cyan().bold());
        println!(
            "    {}",
            "(move the launcher in there too, or pass --allow-unsafe-dir to override)".dark_grey()
        );
    }
    std::fs::create_dir_all(&install_dir).ok();

    if args.launch_only {
        println!();
        kv("Install", &fwd(&install_dir));
        return launch_game(
            &install_dir,
            manifest::default_exec_cmd(),
            &["-manifest".to_string(), "default.fmf".to_string()],
        );
    }

    let cfg = config::load(&install_dir);
    let client = reqwest::Client::builder()
        .user_agent(concat!("quakers-launcher/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(20))
        .build()?;

    // ---- fetch manifest (CLI url > mirror bases from CLI/toml > baked-in default) ----
    let mut bases: Vec<String> = Vec::new();
    bases.extend(args.mirrors.clone());
    bases.extend(cfg.mirrors.clone());
    if bases.is_empty() {
        bases.extend(DEFAULT_MIRRORS.iter().map(|s| s.to_string()));
    }

    let mut source = String::new();
    let manifest = if let Some(u) = &args.manifest_url {
        source = u.clone();
        manifest::fetch_manifest(&client, u)
            .await
            .with_context(|| format!("fetch {u}"))?
    } else {
        let mut found = None;
        let mut errs = Vec::new();
        for b in &bases {
            let u = format!("{}/manifests/{}.json", b.trim_end_matches('/'), args.channel);
            match manifest::fetch_manifest(&client, &u).await {
                Ok(m) => {
                    source = u;
                    found = Some(m);
                    break;
                }
                Err(e) => errs.push(format!("  {u}: {e}")),
            }
        }
        found.ok_or_else(|| anyhow!("could not fetch manifest from any mirror:\n{}", errs.join("\n")))?
    };

    // ---- resolve the download mirror list: CLI > toml > manifest ----
    let mut mirrors: Vec<String> = Vec::new();
    mirrors.extend(args.mirrors.clone());
    mirrors.extend(cfg.mirrors.clone());
    mirrors.extend(manifest.mirrors.clone());
    if mirrors.is_empty() {
        mirrors.extend(DEFAULT_MIRRORS.iter().map(|s| s.to_string()));
    }
    mirrors.dedup();

    println!();
    kv("Install", &fwd(&install_dir));
    kv("System", &ui::system_description());
    kv("Mirror", &source);
    kv("Channel", &format!("{} · {}", manifest.channel, manifest.version));
    // Count only what THIS platform installs — a manifest describes every platform, so the
    // manifest-level totals would over-report by the size of the other platforms' engines.
    let (my_files, my_bytes) = manifest
        .files_for_platform()
        .fold((0u64, 0u64), |(n, b), f| (n + 1, b + f.size));
    kv("Platform", manifest::platform_key());
    kv("Content", &format!("{} files · {:.2} GB", my_files, my_bytes as f64 / 1e9));
    println!();

    // ---- plan ----
    let st = state::load(&install_dir);
    let checking = if args.verify {
        "Verifying every file (full hash check)…"
    } else {
        "Checking local files…"
    };
    println!("  {}", checking.yellow());
    let the_plan = plan::build(&manifest, &install_dir, &st, args.verify).await?;

    if args.dry_run {
        kv("Up to date", &the_plan.up_to_date.to_string());
        kv(
            "To fetch",
            &format!("{} files · {:.2} GB", the_plan.to_download.len(), the_plan.bytes_to_download as f64 / 1e9),
        );
        for e in the_plan.to_download.iter().take(40) {
            println!("    {} {}", "·".dark_grey(), format!("{} ({:.1} MB)", e.path, e.size as f64 / 1e6).grey());
        }
        if the_plan.to_download.len() > 40 {
            println!("    {}", format!("… and {} more", the_plan.to_download.len() - 40).dark_grey());
        }
        return Ok(());
    }

    // ---- download ----
    if !the_plan.to_download.is_empty() {
        kv("Up to date", &the_plan.up_to_date.to_string());
        kv(
            "To fetch",
            &format!("{} files · {:.2} GB", the_plan.to_download.len(), the_plan.bytes_to_download as f64 / 1e9),
        );
        println!();
        let outcome = download::download_all(
            client.clone(),
            mirrors.clone(),
            manifest.hash_algo.clone(),
            install_dir.clone(),
            the_plan.to_download.clone(),
            the_plan.bytes_to_download,
            args.concurrency,
            ui::Header {
                system: ui::system_description(),
                platform: manifest::platform_key().to_string(),
                release: format!("{} · {}", manifest.channel, manifest.version),
                mirror: host_of(&source),
            },
        )
        .await?;
        if outcome.failed.is_empty() {
            // The live view runs on the alternate screen, so it is gone by now — leave a
            // persistent record of what was actually fetched.
            print_breakdown(&outcome.categories);
            println!("  {}", format!("✓ Downloaded {} file(s).", outcome.ok).green().bold());
        } else {
            eprintln!("  {}", format!("{} file(s) FAILED:", outcome.failed.len()).red().bold());
            for (p, e) in outcome.failed.iter().take(20) {
                eprintln!("    {} {p}: {e}", "✗".red());
            }
            return Err(anyhow!(
                "{} download(s) failed — re-run to resume/repair",
                outcome.failed.len()
            ));
        }
    } else {
        println!("  {}", "✓ Everything is up to date.".green().bold());
    }

    // ---- record state (downloaded files verified; trusted files assumed good) ----
    let mut new_state = state::State {
        version: manifest.version.clone(),
        files: st.files.clone(),
    };
    for e in manifest.files_for_platform() {
        new_state.files.insert(e.path.clone(), e.hash.clone());
    }
    if let Err(e) = state::save(&install_dir, &new_state) {
        eprintln!("warning: could not write install state: {e}");
    }

    // Repair +x on anything already present but not runnable (unix only).
    plan::ensure_exec_bits(&manifest, &install_dir);

    if !args.no_launch {
        println!();
        let exec = manifest.exec_for_platform();
        launch_game(&install_dir, &exec.cmd, &exec.args)?;
    }
    Ok(())
}

fn launch_game(install_dir: &Path, cmd: &str, args: &[String]) -> Result<()> {
    let exe = install_dir.join(cmd);
    if !exe.exists() {
        return Err(anyhow!("game executable not found: {}", fwd(&exe)));
    }
    // Last-resort +x: --launch-only never reads a manifest, so nothing else has had the
    // chance to fix permissions on this path.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(md) = std::fs::metadata(&exe) {
            let mode = md.permissions().mode();
            if mode & 0o111 == 0 {
                let mut perm = md.permissions();
                perm.set_mode(mode | 0o755);
                let _ = std::fs::set_permissions(&exe, perm);
            }
        }
    }
    println!("  {} {} {}", "▶ Launching".green().bold(), cmd.cyan(), args.join(" ").dark_grey());
    std::process::Command::new(&exe)
        .args(args)
        .current_dir(install_dir)
        .spawn()
        .with_context(|| format!("spawn {}", fwd(&exe)))?;
    Ok(())
}

/// Subfolder created when the launcher is run from somewhere it shouldn't install into.
const INSTALL_FOLDER_NAME: &str = "Quakers";

/// Folder names that are containers for *other* things, never a home for an install.
/// Matched on the basename, so OneDrive/Dropbox-redirected Desktops are caught too.
const CONTAINER_NAMES: &[&str] = &[
    "desktop", "downloads", "download", "documents", "pictures", "music", "videos",
    "public", "onedrive", "dropbox", "google drive", "icloud drive",
];

/// Why `dir` is a bad place to unpack ~5,000 files, or None if it's fine.
///
/// The failure this prevents: someone drops the launcher on their Desktop and double-clicks
/// it. The install dir defaults to the launcher's own folder, so the entire game — a
/// `quakers/` tree plus the engine binaries — lands loose on the Desktop, interleaved with
/// everything else they own and effectively impossible to tidy up afterwards. A drive root
/// is worse still.
///
/// `home` is passed in rather than read from the environment so this is testable.
fn container_reason_in(dir: &Path, home: Option<&Path>) -> Option<String> {
    if dir.parent().is_none() {
        return Some("a drive root".to_string());
    }
    // On Windows a drive root can also appear as "C:\" with a trailing component.
    if dir.components().count() <= 1 {
        return Some("a drive root".to_string());
    }
    if let Some(h) = home {
        if dir == h {
            return Some("your home folder".to_string());
        }
    }
    let base = dir
        .file_name()
        .map(|s| s.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    if CONTAINER_NAMES.contains(&base.as_str()) {
        return Some(format!("your {} folder", base.trim_end_matches('s')));
    }
    // Windows system locations
    let lower = dir.to_string_lossy().to_lowercase().replace('\\', "/");
    for sys in ["/windows", "/program files", "/program files (x86)", "/programdata"] {
        if lower.contains(sys) {
            return Some("a system folder".to_string());
        }
    }
    None
}

fn container_reason(dir: &Path) -> Option<String> {
    let home = std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from);
    container_reason_in(dir, home.as_deref())
}

/// Apply the guard: if `requested` is a container folder, install into a subfolder of it.
fn resolve_install_dir(requested: PathBuf, allow_unsafe: bool) -> (PathBuf, Option<String>) {
    if allow_unsafe {
        return (requested, None);
    }
    match container_reason(&requested) {
        Some(why) => {
            let sub = requested.join(INSTALL_FOLDER_NAME);
            (sub, Some(why))
        }
        None => (requested, None),
    }
}

fn default_install_dir() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Per-content-type recap printed after the live view has torn down.
fn print_breakdown(cats: &[(&'static str, u64, u64)]) {
    if cats.is_empty() {
        return;
    }
    println!();
    for (name, done, total) in cats {
        let pct = if *total > 0 { done * 100 / total } else { 100 };
        let bar_w = 24usize;
        let filled = (bar_w as u64 * pct / 100) as usize;
        let bar = format!("{}{}", "━".repeat(filled), "─".repeat(bar_w - filled));
        println!(
            "  {} {} {} {}",
            format!("{:<11}", name).green().bold(),
            bar.magenta(),
            format!("{:>3}%", pct).cyan().bold(),
            format!("{:>10} / {:>10}", human(*done), human(*total)).dark_grey(),
        );
    }
    println!();
}

fn human(b: u64) -> String {
    const U: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    if b < 1024 {
        return format!("{} B", b);
    }
    let (mut v, mut i) = (b as f64, 0);
    while v >= 1024.0 && i < 4 {
        v /= 1024.0;
        i += 1;
    }
    format!("{:.1} {}", v, U[i])
}

/// "https://dl.proto.bar/manifests/beta.json" -> "dl.proto.bar", so the header shows
/// which mirror answered without wrapping the whole URL.
fn host_of(url: &str) -> String {
    url.split("://")
        .nth(1)
        .unwrap_or(url)
        .split('/')
        .next()
        .unwrap_or(url)
        .to_string()
}

/// Path with forward slashes (no back/forward-slash mixing in the UI).
fn fwd(p: &Path) -> String {
    p.display().to_string().replace('\\', "/")
}

fn kv(label: &str, value: &str) {
    let l = format!("{:<11}", label);
    println!("  {} {}", l.green().bold(), value.cyan());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn home() -> PathBuf {
        PathBuf::from(if cfg!(windows) { r"C:\Users\Lex" } else { "/home/lex" })
    }
    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    #[test]
    fn rejects_container_folders() {
        let h = home();
        let bad = if cfg!(windows) {
            vec![
                r"C:\Users\Lex\Desktop",
                r"C:\Users\Lex\Downloads",
                r"C:\Users\Lex\Documents",
                r"C:\Users\Lex\Pictures",
                r"C:\Users\Lex\OneDrive\Desktop",   // redirected Desktop
                r"C:\Users\Lex\OneDrive",
                r"C:\Users\Lex",                    // the home folder itself
                r"C:\",                             // drive root
                r"C:\Program Files\Quakers",        // system location
                r"C:\Windows\Temp",
            ]
        } else {
            vec![
                "/home/lex/Desktop",
                "/home/lex/Downloads",
                "/home/lex/Documents",
                "/home/lex",
                "/",
            ]
        };
        for b in bad {
            assert!(
                container_reason_in(&p(b), Some(&h)).is_some(),
                "should have been rejected: {b}"
            );
        }
    }

    #[test]
    fn accepts_real_install_dirs() {
        let h = home();
        let good = if cfg!(windows) {
            vec![
                r"C:\Users\Lex\Desktop\Quakers",  // the redirect target itself
                r"C:\Games\Quakers",
                r"C:\Users\Lex\quakerstest",
                r"D:\Quakers",
            ]
        } else {
            vec!["/home/lex/Desktop/Quakers", "/home/lex/games/quakers", "/opt/quakers"]
        };
        for g in good {
            assert!(
                container_reason_in(&p(g), Some(&h)).is_none(),
                "should have been accepted: {g}"
            );
        }
    }

    #[test]
    fn redirect_creates_one_level_and_is_stable() {
        let h = home();
        let desktop = if cfg!(windows) { r"C:\Users\Lex\Desktop" } else { "/home/lex/Desktop" };
        let (dir, why) = resolve_install_dir(p(desktop), false);
        assert!(why.is_some());
        assert_eq!(dir.file_name().unwrap(), INSTALL_FOLDER_NAME);
        // and the redirect target must not itself redirect -- no Quakers/Quakers nesting
        assert!(container_reason_in(&dir, Some(&h)).is_none());
        let (again, why2) = resolve_install_dir(dir.clone(), false);
        assert_eq!(again, dir);
        assert!(why2.is_none());
    }

    #[test]
    fn override_flag_disables_the_guard() {
        let desktop = if cfg!(windows) { r"C:\Users\Lex\Desktop" } else { "/home/lex/Desktop" };
        let (dir, why) = resolve_install_dir(p(desktop), true);
        assert_eq!(dir, p(desktop));
        assert!(why.is_none());
    }
}

fn print_banner() {
    let w = 74usize;
    let bar = "─".repeat(w - 2);
    println!("{}", format!("╭{}╮", bar).cyan());
    banner_row(w, "QUAKERS  ·  The Religious Society of Friends", true);
    banner_row(w, "ALPHA Download Launcher", false);
    println!("{}", format!("╰{}╯", bar).cyan());
}

fn banner_row(w: usize, text: &str, title: bool) {
    let content = format!("  {}", text);
    let pad = (w - 2).saturating_sub(content.chars().count());
    let inner = format!("{}{}", content, " ".repeat(pad));
    let styled = if title {
        inner.white().bold()
    } else {
        inner.dark_grey().bold()
    };
    println!("{}{}{}", "│".cyan(), styled, "│".cyan());
}
