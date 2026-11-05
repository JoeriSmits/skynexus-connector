//! Where the connector credential lives: the OS keychain (Windows Credential
//! Manager, macOS Keychain) through the `keyring` crate, or memory for dev
//! and tests. Only the connector credential is ever stored, never a password.

use std::sync::Mutex;

use crate::api::Credential;

pub const KEYCHAIN_SERVICE: &str = "SkyNexus Connector";

pub trait CredentialStore: Send + Sync {
    fn load(&self) -> Result<Option<Credential>, String>;
    fn save(&self, c: &Credential) -> Result<(), String>;
    fn clear(&self) -> Result<(), String>;
}

fn encode(c: &Credential) -> String {
    serde_json::json!({ "credential": c.credential, "device_id": c.device_id }).to_string()
}

fn decode(raw: &str) -> Option<Credential> {
    serde_json::from_str::<Credential>(raw)
        .ok()
        .filter(|c| !c.credential.is_empty())
}

/// The OS keychain, one entry per SkyNexus host so a dev server never
/// overwrites the production credential.
pub struct KeyringStore {
    account: String,
}

impl KeyringStore {
    pub fn new(account: impl Into<String>) -> Self {
        KeyringStore {
            account: account.into(),
        }
    }

    fn entry(&self) -> Result<keyring::Entry, String> {
        keyring::Entry::new(KEYCHAIN_SERVICE, &self.account).map_err(|e| e.to_string())
    }
}

impl CredentialStore for KeyringStore {
    fn load(&self) -> Result<Option<Credential>, String> {
        match self.entry()?.get_password() {
            Ok(raw) => Ok(decode(&raw)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(e.to_string()),
        }
    }

    fn save(&self, c: &Credential) -> Result<(), String> {
        self.entry()?.set_password(&encode(c)).map_err(|e| e.to_string())
    }

    fn clear(&self) -> Result<(), String> {
        match self.entry()?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(e.to_string()),
        }
    }
}

/// Memory only: `--ephemeral` and tests.
#[derive(Default)]
pub struct MemoryStore {
    value: Mutex<Option<String>>,
}

impl CredentialStore for MemoryStore {
    fn load(&self) -> Result<Option<Credential>, String> {
        Ok(self.value.lock().unwrap().as_deref().and_then(decode))
    }

    fn save(&self, c: &Credential) -> Result<(), String> {
        *self.value.lock().unwrap() = Some(encode(c));
        Ok(())
    }

    fn clear(&self) -> Result<(), String> {
        *self.value.lock().unwrap() = None;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_store_round_trip() {
        let s = MemoryStore::default();
        assert!(s.load().unwrap().is_none());
        let c = Credential {
            credential: "k".into(),
            device_id: Some("d".into()),
        };
        s.save(&c).unwrap();
        assert_eq!(s.load().unwrap(), Some(c));
        s.clear().unwrap();
        assert!(s.load().unwrap().is_none());
    }

    #[test]
    fn garbage_is_treated_as_signed_out() {
        assert!(decode("not json").is_none());
        assert!(decode(r#"{"credential":""}"#).is_none());
    }
}
