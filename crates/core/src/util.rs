use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

/// Read an HTTP response body, refusing more than `max` bytes. External servers
/// (registry, tracker, Hugging Face, update endpoint) are untrusted, so a body
/// read must be bounded or a hostile/compromised server could stream an unbounded
/// response and exhaust client memory.
#[cfg(feature = "http")]
pub async fn read_body_capped(
    mut resp: reqwest::Response,
    max: usize,
) -> crate::error::Result<Vec<u8>> {
    let mut buf = Vec::new();
    while let Some(chunk) = resp
        .chunk()
        .await
        .map_err(|e| crate::error::Error::other(format!("response body: {e}")))?
    {
        if buf.len() + chunk.len() > max {
            return Err(crate::error::Error::other(
                "response body exceeds maximum allowed size",
            ));
        }
        buf.extend_from_slice(&chunk);
    }
    Ok(buf)
}

/// Current UTC time as an RFC-3339 string, e.g. `2026-06-16T00:00:00Z`.
pub fn now_rfc3339() -> String {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_string())
}

/// Current UNIX time in milliseconds.
pub fn now_unix_millis() -> i64 {
    (OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000) as i64
}

/// Turn an arbitrary model name into a filesystem-safe slug.
pub fn slugify(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut last_dash = false;
    for ch in input.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            last_dash = false;
        } else if matches!(ch, '.' | '_' | '-') {
            out.push(ch);
            last_dash = false;
        } else if !last_dash {
            out.push('-');
            last_dash = true;
        }
    }
    let trimmed = out.trim_matches('-').to_string();
    if trimmed.is_empty() {
        "model".to_string()
    } else {
        trimmed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugify_basic() {
        assert_eq!(slugify("Qwen3 8B Instruct GGUF"), "qwen3-8b-instruct-gguf");
        assert_eq!(slugify("a/b\\c"), "a-b-c");
        assert_eq!(slugify("model.q4_k_m"), "model.q4_k_m");
        assert_eq!(slugify("   "), "model");
    }

    #[test]
    fn timestamps_are_sane() {
        assert!(now_rfc3339().starts_with("20"));
        assert!(now_unix_millis() > 1_700_000_000_000);
    }
}
