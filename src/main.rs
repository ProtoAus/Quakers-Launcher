//! quakers launcher — terminal front-end.
//!
//! Default run: fetch the manifest, diff against the local install, download whatever
//! is missing/changed (resumable, parallel, hash-verified), then launch the game.

use anyhow::{anyhow, Context, Result};
use clap::Parser;
use quakers_launcher::{confirm, config, download, manifest, plan, state, ui};
use ratatui::crossterm::style::Stylize;
use std::path::{Path, PathBuf};
use std::time::Duration;

// The launcher ships as ONE file. Everything below is a compiled-in default so that a bare
// quakers-launcher.exe, dropped into an empty folder, installs and launches the game with no
// companion launcher.toml. That file remains supported as an optional override -- see config.rs
// for the precedence rule (CLI > launcher.toml > these).
/// Baked-in fallback so a lone launcher.exe works with no launcher.toml / CLI args.
const DEFAULT_MIRRORS: &[&str] = &["https://dl.proto.bar"];
/// Player-facing: printed verbatim in the header as "Release <channel> · <version>".
/// Keep in step with publish.py's --channel default.
const DEFAULT_CHANNEL: &str = "alpha";
const DEFAULT_CONCURRENCY: usize = 8;

#[derive(Parser, Debug)]
#[command(name = "quakers-launcher", version, about = "Download, verify, and launch the Quakers game")]
struct Args {
    /// Install directory (defaults to the launcher's own folder).
    #[arg(long)]
    install_dir: Option<PathBuf>,

    /// Release channel [default: alpha]. Shown verbatim in the launcher header, so it is
    /// player-facing. Option, not a clap default, so launcher.toml can sit between the flag
    /// and the compiled-in DEFAULT_CHANNEL.
    #[arg(long)]
    channel: Option<String>,

    /// Override mirror base URL(s); repeatable. Falls back to launcher.toml, then the manifest.
    #[arg(long = "mirror")]
    mirrors: Vec<String>,

    /// Full manifest URL override (handy for local testing).
    #[arg(long)]
    manifest_url: Option<String>,

    /// Parallel download workers [default: 8].
    #[arg(long)]
    concurrency: Option<usize>,

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

    /// Skip the "Download?" confirmation and start immediately (for scripts/CI).
    #[arg(long, short = 'y')]
    yes: bool,
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
    // install_dir has to be resolved before the main config load, because it decides where the
    // second candidate launcher.toml lives -- so only the exe-adjacent file can set it.
    let requested = args
        .install_dir
        .clone()
        .or_else(|| config::install_dir_override().map(PathBuf::from))
        .unwrap_or_else(default_install_dir);
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
    // CLI > launcher.toml > compiled-in default, for each setting independently.
    let channel = args
        .channel
        .clone()
        .or_else(|| cfg.channel.clone())
        .unwrap_or_else(|| DEFAULT_CHANNEL.to_string());
    let concurrency = args
        .concurrency
        .or(cfg.concurrency)
        .unwrap_or(DEFAULT_CONCURRENCY)
        .max(1);

    let client = reqwest::Client::builder()
        .user_agent(concat!("quakers-launcher/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(20))
        // Per-read inactivity timeout, NOT a whole-request timeout: a 450 MB pk3 on a slow
        // line may legitimately take many minutes, so a total deadline would kill healthy
        // downloads. This only fires when the stream stalls outright — without it a hung
        // connection parks one of the 8 workers indefinitely and the run never finishes.
        // A stall now surfaces as a retryable error and the file resumes from its .part.
        .read_timeout(Duration::from_secs(60))
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
            let u = format!("{}/manifests/{}.json", b.trim_end_matches('/'), channel);
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

        // Ask before writing gigabytes. A fresh install and a resume are different
        // decisions, so they get different wording. Enter is what opens the progress view.
        let prompt = if the_plan.up_to_date == 0 {
            confirm::Prompt::Fresh {
                dir: fwd(&install_dir),
                files: the_plan.to_download.len(),
                bytes: the_plan.bytes_to_download,
            }
        } else {
            confirm::Prompt::Resume {
                dir: fwd(&install_dir),
                files: the_plan.to_download.len(),
                bytes: the_plan.bytes_to_download,
                have: the_plan.up_to_date as usize,
            }
        };
        if !confirm::ask(&prompt, args.yes) {
            println!("  {}", "Cancelled — nothing was downloaded.".yellow());
            return Ok(());
        }

        println!();
        let outcome = download::download_all(
            client.clone(),
            mirrors.clone(),
            manifest.hash_algo.clone(),
            install_dir.clone(),
            the_plan.to_download.clone(),
            the_plan.bytes_to_download,
            concurrency,
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

    // ---- delete anything the manifest retired ----
    // Only after a clean sync: pruning while downloads are failing could leave the install
    // missing both the old file and its replacement.
    let pruned = prune_removed(&manifest, &install_dir);
    if pruned > 0 {
        println!("  {}", format!("✓ Removed {pruned} obsolete file(s).").green());
    }

    // ---- record state (downloaded files verified; trusted files assumed good) ----
    let mut new_state = state::State {
        version: manifest.version.clone(),
        files: st.files.clone(),
    };
    for e in manifest.files_for_platform() {
        new_state.files.insert(e.path.clone(), e.hash.clone());
    }
    // Drop retired paths from the ledger too, or every future run would think they are still
    // installed and the entry would outlive the file.
    for p in &manifest.removed {
        new_state.files.remove(p);
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

/// Delete the manifest's retired paths, plus any directories they empty out.
///
/// `path` here is attacker-influenced JSON, so it is validated rather than trusted: relative
/// only, forward slashes only, no `..`, no drive letters. A rejected entry is skipped silently —
/// a malformed manifest must not be able to reach outside the install root, and must not be able
/// to abort an otherwise successful install either.
fn prune_removed(manifest: &manifest::Manifest, install_dir: &Path) -> usize {
    let mut n = 0;
    for rel in &manifest.removed {
        if !is_safe_relpath(rel) {
            eprintln!("warning: ignoring unsafe \"removed\" entry: {rel}");
            continue;
        }
        let p = install_dir.join(rel.replace('/', std::path::MAIN_SEPARATOR_STR));
        if p.is_file() && std::fs::remove_file(&p).is_ok() {
            n += 1;
            // Tidy up directories the deletion emptied, but never climb out of install_dir.
            let mut dir = p.parent().map(|d| d.to_path_buf());
            while let Some(d) = dir {
                if d == install_dir || !d.starts_with(install_dir) {
                    break;
                }
                if std::fs::remove_dir(&d).is_err() {
                    break; // not empty, or in use — fine, stop climbing
                }
                dir = d.parent().map(|x| x.to_path_buf());
            }
        }
    }
    n
}

fn is_safe_relpath(p: &str) -> bool {
    !p.is_empty()
        && !p.starts_with('/')
        && !p.contains('\\')
        && !p.contains(':')
        && p.split('/').all(|seg| !seg.is_empty() && seg != "." && seg != "..")
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

/// Resolve a relative install dir against the current directory.
///
/// The guard below reasons about path *shape* — component count, basename, whether there is a
/// parent — and none of that means anything for a relative path. `Path::new(".")` has exactly
/// one component, so an unresolved "." reads as a drive root and gets redirected into
/// `./Quakers`. That is not hypothetical: `install_dir = "."` is what every launcher.toml
/// shipped before 0.1.6 says, and until 0.1.5 the key was silently ignored, so the guard had
/// only ever been handed absolute paths.
fn absolutise(p: PathBuf) -> PathBuf {
    let abs = if p.is_absolute() {
        p
    } else {
        match std::env::current_dir() {
            Ok(cwd) => cwd.join(&p),
            Err(_) => p,
        }
    };
    // Re-collecting through Components drops interior/trailing "." segments, so joining an
    // absolute cwd with "." gives "C:/games/quakers" rather than "C:/games/quakers/." — which
    // would otherwise show up in the banner and in every path we print.
    let cleaned: PathBuf = abs.components().collect();
    if cleaned.as_os_str().is_empty() { abs } else { cleaned }
}

/// Apply the guard: if `requested` is a container folder, install into a subfolder of it.
fn resolve_install_dir(requested: PathBuf, allow_unsafe: bool) -> (PathBuf, Option<String>) {
    let requested = absolutise(requested);
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

/// "https://dl.proto.bar/manifests/alpha.json" -> "dl.proto.bar", so the header shows
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

    /// `install_dir = "."` is what every launcher.toml shipped before 0.1.6 contains, so an
    /// existing tester who drops a new exe next to their old toml goes down this path. It must
    /// mean "install right here", not "you are at a drive root, have a subfolder" -- the latter
    /// silently re-downloads 6.3 GB into <install>/Quakers.
    #[test]
    fn relative_install_dir_is_resolved_before_the_guard() {
        let cwd = std::env::current_dir().unwrap();
        for rel in [".", "./"] {
            let (dir, why) = resolve_install_dir(PathBuf::from(rel), false);
            assert!(why.is_none(), "{rel} should not be flagged as a container: {why:?}");
            assert!(dir.is_absolute(), "{rel} should resolve to an absolute path, got {dir:?}");
            assert!(
                !dir.ends_with(INSTALL_FOLDER_NAME) || cwd.ends_with(INSTALL_FOLDER_NAME),
                "{rel} was redirected into a subfolder: {dir:?}"
            );
            assert_eq!(dir, cwd, "{rel} should resolve to exactly the current directory");
            assert!(
                !dir.to_string_lossy().contains("/.") && !dir.to_string_lossy().contains("\\."),
                "resolved path kept a \".\" segment: {dir:?}"
            );
        }
    }

    /// ...but resolving must not defeat the guard: a relative path that lands somewhere silly
    /// still has to be caught.
    /// `removed` comes from JSON, so it is an untrusted path list pointed at the user's disk.
    #[test]
    fn removed_paths_are_validated() {
        for good in [
            "quakers/data/default.cfg",
            "quakers/default.cfg",
            "a",
            "a/b/c.txt",
        ] {
            assert!(is_safe_relpath(good), "should be accepted: {good}");
        }
        for bad in [
            "",
            "/etc/passwd",
            "../outside.txt",
            "quakers/../../outside.txt",
            "quakers/./x",
            "C:/Windows/system32/x.dll",
            r"quakers\data\x.cfg",
            "quakers//x",
            "quakers/",
        ] {
            assert!(!is_safe_relpath(bad), "should be rejected: {bad}");
        }
    }

    #[test]
    fn relative_paths_are_still_guarded_after_resolution() {
        let home = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME"));
        if let Some(h) = home {
            let h = PathBuf::from(h);
            assert!(
                container_reason_in(&h.join("Desktop"), Some(&h)).is_some(),
                "an absolutised Desktop must still be rejected"
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
