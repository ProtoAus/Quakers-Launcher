//! Client-free native terminal controls. Never starts a game or contacts an official feed.
use anyhow::{ensure, Result};
use quakers_launcher::{
    download, ftesurf_ui,
    hashing::Hasher,
    manifest::FileEntry,
    ui::{Header, Progress},
};
use std::{
    path::PathBuf,
    sync::atomic::Ordering,
    time::{Duration, Instant},
};

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    ensure!(ftesurf_ui::interactive(false), "native terminal required");
    if args.first().is_some_and(|a| a == "--transfer") {
        ensure!(
            args.len() == 3,
            "--transfer <owned-empty-staging-root> <loopback-url>"
        );
        let root = PathBuf::from(&args[1]);
        ensure!(
            root.is_dir() && std::fs::read_dir(&root)?.next().is_none(),
            "staging root must be an empty owned directory"
        );
        ensure!(
            args[2].starts_with("http://127.0.0.1:"),
            "loopback fixture only"
        );
        let bytes = vec![b'W'; 1024 * 1024];
        let mut hash = Hasher::new("blake2b-256")?;
        hash.update(&bytes);
        let entry = FileEntry {
            path: "ftesurf/menu.dat".into(),
            size: bytes.len() as u64,
            hash: hash.hex(),
            component: String::new(),
            platform: "all".into(),
            exec: false,
        };
        let result = download::download_all_ftesurf_with_ui(
            reqwest::Client::new(),
            vec![args[2].clone()],
            "blake2b-256".into(),
            root.clone(),
            vec![entry],
            bytes.len() as u64,
            1,
            Header {
                release: "UI test fixture".into(),
                mirror: "loopback only".into(),
                ..Header::default()
            },
        )
        .await;
        ensure!(result.is_err(), "expected controlled download cancellation");
        ensure!(
            result
                .err()
                .unwrap()
                .to_string()
                .contains("download cancelled"),
            "wrong failure arm"
        );
        ensure!(
            !root.join("ftesurf/menu.dat").exists(),
            "unverified download was promoted"
        );
        let part = root.join("ftesurf/menu.dat.part");
        ensure!(
            part.is_file() && std::fs::metadata(part)?.len() > 0,
            "transfer must ACT before cancellation"
        );
        println!("UI_PROBE_TRANSFER_CANCELLED");
        return Ok(());
    }
    let normal_stop = args.first().is_some_and(|a| a == "--normal-stop");
    let p = Progress::new(
        1024 * 1024,
        1,
        1,
        vec![("Game data", 1024 * 1024)],
        Header {
            release: "UI test fixture".into(),
            mirror: "no network".into(),
            ..Header::default()
        },
    );
    p.worker_start(0, "fixture.dat (no download)", 1024 * 1024);
    p.worker_set_cur(0, 512 * 1024);
    let (stop, cancelled, handle) = ftesurf_ui::start(p).expect("interactive terminal");
    let start = Instant::now();
    while !cancelled.load(Ordering::Relaxed) && start.elapsed() < Duration::from_secs(8) {
        if normal_stop && start.elapsed() > Duration::from_millis(1500) {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    stop.store(true, Ordering::Relaxed);
    handle.join().expect("renderer thread panicked");
    ensure!(
        normal_stop || cancelled.load(Ordering::Relaxed),
        "terminal cancellation did not arrive"
    );
    println!(
        "{}",
        if normal_stop {
            "UI_PROBE_STOPPED"
        } else {
            "UI_PROBE_CANCELLED"
        }
    );
    Ok(())
}
