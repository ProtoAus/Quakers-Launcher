//! World of Surf presentation for the signed FTESurf bootstrap: check, update, launch.
use anyhow::{ensure, Context, Result};
use clap::Parser;
use quakers_launcher::{
    download, ftesurf_migrate as migrate, ftesurf_ui as ui, ftesurf_update as u,
    ftesurf_update::Identity,
};
use std::{fs, path::PathBuf, time::Duration};

#[derive(Parser)]
#[command(
    name = "World of Surf",
    about = "World of Surf Launcher. Update, verify, then play. Settings and mounted games are never updated."
)]
struct Args {
    /// Render sample logo/progress only; no network, install writes or game launch.
    #[arg(long)]
    ui_preview: bool,
    /// Animate the centered logo with sample data only; Enter or Esc closes the preview.
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
    /// Default: the install identity's feed.
    #[arg(long)]
    manifest_url: Option<String>,
    /// Harness only: the worldofsurf feed a legacy install may move to. With --manifest-url and
    /// without this, no move is offered.
    #[arg(long, hide = true)]
    move_manifest_url: Option<String>,
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
    let screen = match ui::Session::begin() {
        Ok(screen) => screen,
        Err(e) => {
            eprintln!("World of Surf display: {e:#}");
            std::process::exit(1);
        }
    };
    let result = run(&args).await;
    if let Err(e) = &result {
        if screen.is_some() && !args.no_launch && !args.yes {
            // Path prettification is display-only; checked filesystem paths are untouched.
            let detail = format!("{e:#}").replace("\\\\?\\", "");
            let _ = ui::dismiss("Unable to continue", &detail);
        }
    }
    drop(screen);
    if let Err(e) = result {
        eprintln!("World of Surf: {e:#}\nThe launcher did not complete. Close the game and retry; partial updates are resumed, never launched.");
        std::process::exit(1);
    }
}

async fn fetch(client: &reqwest::Client, args: &Args, url: &str) -> Result<String> {
    let url = reqwest::Url::parse(url)?;
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

/// The install verified, so a failed clean-up (the old exe in use) only waits for the next start.
async fn finish_move(
    lock: &u::InstallLock,
    m: &quakers_launcher::manifest::Manifest,
) -> Result<()> {
    match migrate::finish(lock, m).await {
        Ok(()) => ui::message("Moved to World of Surf.", "ftesurf/ is now worldofsurf/."),
        Err(e) => ui::message(
            "World of Surf is installed; the old files are cleaned up next start.",
            &format!("{e:#}"),
        ),
    }
}

async fn run(a: &Args) -> Result<()> {
    u::checked_game_args(&a.game_args)?;
    ui::welcome()?;
    let root = a.dir.clone().unwrap_or(
        std::env::current_exe()?
            .parent()
            .context("launcher location")?
            .to_path_buf(),
    );
    ensure!(root.is_dir(), "install directory is missing");
    // P1593: the install's identity decides which feed, exe and gamedir every check below accepts.
    let mut id = u::install_identity(&root)?;
    // Check the root before canonicalizing (which would hide a junction).
    u::checked_path(&root, ".ftesurf-updater/install.lock", id)?;
    let root = root.canonicalize()?;
    let mut attempts = 0;
    let lock = loop {
        match u::InstallLock::try_acquire(&root, id) {
            Ok(lock) => break lock,
            Err(_) if a.wait_for_game && attempts < 200 => {
                attempts += 1;
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            Err(e) => {
                return Err(e)
                    .context("FTESurf or another updater is running; close it before updating")
            }
        }
    };
    ensure!(
        u::install_identity(&root)? == id,
        "the install's game folder changed while waiting for its lock; start the launcher again"
    );
    // A source checkout must never be repaired back to the public release.
    let development = root.join(".git").try_exists()?;
    let journal = u::checked_path(&root, migrate::JOURNAL, id)?.exists();
    ensure!(
        !development
            || (!journal && !u::checked_path(&root, ".ftesurf-updater/pending.json", id)?.exists()),
        "unfinished release update in a development checkout; resolve it before launching (no files were promoted)"
    );
    // Recovery precedes ALL launch paths, but a dry run must not commit a transaction.
    ensure!(
        !a.dry_run || !u::checked_path(&root, ".ftesurf-updater/pending.json", id)?.exists(),
        "unfinished update: dry run does not recover it; rerun --verify to finish safely"
    );
    ensure!(
        !a.dry_run || !journal,
        "unfinished move to World of Surf: a dry run does not resume it"
    );
    // P1593 stage 2: an interrupted folder move is carried forward (or refused) before anything
    // reads the install as one identity or recovers an update.
    let mut moving = migrate::resume(&lock)?;
    if moving {
        id = u::install_identity(&root)?;
        ensure!(
            id == Identity::WorldOfSurf,
            "the World of Surf move left no marker"
        );
    }
    u::recover(&root, a.allow_insecure, id).await?;
    let ingame_journal = u::checked_path(&root, ".ftesurf-updater/ingame/journal.json", id)?;
    let ingame_pending = ingame_journal.exists();
    let consent = u::checked_path(&root, ".ftesurf-updater/consent", id)?;
    let installed = u::checked_path(&root, id.exec(), id)?.is_file()
        && u::checked_path(&root, "default.fmf", id)?.is_file();
    let mut install_approved = false;
    let enabled = if development {
        ui::message(
            "Development checkout: public release updates skipped; keeping your dev build.",
            "",
        )?;
        false
    } else if a.yes || a.enable_updates {
        if !a.dry_run {
            u::write_meta(&root, "consent", b"1\n", id)?;
        }
        true
    } else if a.offline {
        false
    } else if consent.exists() && (installed || fs::read_to_string(&consent)?.trim() == "1") {
        fs::read_to_string(&consent)?.trim() == "1"
    } else {
        ensure!(
            ui::interactive(false),
            "first launch needs an interactive terminal; use --yes to enable updates or --offline to skip"
        );
        let on = ui::choose(
            if installed { "Enable automatic updates?" } else { "Download World of Surf?" },
            "Automatic updates; services see your IP.\nproto.bar / dl.proto.bar · no files uploaded.",
            false,
        )?;
        if !installed && !on {
            ui::message("Download cancelled.", "No game was started.")?;
            return Ok(());
        }
        install_approved = !installed && on;
        if !a.dry_run {
            u::write_meta(&root, "consent", if on { b"1\n" } else { b"0\n" }, id)?;
        }
        on
    };
    ensure!(!ingame_pending || (enabled && !a.offline), "unfinished in-game update: enable updates so the launcher can verify and repair the complete signed release");
    ensure!(
        !moving || !migrate::awaits_update(&root)? || (enabled && !a.offline),
        "the move to World of Surf is unfinished: enable updates so the launcher can install it (nothing was started)"
    );
    let mut launch_manifest = None;
    if enabled && !a.offline {
        // P1208: say what is installed before anything is fetched.
        let have = u::load_state(&root, id)
            .map(|s| s.version)
            .unwrap_or_default();
        ui::message(
            "Checking for updates…",
            &if have.is_empty() {
                "Verifying the signed release.".to_string()
            } else {
                format!("Installed: World of Surf {have}. Verifying the signed release.")
            },
        )?;
        let client = reqwest::Client::builder()
            .user_agent("FTESurf-Launcher/0.1")
            .connect_timeout(Duration::from_secs(4))
            .timeout(Duration::from_secs(120))
            .build()?;
        // P1593 stage 2: a legacy install moves once a signed worldofsurf feed answers. No
        // answer, or one that does not verify, keeps the legacy feed exactly as before.
        let mut probed = None;
        let mut move_approved = false;
        if !moving
            && !ingame_pending
            && (a.manifest_url.is_none() || a.move_manifest_url.is_some())
            && migrate::eligible(&root)?
        {
            let url = a
                .move_manifest_url
                .as_deref()
                .unwrap_or(Identity::WorldOfSurf.feed_url());
            if let Ok(text) = fetch(&client, a, url).await {
                match migrate::offer(&text, u::PUBLIC_KEY, a.allow_insecure, &u::load_state(&root, id)?) {
                    Ok(m) if a.dry_run => {
                        ui::message(
                            &format!("World of Surf {} is available: ftesurf/ would move to worldofsurf/.", m.version),
                            "",
                        )?;
                        return Ok(());
                    }
                    Ok(m) => {
                        if ui::choose(
                            "Move to World of Surf?",
                            &format!(
                                "World of Surf {}  \u{b7}  ftesurf/ becomes worldofsurf/; settings, runs and maps move with it.",
                                m.version
                            ),
                            a.yes,
                        )? {
                            ui::message("Moving to World of Surf\u{2026}", "ftesurf/ \u{2192} worldofsurf/")?;
                            migrate::start(&lock)?;
                            id = Identity::WorldOfSurf;
                            moving = true;
                            move_approved = true;
                            probed = Some(text);
                        } else {
                            ui::message("Keeping FTESurf for now.", "")?;
                        }
                    }
                    Err(e) => ui::message("Staying on FTESurf updates.", &format!("{e:#}"))?,
                }
            }
        }
        let fetched = match probed {
            Some(text) => Ok(text),
            None => {
                fetch(
                    &client,
                    a,
                    a.manifest_url.as_deref().unwrap_or(id.feed_url()),
                )
                .await
            }
        };
        match fetched {
            Ok(envelope) => {
                // Bad signatures/formats do NOT fall back and launch as if this were a network outage.
                let m = u::decode(&envelope, u::PUBLIC_KEY, a.allow_insecure, id)?;
                let old = u::load_state(&root, id)?;
                let from = old.version.clone(); // P1208: empty for a fresh or adopted install
                let change = if from.is_empty() || from == m.version {
                    format!("World of Surf {}", m.version)
                } else {
                    format!("World of Surf {} \u{2192} {}", from, m.version)
                };
                if !old.version.is_empty() {
                    ensure!(
                        u::version(&m.version)? >= u::version(&old.version)?,
                        "refusing an older signed release"
                    );
                }
                let (jobs, kept) = u::plan(&m, &root, &old, id).await?;
                for p in &kept {
                    ui::message(&format!("Keeping your local file: {p}"), "")?;
                }
                if a.dry_run {
                    ui::message(
                        &format!("{} file(s) would update: {change}", jobs.len()),
                        "",
                    )?;
                    return Ok(());
                }
                let staging = u::checked_path(&root, ".ftesurf-updater/download", id)?;
                if !jobs.is_empty() {
                    let total_bytes = jobs.iter().map(|f| f.size).sum();
                    if !install_approved
                        && !move_approved
                        && !ui::choose(
                            if installed {
                                "Update World of Surf?"
                            } else {
                                "Download World of Surf?"
                            },
                            &format!("{change}  \u{b7}  {}", ui::human(total_bytes)),
                            a.yes,
                        )?
                    {
                        ui::message("Download cancelled. No game was started.", "")?;
                        return Ok(());
                    }
                    let outcome = download::download_all_ftesurf_with_ui(
                        client,
                        m.mirrors.clone(),
                        m.hash_algo.clone(),
                        staging.clone(),
                        jobs.clone(),
                        total_bytes,
                        4,
                        id,
                        quakers_launcher::ui::Header {
                            system: String::new(),
                            platform: quakers_launcher::manifest::platform_key().into(),
                            release: m.version.clone(),
                            mirror: m.mirrors.join(" · "),
                            from: from.clone(),
                        },
                    )
                    .await?;
                    ensure!(
                        outcome.failed.is_empty(),
                        "download failed: {:?}; existing installation was not changed",
                        outcome.failed
                    );
                    ui::message("Installing World of Surf…", "Downloads verified.")?;
                    if moving {
                        migrate::before_install(&lock, &staging, &m, &jobs).await?;
                    }
                    u::begin(&root, &envelope, &jobs, id)?;
                    u::recover(&root, a.allow_insecure, id).await?;
                } else {
                    if moving {
                        migrate::before_install(&lock, &staging, &m, &jobs).await?;
                    }
                    // Adopt only after bytes AND executable permissions are ready.
                    u::prepare_install(&root, &m, id).await?;
                    let mut st = old;
                    for f in m.files_for_platform() {
                        if u::matches(&root, f, id).await? {
                            st.files.insert(f.path.clone(), f.hash.clone());
                        }
                    }
                    st.version = m.version.clone();
                    u::write_meta(&root, "state.json", &serde_json::to_vec_pretty(&st)?, id)?;
                    u::write_meta(&root, "installed.json", envelope.as_bytes(), id)?;
                }
                u::check_complete(&root, &m, id).await?;
                if moving {
                    finish_move(&lock, &m).await?;
                }
                u::establish_identity(&root, id)?; // P1593: the first verified install records it
                if ingame_pending {
                    // The fetched signed feed, not untrusted old journal entries, determines
                    // every repair. Preserve the old journal only after the full set verifies.
                    let stamp = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)?
                        .as_nanos();
                    let saved = u::checked_path(
                        &root,
                        &format!(".ftesurf-updater/recovered-ingame-{stamp}.json"),
                        id,
                    )?;
                    fs::rename(&ingame_journal, saved)?;
                    ui::message("Recovered the interrupted in-game update.", "")?;
                }
                ui::message(
                    &if from == m.version && jobs.is_empty() {
                        format!("World of Surf {} is up to date.", m.version)
                    } else if !from.is_empty() && from != m.version {
                        format!("Updated World of Surf {} \u{2192} {}.", from, m.version)
                    } else {
                        format!("World of Surf {} is ready.", m.version)
                    },
                    "",
                )?;
                launch_manifest = Some(m);
            }
            Err(e) => {
                ensure!(!ingame_pending, "cannot repair the unfinished in-game update while the update service is unavailable");
                ensure!(
                    !moving || !migrate::awaits_update(&root)?,
                    "the move to World of Surf needs the update service to finish; nothing was started (try again online)"
                );
                ui::message(
                    "Update service unavailable. Checking the installed copy for offline play…",
                    &e.to_string(),
                )?;
                let cached = u::checked_path(&root, ".ftesurf-updater/installed.json", id)?;
                ensure!(cached.is_file(), "no verified installation is cached yet; --offline explicitly starts an existing archive installation");
                let m = u::decode(
                    &fs::read_to_string(cached)?,
                    u::PUBLIC_KEY,
                    a.allow_insecure,
                    id,
                )?;
                if a.dry_run {
                    u::check_complete(&root, &m, id).await?;
                } else {
                    u::prepare_install(&root, &m, id).await?;
                    if moving {
                        finish_move(&lock, &m).await?;
                    }
                }
                launch_manifest = Some(m);
            }
        }
    }
    if a.dry_run || a.no_launch {
        return Ok(());
    }
    let exe = u::checked_path(&root, id.exec(), id)?;
    ensure!(
        exe.is_file() && u::checked_path(&root, "default.fmf", id)?.is_file(),
        "FTESurf is not installed here; enable updates to install it"
    );
    if !ui::confirm("Play World of Surf?", a.yes)? {
        ui::message(
            "Launch cancelled. Your installation is ready for next time.",
            "",
        )?;
        return Ok(());
    }
    if let Some(m) = &launch_manifest {
        ui::message("Verifying World of Surf…", "")?;
        u::check_complete(&root, m, id).await?;
    }
    ui::message("Starting World of Surf…", "")?;
    // The engine takes this same lock BEFORE loading plugins/progs. Drop ours before spawn;
    // if another launcher wins that small gap, the engine refuses to boot, not a mixed set.
    lock.unlock()?;
    let mut game = std::process::Command::new(exe);
    game.args(["-manifest", "default.fmf"])
        .args(&a.game_args)
        .current_dir(&root);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // The game must not keep the launcher's console window alive.
        game.stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .creation_flags(0x00000008 | 0x00000200); // DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP
        game.spawn().context("could not start FTESurf")?;
    }
    #[cfg(not(windows))]
    {
        // Keep the untested non-Windows handoff unchanged.
        let status = game.status()?;
        ensure!(status.success(), "game exited with {status}");
    }
    Ok(())
}
