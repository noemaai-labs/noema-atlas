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
    /// Whether this file may appear in the *public* catalog (the operator opted
    /// it into worldwide sharing).
    pub listable: bool,
}

/// The announcer's identity for the catalog: a human device name and an optional
/// device-group capability id (private shares are scoped to it).
#[derive(Debug, Clone, Default)]
pub struct Identity {
    pub device: String,
    pub group: Option<String>,
}

/// Announce that this node is sharing the given items (with browse metadata).
/// Re-announce periodically; the tracker expires stale entries.
///
/// `node_id` is this device's *stable* identity (hex). The tracker keys peers by
/// it so a re-announce after the node's relay/direct addresses change replaces
/// the old record instead of accumulating a second one (which would otherwise
/// count one device as several peers). `node_ticket` still carries the current
/// reachable address used to actually fetch from this peer.
pub async fn announce(
    registry: &str,
    proxy: Option<&str>,
    node_ticket: &str,
    node_id: &str,
    identity: &Identity,
    items: &[AnnounceItem],
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
                "listable": i.listable,
            })
        })
        .collect();
    let mut body = serde_json::json!({ "node": node_ticket, "items": items_json });
    if !node_id.is_empty() {
        body["node_id"] = serde_json::json!(node_id);
    }
    if !identity.device.is_empty() {
        body["device"] = serde_json::json!(identity.device);
    }
    if let Some(g) = &identity.group {
        if !g.is_empty() {
            body["group"] = serde_json::json!(g);
        }
    }
    let resp = client(proxy)?
        .post(&url)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body(serde_json::to_vec(&body)?)
        .send()
        .await
        .map_err(|e| Error::other(format!("announce request: {e}")))?;
    if !resp.status().is_success() {
        return Err(Error::other(format!("announce returned {}", resp.status())));
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
    pub peers: usize,
    #[serde(default)]
    pub devices: Vec<String>,
    /// True when this row is the querier's own share — either it's one of your
    /// devices seeding it (matched on your NodeId) or it's in your device group.
    #[serde(default)]
    pub mine: bool,
}

#[derive(Deserialize)]
struct CatalogResp {
    #[serde(default)]
    models: Vec<CatalogRow>,
}

/// Browse the worldwide catalog of shared models. `q` filters by name; passing
/// the device-group id also returns that group's private (non-public) shares.
/// `self_id` is this node's own NodeId — it's excluded from each row's peer count
/// and flags your own shares as `mine`, so your device never reads as a "peer
/// seeding your files".
pub async fn catalog(
    registry: &str,
    proxy: Option<&str>,
    q: &str,
    group: Option<&str>,
    self_id: Option<&str>,
) -> Result<Vec<CatalogRow>> {
    let base = registry.trim_end_matches('/');
    let mut url = format!("{base}/catalog?limit=200");
    if !q.is_empty() {
        url.push_str(&format!("&q={}", urlencode(q)));
    }
    if let Some(g) = group {
        if !g.is_empty() {
            url.push_str(&format!("&group={}", urlencode(g)));
        }
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
) -> Result<()> {
    if node_id.is_empty() {
        return Ok(());
    }
    let url = format!("{}/withdraw", registry.trim_end_matches('/'));
    let body = serde_json::json!({ "node_id": node_id, "items": blake3s });
    let resp = client(proxy)?
        .post(&url)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body(serde_json::to_vec(&body)?)
        .send()
        .await
        .map_err(|e| Error::other(format!("withdraw request: {e}")))?;
    if !resp.status().is_success() {
        return Err(Error::other(format!("withdraw returned {}", resp.status())));
    }
    Ok(())
}
