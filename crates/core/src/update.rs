//! Signed release manifest for in-app auto-update. Clients verify the ed25519 signature
//! against baked-in [`UPDATE_RELEASE_PUBKEYS`] plus each asset's pinned SHA-256, so an
//! untrusted VPS/MITM/tampered asset can only withhold an update, never force bad bytes.
//! Unconditionally compiled (no feature gate) so the registry shares the same logic.

use base64::Engine as _;
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};

/// Schema version of the release-manifest format itself. Bump only on a
/// breaking shape change; clients tolerate unknown trailing fields via serde.
pub const RELEASE_MANIFEST_SCHEMA: u32 = 1;

/// Ed25519 public keys (64-char lowercase hex) trusted to sign release manifests.
///
/// Matching private keys are held offline (never in CI), so a CI compromise can't forge
/// an update. Ship a set (current + next) for overlap-window rotation; an empty set
/// fails closed and no update is ever offered.
pub const UPDATE_RELEASE_PUBKEYS: &[&str] = &[
    // Atlas release-signing key #1; secret held offline (gen 2026-06-24). Add a second
    // key here ahead of a rotation so both overlap before the first is retired.
    "7a8c8a2a03f606b224f2205595bc50da8495a6b9da9c46f77f3fbd3e51740fc7",
];

/// Clock-skew slack past [`ReleaseManifest::expires_at`] within which a manifest is
/// still tolerated. Expiry is anti-freeze, not a security boundary (the signature and
/// per-asset hash are the real controls).
pub const EXPIRY_SKEW_MS: i64 = 24 * 60 * 60 * 1000;

/// A detached signature over a manifest's [`ReleaseManifest::canonical_bytes`].
/// `key_id` is `ed25519:<hex32>` and carries the verifying key inline; the client
/// only honours it when that key is in its baked-in trust set.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateSignature {
    pub key_id: String,
    #[serde(default = "ed25519_algo")]
    pub algorithm: String,
    /// base64 of the 64-byte ed25519 signature.
    pub value: String,
}

fn ed25519_algo() -> String {
    "ed25519".to_string()
}

/// One downloadable artifact for a specific OS/arch/flavor. `sha256` pins the bytes;
/// the client refuses to apply anything whose hash doesn't match.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlatformAsset {
    /// `macos` | `windows` | `linux`.
    pub os: String,
    /// `x86_64` | `aarch64` | `universal`.
    pub arch: String,
    /// Install flavour, so the client can match how it was installed:
    /// `installer` (NSIS), `portable`, `appimage`, `deb`, `app`, `app_tar_gz`, …
    /// Empty means "the default/only artifact for this os+arch".
    #[serde(default)]
    pub flavor: String,
    /// Bare file name on the GitHub release (kept so the client never has to
    /// reconstruct version-stamped names like `Noema Atlas Studio_0.1.0_amd64.deb`).
    pub name: String,
    /// Absolute download URL (a GitHub release asset URL).
    pub url: String,
    /// Lowercase hex SHA-256 of the asset bytes.
    pub sha256: String,
    #[serde(default)]
    pub size: u64,
    /// For the Tauri updater only: the *contents* of the bundle's `.sig` file
    /// (minisign). Empty for Atlas assets, which are verified by `sha256` + the
    /// manifest's own ed25519 signature instead.
    #[serde(default)]
    pub signature: String,
}

/// Release info for one app (`atlas` or `studio`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppRelease {
    /// `atlas` | `studio`.
    pub app: String,
    /// Latest released version for this app (semver).
    pub version: String,
    /// Clients strictly older than this should treat the update as forced (a
    /// security floor). Empty means no floor.
    #[serde(default)]
    pub min_supported: String,
    /// Human-facing release-notes URL (the GitHub release tag page).
    #[serde(default)]
    pub notes_url: String,
    pub assets: Vec<PlatformAsset>,
}

/// The full signed manifest the VPS serves and clients verify.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReleaseManifest {
    pub schema: u32,
    /// Release channel this manifest describes, e.g. `stable`.
    pub channel: String,
    /// When the manifest was produced (unix millis), informational.
    #[serde(default)]
    pub generated_at: i64,
    /// After this instant (unix millis) the manifest is stale; clients fall back to
    /// a manual check. Keep it short (sized to release cadence) for anti-freeze.
    pub expires_at: i64,
    pub apps: Vec<AppRelease>,
    /// Signatures over [`Self::canonical_bytes`]. Excluded from the signed bytes.
    #[serde(default)]
    pub signatures: Vec<UpdateSignature>,
}

impl ReleaseManifest {
    /// Parse from JSON bytes. Unknown fields are ignored (forward compatibility).
    pub fn from_json(bytes: &[u8]) -> Result<Self, serde_json::Error> {
        serde_json::from_slice(bytes)
    }

    /// Pretty JSON (what the signer writes and the registry serves).
    pub fn to_json_pretty(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self)
    }

    /// Deterministic bytes signed/verified, with `signatures` removed. Mirrors
    /// [`crate::manifest::Manifest::canonical_bytes`]: serde_json's value map sorts
    /// keys recursively, so the encoding is stable regardless of struct field order.
    pub fn canonical_bytes(&self) -> Result<Vec<u8>, serde_json::Error> {
        let mut value = serde_json::to_value(self)?;
        if let serde_json::Value::Object(map) = &mut value {
            map.remove("signatures");
        }
        serde_json::to_vec(&value)
    }

    /// Sign in place with `secret_hex` (a 32-byte ed25519 seed as hex), appending a
    /// signature whose `key_id` carries the public key. Re-signing with the same key
    /// is idempotent.
    pub fn sign(&mut self, secret_hex: &str) -> Result<(), crate::error::Error> {
        let kp = crate::sign::KeyPair::from_secret_hex(secret_hex)?;
        let key_id = kp.key_id();
        self.signatures.retain(|s| s.key_id != key_id);
        let bytes = self
            .canonical_bytes()
            .map_err(|e| crate::error::Error::other(format!("canonicalize: {e}")))?;
        let sig = kp.sign_bytes(&bytes);
        self.signatures.push(UpdateSignature {
            key_id,
            algorithm: "ed25519".to_string(),
            value: base64::engine::general_purpose::STANDARD.encode(sig),
        });
        Ok(())
    }

    /// True iff some signature is from a `trusted` key and verifies over the canonical
    /// bytes. Never panics (malformed input is a non-match). Trusted entries may be bare
    /// 64-hex or `ed25519:<hex>`. Clients pass [`UPDATE_RELEASE_PUBKEYS`]; the registry
    /// passes its own trust set.
    #[must_use]
    pub fn is_signed_by_trusted(&self, trusted: &[&str]) -> bool {
        let Ok(bytes) = self.canonical_bytes() else {
            return false;
        };
        self.signatures
            .iter()
            .any(|sig| signature_is_trusted(&bytes, sig, trusted))
    }

    /// True once `now_ms` is past `expires_at` (plus a small skew). A stale manifest
    /// is still cryptographically valid — callers use this to *also* try a fallback
    /// source rather than to reject outright.
    #[must_use]
    pub fn is_expired(&self, now_ms: i64) -> bool {
        now_ms.saturating_sub(self.expires_at) > EXPIRY_SKEW_MS
    }

    /// The release entry for `app` (`atlas`/`studio`), if present.
    #[must_use]
    pub fn app(&self, app: &str) -> Option<&AppRelease> {
        self.apps.iter().find(|a| a.app.eq_ignore_ascii_case(app))
    }
}

impl AppRelease {
    /// Pick the asset matching `os`/`arch`, optionally constrained to `flavor`.
    ///
    /// `arm64` aliases `aarch64`; a `universal` asset matches any arch on its OS. `None`
    /// means "no update for this platform" — never a wrong-arch binary.
    #[must_use]
    pub fn select_asset(
        &self,
        os: &str,
        arch: &str,
        flavor: Option<&str>,
    ) -> Option<&PlatformAsset> {
        let os = normalize_os(os);
        let arch = normalize_arch(arch);
        self.assets.iter().find(|a| {
            normalize_os(&a.os) == os
                && (normalize_arch(&a.arch) == arch || a.arch.eq_ignore_ascii_case("universal"))
                && flavor.is_none_or(|f| a.flavor.eq_ignore_ascii_case(f))
        })
    }

    /// Is `current` strictly older than this release's `version`? Unparsable or
    /// non-semver inputs return `false` (never offer an update we can't reason about).
    #[must_use]
    pub fn is_newer_than(&self, current: &str) -> bool {
        version_gt(&self.version, current)
    }

    /// Is `current` below the security floor (`min_supported`)? Empty floor or
    /// unparsable inputs return `false`.
    #[must_use]
    pub fn is_forced_for(&self, current: &str) -> bool {
        if self.min_supported.trim().is_empty() {
            return false;
        }
        version_gt(&self.min_supported, current)
    }
}

/// `a > b` under semver, with unparsable inputs treated as "not greater". Note plain
/// semver ordering ranks a prerelease below its release (`1.0.0-rc < 1.0.0`) and
/// ignores build metadata — both the behaviour we want for an update gate.
#[must_use]
pub fn version_gt(a: &str, b: &str) -> bool {
    match (
        semver::Version::parse(a.trim()),
        semver::Version::parse(b.trim()),
    ) {
        (Ok(a), Ok(b)) => a > b,
        _ => false,
    }
}

/// Canonicalize an OS token to one of `macos` / `windows` / `linux`, accepting the
/// common aliases (`darwin`, `osx`, `win`). Anything else is returned lowercased.
pub fn normalize_os(os: &str) -> String {
    match os.trim().to_ascii_lowercase().as_str() {
        "macos" | "darwin" | "osx" | "mac" => "macos".to_string(),
        "windows" | "win" | "win32" | "win64" => "windows".to_string(),
        "linux" => "linux".to_string(),
        other => other.to_string(),
    }
}

/// Canonicalize an arch token to `x86_64` / `aarch64` / `universal`, accepting the
/// common aliases (`amd64`, `x64`, `arm64`).
pub fn normalize_arch(arch: &str) -> String {
    match arch.trim().to_ascii_lowercase().as_str() {
        "x86_64" | "amd64" | "x64" => "x86_64".to_string(),
        "aarch64" | "arm64" => "aarch64".to_string(),
        "universal" => "universal".to_string(),
        other => other.to_string(),
    }
}

/// The host's `(os, arch)` as canonical tokens, from `std::env::consts`. Clients
/// send these to the VPS and select their own asset with them.
pub fn host_os_arch() -> (String, String) {
    (
        normalize_os(std::env::consts::OS),
        normalize_arch(std::env::consts::ARCH),
    )
}

fn signature_is_trusted(bytes: &[u8], sig: &UpdateSignature, trusted: &[&str]) -> bool {
    if !sig.algorithm.eq_ignore_ascii_case("ed25519") {
        return false;
    }
    // The verifying key is carried in key_id (`ed25519:<hex>`); trust it only if that
    // exact key is in the baked-in set. Normalize the `ed25519:` prefix on both sides so
    // a trusted entry written as a full key_id still matches (must not disable updates).
    let norm = |s: &str| s.trim().trim_start_matches("ed25519:").to_ascii_lowercase();
    let pub_hex = norm(&sig.key_id);
    if !trusted.iter().any(|t| norm(t) == pub_hex) {
        return false;
    }
    let Ok(pk_bytes) = hex::decode(&pub_hex) else {
        return false;
    };
    let Ok(pk_arr) = <[u8; 32]>::try_from(pk_bytes.as_slice()) else {
        return false;
    };
    let Ok(vk) = VerifyingKey::from_bytes(&pk_arr) else {
        return false;
    };
    let Ok(sig_bytes) = base64::engine::general_purpose::STANDARD.decode(sig.value.as_bytes())
    else {
        return false;
    };
    let Ok(sig_arr) = <[u8; 64]>::try_from(sig_bytes.as_slice()) else {
        return false;
    };
    vk.verify(bytes, &Signature::from_bytes(&sig_arr)).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sign::KeyPair;

    fn sample() -> ReleaseManifest {
        ReleaseManifest {
            schema: RELEASE_MANIFEST_SCHEMA,
            channel: "stable".into(),
            generated_at: 1_700_000_000_000,
            expires_at: 1_700_000_000_000 + 7 * 24 * 60 * 60 * 1000,
            apps: vec![AppRelease {
                app: "atlas".into(),
                version: "0.2.0".into(),
                min_supported: "0.1.0".into(),
                notes_url: "https://example.com/v0.2.0".into(),
                assets: vec![
                    PlatformAsset {
                        os: "macos".into(),
                        arch: "universal".into(),
                        name: "Noema-Atlas-macos-universal.zip".into(),
                        url: "https://example.com/a.zip".into(),
                        sha256: "ab".repeat(32),
                        size: 100,
                        ..Default::default()
                    },
                    PlatformAsset {
                        os: "windows".into(),
                        arch: "x86_64".into(),
                        flavor: "installer".into(),
                        name: "Noema-Atlas-Setup.exe".into(),
                        url: "https://example.com/s.exe".into(),
                        sha256: "cd".repeat(32),
                        size: 200,
                        ..Default::default()
                    },
                    PlatformAsset {
                        os: "linux".into(),
                        arch: "x86_64".into(),
                        flavor: "appimage".into(),
                        name: "Noema-Atlas-x86_64.AppImage".into(),
                        url: "https://example.com/a.AppImage".into(),
                        sha256: "ef".repeat(32),
                        size: 300,
                        ..Default::default()
                    },
                ],
            }],
            signatures: vec![],
        }
    }

    #[test]
    fn sign_then_verify_roundtrip() {
        let kp = KeyPair::generate();
        let mut m = sample();
        m.sign(&kp.secret_hex()).unwrap();
        let trusted = [kp.public_hex()];
        let trusted_refs: Vec<&str> = trusted.iter().map(|s| s.as_str()).collect();
        assert!(m.is_signed_by_trusted(&trusted_refs));
    }

    #[test]
    fn untrusted_key_is_rejected_even_if_math_checks() {
        let signer = KeyPair::generate();
        let stranger = KeyPair::generate();
        let mut m = sample();
        m.sign(&signer.secret_hex()).unwrap();
        // The signature is mathematically valid, but the signer isn't in the set.
        assert!(!m.is_signed_by_trusted(&[stranger.public_hex().as_str()]));
        // And empty trust set fails closed.
        assert!(!m.is_signed_by_trusted(&[]));
    }

    #[test]
    fn tampering_breaks_signature() {
        let kp = KeyPair::generate();
        let mut m = sample();
        m.sign(&kp.secret_hex()).unwrap();
        // Flip the pinned hash after signing.
        m.apps[0].assets[0].sha256 = "00".repeat(32);
        assert!(!m.is_signed_by_trusted(&[kp.public_hex().as_str()]));
    }

    #[test]
    fn signatures_do_not_affect_canonical_bytes() {
        let kp = KeyPair::generate();
        let mut m = sample();
        let before = m.canonical_bytes().unwrap();
        m.sign(&kp.secret_hex()).unwrap();
        let after = m.canonical_bytes().unwrap();
        assert_eq!(before, after);
    }

    #[test]
    fn canonical_bytes_have_sorted_object_keys() {
        // The whole signing scheme assumes serde_json emits object keys in sorted
        // order (no `preserve_order` feature). Guard that invariant: if it ever
        // flips, signatures would be over a non-deterministic encoding and verify
        // would break in the field. `channel` must serialize before `expires_at`.
        let bytes = sample().canonical_bytes().unwrap();
        let s = String::from_utf8(bytes).unwrap();
        let chan = s.find("\"channel\"").unwrap();
        let exp = s.find("\"expires_at\"").unwrap();
        assert!(chan < exp, "object keys are not sorted: {s}");
    }

    #[test]
    fn trusted_set_tolerates_key_id_prefix() {
        // A trusted entry written as the full `ed25519:<hex>` key_id (what
        // `noema update keygen` prints) must still verify, not silently fail closed.
        let kp = KeyPair::generate();
        let mut m = sample();
        m.sign(&kp.secret_hex()).unwrap();
        assert!(m.is_signed_by_trusted(&[kp.key_id().as_str()]));
        assert!(m.is_signed_by_trusted(&[kp.public_hex().as_str()]));
    }

    #[test]
    fn asset_selection_and_arch_aliases() {
        let m = sample();
        let atlas = m.app("atlas").unwrap();
        // macOS universal matches both arches.
        assert_eq!(
            atlas.select_asset("macos", "aarch64", None).unwrap().name,
            "Noema-Atlas-macos-universal.zip"
        );
        assert_eq!(
            atlas.select_asset("darwin", "x86_64", None).unwrap().name,
            "Noema-Atlas-macos-universal.zip"
        );
        // arm64 alias and amd64 alias.
        assert_eq!(
            atlas
                .select_asset("windows", "amd64", Some("installer"))
                .unwrap()
                .name,
            "Noema-Atlas-Setup.exe"
        );
        // No arm64 Linux asset exists -> None (must degrade to "no update").
        assert!(atlas.select_asset("linux", "arm64", None).is_none());
        // Flavor filter excludes the wrong flavor.
        assert!(atlas.select_asset("linux", "x86_64", Some("deb")).is_none());
        assert_eq!(
            atlas
                .select_asset("linux", "x86_64", Some("appimage"))
                .unwrap()
                .name,
            "Noema-Atlas-x86_64.AppImage"
        );
    }

    #[test]
    fn version_and_forced_logic() {
        let atlas = sample();
        let a = atlas.app("atlas").unwrap();
        assert!(a.is_newer_than("0.1.0"));
        assert!(a.is_newer_than("0.1.9"));
        assert!(!a.is_newer_than("0.2.0")); // equal is not newer
        assert!(!a.is_newer_than("0.3.0")); // older release than current
        assert!(!a.is_newer_than("not-a-version")); // unparsable -> no update
                                                    // min_supported = 0.1.0: a 0.0.9 client is below the floor.
        assert!(a.is_forced_for("0.0.9"));
        assert!(!a.is_forced_for("0.1.0"));
        assert!(!a.is_forced_for("garbage"));
    }

    #[test]
    fn prerelease_version_ordering() {
        // Atlas/Studio ship prerelease versions (e.g. 0.2.0-alpha.2). Confirm semver
        // ordering does the right thing for the auto-update gate:
        // a later prerelease updates an earlier one, a final release updates a
        // prerelease, and a prerelease does NOT "update" its own final release.
        assert!(version_gt("0.2.0-alpha.3", "0.2.0-alpha.2"));
        assert!(version_gt("0.2.0-alpha.10", "0.2.0-alpha.2")); // numeric, not lexical
        assert!(version_gt("0.2.0", "0.2.0-alpha.2")); // release > prerelease
        assert!(version_gt("0.2.0-alpha.2", "0.1.0")); // newer minor beats old release
        assert!(!version_gt("0.2.0-alpha.2", "0.2.0-alpha.2")); // equal is not newer
        assert!(!version_gt("0.2.0-alpha.1", "0.2.0-alpha.2")); // older prerelease
        assert!(!version_gt("0.2.0-alpha.2", "0.2.0")); // prerelease < its release
    }

    #[test]
    fn expiry_with_skew() {
        let m = sample();
        assert!(!m.is_expired(m.expires_at)); // exactly at expiry: fine
        assert!(!m.is_expired(m.expires_at + EXPIRY_SKEW_MS)); // within skew
        assert!(m.is_expired(m.expires_at + EXPIRY_SKEW_MS + 1)); // past skew
                                                                  // Saturating math: an absurd clock can't panic.
        assert!(m.is_expired(i64::MAX));
        assert!(!m.is_expired(i64::MIN));
    }

    #[test]
    fn json_roundtrip_tolerates_unknown_fields() {
        let mut m = sample();
        m.sign(&KeyPair::generate().secret_hex()).unwrap();
        let mut v = serde_json::to_value(&m).unwrap();
        v.as_object_mut()
            .unwrap()
            .insert("future_field".into(), serde_json::json!("ignored"));
        let bytes = serde_json::to_vec(&v).unwrap();
        let back = ReleaseManifest::from_json(&bytes).unwrap();
        assert_eq!(back.apps[0].version, "0.2.0");
    }
}
