//! Guards the gated-leak fix: a token-gated manifest must never be reseeded onto the public BitTorrent swarm.
#![cfg(feature = "bittorrent")]

use noema_core::db::Db;
use noema_core::hash::hash_bytes;
use noema_core::manifest::{
    Access, Artifact, AuthPolicy, License, Manifest, Model, Publisher, RedistributionClass, Source,
    SourceClass, SCHEMA_VERSION,
};
use noema_core::platform::PlatformProfile;
use noema_core::policy::{PolicyConfig, PolicyEngine};
use noema_core::sign::{verify_manifest, KeyPair};

/// Build an openly-licensed manifest whose only source is a token-walled HF repo.
/// `access.gated` stays false on purpose: gating comes solely from the bearer
/// token, so this proves the token branch of `is_gated()` rather than the flag.
fn token_walled_manifest(data: &[u8]) -> Manifest {
    Manifest {
        schema_version: SCHEMA_VERSION.into(),
        manifest_id: "mdl_token_gated".into(),
        publisher: Publisher {
            id: "test-publisher".into(),
            display_name: None,
            public_keys: vec![],
        },
        model: Model {
            name: "gated-model".into(),
            family: None,
            architecture: None,
            revision: None,
            format: None,
            quantization: None,
        },
        license: License {
            spdx: "apache-2.0".into(),
            license_url: None,
            // Openly licensed: the ONLY thing withholding redistribution is the token.
            redistribution: RedistributionClass::PublicP2pAllowed,
        },
        access: Access {
            gated: false,
            require_signed_manifest: false,
            allowed_source_classes: vec![],
        },
        artifacts: vec![Artifact {
            path: "tok.gguf".into(),
            role: "weights".into(),
            size_bytes: data.len() as u64,
            hashes: hash_bytes(data),
            chunking: None,
            format: None,
            sources: vec![Source::Huggingface {
                repo_id: "meta-llama/x".into(),
                revision: "main".into(),
                path: "tok.gguf".into(),
                auth: AuthPolicy::Token,
            }],
        }],
        provenance: None,
        signatures: vec![],
    }
}

#[test]
fn token_gated_manifest_is_not_bittorrent_seeded() {
    let data = b"token-gated weights";
    let mut m = token_walled_manifest(data);

    // The token alone makes it gated, even with access.gated == false.
    assert!(m.is_gated(), "a token-walled HF source must read as gated");

    let engine = PolicyEngine::new(PolicyConfig::default());
    let profile = PlatformProfile::desktop();

    // The cardinal guard: redistribution onto the public BitTorrent swarm is denied.
    let (bt_ok, reason) = engine.redistribution_allowed(&m, SourceClass::BittorrentV2, &profile);
    assert!(
        !bt_ok,
        "token-gated content must NOT be reseeded over BitTorrent (got: {reason})"
    );
    assert!(
        reason.contains("gated"),
        "denial should cite gating: {reason}"
    );

    // Same denial for the other public-distribution channel (Iroh).
    let (iroh_ok, _) = engine.redistribution_allowed(&m, SourceClass::Iroh, &profile);
    assert!(!iroh_ok, "Iroh reseeding of gated content must be denied");

    // Sanity: dropping the token (purely public) flips the BitTorrent guard open,
    // so the denial above is the gating, not a blanket "no".
    m.artifacts[0].sources = vec![Source::Huggingface {
        repo_id: "meta-llama/x".into(),
        revision: "main".into(),
        path: "tok.gguf".into(),
        auth: AuthPolicy::None,
    }];
    assert!(!m.is_gated(), "without the token the manifest is public");
    let (open_ok, reason) = engine.redistribution_allowed(&m, SourceClass::BittorrentV2, &profile);
    assert!(open_ok, "public content should be seedable: {reason}");
}

#[test]
fn token_gated_blob_is_not_shareable_until_confirmed() {
    let data = b"token-gated weights";
    let mut m = token_walled_manifest(data);
    let blake3 = m.artifacts[0].hashes.blake3.clone();
    let sha256 = m.artifacts[0].hashes.sha256.clone();

    let kp = KeyPair::generate();
    kp.sign_manifest(&mut m).unwrap();
    let report = verify_manifest(&m).unwrap();

    let db = Db::open_in_memory().unwrap();
    db.insert_manifest(&m, &report).unwrap();

    // The engine's seed gate (`is_blob_shareable`) refuses the gated blob, even
    // with the global "also share gated" opt-in set — confirm-before-share wins.
    assert!(
        !db.is_blob_shareable(&blake3).unwrap(),
        "token-gated blob must not be shareable by default"
    );
    db.set_share_gated(true);
    assert!(
        !db.is_blob_shareable(&blake3).unwrap(),
        "the global gated opt-in alone must not unlock a token-gated blob"
    );
    db.set_share_gated(false);

    // Only an explicit per-blob confirmation opts it in.
    db.confirm_gated_share(&blake3, &sha256).unwrap();
    assert!(
        db.is_blob_shareable(&blake3).unwrap(),
        "an explicitly confirmed gated blob becomes shareable"
    );
}
