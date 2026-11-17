//! Evidence from successful reports and validated claims. Only identifiers,
//! digests and timestamps reach disk; never credentials, tokens or messages.
use serde::{Deserialize, Serialize};
use std::{
    fs, io,
    path::PathBuf,
    time::{Duration, Instant},
};
use uuid::Uuid;

#[derive(Debug, Clone, Hash, Eq, PartialEq)]
pub(super) struct OwnerKey {
    pub namespace: String, // API and credential digest; shared quota across sessions and workspaces
    pub digest: String,    // unchanged on-disk owner identity
}

#[cfg(test)]
impl From<&str> for OwnerKey {
    fn from(s: &str) -> Self {
        Self {
            namespace: "test".into(),
            digest: s.into(),
        }
    }
}

#[derive(Clone)]
pub(super) struct Owner {
    pub key: OwnerKey,
    pub workspace: Uuid,
    pub store: PathBuf,
}

#[derive(Serialize, Deserialize)]
pub(super) struct Binding {
    owner: String,
    run: Uuid,
    delivery: Option<Uuid>,
    at: i64,
    pub generation: usize,
    #[serde(default)]
    attempt: Option<Uuid>,
}

pub(super) const CLAIM_SECONDS: i64 = 3600;

/// Release the lock when its owner finishes, even if a fork still holds
/// another descriptor for the same open file description.
pub(super) struct FileLock(fs::File, PathBuf);

impl FileLock {
    fn acquire(path: PathBuf) -> io::Result<Self> {
        let deadline = Instant::now() + Duration::from_millis(200);
        loop {
            match Self::try_acquire(&path) {
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::NotFound
                    ) && Instant::now() < deadline =>
                {
                    std::thread::sleep(Duration::from_millis(5))
                }
                result => return result,
            }
        }
    }

    fn try_acquire(path: &std::path::Path) -> io::Result<Self> {
        let mut options = fs::OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options.open(path)?;
        file.try_lock()?;
        // A previous holder may have unlinked this inode between open and
        // try_lock. Only the holder of the currently named file may proceed.
        if same_file::Handle::from_file(file.try_clone()?)? != same_file::Handle::from_path(path)? {
            return Err(io::ErrorKind::WouldBlock.into());
        }
        Ok(Self(file, path.to_owned()))
    }
}

impl Drop for FileLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.1);
        let _ = self.0.unlock();
    }
}

impl Owner {
    // Readers see either complete revision of the atomically replaced book.
    // In particular a lookup of unknown evidence never creates a directory.
    fn entries(&self) -> io::Result<Vec<Binding>> {
        let mut entries: Vec<Binding> = match fs::read(&self.store) {
            Ok(bytes) => serde_json::from_slice(&bytes)?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(e),
        };
        let now = chrono::Utc::now().timestamp();
        entries.retain(|e| {
            let age = now.saturating_sub(e.at);
            (0..if e.delivery.is_some() {
                CLAIM_SECONDS
            } else {
                7 * 86400
            })
                .contains(&age)
        });
        Ok(entries)
    }

    fn book<T>(&self, f: impl FnOnce(&mut Vec<Binding>) -> (T, bool)) -> io::Result<T> {
        let dir = self.store.parent().ok_or(io::ErrorKind::InvalidInput)?;
        let mut builder = fs::DirBuilder::new();
        builder.recursive(true);
        let mut options = fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
            builder.mode(0o700);
            options.mode(0o600);
        }
        builder.create(dir)?;
        let _lock = FileLock::acquire(self.store.with_extension("lock"))?;
        let mut entries = self.entries()?;
        let (answer, changed) = f(&mut entries);
        if changed {
            if entries.len() > 1024 {
                entries.drain(..entries.len() - 1024);
            }
            let tmp = self.store.with_extension("tmp");
            serde_json::to_writer(options.truncate(true).open(&tmp)?, &entries)?;
            fs::rename(tmp, &self.store)?;
        }
        Ok(answer)
    }

    /// The 256 reservation buckets are shared by all writers to this book.
    /// Wait off the async runtime; dropping the guard removes the lock file.
    pub async fn reserve(&self, id: Uuid) -> io::Result<FileLock> {
        if !self.store.is_file() {
            return Err(io::ErrorKind::NotFound.into());
        }
        let path = self
            .store
            .with_extension(format!("claim-{:02x}.lock", id.as_bytes()[0]));
        tokio::task::spawn_blocking(move || FileLock::acquire(path)).await?
    }

    pub async fn bind(
        &self,
        run: Uuid,
        delivery: Option<Uuid>,
        generation: usize,
        attempt: Option<Uuid>,
    ) -> bool {
        let owner = self.clone();
        tokio::task::spawn_blocking(move || owner.bind_blocking(run, delivery, generation, attempt))
            .await
            .unwrap_or(false)
    }

    fn bind_blocking(
        &self,
        run: Uuid,
        delivery: Option<Uuid>,
        generation: usize,
        attempt: Option<Uuid>,
    ) -> bool {
        self.book(|entries| {
            entries.retain(|e| {
                !(e.delivery == delivery
                    && (delivery.is_some() || (e.run == run && e.owner == self.key.digest)))
            });
            entries.push(Binding {
                owner: self.key.digest.clone(),
                run,
                delivery,
                generation,
                attempt,
                at: chrono::Utc::now().timestamp(),
            });
            ((), true)
        })
        .is_ok()
    }

    pub fn reported(&self, run: Uuid) -> bool {
        self.entries().is_ok_and(|entries| {
            entries
                .iter()
                .any(|e| e.run == run && e.delivery.is_none() && e.owner == self.key.digest)
        })
    }

    pub fn already_claimed(&self, id: Uuid, generation: usize) -> bool {
        self.entries()
            .map(|entries| {
                entries
                    .iter()
                    .any(|e| e.delivery == Some(id) && e.generation >= generation)
            })
            .unwrap_or(true)
    }

    pub fn claim(&self, id: Uuid) -> Option<(Uuid, usize, Uuid)> {
        self.entries()
            .ok()?
            .into_iter()
            .rev()
            .find(|e| e.delivery == Some(id) && e.owner == self.key.digest)
            .and_then(|e| Some((e.run, e.generation, e.attempt?)))
    }
}

#[cfg(not(test))]
pub(super) fn default_root() -> Option<PathBuf> {
    dirs::state_dir()
        .or_else(dirs::data_local_dir)
        .map(|p| p.join("contextstream/deliveries"))
}
#[cfg(test)]
pub(super) fn default_root() -> Option<PathBuf> {
    static ROOT: std::sync::LazyLock<tempfile::TempDir> =
        std::sync::LazyLock::new(|| tempfile::tempdir().unwrap());
    Some(ROOT.path().to_path_buf())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn round2_contended_binding_waits_for_its_turn() {
        let dir = tempfile::tempdir().unwrap();
        let owner = Owner {
            key: "owner".into(),
            workspace: Uuid::new_v4(),
            store: dir.path().join("book"),
        };
        let lock = FileLock::acquire(owner.store.with_extension("lock")).unwrap();
        let (started, ready) = std::sync::mpsc::sync_channel(0);
        let writer = std::thread::spawn(move || {
            started.send(()).unwrap();
            owner.bind_blocking(Uuid::new_v4(), None, 0, None)
        });
        ready.recv().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        drop(lock);
        assert!(
            writer.join().unwrap(),
            "a concurrent writer should only delay the bind"
        );
    }

    #[tokio::test]
    async fn review7_an_inherited_descriptor_does_not_extend_a_finished_reservation() {
        let dir = tempfile::tempdir().unwrap();
        let owner = Owner {
            key: "test owner".into(),
            workspace: Uuid::new_v4(),
            store: dir.path().join("bindings"),
        };
        let id = Uuid::new_v4();
        assert!(owner.bind(Uuid::new_v4(), None, 0, None).await);
        let held = owner.reserve(id).await.unwrap();
        // A duplicate references the same open file description, just as a
        // concurrent child between fork and exec does.
        let inherited = held.0.try_clone().unwrap();
        assert!(owner.reserve(id).await.is_err(), "still held by the worker");
        drop(held);
        assert!(owner.reserve(id).await.is_ok(), "the worker has finished");
        drop(inherited);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn round3_contended_binding_does_not_block_the_runtime() {
        let dir = tempfile::tempdir().unwrap();
        let owner = Owner {
            key: "test".into(),
            workspace: Uuid::new_v4(),
            store: dir.path().join("book"),
        };
        let held = FileLock::acquire(owner.store.with_extension("lock")).unwrap();
        let release = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            drop(held);
        });
        assert!(
            owner.bind(Uuid::new_v4(), None, 0, None).await,
            "timer must release the lock before the writer times out"
        );
        release.await.unwrap();
    }
}
