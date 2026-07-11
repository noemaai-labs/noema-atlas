use crate::error::{Error, Result};

/// Abstraction over a secure credential store.
pub trait SecretStore: Send + Sync {
    /// Retrieve a secret, or `None` if not present.
    fn get(&self, service: &str, account: &str) -> Result<Option<String>>;
    /// Store (or overwrite) a secret.
    fn set(&self, service: &str, account: &str, secret: &str) -> Result<()>;
    /// Remove a secret (no-op if absent).
    fn delete(&self, service: &str, account: &str) -> Result<()>;
    /// Whether this store can persist secrets (vs. read-only env resolution).
    fn is_persistent(&self) -> bool;
}

/// Service namespace used for all Noema secrets in the OS keystore.
pub const SERVICE_PREFIX: &str = "noema-atlas";

/// Read-only credential resolution from environment variables; consulted before the persistent store so callers can override per-process.
pub struct EnvStore;

impl EnvStore {
    fn env_key(service: &str) -> String {
        let up: String = service
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() {
                    c.to_ascii_uppercase()
                } else {
                    '_'
                }
            })
            .collect();
        format!("NOEMA_TOKEN_{up}")
    }

    fn aliases(service: &str) -> &'static [&'static str] {
        match service {
            "huggingface" => &["HF_TOKEN", "HUGGING_FACE_HUB_TOKEN", "HUGGINGFACE_TOKEN"],
            _ => &[],
        }
    }
}

impl SecretStore for EnvStore {
    fn get(&self, service: &str, _account: &str) -> Result<Option<String>> {
        if let Ok(v) = std::env::var(Self::env_key(service)) {
            if !v.is_empty() {
                return Ok(Some(v));
            }
        }
        for alias in Self::aliases(service) {
            if let Ok(v) = std::env::var(alias) {
                if !v.is_empty() {
                    return Ok(Some(v));
                }
            }
        }
        Ok(None)
    }

    fn set(&self, _service: &str, _account: &str, _secret: &str) -> Result<()> {
        Err(Error::other(
            "no persistent secret store available; set an environment variable \
             (e.g. HF_TOKEN) or enable the `os-keystore` feature",
        ))
    }

    fn delete(&self, _service: &str, _account: &str) -> Result<()> {
        Ok(())
    }

    fn is_persistent(&self) -> bool {
        false
    }
}

#[cfg(feature = "os-keystore")]
mod keystore {
    use super::*;

    /// OS-native keystore backed by the `keyring` crate. Env variables still
    /// take precedence on read (see [`resolve_token`]).
    pub struct KeyringStore;

    fn full_service(service: &str) -> String {
        format!("{SERVICE_PREFIX}:{service}")
    }

    impl SecretStore for KeyringStore {
        fn get(&self, service: &str, account: &str) -> Result<Option<String>> {
            let entry = keyring::Entry::new(&full_service(service), account)
                .map_err(|e| Error::Key(format!("keystore open: {e}")))?;
            match entry.get_password() {
                Ok(p) => Ok(Some(p)),
                Err(keyring::Error::NoEntry) => Ok(None),
                Err(e) => Err(Error::Key(format!("keystore get: {e}"))),
            }
        }

        fn set(&self, service: &str, account: &str, secret: &str) -> Result<()> {
            let entry = keyring::Entry::new(&full_service(service), account)
                .map_err(|e| Error::Key(format!("keystore open: {e}")))?;
            entry
                .set_password(secret)
                .map_err(|e| Error::Key(format!("keystore set: {e}")))
        }

        fn delete(&self, service: &str, account: &str) -> Result<()> {
            let entry = keyring::Entry::new(&full_service(service), account)
                .map_err(|e| Error::Key(format!("keystore open: {e}")))?;
            match entry.delete_credential() {
                Ok(()) => Ok(()),
                Err(keyring::Error::NoEntry) => Ok(()),
                Err(e) => Err(Error::Key(format!("keystore delete: {e}"))),
            }
        }

        fn is_persistent(&self) -> bool {
            true
        }
    }
}

#[cfg(feature = "os-keystore")]
pub use keystore::KeyringStore;

/// The default secret store for this build: the OS keystore when compiled in,
/// otherwise read-only environment resolution.
pub fn default_store() -> Box<dyn SecretStore> {
    #[cfg(feature = "os-keystore")]
    {
        Box::new(KeyringStore)
    }
    #[cfg(not(feature = "os-keystore"))]
    {
        Box::new(EnvStore)
    }
}

/// Resolve a token for a service, preferring environment variables (so a user
/// can override per-process) and falling back to the persistent store.
pub fn resolve_token(
    store: &dyn SecretStore,
    service: &str,
    account: &str,
) -> Result<Option<String>> {
    if let Some(t) = EnvStore.get(service, account)? {
        return Ok(Some(t));
    }
    store.get(service, account)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_store_reads_alias() {
        // SAFETY: single-threaded test mutating process env.
        std::env::set_var("HF_TOKEN", "secret-abc");
        let got = EnvStore.get("huggingface", "default").unwrap();
        assert_eq!(got.as_deref(), Some("secret-abc"));
        std::env::remove_var("HF_TOKEN");
    }

    #[test]
    fn env_store_namespaced_key() {
        std::env::set_var("NOEMA_TOKEN_MYMIRROR", "tok");
        let got = EnvStore.get("mymirror", "default").unwrap();
        assert_eq!(got.as_deref(), Some("tok"));
        std::env::remove_var("NOEMA_TOKEN_MYMIRROR");
    }

    #[test]
    fn env_store_is_read_only() {
        assert!(EnvStore.set("x", "y", "z").is_err());
        assert!(!EnvStore.is_persistent());
    }
}
