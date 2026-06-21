#![cfg(feature = "iroh")]

use std::path::Path;

use noema_core::iroh_node::IrohNode;

fn sample(seed: u8, len: usize) -> Vec<u8> {
    (0..len)
        .map(|i| (i as u8).wrapping_mul(37).wrapping_add(seed))
        .collect()
}

async fn seed_and_fetch(provider: &IrohNode, fetcher: &IrohNode, src: &Path, dest: &Path) -> bool {
    let blake3 = provider.seed_file(src).await.expect("seed");
    let ticket = provider.node_ticket().await.expect("ticket");
    let size = std::fs::metadata(src).unwrap().len();
    fetcher
        .fetch_from_providers(&blake3, &[ticket], dest, size, None, None)
        .await
        .is_ok()
}

/// A blob seeded on one node can be fetched, byte-for-byte, by another — i.e. the
/// `BlobServer` accept-handler serves exactly like the stock one.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn serves_blob_to_a_peer() {
    let tmp = tempfile::tempdir().unwrap();
    let provider = IrohNode::spawn(&tmp.path().join("provider")).await.unwrap();
    let fetcher = IrohNode::spawn(&tmp.path().join("fetcher")).await.unwrap();

    let src = tmp.path().join("model.bin");
    let bytes = sample(7, 256 * 1024);
    std::fs::write(&src, &bytes).unwrap();
    let dest = tmp.path().join("out.bin");

    assert!(
        seed_and_fetch(&provider, &fetcher, &src, &dest).await,
        "peer should be able to fetch the seeded blob"
    );
    assert_eq!(
        std::fs::read(&dest).unwrap(),
        bytes,
        "bytes must round-trip"
    );
}

/// After `unseed_and_disconnect`, the blob is no longer served — a fresh fetch
/// from a peer fails. (With no active transfer there are no connections to sever,
/// so this checks the unseed half goes through the new code path.)
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unseed_and_disconnect_stops_serving() {
    let tmp = tempfile::tempdir().unwrap();
    let provider = IrohNode::spawn(&tmp.path().join("provider")).await.unwrap();
    let fetcher = IrohNode::spawn(&tmp.path().join("fetcher")).await.unwrap();

    let src = tmp.path().join("model.bin");
    let bytes = sample(9, 128 * 1024);
    std::fs::write(&src, &bytes).unwrap();

    let blake3 = provider.seed_file(&src).await.expect("seed");
    let ticket = provider.node_ticket().await.expect("ticket");
    let size = bytes.len() as u64;
    let dest1 = tmp.path().join("out1.bin");
    assert!(provider.metrics().active_uploads_for_hex(&blake3).eq(&0));
    assert!(fetcher
        .fetch_from_providers(&blake3, &[ticket.clone()], &dest1, size, None, None)
        .await
        .is_ok());
    provider
        .unseed_and_disconnect(&blake3)
        .await
        .expect("unseed");
    let dest2 = tmp.path().join("out2.bin");
    let fetched = fetcher
        .fetch_from_providers(&blake3, &[ticket], &dest2, size, None, None)
        .await;
    assert!(
        fetched.is_err(),
        "blob should no longer be served after unseed_and_disconnect"
    );
}

/// The heart of the fix: a peer pulling *this file* is detectable per-blob (so the
/// UI can warn), and `unseed_and_disconnect` severs it mid-transfer — the
/// provider's per-blob active count goes positive during the pull and drops back
/// to zero once disconnected, and the peer's fetch does not complete.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn hard_disconnects_a_peer_mid_transfer() {
    use std::sync::Arc;
    use std::time::Duration;

    let tmp = tempfile::tempdir().unwrap();
    let provider = Arc::new(IrohNode::spawn(&tmp.path().join("provider")).await.unwrap());
    let fetcher = Arc::new(IrohNode::spawn(&tmp.path().join("fetcher")).await.unwrap());

    let src = tmp.path().join("model.bin");
    let bytes = sample(13, 128 * 1024 * 1024);
    std::fs::write(&src, &bytes).unwrap();

    let blake3 = provider.seed_file(&src).await.expect("seed");
    let ticket = provider.node_ticket().await.expect("ticket");
    let size = bytes.len() as u64;

    let dest = tmp.path().join("out.bin");
    let f = fetcher.clone();
    let b3 = blake3.clone();
    let t = ticket.clone();
    let d = dest.clone();
    let fetch = tokio::spawn(
        async move { f.fetch_from_providers(&b3, &[t], &d, size, None, None).await },
    );

    let metrics = provider.metrics();
    // The provider registers a pull of *this exact blob*.
    let saw_active = tokio::time::timeout(Duration::from_secs(10), async {
        while metrics.active_uploads_for_hex(&blake3) == 0 {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await;
    assert!(
        saw_active.is_ok(),
        "provider never registered an active pull of this blob"
    );

    // Sever exactly that peer.
    provider
        .unseed_and_disconnect(&blake3)
        .await
        .expect("disconnect");

    // Provider-side: the active pull is gone (the connection was closed).
    let drained = tokio::time::timeout(Duration::from_secs(10), async {
        while metrics.active_uploads_for_hex(&blake3) != 0 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await;
    assert!(
        drained.is_ok(),
        "provider should drop the per-blob active count after a hard disconnect"
    );

    // Peer-side: the fetch must not complete successfully.
    let outcome = tokio::time::timeout(Duration::from_secs(15), fetch).await;
    let succeeded = matches!(outcome, Ok(Ok(Ok(()))));
    assert!(
        !succeeded,
        "fetch must not complete successfully after a hard disconnect mid-transfer"
    );
}

/// The same blob seeded on two peers is fetched from both at once (size is over the
/// multi-peer threshold, so the striped path runs) and assembled byte-for-byte.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn aggregates_one_blob_from_two_peers() {
    let tmp = tempfile::tempdir().unwrap();
    let p1 = IrohNode::spawn(&tmp.path().join("p1")).await.unwrap();
    let p2 = IrohNode::spawn(&tmp.path().join("p2")).await.unwrap();
    let fetcher = IrohNode::spawn(&tmp.path().join("fetcher")).await.unwrap();

    // 12 MiB > MULTIPEER_MIN_BYTES, so it stripes across the two peers.
    let src = tmp.path().join("model.bin");
    let bytes = sample(21, 12 * 1024 * 1024);
    std::fs::write(&src, &bytes).unwrap();

    let b1 = p1.seed_file(&src).await.expect("seed p1");
    let b2 = p2.seed_file(&src).await.expect("seed p2");
    assert_eq!(b1, b2, "identical bytes must yield the same blake3");

    let tickets = vec![
        p1.node_ticket().await.expect("ticket p1"),
        p2.node_ticket().await.expect("ticket p2"),
    ];
    let dest = tmp.path().join("out.bin");
    fetcher
        .fetch_from_providers(&b1, &tickets, &dest, bytes.len() as u64, None, None)
        .await
        .expect("striped fetch from two peers should succeed");

    assert_eq!(
        std::fs::read(&dest).unwrap(),
        bytes,
        "the file assembled from two peers must match byte-for-byte"
    );
}
