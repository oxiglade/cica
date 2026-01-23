//! Onboarding flow for new users
//!
//! Two phases:
//! 1. Agent identity (per-user) → writes users/{channel}_{user_id}/IDENTITY.md
//! 2. User profile (per-user) → writes users/{channel}_{user_id}/USER.md
//!
//! Per-user files (in users/{channel}_{user_id}/):
//! - IDENTITY.md - who the assistant is for this user
//! - USER.md - info about this user
//! - memories/ - saved memories about conversations
//!
//! Shared files (configured by owner):
//! - PERSONA.md - general behavior guidelines
//! - SKILLS.md - capabilities

use anyhow::Result;
use std::path::PathBuf;
use tracing::warn;

use crate::config;
use crate::memory::{MemoryIndex, memories_dir};
use crate::skills;

/// Onboarding phase
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// Need to configure agent identity (first user only)
    Identity,
    /// Need to learn about this specific user
    User,
    /// Onboarding complete for this user
    Complete,
}

/// Get the user directory path for a specific user
pub fn user_dir(channel: &str, user_id: &str) -> Result<PathBuf> {
    let dir = config::paths()?
        .base
        .join("users")
        .join(format!("{}_{}", channel, user_id));
    Ok(dir)
}

/// Get the path to a user's IDENTITY.md
pub fn identity_path_for_user(channel: &str, user_id: &str) -> Result<PathBuf> {
    Ok(user_dir(channel, user_id)?.join("IDENTITY.md"))
}

/// Get the path to a user's USER.md
pub fn user_path_for_user(channel: &str, user_id: &str) -> Result<PathBuf> {
    Ok(user_dir(channel, user_id)?.join("USER.md"))
}

/// Check if a user's identity is configured
pub fn is_identity_configured_for_user(channel: &str, user_id: &str) -> Result<bool> {
    Ok(identity_path_for_user(channel, user_id)?.exists())
}

/// Check if a user's profile is configured
pub fn is_user_configured_for_user(channel: &str, user_id: &str) -> Result<bool> {
    Ok(user_path_for_user(channel, user_id)?.exists())
}

/// Get current onboarding phase for a specific user
pub fn current_phase_for_user(channel: &str, user_id: &str) -> Result<Phase> {
    // First check if this user's identity is set up
    if !identity_path_for_user(channel, user_id)?.exists() {
        return Ok(Phase::Identity);
    }

    // Then check if this user's profile is complete
    if !user_path_for_user(channel, user_id)?.exists() {
        return Ok(Phase::User);
    }

    Ok(Phase::Complete)
}

/// Check if onboarding is complete for a user
pub fn is_complete_for_user(channel: &str, user_id: &str) -> Result<bool> {
    Ok(current_phase_for_user(channel, user_id)? == Phase::Complete)
}

/// Get the system prompt for a specific user's onboarding phase
pub fn system_prompt_for_user(channel: &str, user_id: &str) -> Result<String> {
    match current_phase_for_user(channel, user_id)? {
        Phase::Identity => identity_system_prompt(channel, user_id),
        Phase::User => user_system_prompt(channel, user_id),
        Phase::Complete => Ok(String::new()),
    }
}

/// System prompt for identity phase (per-user)
fn identity_system_prompt(channel: &str, user_id: &str) -> Result<String> {
    let path = identity_path_for_user(channel, user_id)?;

    // Ensure user directory exists
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    Ok(format!(
        r#"You are a new AI assistant being set up by a user. You need to learn your identity before you can help them.

On the FIRST message, introduce yourself briefly and ask ALL THREE questions at once:
1. What's my name?
2. What's my vibe? (personality/energy)
3. What's my spirit animal?

Keep it short and friendly. Don't be overly excited or use emojis.

Example first response:
"Hey! I'm your new assistant, but I need an identity first. Can you tell me:
1. What should my name be?
2. What's my vibe?
3. What's my spirit animal?"

After they answer, if any answer is missing or unclear, ask for clarification. Once you have all three answers, write them to: {}

Use this exact format:
```
# IDENTITY.md - Agent Identity

- Name: [name]
- Vibe: [short description]
- Spirit Animal: [animal]
```

After writing the file, tell them "Now tell me about yourself - the more I know about you the better I'll be able to help, so don't be shy!"

IMPORTANT: Do NOT write the file until you have all three answers."#,
        path.display()
    ))
}

/// System prompt for user profile phase (per-user)
fn user_system_prompt(channel: &str, user_id: &str) -> Result<String> {
    let identity_path = identity_path_for_user(channel, user_id)?;
    let user_path = user_path_for_user(channel, user_id)?;
    let identity = std::fs::read_to_string(&identity_path).unwrap_or_default();

    Ok(format!(
        r#"You are an AI assistant with this identity:

{}

You just finished setting up your identity. Now ask the user to tell you about themselves.

Keep it casual and short. Example:
"Now tell me about yourself - the more I know about you the better I'll be able to help, so don't be shy!"

When they respond, write their info to: {}

Use this format:
```
# USER.md - User Profile

- Name: [their name]
- [any other info they shared, one item per line]
```

After writing the file, greet them by name and ask how you can help.

IMPORTANT:
- Name is required, but accept whatever else they share
- Do NOT ask follow-up questions about their profile
- After saving, just move on to helping them"#,
        identity,
        user_path.display()
    ))
}

/// Load identity content for a specific user
pub fn load_identity_for_user(channel: &str, user_id: &str) -> Result<Option<String>> {
    let path = identity_path_for_user(channel, user_id)?;
    if !path.exists() {
        return Ok(None);
    }
    Ok(Some(std::fs::read_to_string(&path)?))
}

/// Load user profile content for a specific user
pub fn load_user_for_user(channel: &str, user_id: &str) -> Result<Option<String>> {
    let path = user_path_for_user(channel, user_id)?;
    if !path.exists() {
        return Ok(None);
    }
    Ok(Some(std::fs::read_to_string(&path)?))
}

/// Load persona content
pub fn load_persona() -> Result<Option<String>> {
    let path = config::paths()?.base.join("PERSONA.md");
    if !path.exists() {
        return Ok(None);
    }
    Ok(Some(std::fs::read_to_string(&path)?))
}

/// Build system prompt with all context for a specific user
///
/// If `user_message` is provided, it will be used to search for relevant memories
/// to include in the context.
pub fn build_context_prompt_for_user(
    channel_display: Option<&str>,
    channel_id: Option<&str>,
    user_id: Option<&str>,
    user_message: Option<&str>,
) -> Result<String> {
    let paths = config::paths()?;
    let mut lines = Vec::new();

    // Load per-user identity
    let identity = if let (Some(ch), Some(uid)) = (channel_id, user_id) {
        load_identity_for_user(ch, uid)?
    } else {
        None
    };

    let assistant_name = identity
        .as_ref()
        .and_then(|content| {
            content
                .lines()
                .find(|l| l.starts_with("- Name:"))
                .map(|l| l.trim_start_matches("- Name:").trim().to_string())
        })
        .unwrap_or_else(|| "Cica".to_string());

    // Load per-user profile
    let user_content = if let (Some(ch), Some(uid)) = (channel_id, user_id) {
        load_user_for_user(ch, uid)?
    } else {
        None
    };

    // Core identity with channel info
    let channel_info = channel_display
        .map(|c| format!(" (via {})", c))
        .unwrap_or_default();
    lines.push(format!(
        "You are {}, a personal AI assistant. You are chatting with your user via a messaging app{}.",
        assistant_name, channel_info
    ));
    lines.push(String::new());

    // Capabilities section
    lines.push("## Capabilities".to_string());
    lines.push("You can:".to_string());
    lines.push("- Have conversations and answer questions".to_string());
    lines.push("- Help with writing, brainstorming, and thinking through problems".to_string());
    lines.push("- Read and write files in your workspace".to_string());
    lines.push("- Run shell commands when needed".to_string());
    lines.push("- Search the web for current information".to_string());
    lines.push(String::new());

    // Channel-specific guidance
    if let Some(channel_name) = channel_display {
        lines.push("## Messaging Channel".to_string());
        lines.push(format!(
            "You are currently communicating via {}.",
            channel_name
        ));
        lines.push(
            "IMPORTANT: Never send streaming/partial replies to external messaging surfaces."
                .to_string(),
        );
        lines.push(String::new());

        // Channel-specific formatting
        lines.push("### Text Formatting".to_string());
        match channel_name.to_lowercase().as_str() {
            "signal" => {
                lines.push(
                    "Do NOT use any text formatting (no markdown, no asterisks, no underscores)."
                        .to_string(),
                );
                lines.push(
                    "Signal requires special APIs for formatting that aren't available here."
                        .to_string(),
                );
                lines.push("Just use plain text.".to_string());
            }
            "telegram" => {
                lines.push("Telegram supports standard markdown:".to_string());
                lines.push("- **bold** or __bold__".to_string());
                lines.push("- *italic* or _italic_".to_string());
                lines.push("- ~strikethrough~".to_string());
                lines.push("- `monospace` and ```code blocks```".to_string());
                lines.push("- [links](url)".to_string());
            }
            _ => {
                lines.push("Use plain text formatting.".to_string());
            }
        }
        lines.push(String::new());
    }

    // Skills section
    lines.push("## Skills".to_string());
    lines.push(
        "Skills extend your capabilities. They live in the skills/ folder of your workspace."
            .to_string(),
    );
    lines.push(String::new());

    // Discover and list available skills
    match skills::discover_skills() {
        Ok(discovered) if !discovered.is_empty() => {
            lines.push("### Available Skills".to_string());
            lines.push("To use a skill, read its SKILL.md file at the location shown, then follow its instructions.".to_string());
            lines.push(String::new());
            lines.push(skills::format_skills_xml(&discovered));
            lines.push(String::new());
        }
        Ok(_) => {
            lines.push("No skills are currently installed.".to_string());
            lines.push(String::new());
        }
        Err(e) => {
            warn!("Failed to discover skills: {}", e);
        }
    }

    lines.push("### Creating Skills".to_string());
    lines.push("When the user asks about something you can't do directly (like accessing email, calendar, APIs, etc.), offer to create a skill for it.".to_string());
    lines.push(String::new());
    lines.push("Each skill is a folder in skills/ containing:".to_string());
    lines.push("1. **SKILL.md** (required) - Instructions with YAML frontmatter:".to_string());
    lines.push("   ```".to_string());
    lines.push("   ---".to_string());
    lines.push("   name: my-skill".to_string());
    lines.push("   description: What this skill does".to_string());
    lines.push("   ---".to_string());
    lines.push("   # My Skill".to_string());
    lines.push("   Instructions for using this skill...".to_string());
    lines.push("   ```".to_string());
    lines.push("2. **index.ts** - The implementation (TypeScript/Bun preferred)".to_string());
    lines.push("3. **config.json** (optional) - Configuration/secrets".to_string());
    lines.push(String::new());
    lines.push(format!(
        "Use the bundled Bun at: {}",
        paths.bun_dir.join("bun").display()
    ));
    lines.push(String::new());

    // Workspace
    lines.push("## Workspace".to_string());
    lines.push(format!(
        "Your workspace directory is: {}",
        paths.base.display()
    ));
    lines.push(String::new());

    // Project context from files
    lines.push("# Project Context".to_string());
    lines.push(String::new());

    if let Some(content) = identity {
        lines.push("## IDENTITY.md".to_string());
        lines.push(content);
        lines.push(String::new());
    }

    if let Some(content) = user_content {
        lines.push("## USER.md".to_string());
        lines.push(content);
        lines.push(String::new());
    }

    if let Some(content) = load_persona()? {
        lines.push("## PERSONA.md".to_string());
        lines.push(content);
        lines.push(String::new());
    }

    // Memory system
    if let (Some(ch), Some(uid)) = (channel_id, user_id) {
        let mem_dir = memories_dir(ch, uid)?;

        // Add memory guidance
        lines.push("## Memories".to_string());
        lines.push(format!(
            "You can save important information about conversations to your memory system at: {}",
            mem_dir.display()
        ));
        lines.push(String::new());
        lines.push("When you learn something important about the user (preferences, projects they're working on, significant life events, technical details they share), you can save it as a memory file.".to_string());
        lines.push(String::new());
        lines.push("To save a memory:".to_string());
        lines.push("1. Ask the user if they'd like you to remember this".to_string());
        lines.push("2. If they agree, write a markdown file to the memories directory".to_string());
        lines.push(
            "3. Use a descriptive filename like `project-foo.md` or `preferences.md`".to_string(),
        );
        lines.push("4. Format the content clearly with headers and bullet points".to_string());
        lines.push(String::new());
        lines.push("DO ask before saving memories. DON'T save trivial information.".to_string());
        lines.push(String::new());

        // Search for relevant memories if we have a user message
        if let Some(query) = user_message {
            match MemoryIndex::open() {
                Ok(index) => {
                    // First ensure memories are indexed
                    // Note: We don't call index_user_memories here because it's mutable
                    // That should be done at startup or when files change

                    match index.search(ch, uid, query, 3) {
                        Ok(results) if !results.is_empty() => {
                            lines.push("### Relevant Memories".to_string());
                            lines.push(
                                "The following memories may be relevant to this conversation:"
                                    .to_string(),
                            );
                            lines.push(String::new());

                            for result in results {
                                if result.score > 0.3 {
                                    // Only include reasonably relevant results
                                    lines.push(format!("**From {}:**", result.path));
                                    lines.push(result.chunk);
                                    lines.push(String::new());
                                }
                            }
                        }
                        Ok(_) => {
                            // No relevant memories found, that's fine
                        }
                        Err(e) => {
                            warn!("Failed to search memories: {}", e);
                        }
                    }
                }
                Err(e) => {
                    warn!("Failed to open memory index: {}", e);
                }
            }
        }
    }

    Ok(lines.join("\n"))
}
