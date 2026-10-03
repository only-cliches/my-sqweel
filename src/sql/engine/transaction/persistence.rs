//! Incremental RocksDB storage. Committed row data remains in RocksDB.
use super::*;
use crate::storage::{RocksDbSnapshot, RocksDbStore, StorageWrite};
use serde::de::DeserializeOwned;
use std::sync::atomic::{AtomicBool, Ordering};

const META: &str = "sqweel:v3";
const KINDS: [&str; 5] = ["schemas", "rows", "auto_inc", "views", "index_comments"];

pub(super) struct Persistence {
    store: Option<RocksDbStore>,
    custom: Option<(
        crate::storage::CustomStorage,
        Arc<crate::runtime::EngineRuntime>,
    )>,
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
        // Reject old layouts before opening RocksDB, which can update its logs.
        let marker = directory.join("sqweel-format");
        if directory.exists() && directory.read_dir()?.next().is_some() {
            ensure!(
                std::fs::read(&marker).ok().as_deref() == Some(b"3\n"),
                "unsupported legacy development database format; use a fresh directory"
            );
        }
        std::fs::create_dir_all(directory)?;
        // Catalog metadata and row contents stay private on disk.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700))?;
        }
        let store = RocksDbStore::open(Some(path))?;
        if !marker.exists() {
            std::fs::write(&marker, b"3\n")?;
        }
        let metadata = store.hgetall(META)?;
        if metadata.is_empty() {
            if !store.keys("*")?.is_empty() {
                return Err(anyhow!(
                    "unsupported legacy development database format; remove the old data directory and restart with fresh storage"
                ));
            }
        } else if metadata.get("version").map(String::as_str) != Some("3") {
            return Err(anyhow!(
                "unsupported development database storage version; remove the old data directory and restart with fresh storage"
            ));
        }
        Ok(Self {
            store: Some(store),
            custom: None,
            poisoned: AtomicBool::new(false),
        })
    }

    pub(super) fn custom(
        storage: crate::storage::CustomStorage,
        runtime: Arc<crate::runtime::EngineRuntime>,
    ) -> Self {
        Self {
            store: None,
            custom: Some((storage, runtime)),
            poisoned: AtomicBool::new(false),
        }
    }
    pub(super) fn is_poisoned(&self) -> bool {
        self.poisoned.load(Ordering::Acquire)
    }

    pub(super) fn native_snapshot(&self) -> Option<RocksDbSnapshot> {
        self.store.as_ref().map(RocksDbStore::snapshot)
    }

    pub(super) fn is_native(&self) -> bool {
        self.store.is_some()
    }

    pub(super) fn hydrate_tables(
        &self,
        raw: &RawEngine,
        tables: impl IntoIterator<Item = String>,
        snapshot: Option<&RocksDbSnapshot>,
    ) -> Result<()> {
        let Some(store) = &self.store else {
            return Ok(());
        };
        let row_key = key(&raw.database_name, "rows");
        for table in tables {
            if raw.rows.contains_key(&table)
                || raw
                    .schemas
                    .get(&table)
                    .is_some_and(|schema| schema.temporary)
            {
                continue;
            }
            let prefix = format!("{row_key}\0[{},", serde_json::to_string(&table)?);
            let entries = match snapshot {
                Some(snapshot) => snapshot.scan_prefix(&prefix)?,
                None => store.scan_prefix(&prefix)?,
            };
            let rows = entries
                .into_iter()
                .map(|(field, value)| {
                    let field = field
                        .strip_prefix(&format!("{row_key}\0"))
                        .ok_or_else(|| anyhow!("invalid RocksDB row key"))?;
                    let (name, pk): (String, String) = serde_json::from_str(field)?;
                    ensure!(name == table, "invalid RocksDB row table");
                    Ok((pk, serde_json::from_str(&value)?))
                })
                .collect::<Result<BTreeMap<String, StoredRow>>>()?;
            let rows: SharedTable<StoredRow> = rows.into();
            raw.baseline_rows.insert(table.clone(), rows.clone());
            raw.rows.insert(table.clone(), rows);
            raw.rebuild_indexes(&table);
        }
        Ok(())
    }

    pub(super) fn hydrate_all(&self, raw: &RawEngine) -> Result<()> {
        self.hydrate_tables(
            raw,
            raw.schemas
                .iter()
                .map(|item| item.key().clone())
                .collect::<Vec<_>>(),
            None,
        )
    }

    pub(super) fn load(&self, cfg: &EngineConfig) -> Result<Option<Committed>> {
        if let Some((storage, runtime)) = &self.custom {
            let storage = storage.0.clone();
            return runtime
                .run(async move { crate::storage::delta::load_state(storage.as_ref()).await })?
                .map(|image| committed_from_image(image, cfg))
                .transpose();
        }
        let metadata = self.store.as_ref().unwrap().hgetall(META)?;
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
            let snapshot = Snapshot {
                version: 1,
                created_at: Utc::now().to_rfc3339(),
                schemas: self.load_map(&name, "schemas")?,
                rows: BTreeMap::new(),
                auto_inc: self.load_map(&name, "auto_inc")?,
                views: self.load_map(&name, "views")?,
                index_comments: self.load_map(&name, "index_comments")?,
            };
            let mut raw = RawEngine::new(cfg.clone());
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
            .as_ref()
            .unwrap()
            .hgetall(&key(database, kind))?
            .into_iter()
            .map(|(field, value)| Ok((field, serde_json::from_str(&value)?)))
            .collect()
    }

    pub(super) fn commit(
        &self,
        before: &Committed,
        after: &Committed,
    ) -> Result<(Committed, Committed)> {
        if let Some((storage, runtime)) = &self.custom {
            let before_image = image(before)?;
            let after_image = image(after)?;
            let mut batch = crate::storage::delta::storage_batch(&before_image, &after_image);
            if !batch
                .metadata
                .iter()
                .any(|item| matches!(item, crate::storage::MetadataMutation::PutCatalog(_)))
            {
                batch.metadata.insert(
                    0,
                    crate::storage::MetadataMutation::PutCatalog(
                        crate::storage::delta::storage_catalog(&after_image),
                    ),
                );
            }
            let storage = storage.0.clone();
            let result = runtime.run(async move { storage.commit(batch).await });
            if result.is_err() {
                self.poisoned.store(true, Ordering::Release);
            }
            result?;
            return Ok((before.clone(), after.clone()));
        }
        let mut loaded_before = before.clone();
        let mut changed_after = after.clone();
        for (database, previous) in &before.databases {
            if after
                .databases
                .get(database)
                .is_some_and(|next| Arc::ptr_eq(previous, next))
            {
                continue;
            }
            let old = previous.fork()?;
            match after.databases.get(database) {
                None => {
                    self.hydrate_all(&old)?;
                }
                Some(next) => {
                    let next = next.fork()?;
                    for table in next
                        .rows
                        .iter()
                        .map(|item| item.key().clone())
                        .collect::<Vec<_>>()
                    {
                        let baseline = next.baseline_rows.get(&table).map(|rows| rows.clone());
                        let current = next.rows.get(&table).expect("loaded table exists");
                        if baseline
                            .as_ref()
                            .is_some_and(|rows| rows == current.value())
                        {
                            drop(current);
                            next.rows.remove(&table);
                            continue;
                        }
                        drop(current);
                        if let Some(baseline) = baseline {
                            old.rows.insert(table, baseline);
                        } else if previous.schemas.contains_key(&table) {
                            self.hydrate_tables(&old, [table], None)?;
                        }
                    }
                    for table in previous.schemas.iter().map(|item| item.key().clone()) {
                        if !next.schemas.contains_key(&table) && !old.rows.contains_key(&table) {
                            if let Some(baseline) = next.baseline_rows.get(&table) {
                                old.rows.insert(table, baseline.clone());
                            } else {
                                self.hydrate_tables(&old, [table], None)?;
                            }
                        }
                    }
                    changed_after
                        .databases
                        .insert(database.clone(), Arc::new(next));
                }
            }
            loaded_before
                .databases
                .insert(database.clone(), Arc::new(old));
        }
        let writes = changes(&loaded_before, &changed_after)?;
        // Keep the previous SQL state visible and refuse further work after a
        // failed RocksDB batch.
        if let Err(error) = self.store.as_ref().unwrap().write_batch(writes) {
            self.poisoned.store(true, Ordering::Release);
            return Err(error);
        }
        Ok((loaded_before, changed_after))
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
        value: "3".into(),
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
    fn native_rows_are_loaded_for_queries_but_not_retained_by_the_engine() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("db");
        let engine = Engine::open(
            EngineConfig::default(),
            crate::Storage::RocksDb(path.clone()),
        )
        .unwrap();
        engine
            .execute_sql("CREATE TABLE items(id INT PRIMARY KEY, value TEXT); INSERT INTO items VALUES(1,'one'),(2,'two')")
            .unwrap();
        {
            let state = engine.shared.committed.lock();
            assert!(state.databases["app"].rows.is_empty());
            assert!(state.databases["app"].indexes.is_empty());
        }
        assert_eq!(
            engine
                .execute_sql("SELECT value FROM items WHERE id=2")
                .unwrap()[0]
                .rows[0]["value"],
            "two"
        );
        assert!(
            engine.shared.committed.lock().databases["app"]
                .rows
                .is_empty()
        );
        engine
            .execute_sql("UPDATE items SET value='updated' WHERE id=1")
            .unwrap();
        drop(engine);

        let reopened =
            Engine::open(EngineConfig::default(), crate::Storage::RocksDb(path)).unwrap();
        assert!(
            reopened.shared.committed.lock().databases["app"]
                .rows
                .is_empty()
        );
        assert_eq!(
            reopened
                .execute_sql("SELECT value FROM items WHERE id=1")
                .unwrap()[0]
                .rows[0]["value"],
            "updated"
        );
        reopened
            .execute_sql("DELETE FROM items WHERE id=2")
            .unwrap();
        assert_eq!(
            reopened
                .execute_sql("SELECT COUNT(*) AS n FROM items")
                .unwrap()[0]
                .rows[0]["n"],
            1
        );
        assert!(
            reopened.shared.committed.lock().databases["app"]
                .rows
                .is_empty()
        );
    }

    #[test]
    fn native_transaction_reads_from_a_stable_rocksdb_snapshot() {
        let directory = tempfile::tempdir().unwrap();
        let engine = Engine::open(
            EngineConfig::default(),
            crate::Storage::RocksDb(directory.path().join("db")),
        )
        .unwrap();
        engine.execute_sql("CREATE TABLE a(id INT PRIMARY KEY, value INT); CREATE TABLE b(id INT PRIMARY KEY, value INT); INSERT INTO a VALUES(1,1); INSERT INTO b VALUES(1,1)").unwrap();
        let mut reader = engine.session();
        reader.execute_sql("BEGIN").unwrap();
        assert_eq!(
            reader.execute_sql("SELECT value FROM a").unwrap()[0].rows[0]["value"],
            1
        );
        assert!(
            reader
                .inner
                .lock()
                .transaction
                .as_ref()
                .unwrap()
                .1
                .databases["app"]
                .rows
                .is_empty()
        );
        engine.execute_sql("UPDATE b SET value=2").unwrap();
        assert_eq!(
            reader.execute_sql("SELECT value FROM b").unwrap()[0].rows[0]["value"],
            1
        );
        reader.execute_sql("ROLLBACK").unwrap();
        assert_eq!(
            engine.execute_sql("SELECT value FROM b").unwrap()[0].rows[0]["value"],
            2
        );

        let mut writer = engine.session();
        writer.execute_sql("BEGIN").unwrap();
        assert_eq!(
            writer.execute_sql("SELECT value FROM a").unwrap()[0].rows[0]["value"],
            1
        );
        engine.execute_sql("UPDATE b SET value=3").unwrap();
        writer.execute_sql("UPDATE a SET value=4").unwrap();
        writer.execute_sql("COMMIT").unwrap();
        assert_eq!(
            engine.execute_sql("SELECT value FROM a").unwrap()[0].rows[0]["value"],
            4
        );
        assert_eq!(
            engine.execute_sql("SELECT value FROM b").unwrap()[0].rows[0]["value"],
            3
        );
    }

    #[test]
    fn native_views_cascades_and_maintenance_read_rows_from_disk() {
        let directory = tempfile::tempdir().unwrap();
        let engine = Engine::open(
            EngineConfig::default(),
            crate::Storage::RocksDb(directory.path().join("db")),
        )
        .unwrap();
        engine.execute_sql("CREATE TABLE parent(id INT PRIMARY KEY); CREATE TABLE child(id INT PRIMARY KEY, parent_id INT, FOREIGN KEY(parent_id) REFERENCES parent(id) ON UPDATE CASCADE); CREATE VIEW child_view AS SELECT parent_id FROM child; INSERT INTO parent VALUES(1); INSERT INTO child VALUES(1,1)").unwrap();
        assert_eq!(
            engine
                .execute_sql("SELECT parent_id FROM child_view")
                .unwrap()[0]
                .rows[0]["parent_id"],
            1
        );
        engine.execute_sql("UPDATE parent SET id=2").unwrap();
        assert_eq!(
            engine
                .execute_sql("SELECT parent_id FROM child_view")
                .unwrap()[0]
                .rows[0]["parent_id"],
            2
        );
        let saved = engine.snapshot();
        assert_eq!(saved.rows["child"].len(), 1);
        engine.reset_table_rows("child").unwrap();
        assert!(
            engine.execute_sql("SELECT * FROM child").unwrap()[0]
                .rows
                .is_empty()
        );
        engine.restore_snapshot(saved).unwrap();
        assert_eq!(
            engine
                .execute_sql("SELECT parent_id FROM child_view")
                .unwrap()[0]
                .rows[0]["parent_id"],
            2
        );
        assert!(
            engine.shared.committed.lock().databases["app"]
                .rows
                .is_empty()
        );
    }

    #[test]
    fn deltas_only_contain_changed_rows_and_schemas() {
        let engine = Engine::new(EngineConfig::mysql_strict());
        engine.execute_sql("CREATE TABLE rows_here(id INT PRIMARY KEY, value TEXT); INSERT INTO rows_here VALUES (1,'old'),(2,'untouched'); CREATE DATABASE elsewhere; USE elsewhere; CREATE TABLE big(id INT PRIMARY KEY, payload TEXT)").unwrap();
        engine
            .execute_sql_with_params(
                "USE elsewhere; INSERT INTO big VALUES (1,?)",
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
    fn rocksdb_persists_each_changed_row_as_one_value() {
        let directory =
            std::env::temp_dir().join(format!("sqweel-rocksdb-row-{}", uuid::Uuid::new_v4()));
        let engine = Engine::open(
            EngineConfig::mysql_strict(),
            (directory.to_str()).map_or(crate::Storage::Memory, |path| {
                crate::Storage::RocksDb(path.into())
            }),
        )
        .unwrap();
        engine
            .execute_sql("CREATE TABLE items(id INT PRIMARY KEY, value TEXT)")
            .unwrap();
        engine
            .execute_sql("INSERT INTO items VALUES(1, 'whole row')")
            .unwrap();
        engine
            .execute_sql("UPDATE items SET value='updated row' WHERE id=1")
            .unwrap();
        let stored = engine
            .shared
            .persistence
            .as_ref()
            .unwrap()
            .store
            .as_ref()
            .unwrap()
            .hgetall(&key("app", "rows"))
            .unwrap();
        assert_eq!(stored.len(), 1);
        let row: StoredRow = serde_json::from_str(stored.values().next().unwrap()).unwrap();
        assert_eq!(row.data.len(), 2);
        assert_eq!(row.data["value"], "updated row");
        assert_eq!(row.table, "items");
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
        let error = match Engine::open(
            EngineConfig::default(),
            (directory.to_str()).map_or(crate::Storage::Memory, |path| {
                crate::Storage::RocksDb(path.into())
            }),
        ) {
            Ok(_) => panic!("legacy image accepted"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("remove the old data directory"));
        assert_eq!(std::fs::read_to_string(file).unwrap(), "legacy");
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn legacy_rocksdb_is_rejected_without_changing_files() {
        let directory = tempfile::tempdir().unwrap();
        {
            let store = RocksDbStore::open(directory.path().to_str()).unwrap();
            store.hset("sqweel:v2", "version", "2").unwrap();
        }
        let files = || {
            std::fs::read_dir(directory.path())
                .unwrap()
                .map(|entry| {
                    let path = entry.unwrap().path();
                    (
                        path.file_name().unwrap().to_owned(),
                        std::fs::read(path).unwrap(),
                    )
                })
                .collect::<BTreeMap<_, _>>()
        };
        let before = files();
        assert!(
            Engine::open(
                EngineConfig::default(),
                crate::Storage::RocksDb(directory.path().into())
            )
            .is_err()
        );
        assert_eq!(before, files());
    }

    #[tokio::test]
    async fn rocksdb_opens_commits_and_closes_inside_async_runtime() {
        let directory =
            std::env::temp_dir().join(format!("sqweel-async-rocksdb-{}", uuid::Uuid::new_v4()));
        {
            let engine = Engine::open(
                EngineConfig::mysql_strict(),
                (directory.to_str()).map_or(crate::Storage::Memory, |path| {
                    crate::Storage::RocksDb(path.into())
                }),
            )
            .unwrap();
            engine
                .execute_sql("CREATE TABLE items(id INT PRIMARY KEY); INSERT INTO items VALUES(7)")
                .unwrap();
            assert!(
                Engine::open(
                    EngineConfig::mysql_strict(),
                    (directory.to_str()).map_or(crate::Storage::Memory, |path| {
                        crate::Storage::RocksDb(path.into())
                    })
                )
                .is_err()
            );
        }
        {
            let engine = Engine::open(
                EngineConfig::mysql_strict(),
                (directory.to_str()).map_or(crate::Storage::Memory, |path| {
                    crate::Storage::RocksDb(path.into())
                }),
            )
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
                let engine = Engine::open(
                    EngineConfig::mysql_strict(),
                    (if persistent { directory.to_str() } else { None })
                        .map_or(crate::Storage::Memory, |path| {
                            crate::Storage::RocksDb(path.into())
                        }),
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

fn image(state: &Committed) -> Result<EngineState> {
    Ok(EngineState {
        version: 1,
        metadata: serde_json::to_value(&state.catalog)?,
        databases: state
            .databases
            .iter()
            .map(|(name, raw)| (name.clone(), raw.snapshot()))
            .collect(),
    })
}
fn committed_from_image(image: EngineState, cfg: &EngineConfig) -> Result<Committed> {
    ensure!(
        image.version == 1 && image.databases.contains_key("app"),
        "invalid custom storage catalog"
    );
    let catalog = serde_json::from_value(image.metadata)?;
    let mut databases = BTreeMap::new();
    for (name, snapshot) in image.databases {
        ensure!(snapshot.version == 1, "unsupported snapshot version");
        let mut raw = RawEngine::new(cfg.clone());
        raw.database_name = name.clone();
        raw.apply_snapshot(snapshot);
        databases.insert(name, Arc::new(raw));
    }
    Ok(Committed { catalog, databases })
}
