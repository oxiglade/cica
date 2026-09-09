//! Durable state storage for sessions and memories.
//!
//! Phase 2 provides only `FilesystemStateStore`. Later phases add
//! feature-gated S3/GCS backends behind the same `StateStore` trait.

pub mod filesystem;
#[cfg(feature = "s3")]
pub mod s3;

pub use filesystem::FilesystemStateStore;

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Result, bail};
use async_trait::async_trait;

use crate::config::{Config, Paths, StoreKind};

/// A durable store of directory trees, addressed by string keys.
///
/// Keys may contain `/` to namespace entries (e.g. `session/<id>`).
#[async_trait]
pub trait StateStore: Send + Sync {
    /// Read one small record without listing or tree traversal.
    async fn get_record(&self, key: &str) -> Result<Option<Vec<u8>>>;
    /// Unconditionally replace one small record.
    async fn put_record(&self, key: &str, bytes: &[u8]) -> Result<()>;
    /// Atomically replace a record only if its bytes still match `expected`.
    async fn compare_exchange_record(
        &self,
        _key: &str,
        _expected: Option<&[u8]>,
        _bytes: &[u8],
    ) -> Result<bool> {
        bail!("state store does not support conditional records")
    }
    /// Delete one small record; absence is successful.
    async fn delete_record(&self, key: &str) -> Result<()>;
    /// Replace `dest` with the tree stored under `key`, as a whole. On error, or when
    /// `key` is absent (`Ok(false)`), `dest` is untouched.
    async fn pull(&self, key: &str, dest: &Path) -> Result<bool>;
    /// Replace what is stored under `key` with the tree at `src`. On error the prior
    /// contents stay readable. An empty `src` stores an empty tree, which reads back
    /// as present and empty, not absent.
    async fn push(&self, src: &Path, key: &str) -> Result<()>;
    /// Diagnostic copies may finish in the background after cancellation.
    /// Canonical state must continue to use `push`.
    async fn push_archive(&self, src: &Path, key: &str) -> Result<()> {
        self.push(src, key).await
    }
    /// Remove `key` and everything under it. An absent key is not an error.
    #[allow(dead_code)]
    async fn delete(&self, key: &str) -> Result<()>;
}

#[cfg(test)]
pub(crate) mod contract {
    use std::collections::BTreeMap;
    use std::fs;
    use std::os::unix::fs::symlink;
    use std::path::Path;

    use anyhow::Result;

    use super::StateStore;

    fn write(path: &Path, contents: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    fn files(root: &Path) -> BTreeMap<String, Vec<u8>> {
        fn collect(root: &Path, dir: &Path, out: &mut BTreeMap<String, Vec<u8>>) {
            if !dir.exists() {
                return;
            }
            for entry in fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    collect(root, &path, out);
                } else {
                    out.insert(
                        path.strip_prefix(root)
                            .unwrap()
                            .to_string_lossy()
                            .replace('\\', "/"),
                        fs::read(path).unwrap(),
                    );
                }
            }
        }
        let mut out = BTreeMap::new();
        collect(root, root, &mut out);
        out
    }

    pub async fn conditional_record_rejects_stale_writers(
        store: &dyn StateStore,
        key: &str,
    ) -> Result<()> {
        assert!(store.compare_exchange_record(key, None, b"first").await?);
        assert!(
            !store
                .compare_exchange_record(key, None, b"duplicate")
                .await?
        );
        assert!(
            !store
                .compare_exchange_record(key, Some(b"wrong"), b"lost")
                .await?
        );
        assert!(
            store
                .compare_exchange_record(key, Some(b"first"), b"second")
                .await?
        );
        assert_eq!(
            store.get_record(key).await?.as_deref(),
            Some(b"second".as_slice())
        );
        Ok(())
    }

    pub async fn push_then_pull_round_trips(store: &dyn StateStore, key: &str) -> Result<()> {
        let src = tempfile::tempdir()?;
        write(&src.path().join("a.txt"), "alpha");
        write(&src.path().join("sub/b.txt"), "beta");
        store.push(src.path(), key).await?;
        let dest_parent = tempfile::tempdir()?;
        let dest = dest_parent.path().join("dest");
        assert!(store.pull(key, &dest).await?);
        assert_eq!(files(&dest), files(src.path()));
        Ok(())
    }

    pub async fn push_smaller_tree_removes_stale_files(
        store: &dyn StateStore,
        key: &str,
    ) -> Result<()> {
        let first = tempfile::tempdir()?;
        write(&first.path().join("a.txt"), "a");
        write(&first.path().join("sub/b.txt"), "b");
        store.push(first.path(), key).await?;
        let second = tempfile::tempdir()?;
        write(&second.path().join("new"), "new");
        store.push(second.path(), key).await?;
        let parent = tempfile::tempdir()?;
        let dest = parent.path().join("dest");
        assert!(store.pull(key, &dest).await?);
        assert_eq!(files(&dest), files(second.path()));
        Ok(())
    }

    pub async fn push_empty_tree_reads_present_and_empty(
        store: &dyn StateStore,
        key: &str,
    ) -> Result<()> {
        let src = tempfile::tempdir()?;
        store.push(src.path(), key).await?;
        let parent = tempfile::tempdir()?;
        let dest = parent.path().join("dest");
        assert!(store.pull(key, &dest).await?);
        assert!(files(&dest).is_empty());
        Ok(())
    }

    pub async fn pull_absent_key_leaves_dest_untouched(
        store: &dyn StateStore,
        key: &str,
    ) -> Result<()> {
        let parent = tempfile::tempdir()?;
        let dest = parent.path().join("dest");
        write(&dest.join("seed.txt"), "seed");
        assert!(!store.pull(key, &dest).await?);
        assert_eq!(fs::read_to_string(dest.join("seed.txt"))?, "seed");
        Ok(())
    }

    pub async fn pull_replaces_dest_whole(store: &dyn StateStore, key: &str) -> Result<()> {
        let src = tempfile::tempdir()?;
        write(&src.path().join("stored.txt"), "stored");
        store.push(src.path(), key).await?;
        let parent = tempfile::tempdir()?;
        let dest = parent.path().join("dest");
        write(&dest.join("stray.txt"), "stray");
        assert!(store.pull(key, &dest).await?);
        assert_eq!(files(&dest), files(src.path()));
        Ok(())
    }

    pub async fn push_failure_leaves_prior_contents(
        store: &dyn StateStore,
        key: &str,
    ) -> Result<()> {
        let first = tempfile::tempdir()?;
        write(&first.path().join("good.txt"), "good");
        store.push(first.path(), key).await?;
        let broken = tempfile::tempdir()?;
        write(&broken.path().join("a.txt"), "new");
        symlink("./missing", broken.path().join("zz-broken"))?;
        assert!(store.push(broken.path(), key).await.is_err());
        let parent = tempfile::tempdir()?;
        let dest = parent.path().join("dest");
        assert!(store.pull(key, &dest).await?);
        assert_eq!(files(&dest), files(first.path()));
        Ok(())
    }

    pub async fn delete_makes_key_absent(store: &dyn StateStore, key: &str) -> Result<()> {
        let src = tempfile::tempdir()?;
        write(&src.path().join("a.txt"), "a");
        store.push(src.path(), key).await?;
        store.delete(key).await?;
        let parent = tempfile::tempdir()?;
        assert!(!store.pull(key, &parent.path().join("dest")).await?);
        Ok(())
    }

    pub async fn delete_absent_key_is_ok(store: &dyn StateStore, key: &str) -> Result<()> {
        store.delete(key).await
    }

    pub async fn record_round_trip(store: &dyn StateStore, key: &str) -> Result<()> {
        store.put_record(key, b"record bytes").await?;
        assert_eq!(store.get_record(key).await?, Some(b"record bytes".to_vec()));
        Ok(())
    }

    pub async fn record_absent_is_none(store: &dyn StateStore, key: &str) -> Result<()> {
        assert_eq!(store.get_record(key).await?, None);
        Ok(())
    }

    pub async fn record_overwrite_replaces(store: &dyn StateStore, key: &str) -> Result<()> {
        store.put_record(key, b"first").await?;
        store.put_record(key, b"second").await?;
        assert_eq!(store.get_record(key).await?, Some(b"second".to_vec()));
        Ok(())
    }

    pub async fn record_delete_makes_absent(store: &dyn StateStore, key: &str) -> Result<()> {
        store.put_record(key, b"value").await?;
        store.delete_record(key).await?;
        assert_eq!(store.get_record(key).await?, None);
        Ok(())
    }

    pub async fn record_delete_absent_is_ok(store: &dyn StateStore, key: &str) -> Result<()> {
        store.delete_record(key).await?;
        assert_eq!(store.get_record(key).await?, None);
        Ok(())
    }
}

/// The filesystem path of the state store: `[deployment].state_path` if set,
/// else `internal/state-store`. Shared so the `FilesystemStateStore` and the
/// Docker host-mount always agree on the same directory.
pub fn resolved_state_path(config: &Config, paths: &Paths) -> PathBuf {
    match &config.deployment.state_path {
        Some(p) => PathBuf::from(p),
        None => paths.internal_dir.join("state-store"),
    }
}

/// Build the configured store, or `None` if deployment.store is unset.
pub fn default_store(config: &Config, paths: &Paths) -> Result<Option<Arc<dyn StateStore>>> {
    match config.deployment.store {
        None => Ok(None),
        Some(StoreKind::Filesystem) => Ok(Some(Arc::new(FilesystemStateStore::new(
            resolved_state_path(config, paths),
        )))),
        Some(StoreKind::S3) => {
            #[cfg(feature = "s3")]
            {
                let s3 = config.deployment.s3.clone().ok_or_else(|| {
                    anyhow::anyhow!("`store = s3` requires a [deployment.s3] section")
                })?;
                Ok(Some(Arc::new(s3::S3StateStore::new(s3))))
            }
            #[cfg(not(feature = "s3"))]
            {
                anyhow::bail!("`store = s3` requires the binary to be built with `--features s3`")
            }
        }
    }
}

/// Join `key` onto `root`, rejecting `..` and normalizing each segment to
/// path-safe characters. Prevents traversal outside `root`.
pub(crate) fn safe_join(root: &Path, key: &str) -> Result<PathBuf> {
    let mut out = root.to_path_buf();
    for segment in key.split('/') {
        if segment.is_empty() || segment == "." {
            continue;
        }
        if segment == ".." {
            bail!("invalid state key segment: ..");
        }
        let safe: String = segment
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        out.push(safe);
    }
    Ok(out)
}

/// Remove all entries inside `dir` (leaving `dir` itself), creating it if absent.
pub(crate) fn clear_dir(dir: &Path) -> Result<()> {
    if dir.exists() {
        for entry in fs::read_dir(dir)? {
            let path = entry?.path();
            if path.is_dir() {
                fs::remove_dir_all(&path)?;
            } else {
                fs::remove_file(&path)?;
            }
        }
    } else {
        fs::create_dir_all(dir)?;
    }
    Ok(())
}

/// Copy a single path (file or directory) from `src` to `dst`.
pub(crate) fn copy_path(src: &Path, dst: &Path) -> Result<()> {
    if src.is_dir() {
        copy_dir_all(src, dst)
    } else {
        if let Some(parent) = dst.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(src, dst)?;
        Ok(())
    }
}

/// Recursively copy the contents of directory `src` into `dst`.
pub(crate) fn copy_dir_all(src: &Path, dst: &Path) -> Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if from.is_dir() {
            copy_dir_all(&from, &to)?;
        } else {
            fs::copy(&from, &to)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_join_rejects_traversal() {
        let root = Path::new("/tmp/store");
        assert!(safe_join(root, "../escape").is_err());
        let ok = safe_join(root, "session/abc-123").unwrap();
        assert_eq!(ok, Path::new("/tmp/store/session/abc-123"));
    }

    #[test]
    fn safe_join_sanitizes_segments() {
        let root = Path::new("/tmp/store");
        let p = safe_join(root, "mem/telegram:42").unwrap();
        assert_eq!(p, Path::new("/tmp/store/mem/telegram_42"));
    }

    #[test]
    fn default_store_none_when_unconfigured() {
        let (_temp, paths) = crate::config::test_paths();
        let cfg = Config::default();
        assert!(default_store(&cfg, &paths).unwrap().is_none());
    }

    #[test]
    fn default_store_some_for_filesystem() {
        let (_temp, paths) = crate::config::test_paths();
        let mut cfg = Config::default();
        cfg.deployment.store = Some(StoreKind::Filesystem);
        cfg.deployment.state_path = Some("/tmp/cica-state-test".to_string());
        assert!(default_store(&cfg, &paths).unwrap().is_some());
    }

    #[cfg(not(feature = "s3"))]
    #[test]
    fn s3_store_requires_feature() {
        let (_temp, paths) = crate::config::test_paths();
        let mut cfg = Config::default();
        cfg.deployment.store = Some(StoreKind::S3);
        assert!(default_store(&cfg, &paths).is_err());
    }

    #[cfg(feature = "s3")]
    #[test]
    fn s3_store_built_lazily_when_feature_on() {
        let (_temp, paths) = crate::config::test_paths();
        use crate::config::S3Config;
        let mut cfg = Config::default();
        cfg.deployment.store = Some(StoreKind::S3);
        cfg.deployment.s3 = Some(S3Config {
            bucket: "b".into(),
            ..Default::default()
        });
        // Lazy client: constructing the store does not connect, so this is Ok without AWS.
        assert!(default_store(&cfg, &paths).unwrap().is_some());
    }

    #[cfg(feature = "s3")]
    #[test]
    fn s3_store_without_section_errors() {
        let (_temp, paths) = crate::config::test_paths();
        let mut cfg = Config::default();
        cfg.deployment.store = Some(StoreKind::S3);
        cfg.deployment.s3 = None;
        assert!(default_store(&cfg, &paths).is_err());
    }
}
