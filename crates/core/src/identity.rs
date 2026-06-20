use rand_core::{OsRng, RngCore};

/// A stable random device id (hex), generated once per install.
pub fn new_device_id() -> String {
    let mut b = [0u8; 16];
    OsRng.fill_bytes(&mut b);
    hex::encode(b)
}

/// A friendly, easy-to-type random group code (no ambiguous chars), e.g.
/// `K7QP-M2RX-9TJD`. Share it with your other devices to link them.
pub fn new_group_code() -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789"; // no I,O,0,1
    let mut b = [0u8; 12];
    OsRng.fill_bytes(&mut b);
    let mut s = String::new();
    for (i, byte) in b.iter().enumerate() {
        if i > 0 && i % 4 == 0 {
            s.push('-');
        }
        s.push(ALPHABET[(*byte as usize) % ALPHABET.len()] as char);
    }
    s
}

/// Derive the tracker group id (a capability) from a user's group code. The raw
/// code is never sent to the tracker. Returns `None` for an empty code.
pub fn group_id(code: &str) -> Option<String> {
    let c = code.trim();
    if c.is_empty() {
        return None;
    }
    // Normalize so the same code matches however it's typed (case, spaces,
    // dashes): keep only alphanumerics, uppercased.
    let norm: String = c
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric())
        .collect::<String>()
        .to_uppercase();
    if norm.is_empty() {
        return None;
    }
    Some(hex::encode(blake3::hash(norm.as_bytes()).as_bytes()))
}

/// Best-effort default device name (editable by the user).
pub fn default_device_name() -> String {
    for var in ["HOST", "HOSTNAME", "COMPUTERNAME"] {
        if let Ok(v) = std::env::var(var) {
            let v = v.trim().trim_end_matches(".local").to_string();
            if !v.is_empty() {
                return v;
            }
        }
    }
    "My device".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn group_id_is_case_and_space_insensitive() {
        let a = group_id("K7QP-M2RX-9TJD").unwrap();
        let b = group_id("  k7qp m2rx 9tjd  ").unwrap();
        assert_eq!(a, b);
        assert_eq!(a.len(), 64);
        assert!(group_id("").is_none());
        assert!(group_id("   ").is_none());
    }

    #[test]
    fn codes_and_ids_are_distinct() {
        assert_ne!(new_device_id(), new_device_id());
        assert_ne!(new_group_code(), new_group_code());
        assert!(new_group_code().contains('-'));
    }
}
