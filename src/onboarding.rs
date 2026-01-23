//! Onboarding flow for new users
//!
//! Two phases:
//! 1. Agent identity → writes IDENTITY.md
//! 2. User profile → writes USER.md (must get name at minimum)

use anyhow::Result;
use std::path::PathBuf;

use crate::config;

/// Onboarding phase
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// Need to configure agent identity
    Identity,
    /// Need to learn about the user
    User,
    /// Onboarding complete
    Complete,
}

/// Get the path to IDENTITY.md
pub fn identity_path() -> Result<PathBuf> {
    Ok(config::paths()?.base.join("IDENTITY.md"))
}

/// Get the path to USER.md
pub fn user_path() -> Result<PathBuf> {
    Ok(config::paths()?.base.join("USER.md"))
}

/// Get current onboarding phase
pub fn current_phase() -> Result<Phase> {
    if !identity_path()?.exists() {
        return Ok(Phase::Identity);
    }
    if !user_path()?.exists() {
        return Ok(Phase::User);
    }
    Ok(Phase::Complete)
}

/// Check if onboarding is complete
pub fn is_complete() -> Result<bool> {
    Ok(current_phase()? == Phase::Complete)
}

/// Get the system prompt for the current onboarding phase
pub fn system_prompt() -> Result<String> {
    match current_phase()? {
        Phase::Identity => identity_system_prompt(),
        Phase::User => user_system_prompt(),
        Phase::Complete => Ok(String::new()),
    }
}

/// System prompt for identity phase
fn identity_system_prompt() -> Result<String> {
    let path = identity_path()?;

    Ok(format!(
        r#"You are a new AI assistant being set up by your owner. You need to learn your identity before you can help them.

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

/// System prompt for user profile phase
fn user_system_prompt() -> Result<String> {
    let identity_path = identity_path()?;
    let user_path = user_path()?;

    let identity = std::fs::read_to_string(&identity_path).unwrap_or_default();

    Ok(format!(
        r#"You are an AI assistant with this identity:

{}

You just asked the user to tell you about themselves. You need to learn their name - that's the only required thing.

When they respond:
1. Extract their name (REQUIRED - this is the only thing you must have)
2. Note any other info they volunteered (pronouns, timezone, location, job, interests, etc.)
3. Immediately write the profile and move on - do NOT ask follow-up questions

Write their profile to: {}

Format (only include fields they actually mentioned):
```
# USER.md - User Profile

- Name: [their name]
- Pronouns: [if they mentioned]
- Location: [if they mentioned]
- Timezone: [if they mentioned]
- Notes: [anything else they shared]
```

After writing, say something brief like "Nice to meet you, [name]! What can I help you with?"

IMPORTANT: Name is the ONLY required field. Do NOT ask for anything else. Just save what they shared and move on."#,
        identity,
        user_path.display()
    ))
}

/// Load identity content
pub fn load_identity() -> Result<Option<String>> {
    let path = identity_path()?;
    if !path.exists() {
        return Ok(None);
    }
    Ok(Some(std::fs::read_to_string(&path)?))
}

/// Load user profile content
pub fn load_user() -> Result<Option<String>> {
    let path = user_path()?;
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

/// Build system prompt with all context for normal operation
pub fn build_context_prompt(channel: Option<&str>) -> Result<String> {
    let paths = config::paths()?;
    let mut lines = Vec::new();

    // Load identity to get assistant name
    let identity = load_identity()?;
    let assistant_name = identity
        .as_ref()
        .and_then(|content| {
            content
                .lines()
                .find(|l| l.starts_with("- Name:"))
                .map(|l| l.trim_start_matches("- Name:").trim().to_string())
        })
        .unwrap_or_else(|| "Cica".to_string());

    // Core identity with channel info
    let channel_info = channel.map(|c| format!(" (via {})", c)).unwrap_or_default();
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
    if let Some(channel_name) = channel {
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
    }

    // Skills section
    lines.push("## Skills".to_string());
    lines.push("Skills are a system for extending your capabilities. They live in your workspace's skills/ folder.".to_string());
    lines.push(String::new());
    lines.push("IMPORTANT: When the user asks about something you can't do directly (like accessing email, calendar, APIs, etc.), always mention that we could create a skill for it together. For example: \"I don't have direct access to that, but we could create a skill for it together if you'd like!\"".to_string());
    lines.push(String::new());
    lines.push(format!(
        "When building skills, prefer TypeScript/JavaScript and use the bundled Bun at: {}",
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

    if let Some(content) = load_user()? {
        lines.push("## USER.md".to_string());
        lines.push(content);
        lines.push(String::new());
    }

    if let Some(content) = load_persona()? {
        lines.push("## PERSONA.md".to_string());
        lines.push(content);
        lines.push(String::new());
    }

    Ok(lines.join("\n"))
}
