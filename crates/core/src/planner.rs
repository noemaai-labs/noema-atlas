use crate::db::SourceHealth;
use crate::manifest::{Artifact, Manifest, Source, SourceClass};
use crate::platform::PlatformProfile;
use crate::policy::PolicyEngine;

/// A source with its computed score and eligibility verdict.
#[derive(Debug, Clone)]
pub struct ScoredSource {
    pub source: Source,
    pub source_id: String,
    pub score: f64,
    pub eligible: bool,
    pub reason: String,
}

/// The plan for fetching one artifact.
#[derive(Debug, Clone, Default)]
pub struct Plan {
    /// Eligible sources, best first.
    pub eligible: Vec<ScoredSource>,
    /// Excluded sources with the reason (for diagnostics / UI).
    pub excluded: Vec<ScoredSource>,
}

impl Plan {
    pub fn best(&self) -> Option<&ScoredSource> {
        self.eligible.first()
    }

    pub fn is_empty(&self) -> bool {
        self.eligible.is_empty()
    }
}

/// Build a fetch plan for `artifact`. `health` maps a `source_id` to its
/// recorded health (callers typically pass a closure over the DB).
pub fn plan_artifact<F>(
    manifest: &Manifest,
    artifact: &Artifact,
    profile: &PlatformProfile,
    policy: &PolicyEngine,
    health: F,
) -> Plan
where
    F: Fn(&str) -> SourceHealth,
{
    let mut eligible = Vec::new();
    let mut excluded = Vec::new();

    for source in &artifact.sources {
        let source_id = source.source_id();
        let class = source.class();
        let (allowed, why) = policy.source_fetch_allowed(manifest, source, profile);
        if !allowed {
            excluded.push(ScoredSource {
                source: source.clone(),
                source_id,
                score: 0.0,
                eligible: false,
                reason: why,
            });
            continue;
        }

        let h = health(&source_id);
        if h.banned {
            excluded.push(ScoredSource {
                source: source.clone(),
                source_id,
                score: 0.0,
                eligible: false,
                reason: "source banned after integrity failure".into(),
            });
            continue;
        }

        let score = score_source(class, &h, profile);
        eligible.push(ScoredSource {
            source: source.clone(),
            source_id,
            score,
            eligible: true,
            reason: "eligible".into(),
        });
    }

    // Highest score first; stable so equal scores keep manifest order.
    eligible.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    Plan { eligible, excluded }
}

/// Compute a source's score. Weighted blend of:
fn score_source(class: SourceClass, h: &SourceHealth, profile: &PlatformProfile) -> f64 {
    let mut score = profile.class_priority(class);

    let total = h.success_count + h.failure_count;
    let success_ratio = if total > 0 {
        h.success_count as f64 / total as f64
    } else {
        0.5 // neutral prior for an unseen source
    };
    score += success_ratio * 30.0;
    score -= h.integrity_failures as f64 * 50.0;

    if let Some(ms) = h.last_latency_ms {
        let ms = ms.clamp(0, 2000) as f64;
        score += (1.0 - ms / 2000.0) * 10.0;
    }
    if matches!(class, SourceClass::Ipfs | SourceClass::Iroh) {
        score += 5.0;
    }
    if (profile.metered || profile.battery_saver) && matches!(class, SourceClass::Iroh) {
        score -= 25.0;
    }

    score
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::tests_support::sample_manifest;
    use crate::manifest::{AuthPolicy, Source};
    use crate::policy::{PolicyConfig, PolicyEngine};

    fn no_health(_: &str) -> SourceHealth {
        SourceHealth::default()
    }

    #[test]
    fn plans_eligible_sources_sorted() {
        let mut m = sample_manifest();
        // Allow all classes for this test.
        m.access.allowed_source_classes.clear();
        m.artifacts[0].sources = vec![
            Source::HttpsMirror {
                url: "https://m/x".into(),
                auth: AuthPolicy::None,
            },
            // LAN peering has been removed: a LanPeer source is never eligible
            // (it deserializes for back-compat but `fetch_enabled` is false), so
            // it must be excluded from the plan rather than ranked.
            Source::LanPeer {
                url: "http://peer/x".into(),
                auth: AuthPolicy::None,
            },
        ];
        let engine = PolicyEngine::new(PolicyConfig::default());
        let plan = plan_artifact(
            &m,
            &m.artifacts[0],
            &PlatformProfile::desktop(),
            &engine,
            no_health,
        );
        // Only the HTTPS mirror is fetchable; the LAN source is dropped.
        assert_eq!(plan.eligible.len(), 1);
        assert!(matches!(
            plan.best().unwrap().source,
            Source::HttpsMirror { .. }
        ));
        assert!(plan
            .excluded
            .iter()
            .any(|s| matches!(s.source, Source::LanPeer { .. })));
    }

    #[test]
    fn banned_sources_are_excluded() {
        let mut m = sample_manifest();
        m.access.allowed_source_classes.clear();
        m.artifacts[0].sources = vec![Source::HttpsMirror {
            url: "https://m/x".into(),
            auth: AuthPolicy::None,
        }];
        let engine = PolicyEngine::new(PolicyConfig::default());
        let banned = |_: &str| SourceHealth {
            banned: true,
            ..Default::default()
        };
        let plan = plan_artifact(
            &m,
            &m.artifacts[0],
            &PlatformProfile::desktop(),
            &engine,
            banned,
        );
        assert!(plan.is_empty());
        assert_eq!(plan.excluded.len(), 1);
    }

    #[test]
    fn disallowed_class_excluded() {
        let m = sample_manifest(); // allows only HF + HTTPS
        let mut art = m.artifacts[0].clone();
        art.sources = vec![Source::Ipfs {
            cid: "bafy".into(),
            retrieval: vec![],
            auth: AuthPolicy::None,
        }];
        let engine = PolicyEngine::new(PolicyConfig::default());
        let plan = plan_artifact(&m, &art, &PlatformProfile::desktop(), &engine, no_health);
        assert!(plan.is_empty());
    }

    #[test]
    fn hugging_face_source_is_excluded_by_default() {
        let m = sample_manifest(); // allows HF + HTTPS
        let mut art = m.artifacts[0].clone();
        art.sources = vec![Source::Huggingface {
            repo_id: "Qwen/Qwen3-8B-Instruct-GGUF".into(),
            revision: "0123".into(),
            path: "qwen3-8b-instruct-q4_k_m.gguf".into(),
            auth: AuthPolicy::None,
        }];
        let engine = PolicyEngine::new(PolicyConfig::default());
        let plan = plan_artifact(&m, &art, &PlatformProfile::desktop(), &engine, no_health);

        assert!(plan.is_empty());
        assert_eq!(plan.excluded.len(), 1);
        assert!(plan.excluded[0]
            .reason
            .contains("Hugging Face downloads are off"));
    }

    #[test]
    fn hugging_face_source_can_be_enabled_as_last_resort() {
        let mut m = sample_manifest();
        m.access.allowed_source_classes.clear();
        m.artifacts[0].sources = vec![
            Source::Huggingface {
                repo_id: "Qwen/Qwen3-8B-Instruct-GGUF".into(),
                revision: "0123".into(),
                path: "qwen3-8b-instruct-q4_k_m.gguf".into(),
                auth: AuthPolicy::None,
            },
            Source::HttpsMirror {
                url: "https://m/x".into(),
                auth: AuthPolicy::None,
            },
        ];
        let mut profile = PlatformProfile::desktop();
        profile.huggingface_download = true;
        let engine = PolicyEngine::new(PolicyConfig::default());
        let plan = plan_artifact(&m, &m.artifacts[0], &profile, &engine, no_health);

        assert_eq!(plan.eligible.len(), 2);
        assert!(matches!(
            plan.best().unwrap().source,
            Source::HttpsMirror { .. }
        ));
    }
}
