//! Parallel, resumable, self-verifying downloader feeding the ratatui UI.
//!
//! N worker tasks pull files off a shared index. Each file streams to `<dest>.part`
//! (HTTP Range-resumed if a partial exists), is hashed on the fly, and is atomically
//! renamed into place only once the hash matches the manifest. On any per-file error
//! we fail over to the next mirror; the `.part` survives so the retry resumes.

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
    // Cycle mirrors with one extra pass: a hash mismatch clears the .part, so a
    // single working mirror still gets a clean-slate retry within the same run.
    let attempts = mirrors.len() + 1;
    for attempt in 0..attempts {
        let base = &mirrors[attempt % mirrors.len()];
        let url = format!("{}/{}", base.trim_end_matches('/'), obj_rel);
        match try_one(client, &url, algo, entry, &part, dest, progress, wid, cat).await {
            Ok(()) => return Ok(()),
            Err(e) => last_err = anyhow!("mirror {}: {}", (attempt % mirrors.len()) + 1, e),
        }
    }
    Err(last_err)
}

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

    let mut req = client.get(url);
    if offset > 0 {
        req = req.header(RANGE, format!("bytes={}-", offset));
    }
    let resp = req.send().await?;
    let status = resp.status();
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
