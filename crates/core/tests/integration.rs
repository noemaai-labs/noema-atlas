mod common;

use common::{Mode, TestServer};
use noema_core::engine::{Engine, EngineConfig, EvictPolicy};
use noema_core::hash::hash_bytes;
use noema_core::manifest::{
    Access, Artifact, AuthPolicy, License, Manifest, Model, Publisher, RedistributionClass, Source,
    SCHEMA_VERSION,
};
use noema_core::platform::PlatformProfile;
use noema_core::policy::PolicyConfig;
use noema_core::sign::KeyPair;
use std::path::Path;

fn content(seed: u8, len: usize) -> Vec<u8> {
    (0..len)
        .map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed))
        .collect()
}

#[allow(clippy::too_many_arguments)]
fn manifest_bytes(
    id: &str,
    name: &str,
    path: &str,
    data: &[u8],
    sources: Vec<Source>,
    gated: bool,
    sign: bool,
    key: &KeyPair,
) -> Vec<u8> {
    let redistribution = if gated {
        RedistributionClass::GatedNoRedistribution
    } else {
        RedistributionClass::PublicP2pAllowed
    };
    let mut m = Manifest {
        schema_version: SCHEMA_VERSION.into(),
        manifest_id: id.into(),
        publisher: Publisher {
            id: "test-publisher".into(),
            display_name: None,
            public_keys: vec![],
        },
        model: Model {
            name: name.into(),
            family: None,
            architecture: None,
            revision: None,
            format: None,
            quantization: None,
        },
        license: License {
            spdx: "apache-2.0".into(),
            license_url: None,
            redistribution,
        },
        access: Access {
            gated,
            require_signed_manifest: gated,
            allowed_source_classes: vec![],
        },
        artifacts: vec![Artifact {
            path: path.into(),
            role: "weights".into(),
            size_bytes: data.len() as u64,
            hashes: hash_bytes(data),
            chunking: None,
            format: None,
            sources,
        }],
        provenance: None,
        signatures: vec![],
    };
    if sign {
        key.sign_manifest(&mut m).unwrap();
    }
    m.to_json_pretty().unwrap().into_bytes()
}

fn mirror(url: String) -> Source {
    Source::HttpsMirror {
        url,
        auth: AuthPolicy::None,
    }
}

fn engine_at(root: &Path, trusted: &[String]) -> EngineConfig {
    let mut cfg = EngineConfig::new(root);
    cfg.platform = PlatformProfile::desktop();
    cfg.policy = PolicyConfig {
        trusted_keys: trusted.iter().cloned().collect(),
        ..Default::default()
    };
    cfg
}

#[tokio::test]
async fn same_file_from_two_sources_dedups_to_one_blob() {
    let key = KeyPair::generate();
    let server = TestServer::start().await;
    let data = content(1, 300_000);
    server.put("/a/model.gguf", data.clone(), Mode::Ok);
    server.put("/b/model.gguf", data.clone(), Mode::Ok);

    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(engine_at(dir.path(), &[])).unwrap();
    let m1 = manifest_bytes(
        "mdl_a",
        "Model A",
        "model.gguf",
        &data,
        vec![mirror(server.url("/a/model.gguf"))],
        false,
        true,
        &key,
    );
    let r1 = engine.import_manifest(&m1).unwrap();
    let out1 = engine.download(&r1.manifest_id, None).await.unwrap();
    assert!(!out1.artifacts[0].from_cache);

    // Manifest B via mirror /b — identical content => CAS hit, no fetch.
    let m2 = manifest_bytes(
        "mdl_b",
        "Model B",
        "model.gguf",
        &data,
        vec![mirror(server.url("/b/model.gguf"))],
        false,
        true,
        &key,
    );
    let r2 = engine.import_manifest(&m2).unwrap();
    let out2 = engine.download(&r2.manifest_id, None).await.unwrap();
    assert!(out2.artifacts[0].from_cache, "second download should dedup");

    // /b was never requested.
    assert_eq!(server.hits("/b/model.gguf"), 0);
    // Exactly one blob in the cache.
    assert_eq!(engine.list_cache().unwrap().len(), 1);
    assert_eq!(engine.cas().total_blob_bytes().unwrap(), data.len() as u64);
}

#[tokio::test]
async fn corrupt_mirror_is_rejected_and_failover_succeeds() {
    let key = KeyPair::generate();
    let server = TestServer::start().await;
    let data = content(2, 250_000);
    server.put("/bad/model.gguf", data.clone(), Mode::Corrupt);
    server.put("/good/model.gguf", data.clone(), Mode::Ok);

    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(engine_at(dir.path(), &[])).unwrap();
    let m = manifest_bytes(
        "mdl_failover",
        "Failover",
        "model.gguf",
        &data,
        vec![
            mirror(server.url("/bad/model.gguf")),
            mirror(server.url("/good/model.gguf")),
        ],
        false,
        true,
        &key,
    );
    let r = engine.import_manifest(&m).unwrap();
    let out = engine.download(&r.manifest_id, None).await.unwrap();

    assert!(!out.artifacts[0].from_cache);
    assert!(
        out.artifacts[0]
            .source_id
            .as_deref()
            .unwrap()
            .contains("/good/"),
        "should have succeeded via the good mirror"
    );
    let bad_id = format!("https:{}", server.url("/bad/model.gguf"));
    let health = engine.report_source_health().unwrap();
    let bad = health.iter().find(|h| h.source_id == bad_id).unwrap();
    assert!(bad.banned, "corrupt source must be banned");
    assert_eq!(bad.integrity_failures, 1);
}

#[tokio::test]
async fn poisoned_single_source_fails_and_leaves_cache_empty() {
    let key = KeyPair::generate();
    let server = TestServer::start().await;
    let data = content(9, 100_000);
    server.put("/p/model.gguf", data.clone(), Mode::Corrupt);

    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(engine_at(dir.path(), &[])).unwrap();
    let m = manifest_bytes(
        "mdl_poison",
        "Poison",
        "model.gguf",
        &data,
        vec![mirror(server.url("/p/model.gguf"))],
        false,
        true,
        &key,
    );
    let r = engine.import_manifest(&m).unwrap();
    let err = engine.download(&r.manifest_id, None).await;
    assert!(err.is_err(), "poisoned download must fail");
    assert_eq!(engine.list_cache().unwrap().len(), 0, "nothing committed");
    assert_eq!(engine.cas().total_blob_bytes().unwrap(), 0);
}

#[tokio::test]
async fn resume_after_interruption_continues() {
    let key = KeyPair::generate();
    let server = TestServer::start().await;
    let data = content(3, 400_000);
    server.put(
        "/t/model.gguf",
        data.clone(),
        Mode::TruncateFirst(data.len() / 2),
    );

    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(engine_at(dir.path(), &[])).unwrap();
    let m = manifest_bytes(
        "mdl_resume",
        "Resume",
        "model.gguf",
        &data,
        vec![mirror(server.url("/t/model.gguf"))],
        false,
        true,
        &key,
    );
    let r = engine.import_manifest(&m).unwrap();
    let out = engine.download(&r.manifest_id, None).await.unwrap();
    assert!(!out.artifacts[0].from_cache);
    // Two GETs: the truncated first attempt + the ranged resume.
    assert_eq!(server.hits("/t/model.gguf"), 2);
    assert_eq!(engine.cas().total_blob_bytes().unwrap(), data.len() as u64);
}

#[tokio::test]
async fn cancel_stops_download_keeps_no_blob_and_does_not_ban_source() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    let key = KeyPair::generate();
    let server = TestServer::start().await;
    // Large enough that the progress callback (emits every ~4 MiB) fires partway
    // through, so we can request cancellation mid-stream rather than at the end.
    let data = content(9, 10 * 1024 * 1024);
    server.put("/c/model.gguf", data.clone(), Mode::Ok);

    let dir = tempfile::tempdir().unwrap();
    let engine = Arc::new(Engine::open(engine_at(dir.path(), &[])).unwrap());
    let m = manifest_bytes(
        "mdl_cancel",
        "Cancel",
        "model.gguf",
        &data,
        vec![mirror(server.url("/c/model.gguf"))],
        false,
        true,
        &key,
    );
    let r = engine.import_manifest(&m).unwrap();

    // Cancel on the first mid-stream progress tick (bytes_done > 0 and < total).
    let eng = engine.clone();
    let fired = Arc::new(AtomicBool::new(false));
    let f = fired.clone();
    let progress: noema_core::Progress = Arc::new(move |p: noema_core::DownloadProgress| {
        if p.bytes_done > 0 && p.bytes_done < p.bytes_total && !f.swap(true, Ordering::SeqCst) {
            eng.request_pause();
        }
    });

    let res = engine.download(&r.manifest_id, Some(progress)).await;
    assert!(
        matches!(res, Err(noema_core::Error::Cancelled)),
        "pause must surface as Error::Cancelled, got {res:?}"
    );
    assert!(
        fired.load(Ordering::SeqCst),
        "the pause hook should have fired"
    );
    // Paused before verification — nothing is committed to the CAS.
    assert_eq!(engine.cas().total_blob_bytes().unwrap(), 0);

    // The source was NOT banned for a user pause: a fresh download succeeds and
    // completes (resuming from the kept partial), proving pause ≠ failure.
    let out = engine.download(&r.manifest_id, None).await.unwrap();
    assert!(!out.artifacts[0].from_cache);
    assert_eq!(engine.cas().total_blob_bytes().unwrap(), data.len() as u64);
}

/// Regression: every progress event must carry the in-flight manifest id. The UI
/// ties a live transfer back to its manifest through this field — without it a
/// content/link download (whose id is synthesized at import time and never seen
/// on the wire) can't be resumed from the Transfers page. The engine used to
/// leave this empty.
#[tokio::test]
async fn progress_events_carry_the_manifest_id() {
    use std::sync::{Arc, Mutex};

    let key = KeyPair::generate();
    let server = TestServer::start().await;
    let data = content(11, 6 * 1024 * 1024);
    server.put("/p/model.gguf", data.clone(), Mode::Ok);

    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(engine_at(dir.path(), &[])).unwrap();
    let m = manifest_bytes(
        "mdl_progress_id",
        "ProgressId",
        "model.gguf",
        &data,
        vec![mirror(server.url("/p/model.gguf"))],
        false,
        true,
        &key,
    );
    let r = engine.import_manifest(&m).unwrap();

    let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let s = seen.clone();
    let progress: noema_core::Progress = Arc::new(move |p: noema_core::DownloadProgress| {
        if let Ok(mut v) = s.lock() {
            v.push(p.manifest_id);
        }
    });
    engine
        .download(&r.manifest_id, Some(progress))
        .await
        .unwrap();

    let ids = seen.lock().unwrap();
    assert!(!ids.is_empty(), "expected at least one progress event");
    assert!(
        ids.iter().all(|id| id == &r.manifest_id),
        "every progress event must carry manifest id {:?}, saw {:?}",
        r.manifest_id,
        ids
    );
}

#[tokio::test]
async fn stop_discards_partial_and_download_row() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    let key = KeyPair::generate();
    let server = TestServer::start().await;
    let data = content(11, 10 * 1024 * 1024);
    server.put("/c/model.gguf", data.clone(), Mode::Ok);

    let dir = tempfile::tempdir().unwrap();
    let engine = Arc::new(Engine::open(engine_at(dir.path(), &[])).unwrap());
    let m = manifest_bytes(
        "mdl_stop",
        "Stop",
        "model.gguf",
        &data,
        vec![mirror(server.url("/c/model.gguf"))],
        false,
        true,
        &key,
    );
    let r = engine.import_manifest(&m).unwrap();
    let eng = engine.clone();
    let fired = Arc::new(AtomicBool::new(false));
    let f = fired.clone();
    let progress: noema_core::Progress = Arc::new(move |p: noema_core::DownloadProgress| {
        if p.bytes_done > 0 && p.bytes_done < p.bytes_total && !f.swap(true, Ordering::SeqCst) {
            eng.request_stop();
        }
    });

    let res = engine.download(&r.manifest_id, Some(progress)).await;
    assert!(
        matches!(res, Err(noema_core::Error::Stopped)),
        "stop must surface as Error::Stopped, got {res:?}"
    );
    assert!(
        fired.load(Ordering::SeqCst),
        "the stop hook should have fired"
    );
    // Nothing committed, no partial left behind, and no row to resume from.
    assert_eq!(engine.cas().total_blob_bytes().unwrap(), 0);
    let leftover: Vec<_> = std::fs::read_dir(engine.cas().tmp_dir())
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().map(|x| x == "part").unwrap_or(false))
        .collect();
    assert!(
        leftover.is_empty(),
        "stop must delete the partial temp, found {leftover:?}"
    );
    assert!(
        engine.db().list_downloads().unwrap().is_empty(),
        "stop must drop the download row so nothing resumes"
    );
    let out = engine.download(&r.manifest_id, None).await.unwrap();
    assert!(!out.artifacts[0].from_cache);
    assert_eq!(engine.cas().total_blob_bytes().unwrap(), data.len() as u64);
}

#[tokio::test]
async fn reconcile_keeps_resumable_pause_but_reaps_orphaned_download() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    let key = KeyPair::generate();
    let server = TestServer::start().await;
    let data = content(12, 10 * 1024 * 1024);
    server.put("/c/model.gguf", data.clone(), Mode::Ok);

    let dir = tempfile::tempdir().unwrap();
    let engine = Arc::new(Engine::open(engine_at(dir.path(), &[])).unwrap());
    let m = manifest_bytes(
        "mdl_reap",
        "Reap",
        "model.gguf",
        &data,
        vec![mirror(server.url("/c/model.gguf"))],
        false,
        true,
        &key,
    );
    let r = engine.import_manifest(&m).unwrap();

    // Pause mid-stream: keeps a `paused` row + a `.part` temp.
    let eng = engine.clone();
    let fired = Arc::new(AtomicBool::new(false));
    let f = fired.clone();
    let progress: noema_core::Progress = Arc::new(move |p: noema_core::DownloadProgress| {
        if p.bytes_done > 0 && p.bytes_done < p.bytes_total && !f.swap(true, Ordering::SeqCst) {
            eng.request_pause();
        }
    });
    let res = engine.download(&r.manifest_id, Some(progress)).await;
    assert!(matches!(res, Err(noema_core::Error::Cancelled)));

    let row = engine.db().list_downloads().unwrap();
    assert_eq!(row.len(), 1, "pause should leave exactly one download row");
    let download_id = row[0].download_id.clone();
    assert_eq!(row[0].state, "paused");
    assert!(
        engine.cas().download_temp_exists(&download_id),
        "pause should keep the .part temp"
    );

    // reconcile() must NOT touch a still-resumable paused download (temp +
    // manifest both present).
    let rep = engine.reconcile().unwrap();
    assert_eq!(rep.removed_downloads, 0, "resumable pause must be kept");
    assert_eq!(engine.db().list_downloads().unwrap().len(), 1);
    assert!(engine.cas().download_temp_exists(&download_id));

    // Now make it unresumable by removing the manifest the resume needs. reconcile()
    // should reap the dead row and delete its leftover `.part`.
    engine.db().delete_manifest(&r.manifest_id).unwrap();
    let rep = engine.reconcile().unwrap();
    assert_eq!(rep.removed_downloads, 1, "orphaned download must be reaped");
    assert!(
        engine.db().list_downloads().unwrap().is_empty(),
        "orphan row should be gone"
    );
    assert!(
        !engine.cas().download_temp_exists(&download_id),
        "orphan .part temp should be deleted"
    );
}

#[tokio::test]
async fn local_file_import_avoids_download() {
    let key = KeyPair::generate();
    let server = TestServer::start().await;
    let data = content(4, 50_000);
    // A route that 404s — proves we never hit the network.
    server.put("/missing/model.gguf", data.clone(), Mode::NotFound);

    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(engine_at(dir.path(), &[])).unwrap();
    let m = manifest_bytes(
        "mdl_local",
        "Local",
        "model.gguf",
        &data,
        vec![mirror(server.url("/missing/model.gguf"))],
        false,
        true,
        &key,
    );
    let r = engine.import_manifest(&m).unwrap();
    let src = dir.path().join("ondisk.gguf");
    std::fs::write(&src, &data).unwrap();
    engine
        .import_artifact_file(&r.manifest_id, "model.gguf", &src)
        .unwrap();

    let out = engine.download(&r.manifest_id, None).await.unwrap();
    assert!(out.artifacts[0].from_cache, "should be a cache hit");
    assert_eq!(server.hits("/missing/model.gguf"), 0);
}

#[tokio::test]
async fn local_import_with_wrong_hash_is_rejected() {
    let key = KeyPair::generate();
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(engine_at(dir.path(), &[])).unwrap();
    let data = content(7, 20_000);
    let m = manifest_bytes(
        "mdl_badimport",
        "BadImport",
        "model.gguf",
        &data,
        vec![],
        false,
        true,
        &key,
    );
    let r = engine.import_manifest(&m).unwrap();
    let wrong = dir.path().join("wrong.gguf");
    std::fs::write(&wrong, content(8, 20_000)).unwrap();
    let res = engine.import_artifact_file(&r.manifest_id, "model.gguf", &wrong);
    assert!(res.is_err(), "mismatched local import must be rejected");
    assert_eq!(engine.list_cache().unwrap().len(), 0);
}

#[tokio::test]
async fn ipfs_gateway_fetch_matches_manifest() {
    let key = KeyPair::generate();
    let server = TestServer::start().await;
    let data = content(5, 120_000);
    let cid = "bafyTESTcid";
    server.put(&format!("/ipfs/{cid}"), data.clone(), Mode::Ok);

    let dir = tempfile::tempdir().unwrap();
    let mut cfg = engine_at(dir.path(), &[]);
    cfg.transport.ipfs_gateways = vec![server.base()];
    let engine = Engine::open(cfg).unwrap();

    let m = manifest_bytes(
        "mdl_ipfs",
        "Ipfs",
        "model.gguf",
        &data,
        vec![Source::Ipfs {
            cid: cid.into(),
            retrieval: vec!["gateway".into()],
            auth: AuthPolicy::None,
        }],
        false,
        true,
        &key,
    );
    let r = engine.import_manifest(&m).unwrap();
    let out = engine.download(&r.manifest_id, None).await.unwrap();
    assert!(!out.artifacts[0].from_cache);
    assert_eq!(
        out.artifacts[0].source_id.as_deref(),
        Some(&*format!("ipfs:{cid}"))
    );
}

#[tokio::test]
async fn gated_model_requires_trusted_signature() {
    let key = KeyPair::generate();
    let server = TestServer::start().await;
    let data = content(6, 80_000);
    server.put("/g/model.gguf", data.clone(), Mode::Ok);

    let m = manifest_bytes(
        "mdl_gated",
        "Gated",
        "model.gguf",
        &data,
        vec![mirror(server.url("/g/model.gguf"))],
        true,
        true,
        &key,
    );

    // Untrusted: policy denies the download.
    let dir1 = tempfile::tempdir().unwrap();
    let engine1 = Engine::open(engine_at(dir1.path(), &[])).unwrap();
    let r1 = engine1.import_manifest(&m).unwrap();
    assert!(!r1.policy.allowed);
    assert!(engine1.download(&r1.manifest_id, None).await.is_err());

    // Trusted: allowed and succeeds.
    let dir2 = tempfile::tempdir().unwrap();
    let engine2 = Engine::open(engine_at(dir2.path(), &[key.key_id()])).unwrap();
    let r2 = engine2.import_manifest(&m).unwrap();
    assert!(r2.policy.allowed, "{}", r2.policy.reason);
    let out = engine2.download(&r2.manifest_id, None).await.unwrap();
    assert!(!out.artifacts[0].from_cache);
}

#[tokio::test]
async fn unsigned_gated_manifest_is_denied() {
    let key = KeyPair::generate();
    let data = content(10, 10_000);
    let m = manifest_bytes(
        "mdl_unsigned_gated",
        "UnsignedGated",
        "model.gguf",
        &data,
        vec![],
        true,  // gated
        false, // not signed
        &key,
    );
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(engine_at(dir.path(), &[])).unwrap();
    let r = engine.import_manifest(&m).unwrap();
    assert!(!r.policy.allowed);
    assert!(engine.download(&r.manifest_id, None).await.is_err());
}

#[tokio::test]
async fn tampered_signature_is_not_trusted() {
    let key = KeyPair::generate();
    let data = content(11, 10_000);
    // Sign, then tamper a field so the embedded signature no longer matches.
    let bytes = manifest_bytes(
        "mdl_tamper",
        "Tamper",
        "model.gguf",
        &data,
        vec![],
        true,
        true,
        &key,
    );
    let mut m: Manifest = serde_json::from_slice(&bytes).unwrap();
    m.artifacts[0].size_bytes += 1;
    let tampered = m.to_json_pretty().unwrap().into_bytes();

    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(engine_at(dir.path(), &[key.key_id()])).unwrap();
    let r = engine.import_manifest(&tampered).unwrap();
    assert!(
        !r.report.is_signed(),
        "tampered signature must not validate"
    );
    assert!(!r.policy.allowed, "gated + invalid signature => denied");
}

#[tokio::test]
async fn path_traversal_artifact_is_rejected_on_import() {
    let key = KeyPair::generate();
    let data = content(12, 1000);
    let bytes = manifest_bytes(
        "mdl_traversal",
        "Traversal",
        "../../etc/evil",
        &data,
        vec![],
        false,
        true,
        &key,
    );
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(engine_at(dir.path(), &[])).unwrap();
    assert!(
        engine.import_manifest(&bytes).is_err(),
        "must reject traversal path"
    );
}

#[tokio::test]
async fn reconcile_drops_models_deleted_outside_the_app() {
    let key = KeyPair::generate();
    let server = TestServer::start().await;
    let data = content(5, 120_000);
    server.put("/m/model.gguf", data.clone(), Mode::Ok);

    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(engine_at(dir.path(), &[])).unwrap();
    let m = manifest_bytes(
        "mdl_rec",
        "Reconcile",
        "model.gguf",
        &data,
        vec![mirror(server.url("/m/model.gguf"))],
        false,
        true,
        &key,
    );
    let r = engine.import_manifest(&m).unwrap();
    engine.download(&r.manifest_id, None).await.unwrap();
    assert_eq!(engine.installed_models().unwrap().len(), 1);

    // Delete the blob behind the app's back (blobs are read-only).
    let b3 = hash_bytes(&data).blake3;
    let blob = engine.cas().blob_path(&b3).unwrap();
    let mut perms = std::fs::metadata(&blob).unwrap().permissions();
    #[allow(clippy::permissions_set_readonly_false)]
    perms.set_readonly(false);
    std::fs::set_permissions(&blob, perms).unwrap();
    std::fs::remove_file(&blob).unwrap();

    let report = engine.reconcile().unwrap();
    assert_eq!(report.removed_blobs, 1);
    assert_eq!(report.removed_blake3s, vec![b3.clone()]);
    assert_eq!(
        engine.installed_models().unwrap().len(),
        0,
        "model deleted on disk should vanish after reconcile"
    );
}

#[tokio::test]
async fn search_aggregates_one_file_across_many_sources() {
    let key = KeyPair::generate();
    let data = content(20, 50_000); // identical bytes in both manifests
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(engine_at(dir.path(), &[])).unwrap();

    // Two manifests for the SAME file, each with a different source.
    let m1 = manifest_bytes(
        "mdl_src_a",
        "Llama Q4",
        "model.gguf",
        &data,
        vec![mirror("https://a.example/model.gguf".into())],
        false,
        true,
        &key,
    );
    let m2 = manifest_bytes(
        "mdl_src_b",
        "Llama Q4 (mirror)",
        "model.gguf",
        &data,
        vec![Source::Ipfs {
            cid: "bafyLLAMA".into(),
            retrieval: vec![],
            auth: AuthPolicy::None,
        }],
        false,
        true,
        &key,
    );
    engine.import_manifest(&m1).unwrap();
    engine.import_manifest(&m2).unwrap();

    let results = engine.search("").unwrap();
    assert_eq!(results.len(), 1, "same file => one deduplicated result");
    let r = &results[0];
    assert_eq!(r.blake3, hash_bytes(&data).blake3);
    assert_eq!(r.sources.len(), 2, "both sources surfaced for the one file");
    assert_eq!(r.manifest_ids.len(), 2);

    // Query filtering by model name.
    assert_eq!(engine.search("llama").unwrap().len(), 1);
    assert_eq!(engine.search("nonexistent-xyz").unwrap().len(), 0);
}

#[tokio::test]
async fn malicious_manifest_id_is_rejected_on_import() {
    let key = KeyPair::generate();
    let data = content(14, 1000);
    // A manifest_id that would escape the manifests/ directory.
    let bytes = manifest_bytes(
        "../../etc/evil",
        "Evil",
        "model.gguf",
        &data,
        vec![],
        false,
        true,
        &key,
    );
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(engine_at(dir.path(), &[])).unwrap();
    assert!(
        engine.import_manifest(&bytes).is_err(),
        "manifest_id path traversal must be rejected"
    );
}

#[tokio::test]
async fn install_then_evict_unreferenced() {
    let key = KeyPair::generate();
    let server = TestServer::start().await;
    let data = content(13, 60_000);
    server.put("/m/model.gguf", data.clone(), Mode::Ok);

    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(engine_at(dir.path(), &[])).unwrap();
    let m = manifest_bytes(
        "mdl_install",
        "Install",
        "model.gguf",
        &data,
        vec![mirror(server.url("/m/model.gguf"))],
        false,
        true,
        &key,
    );
    let r = engine.import_manifest(&m).unwrap();
    engine.download(&r.manifest_id, None).await.unwrap();

    let target = dir.path().join("install-out");
    let views = engine.materialize_install(&r.manifest_id, &target).unwrap();
    assert_eq!(views.len(), 1);
    assert_eq!(std::fs::read(&views[0].dest).unwrap(), data);

    // Installed blob is referenced => unreferenced eviction removes nothing.
    let report = engine.evict_cache(EvictPolicy::Unreferenced).unwrap();
    assert_eq!(report.removed.len(), 0);
    let report = engine.evict_cache(EvictPolicy::All).unwrap();
    assert_eq!(report.removed.len(), 1);
    assert_eq!(engine.cas().total_blob_bytes().unwrap(), 0);
    assert!(!views[0].dest.exists());
    assert_eq!(engine.list_installs().unwrap().len(), 0);
    assert_eq!(engine.installed_models().unwrap().len(), 0);
}

#[tokio::test]
async fn multi_connection_segmented_download_assembles_byte_identical() {
    let key = KeyPair::generate();
    let server = TestServer::start().await;
    // Above the 2 × 32 MiB segmentation threshold (with an uneven remainder) so
    // the engine splits this into several parallel HTTP range requests.
    let data = content(7, 64 * 1024 * 1024 + 7_777);
    server.put("/seg/model.gguf", data.clone(), Mode::Ok);

    let dir = tempfile::tempdir().unwrap();
    let mut cfg = engine_at(dir.path(), &[]);
    cfg.max_download_connections = 4;
    let engine = Engine::open(cfg).unwrap();

    let m = manifest_bytes(
        "mdl_seg",
        "Seg Model",
        "model.gguf",
        &data,
        vec![mirror(server.url("/seg/model.gguf"))],
        false,
        true,
        &key,
    );
    let r = engine.import_manifest(&m).unwrap();
    let out = engine.download(&r.manifest_id, None).await.unwrap();
    assert!(!out.artifacts[0].from_cache);

    // The server saw multiple range requests — proof it was fetched over several
    // connections concurrently rather than a single stream.
    assert!(
        server.hits("/seg/model.gguf") >= 2,
        "expected multiple segment requests, got {}",
        server.hits("/seg/model.gguf")
    );

    // The committed blob is byte-identical to the source (the full-file hash gate
    // catches any mis-stitched segment), and the right size.
    let expected = hash_bytes(&data);
    assert_eq!(out.artifacts[0].blake3, expected.blake3);
    assert_eq!(engine.cas().total_blob_bytes().unwrap(), data.len() as u64);

    // …and it materializes back to the exact bytes.
    let target = dir.path().join("seg-out");
    let views = engine.materialize_install(&r.manifest_id, &target).unwrap();
    assert_eq!(std::fs::read(&views[0].dest).unwrap(), data);
}

#[tokio::test]
async fn single_connection_setting_does_not_segment() {
    let key = KeyPair::generate();
    let server = TestServer::start().await;
    let data = content(9, 64 * 1024 * 1024 + 1_234);
    server.put("/one/model.gguf", data.clone(), Mode::Ok);

    let dir = tempfile::tempdir().unwrap();
    let mut cfg = engine_at(dir.path(), &[]);
    cfg.max_download_connections = 1; // segmentation disabled
    let engine = Engine::open(cfg).unwrap();

    let m = manifest_bytes(
        "mdl_one",
        "One Model",
        "model.gguf",
        &data,
        vec![mirror(server.url("/one/model.gguf"))],
        false,
        true,
        &key,
    );
    let r = engine.import_manifest(&m).unwrap();
    let out = engine.download(&r.manifest_id, None).await.unwrap();
    // A single connection means a single GET (no range splitting), and still the
    // correct bytes.
    assert_eq!(server.hits("/one/model.gguf"), 1);
    assert_eq!(out.artifacts[0].blake3, hash_bytes(&data).blake3);
}
