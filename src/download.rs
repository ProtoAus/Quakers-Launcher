//! Parallel, resumable, self-verifying downloader feeding the ratatui UI.
//!
//! N worker tasks pull files off a shared index. Each file streams to `<dest>.part`
//! (HTTP Range-resumed if a partial exists), is hashed on the fly, and is atomically
//! renamed into place only once the hash matches the manifest.
//!
//! A failing file is retried with exponential backoff and jitter, cycling mirrors if more
//! than one is configured. The `.part` survives between attempts, so a retry resumes from
//! wherever the previous one stopped rather than starting over. Errors that retrying cannot
//! fix (404/410) short-circuit the budget instead of consuming it.

use crate::hashing::Hasher;
use crate::manifest::FileEntry;
use crate::ui::{self, Progress};
use anyhow::{anyhow, Result};
use futures_util::StreamExt;
use reqwest::header::RANGE;
use reqwest::StatusCode;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[derive(Clone, Copy, PartialEq)]
enum DownloadPolicy {
    Quakers,
    Ftesurf,
}

#[derive(Clone, Copy)]
enum Display {
    Quiet,
    Quakers,
    WorldOfSurf,
}

fn part_path(dest: &Path) -> PathBuf {
    let mut part = dest.as_os_str().to_owned();
    part.push(".part");
    PathBuf::from(part)
}

impl DownloadPolicy {
    fn check(self, part: &Path, dest: &Path) -> Result<()> {
        if self == Self::Ftesurf {
            for path in [part, dest] {
                crate::ftesurf_update::checked_download_path(path)
                    .map_err(|e| anyhow!(Permanent(format!("{e:#}"))))?;
            }
        }
        Ok(())
    }
}

/// Atomic no-follow for the final component on the two shipped platforms. Validate
/// the handle before truncating; Windows OPEN_REPARSE_POINT alone still opens a link.
fn open_ftesurf_part(
    part: &Path,
    dest: &Path,
    write: bool,
    append: bool,
) -> Result<tokio::fs::File> {
    DownloadPolicy::Ftesurf.check(part, dest)?;
    let mut options = std::fs::OpenOptions::new();
    options
        .read(!write)
        .write(write)
        .create(write)
        .append(append);
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::fs::OpenOptionsExt;
        // Linux O_NOFOLLOW | O_NONBLOCK: reject links, never block on a planted FIFO.
        options.custom_flags(0o400000 | 0o4000);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.custom_flags(0x00200000); // FILE_FLAG_OPEN_REPARSE_POINT
    }
    #[cfg(not(any(target_os = "linux", windows)))]
    return Err(anyhow!(Permanent(
        "hardened staging opens unsupported on this platform".into()
    )));
    #[cfg(any(target_os = "linux", windows))]
    {
        let file = options.open(part)?;
        let md = file.metadata()?;
        if !md.is_file() {
            return Err(anyhow!(Permanent(
                "staging handle is not a regular file".into()
            )));
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            if md.file_attributes() & 0x400 != 0 {
                return Err(anyhow!(Permanent("linked staging handle".into())));
            }
        }
        Ok(tokio::fs::File::from_std(file))
    }
}

async fn discard_part(policy: DownloadPolicy, part: &Path, dest: &Path) -> Result<()> {
    policy.check(part, dest)?;
    let result = tokio::fs::remove_file(part).await;
    if policy == DownloadPolicy::Ftesurf {
        match result {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}

pub struct DlOutcome {
    pub ok: u64,
    pub failed: Vec<(String, String)>, // (path, error)
    /// Per-content-type totals for the post-download summary: (name, downloaded, expected).
    pub categories: Vec<(&'static str, u64, u64)>,
}

#[allow(clippy::too_many_arguments)]
pub async fn download_all(
    client: reqwest::Client,
    mirrors: Vec<String>,
    algo: String,
    install_dir: PathBuf,
    jobs: Vec<FileEntry>,
    total_bytes: u64,
    concurrency: usize,
    header: ui::Header,
) -> Result<DlOutcome> {
    download_all_inner(
        client,
        mirrors,
        algo,
        install_dir,
        jobs,
        total_bytes,
        concurrency,
        header,
        Display::Quakers,
        DownloadPolicy::Quakers,
    )
    .await
}

/// Same verified/resumable transport, without a terminal UI takeover.
#[allow(clippy::too_many_arguments)]
pub async fn download_all_quiet(
    client: reqwest::Client,
    mirrors: Vec<String>,
    algo: String,
    install_dir: PathBuf,
    jobs: Vec<FileEntry>,
    total_bytes: u64,
    concurrency: usize,
) -> Result<DlOutcome> {
    download_all_inner(
        client,
        mirrors,
        algo,
        install_dir,
        jobs,
        total_bytes,
        concurrency,
        ui::Header::default(),
        Display::Quiet,
        DownloadPolicy::Quakers,
    )
    .await
}

/// FTESurf staging policy: preflight the whole batch before workers create files or
/// contact object mirrors. Executable repair belongs solely to prepare_install.
#[allow(clippy::too_many_arguments)]
pub async fn download_all_ftesurf(
    client: reqwest::Client,
    mirrors: Vec<String>,
    algo: String,
    install_dir: PathBuf,
    jobs: Vec<FileEntry>,
    total_bytes: u64,
    concurrency: usize,
) -> Result<DlOutcome> {
    download_ftesurf(client, mirrors, algo, install_dir, jobs, total_bytes, concurrency,
        ui::Header::default(), Display::Quiet).await
}

/// Branded presentation over the same guarded staging transport.
#[allow(clippy::too_many_arguments)]
pub async fn download_all_ftesurf_with_ui(
    client: reqwest::Client,
    mirrors: Vec<String>,
    algo: String,
    install_dir: PathBuf,
    jobs: Vec<FileEntry>,
    total_bytes: u64,
    concurrency: usize,
    header: ui::Header,
) -> Result<DlOutcome> {
    download_ftesurf(client, mirrors, algo, install_dir, jobs, total_bytes, concurrency,
        header, Display::WorldOfSurf).await
}

#[allow(clippy::too_many_arguments)]
async fn download_ftesurf(
    client: reqwest::Client,
    mirrors: Vec<String>,
    algo: String,
    install_dir: PathBuf,
    jobs: Vec<FileEntry>,
    total_bytes: u64,
    concurrency: usize,
    header: ui::Header,
    display: Display,
) -> Result<DlOutcome> {
    #[cfg(not(any(target_os = "linux", windows)))]
    return Err(anyhow!(Permanent(
        "hardened staging opens unsupported on this platform".into()
    )));
    for entry in &jobs {
        let dest = crate::ftesurf_update::checked_path(&install_dir, &entry.path)?;
        DownloadPolicy::Ftesurf.check(&part_path(&dest), &dest)?;
    }
    download_all_inner(
        client,
        mirrors,
        algo,
        install_dir,
        jobs,
        total_bytes,
        concurrency,
        header,
        display,
        DownloadPolicy::Ftesurf,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn download_all_inner(
    client: reqwest::Client,
    mirrors: Vec<String>,
    algo: String,
    install_dir: PathBuf,
    jobs: Vec<FileEntry>,
    total_bytes: u64,
    concurrency: usize,
    header: ui::Header,
    display: Display,
    policy: DownloadPolicy,
) -> Result<DlOutcome> {
    let total_jobs = jobs.len();
    let n = concurrency.max(1).min(total_jobs.max(1));

    // Bucket the work by content type for the breakdown panel, keeping the fixed
    // display order and dropping anything with nothing to fetch.
    let categories: Vec<(&'static str, u64)> = ui::CATEGORY_ORDER
        .iter()
        .filter_map(|name| {
            let total: u64 = jobs
                .iter()
                .filter(|e| ui::category_of(&e.path) == *name)
                .map(|e| e.size)
                .sum();
            (total > 0).then_some((*name, total))
        })
        .collect();

    let progress = Progress::new(total_bytes, total_jobs, n, categories, header);
    // Resolve each job's bucket once here, never on the byte-counting hot path.
    let cat_idx: Arc<Vec<Option<usize>>> = Arc::new(
        jobs.iter()
            .map(|e| progress.category_index(&e.path))
            .collect(),
    );
    let (ui_handle, cancellation) = match display {
        Display::Quiet => (None, None),
        Display::Quakers => (ui::start(progress.clone()), None),
        Display::WorldOfSurf => match crate::ftesurf_ui::start(progress.clone()) {
            Some((stop, cancelled, handle)) => (Some((stop, handle)), Some(cancelled)),
            None => (None, None),
        },
    };

    let jobs = Arc::new(jobs);
    let idx = Arc::new(AtomicUsize::new(0));
    let ok = Arc::new(AtomicU64::new(0));
    let failed = Arc::new(tokio::sync::Mutex::new(Vec::<(String, String)>::new()));

    let mut tasks = tokio::task::JoinSet::new();
    for wid in 0..n {
        let client = client.clone();
        let mirrors = mirrors.clone();
        let algo = algo.clone();
        let install_dir = install_dir.clone();
        let jobs = jobs.clone();
        let idx = idx.clone();
        let ok = ok.clone();
        let failed = failed.clone();
        let progress = progress.clone();
        let cat_idx = cat_idx.clone();

        tasks.spawn(async move {
            loop {
                let i = idx.fetch_add(1, Ordering::SeqCst);
                if i >= jobs.len() {
                    break;
                }
                let entry = &jobs[i];
                let dest = install_dir.join(&entry.path);
                let cat = cat_idx[i];
                progress.worker_start(wid, &short_name(&entry.path), entry.size);

                match download_object(
                    &client, &mirrors, &algo, entry, &dest, &progress, wid, cat, policy,
                )
                .await
                {
                    Ok(()) => {
                        ok.fetch_add(1, Ordering::SeqCst);
                        progress.file_verified(entry.size, cat);
                    }
                    Err(e) => failed
                        .lock()
                        .await
                        .push((entry.path.clone(), e.to_string())),
                }
            }
            progress.worker_idle(wid);
        });
    }

    let mut interrupted = false;
    while !tasks.is_empty() {
        tokio::select! {
            _ = tasks.join_next() => (),
            _ = tokio::time::sleep(std::time::Duration::from_millis(50)), if cancellation.is_some() => {
                if cancellation.as_ref().is_some_and(|c| c.load(Ordering::Relaxed)) {
                    interrupted = true;
                    tasks.abort_all();
                    // Drain aborted workers before restoring the screen and returning to the caller.
                    while tasks.join_next().await.is_some() {}
                    break;
                }
            }
        }
    }
    if let Some((stop, handle)) = ui_handle {
        stop.store(true, Ordering::Relaxed);
        let _ = handle.join();
    }

    anyhow::ensure!(!interrupted && !cancellation.as_ref().is_some_and(|c| c.load(Ordering::Relaxed)),
        "download cancelled; partial files are retained for resume, no installation was started");

    let failed = Arc::try_unwrap(failed)
        .map(|m| m.into_inner())
        .unwrap_or_default();
    let categories = progress
        .categories
        .iter()
        .map(|c| (c.name, c.done.load(Ordering::Relaxed).min(c.total), c.total))
        .collect();
    Ok(DlOutcome {
        ok: ok.load(Ordering::SeqCst),
        failed,
        categories,
    })
}

#[allow(clippy::too_many_arguments)]
async fn download_object(
    client: &reqwest::Client,
    mirrors: &[String],
    algo: &str,
    entry: &FileEntry,
    dest: &Path,
    progress: &Progress,
    wid: usize,
    cat: Option<usize>,
    policy: DownloadPolicy,
) -> Result<()> {
    let part = part_path(dest);
    policy.check(&part, dest)?;
    if let Some(parent) = dest.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    policy.check(&part, dest)?;

    let obj_rel = entry.object_rel();
    let mut last_err = anyhow!("no mirrors configured");
    if mirrors.is_empty() {
        return Err(last_err);
    }

    // Attempts are decoupled from mirror count. They used to be `mirrors.len() + 1`, which
    // with a single mirror meant two tries fired back-to-back with no delay -- so one dropped
    // connection or momentary packet loss consumed the whole budget in milliseconds and the
    // file was reported failed. That is why a first run could miss a handful of files that a
    // second run then fetched without trouble.
    let attempts = MAX_ATTEMPTS.max(mirrors.len() * 2);
    for attempt in 0..attempts {
        let mirror_no = attempt % mirrors.len();
        let base = &mirrors[mirror_no];
        let url = format!("{}/{}", base.trim_end_matches('/'), obj_rel);
        match try_one(
            client, &url, algo, entry, &part, dest, progress, wid, cat, policy,
        )
        .await
        {
            Ok(()) => return Ok(()),
            Err(e) => {
                // A 404/410 means the manifest and the mirror disagree about what exists.
                // No amount of waiting fixes that, and retrying would only delay the report.
                if e.downcast_ref::<Permanent>().is_some()
                    || (policy == DownloadPolicy::Ftesurf
                        && e.downcast_ref::<std::io::Error>().is_some())
                {
                    return Err(anyhow!("mirror {}: {}", mirror_no + 1, e));
                }
                last_err = anyhow!("mirror {}: {}", mirror_no + 1, e);
                if attempt + 1 < attempts {
                    // Show the wait in the UI, so a backing-off worker does not look frozen.
                    progress.worker_start(
                        wid,
                        &format!(
                            "{} (retry {}/{})",
                            short_name(&entry.path),
                            attempt + 1,
                            attempts - 1
                        ),
                        entry.size,
                    );
                    tokio::time::sleep(backoff(attempt)).await;
                }
            }
        }
    }
    Err(last_err)
}

/// Retry budget per file. Deliberately modest: these delays are paid per *failing* file, so a
/// mirror that is genuinely down should still surface an error in reasonable time rather than
/// spending hours backing off across thousands of files.
const MAX_ATTEMPTS: usize = 5;

/// Exponential backoff, capped at 3s, with jitter so N workers that trip over the same blip do
/// not resynchronise and hammer the mirror in lockstep. Jitter is derived from the clock rather
/// than a rand crate — it needs to be uncorrelated, not cryptographic.
fn backoff(attempt: usize) -> std::time::Duration {
    let base = std::time::Duration::from_millis(300u64 << attempt.min(4))
        .min(std::time::Duration::from_secs(3));
    let jitter = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| u64::from(d.subsec_nanos()) % 250)
        .unwrap_or(0);
    base + std::time::Duration::from_millis(jitter)
}

/// An error retrying cannot fix.
#[derive(Debug)]
struct Permanent(String);

impl std::fmt::Display for Permanent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for Permanent {}

#[allow(clippy::too_many_arguments)]
async fn try_one(
    client: &reqwest::Client,
    url: &str,
    algo: &str,
    entry: &FileEntry,
    part: &Path,
    dest: &Path,
    progress: &Progress,
    wid: usize,
    cat: Option<usize>,
    policy: DownloadPolicy,
) -> Result<()> {
    policy.check(part, dest)?;
    // `None` when there is no .part at all. That distinction matters: an empty manifest entry
    // and a missing file both measure 0 bytes, and conflating them made every zero-byte file
    // fail with a rename of something that was never created (os error 2 / ENOENT).
    let part_len: Option<u64> = if policy == DownloadPolicy::Ftesurf {
        match tokio::fs::symlink_metadata(part).await {
            Ok(md) => Some(md.len()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(e.into()),
        }
    } else {
        tokio::fs::metadata(part).await.ok().map(|m| m.len())
    };

    // A .part that EXISTS and already holds exactly `size` bytes is not a resume -- it is a
    // finished download interrupted between its last write and the rename below. Do NOT
    // range-request from here: `Range: bytes=<size>-` starts at EOF, which RFC 7233 defines as
    // unsatisfiable, so the server answers 416 and that failure is permanent -- nothing shrinks
    // the .part, so every retry recomputes the same offset and asks for the same bad range.
    // The bytes are already local, so this needs no request at all.
    if part_len == Some(entry.size) {
        let mut whole = Hasher::new(algo)?;
        let mut f = if policy == DownloadPolicy::Ftesurf {
            open_ftesurf_part(part, dest, false, false)?
        } else {
            tokio::fs::File::open(part).await?
        };
        let mut buf = vec![0u8; 1 << 20];
        loop {
            let read = f.read(&mut buf).await?;
            if read == 0 {
                break;
            }
            whole.update(&buf[..read]);
        }
        drop(f);
        if whole.hex() == entry.hash {
            progress.worker_set_cur(wid, entry.size);
            return finalize(entry, part, dest, policy).await;
        }
        // Right length, wrong bytes: there is no valid prefix to resume from. Start clean.
        discard_part(policy, part, dest).await?;
    }

    // Resume only from a partial that is genuinely shorter than the target. Anything else
    // (absent, complete-but-corrupt, or somehow longer) restarts at zero.
    let mut offset: u64 = part_len.filter(|&l| l < entry.size).unwrap_or(0);

    // Reconstruct hasher state from the existing prefix.
    let mut hasher = Hasher::new(algo)?;
    if offset > 0 {
        let mut f = if policy == DownloadPolicy::Ftesurf {
            open_ftesurf_part(part, dest, false, false)?
        } else {
            tokio::fs::File::open(part).await?
        };
        let mut buf = vec![0u8; 1 << 20];
        let mut fed: u64 = 0;
        while fed < offset {
            let read = f.read(&mut buf).await?;
            if read == 0 {
                break;
            }
            let take = std::cmp::min(read as u64, offset - fed) as usize;
            hasher.update(&buf[..take]);
            fed += take as u64;
        }
        offset = fed;
    }
    progress.worker_set_cur(wid, offset);

    // Establish writable access before HTTP, without truncating an existing prefix.
    let staged_file = if policy == DownloadPolicy::Ftesurf {
        Some(open_ftesurf_part(part, dest, true, offset > 0)?)
    } else {
        None
    };
    policy.check(part, dest)?;
    let mut req = client.get(url);
    if offset > 0 {
        req = req.header(RANGE, format!("bytes={}-", offset));
    }
    let resp = req.send().await?;
    let status = resp.status();
    if status == StatusCode::RANGE_NOT_SATISFIABLE {
        // The server says our resume point is past the end of the object. Whatever the cause
        // -- a .part from an older build, an object replaced under the same name -- there is
        // nothing here to resume from, and leaving the .part in place makes the failure
        // permanent: every re-run computes the same offset and asks for the same bad range.
        drop(staged_file);
        discard_part(policy, part, dest).await?;
        return Err(anyhow!(
            "HTTP 416 (discarded stale .part; retry starts clean)"
        ));
    }
    if status == StatusCode::NOT_FOUND || status == StatusCode::GONE {
        return Err(anyhow!(Permanent(format!("HTTP {status}"))));
    }
    if !status.is_success() {
        return Err(anyhow!("HTTP {}", status));
    }

    // Asked for a range but got the whole file (200, not 206) -> start over.
    let restart = offset > 0 && status != StatusCode::PARTIAL_CONTENT;
    policy.check(part, dest)?;
    let mut file = if let Some(file) = staged_file {
        if restart || offset == 0 {
            hasher = Hasher::new(algo)?;
            progress.worker_set_cur(wid, 0);
            file.set_len(0).await?;
        }
        file
    } else if restart || offset == 0 {
        hasher = Hasher::new(algo)?;
        progress.worker_set_cur(wid, 0);
        tokio::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(part)
            .await?
    } else {
        tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(part)
            .await?
    };

    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        file.write_all(&chunk).await?;
        hasher.update(&chunk);
        progress.worker_add(wid, chunk.len() as u64);
        progress.add_bytes(chunk.len() as u64);
        progress.add_category_bytes(cat, chunk.len() as u64);
    }
    file.flush().await?;
    drop(file);

    let got = hasher.hex();
    if got != entry.hash {
        discard_part(policy, part, dest).await?;
        return Err(anyhow!(
            "hash mismatch (got {}… want {}…)",
            &got[..8.min(got.len())],
            &entry.hash[..8.min(entry.hash.len())]
        ));
    }

    finalize(entry, part, dest, policy).await
}

/// Promote a verified `.part` to its final name. Split out so the "already complete"
/// short-circuit above lands the file by exactly the same path a fresh download does.
async fn finalize(
    entry: &FileEntry,
    part: &Path,
    dest: &Path,
    policy: DownloadPolicy,
) -> Result<()> {
    policy.check(part, dest)?;
    if policy == DownloadPolicy::Ftesurf {
        match tokio::fs::symlink_metadata(dest).await {
            Ok(_) => tokio::fs::remove_file(dest).await?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        policy.check(part, dest)?;
    } else if tokio::fs::metadata(dest).await.is_ok() {
        let _ = tokio::fs::remove_file(dest).await;
    }
    tokio::fs::rename(part, dest).await?;
    if policy == DownloadPolicy::Quakers {
        set_exec_bit(entry, dest).await;
    }
    Ok(())
}

/// On unix, a freshly downloaded engine binary arrives 0644 and will not run. Mark the
/// entries the manifest flags as executable. No-op on Windows, which has no exec bit.
#[cfg(unix)]
async fn set_exec_bit(entry: &FileEntry, dest: &Path) {
    if !entry.exec {
        return;
    }
    use std::os::unix::fs::PermissionsExt;
    if let Ok(md) = tokio::fs::metadata(dest).await {
        let mut perm = md.permissions();
        perm.set_mode(perm.mode() | 0o755);
        let _ = tokio::fs::set_permissions(dest, perm).await;
    }
}

#[cfg(not(unix))]
async fn set_exec_bit(_entry: &FileEntry, _dest: &Path) {}

fn short_name(path: &str) -> String {
    path.rsplit('/').next().unwrap_or(path).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::Header;

    struct FtesurfFixture(PathBuf);

    impl FtesurfFixture {
        fn new(name: &str) -> Self {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let root = std::env::var_os("FTESURF_DOWNLOAD_TEST_ROOT")
                .map(PathBuf::from)
                .unwrap_or_else(std::env::temp_dir);
            let path = root.join(format!(
                "ftesurf-download-{name}-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::SeqCst)
            ));
            std::fs::create_dir_all(&root).unwrap();
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for FtesurfFixture {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).unwrap();
        }
    }

    fn file_link(target: &Path, link: &Path) {
        #[cfg(unix)]
        std::os::unix::fs::symlink(target, link).unwrap();
        #[cfg(windows)]
        std::os::windows::fs::symlink_file(target, link).unwrap();
    }

    fn directory_link(target: &Path, link: &Path) {
        #[cfg(unix)]
        std::os::unix::fs::symlink(target, link).unwrap();
        #[cfg(windows)]
        assert!(std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(link.to_string_lossy().replace('/', "\\"))
            .arg(target.to_string_lossy().replace('/', "\\"))
            .status()
            .unwrap()
            .success());
    }

    async fn ftesurf_download(
        base: String,
        root: &Path,
        jobs: Vec<FileEntry>,
    ) -> Result<DlOutcome> {
        let bytes = jobs.iter().map(|f| f.size).sum();
        download_all_ftesurf(
            reqwest::Client::new(),
            vec![base],
            "blake2b-256".into(),
            root.into(),
            jobs,
            bytes,
            1,
        )
        .await
    }

    #[tokio::test]
    async fn ftesurf_linked_part_whole_job_preflight_refuses_without_http_or_mutation() {
        for sentinel in [
            b"short".as_slice(),
            b"verified complete object",
            b"corrupt complete object!",
            b"oversized external sentinel not for truncation",
        ] {
            let fixture = FtesurfFixture::new("linked-part");
            let stage = fixture.0.join("stage");
            std::fs::create_dir(&stage).unwrap();
            let outside = fixture.0.join("sentinel");
            std::fs::write(&outside, sentinel).unwrap();
            file_link(&outside, &stage.join("default.fmf.part"));
            let body = b"verified complete object";
            let (base, hits, _) = flaky_server(0, body.to_vec()).await;
            let result = ftesurf_download(
                base,
                &stage,
                vec![entry("LICENSE", body), entry("default.fmf", body)],
            )
            .await;
            assert_eq!(
                std::fs::read(&outside).unwrap(),
                sentinel,
                "outside sentinel changed"
            );
            assert!(
                result.is_err(),
                "whole batch must refuse a linked companion"
            );
            assert_eq!(hits.load(Ordering::SeqCst), 0);
            assert!(!stage.join("LICENSE").exists());
            assert!(!stage.join("LICENSE.part").exists());
            assert!(std::fs::symlink_metadata(stage.join("default.fmf.part")).is_ok());
        }
    }

    #[tokio::test]
    async fn ftesurf_linked_ancestor_refuses_without_http_or_mutation() {
        let fixture = FtesurfFixture::new("linked-ancestor");
        let stage = fixture.0.join("stage");
        let outside = fixture.0.join("outside");
        std::fs::create_dir(&stage).unwrap();
        std::fs::create_dir(&outside).unwrap();
        std::fs::write(outside.join("menu.dat.part"), b"sentinel").unwrap();
        directory_link(&outside, &stage.join("ftesurf"));
        let (base, hits, _) = flaky_server(0, b"replacement".to_vec()).await;
        let result = ftesurf_download(
            base,
            &stage,
            vec![entry("ftesurf/menu.dat", b"replacement")],
        )
        .await;
        assert_eq!(
            std::fs::read(outside.join("menu.dat.part")).unwrap(),
            b"sentinel"
        );
        assert!(result.is_err());
        assert_eq!(hits.load(Ordering::SeqCst), 0);
        assert!(!outside.join("menu.dat").exists());
    }

    #[tokio::test]
    async fn ftesurf_dangling_parts_and_nonfile_leaves_refuse_before_http() {
        for leaf in ["LICENSE", "LICENSE.part"] {
            let fixture = FtesurfFixture::new("nonfile-leaf");
            let stage = fixture.0.join("stage");
            std::fs::create_dir(&stage).unwrap();
            std::fs::create_dir(stage.join(leaf)).unwrap();
            let (base, hits, _) = flaky_server(0, b"body".to_vec()).await;
            assert!(
                ftesurf_download(base, &stage, vec![entry("LICENSE", b"body")])
                    .await
                    .is_err()
            );
            assert_eq!(hits.load(Ordering::SeqCst), 0);
            assert!(stage.join(leaf).is_dir());
        }
        let fixture = FtesurfFixture::new("dangling-part");
        let stage = fixture.0.join("stage");
        std::fs::create_dir(&stage).unwrap();
        let outside = fixture.0.join("missing-target");
        file_link(&outside, &stage.join("LICENSE.part"));
        let (base, hits, _) = flaky_server(0, b"body".to_vec()).await;
        assert!(
            ftesurf_download(base, &stage, vec![entry("LICENSE", b"body")])
                .await
                .is_err()
        );
        assert_eq!(hits.load(Ordering::SeqCst), 0);
        assert!(!outside.exists());
    }

    #[tokio::test]
    async fn ftesurf_complete_regular_part_is_promoted_without_http() {
        let fixture = FtesurfFixture::new("complete-part");
        let stage = fixture.0.join("stage");
        std::fs::create_dir(&stage).unwrap();
        let body = b"complete verified object";
        std::fs::write(stage.join("LICENSE.part"), body).unwrap();
        let (base, hits, _) = flaky_server(0, body.to_vec()).await;
        let result = ftesurf_download(base, &stage, vec![entry("LICENSE", body)])
            .await
            .unwrap();
        assert!(result.failed.is_empty(), "{:?}", result.failed);
        assert_eq!(result.ok, 1);
        assert_eq!(hits.load(Ordering::SeqCst), 0);
        assert_eq!(std::fs::read(stage.join("LICENSE")).unwrap(), body);
        assert!(!stage.join("LICENSE.part").exists());
    }

    // The server's authored Range response proves the prefix was read and appended, not restarted.
    async fn range_server(
        body: Vec<u8>,
        offset: usize,
        swap: Option<(PathBuf, PathBuf)>,
    ) -> (String, Arc<AtomicUsize>, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let hits = Arc::new(AtomicUsize::new(0));
        let counter = hits.clone();
        let task = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            counter.fetch_add(1, Ordering::SeqCst);
            let mut request = Vec::new();
            loop {
                let mut buf = [0u8; 1024];
                let n = sock.read(&mut buf).await.unwrap();
                assert!(n > 0);
                request.extend_from_slice(&buf[..n]);
                if request.windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
            }
            let request = String::from_utf8(request).unwrap().to_ascii_lowercase();
            if offset > 0 {
                assert!(
                    request.contains(&format!("range: bytes={offset}-")),
                    "{request}"
                );
            } else {
                assert!(!request.contains("range:"), "{request}");
            }
            if let Some((part, outside)) = swap {
                if part.exists() {
                    std::fs::remove_file(&part).unwrap();
                }
                file_link(&outside, &part);
            }
            let status = if offset > 0 {
                "206 Partial Content"
            } else {
                "200 OK"
            };
            let head = format!(
                "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len() - offset
            );
            sock.write_all(head.as_bytes()).await.unwrap();
            sock.write_all(&body[offset..]).await.unwrap();
        });
        (format!("http://{addr}"), hits, task)
    }

    #[tokio::test]
    async fn ftesurf_regular_fresh_and_resumed_downloads_act() {
        for offset in [0, 7] {
            let fixture = FtesurfFixture::new("regular");
            let stage = fixture.0.join("stage");
            std::fs::create_dir(&stage).unwrap();
            let body = b"verified regular download";
            if offset > 0 {
                std::fs::write(stage.join("LICENSE.part"), &body[..offset]).unwrap();
            }
            let (base, hits, server) = range_server(body.to_vec(), offset, None).await;
            let result = ftesurf_download(base, &stage, vec![entry("LICENSE", body)])
                .await
                .unwrap();
            server.await.unwrap();
            assert!(result.failed.is_empty(), "{:?}", result.failed);
            assert_eq!(result.ok, 1);
            assert_eq!(hits.load(Ordering::SeqCst), 1);
            assert_eq!(std::fs::read(stage.join("LICENSE")).unwrap(), body);
            assert!(!stage.join("LICENSE.part").exists());
        }
    }

    #[tokio::test]
    async fn ftesurf_part_link_planted_after_preflight_is_refused_before_write() {
        for offset in [0, 7] {
            let fixture = FtesurfFixture::new("late-part-link");
            let stage = fixture.0.join("stage");
            std::fs::create_dir(&stage).unwrap();
            let outside = fixture.0.join("sentinel");
            std::fs::write(&outside, b"outside bytes").unwrap();
            let part = stage.join("LICENSE.part");
            let body = b"verified regular download";
            if offset > 0 {
                std::fs::write(&part, &body[..offset]).unwrap();
            }
            let (base, hits, server) =
                range_server(body.to_vec(), offset, Some((part, outside.clone()))).await;
            let result = ftesurf_download(base, &stage, vec![entry("LICENSE", body)])
                .await
                .unwrap();
            server.await.unwrap();
            assert_eq!(std::fs::read(&outside).unwrap(), b"outside bytes");
            assert_eq!(result.failed.len(), 1);
            assert_eq!(
                hits.load(Ordering::SeqCst),
                1,
                "permanent local refusal must not retry"
            );
            assert!(!stage.join("LICENSE").exists());
        }
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn ftesurf_executable_staging_and_install_preserve_restrictive_modes() {
        use std::os::unix::{fs::PermissionsExt, process::CommandExt};
        // umask is process-global: isolate the restrictive fresh-create arm from
        // parallel tests instead of changing the parent test runner's permissions.
        if std::env::var_os("FTESURF_RESTRICTIVE_TEST_CHILD").is_none() {
            unsafe extern "C" {
                fn umask(mask: u32) -> u32;
            }
            let mut child = std::process::Command::new(std::env::current_exe().unwrap());
            child.args(["--exact", "download::tests::ftesurf_executable_staging_and_install_preserve_restrictive_modes", "--nocapture"])
                .env("FTESURF_RESTRICTIVE_TEST_CHILD", "1");
            // SAFETY: the pre-exec callback only makes an async-signal-safe umask call.
            unsafe {
                child.pre_exec(|| {
                    umask(0o077);
                    Ok(())
                });
            }
            assert!(
                child.status().unwrap().success(),
                "restrictive-mode child failed"
            );
            return;
        }
        for (offset, mode) in [(0, 0o600), (7, 0o600), (7, 0o640), (24, 0o600), (24, 0o640)] {
            let fixture = FtesurfFixture::new("exec-mode");
            let stage = fixture.0.join("stage");
            let install = fixture.0.join("install");
            std::fs::create_dir(&stage).unwrap();
            std::fs::create_dir(&install).unwrap();
            let body = b"verified complete object";
            if offset > 0 {
                let part = stage.join("ftesurf64.part");
                std::fs::write(&part, &body[..offset]).unwrap();
                std::fs::set_permissions(part, std::fs::Permissions::from_mode(mode)).unwrap();
            }
            let mut f = entry("ftesurf64", body);
            f.exec = true;
            let (base, hits, server) = if offset < body.len() {
                let (base, hits, task) = range_server(body.to_vec(), offset, None).await;
                (base, hits, Some(task))
            } else {
                let (base, hits, _) = flaky_server(0, body.to_vec()).await;
                (base, hits, None)
            };
            let result = ftesurf_download(base, &stage, vec![f]).await.unwrap();
            if let Some(server) = server {
                server.await.unwrap();
            }
            assert!(result.failed.is_empty(), "{:?}", result.failed);
            assert_eq!(result.ok, 1);
            assert_eq!(
                hits.load(Ordering::SeqCst),
                usize::from(offset < body.len())
            );
            let staged = stage.join("ftesurf64");
            assert_eq!(std::fs::read(&staged).unwrap(), body);
            assert_eq!(
                std::fs::metadata(&staged).unwrap().permissions().mode() & 0o777,
                mode,
                "staging widened mode"
            );
            std::fs::rename(staged, install.join("ftesurf64")).unwrap();
            let m: crate::manifest::Manifest = serde_json::from_value(serde_json::json!({
                "schema": 2, "channel": "stable", "version": "0.1.0", "hash_algo": "blake2b-256",
                "exec": {"cmd": "ftesurf64"}, "files": [{"path": "ftesurf64", "size": body.len(),
                    "hash": blake2b_256(body), "exec": true}]
            }))
            .unwrap();
            crate::ftesurf_update::prepare_install(&install, &m)
                .await
                .unwrap();
            let installed_mode = std::fs::metadata(install.join("ftesurf64"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(installed_mode, if mode == 0o600 { 0o700 } else { 0o750 });
            assert_eq!(installed_mode & 0o666, mode);
        }
    }

    fn progress() -> Arc<Progress> {
        Progress::new(0, 1, 1, vec![], Header::default())
    }

    fn blake2b_256(bytes: &[u8]) -> String {
        let mut h = Hasher::new("blake2b-256").unwrap();
        h.update(bytes);
        h.hex()
    }

    fn entry(path: &str, bytes: &[u8]) -> FileEntry {
        FileEntry {
            path: path.to_string(),
            size: bytes.len() as u64,
            hash: blake2b_256(bytes),
            component: String::new(),
            platform: "all".to_string(),
            exec: false,
        }
    }

    /// A .part holding every byte is a finished download interrupted before the rename.
    /// It must be promoted locally, with no request: resuming from EOF asks for an
    /// unsatisfiable range and the server answers 416, which used to fail permanently
    /// because the .part never shrank and every re-run recomputed the same offset.
    #[tokio::test]
    async fn complete_part_is_promoted_without_touching_the_network() {
        let dir = std::env::temp_dir().join(format!("ql-dl-{}", std::process::id()));
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let dest = dir.join("complete.bin");
        let part = dir.join("complete.bin.part");
        let body = b"the whole file, already on disk";
        tokio::fs::write(&part, body).await.unwrap();

        // Deliberately unroutable: reaching the network at all is the failure this guards.
        let res = try_one(
            &reqwest::Client::new(),
            "http://127.0.0.1:1/never",
            "blake2b-256",
            &entry("complete.bin", body),
            &part,
            &dest,
            &progress(),
            0,
            None,
            DownloadPolicy::Quakers,
        )
        .await;

        assert!(res.is_ok(), "expected local promotion, got {:?}", res.err());
        assert_eq!(tokio::fs::read(&dest).await.unwrap(), body);
        assert!(
            !part.exists(),
            ".part should be renamed away, not left behind"
        );
        let _ = tokio::fs::remove_file(&dest).await;
    }

    /// Spins a server that refuses the first `fail_times` connections outright, then serves
    /// the body. Returns its base URL and a counter of connections it accepted.
    async fn flaky_server(fail_times: usize, body: Vec<u8>) -> (String, Arc<AtomicUsize>, u16) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let hits = Arc::new(AtomicUsize::new(0));
        let hits2 = hits.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                let n = hits2.fetch_add(1, Ordering::SeqCst);
                // Drain the request line/headers so the client sees a clean close, not a reset
                // mid-write, which is what a real dropped connection looks like.
                let mut buf = [0u8; 2048];
                let _ = sock.read(&mut buf).await;
                if n < fail_times {
                    drop(sock); // close with no response -> transient error
                    continue;
                }
                let head = format!(
                    "HTTP/1.1 200 OK
Content-Length: {}
Accept-Ranges: bytes

",
                    body.len()
                );
                let _ = sock.write_all(head.as_bytes()).await;
                let _ = sock.write_all(&body).await;
                let _ = sock.flush().await;
            }
        });
        (format!("http://{addr}"), hits, addr.port())
    }

    /// The reported symptom: a first run missed files that a second run fetched fine. With one
    /// mirror the budget was two immediate attempts, so a single dropped connection failed the
    /// file outright. Two consecutive drops must now still end in a completed download.
    #[tokio::test]
    async fn transient_connection_drops_are_retried() {
        let body = b"content that survives a flaky connection".to_vec();
        let (base, hits, port) = flaky_server(2, body.clone()).await;
        let dir = std::env::temp_dir().join(format!("ql-retry-{}-{}", std::process::id(), port));
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let dest = dir.join("flaky.bin");

        let res = download_object(
            &reqwest::Client::new(),
            &[base],
            "blake2b-256",
            &entry("flaky.bin", &body),
            &dest,
            &progress(),
            0,
            None,
            DownloadPolicy::Quakers,
        )
        .await;

        assert!(
            res.is_ok(),
            "expected retry to succeed, got {:?}",
            res.err()
        );
        assert_eq!(tokio::fs::read(&dest).await.unwrap(), body);
        assert_eq!(hits.load(Ordering::SeqCst), 3, "2 drops then 1 success");
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    /// A 404 means the manifest and the mirror disagree. Retrying cannot fix it, so the budget
    /// must not be spent on it -- exactly one request.
    #[tokio::test]
    async fn missing_objects_fail_fast_without_burning_retries() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let hits = Arc::new(AtomicUsize::new(0));
        let hits2 = hits.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                hits2.fetch_add(1, Ordering::SeqCst);
                let mut buf = [0u8; 2048];
                let _ = sock.read(&mut buf).await;
                let _ = sock
                    .write_all(
                        b"HTTP/1.1 404 Not Found
Content-Length: 0

",
                    )
                    .await;
                let _ = sock.flush().await;
            }
        });

        let dir =
            std::env::temp_dir().join(format!("ql-404-{}-{}", std::process::id(), addr.port()));
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let res = download_object(
            &reqwest::Client::new(),
            &[format!("http://{addr}")],
            "blake2b-256",
            &entry("gone.bin", b"whatever"),
            &dir.join("gone.bin"),
            &progress(),
            0,
            None,
            DownloadPolicy::Quakers,
        )
        .await;

        assert!(res.is_err());
        assert_eq!(hits.load(Ordering::SeqCst), 1, "404 must not be retried");
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    /// A zero-byte manifest entry with no .part on disk: "already have every byte" and
    /// "have nothing at all" both measure 0. Conflating them made the launcher rename a
    /// .part that was never created — os error 2 — and, like the 416, it was permanent,
    /// because nothing about the state changed between runs. Two real files hit this:
    /// quakers/gibfiltr.cfg and quakers/impfiltr.cfg.
    #[tokio::test]
    async fn zero_byte_files_are_downloaded_not_renamed_from_nothing() {
        let empty: Vec<u8> = Vec::new();
        let (base, hits, port) = flaky_server(0, empty.clone()).await;
        let dir = std::env::temp_dir().join(format!("ql-zero-{}-{}", std::process::id(), port));
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let dest = dir.join("gibfiltr.cfg");

        let res = download_object(
            &reqwest::Client::new(),
            &[base],
            "blake2b-256",
            &entry("gibfiltr.cfg", &empty),
            &dest,
            &progress(),
            0,
            None,
            DownloadPolicy::Quakers,
        )
        .await;

        assert!(res.is_ok(), "zero-byte file failed: {:?}", res.err());
        assert_eq!(tokio::fs::metadata(&dest).await.unwrap().len(), 0);
        assert_eq!(
            hits.load(Ordering::SeqCst),
            1,
            "it must actually fetch, not short-circuit"
        );
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    /// A .part longer than the manifest says must not be promoted just because a reset made
    /// the offset coincide with a zero-length target.
    #[tokio::test]
    async fn oversized_part_is_not_promoted() {
        let empty: Vec<u8> = Vec::new();
        let (base, _hits, port) = flaky_server(0, empty.clone()).await;
        let dir = std::env::temp_dir().join(format!("ql-over-{}-{}", std::process::id(), port));
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let dest = dir.join("over.cfg");
        let part = dir.join("over.cfg.part");
        tokio::fs::write(&part, b"leftover junk from a previous build")
            .await
            .unwrap();

        let res = download_object(
            &reqwest::Client::new(),
            &[base],
            "blake2b-256",
            &entry("over.cfg", &empty),
            &dest,
            &progress(),
            0,
            None,
            DownloadPolicy::Quakers,
        )
        .await;

        assert!(res.is_ok(), "{:?}", res.err());
        assert_eq!(
            tokio::fs::metadata(&dest).await.unwrap().len(),
            0,
            "the 35-byte leftover must not have been installed as the file"
        );
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[test]
    fn backoff_grows_and_is_capped() {
        let a = backoff(0).as_millis();
        let b = backoff(1).as_millis();
        assert!(a >= 300 && a < 600, "first wait {a}ms");
        assert!(b >= 600 && b < 1000, "second wait {b}ms");
        for n in 0..8 {
            assert!(
                backoff(n) <= std::time::Duration::from_millis(3250),
                "capped at attempt {n}"
            );
        }
    }

    /// Full length but wrong bytes has nothing to resume from, so the .part must be
    /// discarded rather than range-requested past EOF.
    #[tokio::test]
    async fn complete_but_corrupt_part_is_discarded() {
        let dir = std::env::temp_dir().join(format!("ql-dl-c-{}", std::process::id()));
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let dest = dir.join("corrupt.bin");
        let part = dir.join("corrupt.bin.part");
        tokio::fs::write(&part, b"xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx")
            .await
            .unwrap();

        let res = try_one(
            &reqwest::Client::new(),
            "http://127.0.0.1:1/never",
            "blake2b-256",
            &entry("corrupt.bin", b"the whole file, already on disk"),
            &part,
            &dest,
            &progress(),
            0,
            None,
            DownloadPolicy::Quakers,
        )
        .await;

        // It must fail on the connection, NOT on a 416: that proves it reset the offset
        // to 0 and issued a plain GET instead of resuming from a bad partial.
        assert!(res.is_err());
        let msg = res.unwrap_err().to_string();
        assert!(
            !msg.contains("416"),
            "should not have range-requested: {msg}"
        );
        assert!(!dest.exists());
    }
}
