//! P1593 stage 2: the folder move over real temp install trees, with feeds signed by the
//! RFC 8032 test key (never the release credential). Each test owns one directory under the
//! system temp dir and removes it.
use anyhow::{bail, Result};
use quakers_launcher::{
    ftesurf_migrate::{self as migrate, Point, Step},
    ftesurf_update::{self as u, Identity, InstallLock},
    hashing::Hasher,
};
use std::{
    collections::{BTreeMap, HashMap},
    fs,
    path::{Path, PathBuf},
};

const TEST_SEED: [u8; 32] = [
    0x9d, 0x61, 0xb1, 0x9d, 0xef, 0xfd, 0x5a, 0x60, 0xba, 0x84, 0x4a, 0xf4, 0x92, 0xec, 0x2c, 0xc4,
    0x44, 0x49, 0xc5, 0x69, 0x7b, 0x32, 0x69, 0x19, 0x70, 0x3b, 0xac, 0x03, 0x1c, 0xae, 0x7f, 0x60,
];
const OTHER_SEED: [u8; 32] = [7; 32];
const DATS: [(&str, &[u8]); 3] = [
    ("menu.dat", b"menu v1"),
    ("csprogs.dat", b"csprogs v1"),
    ("qwprogs.dat", b"qwprogs v1"),
];
const PLAIN_OLD: &[u8] = b"shipped zone v1";
const PLAIN_NEW: &[u8] = b"shipped zone v2";
const EDITED_SHIPPED: &[u8] = b"edited zone as shipped";
const EDITED_PLAYER: &[u8] = b"edited zone as the player left it";
const EDITED_NEW: &[u8] = b"edited zone v2";
const LEGACY_EXE: &[u8] = b"legacy engine";
const NEW_EXE: &[u8] = b"world of surf engine";
const LEGACY_FMF: &[u8] =
    b"FTEMANIFEST 1\nGAME \"FTESurf\"\nBASEGAME \"ftesurf\"\nMAINCONFIG ftesurf\n";
const CFG: &[u8] = b"bind w +forward\r\nupdate_manifest_url \"https://proto.bar/ftesurf/update.json\"\t//Where to look for the content manifest.\r\nsensitivity \"2.5\"\r\n";
const CFG_MOVED: &[u8] = b"bind w +forward\r\nsensitivity \"2.5\"\r\n";
const IDENTITY_FILES: [(&str, &[u8]); 3] = [
    ("fskey", b"fskey bytes"),
    ("qkey", b"qkey bytes"),
    ("identity.pfx", b"pfx bytes"),
];

fn blake(bytes: &[u8]) -> String {
    let mut h = Hasher::new("blake2b-256").unwrap();
    h.update(bytes);
    h.hex()
}

fn put(path: &Path, bytes: &[u8]) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, bytes).unwrap();
}

/// An installed engine as the launcher leaves it on unix (0755), so parking has a bit to clear.
fn executable(p: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(p, fs::Permissions::from_mode(0o755)).unwrap();
    }
    #[cfg(not(unix))]
    let _ = p;
}

fn dir_link(target: &Path, link: &Path) {
    #[cfg(windows)]
    assert!(std::process::Command::new("cmd")
        .args(["/C", "mklink", "/J"])
        .arg(link)
        .arg(target)
        .output()
        .unwrap()
        .status
        .success());
    #[cfg(not(windows))]
    std::os::unix::fs::symlink(target, link).unwrap();
}

fn other_exe(id: Identity) -> &'static str {
    // the exe of the platform this test is not running on
    match (id, cfg!(windows)) {
        (Identity::Legacy, true) => "ftesurf64",
        (Identity::Legacy, false) => "ftesurf64.exe",
        (Identity::WorldOfSurf, true) => "worldofsurf64",
        (Identity::WorldOfSurf, false) => "WorldofSurf.exe",
    }
}

struct Install {
    root: PathBuf,
}

impl Install {
    fn empty(name: &str) -> Self {
        let root = std::env::temp_dir().join(format!("wos-move-{name}-{}", std::process::id()));
        if root.exists() {
            fs::remove_dir_all(&root).unwrap();
        }
        fs::create_dir_all(&root).unwrap();
        Self {
            root: root.canonicalize().unwrap(),
        }
    }

    /// A verified 0.1.48 legacy install as the stage-1 launcher leaves it.
    fn legacy(name: &str, marker: bool) -> Self {
        let i = Self::empty(name);
        let r = &i.root;
        for (leaf, bytes) in DATS {
            put(&r.join("ftesurf").join(leaf), bytes);
        }
        put(&r.join("ftesurf/maps/zones/local/plain.json"), PLAIN_OLD);
        put(
            &r.join("ftesurf/maps/zones/local/edited.json"),
            EDITED_PLAYER,
        );
        put(&r.join("ftesurf/ftesurf.cfg"), CFG);
        put(&r.join("ftesurf/data/runs/surf_a.rec"), b"a recorded run");
        put(&r.join("ftesurf/maps/surf_a.bsp"), b"a downloaded map");
        put(&r.join(Identity::Legacy.exec()), LEGACY_EXE);
        executable(&r.join(Identity::Legacy.exec()));
        put(&r.join("default.fmf"), LEGACY_FMF);
        for (name, bytes) in IDENTITY_FILES {
            put(&r.join(name), bytes);
        }
        let mut files = serde_json::Map::new();
        for (leaf, bytes) in DATS {
            files.insert(format!("ftesurf/{leaf}"), blake(bytes).into());
        }
        files.insert(
            "ftesurf/maps/zones/local/plain.json".into(),
            blake(PLAIN_OLD).into(),
        );
        files.insert(
            "ftesurf/maps/zones/local/edited.json".into(),
            blake(EDITED_SHIPPED).into(),
        );
        files.insert(Identity::Legacy.exec().into(), blake(LEGACY_EXE).into());
        files.insert("default.fmf".into(), blake(LEGACY_FMF).into());
        put(
            &r.join(".ftesurf-updater/state.json"),
            &serde_json::to_vec(&serde_json::json!({"version": "0.1.48", "files": files})).unwrap(),
        );
        put(&r.join(".ftesurf-updater/consent"), b"1\n");
        if marker {
            put(&r.join(u::MARKER), b"ftesurf\n");
        }
        i
    }

    fn lock(&self) -> InstallLock {
        InstallLock::try_acquire(&self.root, u::install_identity(&self.root).unwrap()).unwrap()
    }

    fn read(&self, rel: &str) -> Option<Vec<u8>> {
        fs::read(self.root.join(rel)).ok()
    }

    fn has_barrier(&self) -> bool {
        self.read(".ftesurf-updater/pending.json").as_deref() == Some(migrate::BARRIER)
    }

    /// Every path below the root: a link is recorded, never followed. state.json is compared
    /// parsed (its key order is a hash map's).
    fn snapshot(&self) -> BTreeMap<String, String> {
        fn walk(base: &Path, dir: &Path, out: &mut BTreeMap<String, String>) {
            for e in fs::read_dir(dir).unwrap() {
                let p = e.unwrap().path();
                let rel = p
                    .strip_prefix(base)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/");
                let md = fs::symlink_metadata(&p).unwrap();
                if md.file_type().is_symlink() || is_reparse(&md) {
                    out.insert(rel, "link".into());
                } else if md.is_dir() {
                    out.insert(rel.clone(), "dir".into());
                    walk(base, &p, out);
                } else if rel == ".ftesurf-updater/install.lock" {
                    out.insert(rel, "lock".into()); // held: Windows refuses the read
                } else if rel == ".ftesurf-updater/state.json" {
                    let v: serde_json::Value = serde_json::from_slice(&fs::read(&p).unwrap())
                        .unwrap_or(serde_json::Value::Null);
                    out.insert(rel, v.to_string());
                } else {
                    out.insert(rel, blake(&fs::read(&p).unwrap()));
                }
            }
        }
        let mut out = BTreeMap::new();
        walk(&self.root, &self.root, &mut out);
        out
    }
}

impl Drop for Install {
    fn drop(&mut self) {
        if !std::thread::panicking() {
            fs::remove_dir_all(&self.root).unwrap();
        }
    }
}

fn is_reparse(md: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        md.file_attributes() & 0x400 != 0
    }
    #[cfg(not(windows))]
    {
        let _ = md;
        false
    }
}

struct Feed {
    envelope: String,
    key: String,
    contents: HashMap<String, Vec<u8>>,
}

fn feed(id: Identity, version: &str, mainconfig: &str, seed: [u8; 32]) -> Feed {
    use ed25519_dalek::{Signer, SigningKey};
    let dir = id.gamedir();
    let fmf = match id {
        Identity::Legacy => LEGACY_FMF.to_vec(),
        Identity::WorldOfSurf => format!(
            "FTEMANIFEST 1\nGAME \"WorldofSurf\"\nBASEGAME \"worldofsurf\"\nDISABLEHOMEDIR 1\nMAINCONFIG {mainconfig}\n"
        )
        .into_bytes(),
    };
    let (exe, exe_bytes): (&str, &[u8]) = match id {
        Identity::Legacy => (id.exec(), b"legacy engine v2"),
        Identity::WorldOfSurf => (id.exec(), NEW_EXE),
    };
    let mut contents: Vec<(String, Vec<u8>, &str, bool)> = vec![
        (
            exe.into(),
            exe_bytes.to_vec(),
            quakers_launcher::manifest::platform_key(),
            true,
        ),
        (
            other_exe(id).into(),
            b"other platform".to_vec(),
            if cfg!(windows) { "linux64" } else { "win64" },
            true,
        ),
        ("default.fmf".into(), fmf, "all", false),
        (
            format!("{dir}maps/zones/local/plain.json"),
            PLAIN_NEW.to_vec(),
            "all",
            false,
        ),
        (
            format!("{dir}maps/zones/local/edited.json"),
            EDITED_NEW.to_vec(),
            "all",
            false,
        ),
    ];
    for (leaf, bytes) in DATS {
        contents.push((format!("{dir}{leaf}"), bytes.to_vec(), "all", false));
    }
    let files: Vec<_> = contents
        .iter()
        .map(|(path, bytes, platform, exec)| {
            serde_json::json!({"path": path, "hash": blake(bytes), "size": bytes.len(),
                "platform": platform, "exec": exec})
        })
        .collect();
    let (win, linux) = if cfg!(windows) {
        (exe, other_exe(id))
    } else {
        (other_exe(id), exe)
    };
    let payload = serde_json::json!({
        "schema": 2, "hash_algo": "blake2b-256", "game": id.game(), "channel": "stable",
        "version": version, "object_layout": "objects/{h0h1}/{hash}",
        "mirrors": ["https://dl.proto.bar/ftesurf/update"], "files": files, "removed": [],
        "exec": {"cmd": win, "args": ["-manifest", "default.fmf"]},
        "exec_by_platform": {
            "win64": {"cmd": win, "args": ["-manifest", "default.fmf"]},
            "linux64": {"cmd": linux, "args": ["-manifest", "default.fmf"]},
        },
    })
    .to_string();
    let key = SigningKey::from_bytes(&seed);
    Feed {
        envelope: serde_json::json!({
            "payload": payload,
            "signature": hex::encode(key.sign(payload.as_bytes()).to_bytes()),
        })
        .to_string(),
        key: hex::encode(
            SigningKey::from_bytes(&TEST_SEED)
                .verifying_key()
                .to_bytes(),
        ),
        contents: contents.into_iter().map(|(p, b, _, _)| (p, b)).collect(),
    }
}

fn wos(mainconfig: &str) -> Feed {
    feed(Identity::WorldOfSurf, "0.1.49", mainconfig, TEST_SEED)
}

fn stop_at(stop: Point) -> impl FnMut(Point) -> Result<()> {
    move |p| {
        if p == stop {
            bail!("simulated crash at {p:?}")
        }
        Ok(())
    }
}

#[derive(PartialEq)]
enum Stop {
    No,
    AfterBegin,
    InFinish(Point),
}

fn key() -> String {
    wos("ftesurf").key
}

/// migrate::start with the default feed, as the launcher calls it after the player approved it.
fn start(lock: &InstallLock) -> Result<()> {
    start_feed(lock, &wos("ftesurf"))
}

fn start_feed(lock: &InstallLock, f: &Feed) -> Result<()> {
    migrate::start(lock, &f.envelope, &f.key, false)
}

fn start_with(lock: &InstallLock, hook: &mut dyn FnMut(Point) -> Result<()>) -> Result<()> {
    let f = wos("ftesurf");
    migrate::start_with(lock, &f.envelope, &f.key, false, hook)
}

fn resume(lock: &InstallLock) -> Result<bool> {
    migrate::resume(lock, &key(), false)
}

/// The launcher's update path (ftesurf.rs run() and install_feed): the move's own update (its
/// barrier up) installs the approved feed from FEED with before_install and finish; any other
/// update is the fetched feed alone. Plan, stage what the download would, begin + recover (or
/// adopt), check_complete, establish_identity.
async fn update(lock: &InstallLock, f: &Feed, id: Identity, stop: Stop) -> Result<()> {
    let root = lock.root();
    let moving = migrate::read_journal(root)?.is_some() && migrate::awaits_update(root)?;
    let (envelope, m) = if moving {
        migrate::approved_feed(root, &f.key, false)?
    } else {
        (
            f.envelope.clone(),
            u::decode(&f.envelope, &f.key, false, id)?,
        )
    };
    let f = &Feed {
        envelope,
        key: f.key.clone(),
        contents: f.contents.clone(),
    };
    let old = u::load_state(root, id)?;
    let (jobs, _kept) = u::plan(&m, root, &old, id).await?;
    let staging = root.join(".ftesurf-updater/download");
    if !jobs.is_empty() {
        for j in &jobs {
            if !u::matches(&staging, j, id).await? {
                put(&staging.join(&j.path), &f.contents[&j.path]);
            }
        }
        if moving {
            migrate::before_install(lock, &staging, &m, &jobs).await?;
        }
        u::begin(root, &f.envelope, &jobs, id)?;
        if stop == Stop::AfterBegin {
            bail!("simulated crash after begin");
        }
        u::recover_with_key(root, &f.key, false, id).await?;
    } else {
        if moving {
            migrate::before_install(lock, &staging, &m, &jobs).await?;
        }
        u::prepare_install(root, &m, id).await?;
        let mut st = old;
        for file in m.files_for_platform() {
            if u::matches(root, file, id).await? {
                st.files.insert(file.path.clone(), file.hash.clone());
            }
        }
        st.version = m.version.clone();
        u::write_meta(root, "state.json", &serde_json::to_vec_pretty(&st)?, id)?;
        u::write_meta(root, "installed.json", f.envelope.as_bytes(), id)?;
    }
    u::check_complete(root, &m, id).await?;
    if moving {
        match stop {
            Stop::InFinish(p) => migrate::finish_with(lock, &m, &mut stop_at(p)).await?,
            _ => migrate::finish(lock, &m).await?,
        }
    }
    u::establish_identity(root, id)
}

/// The launcher's stage_move (ftesurf.rs): preconditions, the download of what the moved install
/// will lack into staging, the feed's default.fmf, then the move. Returns what was staged.
async fn move_install(lock: &InstallLock, f: &Feed) -> Result<Vec<String>> {
    let root = lock.root();
    let st = u::load_state(root, Identity::Legacy)?;
    let m = migrate::offer(&f.envelope, &f.key, false, &st)?;
    migrate::check_start(root)?;
    let jobs = migrate::plan_moved(&m, root, &st).await?;
    let staging = root.join(".ftesurf-updater/download");
    for j in &jobs {
        put(&staging.join(&j.path), &f.contents[&j.path]);
    }
    migrate::check_feed(root, &staging, &m, &jobs).await?;
    migrate::start(lock, &f.envelope, &f.key, false)?;
    let mut staged: Vec<_> = jobs.into_iter().map(|j| j.path).collect();
    staged.sort();
    Ok(staged)
}

/// The next launcher start the player makes: resume (or, for an intent that never started, the
/// launcher asks again and starts), recover, then the update. True when it resumed.
async fn next_start(i: &Install, f: &Feed) -> Result<bool> {
    let lock = i.lock();
    let resumed = resume(&lock)?;
    if !resumed {
        assert_eq!(u::install_identity(&i.root)?, Identity::Legacy);
        start_feed(&lock, f)?;
    }
    let id = u::install_identity(&i.root)?;
    assert_eq!(id, Identity::WorldOfSurf);
    u::recover_with_key(&i.root, &f.key, false, id).await?;
    // run(): clean-up first when only that is left, else the move's own update
    if !migrate::finish_installed(&lock, &f.key, false).await?
        && migrate::read_journal(&i.root)?.is_some()
    {
        update(&lock, f, id, Stop::No).await?;
    }
    Ok(resumed)
}

fn assert_moved(i: &Install, cfg_name: &str) {
    let r = &i.root;
    assert!(!r.join("ftesurf").exists(), "ftesurf/ is gone");
    for (leaf, bytes) in DATS {
        assert_eq!(i.read(&format!("worldofsurf/{leaf}")).unwrap(), bytes);
    }
    assert_eq!(
        i.read("worldofsurf/data/runs/surf_a.rec").unwrap(),
        b"a recorded run"
    );
    assert_eq!(
        i.read("worldofsurf/maps/surf_a.bsp").unwrap(),
        b"a downloaded map"
    );
    assert_eq!(
        i.read(&format!("worldofsurf/{cfg_name}")).unwrap(),
        CFG_MOVED
    );
    // an unedited protected file follows the feed; the player's edit is kept
    assert_eq!(
        i.read("worldofsurf/maps/zones/local/plain.json").unwrap(),
        PLAIN_NEW
    );
    assert_eq!(
        i.read("worldofsurf/maps/zones/local/edited.json").unwrap(),
        EDITED_PLAYER
    );
    assert_eq!(i.read(Identity::WorldOfSurf.exec()).unwrap(), NEW_EXE);
    assert!(
        i.read(Identity::Legacy.exec()).is_none(),
        "the legacy exe is removed"
    );
    assert!(
        !r.join(".ftesurf-updater/legacy").exists(),
        "and its parked copy"
    );
    for (name, bytes) in IDENTITY_FILES {
        assert_eq!(i.read(name).unwrap(), bytes, "{name} untouched");
    }
    assert_eq!(i.read(u::MARKER).unwrap(), b"worldofsurf\n");
    assert!(i.read(".ftesurf-updater/pending.json").is_none());
    assert!(i.read(migrate::JOURNAL).is_none());
    assert!(
        i.read(migrate::FEED).is_none(),
        "the approved feed goes with the journal"
    );
    let st = u::load_state(r, Identity::WorldOfSurf).unwrap();
    assert_eq!(st.version, "0.1.49");
    assert!(
        st.files.keys().all(|k| !k.starts_with("ftesurf/")),
        "{:?}",
        st.files
    );
    assert!(!st.files.contains_key(Identity::Legacy.exec()));
    assert_eq!(
        st.files.get("worldofsurf/maps/zones/local/edited.json"),
        Some(&blake(EDITED_SHIPPED)),
        "the kept edit stays recorded as the shipped bytes, so it stays a local edit"
    );
    assert_eq!(u::install_identity(r).unwrap(), Identity::WorldOfSurf);
}

#[tokio::test]
async fn a_legacy_install_moves_then_installs_the_worldofsurf_feed() {
    for marker in [true, false] {
        let i = Install::legacy(&format!("ok-{marker}"), marker);
        let f = wos("ftesurf");
        assert!(migrate::eligible(&i.root).unwrap());
        let lock = i.lock();
        let staged = move_install(&lock, &f).await.unwrap();
        assert!(
            i.has_barrier(),
            "the game stays blocked until the update installs"
        );
        assert_eq!(migrate::read_journal(&i.root).unwrap(), Some(Step::Marked));
        assert!(
            i.read(Identity::Legacy.exec()).is_none(),
            "no pre-P1593 engine is left to boot the new default.fmf unguarded"
        );
        // what was staged before the move is exactly what the moved install needs
        let m = u::decode(&f.envelope, &f.key, false, Identity::WorldOfSurf).unwrap();
        let st = u::load_state(&i.root, Identity::WorldOfSurf).unwrap();
        let (jobs, kept) = u::plan(&m, &i.root, &st, Identity::WorldOfSurf)
            .await
            .unwrap();
        let mut planned: Vec<_> = jobs.into_iter().map(|j| j.path).collect();
        planned.sort();
        assert_eq!(staged, planned);
        assert!(planned.contains(&"worldofsurf/maps/zones/local/plain.json".to_string()));
        assert_eq!(kept, ["worldofsurf/maps/zones/local/edited.json"]);
        update(&lock, &f, Identity::WorldOfSurf, Stop::No)
            .await
            .unwrap();
        assert_moved(&i, "ftesurf.cfg");
        assert!(!migrate::eligible(&i.root).unwrap(), "moved once");
    }
}

#[tokio::test]
async fn every_crash_point_resumes_to_the_same_install() {
    let f = wos("ftesurf");
    let reference = {
        let i = Install::legacy("ref", true);
        let lock = i.lock();
        start(&lock).unwrap();
        update(&lock, &f, Identity::WorldOfSurf, Stop::No)
            .await
            .unwrap();
        drop(lock);
        i.snapshot()
    };
    let points = [
        Point::Recorded(Step::Intent),
        Point::Barrier,
        Point::Parked,
        Point::Done(Step::Renamed),
        Point::Recorded(Step::Renamed),
        Point::Done(Step::Ledger),
        Point::Recorded(Step::Ledger),
        Point::Done(Step::Marked),
        Point::Recorded(Step::Marked),
    ];
    for (n, stop) in points.into_iter().enumerate() {
        let i = Install::legacy(&format!("crash{n}"), true);
        {
            let lock = i.lock();
            let e = start_with(&lock, &mut stop_at(stop)).unwrap_err();
            assert!(e.to_string().contains("simulated crash"), "{stop:?}: {e:#}");
        }
        // never startable as if complete: the barrier blocks every engine, or nothing moved yet
        let untouched =
            i.root.join("ftesurf/menu.dat").is_file() && !i.root.join("worldofsurf").exists();
        assert!(
            i.has_barrier() || (stop == Point::Recorded(Step::Intent) && untouched),
            "{stop:?}"
        );
        assert!(
            untouched || i.read(Identity::Legacy.exec()).is_none(),
            "{stop:?}: the legacy exe leaves the root with the folder"
        );
        // only an intent without its barrier never started; the launcher then asks again
        assert_eq!(
            next_start(&i, &f).await.unwrap(),
            stop != Point::Recorded(Step::Intent),
            "{stop:?}"
        );
        assert_eq!(i.snapshot(), reference, "{stop:?}");
        assert_moved(&i, "ftesurf.cfg");
    }
    for (n, stop) in [
        Stop::AfterBegin,
        Stop::InFinish(Point::BarrierRemoved),
        Stop::InFinish(Point::ExeRemoved),
    ]
    .into_iter()
    .enumerate()
    {
        let i = Install::legacy(&format!("crash-update{n}"), true);
        {
            let lock = i.lock();
            start(&lock).unwrap();
            assert!(update(&lock, &f, Identity::WorldOfSurf, stop)
                .await
                .is_err());
        }
        assert!(next_start(&i, &f).await.unwrap());
        assert_eq!(i.snapshot(), reference, "update crash {n}");
    }
}

#[tokio::test]
async fn a_rename_interrupted_before_its_record_is_recognised() {
    // the rename happened, the journal still says intent, and the barrier has gone since: the
    // move is carried forward, never abandoned (only an unmoved intent is)
    let i = Install::legacy("gap", true);
    {
        let lock = i.lock();
        assert!(start_with(&lock, &mut stop_at(Point::Done(Step::Renamed))).is_err());
    }
    fs::remove_file(i.root.join(".ftesurf-updater/pending.json")).unwrap();
    assert_eq!(migrate::read_journal(&i.root).unwrap(), Some(Step::Intent));
    assert!(next_start(&i, &wos("ftesurf")).await.unwrap(), "resumed");
    assert_moved(&i, "ftesurf.cfg");
}

#[tokio::test]
async fn refusals_change_nothing() {
    type Setup = fn(&Path);
    let cases: Vec<(&str, Setup)> = vec![
        ("both gamedirs", |r| {
            for (leaf, bytes) in DATS {
                put(&r.join("worldofsurf").join(leaf), bytes);
            }
        }),
        ("worldofsurf/ holds files", |r| {
            put(&r.join("worldofsurf/notes.txt"), b"x")
        }),
        ("worldofsurf is a file", |r| {
            put(&r.join("worldofsurf"), b"x")
        }),
        ("worldofsurf/ is a link", |r| {
            fs::create_dir(r.join("elsewhere")).unwrap();
            dir_link(&r.join("elsewhere"), &r.join("worldofsurf"));
        }),
        ("ftesurf/ is a link", |r| {
            fs::rename(r.join("ftesurf"), r.join("elsewhere")).unwrap();
            dir_link(&r.join("elsewhere"), &r.join("ftesurf"));
        }),
        ("the cfg is a link", |r| {
            fs::remove_file(r.join("ftesurf/ftesurf.cfg")).unwrap();
            fs::create_dir(r.join("elsewhere")).unwrap();
            dir_link(&r.join("elsewhere"), &r.join("ftesurf/ftesurf.cfg"));
        }),
        ("the legacy exe is a link", |r| {
            fs::remove_file(r.join(Identity::Legacy.exec())).unwrap();
            fs::create_dir(r.join("elsewhere")).unwrap();
            dir_link(&r.join("elsewhere"), &r.join(Identity::Legacy.exec()));
        }),
        ("the parked folder is a link", |r| {
            fs::create_dir(r.join("elsewhere")).unwrap();
            dir_link(&r.join("elsewhere"), &r.join(".ftesurf-updater/legacy"));
        }),
        ("the parked folder is a file", |r| {
            put(&r.join(".ftesurf-updater/legacy"), b"x")
        }),
        ("an unfinished update", |r| {
            put(&r.join(".ftesurf-updater/pending.json"), b"{}")
        }),
        ("an unfinished in-game update", |r| {
            put(&r.join(".ftesurf-updater/ingame/journal.json"), b"{}")
        }),
        ("a damaged ledger", |r| {
            put(&r.join(".ftesurf-updater/state.json"), b"{")
        }),
        ("already worldofsurf", |r| {
            put(&r.join(u::MARKER), b"worldofsurf\n")
        }),
    ];
    for (n, (name, setup)) in cases.into_iter().enumerate() {
        let i = Install::legacy(&format!("refuse{n}"), true);
        setup(&i.root);
        let lock = i.lock();
        let before = i.snapshot();
        // the launcher asks check_start before downloading anything
        assert!(
            migrate::check_start(&i.root).is_err(),
            "{name}: refused before the download"
        );
        assert!(start(&lock).is_err(), "{name}");
        assert_eq!(i.snapshot(), before, "{name}");
    }
}

#[test]
fn older_launchers_cannot_take_the_barrier_for_an_update() {
    // the Journal every launcher before stage 2 parses pending.json as (ftesurf_update.rs)
    #[derive(serde::Deserialize)]
    #[allow(dead_code)]
    struct Journal {
        envelope: String,
        paths: Vec<String>,
    }
    assert!(serde_json::from_slice::<Journal>(migrate::BARRIER).is_err());
    serde_json::from_slice::<serde_json::Value>(migrate::BARRIER).unwrap();
}

#[test]
fn a_held_lock_refuses_a_second_holder() {
    let i = Install::legacy("lock", true);
    let lock = i.lock();
    assert!(InstallLock::try_acquire(&i.root, Identity::Legacy).is_err());
    drop(lock);
    assert!(InstallLock::try_acquire(&i.root, Identity::Legacy).is_ok());
}

/// A running game holds files in its folder open as the C runtime opens them (no delete
/// sharing); Windows then refuses the rename and the intent is rolled back.
#[cfg(windows)]
#[test]
fn an_open_game_file_blocks_the_rename_and_changes_nothing() {
    use std::os::windows::fs::OpenOptionsExt;
    let i = Install::legacy("inuse", true);
    put(&i.root.join("ftesurf/logs/qconsole.log"), b"log");
    let lock = i.lock();
    let before = i.snapshot();
    let open = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .share_mode(0x1 | 0x2) // FILE_SHARE_READ | FILE_SHARE_WRITE
        .open(i.root.join("ftesurf/logs/qconsole.log"))
        .unwrap();
    let e = start(&lock).unwrap_err();
    assert!(format!("{e:#}").contains("nothing was changed"), "{e:#}");
    drop(open);
    assert_eq!(i.snapshot(), before);
    start(&lock).unwrap();
}

#[test]
fn case_of_the_new_folder_follows_the_filesystem() {
    let i = Install::legacy("case", true);
    put(&i.root.join("WorldofSurf/notes.txt"), b"x");
    let lock = i.lock();
    // one folder on Windows, a different one on Linux
    assert_eq!(start(&lock).is_err(), cfg!(windows));
}

#[tokio::test]
async fn an_empty_worldofsurf_folder_is_replaced() {
    let i = Install::legacy("emptynew", true);
    fs::create_dir(i.root.join("worldofsurf")).unwrap();
    let lock = i.lock();
    start(&lock).unwrap();
    update(&lock, &wos("ftesurf"), Identity::WorldOfSurf, Stop::No)
        .await
        .unwrap();
    assert_moved(&i, "ftesurf.cfg");
}

#[tokio::test]
async fn interrupted_moves_that_no_longer_fit_are_refused() {
    let f = wos("ftesurf");
    // ftesurf/ came back with game files after the rename
    let i = Install::legacy("reappear", true);
    {
        let lock = i.lock();
        assert!(start_with(&lock, &mut stop_at(Point::Recorded(Step::Renamed))).is_err());
    }
    for (leaf, bytes) in DATS {
        put(&i.root.join("ftesurf").join(leaf), bytes);
    }
    let lock = i.lock();
    assert!(resume(&lock).is_err());
    assert_eq!(
        i.read(u::MARKER).unwrap(),
        b"ftesurf\n",
        "no marker rewrite"
    );
    assert!(i.has_barrier(), "still blocked");
    drop(lock);
    // a barrier whose journal is gone
    let j = Install::legacy("orphan", true);
    put(
        &j.root.join(".ftesurf-updater/pending.json"),
        migrate::BARRIER,
    );
    let e = u::recover_with_key(&j.root, &f.key, false, Identity::Legacy)
        .await
        .unwrap_err();
    assert!(format!("{e:#}").contains("without its journal"), "{e:#}");
    assert!(u::begin(&j.root, &f.envelope, &[], Identity::Legacy).is_err());
    // a damaged journal
    let k = Install::legacy("damaged", true);
    put(&k.root.join(migrate::JOURNAL), b"{\"step\":\"marked\"}");
    let lock = k.lock();
    assert!(resume(&lock).is_err());
    assert!(migrate::eligible(&k.root).is_err());
}

#[tokio::test]
async fn finish_waits_for_a_verified_install() {
    let i = Install::legacy("unverified", true);
    let f = wos("ftesurf");
    let lock = i.lock();
    start(&lock).unwrap();
    let m = u::decode(&f.envelope, &f.key, false, Identity::WorldOfSurf).unwrap();
    assert!(
        migrate::finish(&lock, &m).await.is_err(),
        "nothing installed yet"
    );
    assert_eq!(
        i.read(&format!(
            ".ftesurf-updater/legacy/{}.parked",
            Identity::Legacy.exec()
        ))
        .unwrap(),
        LEGACY_EXE,
        "the parked exe is kept until the new install verifies"
    );
    assert!(i.has_barrier());
    assert_eq!(migrate::read_journal(&i.root).unwrap(), Some(Step::Marked));
}

#[tokio::test]
async fn the_marker_is_rewritten_only_by_the_move() {
    let i = Install::legacy("marker", true);
    assert!(u::establish_identity(&i.root, Identity::WorldOfSurf).is_err());
    assert_eq!(i.read(u::MARKER).unwrap(), b"ftesurf\n");
    let lock = i.lock();
    start(&lock).unwrap();
    assert_eq!(i.read(u::MARKER).unwrap(), b"worldofsurf\n");
    update(&lock, &wos("ftesurf"), Identity::WorldOfSurf, Stop::No)
        .await
        .unwrap();
    assert!(
        u::establish_identity(&i.root, Identity::Legacy).is_err(),
        "never back"
    );
    assert!(start(&lock).is_err(), "never twice");
    assert_eq!(i.read(u::MARKER).unwrap(), b"worldofsurf\n");
}

#[tokio::test]
async fn without_a_worldofsurf_feed_a_legacy_install_updates_as_before() {
    let i = Install::legacy("nofeed", true);
    let before = i.snapshot();
    let st = u::load_state(&i.root, Identity::Legacy).unwrap();
    let legacy = feed(Identity::Legacy, "0.1.49", "", TEST_SEED);
    let key = legacy.key.clone();
    for (name, text) in [
        ("a legacy feed at the new URL", legacy.envelope.clone()),
        (
            "a 404 page",
            "<html><body>404 Not Found</body></html>".to_string(),
        ),
        (
            "another key",
            feed(Identity::WorldOfSurf, "0.1.49", "ftesurf", OTHER_SEED).envelope,
        ),
        (
            "older than the install",
            feed(Identity::WorldOfSurf, "0.1.47", "ftesurf", TEST_SEED).envelope,
        ),
    ] {
        assert!(migrate::offer(&text, &key, false, &st).is_err(), "{name}");
    }
    assert!(migrate::offer(
        &feed(Identity::WorldOfSurf, "0.1.48", "ftesurf", TEST_SEED).envelope,
        &key,
        false,
        &st
    )
    .is_ok());
    assert!(migrate::eligible(&i.root).unwrap());
    assert_eq!(i.snapshot(), before, "probing changes nothing");
    let lock = i.lock();
    assert!(!resume(&lock).unwrap());
    update(&lock, &legacy, Identity::Legacy, Stop::No)
        .await
        .unwrap();
    assert_eq!(i.read("ftesurf/menu.dat").unwrap(), b"menu v1");
    assert_eq!(
        i.read(Identity::Legacy.exec()).unwrap(),
        b"legacy engine v2"
    );
    assert_eq!(
        i.read("ftesurf/maps/zones/local/plain.json").unwrap(),
        PLAIN_NEW
    );
    assert_eq!(
        i.read("ftesurf/ftesurf.cfg").unwrap(),
        CFG,
        "the player's cfg is untouched"
    );
    assert!(!i.root.join("worldofsurf").exists());
    assert_eq!(i.read(u::MARKER).unwrap(), b"ftesurf\n");
    assert!(
        i.read(migrate::JOURNAL).is_none() && i.read(".ftesurf-updater/pending.json").is_none()
    );
}

#[test]
fn a_fresh_install_is_not_offered_the_move() {
    let i = Install::empty("fresh");
    assert_eq!(u::install_identity(&i.root).unwrap(), u::FRESH_INSTALL);
    assert!(!migrate::eligible(&i.root).unwrap());
}

#[tokio::test]
async fn mainconfig_worldofsurf_renames_the_cfg_once() {
    let i = Install::legacy("mainconfig", true);
    let lock = i.lock();
    start_feed(&lock, &wos("worldofsurf")).unwrap();
    update(&lock, &wos("worldofsurf"), Identity::WorldOfSurf, Stop::No)
        .await
        .unwrap();
    assert_moved(&i, "worldofsurf.cfg");
    assert!(i.read("worldofsurf/ftesurf.cfg").is_none());
    // a worldofsurf.cfg already there is never overwritten
    let j = Install::legacy("mainconfig-both", true);
    let lock = j.lock();
    start_feed(&lock, &wos("worldofsurf")).unwrap();
    put(
        &j.root.join("worldofsurf/worldofsurf.cfg"),
        b"newer settings",
    );
    update(&lock, &wos("worldofsurf"), Identity::WorldOfSurf, Stop::No)
        .await
        .unwrap();
    assert_eq!(
        j.read("worldofsurf/worldofsurf.cfg").unwrap(),
        b"newer settings"
    );
    assert_eq!(j.read("worldofsurf/ftesurf.cfg").unwrap(), CFG_MOVED);
}

#[tokio::test]
async fn only_the_signed_default_fmf_names_the_cfg() {
    let i = Install::legacy("tampered-fmf", true);
    let f = wos("ftesurf");
    let lock = i.lock();
    start(&lock).unwrap();
    let m = u::decode(&f.envelope, &f.key, false, Identity::WorldOfSurf).unwrap();
    let st = u::load_state(&i.root, Identity::WorldOfSurf).unwrap();
    let (jobs, _) = u::plan(&m, &i.root, &st, Identity::WorldOfSurf)
        .await
        .unwrap();
    let staging = i.root.join(".ftesurf-updater/download");
    for j in &jobs {
        put(&staging.join(&j.path), &f.contents[&j.path]);
    }
    put(
        &staging.join("default.fmf"),
        // valid in every line, so only the signed hash refuses it
        b"FTEMANIFEST 1\nGAME \"WorldofSurf\"\nBASEGAME \"worldofsurf\"\nDISABLEHOMEDIR 1\nMAINCONFIG worldofsurf\n",
    );
    assert!(migrate::before_install(&lock, &staging, &m, &jobs)
        .await
        .is_err());
    assert_eq!(i.read("worldofsurf/ftesurf.cfg").unwrap(), CFG_MOVED);
    assert!(i.read("worldofsurf/worldofsurf.cfg").is_none());
}

#[tokio::test]
async fn a_custom_feed_url_survives_the_move() {
    let i = Install::legacy("custom", true);
    let custom = b"update_manifest_url \"https://example.invalid/feed.json\"\n";
    put(&i.root.join("ftesurf/ftesurf.cfg"), custom);
    let lock = i.lock();
    start(&lock).unwrap();
    assert_eq!(i.read("worldofsurf/ftesurf.cfg").unwrap(), custom);
}

#[tokio::test]
async fn a_feed_that_would_strand_settings_moves_nothing() {
    for (n, mainconfig) in [
        "WorldofSurf",
        "fte",
        "ftesurf\nMAINCONFIG worldofsurf",
        "ftesurf\nDISABLEHOMEDIR 0",
        "ftesurf\n*basegame ftesurf",
    ]
    .into_iter()
    .enumerate()
    {
        let i = Install::legacy(&format!("badfmf{n}"), true);
        let lock = i.lock();
        assert!(
            move_install(&lock, &wos(mainconfig)).await.is_err(),
            "{mainconfig:?}"
        );
        assert!(i.read(migrate::JOURNAL).is_none() && !i.has_barrier());
        assert_eq!(i.read("ftesurf/ftesurf.cfg").unwrap(), CFG);
        assert_eq!(i.read(Identity::Legacy.exec()).unwrap(), LEGACY_EXE);
        assert!(
            migrate::eligible(&i.root).unwrap(),
            "still a working legacy install"
        );
    }
}

#[tokio::test]
async fn a_read_only_cfg_is_left_as_it_is() {
    let i = Install::legacy("readonly", true);
    let cfg = i.root.join("ftesurf/ftesurf.cfg");
    let mut p = fs::metadata(&cfg).unwrap().permissions();
    p.set_readonly(true);
    fs::set_permissions(&cfg, p).unwrap();
    let lock = i.lock();
    move_install(&lock, &wos("ftesurf")).await.unwrap();
    update(&lock, &wos("ftesurf"), Identity::WorldOfSurf, Stop::No)
        .await
        .unwrap();
    let moved = i.root.join("worldofsurf/ftesurf.cfg");
    assert_eq!(fs::read(&moved).unwrap(), CFG);
    assert!(i.read(migrate::JOURNAL).is_none());
    let mut p = fs::metadata(&moved).unwrap().permissions();
    assert!(p.readonly());
    #[allow(clippy::permissions_set_readonly_false)]
    p.set_readonly(false);
    fs::set_permissions(&moved, p).unwrap();
}

#[tokio::test]
async fn a_link_at_the_cfg_temp_name_is_never_written_through() {
    let i = Install::legacy("tmplink", true);
    let victim = i.root.join("victim");
    put(&victim.join("keep.txt"), b"victim");
    // a file symlink where Windows allows one (developer mode), else a junction
    #[cfg(windows)]
    if std::os::windows::fs::symlink_file(
        victim.join("keep.txt"),
        i.root.join("ftesurf/ftesurf.cfg.move-tmp"),
    )
    .is_err()
    {
        dir_link(&victim, &i.root.join("ftesurf/ftesurf.cfg.move-tmp"));
    }
    #[cfg(not(windows))]
    std::os::unix::fs::symlink(
        victim.join("keep.txt"),
        i.root.join("ftesurf/ftesurf.cfg.move-tmp"),
    )
    .unwrap();
    {
        let lock = i.lock();
        assert!(start(&lock).is_err());
    }
    assert_eq!(fs::read(victim.join("keep.txt")).unwrap(), b"victim");
    assert!(
        i.has_barrier(),
        "the move waits, blocked, for the link to go"
    );
    #[cfg(windows)]
    {
        let link = i.root.join("worldofsurf/ftesurf.cfg.move-tmp");
        fs::remove_file(&link)
            .or_else(|_| fs::remove_dir(&link))
            .unwrap();
    }
    #[cfg(not(windows))]
    fs::remove_file(i.root.join("worldofsurf/ftesurf.cfg.move-tmp")).unwrap();
    next_start(&i, &wos("ftesurf")).await.unwrap();
    assert_moved(&i, "ftesurf.cfg");
}

/// An empty worldofsurf/ that another program holds open cannot be removed; nothing moves.
#[cfg(windows)]
#[test]
fn a_held_empty_worldofsurf_folder_changes_nothing() {
    use std::os::windows::fs::OpenOptionsExt;
    let i = Install::legacy("heldempty", true);
    fs::create_dir(i.root.join("worldofsurf")).unwrap();
    let lock = i.lock();
    let before = i.snapshot();
    let open = fs::OpenOptions::new()
        .read(true)
        .share_mode(0x1 | 0x2) // no FILE_SHARE_DELETE
        .custom_flags(0x0200_0000) // FILE_FLAG_BACKUP_SEMANTICS: open a directory
        .open(i.root.join("worldofsurf"))
        .unwrap();
    let e = start(&lock).unwrap_err();
    assert!(format!("{e:#}").contains("nothing was changed"), "{e:#}");
    assert_eq!(i.snapshot(), before, "the exe is back and the intent gone");
    drop(open);
    start(&lock).unwrap();
}

fn parked(i: &Install, name: &str) -> Option<Vec<u8>> {
    i.read(&format!(".ftesurf-updater/legacy/{name}.parked"))
}

/// An exiting engine saves its cfg after releasing the lock: a ftesurf/ that comes back after
/// the rename is reported, never deleted, and holds finish() until it is gone (or empty).
#[tokio::test]
async fn a_recreated_ftesurf_folder_is_reported_and_holds_finish() {
    let f = wos("ftesurf");
    let i = Install::legacy("recreated", true);
    {
        let lock = i.lock();
        move_install(&lock, &f).await.unwrap();
    }
    put(&i.root.join("ftesurf/ftesurf.cfg"), b"seta sensitivity 3\n");
    let lock = i.lock();
    assert!(resume(&lock).unwrap(), "the move goes on");
    assert_eq!(
        migrate::legacy_leftovers(&i.root).unwrap(),
        ["ftesurf/ftesurf.cfg"]
    );
    let e = update(&lock, &f, Identity::WorldOfSurf, Stop::No)
        .await
        .unwrap_err();
    assert!(format!("{e:#}").contains("ftesurf/ftesurf.cfg"), "{e:#}");
    assert_eq!(
        migrate::read_journal(&i.root).unwrap(),
        Some(Step::Marked),
        "finish waits"
    );
    assert!(
        !i.has_barrier(),
        "the install itself verified and can start"
    );
    assert_eq!(parked(&i, Identity::Legacy.exec()).unwrap(), LEGACY_EXE);
    assert_eq!(
        i.read("ftesurf/ftesurf.cfg").unwrap(),
        b"seta sensitivity 3\n",
        "never deleted"
    );
    drop(lock);
    fs::remove_dir_all(i.root.join("ftesurf")).unwrap();
    assert!(next_start(&i, &f).await.unwrap());
    assert_moved(&i, "ftesurf.cfg");
    // an empty one is removed
    let j = Install::legacy("recreated-empty", true);
    let lock = j.lock();
    move_install(&lock, &f).await.unwrap();
    fs::create_dir(j.root.join("ftesurf")).unwrap();
    update(&lock, &f, Identity::WorldOfSurf, Stop::No)
        .await
        .unwrap();
    assert_moved(&j, "ftesurf.cfg");
}

#[tokio::test]
async fn an_intent_without_its_barrier_never_started() {
    let foreign: &[u8] = br#"{"envelope":"an older launcher's update","paths":[]}"#;
    for (n, pending) in [None, Some(foreign)].into_iter().enumerate() {
        let i = Install::legacy(&format!("unstarted{n}"), true);
        let lock = i.lock();
        let before = i.snapshot();
        assert!(start_with(&lock, &mut stop_at(Point::Recorded(Step::Intent))).is_err());
        if let Some(bytes) = pending {
            put(&i.root.join(".ftesurf-updater/pending.json"), bytes);
        }
        assert!(!resume(&lock).unwrap(), "abandoned, not resumed");
        assert!(i.read(migrate::JOURNAL).is_none() && i.read(migrate::FEED).is_none());
        assert_eq!(
            i.read(".ftesurf-updater/pending.json").as_deref(),
            pending,
            "an older launcher's update is left for recover()"
        );
        assert_eq!(i.read(Identity::Legacy.exec()).unwrap(), LEGACY_EXE);
        if pending.is_none() {
            assert_eq!(i.snapshot(), before);
            assert!(migrate::eligible(&i.root).unwrap(), "offered again");
        }
    }
}

#[test]
fn a_link_appearing_at_the_parked_folder_never_strands_the_rollback() {
    let i = Install::legacy("parkedlink", true);
    fs::create_dir(i.root.join("elsewhere")).unwrap();
    let lock = i.lock();
    let root = i.root.clone();
    let e = start_with(&lock, &mut |p| {
        if p == Point::Barrier {
            dir_link(
                &root.join("elsewhere"),
                &root.join(".ftesurf-updater/legacy"),
            );
        }
        Ok(())
    })
    .unwrap_err();
    assert!(format!("{e:#}").contains("nothing was changed"), "{e:#}");
    assert!(i.read(migrate::JOURNAL).is_none() && !i.has_barrier());
    assert_eq!(i.read(Identity::Legacy.exec()).unwrap(), LEGACY_EXE);
    assert!(i.root.join("ftesurf/menu.dat").is_file());
    assert!(
        fs::read_dir(i.root.join("elsewhere"))
            .unwrap()
            .next()
            .is_none(),
        "nothing written through the link"
    );
}

#[tokio::test]
async fn the_move_installs_only_the_release_it_approved() {
    let f = wos("ftesurf");
    let i = Install::legacy("approved", true);
    let lock = i.lock();
    move_install(&lock, &f).await.unwrap();
    let approved = i.read(migrate::FEED).unwrap();
    assert_eq!(approved, f.envelope.as_bytes());
    let newer = feed(Identity::WorldOfSurf, "0.1.50", "ftesurf", TEST_SEED).envelope;
    for (name, bytes) in [
        ("another signed release", Some(newer.into_bytes())),
        (
            "another signed feed of the same version",
            Some(wos("worldofsurf").envelope.into_bytes()),
        ),
        (
            "a damaged copy",
            Some(approved[..approved.len() - 1].to_vec()),
        ),
        ("no copy", None),
    ] {
        match &bytes {
            Some(b) => put(&i.root.join(migrate::FEED), b),
            None => fs::remove_file(i.root.join(migrate::FEED)).unwrap(),
        }
        assert!(
            migrate::approved_feed(&i.root, &f.key, false).is_err(),
            "{name}"
        );
        assert!(resume(&lock).is_err(), "{name}");
    }
    put(&i.root.join(migrate::FEED), &approved);
    // the version is the journal's, not only the bytes' hash
    let journal = fs::read_to_string(i.root.join(migrate::JOURNAL)).unwrap();
    put(
        &i.root.join(migrate::JOURNAL),
        journal.replace("\"0.1.49\"", "\"0.1.50\"").as_bytes(),
    );
    assert!(migrate::approved_feed(&i.root, &f.key, false).is_err());
    put(&i.root.join(migrate::JOURNAL), journal.as_bytes());
    // offline: everything the moved install lacks is already staged
    assert!(resume(&lock).unwrap());
    let (_, m) = migrate::approved_feed(&i.root, &f.key, false).unwrap();
    assert_eq!(m.version, "0.1.49");
    let st = u::load_state(&i.root, Identity::WorldOfSurf).unwrap();
    let (jobs, _) = u::plan(&m, &i.root, &st, Identity::WorldOfSurf)
        .await
        .unwrap();
    let staging = i.root.join(".ftesurf-updater/download");
    for j in &jobs {
        assert!(
            u::matches(&staging, j, Identity::WorldOfSurf)
                .await
                .unwrap(),
            "{} staged",
            j.path
        );
    }
    update(&lock, &f, Identity::WorldOfSurf, Stop::No)
        .await
        .unwrap();
    assert_moved(&i, "ftesurf.cfg");
}

#[tokio::test]
async fn legacy_exes_are_parked_and_only_the_known_ones_deleted() {
    let f = wos("ftesurf");
    let other = other_exe(Identity::Legacy);
    // a shared install: both platforms' legacy exes, both in the ledger
    let i = Install::legacy("shared", true);
    put(&i.root.join(other), b"other legacy engine");
    executable(&i.root.join(other));
    let mut st: serde_json::Value =
        serde_json::from_slice(&i.read(".ftesurf-updater/state.json").unwrap()).unwrap();
    st["files"][other] = blake(b"other legacy engine").into();
    put(
        &i.root.join(".ftesurf-updater/state.json"),
        &serde_json::to_vec(&st).unwrap(),
    );
    let lock = i.lock();
    move_install(&lock, &f).await.unwrap();
    for name in [Identity::Legacy.exec(), other] {
        assert!(i.read(name).is_none(), "{name} left the root");
        assert!(parked(&i, name).is_some(), "{name} parked");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let p = i
                .root
                .join(format!(".ftesurf-updater/legacy/{name}.parked"));
            let mode = fs::metadata(p).unwrap().permissions().mode();
            assert_eq!(mode & 0o111, 0, "{name} parked without execute");
        }
    }
    // a known exe put back in the root goes too
    put(&i.root.join(Identity::Legacy.exec()), LEGACY_EXE);
    update(&lock, &f, Identity::WorldOfSurf, Stop::No)
        .await
        .unwrap();
    assert_moved(&i, "ftesurf.cfg");
    assert!(i.read(other).is_none());
    // exes the ledger does not know (the player's own builds) are kept, wherever they are
    let j = Install::legacy("unknown-exe", true);
    put(&j.root.join(Identity::Legacy.exec()), b"a build of my own");
    let lock = j.lock();
    move_install(&lock, &f).await.unwrap();
    put(&j.root.join(other), b"another build of my own");
    update(&lock, &f, Identity::WorldOfSurf, Stop::No)
        .await
        .unwrap();
    assert_eq!(
        parked(&j, Identity::Legacy.exec()).unwrap(),
        b"a build of my own"
    );
    assert_eq!(j.read(other).unwrap(), b"another build of my own");
    assert!(
        j.read(migrate::JOURNAL).is_none(),
        "the move still finishes"
    );
}

/// Finish held by a recreated ftesurf/, then a later release whose default.fmf the move's own
/// checks would refuse: once the folder is gone the clean-up finishes from the installed release,
/// with no feed and no network, and the later release installs as an ordinary update.
#[tokio::test]
async fn a_later_feed_never_holds_the_clean_up() {
    let f = wos("ftesurf");
    let later = feed(
        Identity::WorldOfSurf,
        "0.1.50",
        "ftesurf\nDISABLEHOMEDIR 0",
        TEST_SEED,
    );
    let i = Install::legacy("later", true);
    let lock = i.lock();
    move_install(&lock, &f).await.unwrap();
    put(&i.root.join("ftesurf/ftesurf.cfg"), b"late save\n");
    assert!(update(&lock, &f, Identity::WorldOfSurf, Stop::No)
        .await
        .is_err());
    assert!(!i.has_barrier() && migrate::read_journal(&i.root).unwrap() == Some(Step::Marked));
    // only clean-up is left: nothing needs the approved feed any more
    fs::remove_file(i.root.join(migrate::FEED)).unwrap();
    assert!(resume(&lock).unwrap());
    // the move's checks are off for any update but its own
    let m = u::decode(&later.envelope, &later.key, false, Identity::WorldOfSurf).unwrap();
    let st = u::load_state(&i.root, Identity::WorldOfSurf).unwrap();
    let (jobs, _) = u::plan(&m, &i.root, &st, Identity::WorldOfSurf)
        .await
        .unwrap();
    let staging = i.root.join(".ftesurf-updater/download");
    for j in &jobs {
        put(&staging.join(&j.path), &later.contents[&j.path]);
    }
    assert!(migrate::check_feed(&i.root, &staging, &m, &jobs)
        .await
        .is_err());
    migrate::before_install(&lock, &staging, &m, &jobs)
        .await
        .unwrap();
    // held while ftesurf/ holds anything; done from installed.json once it is gone
    assert!(migrate::finish_installed(&lock, &key(), false)
        .await
        .is_err());
    fs::remove_dir_all(i.root.join("ftesurf")).unwrap();
    assert!(migrate::finish_installed(&lock, &key(), false)
        .await
        .unwrap());
    assert!(i.read(migrate::JOURNAL).is_none());
    assert!(!migrate::finish_installed(&lock, &key(), false)
        .await
        .unwrap());
    update(&lock, &later, Identity::WorldOfSurf, Stop::No)
        .await
        .unwrap();
    assert_eq!(
        u::load_state(&i.root, Identity::WorldOfSurf)
            .unwrap()
            .version,
        "0.1.50"
    );
}

#[tokio::test]
async fn an_unmoved_intent_with_an_unusable_feed_rolls_back() {
    let i = Install::legacy("unusable", true);
    let lock = i.lock();
    let before = i.snapshot();
    assert!(start_with(&lock, &mut stop_at(Point::Parked)).is_err());
    assert!(i.has_barrier() && parked(&i, Identity::Legacy.exec()).is_some());
    fs::remove_file(i.root.join(migrate::FEED)).unwrap();
    assert!(!resume(&lock).unwrap(), "rolled back, not refused");
    assert_eq!(
        i.snapshot(),
        before,
        "the exe is back, nothing else remains"
    );
    assert!(migrate::eligible(&i.root).unwrap(), "offered again");
}

/// A rollback that cannot put a parked exe back keeps the journal (without its barrier), so the
/// next start rolls back again instead of losing track of the exe.
#[cfg(windows)]
#[test]
fn a_parked_exe_that_cannot_go_back_keeps_the_journal() {
    use std::os::windows::fs::OpenOptionsExt;
    let i = Install::legacy("stuck", true);
    let lock = i.lock();
    let before = i.snapshot();
    assert!(start_with(&lock, &mut stop_at(Point::Parked)).is_err());
    fs::remove_file(i.root.join(migrate::FEED)).unwrap();
    let held = fs::OpenOptions::new()
        .read(true)
        .share_mode(0x1 | 0x2) // no FILE_SHARE_DELETE
        .open(i.root.join(format!(
            ".ftesurf-updater/legacy/{}.parked",
            Identity::Legacy.exec()
        )))
        .unwrap();
    let e = resume(&lock).unwrap_err();
    let fix = format!(
        ".ftesurf-updater/legacy/{0}.parked back to {0}",
        Identity::Legacy.exec()
    );
    assert!(
        format!("{e:#}").contains(&fix),
        "names the manual fix: {e:#}"
    );
    assert_eq!(migrate::read_journal(&i.root).unwrap(), Some(Step::Intent));
    assert!(!i.has_barrier());
    drop(held);
    assert!(!resume(&lock).unwrap());
    assert_eq!(i.snapshot(), before);
}

/// legacy_leftovers names a folder it cannot read (finish refuses on it; the launcher only warns).
#[cfg(windows)]
#[test]
fn an_unreadable_leftover_is_named() {
    let i = Install::legacy("unreadable", true);
    let sub = i.root.join("ftesurf/locked");
    put(&sub.join("x.cfg"), b"x");
    let icacls = |args: &[&str]| {
        assert!(std::process::Command::new("icacls")
            .arg(&sub)
            .args(args)
            .output()
            .unwrap()
            .status
            .success())
    };
    icacls(&["/deny", "*S-1-1-0:(RD)"]); // Everyone: no listing
    let got = migrate::legacy_leftovers(&i.root);
    icacls(&["/remove:d", "*S-1-1-0"]);
    let e = got.unwrap_err();
    assert!(format!("{e:#}").contains("locked"), "{e:#}");
}

/// The reviewer's probe: the barrier deleted by hand after the move. Until installed.json names
/// worldofsurf the move's update is still pending, so it runs with its cfg step.
#[tokio::test]
async fn a_barrier_deleted_by_hand_still_leaves_the_update_pending() {
    let f = wos("worldofsurf");
    let i = Install::legacy("lost-barrier", true);
    let lock = i.lock();
    start_feed(&lock, &f).unwrap();
    assert_eq!(migrate::read_journal(&i.root).unwrap(), Some(Step::Marked));
    fs::remove_file(i.root.join(".ftesurf-updater/pending.json")).unwrap();
    assert!(resume(&lock).unwrap());
    u::recover_with_key(&i.root, &f.key, false, Identity::WorldOfSurf)
        .await
        .unwrap();
    assert!(migrate::awaits_update(&i.root).unwrap());
    assert!(
        !migrate::finish_installed(&lock, &f.key, false)
            .await
            .unwrap(),
        "no clean-up yet"
    );
    update(&lock, &f, Identity::WorldOfSurf, Stop::No)
        .await
        .unwrap();
    assert_moved(&i, "worldofsurf.cfg");
}

/// A rollback that fails keeps the error that made it roll back.
#[cfg(windows)]
#[test]
fn a_failed_rollback_keeps_the_move_error() {
    use std::os::windows::fs::OpenOptionsExt;
    let i = Install::legacy("rollback-context", true);
    put(&i.root.join("ftesurf/logs/qconsole.log"), b"log");
    let open = |p: &Path| {
        fs::OpenOptions::new()
            .read(true)
            .share_mode(0x1 | 0x2) // no FILE_SHARE_DELETE
            .open(p)
            .unwrap()
    };
    let _game = open(&i.root.join("ftesurf/logs/qconsole.log")); // the rename fails
    let parked = i.root.join(format!(
        ".ftesurf-updater/legacy/{}.parked",
        Identity::Legacy.exec()
    ));
    let mut held = None;
    let lock = i.lock();
    let e = start_with(&lock, &mut |p| {
        if p == Point::Parked {
            held = Some(open(&parked)); // and so does putting the exe back
        }
        Ok(())
    })
    .unwrap_err();
    let msg = format!("{e:#}");
    assert!(
        msg.contains("failed (") && msg.contains("os error"),
        "{msg}"
    );
    assert!(msg.contains(".parked back to"), "{msg}");
    drop(held);
}

/// Finish held, a later ordinary update carries the ledger past the approved release, then
/// installed.json and move-feed.json are lost: the ledger shows the move's update ran, so every
/// start falls through to the ordinary path (which rewrites installed.json) instead of refusing.
#[tokio::test]
async fn a_lost_installed_json_after_a_later_update_falls_through() {
    let f = wos("ftesurf");
    let later = feed(Identity::WorldOfSurf, "0.1.50", "ftesurf", TEST_SEED);
    let i = Install::legacy("lost-installed", true);
    let lock = i.lock();
    move_install(&lock, &f).await.unwrap();
    put(&i.root.join("ftesurf/ftesurf.cfg"), b"late save\n");
    assert!(update(&lock, &f, Identity::WorldOfSurf, Stop::No)
        .await
        .is_err());
    update(&lock, &later, Identity::WorldOfSurf, Stop::No)
        .await
        .unwrap();
    assert_eq!(
        u::load_state(&i.root, Identity::WorldOfSurf)
            .unwrap()
            .version,
        "0.1.50"
    );
    fs::remove_file(i.root.join(".ftesurf-updater/installed.json")).unwrap();
    fs::remove_file(i.root.join(migrate::FEED)).unwrap();
    assert!(
        !migrate::awaits_update(&i.root).unwrap(),
        "the ledger records the move's update"
    );
    assert!(resume(&lock).unwrap(), "no approved feed needed");
    update(&lock, &later, Identity::WorldOfSurf, Stop::No)
        .await
        .unwrap();
    assert!(i.read(".ftesurf-updater/installed.json").is_some());
    fs::remove_dir_all(i.root.join("ftesurf")).unwrap();
    assert!(migrate::finish_installed(&lock, &key(), false)
        .await
        .unwrap());
    assert!(i.read(migrate::JOURNAL).is_none());
}

/// A ledger at the approved version is not the move's update unless it records the worldofsurf
/// exe (offer() allows a worldofsurf release equal to the legacy one).
#[tokio::test]
async fn an_equal_version_legacy_ledger_still_waits_for_the_update() {
    let f = feed(Identity::WorldOfSurf, "0.1.48", "ftesurf", TEST_SEED);
    let i = Install::legacy("equal-version", true);
    let lock = i.lock();
    start_feed(&lock, &f).unwrap();
    fs::remove_file(i.root.join(".ftesurf-updater/pending.json")).unwrap();
    assert!(migrate::awaits_update(&i.root).unwrap());
}

/// begin() swapped the barrier for its own journal and the launcher died before recover(): the
/// approved feed is no longer needed, recover() finishes from that journal.
#[tokio::test]
async fn an_update_begun_needs_no_approved_feed() {
    let f = wos("ftesurf");
    let i = Install::legacy("begun", true);
    {
        let lock = i.lock();
        start(&lock).unwrap();
        assert!(update(&lock, &f, Identity::WorldOfSurf, Stop::AfterBegin)
            .await
            .is_err());
    }
    fs::remove_file(i.root.join(migrate::FEED)).unwrap();
    assert!(next_start(&i, &f).await.unwrap());
    assert_moved(&i, "ftesurf.cfg");
}

#[test]
fn a_missing_approved_feed_names_the_fix() {
    const DROP: &str = "delete .ftesurf-updater/migrate.json and .ftesurf-updater/pending.json";
    const RESTORE: &str = "put .ftesurf-updater/move-feed.json back from a backup";
    // at marked the journal may go, and the cfg left unrenamed is named
    let i = Install::legacy("feed-fix", true);
    let lock = i.lock();
    start(&lock).unwrap();
    fs::remove_file(i.root.join(migrate::FEED)).unwrap();
    let e = format!("{:#}", resume(&lock).unwrap_err());
    assert!(e.contains(RESTORE) && e.contains(DROP), "{e}");
    assert!(
        e.contains("copy it to worldofsurf/worldofsurf.cfg if the release's default.fmf says MAINCONFIG worldofsurf"),
        "{e}"
    );
    // half moved: only the safe action, nothing destructive
    let j = Install::legacy("feed-fix-early", true);
    let lock = j.lock();
    assert!(start_with(&lock, &mut stop_at(Point::Recorded(Step::Renamed))).is_err());
    fs::remove_file(j.root.join(migrate::FEED)).unwrap();
    let e = format!("{:#}", resume(&lock).unwrap_err());
    assert!(
        e.contains(RESTORE) && e.contains("change nothing else by hand"),
        "{e}"
    );
    assert!(!e.contains("delete"), "{e}");
}

/// A copy that verifies against the journal's hash but not as a feed, or one of another release
/// than the journal's, names the remedy too.
#[test]
fn every_approved_feed_error_names_the_remedy() {
    use sha2::Digest;
    let i = Install::legacy("feed-remedy", true);
    let lock = i.lock();
    start(&lock).unwrap();
    let path = i.root.join(migrate::JOURNAL);
    let journal: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    let with = |key: &str, value: String| {
        let mut j = journal.clone();
        j[key] = value.into();
        put(&path, &serde_json::to_vec(&j).unwrap());
    };
    let garbage = b"{\"payload\":\"{}\",\"signature\":\"00\"}";
    put(&i.root.join(migrate::FEED), garbage);
    with("feed_sha256", hex::encode(sha2::Sha256::digest(garbage)));
    let e = format!(
        "{:#}",
        migrate::approved_feed(&i.root, &key(), false).unwrap_err()
    );
    assert!(
        e.contains("does not verify") && e.contains("back from a backup"),
        "{e}"
    );
    put(
        &i.root.join(migrate::FEED),
        wos("ftesurf").envelope.as_bytes(),
    );
    with(
        "feed_sha256",
        journal["feed_sha256"].as_str().unwrap().to_string(),
    );
    with("version", "0.1.50".into());
    let e = format!(
        "{:#}",
        migrate::approved_feed(&i.root, &key(), false).unwrap_err()
    );
    assert!(
        e.contains("not the release") && e.contains("back from a backup"),
        "{e}"
    );
}
