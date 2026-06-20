use crate::manifest::{Manifest, RedistributionClass, Source, SourceClass};
use crate::platform::PlatformProfile;
use crate::sign::VerificationReport;
use crate::verify::{classify_file_safety, FileSafety};
use std::collections::HashSet;

/// Engine configuration.
#[derive(Debug, Clone, Default)]
pub struct PolicyConfig {
    /// Keys the user trusts to sign manifests (`key_id`s).
    pub trusted_keys: HashSet<String>,
    /// If true, require *some* valid signature even for non-gated public models.
    pub require_signature_always: bool,
    /// High-trust override: permit otherwise-blocked file types (e.g. `.pt`).
    pub allow_unsafe_files: bool,
    /// If true, only manifests signed by a *trusted* key are accepted (vs. any
    /// embedded key with valid math).
    pub require_trusted_signer: bool,
}

/// The result of evaluating a download request.
#[derive(Debug, Clone)]
pub struct PolicyDecision {
    pub allowed: bool,
    pub reason: String,
    pub warnings: Vec<String>,
    pub class: RedistributionClass,
    pub blocked_artifacts: Vec<String>,
}

impl PolicyDecision {
    fn deny(class: RedistributionClass, reason: impl Into<String>) -> Self {
        PolicyDecision {
            allowed: false,
            reason: reason.into(),
            warnings: Vec::new(),
            class,
            blocked_artifacts: Vec::new(),
        }
    }
}

/// The policy engine. Stateless besides its config; emits decisions the engine
/// records to `policy_events` for audit.
pub struct PolicyEngine {
    cfg: PolicyConfig,
}

impl PolicyEngine {
    pub fn new(cfg: PolicyConfig) -> Self {
        PolicyEngine { cfg }
    }

    pub fn config(&self) -> &PolicyConfig {
        &self.cfg
    }

    /// Evaluate whether a manifest may be downloaded at all.
    pub fn evaluate_download(
        &self,
        manifest: &Manifest,
        report: &VerificationReport,
    ) -> PolicyDecision {
        let class = manifest.license.redistribution;
        let needs_signature = manifest.access.gated
            || manifest.access.require_signed_manifest
            || self.cfg.require_signature_always
            || matches!(
                class,
                RedistributionClass::GatedNoRedistribution | RedistributionClass::EnterprisePrivate
            );

        // Restricted classes are as trust-sensitive as the explicit `gated` flag.
        let class_requires_trust = matches!(
            class,
            RedistributionClass::GatedNoRedistribution | RedistributionClass::EnterprisePrivate
        );

        if needs_signature {
            if !report.is_signed() {
                return PolicyDecision::deny(
                    class,
                    "this model requires a signed manifest, but no valid signature is present",
                );
            }
            if (self.cfg.require_trusted_signer || manifest.access.gated || class_requires_trust)
                && !report.is_trusted_by(&self.cfg.trusted_keys)
            {
                return PolicyDecision::deny(
                    class,
                    "manifest is signed, but not by a key you trust",
                );
            }
        }
        let mut warnings = Vec::new();
        let mut blocked = Vec::new();
        for art in &manifest.artifacts {
            let (safety, why) = classify_file_safety(&art.path);
            match safety {
                FileSafety::Safe => {}
                FileSafety::Warn => warnings.push(format!("{}: {}", art.path, why)),
                FileSafety::Blocked => {
                    if self.cfg.allow_unsafe_files {
                        warnings.push(format!("{}: {} (allowed by override)", art.path, why));
                    } else {
                        blocked.push(art.path.clone());
                    }
                }
            }
        }
        if !blocked.is_empty() {
            return PolicyDecision {
                allowed: false,
                reason: format!(
                    "{} artifact(s) use blocked unsafe file types: {}",
                    blocked.len(),
                    blocked.join(", ")
                ),
                warnings,
                class,
                blocked_artifacts: blocked,
            };
        }

        PolicyDecision {
            allowed: true,
            reason: "download permitted".into(),
            warnings,
            class,
            blocked_artifacts: Vec::new(),
        }
    }

    /// Whether a particular source may be *fetched from* for this manifest, on
    /// this platform. (Auth availability is checked separately by the engine.)
    pub fn source_fetch_allowed(
        &self,
        manifest: &Manifest,
        source: &Source,
        profile: &PlatformProfile,
    ) -> (bool, String) {
        let class = source.class();
        // The publisher's allow-list, when present, is authoritative.
        if !manifest.access.allowed_source_classes.is_empty()
            && !manifest.access.allowed_source_classes.contains(&class)
        {
            return (
                false,
                format!("{class:?} not in manifest allowed_source_classes"),
            );
        }
        if !profile.fetch_enabled(class) {
            if matches!(class, SourceClass::Huggingface) {
                return (
                    false,
                    "Hugging Face downloads are off — P2P only (enable in Settings)".into(),
                );
            }
            return (
                false,
                format!("{class:?} disabled on {:?}", profile.platform),
            );
        }
        (true, "ok".into())
    }

    /// Whether we may *advertise / reseed* an artifact onto a public peer
    /// network. The cardinal redistribution rule lives here.
    pub fn redistribution_allowed(
        &self,
        manifest: &Manifest,
        class_of_source: SourceClass,
        profile: &PlatformProfile,
    ) -> (bool, String) {
        let is_public = matches!(class_of_source, SourceClass::Ipfs | SourceClass::Iroh);
        if !is_public {
            return (true, "not a public-distribution channel".into());
        }
        if manifest.access.gated {
            return (false, "gated model: no public redistribution".into());
        }
        if !manifest
            .license
            .redistribution
            .allows_public_redistribution()
        {
            return (
                false,
                format!(
                    "redistribution class `{}` forbids public reseeding",
                    manifest.license.redistribution.as_str()
                ),
            );
        }
        if !profile.allow_public_seeding {
            return (false, "public seeding disabled on this platform".into());
        }
        (true, "public redistribution permitted".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::tests_support::sample_manifest;
    use crate::sign::{verify_manifest, KeyPair};

    fn signed_sample() -> (Manifest, VerificationReport, KeyPair) {
        let mut m = sample_manifest();
        let kp = KeyPair::generate();
        kp.sign_manifest(&mut m).unwrap();
        let report = verify_manifest(&m).unwrap();
        (m, report, kp)
    }

    #[test]
    fn public_signed_download_allowed() {
        let (m, report, _) = signed_sample();
        let engine = PolicyEngine::new(PolicyConfig::default());
        let d = engine.evaluate_download(&m, &report);
        assert!(d.allowed, "{}", d.reason);
    }

    #[test]
    fn gated_requires_trusted_signature() {
        let (mut m, _, kp) = signed_sample();
        m.access.gated = true;
        m.license.redistribution = RedistributionClass::GatedNoRedistribution;
        // Re-sign after mutation.
        m.signatures.clear();
        kp.sign_manifest(&mut m).unwrap();
        let report = verify_manifest(&m).unwrap();

        // Untrusted: denied.
        let engine = PolicyEngine::new(PolicyConfig::default());
        assert!(!engine.evaluate_download(&m, &report).allowed);

        // Trusted: allowed.
        let mut trusted = HashSet::new();
        trusted.insert(kp.key_id());
        let engine = PolicyEngine::new(PolicyConfig {
            trusted_keys: trusted,
            ..Default::default()
        });
        assert!(engine.evaluate_download(&m, &report).allowed);
    }

    #[test]
    fn restricted_class_requires_trusted_signer_even_without_gated_flag() {
        // gated flag is false, but redistribution class is gated_no_redistribution.
        let (mut m, _, kp) = signed_sample();
        m.access.gated = false;
        m.license.redistribution = RedistributionClass::GatedNoRedistribution;
        m.signatures.clear();
        kp.sign_manifest(&mut m).unwrap();
        let report = verify_manifest(&m).unwrap();

        // Untrusted signer => denied.
        let engine = PolicyEngine::new(PolicyConfig::default());
        assert!(!engine.evaluate_download(&m, &report).allowed);

        // Trusted signer => allowed.
        let mut trusted = HashSet::new();
        trusted.insert(kp.key_id());
        let engine = PolicyEngine::new(PolicyConfig {
            trusted_keys: trusted,
            ..Default::default()
        });
        assert!(engine.evaluate_download(&m, &report).allowed);
    }

    #[test]
    fn gated_forbids_public_redistribution() {
        let (mut m, _, _) = signed_sample();
        m.access.gated = true;
        let engine = PolicyEngine::new(PolicyConfig::default());
        let profile = PlatformProfile::desktop();
        let (ok, _) = engine.redistribution_allowed(&m, SourceClass::Iroh, &profile);
        assert!(!ok);
        let (ok2, _) = engine.redistribution_allowed(&m, SourceClass::Huggingface, &profile);
        assert!(ok2);
    }

    #[test]
    fn blocked_file_type_denies_download() {
        let (mut m, report, _) = signed_sample();
        m.artifacts[0].path = "pytorch_model.pkl".into();
        let engine = PolicyEngine::new(PolicyConfig::default());
        let d = engine.evaluate_download(&m, &report);
        assert!(!d.allowed);
        assert!(!d.blocked_artifacts.is_empty());
        let engine2 = PolicyEngine::new(PolicyConfig {
            allow_unsafe_files: true,
            ..Default::default()
        });
        let d2 = engine2.evaluate_download(&m, &report);
        assert!(d2.allowed);
        assert!(!d2.warnings.is_empty());
    }

    #[test]
    fn source_class_allowlist_enforced() {
        let (m, _, _) = signed_sample();
        // sample allows only Huggingface + HttpsMirror.
        let engine = PolicyEngine::new(PolicyConfig::default());
        let profile = PlatformProfile::desktop();
        let ipfs = Source::Ipfs {
            cid: "bafy".into(),
            retrieval: vec![],
            auth: crate::manifest::AuthPolicy::None,
        };
        let (ok, _) = engine.source_fetch_allowed(&m, &ipfs, &profile);
        assert!(!ok);
    }

    #[test]
    fn hugging_face_fetch_is_blocked_until_profile_opt_in() {
        let (m, _, _) = signed_sample();
        let engine = PolicyEngine::new(PolicyConfig::default());
        let hf = Source::Huggingface {
            repo_id: "Qwen/Qwen3-8B-Instruct-GGUF".into(),
            revision: "0123".into(),
            path: "qwen3-8b-instruct-q4_k_m.gguf".into(),
            auth: crate::manifest::AuthPolicy::None,
        };

        let mut profile = PlatformProfile::desktop();
        let (ok, reason) = engine.source_fetch_allowed(&m, &hf, &profile);
        assert!(!ok);
        assert!(reason.contains("Hugging Face downloads are off"));

        profile.huggingface_download = true;
        let (ok, reason) = engine.source_fetch_allowed(&m, &hf, &profile);
        assert!(ok, "{reason}");
    }
}
