//! Async engine, storage, and execution-filter APIs.

use crate::sql::engine::{HookRegistry, QueryHookEvent, QueryHookOptions, QueryHookSubscription};
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Result, anyhow};
use serde_json::Value;

use crate::sql::engine::{Engine, EngineConfig, EngineSession, EngineState, QueryResult};
use crate::storage::{
    AsyncStorage, DatabaseMetadata, LuxStorage, MetadataMutation, RowMutation, RowScan,
    StorageBatch, StorageCatalog, TableState,
};

/// A SQL request at the filter boundary.
///
/// Parameterized statements are bound before filters run.  Rewriting `sql`
/// therefore applies to the exact statement that will execute.
#[derive(Debug, Clone)]
pub struct QueryRequest {
    pub sql: String,
    pub parameters: Vec<Value>,
    pub prepared: bool,
}

/// The decision made by a query filter.
#[derive(Debug, Clone)]
pub enum QueryFilterAction {
    /// Execute the (possibly mutated) request.
    Continue,
    /// Stop before execution and return this error to the caller.
    Reject(String),
    /// Stop before execution and return these synthetic results.
    Return(Vec<QueryResult>),
}

/// The decision made by a result filter.
#[derive(Debug, Clone)]
pub enum ResultFilterAction {
    /// Return the (possibly mutated) result set.
    Continue,
    /// Replace the result set.
    Replace(Vec<QueryResult>),
    /// Do not return the result set to the caller.
    Reject(String),
}

/// Hook invoked for every SQL execution before authorization and execution.
///
/// Native async traits keep implementations straightforward.  Use
/// [`QueryFilters`] when several independently typed filters need to form an
/// ordered pipeline.
#[allow(async_fn_in_trait)]
pub trait QueryFilter: Send + Sync + 'static {
    async fn filter(&self, request: &mut QueryRequest) -> Result<QueryFilterAction>;
}

/// Hook invoked for every completed SQL execution.
#[allow(async_fn_in_trait)]
pub trait ResultFilter: Send + Sync + 'static {
    async fn filter(
        &self,
        request: &QueryRequest,
        results: &mut Vec<QueryResult>,
    ) -> Result<ResultFilterAction>;
}

trait ErasedQueryFilter: Send + Sync {
    fn filter<'a>(
        &'a self,
        request: &'a mut QueryRequest,
    ) -> Pin<Box<dyn Future<Output = Result<QueryFilterAction>> + 'a>>;
}

impl<T: QueryFilter> ErasedQueryFilter for T {
    fn filter<'a>(
        &'a self,
        request: &'a mut QueryRequest,
    ) -> Pin<Box<dyn Future<Output = Result<QueryFilterAction>> + 'a>> {
        Box::pin(QueryFilter::filter(self, request))
    }
}

trait ErasedResultFilter: Send + Sync {
    fn filter<'a>(
        &'a self,
        request: &'a QueryRequest,
        results: &'a mut Vec<QueryResult>,
    ) -> Pin<Box<dyn Future<Output = Result<ResultFilterAction>> + 'a>>;
}

impl<T: ResultFilter> ErasedResultFilter for T {
    fn filter<'a>(
        &'a self,
        request: &'a QueryRequest,
        results: &'a mut Vec<QueryResult>,
    ) -> Pin<Box<dyn Future<Output = Result<ResultFilterAction>> + 'a>> {
        Box::pin(ResultFilter::filter(self, request, results))
    }
}

/// A mutable, ordered heterogeneous query-filter pipeline.
#[derive(Default)]
pub struct QueryFilters {
    filters: parking_lot::RwLock<Vec<Arc<dyn ErasedQueryFilter>>>,
}

impl QueryFilters {
    pub fn push<F: QueryFilter>(&self, filter: F) {
        self.filters.write().push(Arc::new(filter));
    }

    pub fn clear(&self) {
        self.filters.write().clear();
    }

    pub fn len(&self) -> usize {
        self.filters.read().len()
    }

    pub fn is_empty(&self) -> bool {
        self.filters.read().is_empty()
    }

    async fn apply(&self, request: &mut QueryRequest) -> Result<Option<Vec<QueryResult>>> {
        let filters = self.filters.read().clone();
        for filter in filters {
            match filter.filter(request).await? {
                QueryFilterAction::Continue => {}
                QueryFilterAction::Reject(reason) => return Err(anyhow!(reason)),
                QueryFilterAction::Return(results) => return Ok(Some(results)),
            }
        }
        Ok(None)
    }
}

/// A mutable, ordered heterogeneous result-filter pipeline.
#[derive(Default)]
pub struct ResultFilters {
    filters: parking_lot::RwLock<Vec<Arc<dyn ErasedResultFilter>>>,
}

impl ResultFilters {
    pub fn push<F: ResultFilter>(&self, filter: F) {
        self.filters.write().push(Arc::new(filter));
    }

    pub fn clear(&self) {
        self.filters.write().clear();
    }

    pub fn len(&self) -> usize {
        self.filters.read().len()
    }

    pub fn is_empty(&self) -> bool {
        self.filters.read().is_empty()
    }

    async fn apply(&self, request: &QueryRequest, results: &mut Vec<QueryResult>) -> Result<()> {
        let filters = self.filters.read().clone();
        for filter in filters {
            match filter.filter(request, results).await? {
                ResultFilterAction::Continue => {}
                ResultFilterAction::Replace(replacement) => *results = replacement,
                ResultFilterAction::Reject(reason) => return Err(anyhow!(reason)),
            }
        }
        Ok(())
    }
}

struct Inner<S> {
    hooks: Arc<HookRegistry>,
    poisoned: AtomicBool,
    engine: Mutex<Engine>,
    storage: S,
    storage_initialized: Mutex<bool>,
    commit_gate: tokio::sync::Mutex<()>,
    query_filters: QueryFilters,
    result_filters: ResultFilters,
}

const STORAGE_PAGE_SIZE: usize = 512;

async fn load_state<S: AsyncStorage>(storage: &S) -> Result<Option<EngineState>> {
    let Some(catalog) = storage.load_catalog().await? else {
        return Ok(None);
    };
    if catalog.version != 1 {
        return Err(anyhow!(
            "unsupported async storage catalog version: {}",
            catalog.version
        ));
    }
    let mut databases = std::collections::BTreeMap::new();
    for (database, metadata) in catalog.databases {
        let mut schemas = std::collections::BTreeMap::new();
        let mut rows = std::collections::BTreeMap::new();
        let mut auto_inc = std::collections::BTreeMap::new();
        for table in storage.list_tables(&database).await? {
            let Some(table_state) = storage.load_table(&database, &table).await? else {
                continue;
            };
            schemas.insert(table.clone(), table_state.schema);
            if let Some(value) = table_state.auto_increment {
                auto_inc.insert(table.clone(), value);
            }
            let mut table_rows = std::collections::BTreeMap::new();
            let mut cursor = None;
            loop {
                let page = storage
                    .scan_rows(RowScan {
                        database: database.clone(),
                        table: table.clone(),
                        cursor: cursor.clone(),
                        limit: STORAGE_PAGE_SIZE,
                    })
                    .await?;
                table_rows.extend(page.rows);
                let Some(next_cursor) = page.next_cursor else {
                    break;
                };
                if cursor.as_ref() == Some(&next_cursor) {
                    return Err(anyhow!("async storage returned a non-advancing row cursor"));
                }
                cursor = Some(next_cursor);
            }
            rows.insert(table, table_rows);
        }
        databases.insert(
            database,
            crate::sql::engine::Snapshot {
                version: 1,
                created_at: chrono::Utc::now().to_rfc3339(),
                schemas,
                rows,
                auto_inc,
                views: metadata.views,
                index_comments: metadata.index_comments,
            },
        );
    }
    Ok(Some(EngineState {
        version: 1,
        metadata: catalog.metadata,
        databases,
    }))
}

fn storage_catalog(state: &EngineState) -> StorageCatalog {
    StorageCatalog {
        version: 1,
        metadata: state.metadata.clone(),
        databases: state
            .databases
            .iter()
            .map(|(database, snapshot)| {
                (
                    database.clone(),
                    DatabaseMetadata {
                        views: snapshot.views.clone(),
                        index_comments: snapshot.index_comments.clone(),
                    },
                )
            })
            .collect(),
    }
}

fn storage_batch(before: &EngineState, after: &EngineState) -> StorageBatch {
    let mut batch = StorageBatch::default();
    if storage_catalog(before) != storage_catalog(after) {
        batch
            .metadata
            .push(MetadataMutation::PutCatalog(storage_catalog(after)));
    }
    for (database, previous) in &before.databases {
        if !after.databases.contains_key(database) {
            batch.metadata.push(MetadataMutation::DeleteDatabase {
                database: database.clone(),
            });
            for table in previous.schemas.keys() {
                batch.metadata.push(MetadataMutation::DeleteTable {
                    database: database.clone(),
                    table: table.clone(),
                });
            }
        }
    }
    for (database, current) in &after.databases {
        let previous = before.databases.get(database);
        let mut tables = std::collections::BTreeSet::new();
        if let Some(previous) = previous {
            tables.extend(previous.schemas.keys().cloned());
            tables.extend(previous.rows.keys().cloned());
        }
        tables.extend(current.schemas.keys().cloned());
        tables.extend(current.rows.keys().cloned());
        for table in tables {
            let old_schema = previous.and_then(|snapshot| snapshot.schemas.get(&table));
            let new_schema = current.schemas.get(&table);
            match (old_schema, new_schema) {
                (_, None) => batch.metadata.push(MetadataMutation::DeleteTable {
                    database: database.clone(),
                    table: table.clone(),
                }),
                (old, Some(schema)) => {
                    let old_auto = previous.and_then(|snapshot| snapshot.auto_inc.get(&table));
                    let new_auto = current.auto_inc.get(&table);
                    if old != Some(schema) || old_auto != new_auto {
                        batch.metadata.push(MetadataMutation::PutTable {
                            database: database.clone(),
                            table: table.clone(),
                            state: TableState {
                                schema: schema.clone(),
                                auto_increment: new_auto.copied(),
                            },
                        });
                    }
                }
            }
            let old_rows = previous.and_then(|snapshot| snapshot.rows.get(&table));
            let new_rows = current.rows.get(&table);
            if let Some(old_rows) = old_rows {
                for key in old_rows.keys() {
                    if new_rows.is_none_or(|rows| !rows.contains_key(key)) {
                        batch.rows.push(RowMutation::Delete {
                            database: database.clone(),
                            table: table.clone(),
                            key: key.clone(),
                        });
                    }
                }
            }
            if let Some(new_rows) = new_rows {
                for (key, row) in new_rows {
                    if old_rows.and_then(|rows| rows.get(key)) != Some(row) {
                        batch.rows.push(RowMutation::Put {
                            database: database.clone(),
                            table: table.clone(),
                            key: key.clone(),
                            row: row.clone(),
                        });
                    }
                }
            }
        }
    }
    batch
}

async fn commit_changes<S: AsyncStorage>(
    inner: &Inner<S>,
    before: EngineState,
    after: EngineState,
) -> Result<()> {
    let mut batch = storage_batch(&before, &after);
    let was_initialized = *inner
        .storage_initialized
        .lock()
        .expect("storage state mutex poisoned");
    if !was_initialized {
        batch
            .metadata
            .insert(0, MetadataMutation::PutCatalog(storage_catalog(&after)));
    }
    inner.storage.commit(batch).await?;
    *inner
        .storage_initialized
        .lock()
        .expect("storage state mutex poisoned") = true;
    Ok(())
}

/// An async MySqweel engine with one active storage backend.
///
/// `S` is generic, so an application can use a CSV, HTTP, or database-backed
/// implementation of [`AsyncStorage`].  The built-in [`LuxStorage`] is the
/// only backend supplied by this crate.
pub struct AsyncEngine<S = LuxStorage> {
    inner: Arc<Inner<S>>,
}

impl<S: AsyncStorage> AsyncEngine<S> {
    /// Open an engine from the supplied backend's complete persisted state.
    pub async fn open(config: EngineConfig, storage: S) -> Result<Self> {
        let engine = Engine::new(config);
        let hooks = engine.hooks();
        hooks.external_reads.store(true, Ordering::Relaxed);
        let loaded = load_state(&storage).await?;
        let storage_initialized = loaded.is_some();
        if let Some(state) = loaded {
            engine.import_state(state)?;
        }
        Ok(Self {
            inner: Arc::new(Inner {
                hooks,
                poisoned: AtomicBool::new(false),
                engine: Mutex::new(engine),
                storage,
                storage_initialized: Mutex::new(storage_initialized),
                commit_gate: tokio::sync::Mutex::new(()),
                query_filters: QueryFilters::default(),
                result_filters: ResultFilters::default(),
            }),
        })
    }

    /// Subscribe to committed changes and client-visible reads on this engine.
    pub fn subscribe_query_hooks<F, Fut>(
        &self,
        options: QueryHookOptions,
        callback: F,
    ) -> Result<QueryHookSubscription>
    where
        F: Fn(Arc<QueryHookEvent>) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<()>> + Send + 'static,
    {
        self.inner.hooks.subscribe(options, callback)
    }

    /// The ordered query filters for this engine.
    pub fn query_filters(&self) -> &QueryFilters {
        &self.inner.query_filters
    }

    /// The ordered result filters for this engine.
    pub fn result_filters(&self) -> &ResultFilters {
        &self.inner.result_filters
    }

    pub async fn execute_sql(&self, sql: impl Into<String>) -> Result<Vec<QueryResult>> {
        self.execute(QueryRequest {
            sql: sql.into(),
            parameters: Vec::new(),
            prepared: false,
        })
        .await
    }

    /// Execute a parameterized statement.  Parameters are bound before the
    /// query-filter pipeline, including for prepared-statement callers.
    pub async fn execute_sql_with_params(
        &self,
        sql: impl Into<String>,
        parameters: Vec<Value>,
    ) -> Result<Vec<QueryResult>> {
        let sql = crate::sql::engine::bind_params(&sql.into(), &parameters)?;
        self.execute(QueryRequest {
            sql,
            parameters,
            prepared: true,
        })
        .await
    }

    pub fn session(&self) -> AsyncEngineSession<S> {
        let session = self
            .inner
            .engine
            .lock()
            .expect("engine mutex poisoned")
            .session();
        AsyncEngineSession {
            inner: self.inner.clone(),
            session: Mutex::new(session),
        }
    }

    pub async fn snapshot(&self) -> Result<EngineState> {
        Ok(self
            .inner
            .engine
            .lock()
            .expect("engine mutex poisoned")
            .export_state()?)
    }

    async fn execute(&self, request: QueryRequest) -> Result<Vec<QueryResult>> {
        execute_request(&self.inner, request, None).await
    }
}

impl AsyncEngine<LuxStorage> {
    pub async fn open_lux(
        config: EngineConfig,
        data_dir: Option<std::path::PathBuf>,
    ) -> Result<Self> {
        Self::open(config, LuxStorage::open(data_dir).await?).await
    }
}

/// A connection-owned async session.  It preserves transactions, prepared
/// execution semantics, and session variables just like `EngineSession`.
pub struct AsyncEngineSession<S> {
    inner: Arc<Inner<S>>,
    session: Mutex<EngineSession>,
}

impl<S: AsyncStorage> AsyncEngineSession<S> {
    pub async fn execute_sql(&self, sql: impl Into<String>) -> Result<Vec<QueryResult>> {
        self.execute(QueryRequest {
            sql: sql.into(),
            parameters: Vec::new(),
            prepared: false,
        })
        .await
    }

    pub async fn execute_sql_with_params(
        &self,
        sql: impl Into<String>,
        parameters: Vec<Value>,
    ) -> Result<Vec<QueryResult>> {
        let sql = crate::sql::engine::bind_params(&sql.into(), &parameters)?;
        self.execute(QueryRequest {
            sql,
            parameters,
            prepared: true,
        })
        .await
    }

    async fn execute(&self, request: QueryRequest) -> Result<Vec<QueryResult>> {
        execute_request(&self.inner, request, Some(&self.session)).await
    }
}

// An external storage failure/cancellation leaves the in-memory commit uncertain.
// Refuse further work and never release the buffered feed in that case.
struct CommitHookGuard<'a> {
    hooks: &'a HookRegistry,
    poisoned: &'a AtomicBool,
    armed: bool,
}
impl CommitHookGuard<'_> {
    fn finish(mut self) {
        self.hooks.end_deferred(true);
        self.armed = false;
    }
}
impl Drop for CommitHookGuard<'_> {
    fn drop(&mut self) {
        if self.armed {
            self.hooks.end_deferred(false);
            self.poisoned.store(true, Ordering::Release);
        }
    }
}

async fn execute_request<S: AsyncStorage>(
    inner: &Inner<S>,
    mut request: QueryRequest,
    session: Option<&Mutex<EngineSession>>,
) -> Result<Vec<QueryResult>> {
    let check = || -> Result<()> {
        anyhow::ensure!(
            !inner.poisoned.load(Ordering::Acquire),
            "database commit outcome uncertain; close and reopen MySqweel"
        );
        Ok(())
    };
    check()?;
    if let Some(mut results) = inner.query_filters.apply(&mut request).await? {
        let database = match session {
            Some(session) => session
                .lock()
                .expect("session mutex poisoned")
                .current_database()
                .to_owned(),
            None => {
                inner
                    .engine
                    .lock()
                    .expect("engine mutex poisoned")
                    .hook_context()
                    .0
            }
        };
        inner.result_filters.apply(&request, &mut results).await?;
        check()?;
        if crate::sql::parse(&request.sql).is_ok_and(|items| {
            items
                .iter()
                .any(|s| matches!(s, sqlparser::ast::Statement::Query(_)))
        }) {
            inner.hooks.read(&database, &request.sql, &results);
        }
        return Ok(results);
    }
    let _commit = inner.commit_gate.lock().await;
    check()?;
    inner.hooks.begin_deferred();
    let pending = CommitHookGuard {
        hooks: &inner.hooks,
        poisoned: &inner.poisoned,
        armed: true,
    };
    let (results, before, after, database, read) = {
        let engine = inner.engine.lock().expect("engine mutex poisoned");
        let before = engine.export_state()?;
        let (results, database, read) = match session {
            Some(session) => {
                let mut session = session.lock().expect("session mutex poisoned");
                let database = session.current_database().to_owned();
                let results = session.execute_sql(&request.sql);
                (results, database, session.last_query_read)
            }
            None => {
                let database = engine.hook_context().0;
                let results = engine.execute_sql(&request.sql);
                (results, database, engine.hook_context().1)
            }
        };
        (results, before, engine.export_state()?, database, read)
    };
    // Persist even when a later statement failed after earlier autocommits.
    commit_changes(inner, before, after).await?;
    pending.finish();
    drop(_commit);
    let mut results = results?;
    inner.result_filters.apply(&request, &mut results).await?;
    if read {
        inner.hooks.read(&database, &request.sql, &results);
    }
    Ok(results)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::sync::atomic::{AtomicUsize, Ordering};

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
        engine: &AsyncEngine<TestStore>,
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
        let engine = AsyncEngine::open(EngineConfig::default(), store.clone())
            .await
            .unwrap();
        engine
            .execute_sql("CREATE TABLE items (id INT PRIMARY KEY, v INT)")
            .await
            .unwrap();
        let (_subscription, mut rx) = hook_receiver(&engine);
        store.block_commit.store(true, Ordering::Relaxed);
        let execution = engine.execute_sql("INSERT INTO items VALUES (1, 10)");
        tokio::pin!(execution);
        tokio::select! {
            _ = store.entered.notified() => {},
            result = &mut execution => panic!("commit did not block: {result:?}"),
        }
        tokio::task::yield_now().await;
        assert!(rx.try_recv().is_err());
        engine.query_filters().push(Synthetic);
        engine.execute_sql("SELECT cached").await.unwrap();
        assert!(
            matches!(&*hook_next(&mut rx).await, QueryHookEvent::Read { results, .. } if results[0].columns == ["cached"])
        );
        store.block_commit.store(false, Ordering::Relaxed);
        store.release.notify_one();
        execution.await.unwrap();
        assert!(
            matches!(&*hook_next(&mut rx).await, QueryHookEvent::Write { rows, .. } if rows[0].row["v"] == Value::from(10))
        );
        let session = engine.session();
        session
            .execute_sql("BEGIN; UPDATE items SET v=20; UPDATE items SET v=30")
            .await
            .unwrap();
        tokio::task::yield_now().await;
        assert!(rx.try_recv().is_err());
        session.execute_sql("COMMIT").await.unwrap();
        assert!(
            matches!(&*hook_next(&mut rx).await, QueryHookEvent::Write { rows, .. } if rows.len() == 1 && rows[0].row["v"] == Value::from(30))
        );
        assert!(
            session
                .execute_sql(
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
    async fn failed_or_cancelled_storage_discards_hooks_and_stops_execution() {
        for cancel in [false, true] {
            let store = TestStore::default();
            let engine = AsyncEngine::open(EngineConfig::default(), store.clone())
                .await
                .unwrap();
            engine
                .execute_sql("CREATE TABLE items (id INT PRIMARY KEY)")
                .await
                .unwrap();
            let (_subscription, mut rx) = hook_receiver(&engine);
            if cancel {
                store.block_commit.store(true, Ordering::Relaxed);
                let execution = engine.execute_sql("INSERT INTO items VALUES (1)");
                tokio::pin!(execution);
                tokio::select! {
                    _ = store.entered.notified() => {},
                    result = &mut execution => panic!("commit did not block: {result:?}"),
                }
                // Dropping the pending execution must discard its buffered events.
            } else {
                store.fail_commit.store(true, Ordering::Relaxed);
                assert!(
                    engine
                        .execute_sql("INSERT INTO items VALUES (1)")
                        .await
                        .is_err()
                );
            }
            tokio::task::yield_now().await;
            assert!(rx.try_recv().is_err());
            for result in [
                engine.execute_sql("SELECT * FROM items").await,
                engine.session().execute_sql("SELECT * FROM items").await,
            ] {
                assert!(result.unwrap_err().to_string().contains("reopen"));
            }
        }
    }

    #[tokio::test]
    async fn hook_queue_capacity_also_bounds_deferred_autocommits() {
        let engine = AsyncEngine::open(EngineConfig::default(), TestStore::default())
            .await
            .unwrap();
        engine
            .execute_sql("CREATE TABLE items (id INT PRIMARY KEY)")
            .await
            .unwrap();
        let delivered = Arc::new(AtomicUsize::new(0));
        let count = delivered.clone();
        let mut subscription = engine
            .subscribe_query_hooks(
                QueryHookOptions {
                    capacity: 1,
                    ..Default::default()
                },
                move |_| {
                    count.fetch_add(1, Ordering::Relaxed);
                    async { Ok(()) }
                },
            )
            .unwrap();
        engine
            .execute_sql("INSERT INTO items VALUES (1); INSERT INTO items VALUES (2)")
            .await
            .unwrap();
        assert_eq!(
            subscription.wait().await,
            Err(crate::QueryHookError::Overflow)
        );
        assert_eq!(delivered.load(Ordering::Relaxed), 0);
        assert_eq!(
            engine.execute_sql("SELECT * FROM items").await.unwrap()[0]
                .rows
                .len(),
            2
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
        let engine = AsyncEngine::open(EngineConfig::default(), TestStore::default())
            .await
            .unwrap();
        engine.query_filters().push(Synthetic);
        engine.result_filters().push(HookResultFilter);
        let (_subscription, mut rx) = hook_receiver(&engine);
        engine.execute_sql("SELECT cached").await.unwrap();
        assert!(
            matches!(&*hook_next(&mut rx).await, QueryHookEvent::Read { results, .. } if results[0].columns == ["cached"] && results[0].rows.is_empty())
        );
        engine
            .session()
            .execute_sql("SELECT 1 AS replacement")
            .await
            .unwrap();
        assert!(
            matches!(&*hook_next(&mut rx).await, QueryHookEvent::Read { results, .. } if results[0].rows[0]["replacement"] == Value::from(42))
        );
        assert!(engine.execute_sql("SELECT 1 AS rejected").await.is_err());
        assert!(
            engine
                .session()
                .execute_sql("SELECT 1 AS rejected")
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
        let engine = AsyncEngine::open(EngineConfig::default(), store.clone())
            .await
            .unwrap();
        engine.query_filters().push(Rewrite);
        engine.query_filters().push(Synthetic);
        engine.result_filters().push(HideRows);

        let rewritten = engine.execute_sql("SELECT original").await.unwrap();
        assert_eq!(rewritten[0].columns, ["rewritten"]);
        assert!(rewritten[0].rows.is_empty());

        let cached = engine.execute_sql("SELECT cached").await.unwrap();
        assert_eq!(cached[0].columns, ["cached"]);
        assert!(cached[0].rows.is_empty());
        assert_eq!(store.commits.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn parameters_are_bound_before_filters_and_state_is_reloaded() {
        let store = TestStore::default();
        let engine = AsyncEngine::open(EngineConfig::default(), store.clone())
            .await
            .unwrap();
        engine
            .execute_sql("CREATE TABLE items (id INT PRIMARY KEY, name TEXT)")
            .await
            .unwrap();
        engine
            .execute_sql_with_params(
                "INSERT INTO items VALUES (?, ?)",
                vec![Value::from(1), Value::from("Ada")],
            )
            .await
            .unwrap();

        let reopened = AsyncEngine::open(EngineConfig::default(), store)
            .await
            .unwrap();
        let rows = reopened
            .execute_sql("SELECT name FROM items WHERE id = 1")
            .await
            .unwrap();
        assert_eq!(rows[0].rows[0]["name"], Value::String("Ada".into()));
    }

    #[tokio::test]
    async fn session_commit_is_persisted_as_one_backend_commit() {
        let store = TestStore::default();
        let engine = AsyncEngine::open(EngineConfig::default(), store.clone())
            .await
            .unwrap();
        engine
            .execute_sql("CREATE TABLE events (id INT PRIMARY KEY)")
            .await
            .unwrap();
        let session = engine.session();
        session.execute_sql("BEGIN").await.unwrap();
        session
            .execute_sql("INSERT INTO events VALUES (1)")
            .await
            .unwrap();
        session.execute_sql("COMMIT").await.unwrap();

        let reopened = AsyncEngine::open(EngineConfig::default(), store)
            .await
            .unwrap();
        let rows = reopened.execute_sql("SELECT * FROM events").await.unwrap();
        assert_eq!(rows[0].rows.len(), 1);
    }

    #[tokio::test]
    async fn lux_storage_reopens_async_engine_state() {
        let directory = tempfile::tempdir().unwrap();
        {
            let storage = LuxStorage::open(Some(directory.path().to_path_buf()))
                .await
                .unwrap();
            let engine = AsyncEngine::open(EngineConfig::default(), storage)
                .await
                .unwrap();
            engine
                .execute_sql("CREATE TABLE durable_items (id INT PRIMARY KEY)")
                .await
                .unwrap();
            engine
                .execute_sql("INSERT INTO durable_items VALUES (1)")
                .await
                .unwrap();
        }

        let storage = LuxStorage::open(Some(directory.path().to_path_buf()))
            .await
            .unwrap();
        let engine = AsyncEngine::open(EngineConfig::default(), storage)
            .await
            .unwrap();
        let rows = engine
            .execute_sql("SELECT * FROM durable_items")
            .await
            .unwrap();
        assert_eq!(rows[0].rows.len(), 1);
    }

    #[tokio::test]
    async fn lux_storage_scans_rows_in_bounded_pages() {
        let storage = LuxStorage::open(None).await.unwrap();
        let engine = AsyncEngine::open(EngineConfig::default(), storage.clone())
            .await
            .unwrap();
        engine
            .execute_sql("CREATE TABLE scan_items (id INT PRIMARY KEY)")
            .await
            .unwrap();
        for id in 1..=3 {
            engine
                .execute_sql(&format!("INSERT INTO scan_items VALUES ({id})"))
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
