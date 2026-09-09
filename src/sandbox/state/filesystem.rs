//! Filesystem-backed `StateStore` (Phase 2; also useful for homelab/dev).

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Result;
use async_trait::async_trait;

use crate::atomic::Staging;
use crate::sandbox::state::{StateStore, copy_dir_all, safe_join};

/// Stores each key as a directory tree under `root`.
pub struct FilesystemStateStore {
    root: PathBuf,
}

impl FilesystemStateStore {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }
}

#[async_trait]
impl StateStore for FilesystemStateStore {
    async fn get_record(&self, key: &str) -> Result<Option<Vec<u8>>> {
        let path = safe_join(&self.root, key)?;
        match fs::read(path) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    async fn put_record(&self, key: &str, bytes: &[u8]) -> Result<()> {
        crate::atomic::write(&safe_join(&self.root, key)?, bytes)?;
        Ok(())
    }

    async fn compare_exchange_record(
        &self,
        key: &str,
        expected: Option<&[u8]>,
        bytes: &[u8],
    ) -> Result<bool> {
        let path = safe_join(&self.root, key)?;
        fs::create_dir_all(path.parent().unwrap())?;
        let lock_path = path.with_extension("lock");
        let file = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(lock_path)?;
        match file.try_lock() {
            Ok(()) => {}
            Err(std::fs::TryLockError::WouldBlock) => return Ok(false),
            Err(error) => return Err(error.into()),
        }
        let current = self.get_record(key).await?;
        if current.as_deref() != expected {
            return Ok(false);
        }
        crate::atomic::write(&path, bytes)?;
        Ok(true)
    }

    async fn delete_record(&self, key: &str) -> Result<()> {
        match fs::remove_file(safe_join(&self.root, key)?) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            result => Ok(result?),
        }
    }

    async fn pull(&self, key: &str, dest: &Path) -> Result<bool> {
        let src = safe_join(&self.root, key)?;
        if !src.exists() {
            return Ok(false);
        }
        let staging = Staging::beside(dest)?;
        copy_dir_all(&src, staging.path())?;
        staging.commit()?;
        Ok(true)
    }

    async fn push(&self, src: &Path, key: &str) -> Result<()> {
        let dst = safe_join(&self.root, key)?;
        push_tree(src, &dst)
    }

    async fn push_archive(&self, src: &Path, key: &str) -> Result<()> {
        let src = src.to_path_buf();
        let dst = safe_join(&self.root, key)?;
        run_archive_copy(move || push_tree(&src, &dst)).await
    }

    async fn delete(&self, key: &str) -> Result<()> {
        let dst = safe_join(&self.root, key)?;
        match fs::remove_dir_all(dst) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            result => Ok(result?),
        }
    }
}

fn push_tree(src: &Path, dst: &Path) -> Result<()> {
    let staging = Staging::beside(dst)?;
    copy_dir_all(src, staging.path())?;
    staging.commit()?;
    Ok(())
}

async fn run_archive_copy(copy: impl FnOnce() -> Result<()> + Send + 'static) -> Result<()> {
    tokio::task::spawn_blocking(copy).await?
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::state::contract;

    #[tokio::test]
    async fn non_yielding_archive_copy_cannot_block_its_deadline() {
        let started = std::time::Instant::now();
        let result = tokio::time::timeout(
            std::time::Duration::from_millis(10),
            run_archive_copy(|| {
                std::thread::sleep(std::time::Duration::from_millis(200));
                Ok(())
            }),
        )
        .await;
        assert!(
            result.is_err(),
            "synchronous copy prevented the timeout from firing"
        );
        assert!(started.elapsed() < std::time::Duration::from_millis(150));
    }

    fn write(path: &Path, contents: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    #[tokio::test]
    async fn push_then_pull_round_trips() {
        let root = tempfile::tempdir().unwrap();
        let store = FilesystemStateStore::new(root.path().to_path_buf());
        contract::push_then_pull_round_trips(&store, "round-trip")
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn push_smaller_tree_removes_stale_files() {
        let root = tempfile::tempdir().unwrap();
        let store = FilesystemStateStore::new(root.path().to_path_buf());
        contract::push_smaller_tree_removes_stale_files(&store, "smaller")
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn push_empty_tree_reads_present_and_empty() {
        let root = tempfile::tempdir().unwrap();
        let store = FilesystemStateStore::new(root.path().to_path_buf());
        contract::push_empty_tree_reads_present_and_empty(&store, "empty")
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn pull_absent_key_leaves_dest_untouched() {
        let root = tempfile::tempdir().unwrap();
        let store = FilesystemStateStore::new(root.path().to_path_buf());
        contract::pull_absent_key_leaves_dest_untouched(&store, "absent")
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn pull_replaces_dest_whole() {
        let root = tempfile::tempdir().unwrap();
        let store = FilesystemStateStore::new(root.path().to_path_buf());
        contract::pull_replaces_dest_whole(&store, "replace")
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn push_failure_leaves_prior_contents() {
        let root = tempfile::tempdir().unwrap();
        let store = FilesystemStateStore::new(root.path().to_path_buf());
        contract::push_failure_leaves_prior_contents(&store, "push-failure")
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn delete_makes_key_absent() {
        let root = tempfile::tempdir().unwrap();
        let store = FilesystemStateStore::new(root.path().to_path_buf());
        contract::delete_makes_key_absent(&store, "delete")
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn delete_absent_key_is_ok() {
        let root = tempfile::tempdir().unwrap();
        let store = FilesystemStateStore::new(root.path().to_path_buf());
        contract::delete_absent_key_is_ok(&store, "absent-delete")
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn pull_failure_leaves_prior_local_contents() {
        let root = tempfile::tempdir().unwrap();
        let store = FilesystemStateStore::new(root.path().to_path_buf());
        let first = tempfile::tempdir().unwrap();
        write(&first.path().join("good.txt"), "good");
        store.push(first.path(), "broken").await.unwrap();
        let dest_parent = tempfile::tempdir().unwrap();
        let dest = dest_parent.path().join("dest");
        store.pull("broken", &dest).await.unwrap();
        write(&root.path().join("broken/new.txt"), "new");
        std::os::unix::fs::symlink("./missing", root.path().join("broken/zz-broken")).unwrap();

        assert!(store.pull("broken", &dest).await.is_err());
        assert_eq!(fs::read_to_string(dest.join("good.txt")).unwrap(), "good");
        assert!(!dest.join("new.txt").exists());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_record_claims_have_exactly_one_winner() {
        let root = tempfile::tempdir().unwrap();
        let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(16));
        let mut tasks = tokio::task::JoinSet::new();
        for _ in 0..16 {
            let store = FilesystemStateStore::new(root.path().to_path_buf());
            let barrier = barrier.clone();
            tasks.spawn(async move {
                barrier.wait().await;
                store
                    .compare_exchange_record("claim", None, b"claimed")
                    .await
                    .unwrap()
            });
        }
        let mut winners = 0;
        while let Some(result) = tasks.join_next().await {
            winners += usize::from(result.unwrap());
        }
        assert_eq!(winners, 1);
    }

    macro_rules! record_contract_test {
        ($name:ident) => {
            #[tokio::test]
            async fn $name() {
                let root = tempfile::tempdir().unwrap();
                let store = FilesystemStateStore::new(root.path().to_path_buf());
                contract::$name(&store, concat!("records/", stringify!($name)))
                    .await
                    .unwrap();
            }
        };
    }

    record_contract_test!(conditional_record_rejects_stale_writers);
    record_contract_test!(record_round_trip);
    record_contract_test!(record_absent_is_none);
    record_contract_test!(record_overwrite_replaces);
    record_contract_test!(record_delete_makes_absent);
    record_contract_test!(record_delete_absent_is_ok);
}
