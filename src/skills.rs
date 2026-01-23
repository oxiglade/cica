//! Skills discovery and management.
//!
//! Skills are stored in the skills/ directory as subdirectories containing a SKILL.md file.
//! The SKILL.md file contains YAML frontmatter with name and description.

use anyhow::Result;
use std::path::PathBuf;

use crate::config;

/// A discovered skill
#[derive(Debug, Clone)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub location: PathBuf,
}

/// Discover all available skills from the skills directory
pub fn discover_skills() -> Result<Vec<Skill>> {
    let skills_dir = config::paths()?.skills_dir;

    if !skills_dir.exists() {
        return Ok(Vec::new());
    }

    let mut skills = Vec::new();

    let entries = std::fs::read_dir(&skills_dir)?;
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }

        let skill_file = path.join("SKILL.md");
        if !skill_file.exists() {
            continue;
        }

        if let Ok(skill) = parse_skill(&skill_file) {
            skills.push(skill);
        }
    }

    // Sort by name for consistent ordering
    skills.sort_by(|a, b| a.name.cmp(&b.name));

    Ok(skills)
}

/// Parse a SKILL.md file to extract skill metadata
fn parse_skill(path: &PathBuf) -> Result<Skill> {
    let content = std::fs::read_to_string(path)?;

    // Extract YAML frontmatter (between --- markers)
    let mut name = None;
    let mut description = None;

    if content.starts_with("---") {
        if let Some(end) = content[3..].find("---") {
            let frontmatter = &content[3..end + 3];

            for line in frontmatter.lines() {
                let line = line.trim();
                if let Some(value) = line.strip_prefix("name:") {
                    name = Some(
                        value
                            .trim()
                            .trim_matches('"')
                            .trim_matches('\'')
                            .to_string(),
                    );
                } else if let Some(value) = line.strip_prefix("description:") {
                    description = Some(
                        value
                            .trim()
                            .trim_matches('"')
                            .trim_matches('\'')
                            .to_string(),
                    );
                }
            }
        }
    }

    // Fall back to directory name if no name in frontmatter
    let dir_name = path
        .parent()
        .and_then(|p| p.file_name())
        .and_then(|n| n.to_str())
        .unwrap_or("unknown")
        .to_string();

    Ok(Skill {
        name: name.unwrap_or_else(|| dir_name.clone()),
        description: description.unwrap_or_else(|| format!("Skill: {}", dir_name)),
        location: path.clone(),
    })
}

/// Format skills as XML for the system prompt
pub fn format_skills_xml(skills: &[Skill]) -> String {
    if skills.is_empty() {
        return String::new();
    }

    let mut xml = String::from("<available_skills>\n");

    for skill in skills {
        xml.push_str("  <skill>\n");
        xml.push_str(&format!("    <name>{}</name>\n", escape_xml(&skill.name)));
        xml.push_str(&format!(
            "    <description>{}</description>\n",
            escape_xml(&skill.description)
        ));
        xml.push_str(&format!(
            "    <location>{}</location>\n",
            skill.location.display()
        ));
        xml.push_str("  </skill>\n");
    }

    xml.push_str("</available_skills>");
    xml
}

/// Escape special XML characters
fn escape_xml(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_escape_xml() {
        assert_eq!(escape_xml("hello"), "hello");
        assert_eq!(escape_xml("a < b"), "a &lt; b");
        assert_eq!(escape_xml("a & b"), "a &amp; b");
    }
}
