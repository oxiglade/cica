use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::audit;
use crate::config::Paths;

/// How long a pairing code remains valid
const CODE_TTL: Duration = Duration::from_secs(60 * 60); // 1 hour

/// Characters used for code generation (no ambiguous chars: 0/O, 1/I)
const CODE_ALPHABET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
const CODE_LENGTH: usize = 8;

/// A pending pairing request
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingRequest {
    pub code: String,
    pub channel: String,
    pub user_id: String,
    pub username: Option<String>,
    pub display_name: Option<String>,
    pub created_at: u64, // Unix timestamp
}

/// Per-user profile data
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct UserProfile {
    pub name: Option<String>,
    pub pronouns: Option<String>,
    pub location: Option<String>,
    pub timezone: Option<String>,
    pub notes: Option<String>,
    pub onboarding_complete: bool,
}

/// Storage for all pairing data
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PairingStore {
    #[serde(skip)]
    path: PathBuf,
    pub pending: Vec<PendingRequest>,
    pub approved: HashMap<String, Vec<String>>, // channel -> [user_ids]
    #[serde(default)]
    pub sessions: HashMap<String, String>, // "channel:user_id" -> session_id (UUID)
    #[serde(default)]
    pub user_profiles: HashMap<String, UserProfile>, // "channel:user_id" -> profile
}

impl PairingStore {
    /// Load pairing store from disk
    pub fn load(paths: &Paths) -> Result<Self> {
        Self::load_from(&paths.pairing_file)
    }

    fn load_from(path: &Path) -> Result<Self> {
        let path = path.to_path_buf();

        if !path.exists() {
            return Ok(Self {
                path,
                ..Self::default()
            });
        }

        let content = std::fs::read_to_string(&path)
            .with_context(|| format!("Failed to read pairing file: {:?}", path))?;

        let mut store: Self = serde_json::from_str(&content)
            .with_context(|| format!("Failed to parse pairing file: {:?}", path))?;
        for ids in store.approved.values_mut() {
            let mut seen = HashSet::new();
            ids.retain(|id| !id.trim().is_empty() && seen.insert(id.clone()));
        }
        store.path = path;

        Ok(store)
    }

    /// Save pairing store to disk
    pub fn save(&self) -> Result<()> {
        let content = serde_json::to_string_pretty(self)?;
        crate::atomic::write(&self.path, content.as_bytes())?;

        Ok(())
    }

    /// Re-read the file into `self`, keeping `self.path`. Fails closed on a parse error.
    pub fn reload(&mut self) -> Result<()> {
        let fresh = Self::load_from(&self.path)?;
        self.pending = fresh.pending;
        self.approved = fresh.approved;
        self.sessions = fresh.sessions;
        self.user_profiles = fresh.user_profiles;
        Ok(())
    }

    /// reload → f → atomic save. The only way a mutation reaches disk.
    pub fn modify<T>(&mut self, f: impl FnOnce(&mut Self) -> Result<T>) -> Result<T> {
        self.reload()?;
        let output = f(self)?;
        self.save()?;
        Ok(output)
    }

    pub fn set_session_if(&mut self, key: &str, expected: Option<&str>, new: &str) -> bool {
        if self.sessions.get(key).map(String::as_str) != expected {
            return false;
        }
        self.sessions.insert(key.to_string(), new.to_string());
        true
    }

    /// Remove expired pending requests
    pub fn prune_expired(&mut self) {
        let now = now_timestamp();
        let ttl_secs = CODE_TTL.as_secs();

        self.pending
            .retain(|req| now.saturating_sub(req.created_at) < ttl_secs);
    }

    /// Check if a user is approved for a channel
    pub fn is_approved(&self, channel: &str, user_id: &str) -> bool {
        if user_id.trim().is_empty() {
            return false;
        }
        self.approved
            .get(channel)
            .map(|ids| ids.contains(&user_id.to_string()))
            .unwrap_or(false)
    }

    /// Get or create a pending request for a user
    /// Returns (code, is_new)
    pub fn get_or_create_pending(
        &mut self,
        channel: &str,
        user_id: &str,
        username: Option<String>,
        display_name: Option<String>,
    ) -> Result<(String, bool)> {
        anyhow::ensure!(!user_id.trim().is_empty(), "empty pairing user id");
        self.prune_expired();

        if let Some(existing) = self
            .pending
            .iter()
            .find(|r| r.channel == channel && r.user_id == user_id)
        {
            return Ok((existing.code.clone(), false));
        }

        let code = generate_unique_code(&self.pending)?;

        let request = PendingRequest {
            code: code.clone(),
            channel: channel.to_string(),
            user_id: user_id.to_string(),
            username,
            display_name,
            created_at: now_timestamp(),
        };

        self.pending.push(request);

        audit::log_event(
            "pairing_requested",
            Some(channel),
            Some(user_id),
            Some(&format!("{{\"code\":\"{}\"}}", code)),
        );

        Ok((code, true))
    }

    /// Approve a pending request by code
    /// Returns the approved request details on success
    pub fn approve(&mut self, code: &str) -> Result<PendingRequest> {
        self.prune_expired();

        let code_upper = code.to_uppercase();

        let idx = self
            .pending
            .iter()
            .position(|r| r.code == code_upper)
            .ok_or_else(|| anyhow!("No pending request found for code: {}", code))?;

        anyhow::ensure!(
            !self.pending[idx].user_id.trim().is_empty(),
            "empty pairing user id"
        );
        let request = self.pending.remove(idx);

        let ids = self.approved.entry(request.channel.clone()).or_default();
        if ids.iter().any(|id| id == &request.user_id) {
            return Ok(request);
        }
        ids.push(request.user_id.clone());

        let detail = format!(
            "{{\"code\":\"{}\",\"username\":{}}}",
            code_upper,
            request
                .username
                .as_deref()
                .map(|u| format!("\"{}\"", u))
                .unwrap_or_else(|| "null".to_string()),
        );
        audit::log_event(
            "user_approved",
            Some(&request.channel),
            Some(&request.user_id),
            Some(&detail),
        );

        Ok(request)
    }

    /// Automatically approve a user without requiring a pairing code
    pub fn auto_approve(
        &mut self,
        channel: &str,
        user_id: &str,
        username: Option<String>,
        _display_name: Option<String>,
    ) -> Result<()> {
        anyhow::ensure!(!user_id.trim().is_empty(), "empty pairing user id");
        let ids = self.approved.entry(channel.to_string()).or_default();
        if ids.iter().any(|id| id == user_id) {
            return Ok(());
        }
        ids.push(user_id.to_string());

        let detail = username
            .as_deref()
            .map(|u| format!("{{\"username\":\"{}\"}}", u))
            .unwrap_or_else(|| "{}".to_string());
        audit::log_event(
            "user_auto_approved",
            Some(channel),
            Some(user_id),
            Some(&detail),
        );

        Ok(())
    }
}

/// Generate a unique pairing code
fn generate_unique_code(existing: &[PendingRequest]) -> Result<String> {
    use std::collections::HashSet;

    let existing_codes: HashSet<&str> = existing.iter().map(|r| r.code.as_str()).collect();

    for _ in 0..100 {
        let code = generate_code();
        if !existing_codes.contains(code.as_str()) {
            return Ok(code);
        }
    }

    Err(anyhow!("Failed to generate unique code after 100 attempts"))
}

/// Generate a random code
fn generate_code() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};

    // Simple randomness from system time + process id
    let seed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos() as u64
        ^ std::process::id() as u64;

    let mut rng = SimpleRng::new(seed);

    (0..CODE_LENGTH)
        .map(|_| {
            let idx = rng.next() as usize % CODE_ALPHABET.len();
            CODE_ALPHABET[idx] as char
        })
        .collect()
}

/// Simple PRNG for code generation (no external deps)
struct SimpleRng(u64);

impl SimpleRng {
    fn new(seed: u64) -> Self {
        Self(seed)
    }

    fn next(&mut self) -> u64 {
        // xorshift64
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

fn now_timestamp() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modify_fails_closed() {
        let (_temp, paths) = crate::config::test_paths();
        let mut store = PairingStore::load(&paths).unwrap();
        store.save().unwrap();
        std::fs::write(&paths.pairing_file, b"{\"pending\": ").unwrap();

        assert!(
            store
                .modify(|store| {
                    store.sessions.insert("key".into(), "value".into());
                    Ok(())
                })
                .is_err()
        );
        assert!(store.sessions.is_empty());
        assert_eq!(
            std::fs::read(&paths.pairing_file).unwrap(),
            b"{\"pending\": "
        );
    }

    #[test]
    fn reload_sees_out_of_process_approval() {
        let (_temp, paths) = crate::config::test_paths();
        let mut first = PairingStore::load(&paths).unwrap();
        first
            .modify(|store| store.get_or_create_pending("telegram", "1", None, None))
            .unwrap();
        let code = first.pending[0].code.clone();
        let mut second = PairingStore::load(&paths).unwrap();
        second.modify(|store| store.approve(&code)).unwrap();

        assert!(!first.is_approved("telegram", "1"));
        first.reload().unwrap();
        assert!(first.is_approved("telegram", "1"));
    }

    #[test]
    fn empty_user_id_is_never_approved_or_matched() {
        let mut store = PairingStore::default();
        for id in ["", "   "] {
            assert!(store.auto_approve("linear", id, None, None).is_err());
            assert!(
                store
                    .get_or_create_pending("linear", id, None, None)
                    .is_err()
            );
            store.approved.insert("linear".into(), vec![id.into()]);
            assert!(!store.is_approved("linear", id));
            store.pending.push(PendingRequest {
                code: "INVALID".into(),
                channel: "linear".into(),
                user_id: id.into(),
                username: None,
                display_name: None,
                created_at: now_timestamp(),
            });
            assert!(store.approve("INVALID").is_err());
        }
    }

    #[test]
    fn auto_approve_is_idempotent() {
        let (_temp, paths) = crate::config::test_paths();
        let mut store = PairingStore::load(&paths).unwrap();
        store.auto_approve("telegram", "1", None, None).unwrap();
        store.auto_approve("telegram", "1", None, None).unwrap();

        assert_eq!(store.approved["telegram"], ["1"]);
    }

    #[test]
    fn load_dedups_approved() {
        let (_temp, paths) = crate::config::test_paths();
        std::fs::write(
            &paths.pairing_file,
            r#"{"pending":[],"approved":{"telegram":["1","1","2"]}}"#,
        )
        .unwrap();

        let store = PairingStore::load(&paths).unwrap();
        assert_eq!(store.approved["telegram"], ["1", "2"]);
    }
}
