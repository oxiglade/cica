use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::{Duration, SystemTime};

use crate::config;

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

/// Storage for all pairing data
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PairingStore {
    pub pending: Vec<PendingRequest>,
    pub approved: HashMap<String, Vec<String>>, // channel -> [user_ids]
}

impl PairingStore {
    /// Load pairing store from disk
    pub fn load() -> Result<Self> {
        let path = config::paths()?.pairing_file;

        if !path.exists() {
            return Ok(Self::default());
        }

        let content = std::fs::read_to_string(&path)
            .with_context(|| format!("Failed to read pairing file: {:?}", path))?;

        let store: Self = serde_json::from_str(&content)
            .with_context(|| format!("Failed to parse pairing file: {:?}", path))?;

        Ok(store)
    }

    /// Save pairing store to disk
    pub fn save(&self) -> Result<()> {
        let paths = config::paths()?;

        // Ensure directory exists
        if let Some(parent) = paths.pairing_file.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let content = serde_json::to_string_pretty(self)?;
        std::fs::write(&paths.pairing_file, content)?;

        Ok(())
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
        self.prune_expired();

        // Check if already has pending request
        if let Some(existing) = self
            .pending
            .iter()
            .find(|r| r.channel == channel && r.user_id == user_id)
        {
            return Ok((existing.code.clone(), false));
        }

        // Generate new code
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
        self.save()?;

        Ok((code, true))
    }

    /// Approve a pending request by code
    /// Returns the approved request details on success
    pub fn approve(&mut self, code: &str) -> Result<PendingRequest> {
        self.prune_expired();

        let code_upper = code.to_uppercase();

        // Find the pending request
        let idx = self
            .pending
            .iter()
            .position(|r| r.code == code_upper)
            .ok_or_else(|| anyhow!("No pending request found for code: {}", code))?;

        let request = self.pending.remove(idx);

        // Add to approved list
        self.approved
            .entry(request.channel.clone())
            .or_default()
            .push(request.user_id.clone());

        self.save()?;

        Ok(request)
    }

    /// List all pending requests
    pub fn list_pending(&mut self) -> Vec<&PendingRequest> {
        self.prune_expired();
        self.pending.iter().collect()
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

/// Get current unix timestamp
fn now_timestamp() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}
