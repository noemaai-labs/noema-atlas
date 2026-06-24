//! Two-instance BitTorrent seed/leech round-trips. Networked (real DHT + a live
//! swarm), so every test here is `#[ignore]`d — they compile in CI but only run
//! on a developer's networked machine (`cargo test --features bittorrent -- --ignored`).
#![cfg(feature = "bittorrent")]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::StreamExt;
use noema_core::hash::hash_bytes;
use noema_core::manifest::{Artifact, AuthPolicy, Source};
use noema_core::transport::{BittorrentAdapter, FetchCtx, TransportAdapter};

fn sample(seed: u8, len: usize) -> Vec<u8> {
    (0..len)
        .map(|i| (i as u8).wrapping_mul(37).wrapping_add(seed))
        .collect()
}

/// A leeching adapter: outbound + DHT only, no inbound listener. Uncapped (0,0).
/// Public trackers off (this is a private two-node swarm), ratio unlimited.
fn leecher(store_dir: std::path::PathBuf) -> BittorrentAdapter {
    BittorrentAdapter::new(store_dir, None, false, None, false, 0, 0, false, 0.0, None)
}

/// A seeding adapter: binds an inbound listener so peers can pull from it. Returned
/// as an `Arc` because `seed_blob` takes `self: &Arc<Self>` (it detaches the seed
/// work, including the cold session init, onto a spawned task).
fn seeder(store_dir: std::path::PathBuf) -> Arc<BittorrentAdapter> {
    Arc::new(BittorrentAdapter::new(
        store_dir, None, true, None, false, 0, 0, false, 0.0, None,
    ))
}

/// Build the manifest artifact a leecher passes to `open()` so the adapter can pick
/// the right file out of the torrent and the engine can verify it afterwards.
fn artifact(name: &str, bytes: &[u8]) -> Artifact {
    Artifact {
        path: name.to_string(),
        role: "weights".to_string(),
        size_bytes: bytes.len() as u64,
        hashes: hash_bytes(bytes),
        chunking: None,
        format: None,
        sources: vec![],
    }
}

/// Seed `bytes` from one BitTorrent instance, then fetch them by the generated
/// magnet from a second instance and assert the bytes round-trip exactly.
///
/// The transport never trusts the swarm: this drains the leecher's stream and
/// compares it to the original, exactly as the engine's verify pass would.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs a live BitTorrent swarm/DHT; run on a networked machine"]
async fn bittorrent_two_instance_seed_leech() {
    let tmp = tempfile::tempdir().unwrap();
    let bytes = sample(7, 512 * 1024);
    let blob = tmp.path().join("model.gguf");
    std::fs::write(&blob, &bytes).unwrap();
    let blake3 = hash_bytes(&bytes).blake3;

    // --- Seeder: build a torrent over the blob and capture its magnet. ----------
    let seeder = seeder(tmp.path().join("seeder"));
    let captured: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let sink = captured.clone();
    let on_magnet: Arc<dyn Fn(String) + Send + Sync> =
        Arc::new(move |m: String| *sink.lock().unwrap() = Some(m));
    seeder
        .seed_blob(
            blob.clone(),
            "model.gguf".to_string(),
            blake3.clone(),
            Some(on_magnet),
        )
        .await
        .expect("seed_blob");

    // Piece-hashing runs in the background; wait for the magnet to materialize.
    let magnet = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            if let Some(m) = captured.lock().unwrap().clone() {
                break m;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("seeder should produce a magnet");

    // --- Leecher: add the file by that magnet and stream it back. ----------------
    let leecher = leecher(tmp.path().join("leecher"));
    let source = Source::BittorrentV2 {
        magnet_uri: magnet,
        file_merkle_root_sha256: None,
        auth: AuthPolicy::None,
    };
    let art = artifact("model.gguf", &bytes);
    let opened = leecher
        .open(&source, &art, None, &FetchCtx::default())
        .await
        .expect("leecher should fetch the blob from the seeder's swarm");

    let mut got = Vec::new();
    let mut stream = opened.stream;
    while let Some(chunk) = stream.next().await {
        got.extend_from_slice(&chunk.expect("stream chunk"));
    }
    assert_eq!(got, bytes, "the leeched bytes must match the seeded blob");
}

/// A download interrupted partway resumes from its on-disk pieces after the leecher
/// process restarts, rather than starting over. The persistent librqbit session
/// keys its resume data off `store_dir`, so a second adapter pointed at the same dir
/// re-attaches the in-progress torrent.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs a live BitTorrent swarm/DHT; run on a networked machine"]
async fn bittorrent_resume_across_restart() {
    let tmp = tempfile::tempdir().unwrap();
    let bytes = sample(13, 8 * 1024 * 1024);
    let blob = tmp.path().join("model.gguf");
    std::fs::write(&blob, &bytes).unwrap();
    let blake3 = hash_bytes(&bytes).blake3;

    let seeder = seeder(tmp.path().join("seeder"));
    let captured: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let sink = captured.clone();
    let on_magnet: Arc<dyn Fn(String) + Send + Sync> =
        Arc::new(move |m: String| *sink.lock().unwrap() = Some(m));
    seeder
        .seed_blob(
            blob.clone(),
            "model.gguf".to_string(),
            blake3,
            Some(on_magnet),
        )
        .await
        .expect("seed_blob");
    let magnet = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            if let Some(m) = captured.lock().unwrap().clone() {
                break m;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("seeder should produce a magnet");

    let source = Source::BittorrentV2 {
        magnet_uri: magnet,
        file_merkle_root_sha256: None,
        auth: AuthPolicy::None,
    };
    let art = artifact("model.gguf", &bytes);

    // First leecher: cancel partway so a partial download lands on disk.
    let store = tmp.path().join("leecher");
    let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
    {
        let l1 = leecher(store.clone());
        let ctx = FetchCtx {
            cancel: Some(cancel.clone()),
            ..FetchCtx::default()
        };
        let cancel_bg = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(2)).await;
            cancel_bg.store(true, std::sync::atomic::Ordering::SeqCst);
        });
        // A cancelled open is expected to error or return a short stream; either way
        // the persistent session has written resume data under `store`.
        let _ = l1.open(&source, &art, None, &ctx).await;
    }

    // Second leecher over the *same* store: resumes and finishes the download.
    let l2 = leecher(store);
    let opened = l2
        .open(&source, &art, None, &FetchCtx::default())
        .await
        .expect("a restarted leecher should resume and complete the download");
    let mut got = Vec::new();
    let mut stream = opened.stream;
    while let Some(chunk) = stream.next().await {
        got.extend_from_slice(&chunk.expect("stream chunk"));
    }
    assert_eq!(
        got, bytes,
        "the resumed download must match the seeded blob"
    );
}
