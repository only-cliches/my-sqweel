//! Atomic whole-database images for the serialized development database writer.

use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Write};
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize, de::DeserializeOwned};

use super::{FileLockGuard, acquire_file_lock};

const IMAGE_FILE: &str = "transaction-image.json";
const IMAGE_VERSION: u32 = 1;

#[cfg(test)]
#[derive(Clone, Copy, PartialEq, Eq)]
enum CommitPhase {
    BeforeRename,
    AfterRename,
    AfterDirectorySync,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ImageEnvelope {
    version: u32,
    checksum: u32,
    // Checksum the exact serialized bytes, independent of map iteration order.
    payload: String,
}

pub struct TransactionImageStore {
    directory: PathBuf,
    _lock: FileLockGuard,
    writer: Mutex<()>,
    poisoned: AtomicBool,
    #[cfg(test)]
    commit_hook: Mutex<Option<fn(&TransactionImageStore, CommitPhase) -> Result<()>>>,
}

struct PendingRename<'a> {
    poisoned: &'a AtomicBool,
    armed: bool,
}

impl Drop for PendingRename<'_> {
    fn drop(&mut self) {
        if self.armed {
            self.poisoned.store(true, Ordering::Release);
        }
    }
}

impl TransactionImageStore {
    pub fn open(data_dir: &str) -> Result<Self> {
        // Share Lux's lock: an older persistence owner must not write alongside us.
        let lock = acquire_file_lock(data_dir)?;
        Ok(Self {
            directory: lock.data_dir.clone(),
            _lock: lock,
            writer: Mutex::new(()),
            poisoned: AtomicBool::new(false),
            #[cfg(test)]
            commit_hook: Mutex::new(None),
        })
    }

    pub fn is_poisoned(&self) -> bool {
        self.poisoned.load(Ordering::Acquire)
    }

    fn ensure_healthy(&self) -> Result<()> {
        if self.is_poisoned() {
            bail!("database image commit outcome is uncertain; close and reopen the database");
        }
        Ok(())
    }

    #[cfg(test)]
    fn reach_phase(&self, phase: CommitPhase) -> Result<()> {
        if let Some(hook) = *self.commit_hook.lock().unwrap() {
            hook(self, phase)?;
        }
        Ok(())
    }

    pub fn load<T: DeserializeOwned>(&self) -> Result<Option<T>> {
        let _writer = self
            .writer
            .lock()
            .map_err(|_| anyhow::anyhow!("database image writer poisoned"))?;
        self.ensure_healthy()?;
        let bytes = match fs::read(self.directory.join(IMAGE_FILE)) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error).context("read committed database image"),
        };
        let envelope: ImageEnvelope =
            serde_json::from_slice(&bytes).context("invalid database image envelope")?;
        if envelope.version != IMAGE_VERSION {
            bail!("unsupported database image version {}", envelope.version);
        }
        if crc32fast::hash(envelope.payload.as_bytes()) != envelope.checksum {
            bail!("database image checksum mismatch");
        }
        serde_json::from_str(&envelope.payload)
            .context("invalid database image payload")
            .map(Some)
    }

    /// Persist before publishing the matching in-memory state or acknowledging commit.
    /// A poisoned result requires closing the database: the new image may be committed.
    pub fn commit<T: Serialize>(&self, value: &T) -> Result<()> {
        let _writer = self
            .writer
            .lock()
            .map_err(|_| anyhow::anyhow!("database image writer poisoned"))?;
        self.ensure_healthy()?;
        // ponytail: whole-database images bound write throughput; use a transactional
        // embedded store when development datasets outgrow full-image commits.
        let payload = serde_json::to_string(value).context("serialize database image")?;
        let bytes = serde_json::to_vec(&ImageEnvelope {
            version: IMAGE_VERSION,
            checksum: crc32fast::hash(payload.as_bytes()),
            payload,
        })?;
        // A crash can leave this file behind. Recovery reads only IMAGE_FILE.
        let temporary = self
            .directory
            .join(format!(".{IMAGE_FILE}.{}.tmp", uuid::Uuid::new_v4()));
        let result = (|| -> Result<()> {
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            // Database images include credential verifiers; restrict access at
            // creation so the temporary image is never exposed to other users.
            #[cfg(unix)]
            options.mode(0o600);
            let mut file = options
                .open(&temporary)
                .context("create temporary database image")?;
            file.write_all(&bytes).context("write database image")?;
            file.sync_all().context("sync database image")?;
            #[cfg(test)]
            self.reach_phase(CommitPhase::BeforeRename)?;
            fs::rename(&temporary, self.directory.join(IMAGE_FILE))
                .context("replace committed database image")?;
            // Rename has made the new state visible. If the directory sync fails,
            // returning an error cannot promise rollback to the previous image.
            // Poison only if this commit fails or unwinds. Readers may continue
            // using the previous committed state while a healthy writer syncs.
            let mut pending_rename = PendingRename {
                poisoned: &self.poisoned,
                armed: true,
            };
            #[cfg(test)]
            self.reach_phase(CommitPhase::AfterRename)?;
            File::open(&self.directory)?
                .sync_all()
                .context("sync database image directory")?;
            #[cfg(test)]
            self.reach_phase(CommitPhase::AfterDirectorySync)?;
            pending_rename.armed = false;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            Self(std::env::temp_dir().join(format!("sqweel-image-test-{}", uuid::Uuid::new_v4())))
        }

        fn open(&self) -> TransactionImageStore {
            TransactionImageStore::open(self.0.to_str().unwrap()).unwrap()
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn committed_image_reopens_and_excludes_other_owners() {
        let directory = TestDirectory::new();
        let store = directory.open();
        assert_eq!(store.load::<serde_json::Value>().unwrap(), None);
        store.commit(&json!({"rows": [1, 2]})).unwrap();
        store.commit(&json!({"rows": [3]})).unwrap();
        assert!(TransactionImageStore::open(directory.0.to_str().unwrap()).is_err());
        assert!(super::super::LuxRedisStore::open(Some(directory.0.to_str().unwrap())).is_err());
        drop(store);
        assert_eq!(
            directory.open().load::<serde_json::Value>().unwrap(),
            Some(json!({"rows": [3]}))
        );
    }

    #[test]
    fn dropping_store_unlocks_even_when_another_descriptor_remains_open() {
        let directory = TestDirectory::new();
        let store = directory.open();
        let inherited_descriptor = store._lock.file.try_clone().unwrap();
        store.commit(&json!({"rows": [1]})).unwrap();
        drop(store);
        let reopened = directory.open();
        assert_eq!(
            reopened.load::<serde_json::Value>().unwrap(),
            Some(json!({"rows": [1]}))
        );
        assert!(TransactionImageStore::open(directory.0.to_str().unwrap()).is_err());
        drop(inherited_descriptor);
        assert!(TransactionImageStore::open(directory.0.to_str().unwrap()).is_err());
    }

    #[test]
    fn corruption_and_unknown_versions_fail_closed() {
        let directory = TestDirectory::new();
        let store = directory.open();
        store.commit(&json!({"rows": [1]})).unwrap();
        let path = directory.0.join(IMAGE_FILE);
        let original = fs::read(&path).unwrap();
        let mut envelope: ImageEnvelope = serde_json::from_slice(&original).unwrap();
        envelope.payload = "{\"rows\":[2]}".into();
        fs::write(&path, serde_json::to_vec(&envelope).unwrap()).unwrap();
        assert!(
            store
                .load::<serde_json::Value>()
                .unwrap_err()
                .to_string()
                .contains("checksum")
        );
        envelope = serde_json::from_slice(&original).unwrap();
        envelope.version += 1;
        fs::write(&path, serde_json::to_vec(&envelope).unwrap()).unwrap();
        assert!(
            store
                .load::<serde_json::Value>()
                .unwrap_err()
                .to_string()
                .contains("version")
        );
        fs::write(&path, b"{").unwrap();
        assert!(store.load::<serde_json::Value>().is_err());
    }

    #[cfg(unix)]
    #[test]
    fn committed_images_are_owner_only() {
        use std::os::unix::fs::PermissionsExt;

        let directory = TestDirectory::new();
        let store = directory.open();
        for value in [1, 2] {
            store
                .commit(&json!({"credential_verifier": value}))
                .unwrap();
            let mode = fs::metadata(directory.0.join(IMAGE_FILE))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    #[test]
    fn recovery_ignores_uncommitted_temporary_images() {
        let directory = TestDirectory::new();
        let store = directory.open();
        fs::write(
            directory.0.join(format!(".{IMAGE_FILE}.interrupted.tmp")),
            b"incomplete",
        )
        .unwrap();
        assert_eq!(store.load::<serde_json::Value>().unwrap(), None);
        store.commit(&json!(["committed"])).unwrap();
        drop(store);
        assert_eq!(
            directory.open().load::<serde_json::Value>().unwrap(),
            Some(json!(["committed"]))
        );
    }

    #[test]
    fn failure_before_rename_keeps_old_image_and_allows_retry() {
        let directory = TestDirectory::new();
        let store = directory.open();
        let old = json!({"accounts": [1], "ledger": [1]});
        let new = json!({"accounts": [2], "ledger": [2]});
        store.commit(&old).unwrap();
        *store.commit_hook.lock().unwrap() = Some(|_, phase| {
            if phase == CommitPhase::BeforeRename {
                bail!("injected pre-rename failure");
            }
            Ok(())
        });
        for _ in 0..2 {
            assert!(store.commit(&new).is_err());
            assert!(!store.is_poisoned());
            assert_eq!(
                store.load::<serde_json::Value>().unwrap(),
                Some(old.clone())
            );
        }
        drop(store);
        let store = directory.open();
        assert_eq!(store.load::<serde_json::Value>().unwrap(), Some(old));
        store.commit(&new).unwrap();
        drop(store);
        assert_eq!(
            directory.open().load::<serde_json::Value>().unwrap(),
            Some(new)
        );
    }

    #[test]
    fn healthy_commit_never_exposes_a_poisoned_state() {
        let directory = TestDirectory::new();
        let store = directory.open();
        *store.commit_hook.lock().unwrap() = Some(|store, phase| {
            if phase == CommitPhase::AfterRename || phase == CommitPhase::AfterDirectorySync {
                assert!(!store.is_poisoned());
            }
            Ok(())
        });
        store.commit(&json!({"rows": [1]})).unwrap();
        assert!(!store.is_poisoned());
        assert_eq!(
            store.load::<serde_json::Value>().unwrap(),
            Some(json!({"rows": [1]}))
        );
    }

    #[test]
    fn failure_after_rename_poisoned_until_reopen() {
        let directory = TestDirectory::new();
        let store = directory.open();
        store
            .commit(&json!({"accounts": [1], "ledger": [1]}))
            .unwrap();
        let new = json!({"accounts": [2], "ledger": [2]});
        *store.commit_hook.lock().unwrap() = Some(|_, phase| {
            if phase == CommitPhase::AfterRename {
                bail!("injected directory sync failure");
            }
            Ok(())
        });
        assert!(store.commit(&new).is_err());
        assert!(store.is_poisoned());
        assert!(store.load::<serde_json::Value>().is_err());
        assert!(store.commit(&json!({"accounts": [3]})).is_err());
        drop(store);
        let store = directory.open();
        assert!(!store.is_poisoned());
        assert_eq!(store.load::<serde_json::Value>().unwrap(), Some(new));
    }

    // Run only in a subprocess with an explicit test-owned directory. exit()
    // terminates without Rust drops, leaving temporary files and locks as a crash would.
    #[test]
    fn crash_worker() {
        let Ok(directory) = std::env::var("SQWEEL_IMAGE_CRASH_DIRECTORY") else {
            return;
        };
        let store = TransactionImageStore::open(&directory).unwrap();
        *store.commit_hook.lock().unwrap() = Some(|_, phase| {
            let selected = std::env::var("SQWEEL_IMAGE_CRASH_PHASE").unwrap();
            let stop = match selected.as_str() {
                "before-rename" => phase == CommitPhase::BeforeRename,
                "after-rename" => phase == CommitPhase::AfterRename,
                "after-directory-sync" => phase == CommitPhase::AfterDirectorySync,
                _ => panic!("unknown crash test phase"),
            };
            if stop {
                std::process::exit(73);
            }
            Ok(())
        });
        store
            .commit(&json!({"accounts": [2, 3], "ledger": [2, 3]}))
            .unwrap();
        panic!("crash hook did not execute");
    }

    #[test]
    fn process_termination_at_commit_boundaries_recovers_a_complete_image() {
        for phase in ["before-rename", "after-rename", "after-directory-sync"] {
            let directory = TestDirectory::new();
            let old = json!({"accounts": [1], "ledger": [1]});
            let new = json!({"accounts": [2, 3], "ledger": [2, 3]});
            directory.open().commit(&old).unwrap();
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "storage::transaction_image::tests::crash_worker",
                    "--nocapture",
                ])
                .env("SQWEEL_IMAGE_CRASH_DIRECTORY", &directory.0)
                .env("SQWEEL_IMAGE_CRASH_PHASE", phase)
                .output()
                .unwrap();
            assert_eq!(
                output.status.code(),
                Some(73),
                "{phase}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            let recovered = directory
                .open()
                .load::<serde_json::Value>()
                .unwrap()
                .unwrap();
            // Process termination does not simulate loss of the kernel's page cache:
            // successful rename is visible on reopen even before directory fsync.
            assert_eq!(
                recovered,
                if phase == "before-rename" { old } else { new },
                "{phase}"
            );
            // The orphaned pre-rename temp file must not prevent the next commit.
            directory
                .open()
                .commit(&json!({"accounts": [4], "ledger": [4]}))
                .unwrap();
        }
    }
}
