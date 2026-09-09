//! Maps a Claude session id to its on-disk files and captures/restores them.
//!
//! Capture finds files by session id (slug-independent). Restore writes the
//! transcript under the slug of the *current* cwd, so a worker with a different
//! cwd in a later phase still resumes correctly.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::sandbox::state::{clear_dir, copy_dir_all, copy_path};

/// Backend-specific capture/restore of a session's on-disk state.
///
/// `home` is the backend's HOME dir (claude_home or cursor_home). `capture`
/// copies the files making up `session_id` into `staging` (returns false if the
/// session isn't found); `restore` reinstates them under `home` so a resume run
/// with `cwd` finds them.
pub trait SessionArtifacts {
    fn capture(&self, home: &Path, session_id: &str, staging: &Path) -> Result<bool>;
    fn restore(&self, home: &Path, cwd: &Path, session_id: &str, staging: &Path) -> Result<()>;
    fn forget(&self, home: &Path, session_id: &str) -> Result<()>;

    /// Return the most recently written session in `home`, or `None` if the backend cannot determine it.
    fn latest_session(&self, _home: &Path) -> Option<String> {
        None
    }
}

/// Slugify a working directory the way Claude Code names its project dir:
/// every non-alphanumeric character becomes `-`.
pub fn claude_project_slug(cwd: &Path) -> String {
    cwd.to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

pub struct ClaudeSessionArtifacts;

impl ClaudeSessionArtifacts {
    /// Copy the files making up `session_id` from `claude_home` into `staging`,
    /// laid out as `transcript.jsonl`, `session-env`, and `todos/`.
    /// Returns `false` (capturing nothing) if no transcript is found.
    pub fn capture(claude_home: &Path, session_id: &str, staging: &Path) -> Result<bool> {
        let dot = claude_home.join(".claude");
        clear_dir(staging)?;

        // Transcript: find <session_id>.jsonl under any projects/<slug>/ dir.
        let projects = dot.join("projects");
        let mut transcript: Option<PathBuf> = None;
        if projects.is_dir() {
            for entry in fs::read_dir(&projects)? {
                let candidate = entry?.path().join(format!("{session_id}.jsonl"));
                if candidate.is_file() {
                    transcript = Some(candidate);
                    break;
                }
            }
        }
        let Some(transcript) = transcript else {
            return Ok(false);
        };
        fs::copy(&transcript, staging.join("transcript.jsonl"))?;

        // session-env/<id> (file or dir), if present.
        let env_src = dot.join("session-env").join(session_id);
        if env_src.exists() {
            copy_path(&env_src, &staging.join("session-env"))?;
        }

        // todos/<id>-*.json, if present.
        let todos_src = dot.join("todos");
        if todos_src.is_dir() {
            let prefix = format!("{session_id}-");
            let staged_todos = staging.join("todos");
            for entry in fs::read_dir(&todos_src)? {
                let entry = entry?;
                let name = entry.file_name();
                if name.to_string_lossy().starts_with(&prefix) {
                    fs::create_dir_all(&staged_todos)?;
                    fs::copy(entry.path(), staged_todos.join(&name))?;
                }
            }
        }
        Ok(true)
    }

    /// Restore staged artifacts into `claude_home` so `claude --resume
    /// <session_id>` (run with `cwd`) finds them.
    pub fn restore(claude_home: &Path, cwd: &Path, session_id: &str, staging: &Path) -> Result<()> {
        let dot = claude_home.join(".claude");

        let transcript = staging.join("transcript.jsonl");
        if transcript.is_file() {
            let proj = dot.join("projects").join(claude_project_slug(cwd));
            fs::create_dir_all(&proj)?;
            fs::copy(&transcript, proj.join(format!("{session_id}.jsonl")))?;
        }

        let env_staged = staging.join("session-env");
        if env_staged.exists() {
            let env_dst = dot.join("session-env").join(session_id);
            if let Some(parent) = env_dst.parent() {
                fs::create_dir_all(parent)?;
            }
            // Remove any stale destination so a file<->dir type change can't
            // make the copy fail.
            if env_dst.exists() {
                if env_dst.is_dir() {
                    fs::remove_dir_all(&env_dst)?;
                } else {
                    fs::remove_file(&env_dst)?;
                }
            }
            copy_path(&env_staged, &env_dst)?;
        }

        // `.claude/todos/` is shared across ALL sessions (files are named
        // `<session_id>-agent-*.json`), so we merge our session's todos in
        // rather than clearing the directory, which would delete other
        // sessions' todos.
        let todos_staged = staging.join("todos");
        if todos_staged.is_dir() {
            copy_dir_all(&todos_staged, &dot.join("todos"))?;
        }
        Ok(())
    }
}

impl SessionArtifacts for ClaudeSessionArtifacts {
    fn capture(&self, home: &Path, session_id: &str, staging: &Path) -> Result<bool> {
        ClaudeSessionArtifacts::capture(home, session_id, staging)
    }
    fn restore(&self, home: &Path, cwd: &Path, session_id: &str, staging: &Path) -> Result<()> {
        ClaudeSessionArtifacts::restore(home, cwd, session_id, staging)
    }
    /// Claude Code writes `.jsonl` transcripts during execution, so they survive a timeout without a returned session ID.
    fn latest_session(&self, home: &Path) -> Option<String> {
        let projects = home.join(".claude").join("projects");
        let mut newest: Option<(std::time::SystemTime, String)> = None;
        for project in fs::read_dir(projects).ok()?.flatten() {
            let Ok(entries) = fs::read_dir(project.path()) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().is_none_or(|ext| ext != "jsonl") {
                    continue;
                }
                let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                    continue;
                };
                let Ok(modified) = entry.metadata().and_then(|m| m.modified()) else {
                    continue;
                };
                if newest.as_ref().is_none_or(|(at, _)| modified > *at) {
                    newest = Some((modified, stem.to_string()));
                }
            }
        }
        newest.map(|(_, session)| session)
    }

    fn forget(&self, home: &Path, session_id: &str) -> Result<()> {
        let dot = home.join(".claude");
        let projects = dot.join("projects");
        if projects.is_dir() {
            for entry in fs::read_dir(projects)? {
                let path = entry?.path().join(format!("{session_id}.jsonl"));
                if path.is_file() {
                    fs::remove_file(path)?;
                }
            }
        }
        let env = dot.join("session-env").join(session_id);
        if env.is_dir() {
            fs::remove_dir_all(env)?;
        } else if env.is_file() {
            fs::remove_file(env)?;
        }
        let todos = dot.join("todos");
        if todos.is_dir() {
            let prefix = format!("{session_id}-");
            for entry in fs::read_dir(todos)? {
                let entry = entry?;
                if entry.file_name().to_string_lossy().starts_with(&prefix) {
                    fs::remove_file(entry.path())?;
                }
            }
        }
        Ok(())
    }
}

/// Capture/restore of Cursor session state.
///
/// A Cursor session lives at `cursor_home/.cursor/chats/<workspace_hash>/<id>/`
/// as SQLite files (`store.db` + `-wal` + `-shm`). The workspace hash is
/// `md5(realpath(cwd))`; we record the hash dir at capture and replay it at
/// restore — correct as long as all workers share a resolved cwd (the fleet
/// requirement), and resilient to Cursor changing its hashing.
pub struct CursorSessionArtifacts;

const CURSOR_DB_FILES: [&str; 3] = ["store.db", "store.db-wal", "store.db-shm"];

impl SessionArtifacts for CursorSessionArtifacts {
    fn capture(&self, home: &Path, session_id: &str, staging: &Path) -> Result<bool> {
        clear_dir(staging)?;
        let chats = home.join(".cursor").join("chats");
        if !chats.is_dir() {
            return Ok(false);
        }
        // Find <workspace_hash>/<session_id>/ under chats (hash-independent).
        for entry in fs::read_dir(&chats)? {
            let ws = entry?;
            let session_dir = ws.path().join(session_id);
            if session_dir.is_dir() {
                // Stage as staging/<workspace_hash>/<files>, recording the hash.
                let dest = staging.join(ws.file_name());
                fs::create_dir_all(&dest)?;
                for f in CURSOR_DB_FILES {
                    let src = session_dir.join(f);
                    if src.is_file() {
                        fs::copy(&src, dest.join(f))?;
                    }
                }
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn restore(&self, home: &Path, _cwd: &Path, session_id: &str, staging: &Path) -> Result<()> {
        // The single subdir in staging is the recorded workspace hash.
        let mut recorded = None;
        for entry in fs::read_dir(staging)? {
            let entry = entry?;
            if entry.path().is_dir() {
                recorded = Some(entry.file_name());
                break;
            }
        }
        let Some(hash) = recorded else {
            return Ok(()); // nothing staged
        };
        let staged_dir = staging.join(&hash);
        let dest = home
            .join(".cursor")
            .join("chats")
            .join(&hash)
            .join(session_id);
        fs::create_dir_all(&dest)?;
        for f in CURSOR_DB_FILES {
            let src = staged_dir.join(f);
            if src.is_file() {
                fs::copy(&src, dest.join(f))?;
            }
        }
        Ok(())
    }

    fn forget(&self, home: &Path, session_id: &str) -> Result<()> {
        let chats = home.join(".cursor").join("chats");
        if chats.is_dir() {
            for entry in fs::read_dir(chats)? {
                let path = entry?.path().join(session_id);
                if path.is_dir() {
                    fs::remove_dir_all(path)?;
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slug_matches_known_example() {
        let cwd = Path::new("/Users/dcvz/Library/Application Support/cica");
        assert_eq!(
            claude_project_slug(cwd),
            "-Users-dcvz-Library-Application-Support-cica"
        );
    }

    fn write(path: &Path, contents: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    #[test]
    fn capture_then_restore_reproduces_files() {
        let id = "abc-123";
        let cwd = Path::new("/work/cica");
        let slug = claude_project_slug(cwd);

        let home_a = tempfile::tempdir().unwrap();
        let dot_a = home_a.path().join(".claude");
        write(
            &dot_a
                .join("projects")
                .join(&slug)
                .join(format!("{id}.jsonl")),
            "line1\n",
        );
        write(&dot_a.join("session-env").join(id), "ENV=1");
        write(
            &dot_a.join("todos").join(format!("{id}-agent-{id}.json")),
            "[]",
        );

        let staging = tempfile::tempdir().unwrap();
        assert!(ClaudeSessionArtifacts::capture(home_a.path(), id, staging.path()).unwrap());

        let home_b = tempfile::tempdir().unwrap();
        ClaudeSessionArtifacts::restore(home_b.path(), cwd, id, staging.path()).unwrap();

        let dot_b = home_b.path().join(".claude");
        assert_eq!(
            fs::read_to_string(
                dot_b
                    .join("projects")
                    .join(&slug)
                    .join(format!("{id}.jsonl"))
            )
            .unwrap(),
            "line1\n"
        );
        assert_eq!(
            fs::read_to_string(dot_b.join("session-env").join(id)).unwrap(),
            "ENV=1"
        );
        assert_eq!(
            fs::read_to_string(dot_b.join("todos").join(format!("{id}-agent-{id}.json"))).unwrap(),
            "[]"
        );
    }

    #[test]
    fn capture_returns_false_without_transcript() {
        let home = tempfile::tempdir().unwrap();
        let staging = tempfile::tempdir().unwrap();
        assert!(!ClaudeSessionArtifacts::capture(home.path(), "no-such", staging.path()).unwrap());
    }

    #[test]
    fn claude_via_trait_round_trips() {
        let artifacts: &dyn SessionArtifacts = &ClaudeSessionArtifacts;
        let id = "abc-123";
        let cwd = Path::new("/work/cica");
        let slug = claude_project_slug(cwd);

        let home_a = tempfile::tempdir().unwrap();
        write(
            &home_a
                .path()
                .join(".claude")
                .join("projects")
                .join(&slug)
                .join(format!("{id}.jsonl")),
            "line1\n",
        );
        let staging = tempfile::tempdir().unwrap();
        assert!(
            artifacts
                .capture(home_a.path(), id, staging.path())
                .unwrap()
        );

        let home_b = tempfile::tempdir().unwrap();
        artifacts
            .restore(home_b.path(), cwd, id, staging.path())
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(
                home_b
                    .path()
                    .join(".claude")
                    .join("projects")
                    .join(&slug)
                    .join(format!("{id}.jsonl"))
            )
            .unwrap(),
            "line1\n"
        );
    }

    #[test]
    fn cursor_capture_then_restore_reproduces_session_db() {
        let id = "6cd64aba-d369-4444-b2f9-acda76abdf3f";
        let hash = "5c64d42749f92f28359bff54fe4cb4bc";

        let home_a = tempfile::tempdir().unwrap();
        let session_dir = home_a
            .path()
            .join(".cursor")
            .join("chats")
            .join(hash)
            .join(id);
        write(&session_dir.join("store.db"), "DB");
        write(&session_dir.join("store.db-wal"), "WAL");
        write(&session_dir.join("store.db-shm"), "SHM");

        let artifacts = CursorSessionArtifacts;
        let staging = tempfile::tempdir().unwrap();
        assert!(
            artifacts
                .capture(home_a.path(), id, staging.path())
                .unwrap()
        );

        let home_b = tempfile::tempdir().unwrap();
        artifacts
            .restore(home_b.path(), Path::new("/whatever"), id, staging.path())
            .unwrap();

        let dest = home_b
            .path()
            .join(".cursor")
            .join("chats")
            .join(hash)
            .join(id);
        assert_eq!(
            std::fs::read_to_string(dest.join("store.db")).unwrap(),
            "DB"
        );
        assert_eq!(
            std::fs::read_to_string(dest.join("store.db-wal")).unwrap(),
            "WAL"
        );
        assert_eq!(
            std::fs::read_to_string(dest.join("store.db-shm")).unwrap(),
            "SHM"
        );
    }

    #[test]
    fn cursor_capture_returns_false_when_absent() {
        let home = tempfile::tempdir().unwrap();
        let staging = tempfile::tempdir().unwrap();
        let artifacts = CursorSessionArtifacts;
        assert!(
            !artifacts
                .capture(home.path(), "no-such", staging.path())
                .unwrap()
        );
    }

    #[test]
    fn cursor_restore_with_empty_staging_is_noop() {
        let home = tempfile::tempdir().unwrap();
        let staging = tempfile::tempdir().unwrap();
        CursorSessionArtifacts
            .restore(home.path(), Path::new("/x"), "any", staging.path())
            .unwrap();
        assert!(!home.path().join(".cursor").exists());
    }

    #[test]
    fn claude_forget_removes_only_the_named_session() {
        let home = tempfile::tempdir().unwrap();
        let dot = home.path().join(".claude");
        write(&dot.join("projects/a/one.jsonl"), "one");
        write(&dot.join("projects/a/two.jsonl"), "two");
        write(&dot.join("session-env/one/value"), "env");
        write(&dot.join("todos/one-agent.json"), "[]");
        write(&dot.join("todos/two-agent.json"), "[]");
        ClaudeSessionArtifacts.forget(home.path(), "one").unwrap();
        assert!(!dot.join("projects/a/one.jsonl").exists());
        assert!(!dot.join("session-env/one").exists());
        assert!(!dot.join("todos/one-agent.json").exists());
        assert!(dot.join("projects/a/two.jsonl").exists());
        assert!(dot.join("todos/two-agent.json").exists());
    }

    #[test]
    fn latest_session_finds_the_most_recently_written_transcript() {
        let home = tempfile::tempdir().unwrap();
        let projects = home.path().join(".claude/projects");
        write(&projects.join("-work-a/older.jsonl"), "{}");
        write(&projects.join("-work-b/newer.jsonl"), "{}");
        // Set distinct mtimes explicitly because filesystem timestamp resolution can make consecutive writes indistinguishable.
        let long_ago = std::time::SystemTime::now() - std::time::Duration::from_secs(600);
        fs::File::options()
            .write(true)
            .open(projects.join("-work-a/older.jsonl"))
            .unwrap()
            .set_modified(long_ago)
            .unwrap();

        assert_eq!(
            ClaudeSessionArtifacts.latest_session(home.path()),
            Some("newer".to_string()),
            "picked the wrong transcript, so an abandoned turn would be preserved under the wrong session"
        );
    }

    #[test]
    fn latest_session_is_none_when_nothing_was_written() {
        let home = tempfile::tempdir().unwrap();
        assert_eq!(ClaudeSessionArtifacts.latest_session(home.path()), None);
    }

    #[test]
    fn latest_session_ignores_non_transcripts() {
        let home = tempfile::tempdir().unwrap();
        let projects = home.path().join(".claude/projects");
        write(&projects.join("-work-a/notes.txt"), "not a transcript");
        assert_eq!(ClaudeSessionArtifacts.latest_session(home.path()), None);
    }

    #[test]
    fn cursor_does_not_pretend_to_know_the_latest_session() {
        let home = tempfile::tempdir().unwrap();
        assert_eq!(CursorSessionArtifacts.latest_session(home.path()), None);
    }

    #[test]
    fn cursor_forget_removes_only_the_named_session() {
        let home = tempfile::tempdir().unwrap();
        let chats = home.path().join(".cursor/chats");
        write(&chats.join("a/one/store.db"), "one");
        write(&chats.join("a/two/store.db"), "two");
        write(&chats.join("b/one/store.db"), "one");
        CursorSessionArtifacts.forget(home.path(), "one").unwrap();
        assert!(!chats.join("a/one").exists());
        assert!(!chats.join("b/one").exists());
        assert!(chats.join("a/two/store.db").exists());
    }

    #[test]
    fn cursor_capture_tolerates_missing_wal_shm() {
        let id = "sess-1";
        let hash = "abc123";
        let home_a = tempfile::tempdir().unwrap();
        write(
            &home_a
                .path()
                .join(".cursor")
                .join("chats")
                .join(hash)
                .join(id)
                .join("store.db"),
            "DB",
        );

        let artifacts = CursorSessionArtifacts;
        let staging = tempfile::tempdir().unwrap();
        assert!(
            artifacts
                .capture(home_a.path(), id, staging.path())
                .unwrap()
        );

        let home_b = tempfile::tempdir().unwrap();
        artifacts
            .restore(home_b.path(), Path::new("/x"), id, staging.path())
            .unwrap();
        let dest = home_b
            .path()
            .join(".cursor")
            .join("chats")
            .join(hash)
            .join(id);
        assert_eq!(
            std::fs::read_to_string(dest.join("store.db")).unwrap(),
            "DB"
        );
        assert!(!dest.join("store.db-wal").exists());
    }
}
