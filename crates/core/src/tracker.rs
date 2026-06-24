use crate::error::{Error, Result};
use serde::Deserialize;
use std::time::Duration;

/// Total request budget for a tracker call. Provider lookups are additionally
/// capped tighter at the call site (the engine races them against a short
/// timeout) so a slow tracker never dominates the time-to-first-peer.
const TRACKER_HTTP_TIMEOUT: Duration = Duration::from_secs(10);
/// Fail fast on an unroutable tracker host instead of hanging the full request
/// budget on a stuck TCP connect (reqwest has no default connect timeout).
const TRACKER_CONNECT_TIMEOUT: Duration = Duration::from_secs(4);

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

/// Ownership proof for an announce/withdraw: the request timestamp (ms) and a
/// base64 Ed25519 signature over the canonical payload (see
/// [`crate::announce_auth`]), produced from this device's node secret key. The
/// registry verifies it against the claimed `node_id`. `None` sends an unsigned
/// request (e.g. an iroh-less build); a registry that requires auth rejects it.
pub type AnnounceAuth = (i64, String);

/// Announce that this node is sharing the given items (with browse metadata).
/// Re-announce periodically; the tracker expires stale entries.
///
/// `node_id` is this device's *stable* identity (hex). The tracker keys peers by
/// it so a re-announce after the node's relay/direct addresses change replaces
/// the old record instead of accumulating a second one (which would otherwise
/// count one device as several peers). `node_ticket` still carries the current
/// reachable address used to actually fetch from this peer.
///
/// `auth` is the signed ownership proof (timestamp + signature) so the registry
/// can confirm the announcer actually controls `node_id` instead of trusting the
/// claim — see [`crate::announce_auth`].
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

/// Browse the worldwide catalog of shared models. `q` filters by name.
/// `self_id` is this node's own NodeId — it's excluded from each row's peer count
/// and flags your own shares as `mine`, so your device never reads as a "peer
/// seeding your files".
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
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| Error::other(format!("catalog response: {e}")))?;
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
}

#[derive(Deserialize)]
struct ProvidersResp {
    #[serde(default)]
    blake3: String,
    #[serde(default)]
    providers: Vec<ProviderEntry>,
}

#[derive(Deserialize)]
struct ProviderEntry {
    node: String,
}

/// Look up providers for a content hash (blake3 or sha256, hex). Returns the
/// resolved blake3 (Iroh's content address) and the provider node tickets.
/// `exclude_self` is this node's own NodeId; passing it keeps a peer from
/// discovering — or fetching from, or counting — itself.
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
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| Error::other(format!("providers response: {e}")))?;
    let parsed: ProvidersResp = serde_json::from_slice(&bytes)?;
    Ok(ProviderSet {
        blake3: parsed.blake3,
        nodes: parsed.providers.into_iter().map(|p| p.node).collect(),
    })
}

/// Un-announce content this node no longer serves, so it drops out of the
/// catalog and provider lists immediately instead of lingering until its TTL.
/// `blake3s` empty means "withdraw everything from this node" (e.g. turning
/// worldwide sharing off). Keyed on the stable `node_id`, so it only ever removes
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
