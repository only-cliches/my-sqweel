use std::collections::{BTreeMap, BTreeSet};
use std::fs::{File, OpenOptions};
use std::future::Future;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, mpsc};
use std::time::Duration;

use anyhow::{Result, anyhow};
use bytes::Bytes;
use fs2::FileExt;

use crate::vendor::lux;

/// Minimal Redis command surface MySqweel uses for durable table storage.
pub trait RedisStore: Send + Sync {
    fn is_persistent(&self) -> bool;
    fn hset(&self, key: &str, field: &str, value: &str) -> Result<()>;
    fn hdel(&self, key: &str, field: &str) -> Result<()>;
    fn hgetall(&self, key: &str) -> Result<BTreeMap<String, String>>;
    fn sadd(&self, key: &str, member: &str) -> Result<()>;
    fn srem(&self, key: &str, member: &str) -> Result<()>;
    fn smembers(&self, key: &str) -> Result<BTreeSet<String>>;
    fn del(&self, key: &str) -> Result<()>;
    fn keys(&self, pattern: &str) -> Result<Vec<String>>;

    fn write_batch(&self, writes: Vec<StorageWrite>) -> Result<()> {
        for write in writes {
            match write {
                StorageWrite::HSet { key, field, value } => self.hset(&key, &field, &value)?,
                StorageWrite::HDel { key, field } => self.hdel(&key, &field)?,
                StorageWrite::SAdd { key, member } => self.sadd(&key, &member)?,
                StorageWrite::SRem { key, member } => self.srem(&key, &member)?,
                StorageWrite::Del { key } => self.del(&key)?,
            }
        }
        Ok(())
    }
}

pub enum StorageWrite {
    HSet {
        key: String,
        field: String,
        value: String,
    },
    HDel {
        key: String,
        field: String,
    },
    SAdd {
        key: String,
        member: String,
    },
    SRem {
        key: String,
        member: String,
    },
    Del {
        key: String,
    },
}

pub struct LuxRedisStore {
    rt: Option<Arc<tokio::runtime::Runtime>>,
    client: lux::EmbeddedClient,
    handle: Mutex<Option<lux::ServerHandle>>,
    persistent: bool,
    lock: Option<FileLockGuard>,
}

impl LuxRedisStore {
    pub fn open(data_dir: Option<&str>) -> Result<Self> {
        let lock = data_dir.map(acquire_file_lock).transpose()?;
        let cfg = lux_config(data_dir)?;
        let start = move || -> Result<_> {
            let rt = Arc::new(
                tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(1)
                    .thread_name("my-sqweel-lux")
                    .enable_all()
                    .build()?,
            );
            let handle = rt.block_on(lux::run_with_config(cfg))?;
            Ok((rt, handle))
        };
        let (rt, handle) = if tokio::runtime::Handle::try_current().is_ok() {
            std::thread::spawn(start)
                .join()
                .map_err(|_| anyhow!("Lux startup worker panicked"))??
        } else {
            start()?
        };
        let client = handle.client();

        Ok(Self {
            rt: Some(rt),
            client,
            handle: Mutex::new(Some(handle)),
            persistent: data_dir.is_some(),
            lock,
        })
    }

    fn run_lux<F, T>(&self, fut: F) -> Result<T>
    where
        F: Future<Output = Result<T, lux::LuxError>> + Send + 'static,
        T: Send + 'static,
    {
        let rt = self.rt.as_ref().expect("Lux runtime exists until shutdown");
        if tokio::runtime::Handle::try_current().is_ok() {
            let handle = rt.handle().clone();
            let (tx, rx) = mpsc::sync_channel(1);
            std::thread::spawn(move || {
                let _ = tx.send(handle.block_on(fut));
            });
            return rx
                .recv()
                .map_err(|err| anyhow!("lux worker thread failed: {err}"))?
                .map_err(|err| anyhow!(err));
        }

        rt.block_on(fut).map_err(|err| anyhow!(err))
    }

    fn save_snapshot(&self) -> Result<()> {
        let client = self.client.clone();
        self.run_lux(async move {
            client.execute("SAVE", &[]).await?;
            Ok(())
        })
    }
}

impl RedisStore for LuxRedisStore {
    fn is_persistent(&self) -> bool {
        self.persistent
    }

    fn hset(&self, key: &str, field: &str, value: &str) -> Result<()> {
        let client = self.client.clone();
        let key = key.to_string();
        let field = field.to_string();
        let value = value.to_string();
        self.run_lux(async move {
            client.hset(key, field, value).await?;
            Ok(())
        })
    }

    fn hgetall(&self, key: &str) -> Result<BTreeMap<String, String>> {
        let client = self.client.clone();
        let key = key.to_string();
        let pairs = self.run_lux(async move { client.hgetall(key).await })?;
        pairs
            .into_iter()
            .map(|(field, value)| Ok((bytes_to_string(field)?, bytes_to_string(value)?)))
            .collect()
    }

    fn hdel(&self, key: &str, field: &str) -> Result<()> {
        let client = self.client.clone();
        let key = key.to_string();
        let field = field.to_string();
        self.run_lux(async move {
            client.hdel(key, &[field]).await?;
            Ok(())
        })
    }

    fn sadd(&self, key: &str, member: &str) -> Result<()> {
        let client = self.client.clone();
        let key = key.to_string();
        let member = member.to_string();
        self.run_lux(async move {
            client.sadd(key, &[member]).await?;
            Ok(())
        })
    }

    fn smembers(&self, key: &str) -> Result<BTreeSet<String>> {
        let client = self.client.clone();
        let key = key.to_string();
        let members = self.run_lux(async move { client.smembers(key).await })?;
        members.into_iter().map(bytes_to_string).collect()
    }

    fn srem(&self, key: &str, member: &str) -> Result<()> {
        let client = self.client.clone();
        let key = key.to_string();
        let member = member.to_string();
        self.run_lux(async move {
            client.srem(key, &[member]).await?;
            Ok(())
        })
    }

    fn del(&self, key: &str) -> Result<()> {
        let client = self.client.clone();
        let key = key.to_string();
        self.run_lux(async move {
            client.del(&[key]).await?;
            Ok(())
        })
    }

    fn keys(&self, pattern: &str) -> Result<Vec<String>> {
        let client = self.client.clone();
        let pattern = pattern.to_string();
        self.run_lux(async move { client.keys(&pattern).await })
    }

    fn write_batch(&self, writes: Vec<StorageWrite>) -> Result<()> {
        let commands = writes
            .into_iter()
            .map(|write| match write {
                StorageWrite::HSet { key, field, value } => {
                    vec![
                        b"HSET".to_vec(),
                        key.into_bytes(),
                        field.into_bytes(),
                        value.into_bytes(),
                    ]
                }
                StorageWrite::HDel { key, field } => {
                    vec![b"HDEL".to_vec(), key.into_bytes(), field.into_bytes()]
                }
                StorageWrite::SAdd { key, member } => {
                    vec![b"SADD".to_vec(), key.into_bytes(), member.into_bytes()]
                }
                StorageWrite::SRem { key, member } => {
                    vec![b"SREM".to_vec(), key.into_bytes(), member.into_bytes()]
                }
                StorageWrite::Del { key } => vec![b"DEL".to_vec(), key.into_bytes()],
            })
            .collect::<Vec<_>>();
        if commands.is_empty() {
            return Ok(());
        }
        let client = self.client.clone();
        self.run_lux(async move {
            for reply in client.pipeline_values(&commands).await? {
                if let lux::EmbeddedValue::Error(message) = reply {
                    return Err(lux::LuxError::Command(message));
                }
            }
            Ok(())
        })
    }
}

impl Drop for LuxRedisStore {
    fn drop(&mut self) {
        if self.persistent {
            let _ = self.save_snapshot();
        }

        let Some(handle) = self.handle.lock().ok().and_then(|mut h| h.take()) else {
            return;
        };

        let Some(rt) = self.rt.take() else {
            return;
        };
        let shutdown = move || {
            let _ = rt.block_on(handle.shutdown_and_wait());
            drop(rt);
        };
        if tokio::runtime::Handle::try_current().is_ok() {
            let _ = std::thread::spawn(shutdown).join();
        } else {
            shutdown();
        }

        drop(self.lock.take());
    }
}

struct FileLockGuard {
    data_dir: PathBuf,
    file: File,
}

impl Drop for FileLockGuard {
    fn drop(&mut self) {
        let mut paths = locked_data_dirs()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // A fork or cloned descriptor can retain the open file description after
        // this File drops. Release the OS lock explicitly before allowing another
        // local owner to reserve the directory in the registry.
        let _ = FileExt::unlock(&self.file);
        paths.remove(&self.data_dir);
    }
}

fn locked_data_dirs() -> &'static Mutex<BTreeSet<PathBuf>> {
    static PATHS: OnceLock<Mutex<BTreeSet<PathBuf>>> = OnceLock::new();
    PATHS.get_or_init(|| Mutex::new(BTreeSet::new()))
}

fn acquire_file_lock(data_dir: &str) -> Result<FileLockGuard> {
    let data_dir = PathBuf::from(data_dir);
    std::fs::create_dir_all(&data_dir)?;
    let data_dir = data_dir.canonicalize().unwrap_or(data_dir);

    {
        let mut paths = locked_data_dirs()
            .lock()
            .map_err(|_| anyhow!("data directory lock registry is poisoned"))?;
        if !paths.insert(data_dir.clone()) {
            return Err(anyhow!(
                "data directory is already open by this process: {}",
                data_dir.display()
            ));
        }
    }

    let lock_path = data_dir.join(".my-sqweel.lock");
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(&lock_path)
        .with_cleanup(&data_dir)?;

    if let Err(err) = file.try_lock_exclusive() {
        let _ = locked_data_dirs().lock().map(|mut paths| {
            paths.remove(&data_dir);
        });
        return Err(anyhow!(
            "data directory is already open: {} ({err})",
            data_dir.display()
        ));
    }

    writeln!(file, "pid={}", std::process::id()).with_cleanup(&data_dir)?;
    Ok(FileLockGuard { data_dir, file })
}

trait CleanupLockPath<T> {
    fn with_cleanup(self, data_dir: &Path) -> Result<T>;
}

impl<T> CleanupLockPath<T> for std::io::Result<T> {
    fn with_cleanup(self, data_dir: &Path) -> Result<T> {
        self.map_err(|err| {
            let _ = locked_data_dirs().lock().map(|mut paths| {
                paths.remove(data_dir);
            });
            anyhow!(err)
        })
    }
}

fn lux_config(data_dir: Option<&str>) -> Result<lux::ServerConfig> {
    let persistent = data_dir.is_some();
    let data_dir = data_dir.map(PathBuf::from).unwrap_or_else(|| {
        std::env::temp_dir().join(format!("my-sqweel-lux-{}", uuid::Uuid::new_v4()))
    });

    if persistent {
        std::fs::create_dir_all(&data_dir)?;
    }

    let storage = if persistent {
        lux::StorageConfig {
            mode: lux::StorageMode::Tiered,
            dir: path_string(data_dir.join("storage")),
        }
    } else {
        lux::StorageConfig {
            mode: lux::StorageMode::Memory,
            dir: path_string(data_dir.join("storage")),
        }
    };

    Ok(lux::ServerConfig {
        enable_resp: false,
        http_port: 0,
        data_dir: path_string(data_dir),
        save_interval: if persistent {
            Duration::from_secs(60)
        } else {
            Duration::ZERO
        },
        storage,
        ..lux::ServerConfig::default()
    })
}

fn path_string(path: impl AsRef<Path>) -> String {
    path.as_ref().to_string_lossy().into_owned()
}

fn bytes_to_string(bytes: Bytes) -> Result<String> {
    String::from_utf8(bytes.to_vec()).map_err(Into::into)
}
