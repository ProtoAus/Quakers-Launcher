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
    let cat_idx: Arc<Vec<Option<usize>>> =
        Arc::new(jobs.iter().map(|e| progress.category_index(&e.path)).collect());
    let ui_handle = ui::start(progress.clone());

    let jobs = Arc::new(jobs);
    let idx = Arc::new(AtomicUsize::new(0));
    let ok = Arc::new(AtomicU64::new(0));
    let failed = Arc::new(tokio::sync::Mutex::new(Vec::<(String, String)>::new()));

    let mut handles = Vec::new();
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

        handles.push(tokio::spawn(async move {
            loop {
                let i = idx.fetch_add(1, Ordering::SeqCst);
                if i >= jobs.len() {
                    break;
                }
                let entry = &jobs[i];
                let dest = install_dir.join(&entry.path);
                let cat = cat_idx[i];
                progress.worker_start(wid, &short_name(&entry.path), entry.size);

                match download_object(&client, &mirrors, &algo, entry, &dest, &progress, wid, cat).await {
                    Ok(()) => {
                        ok.fetch_add(1, Ordering::SeqCst);
                        progress.file_done();
                    }
                    Err(e) => failed.lock().await.push((entry.path.clone(), e.to_string())),
                }
            }
            progress.worker_idle(wid);
        }));
    }

    for h in handles {
        let _ = h.await;
    }
    if let Some((stop, handle)) = ui_handle {
        stop.store(true, Ordering::Relaxed);
        let _ = handle.join();
    }

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
) -> Result<()> {
    if let Some(parent) = dest.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let mut part = dest.as_os_str().to_owned();
    part.push(".part");
    let part = PathBuf::from(part);

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
        match try_one(client, &url, algo, entry, &part, dest, progress, wid, cat).await {
            Ok(()) => return Ok(()),
            Err(e) => {
                // A 404/410 means the manifest and the mirror disagree about what exists.
                // No amount of waiting fixes that, and retrying would only delay the report.
                if e.downcast_ref::<Permanent>().is_some() {
                    return Err(anyhow!("mirror {}: {}", mirror_no + 1, e));
                }
                last_err = anyhow!("mirror {}: {}", mirror_no + 1, e);
                if attempt + 1 < attempts {
                    // Show the wait in the UI, so a backing-off worker does not look frozen.
                    progress.worker_start(
                        wid,
                        &format!("{} (retry {}/{})", short_name(&entry.path), attempt + 1, attempts - 1),
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
    let base = std::time::Duration::from_millis(300u64 << attempt.min(4)).min(std::time::Duration::from_secs(3));
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
) -> Result<()> {
    let mut offset: u64 = tokio::fs::metadata(part).await.map(|m| m.len()).unwrap_or(0);
    if offset > entry.size {
        offset = 0;
    }

    // Reconstruct hasher state from the existing prefix.
    let mut hasher = Hasher::new(algo)?;
    if offset > 0 {
        let mut f = tokio::fs::File::open(part).await?;
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

    // A .part holding every byte is not a resume -- it is a finished download that was
    // interrupted between the last write and the rename below. Do NOT range-request from
    // here: `Range: bytes=<size>-` starts at EOF, which RFC 7233 defines as unsatisfiable,
    // so the server answers 416 and the old code reported a hard failure that re-running
    // could never clear (the .part never shrank, so every retry asked for the same range).
    // The bytes are already local, so this needs no request at all.
    if offset == entry.size {
        let got = hasher.hex();
        if got == entry.hash {
            return finalize(entry, part, dest).await;
        }
        // Full length but wrong content: nothing to resume from. Discard and refetch clean.
        let _ = tokio::fs::remove_file(part).await;
        offset = 0;
        hasher = Hasher::new(algo)?;
        progress.worker_set_cur(wid, 0);
    }

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
        let _ = tokio::fs::remove_file(part).await;
        return Err(anyhow!("HTTP 416 (discarded stale .part; retry starts clean)"));
    }
    if status == StatusCode::NOT_FOUND || status == StatusCode::GONE {
        return Err(anyhow!(Permanent(format!("HTTP {status}"))));
    }
    if !status.is_success() {
        return Err(anyhow!("HTTP {}", status));
    }

    // Asked for a range but got the whole file (200, not 206) -> start over.
    let restart = offset > 0 && status != StatusCode::PARTIAL_CONTENT;
    let mut file = if restart || offset == 0 {
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
        let _ = tokio::fs::remove_file(part).await;
        return Err(anyhow!(
            "hash mismatch (got {}… want {}…)",
            &got[..8.min(got.len())],
            &entry.hash[..8.min(entry.hash.len())]
        ));
    }

    finalize(entry, part, dest).await
}

/// Promote a verified `.part` to its final name. Split out so the "already complete"
/// short-circuit above lands the file by exactly the same path a fresh download does.
async fn finalize(entry: &FileEntry, part: &Path, dest: &Path) -> Result<()> {
    if tokio::fs::metadata(dest).await.is_ok() {
        let _ = tokio::fs::remove_file(dest).await;
    }
    tokio::fs::rename(part, dest).await?;
    set_exec_bit(entry, dest).await;
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
        )
        .await;

        assert!(res.is_ok(), "expected local promotion, got {:?}", res.err());
        assert_eq!(tokio::fs::read(&dest).await.unwrap(), body);
        assert!(!part.exists(), ".part should be renamed away, not left behind");
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
                let Ok((mut sock, _)) = listener.accept().await else { return };
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
        )
        .await;

        assert!(res.is_ok(), "expected retry to succeed, got {:?}", res.err());
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
                let Ok((mut sock, _)) = listener.accept().await else { return };
                hits2.fetch_add(1, Ordering::SeqCst);
                let mut buf = [0u8; 2048];
                let _ = sock.read(&mut buf).await;
                let _ = sock
                    .write_all(b"HTTP/1.1 404 Not Found
Content-Length: 0

")
                    .await;
                let _ = sock.flush().await;
            }
        });

        let dir = std::env::temp_dir().join(format!("ql-404-{}-{}", std::process::id(), addr.port()));
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
        )
        .await;

        assert!(res.is_err());
        assert_eq!(hits.load(Ordering::SeqCst), 1, "404 must not be retried");
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[test]
    fn backoff_grows_and_is_capped() {
        let a = backoff(0).as_millis();
        let b = backoff(1).as_millis();
        assert!(a >= 300 && a < 600, "first wait {a}ms");
        assert!(b >= 600 && b < 1000, "second wait {b}ms");
        for n in 0..8 {
            assert!(backoff(n) <= std::time::Duration::from_millis(3250), "capped at attempt {n}");
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
        tokio::fs::write(&part, b"xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx").await.unwrap();

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
        )
        .await;

        // It must fail on the connection, NOT on a 416: that proves it reset the offset
        // to 0 and issued a plain GET instead of resuming from a bad partial.
        assert!(res.is_err());
        let msg = res.unwrap_err().to_string();
        assert!(!msg.contains("416"), "should not have range-requested: {msg}");
        assert!(!dest.exists());
    }
}
