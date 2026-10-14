//! World of Surf presentation for the signed FTESurf bootstrap: check, update, launch.
use anyhow::{bail, ensure, Context, Result};
use clap::Parser;
use fs2::FileExt;
use quakers_launcher::{download, ftesurf_update as u, ftesurf_ui as ui};
use std::{
    fs,
    io,
    path::PathBuf,
    time::Duration,
};

#[derive(Parser)]
#[command(
    name = "World of Surf",
    about = "World of Surf Launcher. Update, verify, then play. Settings and mounted games are never updated."
)]
struct Args {
    /// Render sample artwork/progress only; no network, install writes or game launch.
    #[arg(long)]
    ui_preview: bool,
    /// Animate the logo/artwork with sample data only; Enter or Esc closes the preview.
    #[arg(long, conflicts_with = "ui_preview")]
    ui_demo: bool,
    #[arg(long)]
    dir: Option<PathBuf>,
    /// Consent once without the interactive first-run question (automation/installers).
    #[arg(long)]
    yes: bool,
    /// Enable automatic checks again after choosing offline mode.
    #[arg(long)]
    enable_updates: bool,
    /// Skip this update check and play the existing installation.
    #[arg(long)]
    offline: bool,
    #[arg(long)]
    no_launch: bool,
    /// In-game handoff: wait briefly for the exiting game to release its install lock.
    #[arg(long)]
    wait_for_game: bool,
    /// Checks always hash every managed file; retained as an explicit repair entry point.
    #[arg(long)]
    verify: bool,
    #[arg(long)]
    dry_run: bool,
    #[arg(long, default_value = u::FEED_URL)]
    manifest_url: String,
    /// Harness only: allow HTTP to localhost. Signatures are still mandatory.
    #[arg(long)]
    allow_insecure: bool,
    #[arg(last = true)]
    game_args: Vec<String>,
}

#[tokio::main]
async fn main() {
    let args = Args::parse();
    if args.ui_preview {
        print!("{}", ui::preview());
        return;
    }
    if args.ui_demo {
        if let Err(e) = ui::demo() {
            eprintln!("World of Surf preview: {e:#}");
            std::process::exit(1);
        }
        return;
    }
    if let Err(e) = run(&args).await {
        eprintln!("World of Surf: {e:#}\nThe launcher did not complete. Close the game and retry; partial updates are resumed, never launched.");
        if !args.no_launch && ui::interactive(args.yes) {
            eprint!("Press Enter to close.");
            let _ = io::stdin().read_line(&mut String::new());
        }
        std::process::exit(1);
    }
}

async fn fetch(client: &reqwest::Client, args: &Args) -> Result<String> {
    let url = reqwest::Url::parse(&args.manifest_url)?;
    ensure!(
        url.scheme() == "https"
            || (args.allow_insecure
                && url.scheme() == "http"
                && matches!(url.host_str(), Some("127.0.0.1" | "localhost" | "[::1]"))),
        "manifest must use HTTPS (test HTTP is loopback only)"
    );
    let mut r = client
        .get(url)
        .timeout(Duration::from_secs(8))
        .header("Cache-Control", "no-cache")
        .send()
        .await?
        .error_for_status()?;
    if let Some(n) = r.content_length() {
        ensure!(n <= u::MAX_FEED as u64, "manifest too large");
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = r.chunk().await? {
        ensure!(
            bytes.len() + chunk.len() <= u::MAX_FEED,
            "manifest too large"
        );
        bytes.extend_from_slice(&chunk);
    }
    String::from_utf8(bytes).context("manifest is not UTF-8")
}

async fn run(a: &Args) -> Result<()> {
    ui::welcome()?;
    let root = a.dir.clone().unwrap_or(
        std::env::current_exe()?
            .parent()
            .context("launcher location")?
            .to_path_buf(),
    );
    ensure!(root.is_dir(), "install directory is missing");
    // Check the root before canonicalizing (which would hide a junction).
    u::checked_path(&root, ".ftesurf-updater/install.lock")?;
    let root = root.canonicalize()?;
    let lockpath = u::checked_path(&root, ".ftesurf-updater/install.lock")?;
    fs::create_dir_all(lockpath.parent().unwrap())?;
    let lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(lockpath)?;
    let mut attempts = 0;
    loop {
        match FileExt::try_lock_exclusive(&lock) {
            Ok(()) => break,
            Err(_) if a.wait_for_game && attempts < 200 => {
                attempts += 1;
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            Err(e) => {
                return Err(e)
                    .context("FTESurf or another updater is running; close it before updating")
            }
        }
    }
    // Recovery precedes ALL launch paths, but a dry run must not commit a transaction.
    ensure!(
        !a.dry_run || !u::checked_path(&root, ".ftesurf-updater/pending.json")?.exists(),
        "unfinished update: dry run does not recover it; rerun --verify to finish safely"
    );
    u::recover(&root, a.allow_insecure).await?;
    let ingame_journal = u::checked_path(&root, ".ftesurf-updater/ingame/journal.json")?;
    let ingame_pending = ingame_journal.exists();
    let consent = u::checked_path(&root, ".ftesurf-updater/consent")?;
    let enabled = if a.yes || a.enable_updates {
        if !a.dry_run {
            u::write_meta(&root, "consent", b"1\n")?;
        }
        true
    } else if a.offline {
        false
    } else if consent.exists() {
        fs::read_to_string(&consent)?.trim() == "1"
    } else {
        ensure!(
            ui::interactive(false),
            "first launch needs an interactive terminal; use --yes to enable updates or --offline to skip"
        );
        println!("World of Surf can automatically check proto.bar and download updates from dl.proto.bar on launch.\nThose services see your IP address; no settings, recordings or mounted game assets are uploaded.");
        let on = ui::confirm("Enable automatic updates?", false)?;
        if !a.dry_run {
            u::write_meta(&root, "consent", if on { b"1\n" } else { b"0\n" })?;
        }
        on
    };
    ensure!(!ingame_pending || (enabled && !a.offline), "unfinished in-game update: enable updates so the launcher can verify and repair the complete signed release");
    let mut launch_manifest = None;
    if enabled && !a.offline {
        println!("World of Surf: checking the signed release...");
        let client = reqwest::Client::builder()
            .user_agent("FTESurf-Launcher/0.1")
            .connect_timeout(Duration::from_secs(4))
            .timeout(Duration::from_secs(120))
            .build()?;
        match fetch(&client, a).await {
            Ok(envelope) => {
                // Bad signatures/formats do NOT fall back and launch as if this were a network outage.
                let m = u::decode(&envelope, u::PUBLIC_KEY, a.allow_insecure)?;
                let old = u::load_state(&root)?;
                if !old.version.is_empty() {
                    ensure!(
                        u::version(&m.version)? >= u::version(&old.version)?,
                        "refusing an older signed release"
                    );
                }
                let (jobs, kept) = u::plan(&m, &root, &old).await?;
                for p in &kept {
                    println!("Keeping your local file: {p}");
                }
                if a.dry_run {
                    println!("{} file(s) would update to {}", jobs.len(), m.version);
                    return Ok(());
                }
                if !jobs.is_empty() {
                    println!(
                        "Updating to {}: {} changed file(s), {:.1} MB",
                        m.version,
                        jobs.len(),
                        jobs.iter().map(|f| f.size).sum::<u64>() as f64 / 1048576.0
                    );
                    if !ui::confirm(&format!("Download {} file(s) / {} to {}?", jobs.len(),
                        ui::human(jobs.iter().map(|f| f.size).sum()), root.display()), a.yes)? {
                        println!("Download cancelled. No game was started.");
                        return Ok(());
                    }
                    let staging = u::checked_path(&root, ".ftesurf-updater/download")?;
                    let outcome = download::download_all_ftesurf_with_ui(
                        client,
                        m.mirrors.clone(),
                        m.hash_algo.clone(),
                        staging,
                        jobs.clone(),
                        jobs.iter().map(|f| f.size).sum(),
                        3,
                        quakers_launcher::ui::Header {
                            system: String::new(),
                            platform: quakers_launcher::manifest::platform_key().into(),
                            release: m.version.clone(),
                            mirror: m.mirrors.join(" · "),
                        },
                    )
                    .await?;
                    ensure!(
                        outcome.failed.is_empty(),
                        "download failed: {:?}; existing installation was not changed",
                        outcome.failed
                    );
                    println!("Downloads verified. Installing the signed release...");
                    u::begin(&root, &envelope, &jobs)?;
                    u::recover(&root, a.allow_insecure).await?;
                } else {
                    // Adopt only after bytes AND executable permissions are ready.
                    u::prepare_install(&root, &m).await?;
                    let mut st = old;
                    for f in m.files_for_platform() {
                        if u::matches(&root, f).await? {
                            st.files.insert(f.path.clone(), f.hash.clone());
                        }
                    }
                    st.version = m.version.clone();
                    u::write_meta(&root, "state.json", &serde_json::to_vec_pretty(&st)?)?;
                    u::write_meta(&root, "installed.json", envelope.as_bytes())?;
                }
                u::check_complete(&root, &m).await?;
                if ingame_pending {
                    // The fetched signed feed, not untrusted old journal entries, determines
                    // every repair. Preserve the old journal only after the full set verifies.
                    let stamp = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)?
                        .as_nanos();
                    let saved = u::checked_path(
                        &root,
                        &format!(".ftesurf-updater/recovered-ingame-{stamp}.json"),
                    )?;
                    fs::rename(&ingame_journal, saved)?;
                    println!("Recovered the interrupted in-game update.");
                }
                println!("World of Surf {} is ready.", m.version);
                launch_manifest = Some(m);
            }
            Err(e) => {
                ensure!(!ingame_pending, "cannot repair the unfinished in-game update while the update service is unavailable");
                println!("Update service unavailable: {e}. Checking the installed copy for offline play...");
                let cached = u::checked_path(&root, ".ftesurf-updater/installed.json")?;
                ensure!(cached.is_file(), "no verified installation is cached yet; --offline explicitly starts an existing archive installation");
                let m = u::decode(
                    &fs::read_to_string(cached)?,
                    u::PUBLIC_KEY,
                    a.allow_insecure,
                )?;
                if a.dry_run {
                    u::check_complete(&root, &m).await?;
                } else {
                    u::prepare_install(&root, &m).await?;
                }
                launch_manifest = Some(m);
            }
        }
    }
    if a.dry_run || a.no_launch {
        return Ok(());
    }
    let exe = u::checked_path(&root, u::default_exec())?;
    ensure!(
        exe.is_file() && u::checked_path(&root, "default.fmf")?.is_file(),
        "FTESurf is not installed here; enable updates to install it"
    );
    if !ui::confirm("WORLD OF SURF is ready. Press Enter to launch.", a.yes)? {
        println!("Launch cancelled. Your installation is ready for next time.");
        return Ok(());
    }
    if let Some(m) = &launch_manifest {
        u::check_complete(&root, m).await?;
    }
    // The engine takes this same lock BEFORE loading plugins/progs. Drop ours before spawn;
    // if another launcher wins that small gap, the engine refuses to boot, not a mixed set.
    FileExt::unlock(&lock)?;
    let status = std::process::Command::new(exe)
        .args(["-manifest", "default.fmf"])
        .args(&a.game_args)
        .current_dir(&root)
        .status()?;
    if !status.success() {
        bail!("game exited with {status}");
    }
    Ok(())
}
