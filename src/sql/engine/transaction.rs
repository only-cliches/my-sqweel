//! Serialized development writers with private statement/transaction state.
use super::catalog::{
    AdminCommand, Catalog, CatalogEffect, Identity, PreparedCommand, PreparedSource, parse_use,
};
use super::*;
use anyhow::ensure;
use sqlparser::ast::LockType;
mod persistence;
mod session;
use parking_lot::Condvar;
use persistence::Persistence;
pub use session::EngineSession;

#[derive(Clone)]
struct Committed {
    catalog: Catalog,
    databases: BTreeMap<String, Arc<RawEngine>>,
}

struct Coordinator {
    hooks: Arc<HookRegistry>,
    cfg: EngineConfig,
    committed: Mutex<Committed>,
    writer: Mutex<Writers>,
    publication: Mutex<()>,
    available: Condvar,
    advisory_locks: Mutex<HashMap<String, (uuid::Uuid, u32)>>,
    advisory_available: Condvar,
    persistence: Option<Persistence>,
    runtime: Arc<crate::runtime::EngineRuntime>,
    query_filters: Arc<crate::QueryFilters>,
    result_filters: Arc<crate::ResultFilters>,
}

#[derive(Default)]
struct Writers {
    administrative: bool,
    databases: BTreeSet<String>,
}
struct WriterLease {
    shared: Arc<Coordinator>,
    database: Option<String>,
}
impl Drop for WriterLease {
    fn drop(&mut self) {
        let mut writers = self.shared.writer.lock();
        if let Some(database) = &self.database {
            writers.databases.remove(database);
        } else {
            writers.administrative = false;
        }
        self.shared.available.notify_all();
    }
}

/// An embedded database server. Use `session()` for each independent connection.
#[derive(Clone)]
pub struct Engine {
    shared: Arc<Coordinator>,
}

/// Connection-owned state. Dropping a session rolls back uncommitted changes.
pub(crate) struct SessionState {
    pub(crate) last_query_read: bool,
    endpoint_id: Option<uuid::Uuid>,
    endpoint_stop: Option<Arc<std::sync::atomic::AtomicBool>>,
    ceiling: Option<Vec<AuthScope>>,
    shared: Arc<Coordinator>,
    database: String,
    identity: Identity,
    client_host: String,
    transaction: Option<(Option<WriterLease>, Committed)>,
    native_snapshot: Option<crate::storage::RocksDbSnapshot>,
    transaction_read: bool,
    transaction_read_only: bool,
    next_transaction_read_only: bool,
    observed_tables: Option<BTreeSet<String>>,
    observed_columns: BTreeMap<String, Option<BTreeSet<String>>>,
    savepoints: Vec<(String, Committed)>,
    autocommit: bool,
    session_state: Option<Arc<RawEngine>>,
    variables: HashMap<String, Value>,
    prepared: HashMap<String, (String, String)>,
    session_id: uuid::Uuid,
    temporary_tables: BTreeMap<String, Arc<RawEngine>>,
}

impl Default for Engine {
    fn default() -> Self {
        Self::new(EngineConfig::default())
    }
}

impl Engine {
    pub fn new(cfg: EngineConfig) -> Self {
        Self::open(cfg, crate::Storage::Memory).expect("failed to open MySqweel")
    }

    pub async fn open_async(cfg: EngineConfig, storage: crate::Storage) -> Result<Self> {
        tokio::task::spawn_blocking(move || Self::open(cfg, storage)).await?
    }
    pub fn open(cfg: EngineConfig, storage: crate::Storage) -> Result<Self> {
        if let Some(zone) = &cfg.default_time_zone {
            let bytes = zone.as_bytes();
            let valid = bytes.len() == 6
                && matches!(bytes[0], b'+' | b'-')
                && bytes[3] == b':'
                && [1, 2, 4, 5]
                    .iter()
                    .all(|&index| bytes[index].is_ascii_digit());
            if !valid {
                return Err(anyhow!(
                    "default time zone must be a fixed offset such as +00:00 or -10:00"
                ));
            }
            let hours: u32 = zone[1..3].parse()?;
            let minutes: u32 = zone[4..6].parse()?;
            let limit = if bytes[0] == b'-' {
                13 * 60 + 59
            } else {
                14 * 60
            };
            if minutes >= 60 || hours * 60 + minutes > limit {
                return Err(anyhow!("default time zone offset is out of range"));
            }
        }
        let runtime = Arc::new(crate::runtime::EngineRuntime::new()?);
        let persistence = match storage {
            crate::Storage::Memory => None,
            crate::Storage::RocksDb(path) => Some(Persistence::open(
                path.to_str()
                    .ok_or_else(|| anyhow!("storage path must be UTF-8"))?,
            )?),
            crate::Storage::Custom(storage) => Some(Persistence::custom(storage, runtime.clone())),
        };
        let recovered = persistence
            .as_ref()
            .map(|store| store.load(&cfg))
            .transpose()?
            .flatten();
        let state = match recovered {
            Some(state) => state,
            None => Committed {
                catalog: Catalog::default(),
                databases: BTreeMap::from([("app".into(), Arc::new(RawEngine::new(cfg.clone())))]),
            },
        };
        let shared = Arc::new(Coordinator {
            hooks: Arc::new(HookRegistry::default()),
            cfg,
            committed: Mutex::new(state),
            writer: Mutex::new(Writers::default()),
            publication: Mutex::new(()),
            available: Condvar::new(),
            advisory_locks: Mutex::new(HashMap::new()),
            advisory_available: Condvar::new(),
            persistence,
            runtime,
            query_filters: Arc::default(),
            result_filters: Arc::default(),
        });
        Ok(Self { shared })
    }

    /// Replace the bootstrap administrator before accepting connections.
    pub fn set_admin_credentials(&self, username: &str, password: &str) -> Result<()> {
        if username.is_empty() {
            return Err(anyhow!("administrator username must not be empty"));
        }
        let _lease = self.shared.acquire()?;
        let mut state = self.shared.committed.lock().clone();
        state.catalog.set_admin_credentials(username, password);
        self.shared.publish(state)?;
        Ok(())
    }
    pub fn session(&self) -> EngineSession {
        EngineSession {
            inner: Arc::new(Mutex::new(SessionState::new(self.shared.clone()))),
            shared: self.shared.clone(),
        }
    }
    pub fn query_filters(&self) -> &crate::QueryFilters {
        &self.shared.query_filters
    }
    pub fn result_filters(&self) -> &crate::ResultFilters {
        &self.shared.result_filters
    }
    pub fn execute_sql(&self, sql: &str) -> Result<Vec<QueryResult>> {
        self.session().execute_sql(sql)
    }
    pub fn execute_sql_with_params(&self, sql: &str, params: &[Value]) -> Result<Vec<QueryResult>> {
        self.session().execute_sql_with_params(sql, params)
    }
    pub async fn execute_sql_async(&self, sql: impl Into<String>) -> Result<Vec<QueryResult>> {
        self.session().execute_sql_async(sql).await
    }
    pub async fn execute_sql_with_params_async(
        &self,
        sql: impl Into<String>,
        params: Vec<Value>,
    ) -> Result<Vec<QueryResult>> {
        self.session()
            .execute_sql_with_params_async(sql, params)
            .await
    }
    pub fn execute_statement(&self, statement: Statement) -> Result<QueryResult> {
        self.execute_sql(&statement.to_string())?
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("empty statement"))
    }
    /// Subscribe to engine-wide events on the current Tokio runtime.
    /// The callback receives committed changes and must be trusted with all databases.
    pub fn subscribe_query_hooks<F, Fut>(
        &self,
        options: QueryHookOptions,
        callback: F,
    ) -> Result<QueryHookSubscription>
    where
        F: Fn(Arc<QueryHookEvent>) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = Result<()>> + Send + 'static,
    {
        self.shared.hooks.subscribe(options, callback)
    }
    pub fn subscribe_query_events(&self, options: QueryEventOptions) -> QueryEventStream {
        self.shared.committed.lock().databases["app"].subscribe_query_events(options)
    }
    pub fn snapshot(&self) -> Snapshot {
        self.try_snapshot()
            .expect("could not read RocksDB rows for snapshot")
    }
    pub fn try_snapshot(&self) -> Result<Snapshot> {
        let _publication = self.shared.publication.lock();
        let raw = self.shared.committed.lock().databases["app"].fork()?;
        if let Some(persistence) = &self.shared.persistence {
            persistence.hydrate_all(&raw)?;
        }
        Ok(raw.snapshot())
    }

    /// Export every database together with its catalog for an external
    /// storage backend.  The result is intended to be committed as one unit.
    pub fn export_state(&self) -> Result<EngineState> {
        let _publication = self.shared.publication.lock();
        let state = self.shared.committed.lock();
        let mut databases = BTreeMap::new();
        for (name, raw) in &state.databases {
            let raw = raw.fork()?;
            if let Some(persistence) = &self.shared.persistence {
                persistence.hydrate_all(&raw)?;
            }
            databases.insert(name.clone(), raw.snapshot());
        }
        Ok(EngineState {
            version: 1,
            metadata: serde_json::to_value(&state.catalog)?,
            databases,
        })
    }

    /// Replace the complete committed state from an external storage backend.
    /// Existing sessions keep their connection-local transaction state, so
    /// this is intended for opening an engine before sessions are created.
    pub fn import_state(&self, image: EngineState) -> Result<()> {
        if image.version != 1 {
            return Err(anyhow!(
                "unsupported engine state version: {}",
                image.version
            ));
        }
        if !image.databases.contains_key("app") {
            return Err(anyhow!(
                "external storage state is missing the app database"
            ));
        }
        let catalog: Catalog = serde_json::from_value(image.metadata)?;
        let mut databases = BTreeMap::new();
        for (name, snapshot) in image.databases {
            if snapshot.version != 1 {
                return Err(anyhow!(
                    "unsupported snapshot version: {}",
                    snapshot.version
                ));
            }
            let mut raw = RawEngine::new(self.shared.cfg.clone());
            raw.database_name = name.clone();
            raw.apply_snapshot(snapshot);
            databases.insert(name, Arc::new(raw));
        }
        let committed = Committed { catalog, databases };
        self.shared.publish_quietly(committed)?;
        Ok(())
    }
    pub fn restore_snapshot(&self, snapshot: Snapshot) -> Result<()> {
        if snapshot.version != 1 {
            return Err(anyhow!(
                "unsupported snapshot version: {}",
                snapshot.version
            ));
        }
        self.mutate_with_events(false, |raw| {
            raw.apply_snapshot(snapshot);
            Ok(())
        })
    }
    pub fn drift_report(&self) -> Value {
        let _publication = self.shared.publication.lock();
        let raw = self.shared.committed.lock().databases["app"]
            .fork()
            .expect("could not copy database metadata");
        if let Some(persistence) = &self.shared.persistence {
            persistence
                .hydrate_all(&raw)
                .expect("could not read RocksDB rows for drift report");
        }
        raw.drift_report()
    }
    pub fn rebuild_indexes_for_table(&self, table: &str) -> Result<()> {
        self.mutate(|raw| raw.rebuild_indexes_for_table(table))
    }
    pub fn rebuild_indexes_for_all_tables(&self) -> Result<()> {
        self.mutate(|raw| {
            raw.rebuild_indexes_for_all_tables();
            Ok(())
        })
    }
    pub fn swap_tables(&self, swaps: &[(String, String)]) -> Result<()> {
        self.mutate(|raw| {
            for (left, right) in swaps {
                if left == right {
                    continue;
                }
                let temporary = format!("__sqw_swap_{}", uuid::Uuid::new_v4());
                raw.rename_table(left, &temporary)?;
                raw.rename_table(right, left)?;
                raw.rename_table(&temporary, right)?;
            }
            Ok(())
        })
    }
    pub fn reset_all_rows(&self) -> Result<()> {
        self.mutate(RawEngine::reset_all_rows)
    }
    pub fn reset_table_rows(&self, table: &str) -> Result<()> {
        self.mutate(|raw| raw.reset_table_rows(table))
    }
    pub fn upsert_json_documents(
        &self,
        table: &str,
        rows: Vec<Map<String, Value>>,
        merge: bool,
    ) -> Result<u64> {
        self.mutate(|raw| raw.upsert_json_documents(table, rows, merge))
    }
    pub fn seed_json_rows(
        &self,
        table: &str,
        rows: Vec<Map<String, Value>>,
        mode: SeedMode,
    ) -> Result<SeedReport> {
        self.mutate(|raw| raw.seed_json_rows(table, rows, mode))
    }
    fn mutate<T>(&self, operation: impl FnOnce(&RawEngine) -> Result<T>) -> Result<T> {
        self.mutate_with_events(true, operation)
    }
    fn mutate_with_events<T>(
        &self,
        emit: bool,
        operation: impl FnOnce(&RawEngine) -> Result<T>,
    ) -> Result<T> {
        let _lease = self.shared.acquire_database("app")?;
        let mut state = self.shared.committed.lock().clone();
        let raw = state.databases["app"].fork()?;
        if let Some(persistence) = &self.shared.persistence {
            persistence.hydrate_all(&raw)?;
        }
        let value = operation(&raw)?;
        state.databases.insert("app".into(), Arc::new(raw));
        self.shared
            .publish_database_with_events("app", state, emit)?;
        Ok(value)
    }
    #[cfg(test)]
    pub(super) fn can_parse_without_compat_rewrites(&self, sql: &str) -> bool {
        self.shared.committed.lock().databases["app"].can_parse_without_compat_rewrites(sql)
    }
}

impl Coordinator {
    fn check(&self) -> Result<()> {
        if self
            .persistence
            .as_ref()
            .is_some_and(Persistence::is_poisoned)
        {
            return Err(anyhow!(
                "database commit outcome uncertain; close and reopen MySqweel"
            ));
        }
        Ok(())
    }
    fn acquire(self: &Arc<Self>) -> Result<WriterLease> {
        self.acquire_scope(None)
    }
    fn acquire_database(self: &Arc<Self>, database: &str) -> Result<WriterLease> {
        self.acquire_scope(Some(database))
    }
    fn acquire_scope(self: &Arc<Self>, database: Option<&str>) -> Result<WriterLease> {
        self.check()?;
        let deadline = Instant::now() + StdDuration::from_secs(5);
        let mut writers = self.writer.lock();
        while writers.administrative
            || match database {
                Some(database) => writers.databases.contains(database),
                None => !writers.databases.is_empty(),
            }
        {
            if self
                .available
                .wait_until(&mut writers, deadline)
                .timed_out()
            {
                return Err(anyhow!(
                    "Lock wait timeout exceeded; try restarting transaction"
                ));
            }
        }
        self.check()?;
        if let Some(database) = database {
            writers.databases.insert(database.to_owned());
        } else {
            writers.administrative = true;
        }
        Ok(WriterLease {
            shared: self.clone(),
            database: database.map(str::to_owned),
        })
    }
    fn publish_database(&self, database: &str, pending: Committed) -> Result<()> {
        self.publish_database_with_events(database, pending, true)
    }
    fn publish_database_with_events(
        &self,
        database: &str,
        pending: Committed,
        emit: bool,
    ) -> Result<()> {
        // Writers in other databases can commit while this transaction is open.
        // Serialize durable publication and merge only this lease's database.
        let _publication = self.publication.lock();
        let mut current = self.committed.lock().clone();
        let raw = pending
            .databases
            .get(database)
            .ok_or_else(|| anyhow!("Unknown database: {database}"))?
            .clone();
        current.databases.insert(database.to_owned(), raw);
        self.publish_locked(current, emit)
    }
    fn publish(&self, state: Committed) -> Result<()> {
        let _publication = self.publication.lock();
        self.publish_locked(state, true)
    }
    fn publish_quietly(&self, state: Committed) -> Result<()> {
        let _publication = self.publication.lock();
        self.publish_locked(state, false)
    }
    fn publish_locked(&self, state: Committed, emit: bool) -> Result<()> {
        self.check()?;
        let before = self.committed.lock().clone();
        let previous_databases = before.databases.clone();
        let (hook_before, hook_after) = match &self.persistence {
            Some(persistence) => persistence.commit(&before, &state)?,
            None => (before, state.clone()),
        };
        let published = if self
            .persistence
            .as_ref()
            .is_some_and(Persistence::is_native)
        {
            let mut metadata = state.clone();
            for (name, raw) in &mut metadata.databases {
                if previous_databases
                    .get(name)
                    .is_some_and(|previous| Arc::ptr_eq(previous, raw))
                {
                    continue;
                }
                let stripped = raw.fork()?;
                stripped.rows.clear();
                stripped.baseline_rows.clear();
                stripped.indexes.clear();
                *raw = Arc::new(stripped);
            }
            metadata
        } else {
            state.clone()
        };
        *self.committed.lock() = published;
        if emit {
            self.hooks
                .changes(&hook_before.databases, &hook_after.databases);
        }
        Ok(())
    }
}

impl SessionState {
    fn new(shared: Arc<Coordinator>) -> Self {
        let identity = shared.committed.lock().catalog.administrator_identity();
        Self {
            shared,
            last_query_read: false,
            endpoint_id: None,
            endpoint_stop: None,
            ceiling: None,
            database: "app".into(),
            identity,
            client_host: "localhost".into(),
            transaction: None,
            native_snapshot: None,
            transaction_read: false,
            transaction_read_only: false,
            next_transaction_read_only: false,
            observed_tables: Some(BTreeSet::new()),
            observed_columns: BTreeMap::new(),
            savepoints: Vec::new(),
            autocommit: true,
            session_state: None,
            variables: HashMap::new(),
            prepared: HashMap::new(),
            session_id: uuid::Uuid::new_v4(),
            temporary_tables: BTreeMap::new(),
        }
    }
    pub(crate) fn system_variable(&self, name: &str) -> Option<Value> {
        match name.to_ascii_lowercase().as_str() {
            "autocommit" => Some(Value::Number(u64::from(self.autocommit).into())),
            "transaction_isolation" | "tx_isolation" => {
                Some(Value::String("REPEATABLE-READ".into()))
            }
            "time_zone" => Some(self.variables.get("time_zone").cloned().unwrap_or_else(|| {
                Value::String(
                    self.shared
                        .cfg
                        .default_time_zone
                        .clone()
                        .unwrap_or_else(|| "+00:00".into()),
                )
            })),
            key => self.variables.get(key).cloned(),
        }
    }

    pub fn authenticate(&mut self, username: &str, salt: &[u8], response: &[u8]) -> bool {
        self.reset_authentication_state();
        let state = self.shared.committed.lock();
        if self.shared.check().is_err() || !state.catalog.authenticate(username, salt, response) {
            return false;
        }
        let Ok(identity) = state.catalog.identity(username) else {
            return false;
        };
        self.identity = identity;
        if let Some(scopes) = &self.ceiling {
            self.identity.set_ceiling(scopes.clone());
        }
        true
    }

    pub(crate) fn authenticate_external(
        &mut self,
        username: String,
        scopes: Vec<super::AuthScope>,
    ) -> Result<()> {
        self.reset_authentication_state();
        self.shared.check()?;
        self.identity = Identity::external(username, scopes)?;
        if let Some(scopes) = &self.ceiling {
            self.identity.set_ceiling(scopes.clone());
        }
        Ok(())
    }

    pub(crate) fn require_administrator(&self) -> Result<()> {
        self.shared.check()?;
        ensure!(
            self.shared
                .committed
                .lock()
                .catalog
                .is_admin(&self.identity),
            "Administrative command denied"
        );
        Ok(())
    }

    fn reset_authentication_state(&mut self) {
        self.rollback();
        self.release_advisory_locks();
        self.session_state = None;
        self.variables.clear();
        self.prepared.clear();
        self.temporary_tables.clear();
        self.autocommit = true;
        self.identity = Identity::unauthenticated();
    }
    pub fn use_database(&mut self, database: &str) -> Result<()> {
        self.shared.check()?;
        if self.is_in_transaction() && database != self.database {
            return Err(anyhow!("cannot switch database during a transaction"));
        }
        self.shared
            .committed
            .lock()
            .catalog
            .check_database(&self.identity, database)?;
        self.database = database.into();
        Ok(())
    }
    pub(crate) fn set_client_host(&mut self, host: String) {
        self.client_host = host;
    }

    fn current_state(&self) -> Result<Committed> {
        let mut state = self.shared.committed.lock().clone();
        if let Some(temporary) = self.temporary_tables.get(&self.database) {
            let raw = state
                .databases
                .get(&self.database)
                .ok_or_else(|| anyhow!("Unknown database: {}", self.database))?
                .fork()?;
            for table in &temporary.schemas {
                raw.copy_table_from(temporary, table.key());
            }
            state.databases.insert(self.database.clone(), Arc::new(raw));
        }
        Ok(state)
    }

    fn publish_state(&mut self, mut state: Committed) -> Result<()> {
        let raw = &state.databases[&self.database];
        if !self.temporary_tables.contains_key(&self.database)
            && !raw.schemas.iter().any(|table| table.temporary)
        {
            return self.shared.publish_database(&self.database, state);
        }
        let temporary = RawEngine::new(self.shared.cfg.clone());
        let mut hidden = BTreeSet::new();
        if let Some(previous) = self.temporary_tables.get(&self.database) {
            for table in &previous.schemas {
                if raw
                    .schemas
                    .get(table.key())
                    .is_some_and(|schema| !schema.temporary)
                {
                    // CREATE OR REPLACE TABLE creates a permanent table behind
                    // the session's existing temporary table of the same name.
                    temporary.copy_table_from(previous, table.key());
                } else {
                    hidden.insert(table.key().clone());
                }
            }
        }
        for table in &raw.schemas {
            if table.temporary {
                hidden.insert(table.key().clone());
                temporary.copy_table_from(raw, table.key());
            }
        }
        if !hidden.is_empty() {
            let permanent = raw.fork()?;
            let committed = self.shared.committed.lock().databases[&self.database].clone();
            for table in hidden {
                permanent.copy_table_from(&committed, &table);
            }
            state
                .databases
                .insert(self.database.clone(), Arc::new(permanent));
        }
        self.shared.publish_database(&self.database, state)?;
        if temporary.schemas.is_empty() {
            self.temporary_tables.remove(&self.database);
        } else {
            self.temporary_tables
                .insert(self.database.clone(), Arc::new(temporary));
        }
        Ok(())
    }

    fn configure_visibility(&self, raw: &mut RawEngine, catalog: &Catalog) {
        raw.visible_tables = if catalog.is_admin(&self.identity) {
            None
        } else {
            Some(
                raw.schemas
                    .iter()
                    .map(|entry| entry.key().clone())
                    .chain(raw.views.iter().map(|entry| entry.key().clone()))
                    .filter(|table| catalog.table_visible(&self.identity, &self.database, table))
                    .collect(),
            )
        };
    }
    fn configure_executor(&self, raw: &mut RawEngine) {
        if let Some(session) = &self.session_state {
            raw.copy_session_from(session);
        } else {
            raw.clear_session();
        }
        raw.system_variables = self.variables.clone();
        for name in [
            "autocommit",
            "time_zone",
            "transaction_isolation",
            "tx_isolation",
        ] {
            if let Some(value) = self.system_variable(name) {
                raw.system_variables.insert(name.into(), value);
            }
        }
        raw.session_user = format!("{}@{}", self.identity.username, self.client_host);
        raw.current_user = format!("{}@%", self.identity.username);
    }

    pub(crate) fn prepare_sql_with_params_for_wire(
        &mut self,
        sql: &str,
        params: &[Value],
    ) -> Result<Vec<QueryResult>> {
        self.shared.check()?;
        let sql = substitute_params(sql, params)?;
        let sql = self
            .shared
            .committed
            .lock()
            .catalog
            .authorize_and_normalize(&self.identity, &self.database, &sql)?;
        let state = self
            .transaction
            .as_ref()
            .map(|(_, state)| Ok(state.clone()))
            .unwrap_or_else(|| self.current_state())?;
        let mut raw = state
            .databases
            .get(&self.database)
            .ok_or_else(|| anyhow!("Unknown database"))?
            .fork()?;
        raw.visible_databases = state
            .catalog
            .database_names()
            .filter(|name| state.catalog.check_database(&self.identity, name).is_ok())
            .map(str::to_owned)
            .collect();
        raw.database_charsets = state.catalog.database_charsets().clone();
        self.configure_visibility(&mut raw, &state.catalog);
        self.authorize_views(&state, &self.database, &sql, &mut BTreeSet::new())?;
        self.configure_executor(&mut raw);
        raw.execute_sql_internal(&sql, &sql, false, false)
    }
    pub fn is_in_transaction(&self) -> bool {
        self.transaction.is_some()
    }
    pub fn autocommit(&self) -> bool {
        self.autocommit
    }
    fn run(
        &mut self,
        event_sql: &str,
        execution: Result<String>,
        normalize: bool,
    ) -> Result<Vec<QueryResult>> {
        self.last_query_read = false;
        self.run_observed(event_sql, execution, normalize)
    }

    fn run_observed(
        &mut self,
        event_sql: &str,
        execution: Result<String>,
        normalize: bool,
    ) -> Result<Vec<QueryResult>> {
        let observer = self.shared.committed.lock().databases["app"].clone();
        if !observer.query_events_enabled() {
            return execution.and_then(|sql| self.execute(&sql, normalize, false));
        }
        let query_id = observer.next_query_id.fetch_add(1, AtomicOrdering::Relaxed);
        let metrics = Rc::new(QueryMetricsRecorder::new(true));
        let _guard = QueryMetricsGuard::install(metrics.clone());
        let started = Instant::now();
        observer.publish_query_event(QueryEvent::Received(QueryReceivedEvent {
            query_id,
            query: event_sql.into(),
        }));
        let outcome = execution.and_then(|sql| self.execute(&sql, normalize, false));
        match &outcome {
            Ok(results) => observer.publish_query_completed(
                query_id,
                started.elapsed(),
                metrics.snapshot(),
                Some(results),
                None,
            ),
            Err(error) => observer.publish_query_completed(
                query_id,
                started.elapsed(),
                metrics.snapshot(),
                None,
                Some(error.to_string()),
            ),
        }
        outcome
    }
    fn authorize_views(
        &self,
        state: &Committed,
        database: &str,
        sql: &str,
        visited: &mut BTreeSet<String>,
    ) -> Result<()> {
        use sqlparser::ast::{Visit, Visitor};
        use std::ops::ControlFlow;
        struct Relations(BTreeSet<String>);
        impl Visitor for Relations {
            type Break = ();
            fn pre_visit_relation(&mut self, name: &ObjectName) -> ControlFlow<()> {
                if let Some(name) = name.0.last() {
                    self.0.insert(name.value.clone());
                }
                ControlFlow::Continue(())
            }
        }
        if state.catalog.is_admin(&self.identity) {
            return Ok(());
        }
        let mut relations = Relations(BTreeSet::new());
        let _ = catalog::parse_session_statement(sql)?.visit(&mut relations);
        for name in relations.0 {
            if let Some(definition) = state
                .databases
                .get(database)
                .and_then(|raw| raw.views.get(&name))
            {
                ensure!(visited.insert(name.clone()), "cyclic view reference");
                let definition = strip_view_check_option(&definition);
                state
                    .catalog
                    .authorize_and_normalize(&self.identity, database, &definition)?;
                self.authorize_views(state, database, &definition, visited)?;
                visited.remove(&name);
            }
        }
        Ok(())
    }
    fn authorize_request(&self, sql: &str) -> Result<()> {
        let mut database = self.database.clone();
        let mut state = self.current_state()?;
        let mut prepared = self.prepared.clone();
        for sql in split_sql_statements(sql)? {
            if let Some(target) = parse_use(&sql)? {
                state.catalog.check_database(&self.identity, &target)?;
                database = target;
                continue;
            }
            if let Some(command) = AdminCommand::parse(&sql)? {
                state.catalog.apply(&self.identity, command)?;
                continue;
            }
            let sql = if let Some(command) = PreparedCommand::parse(&sql)? {
                match command {
                    PreparedCommand::Prepare { name, source } => {
                        let sql = match source {
                            PreparedSource::Sql(sql) => sql,
                            PreparedSource::Variable(name) => match self
                                .session_state
                                .as_ref()
                                .map(|raw| raw.user_variable(&name))
                            {
                                Some(Value::String(sql)) => sql,
                                _ => return Err(anyhow!("PREPARE source must be a SQL string")),
                            },
                        };
                        state
                            .catalog
                            .validate_prepared(&self.identity, &database, &sql)?;
                        prepared.insert(name, (database.clone(), sql.clone()));
                        sql
                    }
                    PreparedCommand::Execute { name, variables } => {
                        let (owner, sql) = prepared
                            .get(&name)
                            .ok_or_else(|| anyhow!("unknown prepared statement: {name}"))?;
                        ensure!(
                            owner == &database,
                            "prepared statement belongs to another database"
                        );
                        let params = variables
                            .iter()
                            .map(|name| {
                                self.session_state
                                    .as_ref()
                                    .map(|raw| raw.user_variable(name))
                                    .unwrap_or(Value::Null)
                            })
                            .collect::<Vec<_>>();
                        substitute_params(sql, &params)?
                    }
                    PreparedCommand::Deallocate { name } => {
                        ensure!(
                            prepared.remove(&name).is_some(),
                            "unknown prepared statement: {name}"
                        );
                        continue;
                    }
                }
            } else {
                sql
            };
            let normalized = state
                .catalog
                .authorize_and_normalize(&self.identity, &database, &sql)
                .or_else(|error| {
                    let Some(raw) = state.databases.get(&database) else {
                        return Err(error);
                    };
                    state.catalog.authorize_and_normalize(
                        &self.identity,
                        &database,
                        &raw.rewrite_sql_for_parser(&sql),
                    )
                })?;
            self.authorize_views(&state, &database, &normalized, &mut BTreeSet::new())?;
        }
        Ok(())
    }
    fn authorize(&self, sql: &str) -> Result<String> {
        let state = self.shared.committed.lock();
        let update_low_priority = sql
            .trim_start()
            .to_ascii_uppercase()
            .starts_with("UPDATE LOW_PRIORITY ");
        if update_low_priority {
            let rewritten = sql.replacen("UPDATE LOW_PRIORITY", "UPDATE", 1).replacen(
                "update low_priority",
                "update",
                1,
            );
            let authorization_sql = state
                .databases
                .get(&self.database)
                .map(|raw| raw.rewrite_sql_for_parser(&rewritten))
                .ok_or_else(|| anyhow!("Unknown database: {}", self.database))?;
            state.catalog.authorize_and_normalize(
                &self.identity,
                &self.database,
                &authorization_sql,
            )?;
            return Ok(rewritten);
        }
        match state
            .catalog
            .authorize_and_normalize(&self.identity, &self.database, sql)
        {
            Ok(sql) => Ok(sql),
            Err(error) => {
                // Compatibility rewrites still pass the same complete authorization
                // visitor; parser failure never grants a bypass to the raw executor.
                let Some(raw) = state.databases.get(&self.database) else {
                    return Err(error);
                };
                let rewritten = raw.rewrite_sql_for_parser(sql);
                if rewritten == sql {
                    return Err(error);
                }
                let normalized = state.catalog.authorize_and_normalize(
                    &self.identity,
                    &self.database,
                    &rewritten,
                )?;
                // Preserve the source form when only parser compatibility rewrites
                // were needed; interval warnings depend on that original syntax.
                Ok(if normalized == rewritten {
                    sql.to_owned()
                } else {
                    normalized
                })
            }
        }
    }
    fn release_advisory_locks(&self) {
        self.shared
            .advisory_locks
            .lock()
            .retain(|_, (owner, _)| *owner != self.session_id);
        self.shared.advisory_available.notify_all();
    }
    fn advisory_query(&self, statement: Option<&Statement>) -> Result<Option<QueryResult>> {
        let Some(Statement::Query(query)) = statement else {
            return Ok(None);
        };
        let SetExpr::Select(select) = query.body.as_ref() else {
            return Ok(None);
        };
        if select.projection.len() != 1 {
            return Ok(None);
        }
        let (expr, column) = match &select.projection[0] {
            SelectItem::UnnamedExpr(expr) => (expr, expr.to_string()),
            SelectItem::ExprWithAlias { expr, alias } => (expr, alias.value.clone()),
            _ => return Ok(None),
        };
        let Expr::Function(function) = expr else {
            return Ok(None);
        };
        let name = function.name.to_string().to_ascii_uppercase();
        if name != "GET_LOCK" && name != "RELEASE_LOCK" && name != "IS_FREE_LOCK" {
            return Ok(None);
        }
        // Advisory locks are connection operations; reject compound SQL shapes
        // rather than evaluating them repeatedly during a row scan.
        if query.to_string() != format!("SELECT {}", select.projection[0]) {
            return Err(anyhow!("advisory locks require a standalone SELECT"));
        }
        let FunctionArguments::List(arguments) = &function.args else {
            return Err(anyhow!("invalid advisory lock arguments"));
        };
        let expected = if name == "GET_LOCK" { 2 } else { 1 };
        if arguments.args.len() != expected
            || !arguments.clauses.is_empty()
            || arguments.duplicate_treatment.is_some()
        {
            return Err(anyhow!("invalid advisory lock arguments"));
        }
        let Some(FunctionArg::Unnamed(FunctionArgExpr::Expr(key_expression))) =
            arguments.args.first()
        else {
            return Err(anyhow!("advisory lock name must be a string expression"));
        };
        eval::set_eval_database(&self.database);
        let Value::String(key) = eval_expr(key_expression, &Map::new(), 0)? else {
            return Err(anyhow!("advisory lock name must evaluate to a string"));
        };
        if key.is_empty() || key.chars().count() > 64 {
            return Err(anyhow!(
                "advisory lock name must contain 1 to 64 characters"
            ));
        }
        let mut locks = self.shared.advisory_locks.lock();
        let value = if name == "IS_FREE_LOCK" {
            json!(if locks.contains_key(&key) { 0 } else { 1 })
        } else if name == "RELEASE_LOCK" {
            match locks.get_mut(&key) {
                None => Value::Null,
                Some((owner, _)) if *owner != self.session_id => json!(0),
                Some((_, depth)) => {
                    *depth -= 1;
                    if *depth == 0 {
                        locks.remove(&key);
                        self.shared.advisory_available.notify_all();
                    }
                    json!(1)
                }
            }
        } else {
            let FunctionArg::Unnamed(FunctionArgExpr::Expr(Expr::Value(SqlValue::Number(
                seconds,
                _,
            )))) = &arguments.args[1]
            else {
                return Err(anyhow!(
                    "advisory lock timeout must be a nonnegative number"
                ));
            };
            let seconds: f64 = seconds.parse()?;
            if !seconds.is_finite() || !(0.0..=3600.0).contains(&seconds) {
                return Err(anyhow!(
                    "advisory lock timeout must be between 0 and 3600 seconds"
                ));
            }
            let deadline = Instant::now() + StdDuration::from_secs_f64(seconds);
            loop {
                match locks.get_mut(&key) {
                    None => {
                        locks.insert(key.clone(), (self.session_id, 1));
                        break json!(1);
                    }
                    Some((owner, depth)) if *owner == self.session_id => {
                        *depth = depth
                            .checked_add(1)
                            .ok_or_else(|| anyhow!("advisory lock recursion limit"))?;
                        break json!(1);
                    }
                    _ => {
                        if self
                            .shared
                            .advisory_available
                            .wait_until(&mut locks, deadline)
                            .timed_out()
                        {
                            break json!(0);
                        }
                    }
                }
            }
        };
        Ok(Some(QueryResult {
            columns: vec![column.clone()],
            column_metadata: vec![ColumnMetadata {
                name: column.clone(),
                column_type: MysqlColumnType::Integer,
                ..ColumnMetadata::default()
            }],
            rows: vec![Map::from_iter([(column, value)])],
            ..QueryResult::default()
        }))
    }
    fn begin(&mut self) -> Result<()> {
        self.commit()?;
        let state = self.current_state()?;
        state
            .catalog
            .check_database(&self.identity, &self.database)?;
        self.transaction = Some((None, state));
        self.native_snapshot = None;
        self.transaction_read = false;
        self.transaction_read_only = self.next_transaction_read_only;
        self.next_transaction_read_only = false;
        self.observed_columns.clear();
        self.observed_tables = Some(BTreeSet::new());
        Ok(())
    }
    fn commit(&mut self) -> Result<()> {
        if let Some((Some(lease), state)) = self.transaction.take() {
            self.publish_state(state)?;
            drop(lease);
        }
        self.transaction_read = false;
        self.native_snapshot = None;
        self.transaction_read_only = false;
        self.savepoints.clear();
        Ok(())
    }
    fn rollback(&mut self) {
        self.transaction = None;
        self.native_snapshot = None;
        self.transaction_read = false;
        self.transaction_read_only = false;
        self.savepoints.clear();
    }
    fn execute(&mut self, sql: &str, normalize: bool, emit: bool) -> Result<Vec<QueryResult>> {
        self.shared.check()?;
        let mut results = Vec::new();
        for statement in split_sql_statements(sql)? {
            let text = statement.trim();
            if text.is_empty() {
                continue;
            }
            if let Some(database) = parse_use(text)? {
                self.use_database(&database)?;
                results.push(QueryResult::default());
                continue;
            }
            if let Some(command) = PreparedCommand::parse(text)? {
                match command {
                    PreparedCommand::Prepare { name, source } => {
                        let sql = match source {
                            PreparedSource::Sql(sql) => sql,
                            PreparedSource::Variable(name) => match self
                                .session_state
                                .as_ref()
                                .map(|raw| raw.user_variable(&name))
                            {
                                Some(Value::String(sql)) => sql,
                                _ => return Err(anyhow!("PREPARE source must be a SQL string")),
                            },
                        };
                        let sql = self.shared.committed.lock().catalog.validate_prepared(
                            &self.identity,
                            &self.database,
                            &sql,
                        )?;
                        self.prepared.insert(name, (self.database.clone(), sql));
                        results.push(QueryResult::default());
                    }
                    PreparedCommand::Execute { name, variables } => {
                        let (database, sql) = self
                            .prepared
                            .get(&name)
                            .ok_or_else(|| anyhow!("unknown prepared statement: {name}"))?
                            .clone();
                        if database != self.database {
                            return Err(anyhow!("prepared statement belongs to another database"));
                        }
                        let params = variables
                            .iter()
                            .map(|name| {
                                self.session_state
                                    .as_ref()
                                    .map(|raw| raw.user_variable(name))
                                    .unwrap_or(Value::Null)
                            })
                            .collect::<Vec<_>>();
                        results.extend(self.execute(
                            &substitute_params(&sql, &params)?,
                            normalize,
                            false,
                        )?);
                    }
                    PreparedCommand::Deallocate { name } => {
                        if self.prepared.remove(&name).is_none() {
                            return Err(anyhow!("unknown prepared statement: {name}"));
                        }
                        results.push(QueryResult::default());
                    }
                }
                continue;
            }
            if let Some(command) = AdminCommand::parse(text)? {
                if self.is_in_transaction() {
                    return Err(anyhow!(
                        "administrative statements are not supported inside a transaction"
                    ));
                }
                let _lease = self.shared.acquire()?;
                let mut state = self.shared.committed.lock().clone();
                match state.catalog.apply(&self.identity, command)? {
                    CatalogEffect::None => {}
                    CatalogEffect::CreateDatabase(name) => {
                        let mut raw = RawEngine::new(self.shared.cfg.clone());
                        raw.database_name = name.clone();
                        state.databases.insert(name, Arc::new(raw));
                    }
                    CatalogEffect::DropDatabase(name) => {
                        if name == "app" {
                            return Err(anyhow!(
                                "cannot drop the embedded default database; reset its rows instead"
                            ));
                        }
                        state.databases.remove(&name);
                    }
                }
                self.shared.publish(state)?;
                results.push(QueryResult::default());
                continue;
            }
            let normalized = self.authorize(text)?;
            let parsed = catalog::parse_session_statement(&normalized)
                .or_else(|_| {
                    let state = self.shared.committed.lock();
                    let raw = state
                        .databases
                        .get(&self.database)
                        .ok_or_else(|| anyhow!("Unknown database: {}", self.database))?;
                    catalog::parse_session_statement(&raw.rewrite_sql_for_parser(&normalized))
                })
                .ok();
            let ast = parsed.as_ref().and_then(|items| items.first());
            self.last_query_read |= matches!(ast, Some(Statement::Query(_)));
            if let Some(result) = self.advisory_query(ast)? {
                results.push(result);
                continue;
            }
            let mut control = true;
            match ast {
                Some(Statement::ShowDatabases { .. } | Statement::ShowSchemas { .. }) => {
                    if normalized != "SHOW DATABASES" && normalized != "SHOW SCHEMAS" {
                        return Err(anyhow!("filtered SHOW DATABASES is not supported"));
                    }
                    let state = self.shared.committed.lock();
                    let mut names: Vec<String> = state
                        .catalog
                        .database_names()
                        .filter(|name| state.catalog.check_database(&self.identity, name).is_ok())
                        .map(str::to_owned)
                        .collect();
                    names.push("information_schema".to_string());
                    names.sort_by(|left, right| {
                        left.to_ascii_lowercase().cmp(&right.to_ascii_lowercase())
                    });
                    let rows = names
                        .into_iter()
                        .map(|name| Map::from_iter([("Database".into(), Value::String(name))]))
                        .collect();
                    results.push(QueryResult {
                        columns: vec!["Database".into()],
                        rows,
                        ..QueryResult::default()
                    });
                    continue;
                }
                Some(Statement::StartTransaction { modes, .. }) => {
                    let read_only = modes.iter().any(|mode| {
                        matches!(
                            mode,
                            TransactionMode::AccessMode(TransactionAccessMode::ReadOnly)
                        )
                    });
                    if modes.iter().any(|mode| {
                        !matches!(
                            mode,
                            TransactionMode::AccessMode(TransactionAccessMode::ReadOnly)
                        )
                    }) {
                        return Err(anyhow!(
                            "transaction modes are not supported; use REPEATABLE READ"
                        ));
                    }
                    self.begin()?;
                    if !modes.is_empty() {
                        self.transaction_read_only = read_only;
                    }
                }
                Some(Statement::Commit { chain }) => {
                    self.commit()?;
                    if *chain {
                        self.begin()?;
                    }
                }
                Some(Statement::Rollback { chain, savepoint }) => {
                    if let Some(name) = savepoint {
                        let position = self
                            .savepoints
                            .iter()
                            .rposition(|(key, _)| key == &name.value)
                            .ok_or_else(|| anyhow!("SAVEPOINT does not exist"))?;
                        let snapshot = self.savepoints[position].1.clone();
                        let pending = &mut self
                            .transaction
                            .as_mut()
                            .ok_or_else(|| anyhow!("no active transaction"))?
                            .1;
                        pending.databases.insert(
                            self.database.clone(),
                            snapshot
                                .databases
                                .get(&self.database)
                                .cloned()
                                .ok_or_else(|| anyhow!("Unknown database: {}", self.database))?,
                        );
                        self.savepoints.truncate(position + 1);
                    } else {
                        self.rollback();
                        if *chain {
                            self.begin()?;
                        }
                    }
                }
                Some(Statement::Savepoint { name }) => {
                    let state = self
                        .transaction
                        .as_ref()
                        .ok_or_else(|| anyhow!("no active transaction"))?
                        .1
                        .clone();
                    self.savepoints.retain(|(key, _)| key != &name.value);
                    self.savepoints.push((name.value.clone(), state));
                }
                Some(Statement::ReleaseSavepoint { name }) => {
                    let position = self
                        .savepoints
                        .iter()
                        .position(|(key, _)| key == &name.value)
                        .ok_or_else(|| anyhow!("SAVEPOINT does not exist"))?;
                    self.savepoints.remove(position);
                }
                Some(Statement::SetVariable {
                    variables,
                    value,
                    hivevar,
                    ..
                }) => {
                    if *hivevar || variables.len() != 1 || value.len() != 1 {
                        return Err(anyhow!(
                            "only single session-variable assignments are supported"
                        ));
                    }
                    let name = variables[0]
                        .to_string()
                        .replace('`', "")
                        .to_ascii_lowercase();
                    let name = name
                        .strip_prefix("@@session.")
                        .or_else(|| name.strip_prefix("@@local."))
                        .or_else(|| name.strip_prefix("@@"))
                        .unwrap_or(&name);
                    if name.starts_with('@') {
                        control = false;
                    } else {
                        let literal = value[0].to_string();
                        let setting = literal.trim_matches(['\'', '"']).to_string();
                        match name {
                            "autocommit" => {
                                let enabled = match setting.to_ascii_uppercase().as_str() {
                                    "1" | "ON" | "TRUE" => true,
                                    "0" | "OFF" | "FALSE" => false,
                                    _ => return Err(anyhow!("invalid autocommit setting")),
                                };
                                if enabled && !self.autocommit {
                                    self.commit()?;
                                }
                                self.autocommit = enabled;
                            }
                            "transaction_isolation" | "tx_isolation" => {
                                if self.is_in_transaction()
                                    || !setting.eq_ignore_ascii_case("REPEATABLE-READ")
                                {
                                    return Err(anyhow!(
                                        "only REPEATABLE READ isolation is supported, before starting a transaction"
                                    ));
                                }
                            }
                            "sql_mode" | "time_zone" | "sql_safe_updates" => {
                                let raw = match &self.session_state {
                                    Some(raw) => raw.fork()?,
                                    None => {
                                        let state = self.shared.committed.lock();
                                        state
                                            .catalog
                                            .check_database(&self.identity, &self.database)?;
                                        let raw = state
                                            .databases
                                            .get(&self.database)
                                            .ok_or_else(|| {
                                                anyhow!("Unknown database: {}", self.database)
                                            })?
                                            .fork()?;
                                        raw.clear_session();
                                        raw
                                    }
                                };
                                match name {
                                    "sql_mode" => *raw.sql_mode.lock() = setting.clone(),
                                    "time_zone" => raw.set_session_time_zone(setting.clone()),
                                    _ => raw.set_sql_safe_updates(matches!(
                                        setting.to_ascii_uppercase().as_str(),
                                        "1" | "ON" | "TRUE"
                                    )),
                                }
                                self.session_state = Some(Arc::new(raw));
                                self.variables.insert(name.into(), Value::String(setting));
                            }
                            "optimizer_switch"
                            | "character_set_client"
                            | "character_set_connection"
                            | "character_set_results"
                            | "collation_connection" => {
                                self.variables.insert(name.into(), Value::String(setting));
                            }
                            _ => return Err(anyhow!("unsupported session setting: {name}")),
                        }
                    }
                }
                Some(Statement::SetNames {
                    charset_name,
                    collation_name,
                }) => {
                    for name in [
                        "character_set_client",
                        "character_set_connection",
                        "character_set_results",
                    ] {
                        self.variables
                            .insert(name.into(), Value::String(charset_name.clone()));
                    }
                    if let Some(collation) = collation_name {
                        self.variables.insert(
                            "collation_connection".into(),
                            Value::String(collation.clone()),
                        );
                    }
                }
                Some(Statement::SetNamesDefault { .. }) => {
                    for name in [
                        "character_set_client",
                        "character_set_connection",
                        "character_set_results",
                        "collation_connection",
                    ] {
                        self.variables.remove(name);
                    }
                }
                Some(Statement::SetTransaction {
                    modes, snapshot, ..
                }) => {
                    if self.is_in_transaction() || snapshot.is_some() {
                        return Err(anyhow!(
                            "transaction characteristics must be set before starting a transaction"
                        ));
                    }
                    match modes.as_slice() {
                        [TransactionMode::AccessMode(TransactionAccessMode::ReadOnly)] => {
                            self.next_transaction_read_only = true;
                        }
                        [mode] if mode.to_string() == "ISOLATION LEVEL REPEATABLE READ" => {}
                        _ => {
                            return Err(anyhow!(
                                "only READ ONLY or REPEATABLE READ transaction characteristics are supported"
                            ));
                        }
                    }
                }
                _ => control = false,
            }
            if control {
                results.push(QueryResult::default());
                continue;
            }
            let ddl = parse_alter_table_drop_index(&normalized).is_some()
                || matches!(
                    ast,
                    Some(
                        Statement::CreateTable(_)
                            | Statement::AlterTable { .. }
                            | Statement::Drop { .. }
                            | Statement::Truncate { .. }
                            | Statement::CreateIndex(_)
                            | Statement::CreateView { .. }
                    )
                );
            if ddl && (self.is_in_transaction() || !self.autocommit) {
                return Err(anyhow!("DDL is not supported inside an active transaction"));
            }
            let session_only = matches!(
                ast,
                Some(
                    Statement::SetVariable { .. }
                        | Statement::SetNames { .. }
                        | Statement::SetNamesDefault { .. }
                )
            );
            if !session_only && !self.autocommit && !self.is_in_transaction() {
                self.begin()?;
            }
            let read = session_only
                || matches!(
                    ast,
                    Some(Statement::Query(query))
                        if query
                            .locks
                            .iter()
                            .all(|lock| matches!(lock.lock_type, LockType::Share))
                )
                || matches!(
                    ast,
                    Some(
                        Statement::ShowTables { .. }
                            | Statement::ShowColumns { .. }
                            | Statement::ShowCreate { .. }
                            | Statement::ShowVariable { .. }
                            | Statement::ShowVariables { .. }
                            | Statement::ShowStatus { .. }
                    )
                );
            if !read && self.transaction_read_only {
                return Err(anyhow!(
                    "Cannot execute statement in a READ ONLY transaction"
                ));
            }
            let lease = if !read
                && !self
                    .transaction
                    .as_ref()
                    .is_some_and(|(lease, _)| lease.is_some())
            {
                Some(self.shared.acquire_database(&self.database)?)
            } else {
                None
            };
            if read && !session_only && !self.transaction_read {
                let _publication = self.shared.publication.lock();
                if self.transaction.is_some() && self.native_snapshot.is_none() {
                    self.native_snapshot = self
                        .shared
                        .persistence
                        .as_ref()
                        .and_then(Persistence::native_snapshot);
                }
                let current = self.current_state()?;
                if let Some((None, snapshot)) = &mut self.transaction {
                    *snapshot = current;
                    for (_, saved) in &mut self.savepoints {
                        saved.databases.insert(
                            self.database.clone(),
                            snapshot
                                .databases
                                .get(&self.database)
                                .cloned()
                                .ok_or_else(|| anyhow!("Unknown database: {}", self.database))?,
                        );
                    }
                }
            }
            let mut statement_snapshot = None;
            let mut state = if read
                && self.transaction.is_none()
                && self
                    .shared
                    .persistence
                    .as_ref()
                    .is_some_and(Persistence::is_native)
            {
                let _publication = self.shared.publication.lock();
                let state = self.current_state()?;
                statement_snapshot = self
                    .shared
                    .persistence
                    .as_ref()
                    .and_then(Persistence::native_snapshot);
                state
            } else {
                self.transaction
                    .as_ref()
                    .map(|(_, state)| Ok(state.clone()))
                    .unwrap_or_else(|| self.current_state())?
            };
            // A read-only snapshot must not block independent cancellation/lease
            // updates. Upgrade only before the first write, then serialize writers.
            // A changed snapshot cannot safely overwrite newer committed state.
            let current = lease.as_ref().map(|_| self.current_state()).transpose()?;
            let lease = if let Some((transaction_lease, snapshot)) = &mut self.transaction {
                if let Some(lease) = lease {
                    let mut current = current.expect("writer upgrade has current state");
                    if let Some(persistence) = &self.shared.persistence {
                        if persistence.is_native() {
                            if let Some(tables) = &self.observed_tables {
                                let prior = snapshot.databases[&self.database].fork()?;
                                persistence.hydrate_tables(
                                    &prior,
                                    tables.iter().cloned(),
                                    self.native_snapshot.as_ref(),
                                )?;
                                snapshot
                                    .databases
                                    .insert(self.database.clone(), Arc::new(prior));
                                let raw = current.databases[&self.database].fork()?;
                                persistence.hydrate_tables(&raw, tables.iter().cloned(), None)?;
                                current
                                    .databases
                                    .insert(self.database.clone(), Arc::new(raw));
                            }
                        }
                    }
                    let unchanged = snapshot
                        .databases
                        .get(&self.database)
                        .zip(current.databases.get(&self.database))
                        .is_some_and(|(before, after)| {
                            if self
                                .shared
                                .persistence
                                .as_ref()
                                .is_some_and(Persistence::is_native)
                                && self.observed_tables.is_none()
                            {
                                return Arc::ptr_eq(before, after);
                            }
                            Arc::ptr_eq(before, after)
                                || (self.temporary_tables.contains_key(&self.database)
                                    && before.same_database_state(after))
                                || self.observed_tables.as_ref().is_some_and(|tables| {
                                    tables.iter().all(|table| {
                                        before.same_table_state(
                                            after,
                                            table,
                                            self.observed_columns
                                                .get(table)
                                                .and_then(Option::as_ref),
                                        )
                                    })
                                })
                        });
                    if !unchanged && self.transaction_read {
                        // MySQL 1213 aborts the transaction; retry must start fresh.
                        self.rollback();
                        return Err(anyhow!("transaction snapshot changed; retry transaction"));
                    }
                    // Preserve other databases and account changes committed while
                    // this transaction held only a read snapshot.
                    {
                        for (_, saved) in &mut self.savepoints {
                            saved.databases.insert(
                                self.database.clone(),
                                current.databases.get(&self.database).cloned().ok_or_else(
                                    || anyhow!("Unknown database: {}", self.database),
                                )?,
                            );
                        }
                    }
                    state = current.clone();
                    *snapshot = current;
                    *transaction_lease = Some(lease);
                }
                None
            } else {
                lease
            };
            let mut normalized = self.authorize(text)?;
            if let Some(persistence) = &self.shared.persistence {
                if persistence.is_native() {
                    let baseline = state.databases[&self.database].fork()?;
                    let mut tables = statement_tables(ast, &baseline, !read);
                    if normalized
                        .to_ascii_uppercase()
                        .contains("INFORMATION_SCHEMA")
                    {
                        tables.extend(baseline.schemas.iter().map(|item| item.key().clone()));
                    }
                    if matches!(
                        ast,
                        Some(
                            Statement::ShowTables { .. }
                                | Statement::ShowColumns { .. }
                                | Statement::ShowCreate { .. }
                        )
                    ) {
                        tables.clear();
                    }
                    persistence.hydrate_tables(
                        &baseline,
                        tables,
                        self.native_snapshot
                            .as_ref()
                            .or(statement_snapshot.as_ref()),
                    )?;
                    state
                        .databases
                        .insert(self.database.clone(), Arc::new(baseline));
                }
            }
            let mut raw = state
                .databases
                .get(&self.database)
                .ok_or_else(|| anyhow!("Unknown database: {}", self.database))?
                .fork()?;
            raw.visible_databases = state
                .catalog
                .database_names()
                .filter(|name| state.catalog.check_database(&self.identity, name).is_ok())
                .map(str::to_owned)
                .collect();
            raw.database_charsets = state.catalog.database_charsets().clone();
            self.configure_visibility(&mut raw, &state.catalog);
            // DROP TEMPORARY must never remove a permanent table of the same name.
            if let Some(Statement::Drop {
                temporary: true,
                names,
                if_exists,
                ..
            }) = ast
            {
                let mut targets = Vec::new();
                for name in names {
                    let table = object_name(name)?;
                    if raw
                        .schemas
                        .get(&table)
                        .is_some_and(|schema| schema.temporary)
                    {
                        targets.push(name.to_string());
                    } else if !if_exists {
                        return Err(anyhow!("unknown temporary table: {table}"));
                    }
                }
                if targets.is_empty() {
                    results.push(QueryResult::default());
                    continue;
                }
                normalized = format!("DROP TABLE {}", targets.join(", "));
            }
            self.configure_executor(&mut raw);
            self.authorize_views(&state, &self.database, &normalized, &mut BTreeSet::new())?;
            let mut out = raw.execute_sql_internal(text, &normalized, normalize, emit)?;
            if !read && !ddl && !state.catalog.is_admin(&self.identity) {
                // Check the actual row changes too: cascades and writable views can
                // affect tables that do not appear in the original statement.
                // A changed primary key is still an UPDATE, even though the
                // row map represents it as removing and inserting keys.
                let updates_keys = matches!(ast, Some(Statement::Update { .. }))
                    || matches!(ast, Some(Statement::Insert(insert)) if insert.on.is_some() && !insert.replace_into);
                let replace_table = match ast {
                    Some(Statement::Insert(insert)) if insert.replace_into => {
                        Some(object_name(&insert.table_name)?)
                    }
                    _ => None,
                };
                let before = &state.databases[&self.database];
                for table in raw.rows.iter() {
                    let previous = before.rows.get(table.key());
                    for (key, row) in table.iter() {
                        let old = previous.as_ref().and_then(|rows| rows.get(key));
                        let privilege = if updates_keys {
                            AuthPrivilege::Update
                        } else if old.is_none() || replace_table.as_ref() == Some(table.key()) {
                            AuthPrivilege::Insert
                        } else {
                            AuthPrivilege::Update
                        };
                        if old != Some(row) {
                            state.catalog.check_table(
                                &self.identity,
                                &self.database,
                                table.key(),
                                privilege,
                            )?;
                        }
                    }
                }
                for table in before.rows.iter() {
                    let current = raw.rows.get(table.key());
                    if table
                        .keys()
                        .any(|key| current.as_ref().is_none_or(|rows| !rows.contains_key(key)))
                    {
                        state.catalog.check_table(
                            &self.identity,
                            &self.database,
                            table.key(),
                            if updates_keys {
                                AuthPrivilege::Update
                            } else {
                                AuthPrivilege::Delete
                            },
                        )?;
                    }
                }
            }
            for result in &mut out {
                preserve_select_result_headers(text, result);
                for (metadata, name) in result.column_metadata.iter_mut().zip(&result.columns) {
                    metadata.name = name.clone();
                }
            }
            if read && !session_only && self.is_in_transaction() {
                self.transaction_read = true;
                match (self.observed_tables.as_mut(), read_dependencies(ast, &raw)) {
                    (Some(observed), Some(tables)) => {
                        let columns = projected_read_columns(ast);
                        for table in &tables {
                            self.observed_columns
                                .entry(table.clone())
                                .and_modify(|prior| match (prior.as_mut(), columns.as_ref()) {
                                    (Some(prior), Some(columns)) => {
                                        prior.extend(columns.iter().cloned())
                                    }
                                    _ => *prior = None,
                                })
                                .or_insert_with(|| columns.clone());
                        }
                        observed.extend(tables);
                    }
                    _ => self.observed_tables = None,
                }
            }
            let raw = Arc::new(raw);
            if self
                .shared
                .persistence
                .as_ref()
                .is_some_and(Persistence::is_native)
            {
                let session = raw.fork()?;
                for table in session
                    .rows
                    .iter()
                    .map(|item| item.key().clone())
                    .collect::<Vec<_>>()
                {
                    if !session
                        .schemas
                        .get(&table)
                        .is_some_and(|schema| schema.temporary)
                    {
                        session.rows.remove(&table);
                        session.baseline_rows.remove(&table);
                        session.indexes.remove(&table);
                    }
                }
                self.session_state = Some(Arc::new(session));
            } else {
                self.session_state = Some(raw.clone());
            }
            if !read {
                state.databases.insert(self.database.clone(), raw);
                if let Some((_, pending)) = &mut self.transaction {
                    *pending = state;
                } else {
                    self.publish_state(state)?;
                }
            }
            drop(lease);
            results.extend(out);
        }
        Ok(results)
    }
}

impl Drop for SessionState {
    fn drop(&mut self) {
        self.release_advisory_locks();
    }
}

// Only direct table reads have a bounded dependency set here. Views, metadata,
// functions and nested queries conservatively retain whole-database validation.
fn statement_tables(
    statement: Option<&Statement>,
    raw: &RawEngine,
    write: bool,
) -> BTreeSet<String> {
    use sqlparser::ast::{Visit, Visitor};
    use std::ops::ControlFlow;
    struct Relations(BTreeSet<String>);
    impl Visitor for Relations {
        type Break = ();
        fn pre_visit_relation(&mut self, name: &ObjectName) -> ControlFlow<()> {
            if let Some(name) = name.0.last() {
                self.0.insert(name.value.clone());
            }
            ControlFlow::Continue(())
        }
    }
    let mut relations = Relations(BTreeSet::new());
    if let Some(statement) = statement {
        let _ = statement.visit(&mut relations);
    } else {
        relations
            .0
            .extend(raw.schemas.iter().map(|schema| schema.key().clone()));
    }
    let mut visited_views = BTreeSet::new();
    loop {
        let mut expanded = false;
        for view in relations.0.clone() {
            if !visited_views.insert(view.clone()) {
                continue;
            }
            if let Some(definition) = raw.views.get(&view) {
                if let Ok(statements) = crate::sql::parse(&definition) {
                    for statement in statements {
                        let prior = relations.0.len();
                        let _ = statement.visit(&mut relations);
                        expanded |= relations.0.len() != prior;
                    }
                }
            }
        }
        if !expanded {
            break;
        }
    }
    if write {
        loop {
            let mut expanded = false;
            for schema in &raw.schemas {
                for fk in &schema.foreign_keys {
                    if relations.0.contains(schema.key())
                        || relations.0.contains(&fk.referenced_table)
                    {
                        expanded |= relations.0.insert(schema.key().clone());
                        expanded |= relations.0.insert(fk.referenced_table.clone());
                    }
                }
            }
            if !expanded {
                break;
            }
        }
    }
    relations.0.retain(|name| raw.schemas.contains_key(name));
    relations.0
}

fn read_dependencies(statement: Option<&Statement>, raw: &RawEngine) -> Option<BTreeSet<String>> {
    use sqlparser::ast::{Visit, Visitor};
    use std::ops::ControlFlow;
    struct Dependencies<'a> {
        raw: &'a RawEngine,
        tables: BTreeSet<String>,
        queries: usize,
    }
    impl Visitor for Dependencies<'_> {
        type Break = ();
        fn pre_visit_query(&mut self, query: &Query) -> ControlFlow<()> {
            self.queries += 1;
            if self.queries != 1 || query.with.is_some() {
                return ControlFlow::Break(());
            }
            ControlFlow::Continue(())
        }
        fn pre_visit_relation(&mut self, relation: &ObjectName) -> ControlFlow<()> {
            if relation.0.len() != 1 {
                return ControlFlow::Break(());
            }
            let name = &relation.0[0].value;
            if self.raw.views.contains_key(name) || !self.raw.schemas.contains_key(name) {
                return ControlFlow::Break(());
            }
            self.tables.insert(name.clone());
            ControlFlow::Continue(())
        }
        fn pre_visit_expr(&mut self, expr: &Expr) -> ControlFlow<()> {
            if matches!(expr, Expr::Function(_)) {
                return ControlFlow::Break(());
            }
            ControlFlow::Continue(())
        }
    }
    let statement = statement?;
    if !matches!(statement, Statement::Query(_)) {
        return None;
    }
    let mut visitor = Dependencies {
        raw,
        tables: BTreeSet::new(),
        queries: 0,
    };
    if statement.visit(&mut visitor).is_break() {
        None
    } else {
        Some(visitor.tables)
    }
}

// Only plain projections from one direct table can narrow observed columns.
// Predicate and ordering identifiers are included; complex queries retain the
// complete table comparison established by read_dependencies.
fn projected_read_columns(statement: Option<&Statement>) -> Option<BTreeSet<String>> {
    use sqlparser::ast::{Visit, Visitor};
    use std::ops::ControlFlow;
    let Statement::Query(query) = statement? else {
        return None;
    };
    let SetExpr::Select(select) = query.body.as_ref() else {
        return None;
    };
    if select.from.len() != 1
        || !select.from[0].joins.is_empty()
        || !select.projection.iter().all(|item| {
            matches!(
                item,
                SelectItem::UnnamedExpr(Expr::Identifier(_) | Expr::CompoundIdentifier(_))
            )
        })
    {
        return None;
    }
    struct Columns(BTreeSet<String>);
    impl Visitor for Columns {
        type Break = ();
        fn pre_visit_expr(&mut self, expr: &Expr) -> ControlFlow<()> {
            match expr {
                Expr::Identifier(id) => {
                    self.0.insert(id.value.clone());
                }
                Expr::CompoundIdentifier(parts) => {
                    self.0.insert(parts.last().unwrap().value.clone());
                }
                Expr::Function(_) | Expr::Subquery(_) | Expr::Exists { .. } => {
                    return ControlFlow::Break(());
                }
                _ => {}
            }
            ControlFlow::Continue(())
        }
    }
    let mut columns = Columns(BTreeSet::new());
    if statement?.visit(&mut columns).is_break() {
        None
    } else {
        Some(columns.0)
    }
}

impl RawEngine {
    fn copy_table_from(&self, source: &Self, table: &str) {
        self.schemas.remove(table);
        self.unversioned_columns.remove(table);
        self.rows.remove(table);
        self.baseline_rows.remove(table);
        self.indexes.remove(table);
        self.clear_auto_inc(table);
        let prefix = format!("{table}:");
        self.index_comments
            .retain(|key, _| !key.starts_with(&prefix));
        if let Some(schema) = source.schemas.get(table) {
            self.schemas.insert(table.into(), schema.clone());
        }
        if let Some(columns) = source.unversioned_columns.get(table) {
            self.unversioned_columns
                .insert(table.into(), columns.clone());
        }
        if let Some(rows) = source.rows.get(table) {
            self.rows.insert(table.into(), rows.clone());
        }
        if let Some(rows) = source.baseline_rows.get(table) {
            self.baseline_rows.insert(table.into(), rows.clone());
        }
        if let Some(indexes) = source.indexes.get(table) {
            self.indexes.insert(table.into(), indexes.clone());
        }
        for value in &source.auto_inc {
            if value.key().starts_with(&prefix) {
                self.auto_inc.insert(value.key().clone(), *value.value());
            }
        }
        for value in &source.index_comments {
            if value.key().starts_with(&prefix) {
                self.index_comments
                    .insert(value.key().clone(), value.value().clone());
            }
        }
    }

    fn same_database_state(&self, other: &Self) -> bool {
        self.schemas.len() == other.schemas.len()
            && self
                .schemas
                .iter()
                .all(|table| self.same_table_state(other, table.key(), None))
            && self.auto_inc.len() == other.auto_inc.len()
            && self
                .auto_inc
                .iter()
                .all(|value| other.auto_inc.get(value.key()).as_deref() == Some(value.value()))
            && self.views.len() == other.views.len()
            && self
                .views
                .iter()
                .all(|value| other.views.get(value.key()).as_deref() == Some(value.value()))
            && self.index_comments.len() == other.index_comments.len()
            && self.index_comments.iter().all(|value| {
                other.index_comments.get(value.key()).as_deref() == Some(value.value())
            })
    }

    fn same_table_state(
        &self,
        other: &Self,
        table: &str,
        columns: Option<&BTreeSet<String>>,
    ) -> bool {
        self.schemas.get(table).as_deref() == other.schemas.get(table).as_deref()
            && self.unversioned_columns.get(table).as_deref()
                == other.unversioned_columns.get(table).as_deref()
            && match (self.rows.get(table), other.rows.get(table), columns) {
                (Some(before), Some(after), _) if before.ptr_eq(&after) => true,
                (Some(before), Some(after), Some(columns)) => {
                    before.len() == after.len()
                        && before.iter().all(|(key, row)| {
                            after.get(key).is_some_and(|next| {
                                row.created_at == next.created_at
                                    && columns.iter().all(|column| {
                                        row.data
                                            .iter()
                                            .find(|(name, _)| name.eq_ignore_ascii_case(column))
                                            .map(|(_, value)| value)
                                            == next
                                                .data
                                                .iter()
                                                .find(|(name, _)| name.eq_ignore_ascii_case(column))
                                                .map(|(_, value)| value)
                                    })
                            })
                        })
                }
                (before, after, _) => before.as_deref() == after.as_deref(),
            }
            && self.auto_inc.get(table).as_deref() == other.auto_inc.get(table).as_deref()
    }
    pub(super) fn fork(&self) -> Result<Self> {
        // Copying a private working version is engine bookkeeping, not logical
        // SQL row access. Keep diagnostics scoped to the requested operation.
        let _metrics = QueryMetricsGuard::install(Rc::new(QueryMetricsRecorder::new(false)));
        // ponytail: writes still copy the affected table. Consider row-level
        // sharing only if large single-table write workloads justify it.
        let raw = Self {
            database_name: self.database_name.clone(),
            system_variables: self.system_variables.clone(),
            session_user: self.session_user.clone(),
            current_user: self.current_user.clone(),
            visible_databases: self.visible_databases.clone(),
            visible_tables: self.visible_tables.clone(),
            database_charsets: self.database_charsets.clone(),
            cfg: self.cfg.clone(),
            schemas: self.schemas.clone(),
            unversioned_columns: self.unversioned_columns.clone(),
            rows: self.rows.clone(),
            baseline_rows: self.baseline_rows.clone(),
            auto_inc: self.auto_inc.clone(),
            indexes: self.indexes.clone(),
            index_comments: self.index_comments.clone(),
            last_insert_id: AtomicU64::new(self.last_insert_id.load(AtomicOrdering::Relaxed)),
            next_query_id: self.next_query_id.clone(),
            read_query_count: self.read_query_count.clone(),
            write_query_count: self.write_query_count.clone(),
            last_rows_affected: AtomicU64::new(
                self.last_rows_affected.load(AtomicOrdering::Relaxed),
            ),
            last_found_rows: AtomicU64::new(self.last_found_rows.load(AtomicOrdering::Relaxed)),
            sql_mode: Mutex::new(self.sql_mode.lock().clone()),
            user_variables: self.user_variables.clone(),
            user_variable_types: self.user_variable_types.clone(),
            prepared_statements: self.prepared_statements.clone(),
            views: self.views.clone(),
            // Only parsed SQL syntax is cached, never plans or evaluated results.
            // Sharing across statement forks cannot retain stale schema/session data.
            parsed_select_cache: self.parsed_select_cache.clone(),
            query_event_subscribers: self.query_event_subscribers.clone(),
        };
        Ok(raw)
    }
    fn clear_session(&self) {
        self.user_variables.clear();
        self.user_variable_types.clear();
        if let Some(zone) = &self.cfg.default_time_zone {
            self.set_session_time_zone(zone.clone());
        }
        self.prepared_statements.clear();
        self.sql_mode.lock().clear();
        self.last_insert_id.store(0, AtomicOrdering::Relaxed);
        self.last_rows_affected.store(0, AtomicOrdering::Relaxed);
        self.last_found_rows.store(0, AtomicOrdering::Relaxed);
    }
    fn copy_session_from(&self, source: &Self) {
        *self.sql_mode.lock() = source.sql_mode.lock().clone();
        self.user_variables.clear();
        for item in &source.user_variables {
            self.user_variables
                .insert(item.key().clone(), item.value().clone());
        }
        self.user_variable_types.clear();
        for item in &source.user_variable_types {
            self.user_variable_types
                .insert(item.key().clone(), *item.value());
        }
        self.prepared_statements.clear();
        for item in &source.prepared_statements {
            self.prepared_statements
                .insert(item.key().clone(), item.value().clone());
        }
        self.last_insert_id.store(
            source.last_insert_id.load(AtomicOrdering::Relaxed),
            AtomicOrdering::Relaxed,
        );
        self.last_rows_affected.store(
            source.last_rows_affected.load(AtomicOrdering::Relaxed),
            AtomicOrdering::Relaxed,
        );
        self.last_found_rows.store(
            source.last_found_rows.load(AtomicOrdering::Relaxed),
            AtomicOrdering::Relaxed,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_buffers_preserve_returning_and_upsert_results() {
        let engine = Engine::new(EngineConfig::mysql_strict());
        engine
            .execute_sql("CREATE TABLE items (id INT PRIMARY KEY, value INT)")
            .unwrap();
        let plain = engine
            .execute_sql("INSERT INTO items VALUES (1,10),(2,20)")
            .unwrap();
        assert_eq!(plain[0].rows_affected, 2);
        assert!(plain[0].rows.is_empty());
        let returning = engine
            .execute_sql("INSERT INTO items VALUES (3,30) RETURNING id,value")
            .unwrap();
        assert_eq!(returning[0].rows[0]["value"], 30);
        let updated = engine.execute_sql("INSERT INTO items VALUES (3,40) ON DUPLICATE KEY UPDATE value=40 RETURNING id,value").unwrap();
        assert_eq!(updated[0].rows_affected, 2);
        assert_eq!(updated[0].rows[0]["value"], 40);
        assert!(
            engine
                .execute_sql("INSERT INTO items VALUES (4,50),(1,60)")
                .is_err()
        );
        assert_eq!(
            engine.execute_sql("SELECT id FROM items").unwrap()[0]
                .rows
                .len(),
            3
        );
    }

    #[test]
    fn metadata_table_filter_preserves_case_or_and_row_expressions() {
        let engine = Engine::new(EngineConfig::mysql_strict());
        engine.execute_sql("CREATE TABLE first (id INT PRIMARY KEY, first INT); CREATE TABLE second (id INT PRIMARY KEY)").unwrap();
        assert_eq!(engine.execute_sql("SELECT column_name FROM information_schema.columns WHERE (TABLE_NAME = 'FIRST') AND column_key='PRI'").unwrap()[0].rows.len(), 1);
        assert_eq!(engine.execute_sql("SELECT column_name FROM information_schema.columns WHERE table_name='first' OR table_name='second'").unwrap()[0].rows.len(), 3);
        assert_eq!(engine.execute_sql("SELECT column_name FROM information_schema.columns WHERE table_name=column_name").unwrap()[0].rows.len(), 1);
        assert!(
            engine
                .execute_sql(
                    "SELECT column_name FROM information_schema.columns WHERE table_name='missing'"
                )
                .unwrap()[0]
                .rows
                .is_empty()
        );
    }

    #[test]
    #[ignore = "manual comparison of metadata predicate pushdown"]
    fn benchmark_metadata_table_filter() {
        let engine = Engine::new(EngineConfig::mysql_strict());
        for table in 0..100 {
            engine.execute_sql(&format!("CREATE TABLE table_{table} (id INT PRIMARY KEY, label VARCHAR(100), payload TEXT)")).unwrap();
        }
        for predicate in [
            "CONCAT('table_', '0')",
            "'table_0'",
            "CONCAT('table_', '0')",
            "'table_0'",
        ] {
            let sql = format!(
                "SELECT column_name FROM information_schema.columns WHERE table_name={predicate} AND column_key='PRI'"
            );
            let started = Instant::now();
            for _ in 0..100 {
                assert_eq!(engine.execute_sql(&sql).unwrap()[0].rows.len(), 1);
            }
            eprintln!(
                "metadata {predicate}: 100 lookups {:.3} ms",
                started.elapsed().as_secs_f64() * 1000.0
            );
        }
    }

    #[test]
    fn foreign_key_primary_lookup_preserves_coercions_and_statement_rollback() {
        let engine = Engine::new(EngineConfig::mysql_strict());
        engine.execute_sql("CREATE TABLE parents (id VARCHAR(20) PRIMARY KEY, tag VARCHAR(20) UNIQUE);
            INSERT INTO parents VALUES ('AbC', 'Label'), ('001', 'Other');
            CREATE TABLE exact_child (id INT PRIMARY KEY, parent_id VARCHAR(20), FOREIGN KEY (parent_id) REFERENCES parents(id));
            CREATE TABLE numeric_child (id INT PRIMARY KEY, parent_id INT, FOREIGN KEY (parent_id) REFERENCES parents(id));
            CREATE TABLE tag_child (id INT PRIMARY KEY, tag VARCHAR(20), FOREIGN KEY (tag) REFERENCES parents(tag));
            CREATE TABLE composite_parent (a INT, b VARCHAR(20), PRIMARY KEY(a,b));
            INSERT INTO composite_parent VALUES (1,'AbC');
            CREATE TABLE composite_child (id INT PRIMARY KEY, a INT, b VARCHAR(20), FOREIGN KEY (a,b) REFERENCES composite_parent(a,b));
            INSERT INTO exact_child VALUES (1,'AbC'), (2,'abc');
            INSERT INTO numeric_child VALUES (1,1);
            INSERT INTO tag_child VALUES (1,'label');
            INSERT INTO composite_child VALUES (1,1,'AbC'), (2,1,'abc')").unwrap();
        assert!(
            engine
                .execute_sql("INSERT INTO exact_child VALUES (3,'AbC'), (4,'missing')")
                .is_err()
        );
        assert_eq!(
            engine
                .execute_sql("SELECT id FROM exact_child ORDER BY id")
                .unwrap()[0]
                .rows
                .len(),
            2
        );
        assert!(
            engine
                .execute_sql("INSERT INTO composite_child VALUES (3,2,'AbC')")
                .is_err()
        );
    }

    #[test]
    fn foreign_keys_work_when_parent_and_child_share_a_directory_shard() {
        let engine = Engine::new(EngineConfig::mysql_strict());
        engine
            .execute_sql("CREATE TABLE parent (id INT PRIMARY KEY); INSERT INTO parent VALUES (1)")
            .unwrap();
        let child = {
            let raw = engine.shared.committed.lock().databases["app"].clone();
            let _parent_guard = raw.rows.get_mut("parent").unwrap();
            // try_get identifies a real collision without relying on DashMap's
            // private hash-to-shard algorithm or blocking this test thread.
            (0..1024)
                .map(|index| format!("child_{index}"))
                .find(|name| raw.rows.try_get(name).is_locked())
                .unwrap()
        };
        engine.execute_sql(&format!("CREATE TABLE {child} (id INT PRIMARY KEY, parent_id INT, FOREIGN KEY (parent_id) REFERENCES parent(id) ON UPDATE CASCADE ON DELETE CASCADE); INSERT INTO {child} VALUES (1,1); UPDATE parent SET id=2")).unwrap();
        assert_eq!(
            engine
                .execute_sql(&format!("SELECT parent_id FROM {child}"))
                .unwrap()[0]
                .rows[0]["parent_id"],
            2
        );
        engine.execute_sql("DELETE FROM parent").unwrap();
        assert!(
            engine
                .execute_sql(&format!("SELECT * FROM {child}"))
                .unwrap()[0]
                .rows
                .is_empty()
        );
    }

    #[test]
    #[ignore = "manual comparison of private directory shard allocation costs"]
    fn benchmark_private_directory_shards() {
        fn reshard<T: Clone>(source: &DashMap<String, T>, shards: usize) -> DashMap<String, T> {
            let result = DashMap::with_shard_amount(shards);
            for entry in source {
                result.insert(entry.key().clone(), entry.value().clone());
            }
            result
        }
        let engine = Engine::new(EngineConfig::mysql_strict());
        for table in 0..100 {
            engine
                .execute_sql(&format!(
                    "CREATE TABLE table_{table} (id INT PRIMARY KEY, value INT)"
                ))
                .unwrap();
        }
        engine
            .execute_sql("INSERT INTO table_0 VALUES (1, 10)")
            .unwrap();
        for shards in [64, 2, 64, 2, 64, 2] {
            {
                let mut state = engine.shared.committed.lock();
                let mut raw = state.databases["app"].fork().unwrap();
                raw.schemas = reshard(&raw.schemas, shards);
                raw.rows = reshard(&raw.rows, shards);
                raw.auto_inc = reshard(&raw.auto_inc, shards);
                raw.indexes = reshard(&raw.indexes, shards);
                raw.index_comments = reshard(&raw.index_comments, shards);
                raw.user_variables = reshard(&raw.user_variables, shards);
                raw.prepared_statements = reshard(&raw.prepared_statements, shards);
                raw.views = reshard(&raw.views, shards);
                state.databases.insert("app".into(), Arc::new(raw));
            }
            for _ in 0..100 {
                engine
                    .execute_sql("SELECT id FROM table_0 WHERE id = 1")
                    .unwrap();
            }
            let started = std::time::Instant::now();
            for _ in 0..3000 {
                assert_eq!(
                    engine
                        .execute_sql("SELECT id FROM table_0 WHERE id = 1")
                        .unwrap()[0]
                        .rows[0]["id"],
                    1
                );
            }
            eprintln!(
                "{shards} shards: 3000 SELECTs in {:.3} ms",
                started.elapsed().as_secs_f64() * 1000.0
            );
        }
    }

    #[test]
    fn forks_share_schema_and_cached_syntax_without_stale_ddl_results() {
        let engine = Engine::new(EngineConfig::mysql_strict());
        engine.execute_sql("CREATE TABLE items (id INT PRIMARY KEY); CREATE TABLE unrelated (id INT PRIMARY KEY)").unwrap();
        let before = engine.shared.committed.lock().databases["app"].clone();
        let mut reader = engine.session();
        let sql = "SELECT * FROM items";
        assert_eq!(reader.execute_sql(sql).unwrap()[0].columns, ["id"]);
        let read = reader.inner.lock().session_state.as_ref().unwrap().clone();
        for table in ["items", "unrelated"] {
            assert!(
                before
                    .schemas
                    .get(table)
                    .unwrap()
                    .ptr_eq(&read.schemas.get(table).unwrap())
            );
        }
        assert!(Arc::ptr_eq(
            &before.parsed_select_cache,
            &read.parsed_select_cache
        ));
        let cached = read
            .parsed_select_cache
            .lock()
            .entries
            .get(sql)
            .unwrap()
            .clone();
        reader.execute_sql(sql).unwrap();
        assert!(Arc::ptr_eq(
            &cached,
            reader
                .inner
                .lock()
                .session_state
                .as_ref()
                .unwrap()
                .parsed_select_cache
                .lock()
                .entries
                .get(sql)
                .unwrap()
        ));

        engine
            .execute_sql("ALTER TABLE items ADD COLUMN value INT")
            .unwrap();
        let after = engine.shared.committed.lock().databases["app"].clone();
        assert!(
            !before
                .schemas
                .get("items")
                .unwrap()
                .ptr_eq(&after.schemas.get("items").unwrap())
        );
        assert!(
            before
                .schemas
                .get("unrelated")
                .unwrap()
                .ptr_eq(&after.schemas.get("unrelated").unwrap())
        );
        assert!(
            !before
                .schemas
                .get("items")
                .unwrap()
                .columns
                .contains_key("value")
        );
        assert_eq!(reader.execute_sql(sql).unwrap()[0].columns, ["id", "value"]);
        assert!(Arc::ptr_eq(
            &cached,
            after.parsed_select_cache.lock().entries.get(sql).unwrap()
        ));
    }

    #[test]
    fn statement_forks_share_tables_and_detach_only_writes() {
        let engine = Engine::new(EngineConfig::mysql_strict());
        engine
            .execute_sql(
                "CREATE TABLE items (id INT PRIMARY KEY, value INT, INDEX by_value (value));
             CREATE TABLE unrelated (id INT PRIMARY KEY, value INT, INDEX by_value (value));
             INSERT INTO items VALUES (1, 10);
             INSERT INTO unrelated VALUES (1, 20)",
            )
            .unwrap();
        let before = engine.shared.committed.lock().databases["app"].clone();
        let mut reader = engine.session();
        reader
            .execute_sql("SELECT id FROM items WHERE value = 10")
            .unwrap();
        let read = reader.inner.lock().session_state.as_ref().unwrap().clone();
        for table in ["items", "unrelated"] {
            assert!(
                before
                    .rows
                    .get(table)
                    .unwrap()
                    .ptr_eq(&read.rows.get(table).unwrap())
            );
            assert!(
                before
                    .indexes
                    .get(table)
                    .unwrap()
                    .ptr_eq(&read.indexes.get(table).unwrap())
            );
        }

        engine
            .execute_sql("INSERT INTO items VALUES (2, 30)")
            .unwrap();
        let after = engine.shared.committed.lock().databases["app"].clone();
        assert!(
            !before
                .rows
                .get("items")
                .unwrap()
                .ptr_eq(&after.rows.get("items").unwrap())
        );
        assert!(
            !before
                .indexes
                .get("items")
                .unwrap()
                .ptr_eq(&after.indexes.get("items").unwrap())
        );
        assert!(
            before
                .rows
                .get("unrelated")
                .unwrap()
                .ptr_eq(&after.rows.get("unrelated").unwrap())
        );
        assert!(
            before
                .indexes
                .get("unrelated")
                .unwrap()
                .ptr_eq(&after.indexes.get("unrelated").unwrap())
        );
        assert_eq!(before.rows.get("items").unwrap().len(), 1);
        assert_eq!(after.rows.get("items").unwrap().len(), 2);

        // The first row mutates the private fork before the duplicate fails.
        assert!(
            engine
                .execute_sql("INSERT INTO items VALUES (3, 40), (2, 50)")
                .is_err()
        );
        let failed = engine.shared.committed.lock().databases["app"].clone();
        assert!(Arc::ptr_eq(&after, &failed));
        assert!(
            after
                .rows
                .get("items")
                .unwrap()
                .ptr_eq(&failed.rows.get("items").unwrap())
        );
        assert!(
            after
                .indexes
                .get("items")
                .unwrap()
                .ptr_eq(&failed.indexes.get("items").unwrap())
        );
        assert!(
            engine
                .execute_sql("SELECT id FROM items WHERE value = 40")
                .unwrap()[0]
                .rows
                .is_empty()
        );
    }

    #[test]
    fn read_only_transaction_does_not_block_committed_updates_or_publish_old_state() {
        let engine = Engine::new(EngineConfig::mysql_strict());
        engine.execute_sql("CREATE TABLE items (id INT PRIMARY KEY, value INT); INSERT INTO items VALUES (1,1)").unwrap();
        let mut reader = engine.session();
        reader.execute_sql("BEGIN").unwrap();
        assert_eq!(
            reader.execute_sql("SELECT value FROM items").unwrap()[0].rows[0]["value"],
            1
        );
        engine.execute_sql("UPDATE items SET value=2").unwrap();
        assert_eq!(
            reader.execute_sql("SELECT value FROM items").unwrap()[0].rows[0]["value"],
            1
        );
        assert!(
            reader
                .execute_sql("UPDATE items SET value=3")
                .unwrap_err()
                .to_string()
                .contains("retry transaction")
        );
        reader.execute_sql("COMMIT").unwrap();
        assert_eq!(
            engine.execute_sql("SELECT value FROM items").unwrap()[0].rows[0]["value"],
            2
        );
    }
}
