//! Signed-announce authentication: the client signs a canonical payload with its
//! node secret key; the registry verifies it against the claimed NodeId (the
//! Ed25519 public key itself, so no extra key distribution). Compiled without the
//! `iroh` feature so the registry can verify; the signing side lives in
//! [`crate::iroh_node`].

use ed25519_dalek::{Signature, Verifier, VerifyingKey};

/// Max lag of a timestamp behind the registry clock before it's rejected as stale (replay limiting).
pub const FRESHNESS_WINDOW_MS: i64 = 5 * 60 * 1000;

/// Max a timestamp may sit in the future (signer clock skew) before rejection.
pub const FUTURE_SKEW_MS: i64 = 3 * 60 * 1000;

/// Canonicalize a registry base URL into the audience token bound by the signature:
/// lowercase scheme+host, drop a default port, strip a trailing slash. Scheme and
/// path stay significant. Client `tracker_url` and registry `--public-url` must agree.
fn canonical_audience(url: &str) -> String {
    let s = url.trim().trim_end_matches('/');
    let Some((scheme, rest)) = s.split_once("://") else {
        // No scheme — treat the whole thing as a host-like token.
        return s.to_ascii_lowercase();
    };
    let scheme = scheme.to_ascii_lowercase();
    let (authority, path) = match rest.split_once('/') {
        Some((a, p)) => (a, Some(p)),
        None => (rest, None),
    };
    let mut authority = authority.to_ascii_lowercase();
    if scheme == "https" {
        authority = authority.trim_end_matches(":443").to_string();
    } else if scheme == "http" {
        authority = authority.trim_end_matches(":80").to_string();
    }
    match path {
        Some(p) => format!("{scheme}://{authority}/{p}"),
        None => format!("{scheme}://{authority}"),
    }
}

/// True only for a canonical content id: exactly 64 lowercase hex chars (rejecting
/// others stops an id smuggling a `\n` to forge extra payload fields).
pub fn is_canonical_id(id: &str) -> bool {
    id.len() == 64
        && id
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Build the canonical payload both sides sign/verify, or `None` if any item id isn't
/// canonical 64-hex. Ids are sorted (order-independent) and the payload binds `method`
/// (an announce sig can't replay as withdraw), `ticket` (MITM can't rewrite the
/// address), and `audience` (no cross-registry replay).
/// Shape: `"<method>\n<node_id>\n<ts>\n<audience>\n<ticket>\n<sorted id>\n…"`.
pub fn canonical_payload(
    method: &str,
    node_id: &str,
    ts_ms: i64,
    ticket: &str,
    audience: &str,
    item_ids: &[String],
) -> Option<Vec<u8>> {
    let mut ids: Vec<&str> = item_ids
        .iter()
        .map(|s| s.as_str())
        .filter(|s| !s.is_empty())
        .collect();
    if !ids.iter().all(|id| is_canonical_id(id)) {
        return None;
    }
    ids.sort_unstable();
    let audience = canonical_audience(audience);
    let mut s = String::with_capacity(
        method.len() + node_id.len() + audience.len() + ticket.len() + 32 + ids.len() * 65,
    );
    s.push_str(method);
    s.push('\n');
    s.push_str(node_id);
    s.push('\n');
    s.push_str(&ts_ms.to_string());
    s.push('\n');
    s.push_str(&audience);
    s.push('\n');
    s.push_str(ticket);
    for id in ids {
        s.push('\n');
        s.push_str(id);
    }
    Some(s.into_bytes())
}

/// Decode a NodeId to its 32 raw Ed25519 public-key bytes: 64-char input is hex,
/// otherwise RFC 4648 base32 (iroh's `PublicKey` string encoding).
fn decode_node_id(node_id: &str) -> Option<[u8; 32]> {
    let s = node_id.trim();
    if s.len() == 64 {
        let mut out = [0u8; 32];
        return hex::decode_to_slice(s, &mut out).ok().map(|_| out);
    }
    let bytes = base32_decode(s)?;
    bytes.as_slice().try_into().ok()
}

/// True if `ts_ms` is fresh: within [`FUTURE_SKEW_MS`] ahead and [`FRESHNESS_WINDOW_MS`]
/// behind `now_ms`. Saturating math so an extreme `ts` (e.g. `i64::MIN`) can't panic.
fn ts_is_fresh(ts_ms: i64, now_ms: i64) -> bool {
    let delta = now_ms.saturating_sub(ts_ms); // >0 = ts in the past, <0 = in the future
    (-FUTURE_SKEW_MS..=FRESHNESS_WINDOW_MS).contains(&delta)
}

/// Verify an announce/withdraw signature against the claimed NodeId. Returns `true`
/// only when `ts_ms` is fresh, every id is canonical 64-hex, the NodeId is a valid
/// Ed25519 key, and the signature over the canonical payload checks out. Any
/// malformed input is a rejection, never a panic.
#[allow(clippy::too_many_arguments)]
pub fn verify(
    method: &str,
    node_id: &str,
    ts_ms: i64,
    ticket: &str,
    audience: &str,
    item_ids: &[String],
    signature_b64: &str,
    now_ms: i64,
) -> bool {
    if !ts_is_fresh(ts_ms, now_ms) {
        return false;
    }
    let Some(payload) = canonical_payload(method, node_id, ts_ms, ticket, audience, item_ids)
    else {
        return false;
    };
    let Some(pk_bytes) = decode_node_id(node_id) else {
        return false;
    };
    let Ok(vk) = VerifyingKey::from_bytes(&pk_bytes) else {
        return false;
    };
    let Ok(sig_bytes) = base64::Engine::decode(
        &base64::engine::general_purpose::STANDARD,
        signature_b64.as_bytes(),
    ) else {
        return false;
    };
    let Ok(sig_arr) = <[u8; 64]>::try_from(sig_bytes.as_slice()) else {
        return false;
    };
    let sig = Signature::from_bytes(&sig_arr);
    vk.verify(&payload, &sig).is_ok()
}

/// Decode an RFC 4648 base32 string (no padding, case-insensitive) to bytes, or
/// `None` on a bad character or invalid bit-length tail.
fn base32_decode(input: &str) -> Option<Vec<u8>> {
    fn val(c: u8) -> Option<u32> {
        match c {
            b'A'..=b'Z' => Some((c - b'A') as u32),
            b'a'..=b'z' => Some((c - b'a') as u32),
            b'2'..=b'7' => Some((c - b'2' + 26) as u32),
            _ => None,
        }
    }
    let mut out = Vec::with_capacity(input.len() * 5 / 8);
    let mut buffer: u32 = 0;
    let mut bits: u32 = 0;
    for &c in input.as_bytes() {
        if c == b'=' {
            break; // tolerate stray padding even though we expect none
        }
        let v = val(c)?;
        buffer = (buffer << 5) | v;
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            out.push((buffer >> bits) as u8);
        }
    }
    // A valid encoding leaves only zero-padding bits (< 5) in the tail.
    if bits >= 5 || (buffer & ((1 << bits) - 1)) != 0 {
        return None;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};

    fn b32_encode(bytes: &[u8]) -> String {
        const A: &[u8] = b"abcdefghijklmnopqrstuvwxyz234567";
        let mut out = String::new();
        let mut buffer: u32 = 0;
        let mut bits: u32 = 0;
        for &b in bytes {
            buffer = (buffer << 8) | b as u32;
            bits += 8;
            while bits >= 5 {
                bits -= 5;
                out.push(A[((buffer >> bits) & 0x1f) as usize] as char);
            }
        }
        if bits > 0 {
            out.push(A[((buffer << (5 - bits)) & 0x1f) as usize] as char);
        }
        out
    }

    #[test]
    fn base32_roundtrip_matches_known_vectors() {
        // RFC 4648 vectors (lowercased, no padding).
        assert_eq!(base32_decode("my").unwrap(), b"f");
        assert_eq!(base32_decode("mzxw6").unwrap(), b"foo");
        assert_eq!(base32_decode("mzxw6yq").unwrap(), b"foob");
        assert_eq!(base32_decode("MZXW6YQ").unwrap(), b"foob"); // case-insensitive
        assert_eq!(base32_decode("mzxw6ytboi").unwrap(), b"foobar");
        // Our own encoder (used in tests) round-trips through the decoder.
        for v in [&b"x"[..], b"hello", b"\x00\x01\x02\x03\x04"] {
            assert_eq!(base32_decode(&b32_encode(v)).unwrap(), v);
        }
    }

    const TICKET: &str = "node-ticket-v1";
    const AUD: &str = "https://atlas.noemaai.com";

    // Two distinct canonical 64-hex ids for payloads.
    fn id_a() -> String {
        "aa".repeat(32)
    }
    fn id_b() -> String {
        "bb".repeat(32)
    }

    fn sign(
        sk: &SigningKey,
        method: &str,
        node_id: &str,
        ts: i64,
        ticket: &str,
        aud: &str,
        items: &[String],
    ) -> String {
        let payload = canonical_payload(method, node_id, ts, ticket, aud, items).unwrap();
        let sig = sk.sign(&payload);
        base64::Engine::encode(&base64::engine::general_purpose::STANDARD, sig.to_bytes())
    }

    #[test]
    fn sign_and_verify_roundtrip_base32_id() {
        let sk = SigningKey::generate(&mut rand_core::OsRng);
        let node_id = b32_encode(sk.verifying_key().as_bytes());
        let ts = 1_700_000_000_000;
        let items = vec![id_b(), id_a()];
        let sig_b64 = sign(&sk, "announce", &node_id, ts, TICKET, AUD, &items);
        assert!(verify(
            "announce", &node_id, ts, TICKET, AUD, &items, &sig_b64, ts
        ));
        // Item order doesn't matter (canonical sorts).
        let reordered = vec![id_a(), id_b()];
        assert!(verify(
            "announce", &node_id, ts, TICKET, AUD, &reordered, &sig_b64, ts
        ));
    }

    #[test]
    fn sign_and_verify_roundtrip_hex_id() {
        let sk = SigningKey::generate(&mut rand_core::OsRng);
        let node_id = hex::encode(sk.verifying_key().as_bytes());
        let ts = 1_700_000_000_000;
        let sig_b64 = sign(&sk, "withdraw", &node_id, ts, TICKET, AUD, &[]);
        assert!(verify(
            "withdraw",
            &node_id,
            ts,
            TICKET,
            AUD,
            &[],
            &sig_b64,
            ts
        ));
    }

    #[test]
    fn rejects_wrong_method_node_or_stale() {
        let sk = SigningKey::generate(&mut rand_core::OsRng);
        let node_id = hex::encode(sk.verifying_key().as_bytes());
        let ts = 1_700_000_000_000;
        let items = vec![id_a()];
        let sig_b64 = sign(&sk, "announce", &node_id, ts, TICKET, AUD, &items);

        // Wrong method (announce sig replayed as withdraw) fails.
        assert!(!verify(
            "withdraw", &node_id, ts, TICKET, AUD, &items, &sig_b64, ts
        ));
        // Different claimed node id fails (signature is over a different key).
        let other = hex::encode(
            SigningKey::generate(&mut rand_core::OsRng)
                .verifying_key()
                .as_bytes(),
        );
        assert!(!verify(
            "announce", &other, ts, TICKET, AUD, &items, &sig_b64, ts
        ));
        // Stale timestamp outside the freshness window fails.
        assert!(!verify(
            "announce",
            &node_id,
            ts,
            TICKET,
            AUD,
            &items,
            &sig_b64,
            ts + FRESHNESS_WINDOW_MS + 1
        ));
        // Tampered item set fails.
        let tampered = vec![id_b()];
        assert!(!verify(
            "announce", &node_id, ts, TICKET, AUD, &tampered, &sig_b64, ts
        ));
    }

    #[test]
    fn rejects_tampered_ticket() {
        let sk = SigningKey::generate(&mut rand_core::OsRng);
        let node_id = hex::encode(sk.verifying_key().as_bytes());
        let ts = 1_700_000_000_000;
        let items = vec![id_a()];
        let sig_b64 = sign(&sk, "announce", &node_id, ts, TICKET, AUD, &items);
        // A MITM rewrites the reachable ticket but keeps the captured signature.
        assert!(!verify(
            "announce",
            &node_id,
            ts,
            "attacker-ticket",
            AUD,
            &items,
            &sig_b64,
            ts
        ));
        // The genuine ticket still verifies.
        assert!(verify(
            "announce", &node_id, ts, TICKET, AUD, &items, &sig_b64, ts
        ));
    }

    #[test]
    fn rejects_wrong_audience() {
        let sk = SigningKey::generate(&mut rand_core::OsRng);
        let node_id = hex::encode(sk.verifying_key().as_bytes());
        let ts = 1_700_000_000_000;
        let items = vec![id_a()];
        let sig_b64 = sign(&sk, "announce", &node_id, ts, TICKET, AUD, &items);
        // A request captured for one registry replayed against another fails.
        assert!(!verify(
            "announce",
            &node_id,
            ts,
            TICKET,
            "https://evil.example.com",
            &items,
            &sig_b64,
            ts
        ));
        // A trailing slash on the audience is canonicalized away (still verifies).
        assert!(verify(
            "announce",
            &node_id,
            ts,
            TICKET,
            "https://atlas.noemaai.com/",
            &items,
            &sig_b64,
            ts
        ));
    }

    #[test]
    fn accepts_canonically_equivalent_audience() {
        let sk = SigningKey::generate(&mut rand_core::OsRng);
        let node_id = hex::encode(sk.verifying_key().as_bytes());
        let ts = 1_700_000_000_000;
        let items = vec![id_a()];
        // Sign with a mixed-case host, an explicit default :443, and a trailing
        // slash — all semantically equal to the plain canonical form a registry
        // verifies against, so a legitimate share must not 401.
        let sig_b64 = sign(
            &sk,
            "announce",
            &node_id,
            ts,
            TICKET,
            "https://Atlas.NoemaAI.com:443/",
            &items,
        );
        assert!(verify(
            "announce",
            &node_id,
            ts,
            TICKET,
            "https://atlas.noemaai.com",
            &items,
            &sig_b64,
            ts
        ));
        // Scheme is still significant — http vs https is a real audience.
        assert!(!verify(
            "announce",
            &node_id,
            ts,
            TICKET,
            "http://atlas.noemaai.com",
            &items,
            &sig_b64,
            ts
        ));
    }

    #[test]
    fn rejects_extreme_old_or_future_ts_without_panic() {
        let sk = SigningKey::generate(&mut rand_core::OsRng);
        let node_id = hex::encode(sk.verifying_key().as_bytes());
        let now = 1_700_000_000_000;
        let items = vec![id_a()];

        // i64::MIN ts must reject (not overflow/panic on the saturating math).
        let sig_min = sign(&sk, "announce", &node_id, i64::MIN, TICKET, AUD, &items);
        assert!(!verify(
            "announce",
            &node_id,
            i64::MIN,
            TICKET,
            AUD,
            &items,
            &sig_min,
            now
        ));
        // i64::MAX (far future) must reject without panic.
        let sig_max = sign(&sk, "announce", &node_id, i64::MAX, TICKET, AUD, &items);
        assert!(!verify(
            "announce",
            &node_id,
            i64::MAX,
            TICKET,
            AUD,
            &items,
            &sig_max,
            now
        ));
        // A modest future ts within the skew is accepted; beyond it is rejected.
        let near = now + FUTURE_SKEW_MS - 1;
        let sig_near = sign(&sk, "announce", &node_id, near, TICKET, AUD, &items);
        assert!(verify(
            "announce", &node_id, near, TICKET, AUD, &items, &sig_near, now
        ));
        let far = now + FUTURE_SKEW_MS + 1;
        let sig_far = sign(&sk, "announce", &node_id, far, TICKET, AUD, &items);
        assert!(!verify(
            "announce", &node_id, far, TICKET, AUD, &items, &sig_far, now
        ));
    }

    #[test]
    fn rejects_non_canonical_id() {
        // Non-hex / wrong-length / uppercase ids never form a payload to sign...
        assert!(
            canonical_payload("announce", "node", 1, TICKET, AUD, &["not-hex".to_string()])
                .is_none()
        );
        assert!(
            canonical_payload("announce", "node", 1, TICKET, AUD, &["abc".to_string()]).is_none()
        );
        let upper = "AB".repeat(32);
        assert!(canonical_payload("announce", "node", 1, TICKET, AUD, &[upper]).is_none());
        assert!(is_canonical_id(&id_a()));
        assert!(!is_canonical_id("xyz"));

        // ...and verify rejects them even when a (bogus) signature is supplied.
        let sk = SigningKey::generate(&mut rand_core::OsRng);
        let node_id = hex::encode(sk.verifying_key().as_bytes());
        let ts = 1_700_000_000_000;
        let bad = vec!["zz".repeat(32)]; // 64 chars but not hex
        assert!(!verify(
            "announce", &node_id, ts, TICKET, AUD, &bad, "AAAA", ts
        ));
    }
}
