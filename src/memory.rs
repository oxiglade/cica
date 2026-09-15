//! Memory system for storing and retrieving user memories using vector search.
//!
//! Memories are stored as markdown files in users/{channel}_{user_id}/memories/
//! and indexed in a SQLite database with vector embeddings for semantic search.

use anyhow::{Context, Result};
use rusqlite::{Connection, ffi::sqlite3_auto_extension};
use std::ffi::c_char;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, Once};
use tracing::{debug, info, warn};

use crate::config::Paths;
use crate::onboarding::user_dir;

static SQLITE_VEC_INIT: Once = Once::new();

fn ensure_sqlite_vec_init() {
    SQLITE_VEC_INIT.call_once(|| unsafe {
        sqlite3_auto_extension(Some(std::mem::transmute::<
            *const (),
            unsafe extern "C" fn(
                *mut rusqlite::ffi::sqlite3,
                *mut *mut c_char,
                *const rusqlite::ffi::sqlite3_api_routines,
            ) -> i32,
        >(sqlite_vec::sqlite3_vec_init as *const ())));
    });
}

static EMBEDDING_MODEL: Mutex<Option<fastembed::TextEmbedding>> = Mutex::new(None);

fn with_embedding_model<F, R>(cache_dir: &Path, f: F) -> Result<R>
where
    F: FnOnce(&mut fastembed::TextEmbedding) -> Result<R>,
{
    let mut guard = EMBEDDING_MODEL
        .lock()
        .map_err(|e| anyhow::anyhow!("Lock poisoned: {}", e))?;

    if guard.is_none() {
        info!("Loading embedding model...");
        let model = fastembed::TextEmbedding::try_new(
            fastembed::InitOptions::new(fastembed::EmbeddingModel::BGESmallENV15)
                .with_cache_dir(cache_dir.to_path_buf())
                .with_show_download_progress(false),
        )
        .context("Failed to initialize embedding model")?;
        info!("Embedding model ready");
        *guard = Some(model);
    }

    f(guard.as_mut().unwrap())
}

/// Placeholder emitted into the system prompt by the router; substituted to the
/// local per-user memories path by `LocalProcessProvider` in the process that
/// actually runs the agent (the worker in cloud mode, the box itself single-box).
pub const MEMORIES_DIR_TOKEN: &str = "{MEMORIES_DIR}";

/// Get the memories directory for a user
pub fn memories_dir(paths: &Paths, channel: &str, user_id: &str) -> PathBuf {
    user_dir(paths, channel, user_id).join("memories")
}

/// Ensure the embedding model is downloaded (called during setup)
pub fn ensure_model_downloaded(paths: &Paths) -> Result<()> {
    with_embedding_model(&paths.models_dir, |_| Ok(()))
}

/// Memory search result
#[derive(Debug, Clone)]
pub struct MemorySearchResult {
    pub path: String,
    pub chunk: String,
    pub score: f32,
}

/// Memory index manager
pub struct MemoryIndex {
    db: Connection,
    paths: Paths,
}

impl MemoryIndex {
    /// Open or create the memory index database
    pub fn open(paths: &Paths) -> Result<Self> {
        ensure_sqlite_vec_init();

        let db_path = paths.base.join("memory.db");

        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let db = Connection::open(&db_path)?;

        db.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS memory_files (
                id INTEGER PRIMARY KEY,
                channel TEXT NOT NULL,
                user_id TEXT NOT NULL,
                path TEXT NOT NULL,
                hash TEXT NOT NULL,
                updated_at INTEGER NOT NULL,
                UNIQUE(channel, user_id, path)
            );

            CREATE TABLE IF NOT EXISTS memory_chunks (
                id INTEGER PRIMARY KEY,
                file_id INTEGER NOT NULL REFERENCES memory_files(id) ON DELETE CASCADE,
                chunk_index INTEGER NOT NULL,
                content TEXT NOT NULL,
                start_line INTEGER NOT NULL,
                end_line INTEGER NOT NULL,
                UNIQUE(file_id, chunk_index)
            );
            "#,
        )?;

        let has_vec_table: bool = db.query_row(
            "SELECT COUNT(*) > 0 FROM sqlite_master WHERE type='table' AND name='memory_vectors'",
            [],
            |row| row.get(0),
        )?;

        if !has_vec_table {
            // BGE-small-en-v1.5 produces 384-dimensional vectors
            db.execute_batch(
                r#"
                CREATE VIRTUAL TABLE memory_vectors USING vec0(
                    chunk_id INTEGER PRIMARY KEY,
                    embedding FLOAT[384]
                );
                "#,
            )?;
        }

        Ok(Self {
            db,
            paths: paths.clone(),
        })
    }

    /// Index all memory files for a user
    /// Drop every indexed row for one memory file.
    ///
    /// Deleting the file is not enough on its own: the chunk text and its
    /// embedding live in this database, so an orphaned row keeps being retrieved
    /// and pasted into prompts long after the memory it came from is gone.
    fn forget_file(&self, channel: &str, user_id: &str, rel_path: &str) -> Result<()> {
        self.db.execute(
            r#"
            DELETE FROM memory_vectors WHERE chunk_id IN (
                SELECT c.id FROM memory_chunks c
                JOIN memory_files f ON c.file_id = f.id
                WHERE f.channel = ? AND f.user_id = ? AND f.path = ?
            )
            "#,
            [channel, user_id, rel_path],
        )?;
        self.db.execute(
            r#"
            DELETE FROM memory_chunks WHERE file_id IN (
                SELECT id FROM memory_files
                WHERE channel = ? AND user_id = ? AND path = ?
            )
            "#,
            [channel, user_id, rel_path],
        )?;
        self.db.execute(
            "DELETE FROM memory_files WHERE channel = ? AND user_id = ? AND path = ?",
            [channel, user_id, rel_path],
        )?;
        Ok(())
    }

    /// Forget every indexed file for this user that is no longer on disk.
    ///
    /// The store is authoritative and is pulled onto disk before indexing, so a
    /// file missing here has been deleted — not merely un-synced.
    fn prune_missing(&self, channel: &str, user_id: &str, present: &[String]) -> Result<usize> {
        let indexed: Vec<String> = self
            .db
            .prepare("SELECT path FROM memory_files WHERE channel = ? AND user_id = ?")?
            .query_map([channel, user_id], |row| row.get(0))?
            .collect::<std::result::Result<_, _>>()?;

        let mut pruned = 0;
        for path in indexed {
            if !present.iter().any(|p| p == &path) {
                info!("Forgetting deleted memory file: {path}");
                self.forget_file(channel, user_id, &path)?;
                pruned += 1;
            }
        }
        Ok(pruned)
    }

    pub fn index_user_memories(&mut self, channel: &str, user_id: &str) -> Result<()> {
        let memories_path = memories_dir(&self.paths, channel, user_id);

        if !memories_path.exists() {
            // Not a no-op: a user who deleted their last memory has no directory
            // at all, and returning here used to leave every chunk they ever
            // wrote in the index, still retrievable and still injected.
            let pruned = self.prune_missing(channel, user_id, &[])?;
            if pruned > 0 {
                info!(
                    "Forgot {pruned} memory file(s) for {channel}:{user_id}; none remain on disk"
                );
            } else {
                debug!("No memories directory for {}:{}", channel, user_id);
            }
            return Ok(());
        }

        let entries: Vec<_> = std::fs::read_dir(&memories_path)?
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().map(|ext| ext == "md").unwrap_or(false))
            .collect();

        let present: Vec<String> = entries
            .iter()
            .map(|entry| {
                let path = entry.path();
                path.strip_prefix(&memories_path)
                    .unwrap_or(&path)
                    .to_string_lossy()
                    .to_string()
            })
            .collect();
        self.prune_missing(channel, user_id, &present)?;

        for entry in entries {
            let path = entry.path();
            let rel_path = path
                .strip_prefix(&memories_path)
                .unwrap_or(&path)
                .to_string_lossy()
                .to_string();

            let content = match std::fs::read_to_string(&path) {
                Ok(c) => c,
                Err(e) => {
                    warn!("Failed to read memory file {:?}: {}", path, e);
                    continue;
                }
            };

            let hash = format!("{:x}", md5_hash(&content));

            let existing_hash: Option<String> = self
                .db
                .query_row(
                    "SELECT hash FROM memory_files WHERE channel = ? AND user_id = ? AND path = ?",
                    [channel, user_id, &rel_path],
                    |row| row.get(0),
                )
                .ok();

            if existing_hash.as_ref() == Some(&hash) {
                debug!("Memory file {} unchanged, skipping", rel_path);
                continue;
            }

            info!("Indexing memory file: {}", rel_path);

            self.forget_file(channel, user_id, &rel_path)?;

            self.db.execute(
                "INSERT INTO memory_files (channel, user_id, path, hash, updated_at) VALUES (?, ?, ?, ?, ?)",
                rusqlite::params![
                    channel,
                    user_id,
                    &rel_path,
                    &hash,
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap()
                        .as_secs() as i64
                ],
            )?;

            let file_id = self.db.last_insert_rowid();

            let chunks = chunk_text(&content);
            let chunk_texts: Vec<String> = chunks.iter().map(|c| c.text.clone()).collect();
            let embeddings = with_embedding_model(&self.paths.models_dir, |model| {
                model
                    .embed(chunk_texts.clone(), None)
                    .context("Failed to generate embeddings")
            })?;

            for (i, (chunk, embedding)) in chunks.iter().zip(embeddings.iter()).enumerate() {
                self.db.execute(
                    "INSERT INTO memory_chunks (file_id, chunk_index, content, start_line, end_line) VALUES (?, ?, ?, ?, ?)",
                    rusqlite::params![file_id, i as i64, &chunk.text, chunk.start_line as i64, chunk.end_line as i64],
                )?;

                let chunk_id = self.db.last_insert_rowid();
                let embedding_bytes = embedding_to_bytes(embedding);

                self.db.execute(
                    "INSERT INTO memory_vectors (chunk_id, embedding) VALUES (?, ?)",
                    rusqlite::params![chunk_id, embedding_bytes],
                )?;
            }

            debug!("Indexed {} chunks from {}", chunks.len(), rel_path);
        }

        Ok(())
    }

    /// Search memories for a user
    pub fn search(
        &self,
        channel: &str,
        user_id: &str,
        query: &str,
        limit: usize,
    ) -> Result<Vec<MemorySearchResult>> {
        let query_bytes = with_embedding_model(&self.paths.models_dir, |model| {
            let embeddings = model
                .embed(vec![query.to_string()], None)
                .context("Failed to generate query embedding")?;
            Ok(embedding_to_bytes(&embeddings[0]))
        })?;

        let mut stmt = self.db.prepare(
            r#"
            SELECT
                f.path,
                c.content,
                vec_distance_cosine(v.embedding, ?) as distance
            FROM memory_vectors v
            JOIN memory_chunks c ON v.chunk_id = c.id
            JOIN memory_files f ON c.file_id = f.id
            WHERE f.channel = ? AND f.user_id = ?
            ORDER BY distance ASC
            LIMIT ?
            "#,
        )?;

        let results = stmt
            .query_map(
                rusqlite::params![query_bytes, channel, user_id, limit as i64],
                |row| {
                    Ok(MemorySearchResult {
                        path: row.get(0)?,
                        chunk: row.get(1)?,
                        score: 1.0 - row.get::<_, f32>(2)?, // Convert distance to similarity
                    })
                },
            )?
            .filter_map(|r| r.ok())
            .collect();

        Ok(results)
    }
}

/// A chunk of text with line information
struct TextChunk {
    text: String,
    start_line: usize,
    end_line: usize,
}

/// Chunk text into smaller pieces for embedding
/// Uses a simple approach: split by headers or paragraph breaks
fn chunk_text(content: &str) -> Vec<TextChunk> {
    let mut chunks = Vec::new();
    let lines: Vec<&str> = content.lines().collect();

    if lines.is_empty() {
        return chunks;
    }

    let mut current_chunk = String::new();
    let mut chunk_start = 0;
    let mut in_code_block = false;

    for (i, line) in lines.iter().enumerate() {
        if line.starts_with("```") {
            in_code_block = !in_code_block;
        }

        // Start new chunk on headers (but not in code blocks)
        let is_header = !in_code_block && line.starts_with('#');
        let should_split = is_header && !current_chunk.is_empty();

        if should_split {
            chunks.push(TextChunk {
                text: current_chunk.trim().to_string(),
                start_line: chunk_start + 1,
                end_line: i,
            });
            current_chunk = String::new();
            chunk_start = i;
        }

        if !current_chunk.is_empty() {
            current_chunk.push('\n');
        }
        current_chunk.push_str(line);

        // Also split if chunk gets too long (roughly 500 tokens ~ 2000 chars)
        if current_chunk.len() > 2000 && !in_code_block {
            chunks.push(TextChunk {
                text: current_chunk.trim().to_string(),
                start_line: chunk_start + 1,
                end_line: i + 1,
            });
            current_chunk = String::new();
            chunk_start = i + 1;
        }
    }

    // Don't forget the last chunk
    if !current_chunk.trim().is_empty() {
        chunks.push(TextChunk {
            text: current_chunk.trim().to_string(),
            start_line: chunk_start + 1,
            end_line: lines.len(),
        });
    }

    chunks
}

fn md5_hash(content: &str) -> u128 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let mut hasher = DefaultHasher::new();
    content.hash(&mut hasher);
    hasher.finish() as u128
}

/// Convert f32 embedding to bytes for sqlite-vec
fn embedding_to_bytes(embedding: &[f32]) -> Vec<u8> {
    embedding.iter().flat_map(|f| f.to_le_bytes()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Insert an indexed memory file without needing the embedding model.
    fn seed_file(index: &MemoryIndex, channel: &str, user: &str, path: &str) {
        index
            .db
            .execute(
                "INSERT INTO memory_files (channel, user_id, path, hash, updated_at) VALUES (?, ?, ?, 'h', 0)",
                [channel, user, path],
            )
            .unwrap();
        let file_id = index.db.last_insert_rowid();
        index
            .db
            .execute(
                "INSERT INTO memory_chunks (file_id, chunk_index, content, start_line, end_line) VALUES (?, 0, 'contact details', 1, 2)",
                rusqlite::params![file_id],
            )
            .unwrap();
    }

    fn indexed_paths(index: &MemoryIndex, channel: &str, user: &str) -> Vec<String> {
        index
            .db
            .prepare(
                "SELECT path FROM memory_files WHERE channel = ? AND user_id = ? ORDER BY path",
            )
            .unwrap()
            .query_map([channel, user], |row| row.get(0))
            .unwrap()
            .collect::<std::result::Result<_, _>>()
            .unwrap()
    }

    fn chunk_count(index: &MemoryIndex) -> i64 {
        index
            .db
            .query_row("SELECT COUNT(*) FROM memory_chunks", [], |row| row.get(0))
            .unwrap()
    }

    #[test]
    fn a_deleted_memory_file_is_forgotten() {
        let (_temp, paths) = crate::config::test_paths();
        let index = MemoryIndex::open(&paths).unwrap();
        seed_file(&index, "slack", "U1", "preferences.md");
        seed_file(&index, "slack", "U1", "methodology-contacts.md");

        // Only preferences.md is still on disk.
        let pruned = index
            .prune_missing("slack", "U1", &["preferences.md".to_string()])
            .unwrap();

        assert_eq!(pruned, 1);
        assert_eq!(indexed_paths(&index, "slack", "U1"), ["preferences.md"]);
        // The chunk text lives in this database, so forgetting the file has to
        // take the chunk with it or the content stays retrievable.
        assert_eq!(chunk_count(&index), 1);
    }

    #[test]
    fn a_user_who_deleted_everything_keeps_nothing_indexed() {
        // The production case: no memories directory at all. index_user_memories
        // used to return early here, leaving every chunk the user ever wrote
        // indexed and still being injected into their prompts.
        let (_temp, paths) = crate::config::test_paths();
        let mut index = MemoryIndex::open(&paths).unwrap();
        seed_file(&index, "slack", "U1", "methodology-contacts.md");
        assert!(!memories_dir(&paths, "slack", "U1").exists());

        index.index_user_memories("slack", "U1").unwrap();

        assert!(indexed_paths(&index, "slack", "U1").is_empty());
        assert_eq!(chunk_count(&index), 0);
    }

    #[test]
    fn another_users_memories_are_left_alone() {
        let (_temp, paths) = crate::config::test_paths();
        let index = MemoryIndex::open(&paths).unwrap();
        seed_file(&index, "slack", "U1", "gone.md");
        seed_file(&index, "slack", "U2", "gone.md");

        index.prune_missing("slack", "U1", &[]).unwrap();

        assert!(indexed_paths(&index, "slack", "U1").is_empty());
        assert_eq!(indexed_paths(&index, "slack", "U2"), ["gone.md"]);
    }

    #[test]
    fn test_chunk_text() {
        let content = r#"# Title

Some intro text.

## Section 1

Content for section 1.

## Section 2

Content for section 2.
"#;

        let chunks = chunk_text(content);
        assert_eq!(chunks.len(), 3);
        assert!(chunks[0].text.contains("Title"));
        assert!(chunks[1].text.contains("Section 1"));
        assert!(chunks[2].text.contains("Section 2"));
    }
}
