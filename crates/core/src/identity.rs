use rand_core::{OsRng, RngCore};

/// A stable random device id (hex), generated once per install.
pub fn new_device_id() -> String {
    let mut b = [0u8; 16];
    OsRng.fill_bytes(&mut b);
    hex::encode(b)
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
    fn device_ids_are_distinct() {
        assert_ne!(new_device_id(), new_device_id());
    }
}
