use crate::error::{Error, Result};
use crate::manifest::{Manifest, PublicKey, SigAlgorithm, Signature};
use base64::Engine as _;
use ed25519_dalek::{Signer, SigningKey, Verifier, VerifyingKey};
use std::collections::HashSet;

/// An Ed25519 key pair used to sign manifests.
pub struct KeyPair {
    signing: SigningKey,
}

impl KeyPair {
    /// Generate a fresh key pair from the OS CSPRNG.
    pub fn generate() -> Self {
        let signing = SigningKey::generate(&mut rand_core::OsRng);
        KeyPair { signing }
    }

    /// Reconstruct from a 32-byte secret seed (hex).
    pub fn from_secret_hex(hex_seed: &str) -> Result<Self> {
        let bytes =
            hex::decode(hex_seed.trim()).map_err(|e| Error::Key(format!("bad secret hex: {e}")))?;
        let arr: [u8; 32] = bytes
            .as_slice()
            .try_into()
            .map_err(|_| Error::Key("secret key must be 32 bytes".into()))?;
        Ok(KeyPair {
            signing: SigningKey::from_bytes(&arr),
        })
    }

    /// The 32-byte secret seed as hex. Treat as a secret — store only in the OS
    /// keystore or a permission-restricted file.
    pub fn secret_hex(&self) -> String {
        hex::encode(self.signing.to_bytes())
    }

    /// The 32-byte public key as hex.
    pub fn public_hex(&self) -> String {
        hex::encode(self.signing.verifying_key().to_bytes())
    }

    /// The conventional key id: `ed25519:<hex32-public-key>`.
    pub fn key_id(&self) -> String {
        format!("ed25519:{}", self.public_hex())
    }

    /// Raw ed25519 signature (64 bytes) over arbitrary bytes. Used by callers that
    /// sign their own canonical payload rather than a [`Manifest`] (e.g. the release
    /// manifest in [`crate::update`]).
    pub fn sign_bytes(&self, msg: &[u8]) -> [u8; 64] {
        self.signing.sign(msg).to_bytes()
    }

    /// The [`PublicKey`] descriptor to embed in a manifest.
    pub fn public_key_descriptor(&self) -> PublicKey {
        PublicKey {
            key_id: self.key_id(),
            algorithm: SigAlgorithm::Ed25519,
            public_key: self.public_hex(),
            purpose: vec!["manifest-signing".to_string()],
        }
    }

    /// Sign a manifest in place: ensures the public key is embedded, recomputes
    /// the manifest id from content if it is empty, then appends a signature.
    pub fn sign_manifest(&self, manifest: &mut Manifest) -> Result<()> {
        let key_id = self.key_id();

        // Ensure our public key is present in the publisher key set *before*
        // canonicalization, so the signature commits to it.
        if !manifest
            .publisher
            .public_keys
            .iter()
            .any(|k| k.key_id == key_id)
        {
            manifest
                .publisher
                .public_keys
                .push(self.public_key_descriptor());
        }

        // Drop any prior signature from this key (re-signing is idempotent).
        manifest.signatures.retain(|s| s.key_id != key_id);

        let bytes = manifest.canonical_bytes()?;
        let sig = self.signing.sign(&bytes);
        manifest.signatures.push(Signature {
            key_id,
            algorithm: SigAlgorithm::Ed25519,
            signature: base64::engine::general_purpose::STANDARD.encode(sig.to_bytes()),
        });
        Ok(())
    }
}

/// The outcome of verifying a manifest's signatures.
#[derive(Debug, Clone, Default)]
pub struct VerificationReport {
    /// key_ids whose signatures verified against their embedded public key.
    pub valid_signatures: Vec<String>,
    /// key_ids whose signatures were present but failed verification.
    pub invalid_signatures: Vec<String>,
    /// Total signatures present on the manifest.
    pub total_signatures: usize,
}

impl VerificationReport {
    /// At least one signature verified against an embedded key.
    pub fn is_signed(&self) -> bool {
        !self.valid_signatures.is_empty()
    }

    /// At least one *trusted* key (per `trusted`) produced a valid signature.
    pub fn is_trusted_by(&self, trusted: &HashSet<String>) -> bool {
        self.valid_signatures.iter().any(|k| trusted.contains(k))
    }
}

/// Verify every signature on a manifest against the public keys embedded in the
/// manifest itself. This proves the math, not the trust: the caller decides
/// whether any of the validating keys is one it trusts.
pub fn verify_manifest(manifest: &Manifest) -> Result<VerificationReport> {
    let bytes = manifest.canonical_bytes()?;
    let mut report = VerificationReport {
        total_signatures: manifest.signatures.len(),
        ..Default::default()
    };

    for sig in &manifest.signatures {
        match sig.algorithm {
            SigAlgorithm::Ed25519 => {}
        }
        let pubkey = manifest
            .publisher
            .public_keys
            .iter()
            .find(|k| k.key_id == sig.key_id);
        let Some(pk) = pubkey else {
            report.invalid_signatures.push(sig.key_id.clone());
            continue;
        };
        match verify_one(&bytes, pk, sig) {
            Ok(()) => report.valid_signatures.push(sig.key_id.clone()),
            Err(_) => report.invalid_signatures.push(sig.key_id.clone()),
        }
    }
    Ok(report)
}

fn verify_one(bytes: &[u8], pk: &PublicKey, sig: &Signature) -> Result<()> {
    // The trust decision keys off `key_id` (see `is_trusted_by`), so the verifying
    // key MUST be the one *named* by `key_id` — `ed25519:<hex32>` — never the
    // separately-supplied `public_key` field. Otherwise an attacker could label
    // their own key with a victim's trusted `key_id`, sign with their own key, and
    // have the manifest verify as trusted. Bind the two: derive the key from the id
    // and reject any descriptor whose `public_key` disagrees with it.
    let key_hex = sig
        .key_id
        .strip_prefix("ed25519:")
        .ok_or_else(|| Error::Key("key_id must be of the form ed25519:<hex>".into()))?;
    if !pk.public_key.eq_ignore_ascii_case(key_hex) {
        return Err(Error::Key(
            "key_id does not match its public_key material".into(),
        ));
    }
    let pk_bytes =
        hex::decode(key_hex).map_err(|e| Error::Key(format!("bad public key hex: {e}")))?;
    let pk_arr: [u8; 32] = pk_bytes
        .as_slice()
        .try_into()
        .map_err(|_| Error::Key("public key must be 32 bytes".into()))?;
    let vk = VerifyingKey::from_bytes(&pk_arr)
        .map_err(|e| Error::Key(format!("invalid ed25519 public key: {e}")))?;

    let sig_bytes = base64::engine::general_purpose::STANDARD
        .decode(sig.signature.as_bytes())
        .map_err(|e| Error::Signature(format!("bad base64 signature: {e}")))?;
    let sig_arr: [u8; 64] = sig_bytes
        .as_slice()
        .try_into()
        .map_err(|_| Error::Signature("signature must be 64 bytes".into()))?;
    let signature = ed25519_dalek::Signature::from_bytes(&sig_arr);

    vk.verify(bytes, &signature)
        .map_err(|e| Error::Signature(format!("verification failed: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::tests_support::sample_manifest;

    #[test]
    fn sign_then_verify_roundtrip() {
        let kp = KeyPair::generate();
        let mut m = sample_manifest();
        kp.sign_manifest(&mut m).unwrap();

        let report = verify_manifest(&m).unwrap();
        assert!(report.is_signed());
        assert_eq!(report.valid_signatures.len(), 1);
        assert_eq!(report.invalid_signatures.len(), 0);
        assert_eq!(report.valid_signatures[0], kp.key_id());
    }

    #[test]
    fn tampering_breaks_signature() {
        let kp = KeyPair::generate();
        let mut m = sample_manifest();
        kp.sign_manifest(&mut m).unwrap();

        // Tamper with content after signing.
        m.artifacts[0].size_bytes += 1;
        let report = verify_manifest(&m).unwrap();
        assert!(!report.is_signed());
        assert_eq!(report.invalid_signatures.len(), 1);
    }

    #[test]
    fn secret_roundtrip() {
        let kp = KeyPair::generate();
        let hex = kp.secret_hex();
        let kp2 = KeyPair::from_secret_hex(&hex).unwrap();
        assert_eq!(kp.public_hex(), kp2.public_hex());
    }

    #[test]
    fn forged_key_id_label_is_not_trusted() {
        // An attacker embeds their OWN public key but labels it (and the signature)
        // with a victim's trusted key_id. The signature math is valid for the
        // attacker's key, but it must NOT verify as the victim's key.
        let victim = KeyPair::generate();
        let attacker = KeyPair::generate();
        let victim_id = victim.key_id();

        let mut m = sample_manifest();
        attacker.sign_manifest(&mut m).unwrap();
        // Relabel the attacker's embedded key + signature with the victim's id,
        // keeping the attacker's actual public_key material.
        for k in &mut m.publisher.public_keys {
            k.key_id = victim_id.clone();
        }
        for s in &mut m.signatures {
            s.key_id = victim_id.clone();
        }

        let report = verify_manifest(&m).unwrap();
        assert!(
            !report.is_signed(),
            "manifest with mismatched key_id/public_key must not verify"
        );

        let mut trusted = HashSet::new();
        trusted.insert(victim_id);
        assert!(!report.is_trusted_by(&trusted));
    }

    #[test]
    fn wrong_key_not_trusted() {
        let kp = KeyPair::generate();
        let mut m = sample_manifest();
        kp.sign_manifest(&mut m).unwrap();
        let report = verify_manifest(&m).unwrap();

        let mut trusted = HashSet::new();
        trusted.insert("ed25519:deadbeef".to_string());
        assert!(!report.is_trusted_by(&trusted));

        trusted.insert(kp.key_id());
        assert!(report.is_trusted_by(&trusted));
    }
}
