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
use std::path::{Path, PathBuf};
use tracing::{info, warn};

use crate::config::{self, ChannelSettings, Config, Paths};
use crate::memory::MemoryIndex;
use crate::setup;
use crate::skills;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// Need to configure agent identity (first user only)
    Identity,
    /// Need to learn about this specific user
    User,
    /// Onboarding complete for this user
    Complete,
}

pub fn user_dir(paths: &Paths, channel: &str, user_id: &str) -> PathBuf {
    paths
        .base
        .join("users")
        .join(format!("{}_{}", channel, user_id))
}

pub fn identity_path_for_user(paths: &Paths, channel: &str, user_id: &str) -> PathBuf {
    user_dir(paths, channel, user_id).join("IDENTITY.md")
}

pub fn user_path_for_user(paths: &Paths, channel: &str, user_id: &str) -> PathBuf {
    user_dir(paths, channel, user_id).join("USER.md")
}

pub fn current_phase_for_user(
    paths: &Paths,
    settings: &ChannelSettings,
    channel: &str,
    user_id: &str,
) -> Result<Phase> {
    // If shared_identity is enabled, skip identity phase (use PERSONA.md)
    if !settings.shared_identity && !identity_path_for_user(paths, channel, user_id).exists() {
        return Ok(Phase::Identity);
    }

    if !user_path_for_user(paths, channel, user_id).exists() {
        return Ok(Phase::User);
    }

    Ok(Phase::Complete)
}

pub fn is_complete_for_user(
    paths: &Paths,
    settings: &ChannelSettings,
    channel: &str,
    user_id: &str,
) -> Result<bool> {
    Ok(current_phase_for_user(paths, settings, channel, user_id)? == Phase::Complete)
}

pub fn system_prompt_for_user(
    paths: &Paths,
    settings: &ChannelSettings,
    channel: &str,
    user_id: &str,
) -> Result<String> {
    match current_phase_for_user(paths, settings, channel, user_id)? {
        Phase::Identity => identity_system_prompt(paths, channel, user_id),
        Phase::User => user_system_prompt(paths, settings, channel, user_id),
        Phase::Complete => Ok(String::new()),
    }
}

fn identity_system_prompt(paths: &Paths, channel: &str, user_id: &str) -> Result<String> {
    let path = identity_path_for_user(paths, channel, user_id);

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

const DEFAULT_ONBOARDING_PROMPT: &str = "Tell me about yourself - the more I know about you the better I'll be able to help, so don't be shy!";

fn user_system_prompt(
    paths: &Paths,
    settings: &ChannelSettings,
    channel: &str,
    user_id: &str,
) -> Result<String> {
    let identity_path = identity_path_for_user(paths, channel, user_id);
    let user_path = user_path_for_user(paths, channel, user_id);
    let identity = std::fs::read_to_string(&identity_path).unwrap_or_default();

    let onboarding_prompt = settings
        .clone()
        .onboarding_prompt
        .unwrap_or_else(|| DEFAULT_ONBOARDING_PROMPT.to_string());

    Ok(format!(
        r#"You are an AI assistant with this identity:

{}

You just finished setting up your identity. Now ask the user to tell you about themselves.

Keep it casual and short. Use this prompt:
"{}"

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
        onboarding_prompt,
        user_path.display()
    ))
}

pub fn load_identity_for_user(
    paths: &Paths,
    channel: &str,
    user_id: &str,
) -> Result<Option<String>> {
    let path = identity_path_for_user(paths, channel, user_id);
    if !path.exists() {
        return Ok(None);
    }
    Ok(Some(std::fs::read_to_string(&path)?))
}

pub fn load_user_for_user(paths: &Paths, channel: &str, user_id: &str) -> Result<Option<String>> {
    let path = user_path_for_user(paths, channel, user_id);
    if !path.exists() {
        return Ok(None);
    }
    Ok(Some(std::fs::read_to_string(&path)?))
}

pub fn load_persona(paths: &Paths) -> Result<Option<String>> {
    let path = paths.base.join("PERSONA.md");
    if !path.exists() {
        return Ok(None);
    }
    Ok(Some(std::fs::read_to_string(&path)?))
}

fn workspace_relative(path: &Path, base: &Path) -> String {
    path.strip_prefix(base)
        .unwrap_or(path)
        .display()
        .to_string()
}

/// Build system prompt with all context for a specific user
///
/// If `user_message` is provided, it will be used to search for relevant memories
/// to include in the context.
/// How many memory chunks the similarity search returns for a turn.
const MEMORY_SEARCH_LIMIT: usize = 3;

/// Chunks scoring at or below this are not pasted into the prompt. Deliberately
/// a named constant: it is the knob this behaviour is tuned with, and 0.3 has
/// been observed admitting chunks with no topical relationship to the question.
const MEMORY_SCORE_THRESHOLD: f32 = 0.3;

/// The `### Relevant Memories` block, or nothing.
///
/// Split out from prompt assembly so the threshold behaviour can be tested
/// without an index and an embedding model behind it.
fn render_relevant_memories(results: Vec<crate::memory::MemorySearchResult>) -> Vec<String> {
    let (kept, dropped): (Vec<_>, Vec<_>) = results
        .into_iter()
        .partition(|r| r.score > MEMORY_SCORE_THRESHOLD);

    // What gets pasted into the prompt is otherwise invisible: nobody reads the
    // assembled system prompt. Log it per turn so the threshold can be tuned
    // against evidence, and so an answer that names something nobody asked
    // about can be checked against what the model was actually handed.
    if !kept.is_empty() || !dropped.is_empty() {
        let summary = |rs: &[crate::memory::MemorySearchResult]| {
            rs.iter()
                .map(|r| format!("{}({:.2})", r.path, r.score))
                .collect::<Vec<_>>()
                .join(", ")
        };
        info!(
            injected_chars = kept.iter().map(|r| r.chunk.len()).sum::<usize>(),
            threshold = MEMORY_SCORE_THRESHOLD,
            injected = %summary(&kept),
            below_threshold = %summary(&dropped),
            "memory retrieval",
        );
    }

    if kept.is_empty() {
        return Vec::new();
    }

    // The heading is emitted only now. It used to be pushed ahead of the score
    // filter, so a turn whose every hit fell below the threshold produced a
    // "Relevant Memories" heading with nothing under it.
    let mut lines = vec![
        "### Relevant Memories".to_string(),
        "The following memories may be relevant to this conversation:".to_string(),
        String::new(),
    ];
    for result in kept {
        lines.push(format!("**From {}:**", result.path));
        lines.push(result.chunk);
        lines.push(String::new());
    }
    lines
}

pub fn build_context_prompt_for_user(
    config: &Config,
    paths: &Paths,
    channel_display: Option<&str>,
    channel_id: Option<&str>,
    user_id: Option<&str>,
    user_message: Option<&str>,
) -> Result<String> {
    let mut lines = Vec::new();

    let identity = if let (Some(ch), Some(uid)) = (channel_id, user_id) {
        load_identity_for_user(paths, ch, uid)?
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

    let user_content = if let (Some(ch), Some(uid)) = (channel_id, user_id) {
        load_user_for_user(paths, ch, uid)?
    } else {
        None
    };

    let channel_info = channel_display
        .map(|c| format!(" (via {})", c))
        .unwrap_or_default();
    lines.push(format!(
        "You are {}, a personal AI assistant. You are chatting with your user via a messaging app{}.",
        assistant_name, channel_info
    ));
    lines.push(String::new());

    let now = chrono::Local::now();
    lines.push(format!(
        "Current date and time: {}",
        now.format("%Y-%m-%d %H:%M (%A)")
    ));
    lines.push(String::new());

    lines.push("## Capabilities".to_string());
    lines.push("You can:".to_string());
    lines.push("- Have conversations and answer questions".to_string());
    lines.push("- Help with writing, brainstorming, and thinking through problems".to_string());
    lines.push("- Read and write files in your workspace".to_string());
    lines.push("- Run shell commands when needed".to_string());
    lines.push("- Search the web for current information".to_string());
    lines.push("- Schedule tasks to run automatically (cron jobs)".to_string());
    lines.push(String::new());

    if let Some(channel_name) = channel_display {
        lines.push("## Messaging Channel".to_string());
        lines.push(format!(
            "You are currently communicating via {}.",
            channel_name
        ));
        lines.push(
            "IMPORTANT: Your final response is the ONLY thing sent to the user. Tool calls, intermediate reasoning, and earlier assistant messages are NOT visible. If a task produces a deliverable (report, analysis, code, etc.), the full deliverable MUST be in your final response — not a summary of what you did."
                .to_string(),
        );
        lines.push(String::new());

        // Media attachments
        lines.push("### Media Attachments".to_string());
        lines.push(
            "When your response includes a file that should be sent to the user (image, video, etc.), output it on its own line using this format:"
                .to_string(),
        );
        lines.push("`[attachment:/path/to/file.png]`".to_string());
        lines.push(
            "The messaging system will automatically detect it, attach the file, and remove the marker line from the message. Use this for any generated images, videos, or other media files."
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

    lines.push("## Skills".to_string());
    lines.push(
        "Skills extend your capabilities. They live in the skills/ folder of your workspace."
            .to_string(),
    );
    lines.push(String::new());

    // Discover and list available skills
    match skills::discover_skills(
        paths,
        config::prep_skill_deps_locally(config.deployment.provider),
    ) {
        Ok(discovered) if !discovered.is_empty() => {
            lines.push("### Available Skills".to_string());
            lines.push("To use a skill, read its SKILL.md file at the location shown, then follow its instructions.".to_string());
            lines.push(String::new());
            lines.push(skills::format_skills_xml(&discovered, &paths.base));
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
    lines.push(String::new());
    lines.push(format!(
        "Use the bundled Bun at: {} (relative to your workspace)",
        workspace_relative(&paths.bun_dir.join("bun"), &paths.base)
    ));
    lines.push(String::new());

    // Skill configuration
    lines.push("### Skill Configuration".to_string());
    lines.push("Skills that need configuration (API keys, credentials, preferences) should support two config locations:".to_string());
    lines.push(String::new());
    lines.push(
        "1. **Global config**: `skills/{skill-name}/config.json` - shared by all users".to_string(),
    );
    lines.push("2. **Per-user config**: `users/{channel}_{user_id}/skill-configs/{skill-name}.json` - specific to one user".to_string());
    lines.push(String::new());
    lines.push("**When creating a skill that needs config:**".to_string());
    lines.push(
        "- Ask the user: \"Should this config be shared globally, or specific to just you?\""
            .to_string(),
    );
    lines.push("- Global: useful for shared API keys or server-wide settings".to_string());
    lines
        .push("- Per-user: useful for personal credentials, user-specific preferences".to_string());
    lines.push(String::new());
    lines.push("**When running a skill:**".to_string());
    lines.push("- Check for per-user config first (using current channel and user_id)".to_string());
    lines.push("- Fall back to global config if no per-user config exists".to_string());
    lines.push(String::new());

    lines.push("## Workspace".to_string());
    // No absolute paths: the prompt is built where the router runs but the turn
    // may execute in a container or remote sandbox with a different layout.
    lines.push(
        "Your workspace is the current working directory. Paths shown below, including \
         skill locations, are relative to it."
            .to_string(),
    );
    lines.push(String::new());

    if let (Some(ch), Some(uid)) = (channel_id, user_id) {
        lines.push("## Current User Context".to_string());
        lines.push(format!("- Channel: {}", ch));
        lines.push(format!("- User ID: {}", uid));
        lines.push(String::new());
    }

    lines.push("## MCP (Model Context Protocol)".to_string());
    lines.push("You can extend your capabilities by adding MCP servers. MCP servers provide additional tools (API access, databases, services, etc.) that become available to you automatically.".to_string());
    lines.push(String::new());
    match config.backend {
        config::AiBackend::Claude => {
            let mcp_config_path = paths.claude_home.join(".claude").join("settings.json");
            lines.push(format!(
                "To add an MCP server, edit: {}",
                workspace_relative(&mcp_config_path, &paths.base)
            ));
            lines.push(String::new());
            lines.push("The file uses this format:".to_string());
            lines.push("```json".to_string());
            lines.push(
                r#"{
  "mcpServers": {
    "server-name": {
      "command": "npx",
      "args": ["-y", "some-mcp-package"],
      "env": {}
    }
  }
}"#
                .to_string(),
            );
            lines.push("```".to_string());
        }
        config::AiBackend::Cursor => {
            let mcp_config_path = paths.cursor_home.join(".cursor").join("mcp.json");
            let cursor_cli = setup::find_cursor_cli(paths)
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "cursor-agent".to_string());
            lines.push(format!(
                "To add an MCP server, edit: {}",
                workspace_relative(&mcp_config_path, &paths.base)
            ));
            lines.push(String::new());
            lines.push("The file uses this format:".to_string());
            lines.push("```json".to_string());
            lines.push(
                r#"{
  "mcpServers": {
    "server-name": {
      "command": "npx",
      "args": ["-y", "some-mcp-package"],
      "env": {}
    }
  }
}"#
                .to_string(),
            );
            lines.push("```".to_string());
            lines.push(String::new());
            lines.push(format!(
                "After adding the config, enable the server by running: HOME=$PWD/{} {} mcp enable <server-name>",
                workspace_relative(&paths.cursor_home, &paths.base),
                cursor_cli,
            ));
        }
    }
    lines.push(String::new());
    lines.push("After adding an MCP server, it will be available on the next message (new session). The user may need to send /new to start a fresh session for new MCP servers to take effect.".to_string());
    lines.push(String::new());

    lines.push("## Cron Job Management".to_string());
    lines.push(
        "You can manage scheduled cron jobs conversationally by reading and writing the cron store file directly."
            .to_string(),
    );
    lines.push(format!(
        "The cron store is a JSON file at: {}",
        workspace_relative(&paths.base.join("cron.json"), &paths.base)
    ));
    lines.push(String::new());
    lines.push(
        r#"### Cron Store Format

The file is a JSON object with a `jobs` key mapping job IDs to job objects:

```json
{
  "jobs": {
    "uuid-string": {
      "id": "uuid-string",
      "name": "Short name (max 30 chars)",
      "prompt": "The prompt to execute",
      "schedule": { "type": "Every", "value": 3600000 },
      "channel": "slack",
      "user_id": "U12345678",
      "target": {
        "channel_id": "C12345678"
      },
      "notify": true,
      "enabled": true,
      "created_at": 1700000000000,
      "state": {
        "next_run_at": 1700003600000,
        "last_run_at": null,
        "last_status": "Pending",
        "last_duration_ms": null,
        "failure_count": 0
      }
    }
  }
}
```

### Schedule Types

- Recurring interval (milliseconds): `{ "type": "Every", "value": 3600000 }` (common: 10s=10000, 1m=60000, 5m=300000, 1h=3600000, 1d=86400000)
- One-time (Unix ms timestamp): `{ "type": "At", "value": 1700000000000 }`
- Cron expression (5-field): `{ "type": "Cron", "value": "0 9 * * *" }` (minute hour day-of-month month day-of-week)

### Delivery Target

Controls where results are sent. Omit entirely or use `{}` for owner DM (default).

- `channel_id`: a platform channel ID (e.g., Slack channel "C0123456789"). Null/absent = owner DM.
- `thread_id`: optional thread identifier (e.g., Slack thread_ts). Only meaningful with channel_id.

For Telegram and Signal, target is ignored (always DMs the owner).

### Creating a Job

1. Generate a UUID (use `uuidgen` command)
2. Set `channel` and `user_id` from Current User Context above
3. Set `created_at` to current Unix milliseconds
4. For `state`, use defaults: `{ "next_run_at": null, "last_run_at": null, "last_status": "Pending", "last_duration_ms": null, "failure_count": 0 }`
5. Calculate `next_run_at`: for Every schedules use `current_time_ms + interval_ms`, for At use the timestamp, for Cron set to null (the scheduler calculates it)
6. Read existing cron.json, add the job, write it back

The scheduler automatically reloads cron.json every 60 seconds — changes take effect without a restart.

### Other Operations

- **List**: Read cron.json, filter by channel + user_id
- **Pause**: Set `enabled` to false and `next_run_at` to null
- **Resume**: Set `enabled` to true and recalculate `next_run_at`
- **Delete**: Remove the job entry from the jobs map
- **Edit**: Update fields (prompt, schedule, target, name), recalculate `next_run_at` if schedule changed

IMPORTANT: Do not modify the `state` fields of jobs with `last_status: "Running"` — they are being executed."#
            .to_string(),
    );
    lines.push(String::new());

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

    if let Some(content) = load_persona(paths)? {
        lines.push("## PERSONA.md".to_string());
        lines.push(content);
        lines.push(String::new());
    }

    if let (Some(ch), Some(uid)) = (channel_id, user_id) {
        lines.push("## Memories".to_string());
        lines.push(format!(
            "You have a per-user memory store at: {}",
            crate::memory::MEMORIES_DIR_TOKEN
        ));
        lines.push(String::new());
        lines.push("**Personal / user-specific** facts — this user's preferences, the projects they're driving, how they like answers, things they tell you about themselves — go in memory:".to_string());
        lines.push("1. Ask the user if they'd like you to remember it.".to_string());
        lines.push(format!(
            "2. If they agree, write a markdown file under {} with a descriptive name (e.g. `preferences.md`, `project-foo.md`), formatted with headers and bullets.",
            crate::memory::MEMORIES_DIR_TOKEN
        ));
        lines.push("Ask first; don't save trivia.".to_string());
        lines.push(String::new());
        // Search for relevant memories if we have a user message
        if let Some(query) = user_message {
            match MemoryIndex::open(paths) {
                Ok(index) => match index.search(ch, uid, query, MEMORY_SEARCH_LIMIT) {
                    Ok(results) => lines.extend(render_relevant_memories(results)),
                    Err(e) => {
                        warn!("Failed to search memories: {}", e);
                    }
                },
                Err(e) => {
                    warn!("Failed to open memory index: {}", e);
                }
            }
        }
    }

    Ok(lines.join("\n"))
}

#[cfg(test)]
mod memory_guidance_tests {
    use super::*;

    fn prompt_for(paths: &Paths) -> String {
        build_context_prompt_for_user(
            &Config::default(),
            paths,
            Some("Telegram"),
            Some("telegram"),
            Some("1"),
            None,
        )
        .expect("prompt builds")
    }

    #[test]
    fn guidance_emits_token_and_says_nothing_about_a_shared_corpus() {
        let (_temp, paths) = config::test_paths();
        let prompt = prompt_for(&paths);
        // Emits the placeholder token, not a router-absolute path.
        assert!(prompt.contains(crate::memory::MEMORIES_DIR_TOKEN));
        assert!(!prompt.contains("propose-knowledge"));
        assert!(!prompt.contains("org-wide"));
    }

    fn hit(path: &str, score: f32) -> crate::memory::MemorySearchResult {
        crate::memory::MemorySearchResult {
            path: path.to_string(),
            chunk: format!("chunk from {path}"),
            score,
        }
    }

    #[test]
    fn a_heading_is_only_emitted_when_something_clears_the_threshold() {
        // Every hit below the threshold used to still produce the heading and
        // its "may be relevant" preamble, with no memories underneath.
        let all_weak = render_relevant_memories(vec![
            hit("methodology-contacts.md", 0.29),
            hit("preferences.md", 0.12),
        ]);
        assert!(all_weak.is_empty(), "got {all_weak:?}");

        let some_strong = render_relevant_memories(vec![
            hit("project-sprout.md", 0.61),
            hit("methodology-contacts.md", 0.29),
        ]);
        let text = some_strong.join("\n");
        assert!(text.contains("### Relevant Memories"));
        assert!(text.contains("project-sprout.md"));
        assert!(
            !text.contains("methodology-contacts.md"),
            "a chunk below the threshold was injected: {text}"
        );
    }

    #[test]
    fn nothing_retrieved_means_nothing_added() {
        assert!(render_relevant_memories(Vec::new()).is_empty());
    }

    #[test]
    fn the_threshold_is_exclusive_at_the_boundary() {
        assert!(render_relevant_memories(vec![hit("a.md", MEMORY_SCORE_THRESHOLD)]).is_empty());
        assert!(
            !render_relevant_memories(vec![hit("a.md", MEMORY_SCORE_THRESHOLD + 0.01)]).is_empty()
        );
    }

    #[test]
    fn prompt_does_not_leak_the_building_machine_paths() {
        let (temp, paths) = config::test_paths();
        let prompt = build_context_prompt_for_user(
            &Config::default(),
            &paths,
            Some("Telegram"),
            Some("telegram"),
            Some("1"),
            None,
        )
        .expect("prompt builds");

        assert!(
            !prompt.contains(&temp.path().display().to_string()),
            "prompt leaks the workspace root {}, which will not exist on a worker",
            temp.path().display()
        );
        assert!(prompt.contains("current working directory"));
    }
}
