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

/// Downloads into staging what is not already there under its signed hash (a staged move, an
/// interrupted run); recover() checks every staged file again before installing it. Returns
/// the failed downloads.
async fn stage(
    client: &reqwest::Client,
    m: &quakers_launcher::manifest::Manifest,
    staging: &std::path::Path,
    jobs: &[quakers_launcher::manifest::FileEntry],
    id: Identity,
    from: &str,
) -> Result<Vec<(String, String)>> {
    let mut fetch = Vec::new();
    for j in jobs {
        if !u::matches(staging, j, id).await? {
            fetch.push(j.clone());
        }
    }
    if fetch.is_empty() {
        return Ok(Vec::new());
    }
    let total_bytes = fetch.iter().map(|f| f.size).sum();
    let outcome = download::download_all_ftesurf_with_ui(
        client.clone(),
        m.mirrors.clone(),
        m.hash_algo.clone(),
        staging.to_path_buf(),
        fetch,
        total_bytes,
        4,
        id,
        quakers_launcher::ui::Header {
            system: String::new(),
            platform: quakers_launcher::manifest::platform_key().into(),
            release: m.version.clone(),
            mirror: m.mirrors.join(" · "),
            from: from.into(),
        },
    )
    .await?;
    Ok(outcome.failed)
}

/// P1593 stage 2: everything that can fail without moving anything goes first: the move's
/// preconditions, the worldofsurf feed's download into staging, its default.fmf. Then the move.
async fn stage_move(
    client: &reqwest::Client,
    lock: &u::InstallLock,
    envelope: &str,
    m: &quakers_launcher::manifest::Manifest,
    allow_insecure: bool,
) -> Result<()> {
    let root = lock.root();
    migrate::check_start(root)?;
    let old = u::load_state(root, Identity::Legacy)?;
    let jobs = migrate::plan_moved(m, root, &old).await?;
    let staging = u::checked_path(root, ".ftesurf-updater/download", Identity::WorldOfSurf)?;
    let failed = stage(
        client,
        m,
        &staging,
        &jobs,
        Identity::WorldOfSurf,
        &old.version,
    )
    .await?;
    ensure!(failed.is_empty(), "download failed: {failed:?}");
    migrate::check_feed(root, &staging, m, &jobs).await?;
    ui::message(
        "Moving to World of Surf\u{2026}",
        "ftesurf/ \u{2192} worldofsurf/",
    )?;
    migrate::start(lock, envelope, u::PUBLIC_KEY, allow_insecure)
}

/// The update after its download question: stage (downloading only with a client), install
/// (P1593: the move's cfg step first), verify, finish a move, record the identity.
#[allow(clippy::too_many_arguments)]
async fn install_feed(
    lock: &u::InstallLock,
    client: Option<&reqwest::Client>,
    allow_insecure: bool,
    envelope: &str,
    m: &quakers_launcher::manifest::Manifest,
    old: quakers_launcher::state::State,
    jobs: &[quakers_launcher::manifest::FileEntry],
    id: Identity,
    moving: bool,
) -> Result<()> {
    let root = lock.root();
    let staging = u::checked_path(root, ".ftesurf-updater/download", id)?;
    if !jobs.is_empty() {
        let failed = match client {
            Some(client) => stage(client, m, &staging, jobs, id, &old.version).await?,
            None => {
                let mut missing = Vec::new();
                for j in jobs {
                    if !u::matches(&staging, j, id).await? {
                        missing.push((j.path.clone(), "not staged; updates are off".into()));
                    }
                }
                missing
            }
        };
        ensure!(
            failed.is_empty(),
            "download failed: {:?}; {}",
            failed,
            if moving {
                "the move to World of Surf finishes at the next start; the game cannot start until then"
            } else {
                "existing installation was not changed"
            }
        );
        ui::message("Installing World of Surf…", "Downloads verified.")?;
        if moving {
            migrate::before_install(lock, &staging, m, jobs).await?;
        }
        u::begin(root, envelope, jobs, id)?;
        u::recover(root, allow_insecure, id).await?;
    } else {
        if moving {
            migrate::before_install(lock, &staging, m, jobs).await?;
        }
        // Adopt only after bytes AND executable permissions are ready.
        u::prepare_install(root, m, id).await?;
        let mut st = old;
        for f in m.files_for_platform() {
            if u::matches(root, f, id).await? {
                st.files.insert(f.path.clone(), f.hash.clone());
            }
        }
        st.version = m.version.clone();
        u::write_meta(root, "state.json", &serde_json::to_vec_pretty(&st)?, id)?;
        u::write_meta(root, "installed.json", envelope.as_bytes(), id)?;
    }
    u::check_complete(root, m, id).await?;
    if moving {
        finish_move(lock, m).await?;
    }
    u::establish_identity(root, id) // P1593: the first verified install records it
}

/// P1593: a ftesurf/ that came back after the move may hold the player's last settings (an
/// exiting engine saves its cfg after releasing the lock); said every start until it is gone.
/// Best effort: an unreadable folder is named, never fatal (finish() still refuses on it).
fn report_leftovers(root: &std::path::Path) -> Result<()> {
    let found = match migrate::legacy_leftovers(root) {
        Ok(left) if left.is_empty() => return Ok(()),
        Ok(left) => format!("It holds {}.", left.join(", ")),
        Err(e) => format!("{e:#}."),
    };
    ui::message(
        "ftesurf/ came back after the move to World of Surf.",
        &format!("{found} Move anything you want to keep into worldofsurf/, then delete ftesurf/."),
    )
}

/// The install verified, so a failed clean-up (the old exe in use) only waits for the next start.
async fn finish_move(
    lock: &u::InstallLock,
    m: &quakers_launcher::manifest::Manifest,
) -> Result<()> {
    match migrate::finish(lock, m).await {
        Ok(()) => ui::message("Moved to World of Surf.", "ftesurf/ is now worldofsurf/."),
        Err(e) => ui::message(
            "The move to World of Surf has its clean-up left; the launcher tries it again every start.",
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
    // reads the install as one identity or recovers an update, but only on a start the player
    // makes: on the in-game restart (--wait-for-game) the exiting engine still saves its cfg
    // after releasing the lock (cl_main.c), so a move neither starts nor continues then.
    let mut moving = if a.wait_for_game && journal {
        ensure!(
            migrate::read_journal(&root)? == Some(migrate::Step::Marked)
                && !migrate::awaits_update(&root)?,
            "the move to World of Surf continues when you start WOS-Launcher yourself (nothing was started)"
        );
        false // installed and verified; its clean-up waits for that start
    } else {
        migrate::resume(&lock, u::PUBLIC_KEY, a.allow_insecure)?
    };
    if moving {
        id = u::install_identity(&root)?;
        ensure!(
            id == Identity::WorldOfSurf,
            "the World of Surf move left no marker"
        );
        report_leftovers(&root)?;
    }
    u::recover(&root, a.allow_insecure, id).await?;
    // P1593 stage 2: a move with only its clean-up left finishes now, from the installed release
    // (offline too), so no later feed can hold it; it is retried every start until it succeeds.
    if moving && !migrate::awaits_update(&root)? {
        match migrate::finish_installed(&lock, u::PUBLIC_KEY, a.allow_insecure).await {
            Ok(_) => ui::message("Moved to World of Surf.", "ftesurf/ is now worldofsurf/.")?,
            Err(e) => ui::message(
                "The move to World of Surf has its clean-up left; the launcher tries it again every start.",
                &format!("{e:#}"),
            )?,
        }
        moving = false;
    }
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
    let mut launch_manifest = None;
    let client = if enabled && !a.offline {
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
        Some(
            reqwest::Client::builder()
                .user_agent("FTESurf-Launcher/0.1")
                .connect_timeout(Duration::from_secs(4))
                .timeout(Duration::from_secs(120))
                .build()?,
        )
    } else {
        None
    };
    // P1593 stage 2: a legacy install moves once a signed worldofsurf feed answers. No answer,
    // or one that does not verify, keeps the legacy feed exactly as before. Never on the in-game
    // restart (see above; that run also never asks).
    if let Some(client) = client.as_ref().filter(|_| {
        !moving
            && !ingame_pending
            && !a.wait_for_game
            && (a.manifest_url.is_none() || a.move_manifest_url.is_some())
    }) {
        if migrate::eligible(&root)? {
            let url = a
                .move_manifest_url
                .as_deref()
                .unwrap_or(Identity::WorldOfSurf.feed_url());
            if let Ok(text) = fetch(client, a, url).await {
                match migrate::offer(
                    &text,
                    u::PUBLIC_KEY,
                    a.allow_insecure,
                    &u::load_state(&root, id)?,
                ) {
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
                            match stage_move(client, &lock, &text, &m, a.allow_insecure).await {
                                Ok(()) => {
                                    id = Identity::WorldOfSurf;
                                    moving = true;
                                }
                                // Nothing moved: this start stays on the legacy install.
                                Err(e) if migrate::read_journal(&root)?.is_none() => ui::message(
                                    "World of Surf is not installed yet; playing FTESurf this time.",
                                    &format!("{e:#}"),
                                )?,
                                Err(e) => return Err(e),
                            }
                        } else {
                            ui::message("Keeping FTESurf for now.", "")?;
                        }
                    }
                    Err(e) => ui::message("Staying on FTESurf updates.", &format!("{e:#}"))?,
                }
            }
        }
    }
    // P1593 stage 2: a move waiting for its update installs exactly the release it approved,
    // from staging; the network is needed only for objects staging lacks.
    if moving && migrate::awaits_update(&root)? {
        let (envelope, m) = migrate::approved_feed(&root, u::PUBLIC_KEY, a.allow_insecure)?;
        let old = u::load_state(&root, id)?;
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
        install_feed(
            &lock,
            client.as_ref(),
            a.allow_insecure,
            &envelope,
            &m,
            old,
            &jobs,
            id,
            true,
        )
        .await?;
        ui::message(&format!("World of Surf {} is ready.", m.version), "")?;
        launch_manifest = Some(m);
    }
    if let Some(client) = client.as_ref().filter(|_| launch_manifest.is_none()) {
        match fetch(
            client,
            a,
            a.manifest_url.as_deref().unwrap_or(id.feed_url()),
        )
        .await
        {
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
                if !jobs.is_empty()
                    && !install_approved
                    && !ui::choose(
                        if installed {
                            "Update World of Surf?"
                        } else {
                            "Download World of Surf?"
                        },
                        &format!(
                            "{change}  \u{b7}  {}",
                            ui::human(jobs.iter().map(|f| f.size).sum())
                        ),
                        a.yes,
                    )?
                {
                    ui::message("Download cancelled. No game was started.", "")?;
                    return Ok(());
                }
                let unchanged = jobs.is_empty();
                install_feed(
                    &lock,
                    Some(client),
                    a.allow_insecure,
                    &envelope,
                    &m,
                    old,
                    &jobs,
                    id,
                    false, // a move's own update ran above; its clean-up ran after recovery
                )
                .await?;
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
                    &if from == m.version && unchanged {
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
                ui::message(
                    "Update service unavailable. Checking the installed copy for offline play…",
                    &e.to_string(),
                )?;
                let cached = u::checked_path(&root, ".ftesurf-updater/installed.json", id)?;
                ensure!(cached.is_file(), "no verified installation is cached yet; --offline explicitly starts an existing archive installation");
                let mut m = u::decode(
                    &fs::read_to_string(cached)?,
                    u::PUBLIC_KEY,
                    a.allow_insecure,
                    id,
                )?;
                u::without_absent_ext(&root, &mut m, id)?; // P1668: a plugin this install never received
                if a.dry_run {
                    u::check_complete(&root, &m, id).await?;
                } else {
                    u::prepare_install(&root, &m, id).await?;
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
