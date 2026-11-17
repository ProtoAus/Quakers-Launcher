//! P1593 stage 2: the one-time move of a legacy install's `ftesurf/` to `worldofsurf/`, before
//! the worldofsurf feed's first update. The launcher stages and checks that feed first
//! (plan_moved, check_feed), so a failed download or a bad default.fmf moves nothing.
//! Journalled in JOURNAL; each step is recorded after its effect and every effect is idempotent,
//! so resume() finishes an interrupted move from any point:
//!   intent  - journal written, then BARRIER as pending.json: every FTESurf engine refuses to boot
//!             while pending.json exists (qkupdate.c QKU_FSLock); older launchers cannot parse it
//!   renamed - the legacy exe parked in PARKED, then ftesurf/ renamed to worldofsurf/
//!   ledger  - state.json keys respelled; the main cfg's saved legacy feed URL dropped
//!   marked  - the identity marker rewritten ftesurf -> worldofsurf (the commit point)
//! The ordinary update then installs the worldofsurf feed; begin() swaps BARRIER for its own
//! journal. finish() deletes the parked exe and the journal once that install verifies.
use crate::{
    ftesurf_update::{self as u, Identity, InstallLock},
    manifest::{FileEntry, Manifest},
    state::State,
};
use anyhow::{bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
};

pub const JOURNAL: &str = ".ftesurf-updater/migrate.json";
const PENDING: &str = ".ftesurf-updater/pending.json";
const INGAME: &str = ".ftesurf-updater/ingame/journal.json";
const STATE: &str = ".ftesurf-updater/state.json";
/// pending.json while a move is unfinished. People read it; nothing parses it.
pub const BARRIER: &[u8] = b"{\"move\":\"ftesurf/ to worldofsurf/ is unfinished; run WOS-Launcher to finish it\",\"journal\":\".ftesurf-updater/migrate.json\"}\n";
/// The legacy MAINCONFIG's file, and the one name a worldofsurf default.fmf may switch it to.
const LEGACY_CFG: &str = "ftesurf.cfg";
const NEW_CFG: &str = "worldofsurf.cfg";
/// Where the legacy exe waits for finish(). An engine from before P1593 takes the install lock
/// and honours pending.json only under GAME "FTESurf" (patch-1588 qkupdate.c:2170), so left in
/// the root it would boot the new default.fmf unguarded.
const PARKED: &str = ".ftesurf-updater/legacy";

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Step {
    Intent,
    Renamed,
    Ledger,
    Marked,
}

/// Where a test stops the move to stand in for a crash: after an effect (`Done`), after its
/// journal record, or between finish()'s removals.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Point {
    Barrier,
    Done(Step),
    Recorded(Step),
    BarrierRemoved,
    ExeRemoved,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Journal {
    schema: u32,
    from: String,
    to: String,
    step: Step,
}

/// record_identity's proof that the marker step of a journalled move is asking.
pub struct MoveToken {
    _private: (),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Dir {
    Absent,
    Empty,
    Game,
    Link,
    Other,
}

fn meta(root: &Path, rel: &str) -> Result<PathBuf> {
    // META paths are the same under both identities
    u::checked_path(root, rel, Identity::Legacy)
}

fn gamedir_path(root: &Path, id: Identity) -> PathBuf {
    root.join(id.gamedir().trim_end_matches('/'))
}

/// A link or junction, a file at the name, or a non-empty folder without the game is `Other`
/// or `Link`: the move neither follows nor merges into either.
fn gamedir(root: &Path, id: Identity) -> Result<Dir> {
    let p = gamedir_path(root, id);
    let Some(md) = u::inspect(&p)? else {
        return Ok(Dir::Absent);
    };
    if u::is_link(&md) {
        return Ok(Dir::Link);
    }
    if !md.is_dir() {
        return Ok(Dir::Other);
    }
    if u::gamedir_counts(root, id)? {
        return Ok(Dir::Game);
    }
    Ok(if fs::read_dir(&p)?.next().is_none() {
        Dir::Empty
    } else {
        Dir::Other
    })
}

/// None without a journal. A journal that is not a regular file holding exactly this format
/// is refused, never guessed past.
pub fn read_journal(root: &Path) -> Result<Option<Step>> {
    let p = meta(root, JOURNAL)?;
    let Some(md) = u::inspect(&p)? else {
        return Ok(None);
    };
    ensure!(
        md.is_file() && !u::is_link(&md) && md.len() <= 4096,
        "the World of Surf move journal is not a regular file: {JOURNAL}"
    );
    let j: Journal = serde_json::from_slice(&fs::read(&p)?)
        .with_context(|| format!("damaged World of Surf move journal: {JOURNAL}"))?;
    ensure!(
        j.schema == 1 && j.from == "ftesurf" && j.to == "worldofsurf",
        "unknown World of Surf move journal: {JOURNAL}"
    );
    Ok(Some(j.step))
}

fn record(root: &Path, step: Step, hook: &mut dyn FnMut(Point) -> Result<()>) -> Result<()> {
    let j = Journal {
        schema: 1,
        from: "ftesurf".into(),
        to: "worldofsurf".into(),
        step,
    };
    u::replace_meta(
        root,
        "migrate.json",
        &serde_json::to_vec(&j)?,
        Identity::Legacy,
    )?;
    hook(Point::Recorded(step))
}

/// Some(step) when pending.json is BARRIER and the journal says `step`; None when pending.json
/// is absent or an ordinary update journal. BARRIER without a journal is refused.
pub fn pending_barrier(root: &Path) -> Result<Option<Step>> {
    let p = meta(root, PENDING)?;
    match u::inspect(&p)? {
        Some(md) if md.is_file() && md.len() == BARRIER.len() as u64 => {}
        _ => return Ok(None),
    }
    if fs::read(&p)? != BARRIER {
        return Ok(None);
    }
    let step = read_journal(root)?;
    ensure!(
        step.is_some(),
        "{PENDING} is a World of Surf move barrier without its journal {JOURNAL}; inspect .ftesurf-updater before starting"
    );
    Ok(step)
}

/// An existing legacy install with nothing unfinished: what the launcher offers the move to.
/// A fresh install is not (FRESH_INSTALL decides it), nor one with an update still pending.
pub fn eligible(root: &Path) -> Result<bool> {
    Ok(u::install_identity(root)? == Identity::Legacy
        && read_journal(root)?.is_none()
        && gamedir(root, Identity::Legacy)? == Dir::Game
        && u::inspect(&meta(root, PENDING)?)?.is_none()
        && u::inspect(&meta(root, INGAME)?)?.is_none())
}

/// The worldofsurf feed the move would install: signed, and not older than this install
/// (both identities share state.json's version floor). Err keeps the legacy feed.
pub fn offer(text: &str, public_key: &str, allow_insecure: bool, st: &State) -> Result<Manifest> {
    let m = u::decode(text, public_key, allow_insecure, Identity::WorldOfSurf)?;
    if !st.version.is_empty() {
        ensure!(
            u::version(&m.version)? >= u::version(&st.version)?,
            "World of Surf {} is older than this install's {}",
            m.version,
            st.version
        );
    }
    Ok(m)
}

fn regular_or_absent(p: &Path) -> Result<bool> {
    match u::inspect(p)? {
        None => Ok(false),
        Some(md) => {
            ensure!(
                md.is_file() && !u::is_link(&md),
                "{} is a link or not a regular file; the move does not follow it",
                p.display()
            );
            Ok(true)
        }
    }
}

/// Everything the steps need, checked before the journal exists, so a refusal changes nothing.
/// The launcher asks it before downloading; start() asks again under the same lock.
pub fn check_start(root: &Path) -> Result<()> {
    ensure!(
        read_journal(root)?.is_none(),
        "a World of Surf move is already journalled"
    );
    ensure!(
        matches!(u::read_marker(root)?, None | Some(Identity::Legacy)),
        "this install is already World of Surf"
    );
    match gamedir(root, Identity::Legacy)? {
        Dir::Game => {}
        Dir::Link => bail!(
            "ftesurf/ is a link or junction; the move does not follow it (nothing was changed)"
        ),
        _ => bail!("there is no ftesurf/ game folder to move"),
    }
    match gamedir(root, Identity::WorldOfSurf)? {
        Dir::Absent | Dir::Empty => {}
        Dir::Link => bail!("worldofsurf/ is a link or junction; remove it so ftesurf/ can move there (nothing was changed)"),
        _ => bail!("worldofsurf/ already exists beside ftesurf/; move it out of the install folder so ftesurf/ can move there (nothing was changed)"),
    }
    ensure!(
        u::inspect(&meta(root, PENDING)?)?.is_none(),
        "an update is unfinished; it is finished before the move"
    );
    ensure!(
        u::inspect(&meta(root, INGAME)?)?.is_none(),
        "an in-game update is unfinished; it is repaired before the move"
    );
    regular_or_absent(&gamedir_path(root, Identity::Legacy).join(LEGACY_CFG))?;
    regular_or_absent(&root.join(Identity::Legacy.exec()))?;
    u::load_state(root, Identity::Legacy)?;
    Ok(())
}

fn parked_exe(root: &Path) -> Result<PathBuf> {
    meta(root, &format!("{PARKED}/{}", Identity::Legacy.exec()))
}

/// Idempotent: a root exe (re)appearing is parked over an older parked copy.
fn park_exe(root: &Path) -> Result<()> {
    let exe = root.join(Identity::Legacy.exec());
    if !regular_or_absent(&exe)? {
        return Ok(());
    }
    let parked = parked_exe(root)?;
    fs::create_dir_all(parked.parent().unwrap())?;
    fs::rename(&exe, &parked).with_context(|| {
        format!(
            "could not move the old {} aside (in use?)",
            Identity::Legacy.exec()
        )
    })?;
    u::sync_dir(root)
}

fn unpark_exe(root: &Path) -> Result<()> {
    let (exe, parked) = (root.join(Identity::Legacy.exec()), parked_exe(root)?);
    if u::inspect(&exe)?.is_none() && regular_or_absent(&parked)? {
        fs::rename(&parked, &exe)?;
        u::sync_dir(root)?;
    }
    let _ = fs::remove_dir(parked.parent().unwrap()); // only when empty
    Ok(())
}

fn ensure_barrier(root: &Path) -> Result<()> {
    if pending_barrier(root)?.is_some() {
        return Ok(());
    }
    ensure!(
        u::inspect(&meta(root, PENDING)?)?.is_none(),
        "an update appeared in the middle of the World of Surf move; inspect .ftesurf-updater"
    );
    u::replace_meta(root, "pending.json", BARRIER, Identity::Legacy)
}

/// Undo an intent whose rename did not happen: the install stays a working legacy one.
fn roll_back(root: &Path) -> Result<()> {
    unpark_exe(root)?;
    if pending_barrier(root)?.is_some() {
        fs::remove_file(meta(root, PENDING)?)?;
    }
    let journal = meta(root, JOURNAL)?;
    fs::remove_file(&journal)?;
    u::sync_dir(journal.parent().unwrap())
}

fn rename(root: &Path) -> Result<()> {
    let (old, new) = (
        gamedir_path(root, Identity::Legacy),
        gamedir_path(root, Identity::WorldOfSurf),
    );
    let (l, w) = (
        gamedir(root, Identity::Legacy)?,
        gamedir(root, Identity::WorldOfSurf)?,
    );
    match (l, w) {
        // renamed before an interruption (check_start saw ftesurf/ with the game, worldofsurf/ without)
        (Dir::Absent | Dir::Empty | Dir::Other, Dir::Game) => return park_exe(root),
        (Dir::Game, Dir::Absent | Dir::Empty) => {}
        (l, w) => bail!(
            "cannot move ftesurf/ ({l:?}) to worldofsurf/ ({w:?}); the move is journalled in {JOURNAL}; inspect both folders before starting"
        ),
    }
    // Windows cannot rename onto a folder, so an empty one goes first everywhere.
    let moved = park_exe(root)
        .and_then(|()| match w {
            Dir::Empty => fs::remove_dir(&new).map_err(Into::into),
            _ => Ok(()),
        })
        .and_then(|()| fs::rename(&old, &new).map_err(Into::into));
    if let Err(e) = moved {
        if gamedir(root, Identity::Legacy)? == Dir::Game
            && matches!(
                gamedir(root, Identity::WorldOfSurf)?,
                Dir::Absent | Dir::Empty
            )
        {
            roll_back(root)?;
            return Err(e).context(
                "could not move ftesurf/ to worldofsurf/ (is a file in it in use?); nothing was changed: close the game and anything using its folder, then start again",
            );
        }
        return Err(e).context(format!(
            "moving ftesurf/ to worldofsurf/ failed part-way; the move is journalled in {JOURNAL}"
        ));
    }
    u::sync_dir(root)
}

/// After the rename nothing may make the install half-moved again.
fn require_moved(root: &Path) -> Result<()> {
    let (l, w) = (
        gamedir(root, Identity::Legacy)?,
        gamedir(root, Identity::WorldOfSurf)?,
    );
    ensure!(
        w == Dir::Game && !matches!(l, Dir::Game | Dir::Link),
        "the World of Surf move found ftesurf/ ({l:?}) and worldofsurf/ ({w:?}); it is journalled in {JOURNAL}; inspect both folders before starting"
    );
    Ok(())
}

/// Ledger keys as the worldofsurf install spells them, so its update reads unedited protected
/// files as unedited. The legacy exes leave the ledger; finish() removes the file. An existing
/// worldofsurf/ key wins over a respelled one.
fn respelled(files: HashMap<String, String>) -> HashMap<String, String> {
    let mut out = HashMap::new();
    let mut moved = Vec::new();
    for (k, v) in files {
        if Identity::Legacy.exes().contains(&k.as_str()) {
            continue;
        }
        match k.strip_prefix(Identity::Legacy.gamedir()) {
            Some(tail) => moved.push((format!("{}{tail}", Identity::WorldOfSurf.gamedir()), v)),
            None => {
                out.insert(k, v);
            }
        }
    }
    for (k, v) in moved {
        out.entry(k).or_insert(v);
    }
    out
}

fn split_word(s: &str) -> (&str, &str) {
    let s = s.trim_start_matches([' ', '\t']);
    match s.find([' ', '\t']) {
        Some(i) => (&s[..i], &s[i..]),
        None => (s, ""),
    }
}

/// `[set|seta] update_manifest_url "<legacy feed>" [// comment]`, as the engine archives it in
/// every legacy cfg. Under the new identity it would point the in-game updater at a feed it
/// refuses (QKU_ConfigureProfile keeps a saved value). Any other value or trailing text is kept.
fn is_legacy_feed_line(line: &[u8]) -> bool {
    let Ok(s) = std::str::from_utf8(line) else {
        return false;
    };
    let (first, rest) = split_word(s.trim_matches(|c: char| c.is_ascii_whitespace()));
    let (name, rest) = if first.eq_ignore_ascii_case("set") || first.eq_ignore_ascii_case("seta") {
        split_word(rest)
    } else {
        (first, rest)
    };
    if !name.eq_ignore_ascii_case("update_manifest_url") {
        return false;
    }
    let rest = rest.trim_start_matches([' ', '\t']);
    let (value, after) = match rest.strip_prefix('"') {
        Some(q) => match q.find('"') {
            Some(i) => (&q[..i], &q[i + 1..]),
            None => return false,
        },
        None => split_word(rest),
    };
    let after = after.trim_matches([' ', '\t']);
    value == u::FEED_URL && (after.is_empty() || after.starts_with("//"))
}

fn without_legacy_feed(text: &[u8]) -> Vec<u8> {
    text.split_inclusive(|&b| b == b'\n')
        .filter(|line| !is_legacy_feed_line(line))
        .flatten()
        .copied()
        .collect()
}

fn respell(root: &Path) -> Result<()> {
    park_exe(root)?; // done by the rename step; again here for an exe put back since
    let cfg = gamedir_path(root, Identity::WorldOfSurf).join(LEGACY_CFG);
    // A read-only cfg is the player's choice and stays as it is: a move that cannot finish would
    // block the game, and the line only affects the in-game updater (the launcher's is per identity).
    if regular_or_absent(&cfg)? && !fs::metadata(&cfg)?.permissions().readonly() {
        let text = fs::read(&cfg)?;
        let kept = without_legacy_feed(&text);
        if kept.len() != text.len() {
            u::replace_file(&cfg.with_file_name("ftesurf.cfg.move-tmp"), &cfg, &kept)?;
        }
    }
    if u::inspect(&meta(root, STATE)?)?.is_none() {
        return Ok(());
    }
    let mut st = u::load_state(root, Identity::WorldOfSurf)?;
    st.files = respelled(std::mem::take(&mut st.files));
    u::replace_meta(
        root,
        "state.json",
        &serde_json::to_vec_pretty(&st)?,
        Identity::WorldOfSurf,
    )
}

fn advance(root: &Path, mut step: Step, hook: &mut dyn FnMut(Point) -> Result<()>) -> Result<()> {
    if step < Step::Marked {
        ensure_barrier(root)?;
        hook(Point::Barrier)?;
    }
    if step == Step::Intent {
        rename(root)?;
        hook(Point::Done(Step::Renamed))?;
        step = Step::Renamed;
        record(root, step, hook)?;
    }
    if step == Step::Renamed {
        require_moved(root)?;
        respell(root)?;
        hook(Point::Done(Step::Ledger))?;
        step = Step::Ledger;
        record(root, step, hook)?;
    }
    if step == Step::Ledger {
        require_moved(root)?;
        u::record_identity(
            root,
            Identity::WorldOfSurf,
            Some(&MoveToken { _private: () }),
        )?;
        hook(Point::Done(Step::Marked))?;
        record(root, Step::Marked, hook)?;
    }
    Ok(())
}

/// Start the move under the install lock, after offer() verified a worldofsurf feed. Refusals
/// before the journal change nothing; a failed rename rolls the intent back.
pub fn start(lock: &InstallLock) -> Result<()> {
    start_with(lock, &mut |_| Ok(()))
}

pub fn start_with(lock: &InstallLock, hook: &mut dyn FnMut(Point) -> Result<()>) -> Result<()> {
    let root = lock.root();
    check_start(root)?;
    record(root, Step::Intent, hook)?;
    advance(root, Step::Intent, hook)
}

/// At every start, before recovery: carries an interrupted move to `marked` (or refuses).
/// True while a move is unfinished, i.e. until finish().
pub fn resume(lock: &InstallLock) -> Result<bool> {
    resume_with(lock, &mut |_| Ok(()))
}

pub fn resume_with(lock: &InstallLock, hook: &mut dyn FnMut(Point) -> Result<()>) -> Result<bool> {
    let Some(step) = read_journal(lock.root())? else {
        return Ok(false);
    };
    advance(lock.root(), step, hook)?;
    Ok(true)
}

/// The move still waits for its update: the game is blocked and only a verified install of the
/// worldofsurf feed lifts it.
pub fn awaits_update(root: &Path) -> Result<bool> {
    Ok(pending_barrier(root)?.is_some())
}

/// The player's cfg name under a worldofsurf default.fmf, which must hold exactly one GAME
/// "WorldofSurf", one BASEGAME "worldofsurf" and one MAINCONFIG naming ftesurf or worldofsurf
/// (the engine reads the last MAINCONFIG; any other name would start the player on a fresh cfg).
fn fmf_cfg(fmf: &[u8]) -> Result<&'static str> {
    let text = std::str::from_utf8(fmf).context("default.fmf is not UTF-8")?;
    let mut found: HashMap<String, Vec<String>> = HashMap::new();
    for line in text.lines() {
        let mut words = line.split("//").next().unwrap().split_ascii_whitespace();
        let Some(key) = words.next().map(str::to_ascii_lowercase) else {
            continue;
        };
        if !matches!(key.as_str(), "game" | "basegame" | "mainconfig") {
            continue;
        }
        let raw = words.next().unwrap_or("");
        let value = match raw.strip_prefix('"') {
            Some(q) => q
                .strip_suffix('"')
                .context("default.fmf holds an unpaired quote")?,
            None => raw,
        };
        ensure!(
            words.next().is_none(),
            "default.fmf's {key} line has extra words"
        );
        found.entry(key).or_default().push(value.to_string());
    }
    let one = |key: &str| match found.get(key).map(Vec::as_slice) {
        Some([v]) => Ok(v.as_str()),
        _ => Err(anyhow::anyhow!(
            "default.fmf must hold exactly one {key} line"
        )),
    };
    ensure!(
        one("game")? == "WorldofSurf" && one("basegame")? == "worldofsurf",
        "default.fmf must say GAME \"WorldofSurf\" and BASEGAME \"worldofsurf\""
    );
    match one("mainconfig")? {
        "ftesurf" | "ftesurf.cfg" => Ok(LEGACY_CFG),
        "worldofsurf" | "worldofsurf.cfg" => Ok(NEW_CFG),
        other => {
            bail!("default.fmf's MAINCONFIG {other:?} would leave the player's settings behind")
        }
    }
}

/// u::plan for the install as it will be once moved: each worldofsurf/ file is looked for at its
/// ftesurf/ path and in the ledger under its legacy key. What the move's update downloads,
/// staged before anything moves.
pub async fn plan_moved(m: &Manifest, root: &Path, old: &State) -> Result<Vec<FileEntry>> {
    let mut jobs = Vec::new();
    for f in m.files_for_platform() {
        let (rel, id) = match f.path.strip_prefix(Identity::WorldOfSurf.gamedir()) {
            Some(tail) => (
                format!("{}{tail}", Identity::Legacy.gamedir()),
                Identity::Legacy,
            ),
            None => (f.path.clone(), Identity::WorldOfSurf),
        };
        let here = FileEntry {
            path: rel.clone(),
            ..f.clone()
        };
        if u::matches(root, &here, id).await? {
            continue;
        }
        let p = u::checked_path(root, &rel, id)?;
        if u::protected(&f.path, Identity::WorldOfSurf) && p.exists() {
            let have = crate::hashing::hash_file(&p, "blake2b-256").await?;
            if f.path == format!("{}fs_addons.txt", Identity::WorldOfSurf.gamedir())
                || old.files.get(&rel) != Some(&have)
            {
                continue; // the player's edit, kept as plan() keeps it
            }
        }
        jobs.push(f.clone());
    }
    Ok(jobs)
}

/// The feed's signed default.fmf, from staging when the update brings it: checked before the
/// move starts and again before its update installs. Returns the player's cfg name.
pub async fn check_feed(
    root: &Path,
    staging: &Path,
    m: &Manifest,
    jobs: &[FileEntry],
) -> Result<&'static str> {
    let f = m
        .files_for_platform()
        .find(|f| f.path == "default.fmf")
        .context("the World of Surf feed has no default.fmf")?;
    let base = if jobs.iter().any(|j| j.path == f.path) {
        staging
    } else {
        root
    };
    ensure!(
        u::matches(base, f, Identity::WorldOfSurf).await?,
        "the World of Surf default.fmf does not match its signed hash"
    );
    fmf_cfg(&fs::read(u::checked_path(
        base,
        "default.fmf",
        Identity::WorldOfSurf,
    )?)?)
}

/// Before the move's update is installed, while BARRIER or that update's journal still blocks
/// the game: when the feed's checked default.fmf says `MAINCONFIG worldofsurf`, the player's
/// ftesurf.cfg becomes worldofsurf.cfg. An existing worldofsurf.cfg keeps both.
pub async fn before_install(
    lock: &InstallLock,
    staging: &Path,
    m: &Manifest,
    jobs: &[FileEntry],
) -> Result<()> {
    let root = lock.root();
    ensure!(
        read_journal(root)? == Some(Step::Marked),
        "no World of Surf move is waiting for its update"
    );
    if check_feed(root, staging, m, jobs).await? != NEW_CFG {
        return Ok(());
    }
    let dir = gamedir_path(root, Identity::WorldOfSurf);
    let (old, new) = (dir.join(LEGACY_CFG), dir.join(NEW_CFG));
    if u::inspect(&new)?.is_none() && regular_or_absent(&old)? {
        fs::rename(&old, &new)?;
        u::sync_dir(&dir)?;
    }
    Ok(())
}

/// Once the worldofsurf feed's install verifies (check_complete, run again here): drop BARRIER
/// if no update took it over, then the parked legacy exe (and one that reappeared in the root),
/// then the journal.
pub async fn finish(lock: &InstallLock, m: &Manifest) -> Result<()> {
    finish_with(lock, m, &mut |_| Ok(())).await
}

pub async fn finish_with(
    lock: &InstallLock,
    m: &Manifest,
    hook: &mut dyn FnMut(Point) -> Result<()>,
) -> Result<()> {
    let root = lock.root();
    ensure!(
        read_journal(root)? == Some(Step::Marked),
        "no World of Surf move is waiting to finish"
    );
    ensure!(
        u::read_marker(root)? == Some(Identity::WorldOfSurf),
        "the World of Surf move's marker is missing"
    );
    u::check_complete(root, m, Identity::WorldOfSurf).await?;
    if pending_barrier(root)?.is_some() {
        fs::remove_file(meta(root, PENDING)?)?;
    }
    ensure!(
        u::inspect(&meta(root, PENDING)?)?.is_none(),
        "an update is still pending after the World of Surf install verified"
    );
    hook(Point::BarrierRemoved)?;
    // A link at the old name is the player's; only the shipped file goes.
    for exe in [root.join(Identity::Legacy.exec()), parked_exe(root)?] {
        if u::inspect(&exe)?.is_some_and(|md| md.is_file() && !u::is_link(&md)) {
            fs::remove_file(&exe)
                .with_context(|| format!("could not remove the old {} (in use?)", exe.display()))?;
            u::sync_dir(exe.parent().unwrap())?;
        }
    }
    let _ = fs::remove_dir(meta(root, PARKED)?); // only when empty
    hook(Point::ExeRemoved)?;
    let journal = meta(root, JOURNAL)?;
    fs::remove_file(&journal)?;
    u::sync_dir(journal.parent().unwrap())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_exact_legacy_feed_line_is_dropped() {
        let url = u::FEED_URL;
        for line in [
            format!("update_manifest_url \"{url}\"\t//Where to look for the content manifest.\n"),
            format!("update_manifest_url \"{url}\"\r\n"),
            format!("  seta update_manifest_url {url}\n"),
            format!("SET Update_Manifest_URL \"{url}\""),
        ] {
            assert!(is_legacy_feed_line(line.as_bytes()), "{line:?}");
        }
        for line in [
            "update_manifest_url \"https://example.invalid/feed.json\"\n".to_string(),
            format!("update_manifest_url \"{url}x\"\n"),
            format!("update_manifest_url \"{url}\"; exec x.cfg\n"),
            format!("update_manifest_url \"{url}\n"),
            format!("// update_manifest_url \"{url}\"\n"),
            format!("update_manifest_urlx \"{url}\"\n"),
            format!("update_manifest_url \"{}\"\n", u::WOS_FEED_URL),
            "update_manifest_url\n".to_string(),
        ] {
            assert!(!is_legacy_feed_line(line.as_bytes()), "{line:?}");
        }
        let mut bytes = format!(
            "bind w +forward\r\nupdate_manifest_url \"{url}\"\t//desc\r\nsensitivity \"2\"\n"
        )
        .into_bytes();
        bytes.extend_from_slice(b"\xffraw\n\xfe tail");
        let kept = without_legacy_feed(&bytes);
        assert_eq!(
            kept,
            b"bind w +forward\r\nsensitivity \"2\"\n\xffraw\n\xfe tail"
        );
    }

    #[test]
    fn default_fmf_names_the_identity_and_one_known_cfg() {
        let head = "FTEMANIFEST 1\nGAME \"WorldofSurf\"\nNAME \"World of Surf\"\nBASEGAME \"worldofsurf\"\n";
        for (tail, want) in [
            ("MAINCONFIG worldofsurf\n", Some("worldofsurf.cfg")),
            (
                "MAINCONFIG \"worldofsurf\" // the player's settings\n",
                Some("worldofsurf.cfg"),
            ),
            ("mainconfig worldofsurf.cfg\n", Some("worldofsurf.cfg")),
            ("MAINCONFIG ftesurf\n", Some("ftesurf.cfg")),
            (
                "// MAINCONFIG worldofsurf\nMAINCONFIG ftesurf\n",
                Some("ftesurf.cfg"),
            ),
            ("", None),
            ("MAINCONFIG ftesurf\nMAINCONFIG worldofsurf\n", None),
            ("MAINCONFIG WorldofSurf\n", None),
            ("MAINCONFIG fte\n", None),
            ("MAINCONFIG ../worldofsurf\n", None),
            ("MAINCONFIG \"worldofsurf\n", None),
            ("MAINCONFIG worldofsurf extra\n", None),
            ("MAINCONFIG\n", None),
            ("MAINCONFIG ftesurf\nBASEGAME \"ftesurf\"\n", None),
            ("MAINCONFIG ftesurf\nGAME \"World of Surf\"\n", None),
        ] {
            let fmf = format!("{head}{tail}");
            assert_eq!(fmf_cfg(fmf.as_bytes()).ok(), want, "{fmf:?}");
        }
        for bad_head in [
            "GAME \"FTESurf\"\nBASEGAME \"worldofsurf\"\nMAINCONFIG ftesurf\n",
            "GAME \"WorldofSurf\"\nBASEGAME \"ftesurf\"\nMAINCONFIG ftesurf\n",
            "GAME \"WorldofSurf\"\nMAINCONFIG ftesurf\n",
        ] {
            assert!(fmf_cfg(bad_head.as_bytes()).is_err(), "{bad_head:?}");
        }
        assert!(fmf_cfg(b"GAME \"WorldofSurf\"\nBASEGAME worldofsurf\nMAINCONFIG \xff\n").is_err());
    }

    #[test]
    fn ledger_keys_follow_the_folder() {
        let files: HashMap<String, String> = [
            ("ftesurf/menu.dat", "a"),
            ("ftesurf/maps/zones/local/x.json", "b"),
            ("default.fmf", "c"),
            ("ftesurf64.exe", "d"),
            ("ftesurf64", "e"),
            ("fteplug_hl2_x64.dll", "f"),
            ("worldofsurf/menu.dat", "kept"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        let out = respelled(files);
        let want: HashMap<String, String> = [
            ("worldofsurf/menu.dat", "kept"),
            ("worldofsurf/maps/zones/local/x.json", "b"),
            ("default.fmf", "c"),
            ("fteplug_hl2_x64.dll", "f"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        assert_eq!(out, want);
        assert_eq!(respelled(out.clone()), out, "idempotent");
    }

    #[test]
    fn a_move_token_allows_only_the_forward_rewrite() {
        let root = std::env::temp_dir().join(format!("wos-token-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join(".ftesurf-updater")).unwrap();
        let token = MoveToken { _private: () };
        let marker = root.join(u::MARKER);
        fs::write(&marker, b"worldofsurf\n").unwrap();
        assert!(u::record_identity(&root, Identity::Legacy, Some(&token)).is_err());
        assert_eq!(fs::read(&marker).unwrap(), b"worldofsurf\n");
        fs::write(&marker, b"ftesurf\n").unwrap();
        assert!(u::record_identity(&root, Identity::WorldOfSurf, None).is_err());
        assert_eq!(fs::read(&marker).unwrap(), b"ftesurf\n");
        u::record_identity(&root, Identity::WorldOfSurf, Some(&token)).unwrap();
        assert_eq!(fs::read(&marker).unwrap(), b"worldofsurf\n");
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn journals_are_read_strictly() {
        let root = std::env::temp_dir().join(format!("wos-journal-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join(".ftesurf-updater")).unwrap();
        let p = root.join(JOURNAL);
        assert_eq!(read_journal(&root).unwrap(), None);
        for (text, step) in [
            (
                r#"{"schema":1,"from":"ftesurf","to":"worldofsurf","step":"intent"}"#,
                Step::Intent,
            ),
            (
                r#"{"schema":1,"from":"ftesurf","to":"worldofsurf","step":"marked"}"#,
                Step::Marked,
            ),
        ] {
            fs::write(&p, text).unwrap();
            assert_eq!(read_journal(&root).unwrap(), Some(step));
        }
        for bad in [
            r#"{"schema":1,"from":"ftesurf","to":"worldofsurf","step":"done"}"#,
            r#"{"schema":1,"from":"ftesurf","to":"worldofsurf","step":"Marked"}"#,
            r#"{"schema":2,"from":"ftesurf","to":"worldofsurf","step":"marked"}"#,
            r#"{"schema":1,"from":"worldofsurf","to":"ftesurf","step":"marked"}"#,
            r#"{"schema":1,"from":"ftesurf","to":"worldofsurf","step":"marked","x":1}"#,
            r#"{"schema":1,"from":"ftesurf","to":"worldofsurf"}"#,
            "",
        ] {
            fs::write(&p, bad).unwrap();
            assert!(read_journal(&root).is_err(), "{bad}");
        }
        fs::remove_file(&p).unwrap();
        fs::create_dir(&p).unwrap();
        assert!(read_journal(&root).is_err(), "a directory");
        fs::remove_dir_all(&root).unwrap();
    }
}
