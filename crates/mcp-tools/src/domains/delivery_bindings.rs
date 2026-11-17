//! Evidence from successful reports and validated claims. Only identifiers,
//! digests and timestamps reach disk; never credentials, tokens or messages.
use serde::{Deserialize, Serialize};
use std::{fs, io, path::PathBuf};
use uuid::Uuid;

#[derive(Clone)]
pub(super) struct Owner {
    pub key: String,
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
pub(super) struct FileLock(fs::File);

impl FileLock {
    fn acquire(file: fs::File) -> io::Result<Self> {
        file.try_lock()?;
        Ok(Self(file))
    }
}

impl Drop for FileLock {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}

impl Owner {
    // One lock per API/credential/workspace, including across processes.
    // Rename under a separate lock so a crash never leaves a partial book.
    fn book<T>(&self, f: impl FnOnce(&mut Vec<Binding>) -> (T, bool)) -> io::Result<T> {
        let dir = self.store.parent().ok_or(io::ErrorKind::InvalidInput)?;
        let mut builder = fs::DirBuilder::new();
        builder.recursive(true);
        let mut options = fs::OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
            builder.mode(0o700);
            options.mode(0o600);
        }
        builder.create(dir)?;
        let _lock = FileLock::acquire(options.open(self.store.with_extension("lock"))?)?;
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

    /// A bounded set of nonblocking OS locks; drop releases the reservation
    /// even on cancellation or process exit. Never unlink a live lock file.
    pub fn reserve(&self, id: Uuid) -> Option<FileLock> {
        let mut dir = fs::DirBuilder::new();
        dir.recursive(true);
        let mut options = fs::OpenOptions::new();
        options.create(true).truncate(false).read(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
            dir.mode(0o700);
            options.mode(0o600);
        }
        let root = self.store.parent()?;
        dir.create(root).ok()?;
        let file = options
            .open(
                self.store
                    .with_extension(format!("claim-{:02x}.lock", id.as_bytes()[0])),
            )
            .ok()?;
        FileLock::acquire(file).ok()
    }

    pub fn bind(
        &self,
        run: Uuid,
        delivery: Option<Uuid>,
        generation: usize,
        attempt: Option<Uuid>,
    ) -> bool {
        self.book(|entries| {
            entries.retain(|e| {
                !(e.delivery == delivery
                    && (delivery.is_some() || (e.run == run && e.owner == self.key)))
            });
            entries.push(Binding {
                owner: self.key.clone(),
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
        self.book(|entries| {
            (
                entries
                    .iter()
                    .any(|e| e.run == run && e.delivery.is_none() && e.owner == self.key),
                false,
            )
        })
        .unwrap_or(false)
    }

    pub fn already_claimed(&self, id: Uuid, generation: usize) -> bool {
        self.book(|entries| {
            (
                entries
                    .iter()
                    .any(|e| e.delivery == Some(id) && e.generation >= generation),
                false,
            )
        })
        .unwrap_or(true)
    }

    pub fn claim(&self, id: Uuid) -> Option<(Uuid, usize, Uuid)> {
        self.book(|entries| {
            (
                entries
                    .iter()
                    .rev()
                    .find(|e| e.delivery == Some(id) && e.owner == self.key)
                    .and_then(|e| Some((e.run, e.generation, e.attempt?))),
                false,
            )
        })
        .ok()
        .flatten()
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
    fn review7_an_inherited_descriptor_does_not_extend_a_finished_reservation() {
        let dir = tempfile::tempdir().unwrap();
        let owner = Owner {
            key: "test owner".into(),
            workspace: Uuid::new_v4(),
            store: dir.path().join("bindings"),
        };
        let id = Uuid::new_v4();
        let held = owner.reserve(id).unwrap();
        // A duplicate references the same open file description, just as a
        // concurrent child between fork and exec does.
        let inherited = held.0.try_clone().unwrap();
        assert!(owner.reserve(id).is_none(), "still held by the worker");
        drop(held);
        assert!(owner.reserve(id).is_some(), "the worker has finished");
        drop(inherited);
    }
}
