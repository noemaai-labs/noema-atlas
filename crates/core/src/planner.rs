use crate::db::SourceHealth;
use crate::manifest::{Artifact, Manifest, Source, SourceClass};
use crate::platform::PlatformProfile;
use crate::policy::PolicyEngine;

/// How the user wants downloads routed across transports. Folds into
/// [`score_source`] as a per-class bias on top of the base priorities, so it
/// nudges source selection without overriding health/integrity signals.
///
/// `Auto` is the default and keeps the historical weighting. The others express a
/// deliberate trade-off: favor peer-to-peer swarms (less load on origin mirrors),
/// favor BitTorrent specifically, or save data by funneling through a single
/// mirror instead of a high-fanout P2P swarm.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DownloadPreference {
    /// Balanced default: the existing class weighting, unbiased.
    #[default]
    Auto,
    /// Prefer P2P (Iroh + BitTorrent) over the centralized mirrors (HF/HTTPS).
    PreferP2p,
    /// Prefer BitTorrent specifically, ranking it above Iroh.
    PreferBittorrent,
    /// Minimize data use / fanout: favor a single mirror or HF over P2P swarms.
    SaveData,
}

impl DownloadPreference {
    /// Encode as a stable `u8` so the engine can hold it in an atomic for live,
    /// restart-free adjustment. Paired with [`DownloadPreference::from_u8`].
    pub fn as_u8(self) -> u8 {
        match self {
            DownloadPreference::Auto => 0,
            DownloadPreference::PreferP2p => 1,
            DownloadPreference::PreferBittorrent => 2,
            DownloadPreference::SaveData => 3,
        }
    }

    /// Decode from [`DownloadPreference::as_u8`]; any unknown code falls back to
    /// `Auto` so a corrupt/forward-incompatible value is harmless.
    pub fn from_u8(v: u8) -> Self {
        match v {
            1 => DownloadPreference::PreferP2p,
            2 => DownloadPreference::PreferBittorrent,
            3 => DownloadPreference::SaveData,
            _ => DownloadPreference::Auto,
        }
    }
}

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
///
/// `preference` biases the ranking toward the user's chosen routing (P2P /
/// BitTorrent / save-data); `peers` is an optional known live-peer count for this
/// file (from the tracker/catalog) that adds a small bonus favoring better-seeded
/// sources. See [`plan_artifact_with`] for the back-compat `Auto`/no-peers form.
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
    plan_artifact_with(
        manifest,
        artifact,
        profile,
        policy,
        DownloadPreference::Auto,
        None,
        health,
    )
}

/// [`plan_artifact`] with an explicit download preference and optional peer-count
/// hint threaded into scoring.
#[allow(clippy::too_many_arguments)]
pub fn plan_artifact_with<F>(
    manifest: &Manifest,
    artifact: &Artifact,
    profile: &PlatformProfile,
    policy: &PolicyEngine,
    preference: DownloadPreference,
    peers: Option<usize>,
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

        let score = score_source(class, &h, profile, preference, peers);
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

/// Class-tier offset applied per preference. Large enough that the preferred class
/// tier always orders above the others regardless of health/latency/peers: the base
/// class-priority spread (~75) plus every positive within-tier signal (success +30,
/// latency +10, Iroh +5, peers ~+7) sums to well under `200`, so a one-tier gap is
/// never closed by health alone. Health/latency/peers therefore only break ties
/// *within* a tier. (Integrity-failed sources are still sunk by their own penalty —
/// or excluded as `banned` — independent of the preference, by design.)
const TIER_OFFSET: f64 = 200.0;

/// Compute a source's score. The user's download `preference` is applied as a
/// decisive class-tier offset ([`preference_tier`] × [`TIER_OFFSET`]) so the
/// preferred transport class always outranks the others; the platform base class
/// priority, recorded health (success ratio, integrity failures, latency), and a
/// small live-`peers` bonus then only reorder sources *within* the same tier.
fn score_source(
    class: SourceClass,
    h: &SourceHealth,
    profile: &PlatformProfile,
    preference: DownloadPreference,
    peers: Option<usize>,
) -> f64 {
    let mut score = preference_tier(preference, class) as f64 * TIER_OFFSET;
    score += profile.class_priority(class);

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
    if matches!(class, SourceClass::Iroh) {
        score += 5.0;
    }
    if (profile.metered || profile.battery_saver) && matches!(class, SourceClass::Iroh) {
        score -= 25.0;
    }

    score += peer_bonus(class, peers);

    score
}

/// The preference's class tier (higher = a more-preferred class group). Multiplied
/// by [`TIER_OFFSET`] in [`score_source`] so the preference is *decisive*: a fresh
/// source in the preferred tier outranks a healthy one in a lower tier. `Auto` puts
/// every class in one tier so the base priority + health alone decide.
fn preference_tier(preference: DownloadPreference, class: SourceClass) -> i32 {
    use DownloadPreference::*;
    use SourceClass::*;
    match preference {
        Auto => 0,
        // Swarms (Iroh + BT) above the centralized mirrors (HF/HTTPS).
        PreferP2p => match class {
            Iroh | BittorrentV2 => 1,
            Huggingface | HttpsMirror => -1,
            _ => 0,
        },
        // BitTorrent above Iroh, both above the mirrors.
        PreferBittorrent => match class {
            BittorrentV2 => 2,
            Iroh => 1,
            Huggingface | HttpsMirror => -1,
            _ => 0,
        },
        // Save data / minimize fanout: a single mirror or HF above the high-fanout
        // P2P classes.
        SaveData => match class {
            HttpsMirror | Huggingface => 1,
            Iroh | BittorrentV2 => -1,
            _ => 0,
        },
    }
}

/// A small monotonic bonus that favors a better-seeded source, scoped to the Iroh
/// class. The live-peer count threaded into the plan is the *Iroh provider* count
/// the tracker reports (see `Engine::download_artifact`); crediting it to a
/// BitTorrent source too would wrongly boost a dead BT swarm, so only Iroh gets it.
///
/// NOTE: a fresh magnet / bare manifest has no known peer count before `open()`
/// (the swarm is only probed once we join it), so `peers` is `None` there and this
/// is neutral — the count is known only for tracker/catalog-discovered downloads.
fn peer_bonus(class: SourceClass, peers: Option<usize>) -> f64 {
    let Some(n) = peers else { return 0.0 };
    if !matches!(class, SourceClass::Iroh) {
        return 0.0;
    }
    // Diminishing returns: ln(1+n) keeps it monotonic but bounded (≈6.9 at 1000
    // peers), so a well-seeded source is preferred without swamping health signals.
    ((1.0 + n as f64).ln() * 3.0).min(15.0)
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
        art.sources = vec![Source::Iroh {
            blob_hash: "abc".into(),
            tickets: vec![],
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

    fn h() -> SourceHealth {
        SourceHealth::default()
    }

    #[test]
    fn prefer_p2p_demotes_mirrors_below_swarms() {
        let p = PlatformProfile::desktop();
        // Base order puts Iroh > HTTPS; PreferP2p widens that gap further.
        let iroh = score_source(
            SourceClass::Iroh,
            &h(),
            &p,
            DownloadPreference::PreferP2p,
            None,
        );
        let mirror = score_source(
            SourceClass::HttpsMirror,
            &h(),
            &p,
            DownloadPreference::PreferP2p,
            None,
        );
        assert!(iroh > mirror);
    }

    #[test]
    fn prefer_bittorrent_ranks_bt_above_iroh() {
        let p = PlatformProfile::desktop();
        // Auto keeps Iroh ahead of BitTorrent (base priorities).
        let auto_iroh = score_source(SourceClass::Iroh, &h(), &p, DownloadPreference::Auto, None);
        let auto_bt = score_source(
            SourceClass::BittorrentV2,
            &h(),
            &p,
            DownloadPreference::Auto,
            None,
        );
        assert!(auto_iroh > auto_bt);
        // PreferBittorrent flips it.
        let bt = score_source(
            SourceClass::BittorrentV2,
            &h(),
            &p,
            DownloadPreference::PreferBittorrent,
            None,
        );
        let iroh = score_source(
            SourceClass::Iroh,
            &h(),
            &p,
            DownloadPreference::PreferBittorrent,
            None,
        );
        assert!(bt > iroh);
    }

    #[test]
    fn save_data_favors_mirror_over_p2p() {
        let p = PlatformProfile::desktop();
        let mirror = score_source(
            SourceClass::HttpsMirror,
            &h(),
            &p,
            DownloadPreference::SaveData,
            None,
        );
        let iroh = score_source(
            SourceClass::Iroh,
            &h(),
            &p,
            DownloadPreference::SaveData,
            None,
        );
        assert!(mirror > iroh);
    }

    #[test]
    fn more_peers_score_at_least_as_high_and_only_for_iroh() {
        let p = PlatformProfile::desktop();
        // Monotonic in the peer count for the Iroh class.
        let none = score_source(SourceClass::Iroh, &h(), &p, DownloadPreference::Auto, None);
        let few = score_source(
            SourceClass::Iroh,
            &h(),
            &p,
            DownloadPreference::Auto,
            Some(2),
        );
        let many = score_source(
            SourceClass::Iroh,
            &h(),
            &p,
            DownloadPreference::Auto,
            Some(200),
        );
        assert!(few > none);
        assert!(many > few);
        // The peer count is the tracker's Iroh-provider count, so a BitTorrent
        // source (a possibly-dead swarm) is NOT credited with it.
        let bt_none = score_source(
            SourceClass::BittorrentV2,
            &h(),
            &p,
            DownloadPreference::Auto,
            None,
        );
        let bt_peers = score_source(
            SourceClass::BittorrentV2,
            &h(),
            &p,
            DownloadPreference::Auto,
            Some(200),
        );
        assert_eq!(bt_none, bt_peers);
        // Nor does a mirror.
        let mirror_none = score_source(
            SourceClass::HttpsMirror,
            &h(),
            &p,
            DownloadPreference::Auto,
            None,
        );
        let mirror_peers = score_source(
            SourceClass::HttpsMirror,
            &h(),
            &p,
            DownloadPreference::Auto,
            Some(200),
        );
        assert_eq!(mirror_none, mirror_peers);
    }

    /// A pristine source: many clean successes, fast, no integrity failures — the
    /// strongest possible within-tier health signal, used to prove the preference
    /// tier still dominates a *fresh* source of the preferred class.
    fn healthy() -> SourceHealth {
        SourceHealth {
            success_count: 100,
            failure_count: 0,
            integrity_failures: 0,
            last_latency_ms: Some(0),
            ..Default::default()
        }
    }

    #[test]
    fn prefer_bittorrent_fresh_bt_outranks_healthy_iroh() {
        let p = PlatformProfile::desktop();
        // A brand-new BT source (neutral health) must still beat a battle-tested,
        // low-latency Iroh source — the preference tier is decisive.
        let bt_fresh = score_source(
            SourceClass::BittorrentV2,
            &h(),
            &p,
            DownloadPreference::PreferBittorrent,
            None,
        );
        let iroh_healthy = score_source(
            SourceClass::Iroh,
            &healthy(),
            &p,
            DownloadPreference::PreferBittorrent,
            Some(500),
        );
        assert!(bt_fresh > iroh_healthy);
    }

    #[test]
    fn save_data_healthy_iroh_ranks_below_fresh_mirror() {
        let p = PlatformProfile::desktop();
        // SaveData must demote even a healthy, well-seeded Iroh below a fresh mirror.
        let iroh_healthy = score_source(
            SourceClass::Iroh,
            &healthy(),
            &p,
            DownloadPreference::SaveData,
            Some(500),
        );
        let mirror_fresh = score_source(
            SourceClass::HttpsMirror,
            &h(),
            &p,
            DownloadPreference::SaveData,
            None,
        );
        assert!(mirror_fresh > iroh_healthy);
    }

    #[test]
    fn prefer_p2p_fresh_swarms_outrank_healthy_mirrors() {
        let p = PlatformProfile::desktop();
        // Both swarm classes (fresh) must beat a healthy mirror / HF.
        let bt_fresh = score_source(
            SourceClass::BittorrentV2,
            &h(),
            &p,
            DownloadPreference::PreferP2p,
            None,
        );
        let iroh_fresh = score_source(
            SourceClass::Iroh,
            &h(),
            &p,
            DownloadPreference::PreferP2p,
            None,
        );
        let mirror_healthy = score_source(
            SourceClass::HttpsMirror,
            &healthy(),
            &p,
            DownloadPreference::PreferP2p,
            None,
        );
        let hf_healthy = score_source(
            SourceClass::Huggingface,
            &healthy(),
            &p,
            DownloadPreference::PreferP2p,
            None,
        );
        assert!(bt_fresh > mirror_healthy);
        assert!(bt_fresh > hf_healthy);
        assert!(iroh_fresh > mirror_healthy);
        assert!(iroh_fresh > hf_healthy);
    }

    #[test]
    fn download_preference_u8_roundtrips() {
        for pref in [
            DownloadPreference::Auto,
            DownloadPreference::PreferP2p,
            DownloadPreference::PreferBittorrent,
            DownloadPreference::SaveData,
        ] {
            assert_eq!(DownloadPreference::from_u8(pref.as_u8()), pref);
        }
        // Unknown codes fall back to Auto.
        assert_eq!(DownloadPreference::from_u8(99), DownloadPreference::Auto);
    }
}
