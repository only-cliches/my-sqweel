#[cfg(test)]
mod tests {
    use crate::sql::engine::*;
    use crate::storage::*;
    use crate::*;
    use anyhow::Result;
    use serde_json::Value;
    use std::collections::BTreeMap;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex, atomic::AtomicBool};

    #[derive(Clone, Default)]
    struct TestStore {
        catalog: Arc<Mutex<Option<StorageCatalog>>>,
        tables: Arc<Mutex<BTreeMap<(String, String), TableState>>>,
        rows: Arc<Mutex<BTreeMap<(String, String), BTreeMap<String, crate::model::StoredRow>>>>,
        commits: Arc<AtomicUsize>,
        fail_commit: Arc<AtomicBool>,
        block_commit: Arc<AtomicBool>,
        entered: Arc<tokio::sync::Notify>,
        release: Arc<tokio::sync::Notify>,
    }

    impl AsyncStorage for TestStore {
        async fn load_catalog(&self) -> Result<Option<StorageCatalog>> {
            Ok(self.catalog.lock().unwrap().clone())
        }

        async fn list_tables(&self, database: &str) -> Result<Vec<String>> {
            Ok(self
                .tables
                .lock()
                .unwrap()
                .keys()
                .filter(|(candidate, _)| candidate == database)
                .map(|(_, table)| table.clone())
                .collect())
        }

        async fn load_table(&self, database: &str, table: &str) -> Result<Option<TableState>> {
            Ok(self
                .tables
                .lock()
                .unwrap()
                .get(&(database.into(), table.into()))
                .cloned())
        }

        async fn scan_rows(&self, scan: RowScan) -> Result<crate::storage::RowPage> {
            let rows = self
                .rows
                .lock()
                .unwrap()
                .get(&(scan.database, scan.table))
                .cloned()
                .unwrap_or_default();
            let mut page = BTreeMap::new();
            let limit = scan.limit.max(1);
            let mut more = false;
            for (key, row) in rows {
                if scan.cursor.as_ref().is_some_and(|cursor| key <= *cursor) {
                    continue;
                }
                if page.len() == limit {
                    more = true;
                    break;
                }
                page.insert(key, row);
            }
            Ok(crate::storage::RowPage {
                next_cursor: more.then(|| page.last_key_value().unwrap().0.clone()),
                rows: page,
            })
        }

        async fn commit(&self, batch: StorageBatch) -> Result<()> {
            if self.block_commit.load(Ordering::Relaxed) {
                self.entered.notify_one();
                self.release.notified().await;
            }
            anyhow::ensure!(!self.fail_commit.load(Ordering::Relaxed), "storage offline");
            for mutation in batch.metadata {
                match mutation {
                    MetadataMutation::PutCatalog(catalog) => {
                        *self.catalog.lock().unwrap() = Some(catalog)
                    }
                    MetadataMutation::DeleteDatabase { database } => {
                        self.tables
                            .lock()
                            .unwrap()
                            .retain(|(candidate, _), _| candidate != &database);
                        self.rows
                            .lock()
                            .unwrap()
                            .retain(|(candidate, _), _| candidate != &database);
                    }
                    MetadataMutation::PutTable {
                        database,
                        table,
                        state,
                    } => {
                        self.tables.lock().unwrap().insert((database, table), state);
                    }
                    MetadataMutation::DeleteTable { database, table } => {
                        self.tables
                            .lock()
                            .unwrap()
                            .remove(&(database.clone(), table.clone()));
                        self.rows.lock().unwrap().remove(&(database, table));
                    }
                }
            }
            for mutation in batch.rows {
                match mutation {
                    RowMutation::Put {
                        database,
                        table,
                        key,
                        row,
                    } => {
                        self.rows
                            .lock()
                            .unwrap()
                            .entry((database, table))
                            .or_default()
                            .insert(key, row);
                    }
                    RowMutation::Delete {
                        database,
                        table,
                        key,
                    } => {
                        if let Some(rows) = self.rows.lock().unwrap().get_mut(&(database, table)) {
                            rows.remove(&key);
                        }
                    }
                }
            }
            self.commits.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }
    }

    fn hook_receiver(
        engine: &Engine,
    ) -> (
        QueryHookSubscription,
        tokio::sync::mpsc::UnboundedReceiver<Arc<QueryHookEvent>>,
    ) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let subscription = engine
            .subscribe_query_hooks(
                QueryHookOptions {
                    read: true,
                    ..Default::default()
                },
                move |event| {
                    tx.send(event).unwrap();
                    async { Ok(()) }
                },
            )
            .unwrap();
        (subscription, rx)
    }

    async fn hook_next(
        rx: &mut tokio::sync::mpsc::UnboundedReceiver<Arc<QueryHookEvent>>,
    ) -> Arc<QueryHookEvent> {
        tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
            .await
            .unwrap()
            .unwrap()
    }

    #[tokio::test]
    async fn hooks_wait_for_storage_and_preserve_commit_boundaries() {
        let store = TestStore::default();
        let engine = Engine::open_async(EngineConfig::default(), Storage::custom(store.clone()))
            .await
            .unwrap();
        engine
            .execute_sql_async("CREATE TABLE items (id INT PRIMARY KEY, v INT)")
            .await
            .unwrap();
        let (_subscription, mut rx) = hook_receiver(&engine);
        store.block_commit.store(true, Ordering::Relaxed);
        let execution = engine.execute_sql_async("INSERT INTO items VALUES (1, 10)");
        tokio::pin!(execution);
        tokio::select! {
            _ = store.entered.notified() => {},
            result = &mut execution => panic!("commit did not block: {result:?}"),
        }
        tokio::task::yield_now().await;
        assert!(rx.try_recv().is_err());
        engine.query_filters().push(Synthetic);
        engine.execute_sql_async("SELECT cached").await.unwrap();
        assert!(
            matches!(&*hook_next(&mut rx).await, QueryHookEvent::Read { results, .. } if results[0].columns == ["cached"])
        );
        store.block_commit.store(false, Ordering::Relaxed);
        store.release.notify_one();
        execution.await.unwrap();
        assert!(
            matches!(&*hook_next(&mut rx).await, QueryHookEvent::Write { rows, .. } if rows[0].row["v"] == Value::from(10))
        );
        let mut session = engine.session();
        session
            .execute_sql_async("BEGIN; UPDATE items SET v=20; UPDATE items SET v=30")
            .await
            .unwrap();
        tokio::task::yield_now().await;
        assert!(rx.try_recv().is_err());
        session.execute_sql_async("COMMIT").await.unwrap();
        assert!(
            matches!(&*hook_next(&mut rx).await, QueryHookEvent::Write { rows, .. } if rows.len() == 1 && rows[0].row["v"] == Value::from(30))
        );
        assert!(
            session
                .execute_sql_async(
                    "UPDATE items SET v=40; UPDATE items SET v=50; INSERT INTO absent VALUES (1)"
                )
                .await
                .is_err()
        );
        for value in [40, 50] {
            assert!(
                matches!(&*hook_next(&mut rx).await, QueryHookEvent::Write { rows, .. } if rows[0].row["v"] == Value::from(value))
            );
        }
    }

    #[tokio::test]
    async fn failed_storage_discards_hooks_and_stops_execution() {
        let store = TestStore::default();
        let engine = Engine::open_async(EngineConfig::default(), Storage::custom(store.clone()))
            .await
            .unwrap();
        engine
            .execute_sql_async("CREATE TABLE items (id INT PRIMARY KEY)")
            .await
            .unwrap();
        let (_subscription, mut rx) = hook_receiver(&engine);
        store.fail_commit.store(true, Ordering::Relaxed);
        assert!(
            engine
                .execute_sql_async("INSERT INTO items VALUES (1)")
                .await
                .is_err()
        );
        assert!(rx.try_recv().is_err());
        assert!(
            engine
                .execute_sql_async("SELECT * FROM items")
                .await
                .unwrap_err()
                .to_string()
                .contains("reopen")
        );
        assert!(
            store
                .rows
                .lock()
                .unwrap()
                .values()
                .all(|rows| rows.is_empty())
        );
    }

    #[tokio::test]
    async fn cancelled_caller_does_not_cancel_an_accepted_commit() {
        let store = TestStore::default();
        let engine = Engine::open_async(EngineConfig::default(), Storage::custom(store.clone()))
            .await
            .unwrap();
        engine
            .execute_sql_async("CREATE TABLE items (id INT PRIMARY KEY)")
            .await
            .unwrap();
        let (_subscription, mut rx) = hook_receiver(&engine);
        store.block_commit.store(true, Ordering::Relaxed);
        let writer = engine.clone();
        let execution = tokio::spawn(async move {
            writer
                .execute_sql_async("INSERT INTO items VALUES (1)")
                .await
        });
        store.entered.notified().await;
        execution.abort();
        assert!(rx.try_recv().is_err());
        store.block_commit.store(false, Ordering::Relaxed);
        store.release.notify_one();
        assert!(matches!(
            &*hook_next(&mut rx).await,
            QueryHookEvent::Write { .. }
        ));
        assert_eq!(
            engine
                .execute_sql_async("SELECT * FROM items")
                .await
                .unwrap()[0]
                .rows
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn hook_queue_capacity_bounds_slow_subscribers() {
        let engine = Engine::open_async(
            EngineConfig::default(),
            Storage::custom(TestStore::default()),
        )
        .await
        .unwrap();
        engine
            .execute_sql_async("CREATE TABLE items (id INT PRIMARY KEY)")
            .await
            .unwrap();
        let entered = Arc::new(tokio::sync::Notify::new());
        let callback_entered = entered.clone();
        let mut subscription = engine
            .subscribe_query_hooks(
                QueryHookOptions {
                    capacity: 1,
                    ..Default::default()
                },
                move |_| {
                    callback_entered.notify_one();
                    std::future::pending::<Result<()>>()
                },
            )
            .unwrap();
        engine
            .execute_sql_async("INSERT INTO items VALUES (1)")
            .await
            .unwrap();
        entered.notified().await;
        engine
            .execute_sql_async("INSERT INTO items VALUES (2); INSERT INTO items VALUES (3)")
            .await
            .unwrap();
        assert_eq!(
            tokio::time::timeout(std::time::Duration::from_secs(3), subscription.wait())
                .await
                .unwrap(),
            Err(crate::QueryHookError::Overflow)
        );
        assert_eq!(
            engine
                .execute_sql_async("SELECT * FROM items")
                .await
                .unwrap()[0]
                .rows
                .len(),
            3
        );
    }

    struct HookResultFilter;
    impl ResultFilter for HookResultFilter {
        async fn filter(
            &self,
            request: &QueryRequest,
            results: &mut Vec<QueryResult>,
        ) -> Result<ResultFilterAction> {
            if request.sql.contains("rejected") {
                return Ok(ResultFilterAction::Reject("hidden".into()));
            }
            if request.sql.contains("replacement") {
                return Ok(ResultFilterAction::Replace(vec![QueryResult {
                    columns: vec!["replacement".into()],
                    rows: vec![
                        serde_json::json!({"replacement":42})
                            .as_object()
                            .unwrap()
                            .clone(),
                    ],
                    ..Default::default()
                }]));
            }
            for result in results {
                result.rows.clear();
            }
            Ok(ResultFilterAction::Continue)
        }
    }

    #[tokio::test]
    async fn read_hooks_follow_filters_for_engine_and_sessions() {
        let engine = Engine::open_async(
            EngineConfig::default(),
            Storage::custom(TestStore::default()),
        )
        .await
        .unwrap();
        engine.query_filters().push(Synthetic);
        engine.result_filters().push(HookResultFilter);
        let (_subscription, mut rx) = hook_receiver(&engine);
        engine.execute_sql_async("SELECT cached").await.unwrap();
        assert!(
            matches!(&*hook_next(&mut rx).await, QueryHookEvent::Read { results, .. } if results[0].columns == ["cached"] && results[0].rows.is_empty())
        );
        engine
            .session()
            .execute_sql_async("SELECT 1 AS replacement")
            .await
            .unwrap();
        assert!(
            matches!(&*hook_next(&mut rx).await, QueryHookEvent::Read { results, .. } if results[0].rows[0]["replacement"] == Value::from(42))
        );
        assert!(
            engine
                .execute_sql_async("SELECT 1 AS rejected")
                .await
                .is_err()
        );
        assert!(
            engine
                .session()
                .execute_sql_async("SELECT 1 AS rejected")
                .await
                .is_err()
        );
        tokio::task::yield_now().await;
        assert!(rx.try_recv().is_err());
    }

    struct Rewrite;
    impl QueryFilter for Rewrite {
        async fn filter(&self, request: &mut QueryRequest) -> Result<QueryFilterAction> {
            if request.sql == "SELECT original" {
                request.sql = "SELECT 7 AS rewritten".into();
            }
            Ok(QueryFilterAction::Continue)
        }
    }

    struct HideRows;
    impl ResultFilter for HideRows {
        async fn filter(
            &self,
            _request: &QueryRequest,
            results: &mut Vec<QueryResult>,
        ) -> Result<ResultFilterAction> {
            for result in results {
                result.rows.clear();
            }
            Ok(ResultFilterAction::Continue)
        }
    }

    struct Synthetic;
    impl QueryFilter for Synthetic {
        async fn filter(&self, request: &mut QueryRequest) -> Result<QueryFilterAction> {
            if request.sql == "SELECT cached" {
                return Ok(QueryFilterAction::Return(vec![QueryResult {
                    columns: vec!["cached".into()],
                    rows: vec![serde_json::Map::from_iter([(
                        "cached".into(),
                        Value::Bool(true),
                    )])],
                    ..QueryResult::default()
                }]));
            }
            Ok(QueryFilterAction::Continue)
        }
    }

    #[tokio::test]
    async fn filters_rewrite_short_circuit_and_change_results() {
        let store = TestStore::default();
        let engine = Engine::open_async(EngineConfig::default(), Storage::custom(store.clone()))
            .await
            .unwrap();
        engine.query_filters().push(Rewrite);
        engine.query_filters().push(Synthetic);
        engine.result_filters().push(HideRows);

        let rewritten = engine.execute_sql_async("SELECT original").await.unwrap();
        assert_eq!(rewritten[0].columns, ["rewritten"]);
        assert!(rewritten[0].rows.is_empty());

        let cached = engine.execute_sql_async("SELECT cached").await.unwrap();
        assert_eq!(cached[0].columns, ["cached"]);
        assert!(cached[0].rows.is_empty());
        assert_eq!(store.commits.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn parameters_are_bound_before_filters_and_state_is_reloaded() {
        let store = TestStore::default();
        let engine = Engine::open_async(EngineConfig::default(), Storage::custom(store.clone()))
            .await
            .unwrap();
        engine
            .execute_sql_async("CREATE TABLE items (id INT PRIMARY KEY, name TEXT)")
            .await
            .unwrap();
        engine
            .execute_sql_with_params_async(
                "INSERT INTO items VALUES (?, ?)",
                vec![Value::from(1), Value::from("Ada")],
            )
            .await
            .unwrap();

        let reopened = Engine::open_async(EngineConfig::default(), Storage::custom(store))
            .await
            .unwrap();
        let rows = reopened
            .execute_sql_async("SELECT name FROM items WHERE id = 1")
            .await
            .unwrap();
        assert_eq!(rows[0].rows[0]["name"], Value::String("Ada".into()));
    }

    #[tokio::test]
    async fn session_commit_is_persisted_as_one_backend_commit() {
        let store = TestStore::default();
        let engine = Engine::open_async(EngineConfig::default(), Storage::custom(store.clone()))
            .await
            .unwrap();
        engine
            .execute_sql_async("CREATE TABLE events (id INT PRIMARY KEY)")
            .await
            .unwrap();
        let mut session = engine.session();
        session.execute_sql_async("BEGIN").await.unwrap();
        session
            .execute_sql_async("INSERT INTO events VALUES (1)")
            .await
            .unwrap();
        session.execute_sql_async("COMMIT").await.unwrap();

        let reopened = Engine::open_async(EngineConfig::default(), Storage::custom(store))
            .await
            .unwrap();
        let rows = reopened
            .execute_sql_async("SELECT * FROM events")
            .await
            .unwrap();
        assert_eq!(rows[0].rows.len(), 1);
    }

    #[tokio::test]
    async fn rocksdb_storage_reopens_async_engine_state() {
        let directory = tempfile::tempdir().unwrap();
        {
            let storage = RocksDbStorage::open(Some(directory.path().to_path_buf()))
                .await
                .unwrap();
            let engine = Engine::open_async(EngineConfig::default(), Storage::custom(storage))
                .await
                .unwrap();
            engine
                .execute_sql_async("CREATE TABLE durable_items (id INT PRIMARY KEY)")
                .await
                .unwrap();
            engine
                .execute_sql_async("INSERT INTO durable_items VALUES (1)")
                .await
                .unwrap();
        }

        let storage = RocksDbStorage::open(Some(directory.path().to_path_buf()))
            .await
            .unwrap();
        let engine = Engine::open_async(EngineConfig::default(), Storage::custom(storage))
            .await
            .unwrap();
        let rows = engine
            .execute_sql_async("SELECT * FROM durable_items")
            .await
            .unwrap();
        assert_eq!(rows[0].rows.len(), 1);
    }

    #[tokio::test]
    async fn rocksdb_storage_scans_rows_in_bounded_pages() {
        let storage = RocksDbStorage::open(None).await.unwrap();
        let engine = Engine::open_async(EngineConfig::default(), Storage::custom(storage.clone()))
            .await
            .unwrap();
        engine
            .execute_sql_async("CREATE TABLE scan_items (id INT PRIMARY KEY)")
            .await
            .unwrap();
        for id in 1..=3 {
            engine
                .execute_sql_async(&format!("INSERT INTO scan_items VALUES ({id})"))
                .await
                .unwrap();
        }

        let mut cursor = None;
        let mut keys = Vec::new();
        loop {
            let page = storage
                .scan_rows(RowScan {
                    database: "app".into(),
                    table: "scan_items".into(),
                    cursor,
                    limit: 1,
                })
                .await
                .unwrap();
            assert!(page.rows.len() <= 1);
            keys.extend(page.rows.into_keys());
            let Some(next_cursor) = page.next_cursor else {
                break;
            };
            cursor = Some(next_cursor);
        }
        keys.sort();
        keys.dedup();
        assert_eq!(keys.len(), 3);
    }
}

#[cfg(test)]
mod engine_api {
    use crate::{Engine, EngineConfig, Storage};
    use serde_json::json;

    #[tokio::test]
    async fn sync_and_async_calls_share_storage_but_direct_calls_have_fresh_sessions() {
        let directory = tempfile::tempdir().unwrap();
        {
            let engine = Engine::open_async(
                EngineConfig::mysql_strict(),
                Storage::RocksDb(directory.path().into()),
            )
            .await
            .unwrap();
            let clone = engine.clone();
            engine
                .execute_sql("CREATE TABLE items (id INT PRIMARY KEY, payload TEXT)")
                .unwrap();
            let mut session = clone.session();
            session
                .execute_sql("BEGIN; INSERT INTO items VALUES (1, 'whole row')")
                .unwrap();
            assert!(
                engine
                    .execute_sql_async("SELECT * FROM items")
                    .await
                    .unwrap()[0]
                    .rows
                    .is_empty()
            );
            session
                .execute_sql_async("UPDATE items SET payload='updated'; COMMIT")
                .await
                .unwrap();
            assert_eq!(
                engine.execute_sql("SELECT payload FROM items").unwrap()[0].rows[0]["payload"],
                json!("updated")
            );
            session.execute_sql("SET @value=7").unwrap();
            assert_eq!(
                session
                    .execute_sql_async("SELECT @value AS value")
                    .await
                    .unwrap()[0]
                    .rows[0]["value"],
                json!(7)
            );
            assert!(
                engine.execute_sql("SELECT @value AS value").unwrap()[0].rows[0]["value"].is_null()
            );
        }
        let reopened = Engine::open(
            EngineConfig::mysql_strict(),
            Storage::RocksDb(directory.path().into()),
        )
        .unwrap();
        assert_eq!(
            reopened
                .execute_sql_async("SELECT payload FROM items")
                .await
                .unwrap()[0]
                .rows[0]["payload"],
            json!("updated")
        );
    }
}

#[cfg(test)]
mod scope_regressions {
    use crate::sql::engine::{AuthPrivilege::*, AuthScope};
    use crate::{Engine, EngineConfig, Storage};
    #[test]
    fn scoped_queries_check_ctes_returning_and_replacement_ddl() {
        let engine = Engine::open(EngineConfig::mysql_strict(), Storage::Memory).unwrap();
        engine.execute_sql("CREATE TABLE allowed(id INT PRIMARY KEY); CREATE TABLE hidden(id INT PRIMARY KEY); INSERT INTO allowed VALUES(1); INSERT INTO hidden VALUES(2)").unwrap();
        let mut session = engine.session();
        session
            .authenticate_external(
                "user".into(),
                vec![
                    AuthScope::table("app", "allowed", [Select]),
                    AuthScope::table("app", "hidden", [Delete, Create]),
                ],
            )
            .unwrap();
        assert_eq!(
            session
                .execute_sql("WITH q AS (SELECT * FROM allowed) SELECT * FROM q")
                .unwrap()[0]
                .rows
                .len(),
            1
        );
        for sql in [
            "WITH q AS (SELECT * FROM hidden) SELECT * FROM q",
            "WITH RECURSIVE hidden AS (SELECT * FROM hidden) SELECT * FROM hidden",
            "WITH RECURSIVE hidden AS (SELECT * FROM hidden UNION ALL SELECT id+1 FROM hidden WHERE id<3) SELECT * FROM hidden",
            "DELETE FROM hidden RETURNING *",
            "CREATE OR REPLACE TABLE hidden(id INT)",
            "CREATE TABLE hidden AS SELECT * FROM allowed",
        ] {
            assert!(session.execute_sql(sql).is_err(), "scope bypass: {sql}");
        }
        assert_eq!(
            engine.execute_sql("SELECT * FROM hidden").unwrap()[0]
                .rows
                .len(),
            1
        );
    }
}

#[cfg(test)]
mod filter_authorization {
    use crate::sql::engine::{AuthPrivilege, AuthScope};
    use crate::*;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    struct Rewrite(Arc<AtomicUsize>);
    impl QueryFilter for Rewrite {
        async fn filter(&self, request: &mut QueryRequest) -> anyhow::Result<QueryFilterAction> {
            self.0.fetch_add(1, Ordering::Relaxed);
            request.sql = "SELECT * FROM hidden".into();
            Ok(QueryFilterAction::Continue)
        }
    }
    #[test]
    fn authorization_runs_before_filters_and_after_rewrites() {
        let engine = Engine::default();
        engine
            .execute_sql("CREATE TABLE allowed(id INT); CREATE TABLE hidden(id INT)")
            .unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        engine.query_filters().push(Rewrite(calls.clone()));
        let mut session = engine.session();
        session
            .authenticate_external(
                "reader".into(),
                vec![AuthScope::table("app", "allowed", [AuthPrivilege::Select])],
            )
            .unwrap();
        assert!(session.execute_sql("SELECT * FROM hidden").is_err());
        assert_eq!(calls.load(Ordering::Relaxed), 0);
        assert!(session.execute_sql("SELECT * FROM allowed").is_err());
        assert_eq!(calls.load(Ordering::Relaxed), 1);
    }
    struct Reenter(Engine);
    impl QueryFilter for Reenter {
        async fn filter(&self, _: &mut QueryRequest) -> anyhow::Result<QueryFilterAction> {
            self.0.execute_sql("SELECT 1")?;
            Ok(QueryFilterAction::Continue)
        }
    }
    #[test]
    fn callback_reentry_returns_an_error() {
        let engine = Engine::default();
        engine.query_filters().push(Reenter(engine.clone()));
        assert!(
            engine
                .execute_sql("SELECT 1")
                .unwrap_err()
                .to_string()
                .contains("callbacks")
        );
        engine.query_filters().clear();
    }
}

#[cfg(test)]
mod joined_scopes {
    use crate::{
        Engine,
        sql::engine::{AuthPrivilege::*, AuthScope},
    };
    #[test]
    fn update_join_requires_write_on_target_and_read_on_source() {
        let engine = Engine::default();
        engine.execute_sql("CREATE TABLE target(id INT PRIMARY KEY, value INT); CREATE TABLE source(id INT PRIMARY KEY, value INT); INSERT INTO target VALUES(1,0); INSERT INTO source VALUES(1,7)").unwrap();
        let mut session = engine.session();
        session
            .authenticate_external(
                "writer".into(),
                vec![
                    AuthScope::table("app", "target", [Update]),
                    AuthScope::table("app", "source", [Select]),
                ],
            )
            .unwrap();
        session
            .execute_sql("UPDATE target AS t JOIN source AS s ON t.id=s.id SET t.value=s.value")
            .unwrap();
        assert_eq!(
            engine.execute_sql("SELECT value FROM target").unwrap()[0].rows[0]["value"],
            serde_json::json!(7)
        );
        assert!(
            session
                .execute_sql("UPDATE source AS s JOIN target AS t ON s.id=t.id SET s.value=9")
                .is_err()
        );
    }
}

#[cfg(test)]
mod scoped_key_changes {
    use crate::{
        Engine,
        sql::engine::{AuthPrivilege::*, AuthScope},
    };

    #[test]
    fn key_changes_use_statement_privileges_and_check_cascades() {
        let engine = Engine::default();
        engine.execute_sql("CREATE TABLE parent(id INT PRIMARY KEY); CREATE TABLE child(id INT PRIMARY KEY, parent_id INT, FOREIGN KEY(parent_id) REFERENCES parent(id) ON UPDATE CASCADE); INSERT INTO parent VALUES(1); INSERT INTO child VALUES(1,1)").unwrap();
        let mut session = engine.session();
        session
            .authenticate_external(
                "writer".into(),
                vec![AuthScope::table("app", "parent", [Update])],
            )
            .unwrap();
        assert!(session.execute_sql("UPDATE parent SET id=2").is_err());
        assert_eq!(
            engine.execute_sql("SELECT id FROM parent").unwrap()[0].rows[0]["id"],
            serde_json::json!(1)
        );
        session
            .authenticate_external(
                "writer".into(),
                vec![
                    AuthScope::table("app", "parent", [Update]),
                    AuthScope::table("app", "child", [Update]),
                ],
            )
            .unwrap();
        session.execute_sql("UPDATE parent SET id=2").unwrap();
        assert_eq!(
            engine.execute_sql("SELECT parent_id FROM child").unwrap()[0].rows[0]["parent_id"],
            serde_json::json!(2)
        );

        engine
            .execute_sql(
                "CREATE TABLE items(id INT PRIMARY KEY, value INT); INSERT INTO items VALUES(1,1)",
            )
            .unwrap();
        session
            .authenticate_external(
                "writer".into(),
                vec![AuthScope::table("app", "items", [Insert, Delete])],
            )
            .unwrap();
        session
            .execute_sql("REPLACE INTO items VALUES(1,2)")
            .unwrap();
        assert_eq!(
            engine.execute_sql("SELECT value FROM items").unwrap()[0].rows[0]["value"],
            serde_json::json!(2)
        );
        session
            .authenticate_external(
                "writer".into(),
                vec![AuthScope::table("app", "items", [Insert, Update])],
            )
            .unwrap();
        session
            .execute_sql("INSERT INTO items VALUES(1,3) ON DUPLICATE KEY UPDATE id=2")
            .unwrap();
        assert_eq!(
            engine.execute_sql("SELECT id FROM items").unwrap()[0].rows[0]["id"],
            serde_json::json!(2)
        );
    }
}
