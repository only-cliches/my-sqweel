//! Incremental embedded Lux storage. SQL isolation is in-memory; crash-atomic
//! multi-key commits and synchronous durability are deliberately not promised.
use super::*;
use crate::storage::{LuxRedisStore, StorageWrite};
use serde::de::DeserializeOwned;
use std::sync::atomic::{AtomicBool, Ordering};

const META: &str = "sqweel:v2";
const KINDS: [&str; 5] = ["schemas", "rows", "auto_inc", "views", "index_comments"];

pub(super) struct Persistence {
    store: LuxRedisStore,
    poisoned: AtomicBool,
}

fn key(database: &str, kind: &str) -> String {
    // JSON encoding keeps arbitrary quoted SQL names from colliding.
    serde_json::to_string(&(META, database, kind)).expect("string tuple serialization")
}

impl Persistence {
    pub(super) fn open(path: &str) -> Result<Self> {
        let directory = std::path::Path::new(path);
        if directory.join("transaction-image.json").exists() {
            return Err(anyhow!(
                "unsupported legacy development database format; remove the old data directory and restart with fresh storage"
            ));
        }
        std::fs::create_dir_all(directory)?;
        // Catalog verifiers and row contents must stay private even when Lux
        // creates WAL/snapshot files using the process's ordinary umask.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700))?;
        }
        let store = LuxRedisStore::open(Some(path))?;
        let metadata = store.hgetall(META)?;
        if metadata.is_empty() {
            if !store.keys("*")?.is_empty() {
                return Err(anyhow!(
                    "unsupported legacy development database format; remove the old data directory and restart with fresh storage"
                ));
            }
        } else if metadata.get("version").map(String::as_str) != Some("2") {
            return Err(anyhow!(
                "unsupported development database storage version; remove the old data directory and restart with fresh storage"
            ));
        }
        Ok(Self {
            store,
            poisoned: AtomicBool::new(false),
        })
    }

    pub(super) fn is_poisoned(&self) -> bool {
        self.poisoned.load(Ordering::Acquire)
    }

    pub(super) fn load(&self, cfg: &EngineConfig) -> Result<Option<Committed>> {
        let metadata = self.store.hgetall(META)?;
        let Some(catalog) = metadata.get("catalog") else {
            return Ok(None);
        };
        let catalog: Catalog = serde_json::from_str(catalog)?;
        let names: Vec<String> = serde_json::from_str(
            metadata
                .get("databases")
                .ok_or_else(|| anyhow!("missing database catalog"))?,
        )?;
        let mut databases = BTreeMap::new();
        for name in names {
            let mut rows: BTreeMap<String, BTreeMap<String, StoredRow>> = BTreeMap::new();
            for (field, value) in self.store.hgetall(&key(&name, "rows"))? {
                let (table, pk): (String, String) = serde_json::from_str(&field)?;
                rows.entry(table)
                    .or_default()
                    .insert(pk, serde_json::from_str(&value)?);
            }
            let snapshot = Snapshot {
                version: 1,
                created_at: Utc::now().to_rfc3339(),
                schemas: self.load_map(&name, "schemas")?,
                rows,
                auto_inc: self.load_map(&name, "auto_inc")?,
                views: self.load_map(&name, "views")?,
                index_comments: self.load_map(&name, "index_comments")?,
            };
            let mut raw = RawEngine::with_storage(cfg.clone(), Arc::new(PrivateStorage))?;
            raw.database_name = name.clone();
            raw.apply_snapshot(snapshot);
            databases.insert(name, Arc::new(raw));
        }
        if !databases.contains_key("app") {
            return Err(anyhow!("missing default database in storage"));
        }
        Ok(Some(Committed { catalog, databases }))
    }

    fn load_map<T: DeserializeOwned>(
        &self,
        database: &str,
        kind: &str,
    ) -> Result<BTreeMap<String, T>> {
        self.store
            .hgetall(&key(database, kind))?
            .into_iter()
            .map(|(field, value)| Ok((field, serde_json::from_str(&value)?)))
            .collect()
    }

    pub(super) fn commit(&self, before: &Committed, after: &Committed) -> Result<()> {
        let writes = changes(before, after)?;
        // Lux's pipeline can apply a prefix before an I/O/command error. Keep
        // the previous SQL state visible and refuse further work after failure.
        if let Err(error) = self.store.write_batch(writes) {
            self.poisoned.store(true, Ordering::Release);
            return Err(error);
        }
        Ok(())
    }
}

fn set(
    writes: &mut Vec<StorageWrite>,
    key: &str,
    field: &str,
    value: &impl Serialize,
) -> Result<()> {
    writes.push(StorageWrite::HSet {
        key: key.into(),
        field: field.into(),
        value: serde_json::to_string(value)?,
    });
    Ok(())
}

fn map_changes<T: Serialize + PartialEq>(
    writes: &mut Vec<StorageWrite>,
    key: &str,
    before: Option<&DashMap<String, T>>,
    after: &DashMap<String, T>,
) -> Result<()> {
    if let Some(before) = before {
        for item in before {
            if !after.contains_key(item.key()) {
                writes.push(StorageWrite::HDel {
                    key: key.into(),
                    field: item.key().clone(),
                });
            }
        }
    }
    for item in after {
        if before.and_then(|map| map.get(item.key())).as_deref() != Some(item.value()) {
            set(writes, key, item.key(), item.value())?;
        }
    }
    Ok(())
}

fn changes(before: &Committed, after: &Committed) -> Result<Vec<StorageWrite>> {
    let mut writes = vec![StorageWrite::HSet {
        key: META.into(),
        field: "version".into(),
        value: "2".into(),
    }];
    // Catalog metadata is small and also initializes a new store on its first commit.
    set(&mut writes, META, "catalog", &after.catalog)?;
    set(
        &mut writes,
        META,
        "databases",
        &after.databases.keys().collect::<Vec<_>>(),
    )?;
    for name in before.databases.keys() {
        if !after.databases.contains_key(name) {
            for kind in KINDS {
                writes.push(StorageWrite::Del {
                    key: key(name, kind),
                });
            }
        }
    }
    for (name, raw) in &after.databases {
        let previous = before.databases.get(name);
        if previous.is_some_and(|old| Arc::ptr_eq(old, raw)) {
            continue;
        }
        let previous = previous.map(Arc::as_ref);
        map_changes(
            &mut writes,
            &key(name, "schemas"),
            previous.map(|raw| &raw.schemas),
            &raw.schemas,
        )?;
        map_changes(
            &mut writes,
            &key(name, "auto_inc"),
            previous.map(|raw| &raw.auto_inc),
            &raw.auto_inc,
        )?;
        map_changes(
            &mut writes,
            &key(name, "views"),
            previous.map(|raw| &raw.views),
            &raw.views,
        )?;
        map_changes(
            &mut writes,
            &key(name, "index_comments"),
            previous.map(|raw| &raw.index_comments),
            &raw.index_comments,
        )?;
        let row_key = key(name, "rows");
        if let Some(previous) = previous {
            for table in &previous.rows {
                let next = raw.rows.get(table.key());
                if next
                    .as_ref()
                    .is_some_and(|next| table.value().ptr_eq(next.value()))
                {
                    continue;
                }
                for pk in table.value().keys() {
                    if !next.as_ref().is_some_and(|rows| rows.contains_key(pk)) {
                        writes.push(StorageWrite::HDel {
                            key: row_key.clone(),
                            field: serde_json::to_string(&(table.key(), pk))?,
                        });
                    }
                }
            }
        }
        // Shared tables were not modified, so persistence can skip their rows.
        // ponytail: changed tables still require a linear comparison; track dirty
        // row IDs if development datasets make this cost significant.
        for table in &raw.rows {
            let old = previous.and_then(|raw| raw.rows.get(table.key()));
            if old
                .as_ref()
                .is_some_and(|old| table.value().ptr_eq(old.value()))
            {
                continue;
            }
            for (pk, row) in table.value() {
                if old.as_ref().and_then(|rows| rows.get(pk)) != Some(row) {
                    set(
                        &mut writes,
                        &row_key,
                        &serde_json::to_string(&(table.key(), pk))?,
                        row,
                    )?;
                }
            }
        }
    }
    Ok(writes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deltas_only_contain_changed_rows_and_schemas() {
        let engine = Engine::new(EngineConfig::mysql_strict());
        engine.execute_sql("CREATE TABLE rows_here(id INT PRIMARY KEY, value TEXT); INSERT INTO rows_here VALUES (1,'old'),(2,'untouched'); CREATE DATABASE elsewhere; USE elsewhere; CREATE TABLE big(id INT PRIMARY KEY, payload TEXT)").unwrap();
        engine
            .execute_sql_with_params(
                "INSERT INTO big VALUES (1,?)",
                &[Value::String("x".repeat(5_000_000))],
            )
            .unwrap();
        let before = engine.shared.committed.lock().clone();
        engine
            .execute_sql("USE app; UPDATE rows_here SET value='new' WHERE id=1")
            .unwrap();
        let after = engine.shared.committed.lock().clone();
        let writes = changes(&before, &after).unwrap();
        let row_writes: Vec<_> = writes.iter().filter(|write| matches!(write, StorageWrite::HSet { key: write_key, .. } if write_key == &key("app", "rows"))).collect();
        assert_eq!(row_writes.len(), 1);
        assert!(writes.iter().all(|write| match write {
            StorageWrite::HSet {
                key: write_key,
                value,
                ..
            } => !write_key.contains("elsewhere") && value.len() < 2000,
            _ => true,
        }));
        let before = after;
        engine
            .execute_sql("CREATE TABLE empty_new(id INT PRIMARY KEY)")
            .unwrap();
        let writes = changes(&before, &engine.shared.committed.lock()).unwrap();
        assert!(!writes.iter().any(|write| matches!(write, StorageWrite::HSet { key: write_key, .. } if write_key == &key("app", "rows"))));
        assert_eq!(writes.iter().filter(|write| matches!(write, StorageWrite::HSet { key: write_key, .. } if write_key == &key("app", "schemas"))).count(), 1);
    }

    #[test]
    fn lux_command_errors_poison_without_publishing_sql_state() {
        let directory =
            std::env::temp_dir().join(format!("sqweel-lux-failure-{}", uuid::Uuid::new_v4()));
        let engine =
            Engine::open_with_data_dir(EngineConfig::mysql_strict(), directory.to_str()).unwrap();
        engine
            .execute_sql("CREATE TABLE items(id INT PRIMARY KEY)")
            .unwrap();
        let persistence = engine.shared.persistence.as_ref().unwrap();
        // Deliberately make a rows hash the wrong Lux type to exercise command-level
        // errors embedded in an otherwise successfully executed pipeline.
        persistence
            .store
            .sadd(&key("app", "rows"), "wrong-type")
            .unwrap();
        assert!(engine.execute_sql("INSERT INTO items VALUES(1)").is_err());
        assert!(persistence.is_poisoned());
        assert!(
            engine.shared.committed.lock().databases["app"]
                .rows
                .get("items")
                .map(|rows| rows.is_empty())
                .unwrap_or(true)
        );
        assert!(engine.execute_sql("SELECT * FROM items").is_err());
        drop(engine);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn legacy_images_are_rejected_without_modification() {
        let directory =
            std::env::temp_dir().join(format!("sqweel-old-image-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&directory).unwrap();
        let file = directory.join("transaction-image.json");
        std::fs::write(&file, "legacy").unwrap();
        let error = match Engine::open_with_data_dir(EngineConfig::default(), directory.to_str()) {
            Ok(_) => panic!("legacy image accepted"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("remove the old data directory"));
        assert_eq!(std::fs::read_to_string(file).unwrap(), "legacy");
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn lux_opens_commits_and_closes_inside_async_runtime() {
        let directory =
            std::env::temp_dir().join(format!("sqweel-async-lux-{}", uuid::Uuid::new_v4()));
        {
            let engine =
                Engine::open_with_data_dir(EngineConfig::mysql_strict(), directory.to_str())
                    .unwrap();
            engine
                .execute_sql("CREATE TABLE items(id INT PRIMARY KEY); INSERT INTO items VALUES(7)")
                .unwrap();
            assert!(
                Engine::open_with_data_dir(EngineConfig::mysql_strict(), directory.to_str())
                    .is_err()
            );
        }
        {
            let engine =
                Engine::open_with_data_dir(EngineConfig::mysql_strict(), directory.to_str())
                    .unwrap();
            assert_eq!(
                engine.execute_sql("SELECT id FROM items").unwrap()[0].rows[0]["id"],
                7
            );
        }
        std::fs::remove_dir_all(directory).unwrap();
    }
}

#[cfg(test)]
mod benchmarks {
    use super::*;

    /// `cargo test --lib ddl_cost -- --ignored --nocapture --test-threads=1`
    /// Setup and orderly shutdown are deliberately outside the timed DDL region.
    #[test]
    #[ignore = "manual development performance measurement"]
    fn ddl_cost_with_unrelated_database_payload() {
        for persistent in [false, true] {
            for populated in [false, true] {
                let directory =
                    std::env::temp_dir().join(format!("sqweel-ddl-bench-{}", uuid::Uuid::new_v4()));
                let engine = Engine::open_with_data_dir(
                    EngineConfig::mysql_strict(),
                    if persistent { directory.to_str() } else { None },
                )
                .unwrap();
                if populated {
                    engine
                        .execute_sql("CREATE TABLE existing(id INT PRIMARY KEY, payload TEXT)")
                        .unwrap();
                    let payload = "x".repeat(1000);
                    let values = (0..5000)
                        .map(|id| format!("({id},'{payload}')"))
                        .collect::<Vec<_>>()
                        .join(",");
                    engine
                        .execute_sql(&format!("INSERT INTO existing VALUES {values}"))
                        .unwrap();
                }
                engine
                    .execute_sql("CREATE DATABASE shape; USE shape")
                    .unwrap();
                let started = Instant::now();
                for count in 1..=1000 {
                    engine
                        .execute_sql(&format!(
                            "CREATE TABLE t{count}(id BIGINT PRIMARY KEY, name VARCHAR(255))"
                        ))
                        .unwrap();
                    if count == 200 || count == 1000 {
                        println!(
                            "DDL benchmark persistent={persistent} unrelated_5mb={populated} tables={count}: {:.3} ms",
                            started.elapsed().as_secs_f64() * 1000.0
                        );
                    }
                }
                drop(engine);
                if persistent {
                    std::fs::remove_dir_all(directory).unwrap();
                }
            }
        }
    }
}
