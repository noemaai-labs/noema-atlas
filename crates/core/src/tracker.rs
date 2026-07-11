use crate::error::{Error, Result};
use serde::Deserialize;
use std::time::Duration;

/// Total request budget for a tracker call; provider lookups are capped tighter at the call site.
const TRACKER_HTTP_TIMEOUT: Duration = Duration::from_secs(10);
/// Fail fast on an unroutable tracker host instead of hanging the full request
/// budget on a stuck TCP connect (reqwest has no default connect timeout).
const TRACKER_CONNECT_TIMEOUT: Duration = Duration::from_secs(4);
/// Cap on a tracker JSON response body. The tracker is untrusted, so without a
/// limit a hostile one could stream an unbounded body and exhaust client memory.
const TRACKER_MAX_BODY: usize = 8 * 1024 * 1024; // 8 MiB
use crate::util::read_body_capped;

/// Build the tracker HTTP client, routed through the optional app proxy ("VPN
/// tunnel") so announce/catalog/providers traffic tunnels like everything else.
fn client(proxy: Option<&str>) -> Result<reqwest::Client> {
    let builder = reqwest::Client::builder()
        .user_agent(concat!("noema-atlas/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(TRACKER_CONNECT_TIMEOUT)
        .timeout(TRACKER_HTTP_TIMEOUT);
    crate::transport::apply_proxy(builder, proxy)?
        .build()
        .map_err(|e| Error::other(format!("tracker client: {e}")))
}

/// One file to announce, with optional browse metadata so receivers can find it
/// by name in the catalog (not just resolve a hash they already know).
#[derive(Debug, Clone, Default)]
pub struct AnnounceItem {
    pub blake3: String,
    pub sha256: String,
    pub name: String,
    pub size: u64,
    pub quant: String,
    pub license: String,
    /// BitTorrent magnet (info-hash) when this file is seeded over BT, so the
    /// public catalog can advertise it for swarm joining. Empty otherwise.
    pub magnet: String,
    /// Whether this file may appear in the *public* catalog (the operator opted
    /// it into worldwide sharing).
    pub listable: bool,
}

/// The announcer's identity for the catalog: a human device name.
#[derive(Debug, Clone, Default)]
pub struct Identity {
    pub device: String,
}

/// Ownership proof for announce/withdraw: (request timestamp ms, base64 Ed25519
/// signature over the canonical payload). The registry verifies it against the
/// claimed `node_id`; `None` sends an unsigned request.
pub type AnnounceAuth = (i64, String);

/// Announce that this node is sharing the given items. Re-announce periodically;
/// the tracker expires stale entries. `node_id` (stable hex identity) is the peer
/// key, so a re-announce after address changes replaces the old record instead of
/// counting one device as several peers.
pub async fn announce(
    registry: &str,
    proxy: Option<&str>,
    node_ticket: &str,
    node_id: &str,
    identity: &Identity,
    items: &[AnnounceItem],
    auth: Option<&AnnounceAuth>,
) -> Result<usize> {
    let url = format!("{}/announce", registry.trim_end_matches('/'));
    let items_json: Vec<_> = items
        .iter()
        .map(|i| {
            serde_json::json!({
                "blake3": i.blake3,
                "sha256": i.sha256,
                "name": i.name,
                "size": i.size,
                "quant": i.quant,
                "license": i.license,
                "magnet": i.magnet,
                "listable": i.listable,
            })
        })
        .collect();
    let mut body = serde_json::json!({ "node": node_ticket, "items": items_json });
    if !node_id.is_empty() {
        body["node_id"] = serde_json::json!(node_id);
    }
    if let Some((ts, sig)) = auth {
        body["ts"] = serde_json::json!(ts);
        body["sig"] = serde_json::json!(sig);
    }
    if !identity.device.is_empty() {
        body["device"] = serde_json::json!(identity.device);
    }
    let resp = client(proxy)?
        .post(&url)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body(serde_json::to_vec(&body)?)
        .send()
        .await
        .map_err(|e| Error::other(format!("announce request: {e}")))?;
    if !resp.status().is_success() {
        let status = resp.status();
        // Callers fire-and-forget this; log so a silently-rejected announce (the
        // common cause is a 401 from the registry's --public-url not matching this
        // client's tracker URL, or a fast local clock) is diagnosable.
        tracing::warn!(
            %status,
            items = items.len(),
            "tracker announce rejected — shared models may not be visible to others"
        );
        return Err(Error::other(format!("announce returned {status}")));
    }
    Ok(items.len())
}

/// A browsable catalog row from the tracker.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct CatalogRow {
    #[serde(default)]
    pub blake3: String,
    #[serde(default)]
    pub sha256: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub size: u64,
    #[serde(default)]
    pub quant: String,
    #[serde(default)]
    pub license: String,
    #[serde(default)]
    pub magnet: String,
    /// Distinct peers seeding this over Iroh (excludes you).
    #[serde(default)]
    pub peers: usize,
    /// Distinct peers seeding this over BitTorrent (excludes you).
    #[serde(default)]
    pub bt_seeders: usize,
    #[serde(default)]
    pub devices: Vec<String>,
    /// True when this row is the querier's own share — one of your devices is
    /// seeding it (matched on your NodeId).
    #[serde(default)]
    pub mine: bool,
}

#[derive(Deserialize)]
struct CatalogResp {
    #[serde(default)]
    models: Vec<CatalogRow>,
}

/// Browse the worldwide catalog of shared models. `q` filters by name. `self_id`
/// (this node's NodeId) is excluded from each row's peer count and flags your own
/// shares as `mine`.
pub async fn catalog(
    registry: &str,
    proxy: Option<&str>,
    q: &str,
    self_id: Option<&str>,
) -> Result<Vec<CatalogRow>> {
    let base = registry.trim_end_matches('/');
    let mut url = format!("{base}/catalog?limit=200");
    if !q.is_empty() {
        url.push_str(&format!("&q={}", urlencode(q)));
    }
    if let Some(id) = self_id.filter(|s| !s.is_empty()) {
        url.push_str(&format!("&self={}", urlencode(id)));
    }
    let resp = client(proxy)?
        .get(&url)
        .send()
        .await
        .map_err(|e| Error::other(format!("catalog request: {e}")))?;
    if !resp.status().is_success() {
        return Err(Error::other(format!("catalog returned {}", resp.status())));
    }
    let bytes = read_body_capped(resp, TRACKER_MAX_BODY).await?;
    let parsed: CatalogResp = serde_json::from_slice(&bytes)?;
    Ok(parsed.models)
}

/// Minimal percent-encoding for query values (alnum + a few safe chars pass).
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Providers for a content hash, plus the resolved blake3 needed to fetch it.
#[derive(Debug, Clone, Default)]
pub struct ProviderSet {
    pub blake3: String,
    pub nodes: Vec<String>,
    /// Peers BitTorrent-seeding this file (announced a magnet), excluding self.
    /// `0` against an older registry that doesn't report the field.
    pub bt_seeders: usize,
}

#[derive(Deserialize)]
struct ProvidersResp {
    #[serde(default)]
    blake3: String,
    #[serde(default)]
    bt_seeders: usize,
    #[serde(default)]
    providers: Vec<ProviderEntry>,
}

#[derive(Deserialize)]
struct ProviderEntry {
    node: String,
}

/// Look up providers for a content hash (blake3 or sha256, hex). Returns the
/// resolved blake3 (Iroh's content address) and the provider node tickets.
/// `exclude_self` (this node's NodeId) keeps a peer from discovering itself.
pub async fn providers(
    registry: &str,
    proxy: Option<&str>,
    hash: &str,
    exclude_self: Option<&str>,
) -> Result<ProviderSet> {
    let mut url = format!("{}/providers/{}", registry.trim_end_matches('/'), hash);
    if let Some(id) = exclude_self.filter(|s| !s.is_empty()) {
        url.push_str(&format!("?self={}", urlencode(id)));
    }
    let resp = client(proxy)?
        .get(&url)
        .send()
        .await
        .map_err(|e| Error::other(format!("providers request: {e}")))?;
    if !resp.status().is_success() {
        return Err(Error::other(format!(
            "providers returned {}",
            resp.status()
        )));
    }
    let bytes = read_body_capped(resp, TRACKER_MAX_BODY).await?;
    let parsed: ProvidersResp = serde_json::from_slice(&bytes)?;
    Ok(ProviderSet {
        blake3: parsed.blake3,
        nodes: parsed.providers.into_iter().map(|p| p.node).collect(),
        bt_seeders: parsed.bt_seeders,
    })
}

/// Un-announce content this node no longer serves so it drops from the catalog
/// immediately instead of lingering until its TTL. Empty `blake3s` withdraws
/// everything from this node. Keyed on the stable `node_id`, so it only removes
/// this node's own records. Best-effort: errors are non-fatal to the caller.
pub async fn withdraw(
    registry: &str,
    proxy: Option<&str>,
    node_id: &str,
    blake3s: &[String],
    auth: Option<&AnnounceAuth>,
) -> Result<()> {
    if node_id.is_empty() {
        return Ok(());
    }
    let url = format!("{}/withdraw", registry.trim_end_matches('/'));
    let mut body = serde_json::json!({ "node_id": node_id, "items": blake3s });
    if let Some((ts, sig)) = auth {
        body["ts"] = serde_json::json!(ts);
        body["sig"] = serde_json::json!(sig);
    }
    let resp = client(proxy)?
        .post(&url)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body(serde_json::to_vec(&body)?)
        .send()
        .await
        .map_err(|e| Error::other(format!("withdraw request: {e}")))?;
    if !resp.status().is_success() {
        let status = resp.status();
        tracing::warn!(%status, "tracker withdraw rejected");
        return Err(Error::other(format!("withdraw returned {status}")));
    }
    Ok(())
}
