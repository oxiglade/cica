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
    pub pending: Vec<PendingRequest>,
    pub approved: HashMap<String, Vec<String>>, // channel -> [user_ids]
    #[serde(default)]
    pub sessions: HashMap<String, String>, // "channel:user_id" -> session_id (UUID)
    #[serde(default)]
    pub user_profiles: HashMap<String, UserProfile>, // "channel:user_id" -> profile
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

    /// Automatically approve a user without requiring a pairing code
    pub fn auto_approve(
        &mut self,
        channel: &str,
        user_id: &str,
        _username: Option<String>,
        _display_name: Option<String>,
    ) -> Result<()> {
        self.approved
            .entry(channel.to_string())
            .or_default()
            .push(user_id.to_string());
        self.save()
    }

    /// List all pending requests
    #[allow(dead_code)]
    pub fn list_pending(&mut self) -> Vec<&PendingRequest> {
        self.prune_expired();
        self.pending.iter().collect()
    }

    /// Get or create a session ID for a user
    #[allow(dead_code)]
    pub fn get_or_create_session(&mut self, channel: &str, user_id: &str) -> Result<String> {
        let key = format!("{}:{}", channel, user_id);

        if let Some(session_id) = self.sessions.get(&key) {
            return Ok(session_id.clone());
        }

        // Generate a new UUID for the session
        let session_id = generate_uuid();
        self.sessions.insert(key, session_id.clone());
        self.save()?;

        Ok(session_id)
    }

    /// Reset a user's session (start fresh conversation)
    #[allow(dead_code)]
    pub fn reset_session(&mut self, channel: &str, user_id: &str) -> Result<()> {
        let key = format!("{}:{}", channel, user_id);
        self.sessions.remove(&key);
        self.save()
    }

    /// Get a user's profile
    #[allow(dead_code)]
    pub fn get_user_profile(&self, channel: &str, user_id: &str) -> Option<&UserProfile> {
        let key = format!("{}:{}", channel, user_id);
        self.user_profiles.get(&key)
    }

    /// Get or create a user's profile
    #[allow(dead_code)]
    pub fn get_or_create_user_profile(&mut self, channel: &str, user_id: &str) -> &mut UserProfile {
        let key = format!("{}:{}", channel, user_id);
        self.user_profiles.entry(key).or_default()
    }

    /// Update a user's profile
    #[allow(dead_code)]
    pub fn update_user_profile(
        &mut self,
        channel: &str,
        user_id: &str,
        profile: UserProfile,
    ) -> Result<()> {
        let key = format!("{}:{}", channel, user_id);
        self.user_profiles.insert(key, profile);
        self.save()
    }

    /// Check if a user's onboarding is complete
    #[allow(dead_code)]
    pub fn is_user_onboarded(&self, channel: &str, user_id: &str) -> bool {
        self.get_user_profile(channel, user_id)
            .map(|p| p.onboarding_complete)
            .unwrap_or(false)
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

/// Generate a UUID v4 (random)
#[allow(dead_code)]
fn generate_uuid() -> String {
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_nanos() as u64;

    let mut rng = SimpleRng::new(now ^ std::process::id() as u64);

    let bytes: Vec<u8> = (0..16).map(|_| rng.next() as u8).collect();

    // Format as UUID with version 4 and variant bits
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-4{:01x}{:02x}-{:01x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        bytes[0],
        bytes[1],
        bytes[2],
        bytes[3],
        bytes[4],
        bytes[5],
        bytes[6] & 0x0f,
        bytes[7],
        (bytes[8] & 0x3f) | 0x80,
        bytes[9],
        bytes[10],
        bytes[11],
        bytes[12],
        bytes[13],
        bytes[14],
        bytes[15]
    )
}
