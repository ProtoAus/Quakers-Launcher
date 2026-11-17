//! P1593 stage 2: the one-time move of a legacy install's `ftesurf/` to `worldofsurf/`, before
//! the worldofsurf feed's first update. The launcher stages and checks that feed first
//! (plan_moved, check_feed), so a failed download or a bad default.fmf moves nothing.
//! Journalled in JOURNAL; each step is recorded after its effect and every effect is idempotent,
//! so resume() finishes an interrupted move from any point:
//!   intent  - the approved feed staged as FEED, the journal (its sha256 and version), then
//!             BARRIER as pending.json: every FTESurf engine refuses to boot while pending.json
//!             exists (qkupdate.c QKU_FSLock); older launchers cannot parse it
//!   renamed - the legacy exes parked in PARKED, then ftesurf/ renamed to worldofsurf/
//!   ledger  - state.json keys respelled; the main cfg's saved legacy feed URL dropped
//!   marked  - the identity marker rewritten ftesurf -> worldofsurf (the commit point)
//! The launcher then installs exactly the approved feed from staging (approved_feed); begin()
//! swaps BARRIER for its own journal. finish() deletes the parked exes and the journal once that
//! install verifies. An intent without its barrier never started and is abandoned.
use crate::{
    ftesurf_update::{self as u, Identity, InstallLock},
    manifest::{FileEntry, Manifest},
    state::State,
};
use anyhow::{bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, HashMap},
    fs,
    path::{Path, PathBuf},
};

pub const JOURNAL: &str = ".ftesurf-updater/migrate.json";
/// The worldofsurf feed the player approved, byte for byte; the journal holds its sha256.
pub const FEED: &str = ".ftesurf-updater/move-feed.json";
const PENDING: &str = ".ftesurf-updater/pending.json";
const INGAME: &str = ".ftesurf-updater/ingame/journal.json";
const STATE: &str = ".ftesurf-updater/state.json";
/// pending.json while a move is unfinished. People read it; nothing parses it.
pub const BARRIER: &[u8] = b"{\"move\":\"ftesurf/ to worldofsurf/ is unfinished; run WOS-Launcher to finish it\",\"journal\":\".ftesurf-updater/migrate.json\"}\n";
/// The legacy MAINCONFIG's file, and the one name a worldofsurf default.fmf may switch it to.
const LEGACY_CFG: &str = "ftesurf.cfg";
const NEW_CFG: &str = "worldofsurf.cfg";
/// Where the legacy exes (both platforms', for a shared install) wait for finish(), as
/// `<exe>.parked`, not executable. An engine from before P1593 takes the install lock and honours
/// pending.json only under GAME "FTESurf" (patch-1588 qkupdate.c:2170), so left in the root it
/// would boot the new default.fmf unguarded.
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
    /// the legacy exes parked, ftesurf/ not yet renamed
    Parked,
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
    /// FEED's sha256 and version: resume installs that release and no other.
    feed_sha256: String,
    version: String,
    /// The ledger's hash of each legacy exe, taken before respelling drops them: finish()
    /// deletes only files that still match.
    legacy_exes: BTreeMap<String, String>,
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::Digest;
    hex::encode(sha2::Sha256::digest(bytes))
}

fn is_hash(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|c| matches!(c, b'0'..=b'9' | b'a'..=b'f'))
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
fn load_journal(root: &Path) -> Result<Option<Journal>> {
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
        j.schema == 1
            && j.from == "ftesurf"
            && j.to == "worldofsurf"
            && is_hash(&j.feed_sha256)
            && u::version(&j.version).is_ok()
            && j.legacy_exes.iter().all(|(name, hash)| {
                Identity::Legacy.exes().contains(&name.as_str()) && is_hash(hash)
            }),
        "unknown World of Surf move journal: {JOURNAL}"
    );
    Ok(Some(j))
}

pub fn read_journal(root: &Path) -> Result<Option<Step>> {
    Ok(load_journal(root)?.map(|j| j.step))
}

fn record(
    root: &Path,
    j: &mut Journal,
    step: Step,
    hook: &mut dyn FnMut(Point) -> Result<()>,
) -> Result<()> {
    j.step = step;
    u::replace_meta(
        root,
        "migrate.json",
        &serde_json::to_vec(j)?,
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
    let parked = meta(root, PARKED)?;
    ensure!(
        u::inspect(&parked)?.is_none_or(|md| md.is_dir() && !u::is_link(&md)),
        "{PARKED} is a link or not a folder; the move does not follow it (nothing was changed)"
    );
    for name in Identity::Legacy.exes() {
        regular_or_absent(&root.join(name))?;
        regular_or_absent(&parked_exe(root, name)?)?;
    }
    u::load_state(root, Identity::Legacy)?;
    Ok(())
}

fn parked_exe(root: &Path, name: &str) -> Result<PathBuf> {
    meta(root, &format!("{PARKED}/{name}.parked"))
}

/// Unix: no execute bit while parked; `on` restores it as prepare_install grants it.
fn set_exec(p: &Path, on: bool) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(p)?.permissions().mode();
        let want = if on {
            mode | 0o100 | ((mode & 0o444) >> 2)
        } else {
            mode & !0o111
        };
        if want != mode {
            fs::set_permissions(p, fs::Permissions::from_mode(want))?;
        }
    }
    #[cfg(not(unix))]
    let _ = (p, on);
    Ok(())
}

/// Idempotent: a root exe (re)appearing is parked over an older parked copy.
fn park_exes(root: &Path) -> Result<()> {
    for name in Identity::Legacy.exes() {
        let exe = root.join(name);
        if !regular_or_absent(&exe)? {
            continue;
        }
        let parked = parked_exe(root, name)?;
        fs::create_dir_all(parked.parent().unwrap())?;
        fs::rename(&exe, &parked)
            .with_context(|| format!("could not move the old {name} aside (in use?)"))?;
        set_exec(&parked, false)?;
        u::sync_dir(root)?;
    }
    Ok(())
}

/// Best effort, for a rollback that must not strand the intent: an exe still in the root (never
/// moved) is left alone without touching PARKED at all. Returns the exes left parked.
fn unpark_exes(root: &Path) -> Vec<&'static str> {
    let mut stuck = Vec::new();
    for name in Identity::Legacy.exes() {
        let exe = root.join(name);
        if !matches!(u::inspect(&exe), Ok(None)) {
            continue;
        }
        if let Ok(parked) = parked_exe(root, name) {
            if matches!(regular_or_absent(&parked), Ok(true)) {
                if fs::rename(&parked, &exe).is_ok() {
                    let _ = set_exec(&exe, true);
                    let _ = u::sync_dir(root);
                } else {
                    stuck.push(name);
                }
            }
        }
    }
    if let Ok(dir) = meta(root, PARKED) {
        let _ = fs::remove_dir(dir); // only when empty
    }
    stuck
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

/// Undo an intent whose rename did not happen: the install stays a working legacy one, and the
/// launcher asks again next time. A pending.json that is not BARRIER (an older launcher's
/// update) is left for recover().
fn roll_back(root: &Path) -> Result<()> {
    let stuck = unpark_exes(root);
    if pending_barrier(root)?.is_some() {
        fs::remove_file(meta(root, PENDING)?)?;
    }
    // An intent without its barrier is rolled back again at the next start, retrying these.
    ensure!(
        stuck.is_empty(),
        "could not put {} back from {PARKED}; nothing else changed, and the next start tries again",
        stuck.join(", ")
    );
    let journal = meta(root, JOURNAL)?;
    fs::remove_file(&journal)?;
    u::sync_dir(journal.parent().unwrap())?;
    if let Ok(feed) = meta(root, FEED) {
        let _ = fs::remove_file(feed);
    }
    Ok(())
}

/// Nothing has moved: ftesurf/ still holds the game and worldofsurf/ does not.
fn unmoved(root: &Path) -> Result<bool> {
    Ok(gamedir(root, Identity::Legacy)? == Dir::Game
        && matches!(
            gamedir(root, Identity::WorldOfSurf)?,
            Dir::Absent | Dir::Empty
        ))
}

/// A failed park or rename: rolled back when nothing moved.
fn not_moved(root: &Path, e: anyhow::Error) -> Result<()> {
    if unmoved(root)? {
        roll_back(root)?;
        return Err(e).context(
            "could not move ftesurf/ to worldofsurf/ (is a file in it in use?); nothing was changed: close the game and anything using its folder, then start again",
        );
    }
    Err(e).context(format!(
        "moving ftesurf/ to worldofsurf/ failed part-way; the move is journalled in {JOURNAL}"
    ))
}

fn rename(root: &Path, hook: &mut dyn FnMut(Point) -> Result<()>) -> Result<()> {
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
        (Dir::Absent | Dir::Empty | Dir::Other, Dir::Game) => return park_exes(root),
        (Dir::Game, Dir::Absent | Dir::Empty) => {}
        (l, w) => bail!(
            "cannot move ftesurf/ ({l:?}) to worldofsurf/ ({w:?}); the move is journalled in {JOURNAL}; inspect both folders before starting"
        ),
    }
    if let Err(e) = park_exes(root) {
        return not_moved(root, e);
    }
    hook(Point::Parked)?;
    // Windows cannot rename onto a folder, so an empty one goes first everywhere.
    let moved = match w {
        Dir::Empty => fs::remove_dir(&new).map_err(anyhow::Error::from),
        _ => Ok(()),
    }
    .and_then(|()| fs::rename(&old, &new).map_err(Into::into));
    if let Err(e) = moved {
        return not_moved(root, e);
    }
    u::sync_dir(root)
}

/// After the rename nothing may make the install half-moved again. A ftesurf/ without the game
/// (a late cfg save of an exiting engine) does not stop the move; legacy_leftovers reports it and
/// finish() waits for it.
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
    park_exes(root)?; // done by the rename step; again here for an exe put back since
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

fn advance(root: &Path, j: &mut Journal, hook: &mut dyn FnMut(Point) -> Result<()>) -> Result<()> {
    if j.step < Step::Marked {
        ensure_barrier(root)?;
        hook(Point::Barrier)?;
    }
    if j.step == Step::Intent {
        rename(root, hook)?;
        hook(Point::Done(Step::Renamed))?;
        record(root, j, Step::Renamed, hook)?;
    }
    if j.step == Step::Renamed {
        require_moved(root)?;
        respell(root)?;
        hook(Point::Done(Step::Ledger))?;
        record(root, j, Step::Ledger, hook)?;
    }
    if j.step == Step::Ledger {
        require_moved(root)?;
        u::record_identity(
            root,
            Identity::WorldOfSurf,
            Some(&MoveToken { _private: () }),
        )?;
        hook(Point::Done(Step::Marked))?;
        record(root, j, Step::Marked, hook)?;
    }
    Ok(())
}

/// Start the move under the install lock with the feed the player approved (offer() checks it
/// again here). Refusals before the journal change nothing; a failed rename rolls back.
pub fn start(
    lock: &InstallLock,
    envelope: &str,
    public_key: &str,
    allow_insecure: bool,
) -> Result<()> {
    start_with(lock, envelope, public_key, allow_insecure, &mut |_| Ok(()))
}

pub fn start_with(
    lock: &InstallLock,
    envelope: &str,
    public_key: &str,
    allow_insecure: bool,
    hook: &mut dyn FnMut(Point) -> Result<()>,
) -> Result<()> {
    let root = lock.root();
    check_start(root)?;
    let st = u::load_state(root, Identity::Legacy)?;
    let m = offer(envelope, public_key, allow_insecure, &st)?;
    u::replace_meta(
        root,
        "move-feed.json",
        envelope.as_bytes(),
        Identity::Legacy,
    )?;
    let mut j = Journal {
        schema: 1,
        from: "ftesurf".into(),
        to: "worldofsurf".into(),
        step: Step::Intent,
        feed_sha256: sha256_hex(envelope.as_bytes()),
        version: m.version,
        legacy_exes: Identity::Legacy
            .exes()
            .iter()
            .filter_map(|name| st.files.get(*name).map(|h| (name.to_string(), h.clone())))
            .collect(),
    };
    record(root, &mut j, Step::Intent, hook)?;
    advance(root, &mut j, hook)
}

/// At every start the player makes (never the in-game restart), before recovery: carries an
/// interrupted move to `marked`, abandons one that never started, or refuses. True while a move
/// is unfinished, i.e. until finish().
pub fn resume(lock: &InstallLock, public_key: &str, allow_insecure: bool) -> Result<bool> {
    resume_with(lock, public_key, allow_insecure, &mut |_| Ok(()))
}

pub fn resume_with(
    lock: &InstallLock,
    public_key: &str,
    allow_insecure: bool,
    hook: &mut dyn FnMut(Point) -> Result<()>,
) -> Result<bool> {
    let root = lock.root();
    let Some(mut j) = load_journal(root)? else {
        return Ok(false);
    };
    let barrier = pending_barrier(root)?.is_some();
    // Nothing moved yet: an intent without its barrier never started (the barrier follows the
    // intent record, the rename the barrier), and one whose approved feed is unusable cannot go
    // on. Both roll back; the launcher asks again.
    if j.step == Step::Intent
        && unmoved(root)?
        && (!barrier || approved_feed(root, public_key, allow_insecure).is_err())
    {
        roll_back(root)?;
        return Ok(false);
    }
    // The approved feed pins the move's update; once that is installed only clean-up is left.
    if j.step < Step::Marked || barrier {
        approved_feed(root, public_key, allow_insecure)?;
    }
    advance(root, &mut j, hook)?;
    Ok(true)
}

/// The feed the move approved, from FEED: its sha256 and version must be the journal's, so an
/// interrupted move installs that release (from staging, offline if its objects are there) and
/// never an older one.
pub fn approved_feed(
    root: &Path,
    public_key: &str,
    allow_insecure: bool,
) -> Result<(String, Manifest)> {
    let j = load_journal(root)?.context("no World of Surf move is journalled")?;
    let p = meta(root, FEED)?;
    ensure!(
        regular_or_absent(&p)? && fs::metadata(&p)?.len() <= u::MAX_FEED as u64,
        "the World of Surf move's approved feed is missing: {FEED}"
    );
    let text = fs::read_to_string(&p).context("the move's approved feed is not UTF-8")?;
    ensure!(
        sha256_hex(text.as_bytes()) == j.feed_sha256,
        "{FEED} is not the feed the World of Surf move approved"
    );
    let m = u::decode(&text, public_key, allow_insecure, Identity::WorldOfSurf)?;
    ensure!(
        m.version == j.version,
        "{FEED} is not the release the World of Surf move approved"
    );
    Ok((text, m))
}

/// What a ftesurf/ that came back after the rename holds (an exiting engine's late cfg save, or
/// the player), at most 20 entries, links unfollowed. The launcher reports it; finish() waits.
pub fn legacy_leftovers(root: &Path) -> Result<Vec<String>> {
    let dir = gamedir_path(root, Identity::Legacy);
    let Some(md) = u::inspect(&dir)? else {
        return Ok(Vec::new());
    };
    if u::is_link(&md) || !md.is_dir() {
        return Ok(vec!["ftesurf".into()]);
    }
    let (mut out, mut todo) = (Vec::new(), vec![dir.clone()]);
    while let Some(d) = todo.pop() {
        let unreadable = || format!("cannot look inside {}", d.display());
        for e in fs::read_dir(&d).with_context(unreadable)? {
            let p = e.with_context(unreadable)?.path();
            let md = fs::symlink_metadata(&p).with_context(unreadable)?;
            out.push(
                p.strip_prefix(root)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/"),
            );
            if out.len() >= 20 {
                return Ok(out);
            }
            if md.is_dir() && !u::is_link(&md) {
                todo.push(p);
            }
        }
    }
    out.sort();
    Ok(out)
}

/// The move still waits for its update: the game is blocked and only a verified install of the
/// worldofsurf feed lifts it.
pub fn awaits_update(root: &Path) -> Result<bool> {
    Ok(pending_barrier(root)?.is_some())
}

/// One default.fmf line as the engine's tokenizer reads it, under updatefeed.py check_manifest's
/// rules (no control character, block comment, backslash, empty or glued quote): a quote opens
/// after whitespace and closes before it, and a token starting `//` ends the line.
fn fmf_tokens(line: &str) -> Result<Vec<&str>> {
    // the whole line, comments too, as updatefeed checks it
    let (b, mut quoted) = (line.as_bytes(), false);
    for (i, &c) in b.iter().enumerate() {
        if c == b'"' {
            let edge = if quoted {
                b.get(i + 1)
            } else {
                i.checked_sub(1).map(|i| &b[i])
            };
            ensure!(
                edge.is_none_or(|e| matches!(e, b' ' | b'\t')),
                "default.fmf holds a quote glued to a word"
            );
            quoted = !quoted;
        }
    }
    ensure!(!quoted, "default.fmf holds an unpaired quote");
    let mut tokens = Vec::new();
    let mut rest = line;
    loop {
        rest = rest.trim_start_matches([' ', '\t']);
        if rest.is_empty() {
            return Ok(tokens);
        }
        if let Some(q) = rest.strip_prefix('"') {
            let end = q.find('"').context("default.fmf holds an unpaired quote")?;
            ensure!(
                q[end + 1..].is_empty() || q[end + 1..].starts_with([' ', '\t']),
                "default.fmf holds a quote glued to a word"
            );
            tokens.push(&q[..end]);
            rest = &q[end + 1..];
        } else {
            let end = rest.find([' ', '\t', '"']).unwrap_or(rest.len());
            ensure!(
                !rest[end..].starts_with('"'),
                "default.fmf holds a quote glued to a word"
            );
            if rest.starts_with("//") {
                return Ok(tokens);
            }
            tokens.push(&rest[..end]);
            rest = &rest[end..];
        }
    }
}

/// The player's cfg name under a worldofsurf default.fmf, which must hold exactly one GAME
/// "WorldofSurf", BASEGAME "worldofsurf", DISABLEHOMEDIR 1 (else the engine keeps the cfg in the
/// user's home) and MAINCONFIG naming ftesurf or worldofsurf (any other name would start the
/// player on a fresh cfg).
fn fmf_cfg(fmf: &[u8]) -> Result<&'static str> {
    let text = std::str::from_utf8(fmf).context("default.fmf is not UTF-8")?;
    ensure!(
        !text
            .chars()
            .any(|c| (c < ' ' && c != '\n' && c != '\t') || c == '\x7f')
            && !text.contains("/*"),
        "default.fmf holds a control character or a block comment"
    );
    ensure!(
        !text.contains('\\') && !text.contains("\"\""),
        "default.fmf holds a backslash or an empty quoted string"
    );
    let mut found: HashMap<String, Vec<String>> = HashMap::new();
    for line in text.split('\n') {
        let tokens = fmf_tokens(line)?;
        let Some(first) = tokens.first() else {
            continue;
        };
        ensure!(
            !first.starts_with('*'),
            "default.fmf holds a *-prefixed keyword"
        );
        let key = first.to_ascii_lowercase();
        if !matches!(
            key.as_str(),
            "game" | "basegame" | "mainconfig" | "disablehomedir"
        ) {
            continue;
        }
        ensure!(
            tokens.len() == 2,
            "default.fmf's {key} line must hold one value"
        );
        found.entry(key).or_default().push(tokens[1].to_string());
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
    ensure!(
        one("disablehomedir")? == "1",
        "default.fmf must say DISABLEHOMEDIR 1"
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
    // Only the move's own update: a later feed's default.fmf must never hold the clean-up.
    if pending_barrier(root)? != Some(Step::Marked) {
        return Ok(());
    }
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

/// Once the worldofsurf feed's install verifies (check_complete, run again here) and no
/// ftesurf/ came back (an empty one is removed): drop BARRIER if no update took it over, then
/// each legacy exe, parked or back in the root, that still matches the ledger hash the journal
/// took; then the journal. A legacy exe that does not match is left where it is.
pub async fn finish(lock: &InstallLock, m: &Manifest) -> Result<()> {
    finish_with(lock, m, &mut |_| Ok(())).await
}

/// A move with only its clean-up left (its update installed, no barrier) finishes from the
/// release installed.json verifies, before any fetch: no network, and no later feed involved.
/// Ok(false) when there is no such move.
pub async fn finish_installed(
    lock: &InstallLock,
    public_key: &str,
    allow_insecure: bool,
) -> Result<bool> {
    let root = lock.root();
    if read_journal(root)? != Some(Step::Marked) || awaits_update(root)? {
        return Ok(false);
    }
    let cached = meta(root, ".ftesurf-updater/installed.json")?;
    ensure!(
        regular_or_absent(&cached)?,
        "no verified World of Surf install is recorded yet"
    );
    let m = u::decode(
        &fs::read_to_string(&cached)?,
        public_key,
        allow_insecure,
        Identity::WorldOfSurf,
    )?;
    finish(lock, &m).await?;
    Ok(true)
}

pub async fn finish_with(
    lock: &InstallLock,
    m: &Manifest,
    hook: &mut dyn FnMut(Point) -> Result<()>,
) -> Result<()> {
    let root = lock.root();
    let j = load_journal(root)?.context("no World of Surf move is waiting to finish")?;
    ensure!(
        j.step == Step::Marked,
        "no World of Surf move is waiting to finish"
    );
    ensure!(
        u::read_marker(root)? == Some(Identity::WorldOfSurf),
        "the World of Surf move's marker is missing"
    );
    let left = legacy_leftovers(root)?;
    ensure!(
        left.is_empty(),
        "ftesurf/ came back after the move and holds {}; move what you want to keep into worldofsurf/ and delete ftesurf/",
        left.join(", ")
    );
    if gamedir(root, Identity::Legacy)? == Dir::Empty {
        fs::remove_dir(gamedir_path(root, Identity::Legacy))?;
    }
    u::check_complete(root, m, Identity::WorldOfSurf).await?;
    if pending_barrier(root)?.is_some() {
        fs::remove_file(meta(root, PENDING)?)?;
    }
    ensure!(
        u::inspect(&meta(root, PENDING)?)?.is_none(),
        "an update is still pending after the World of Surf install verified"
    );
    hook(Point::BarrierRemoved)?;
    for name in Identity::Legacy.exes() {
        let Some(want) = j.legacy_exes.get(name) else {
            continue;
        };
        for exe in [root.join(name), parked_exe(root, name)?] {
            // a link at either name is the player's; only the shipped file goes
            if u::inspect(&exe)?.is_some_and(|md| md.is_file() && !u::is_link(&md))
                && &crate::hashing::hash_file(&exe, "blake2b-256").await? == want
            {
                fs::remove_file(&exe).with_context(|| {
                    format!("could not remove the old {} (in use?)", exe.display())
                })?;
                u::sync_dir(exe.parent().unwrap())?;
            }
        }
    }
    let _ = fs::remove_dir(meta(root, PARKED)?); // only when empty
    hook(Point::ExeRemoved)?;
    let journal = meta(root, JOURNAL)?;
    fs::remove_file(&journal)?;
    u::sync_dir(journal.parent().unwrap())?;
    let _ = fs::remove_file(meta(root, FEED)?); // after the journal: a crash leaves only a stray copy
    Ok(())
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
        let head = "FTEMANIFEST 1\nGAME \"WorldofSurf\"\nNAME \"World of Surf\"\nBASEGAME \"worldofsurf\"\nDISABLEHOMEDIR 1\n";
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
            ("MAINCONFIG ftesurf\nGAME \"WorldofSurf\"\n", None), // the same GAME twice
            ("MAINCONFIG ftesurf\nDISABLEHOMEDIR 1\n", None),
            ("MAINCONFIG ftesurf\n*game x\n", None),
            ("MAINCONFIG ftesurf\n*\n", None),
            ("MAINCONFIG ftesurf /* worldofsurf */\n", None),
            // on a line nothing else reads, only the character checks refuse these
            ("MAINCONFIG ftesurf\nNAME x /* y */\n", None),
            ("MAINCONFIG ftesurf\nNAME x\r\n", None),
            ("MAINCONFIG ftesurf\nNAME x\x7f\n", None),
            ("MAINCONFIG ftesurf\r\n", None),
            ("MAINCONFIG ftesurf\n-set a\\b 1\n", None),
            ("MAINCONFIG ftesurf\nNAME \"\"\n", None),
            ("MAINCONFIG\"ftesurf\"\n", None),
            ("MAINCONFIG \"ftesurf\"x\n", None),
            ("MAINCONFIG ftesurf // don\"t\n", None),
            ("MAINCONFIG ftesurf // \"open\n", None),
            ("MAINCONFIG ftesurf // a\"b\"c\n", None),
            (
                "MAINCONFIG ftesurf\n-set url \"http://a/b\" // ok\n",
                Some("ftesurf.cfg"),
            ),
            ("MAINCONFIG ftesurf\nNAME \"a // b\"\n", Some("ftesurf.cfg")),
        ] {
            let fmf = format!("{head}{tail}");
            assert_eq!(fmf_cfg(fmf.as_bytes()).ok(), want, "{fmf:?}");
        }
        for bad_head in [
            "GAME \"FTESurf\"\nBASEGAME \"worldofsurf\"\nDISABLEHOMEDIR 1\nMAINCONFIG ftesurf\n",
            "GAME \"WorldofSurf\"\nBASEGAME \"ftesurf\"\nDISABLEHOMEDIR 1\nMAINCONFIG ftesurf\n",
            "GAME \"WorldofSurf\"\nDISABLEHOMEDIR 1\nMAINCONFIG ftesurf\n",
            "GAME \"WorldofSurf\"\nBASEGAME \"worldofsurf\"\nMAINCONFIG ftesurf\n",
            "GAME \"WorldofSurf\"\nBASEGAME \"worldofsurf\"\nDISABLEHOMEDIR 0\nMAINCONFIG ftesurf\n",
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
        let h = "ab".repeat(32);
        let journal = |schema: &str,
                       from: &str,
                       step: &str,
                       sha: &str,
                       version: &str,
                       exe: &str| {
            format!(
                r#"{{"schema":{schema},"from":"{from}","to":"worldofsurf","step":"{step}","feed_sha256":"{sha}","version":"{version}","legacy_exes":{{"{exe}":"{h}"}}}}"#
            )
        };
        for (step, want) in [("intent", Step::Intent), ("marked", Step::Marked)] {
            fs::write(&p, journal("1", "ftesurf", step, &h, "0.1.49", "ftesurf64")).unwrap();
            assert_eq!(read_journal(&root).unwrap(), Some(want));
        }
        for bad in [
            journal("1", "ftesurf", "done", &h, "0.1.49", "ftesurf64"),
            journal("1", "ftesurf", "Marked", &h, "0.1.49", "ftesurf64"),
            journal("2", "ftesurf", "marked", &h, "0.1.49", "ftesurf64"),
            journal("1", "worldofsurf", "marked", &h, "0.1.49", "ftesurf64"),
            journal("1", "ftesurf", "marked", &h[1..], "0.1.49", "ftesurf64"),
            journal(
                "1",
                "ftesurf",
                "marked",
                &h.to_uppercase(),
                "0.1.49",
                "ftesurf64",
            ),
            journal("1", "ftesurf", "marked", &h, "0.1", "ftesurf64"),
            journal("1", "ftesurf", "marked", &h, "0.1.49", "WorldofSurf.exe"),
            journal("1", "ftesurf", "marked", &h, "0.1.49", "ftesurf64")
                .replace("}}", r#"},"x":1}"#),
            r#"{"schema":1,"from":"ftesurf","to":"worldofsurf","step":"marked"}"#.to_string(),
            String::new(),
        ] {
            fs::write(&p, &bad).unwrap();
            assert!(read_journal(&root).is_err(), "{bad}");
        }
        fs::remove_file(&p).unwrap();
        fs::create_dir(&p).unwrap();
        assert!(read_journal(&root).is_err(), "a directory");
        fs::remove_dir_all(&root).unwrap();
    }
}
