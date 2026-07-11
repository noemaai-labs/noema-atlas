//! Two-instance BitTorrent seed/leech round-trips. Networked, so every test is
//! `#[ignore]`d: `cargo test --features bittorrent -- --ignored`.
#![cfg(feature = "bittorrent")]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::StreamExt;
use noema_core::hash::hash_bytes;
use noema_core::manifest::{Artifact, AuthPolicy, Source};
use noema_core::transport::{BittorrentAdapter, FetchCtx, TransportAdapter, TransportConfig};

fn sample(seed: u8, len: usize) -> Vec<u8> {
    (0..len)
        .map(|i| (i as u8).wrapping_mul(37).wrapping_add(seed))
        .collect()
}

/// Adapter config for one test instance: outbound + DHT only, public trackers off (private two-node swarm).
fn bt_cfg(store_dir: std::path::PathBuf, seed: bool) -> TransportConfig {
    TransportConfig {
        bittorrent_store_dir: store_dir,
        bittorrent_seed: seed,
        bittorrent_listen_port_range: None,
        bittorrent_enable_upnp: false,
        bittorrent_use_public_trackers: false,
        ..TransportConfig::default()
    }
}

/// A leeching adapter.
fn leecher(store_dir: std::path::PathBuf) -> BittorrentAdapter {
    BittorrentAdapter::new(&bt_cfg(store_dir, false), None)
}

/// A seeding adapter. `Arc` because `seed_blob` takes `self: &Arc<Self>`.
fn seeder(store_dir: std::path::PathBuf) -> Arc<BittorrentAdapter> {
    Arc::new(BittorrentAdapter::new(&bt_cfg(store_dir, true), None))
}

/// Manifest artifact a leecher passes to `open()`, used to select the file and verify it.
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

/// Seed `bytes` from one instance, fetch them by magnet from a second, and assert an exact round-trip.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs a live BitTorrent swarm/DHT; run on a networked machine"]
async fn bittorrent_two_instance_seed_leech() {
    let tmp = tempfile::tempdir().unwrap();
    let bytes = sample(7, 512 * 1024);
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

/// A download interrupted partway resumes from on-disk pieces after the leecher restarts.
/// The persistent librqbit session keys resume data off `store_dir`, so a second adapter on the same dir re-attaches.
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
