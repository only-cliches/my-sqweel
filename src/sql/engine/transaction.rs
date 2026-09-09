//! Serialized development writers with private statement/transaction state.
use super::catalog::{
    AdminCommand, Catalog, CatalogEffect, Identity, PreparedCommand, PreparedSource, parse_use,
};
use super::*;
use crate::storage::transaction_image::TransactionImageStore;
use parking_lot::Condvar;

#[derive(Clone, Serialize, Deserialize)]
struct Image {
    catalog: Catalog,
    databases: BTreeMap<String, Snapshot>,
}

#[derive(Clone)]
struct Committed {
    catalog: Catalog,
    databases: BTreeMap<String, Arc<RawEngine>>,
}

struct Coordinator {
    cfg: EngineConfig,
    committed: Mutex<Committed>,
    writer: Mutex<Writers>,
    publication: Mutex<()>,
    available: Condvar,
    advisory_locks: Mutex<HashMap<String, (uuid::Uuid, u32)>>,
    advisory_available: Condvar,
    image: Option<TransactionImageStore>,
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
pub struct Engine {
    shared: Arc<Coordinator>,
    default_session: Mutex<EngineSession>,
}

/// Connection-owned state. Dropping a session rolls back uncommitted changes.
pub struct EngineSession {
    shared: Arc<Coordinator>,
    database: String,
    identity: Identity,
    transaction: Option<(Option<WriterLease>, Committed)>,
    transaction_read: bool,
    observed_tables: Option<BTreeSet<String>>,
    observed_columns: BTreeMap<String, Option<BTreeSet<String>>>,
    savepoints: Vec<(String, Committed)>,
    autocommit: bool,
    session_state: Option<Arc<RawEngine>>,
    variables: HashMap<String, Value>,
    prepared: HashMap<String, (String, String)>,
    session_id: uuid::Uuid,
}

impl Default for Engine {
    fn default() -> Self {
        Self::new(EngineConfig::default())
    }
}

impl Engine {
    pub fn new(cfg: EngineConfig) -> Self {
        Self::open_with_data_dir(cfg, None).expect("failed to open MySqweel")
    }

    pub fn open_with_data_dir(cfg: EngineConfig, data_dir: Option<&str>) -> Result<Self> {
        let image = data_dir.map(TransactionImageStore::open).transpose()?;
        let recovered = image
            .as_ref()
            .map(|store| store.load::<Image>())
            .transpose()?
            .flatten();
        let mut databases = BTreeMap::new();
        let mut catalog = Catalog::default();
        if let Some(recovered) = recovered {
            catalog = recovered.catalog;
            for (name, snapshot) in recovered.databases {
                let mut raw = RawEngine::with_storage(cfg.clone(), Arc::new(PrivateStorage))?;
                raw.database_name = name.clone();
                raw.apply_snapshot(snapshot);
                databases.insert(name, Arc::new(raw));
            }
        } else {
            // Never silently open an old Lux directory as an empty database.
            if let Some(path) = data_dir {
                if std::path::Path::new(path).join("storage").exists() {
                    return Err(anyhow!(
                        "unsupported legacy development database format; remove the old data directory and restart with fresh storage"
                    ));
                }
            }
            databases.insert(
                "app".into(),
                Arc::new(RawEngine::with_storage(
                    cfg.clone(),
                    Arc::new(PrivateStorage),
                )?),
            );
        }
        let shared = Arc::new(Coordinator {
            cfg,
            committed: Mutex::new(Committed { databases, catalog }),
            writer: Mutex::new(Writers::default()),
            publication: Mutex::new(()),
            available: Condvar::new(),
            advisory_locks: Mutex::new(HashMap::new()),
            advisory_available: Condvar::new(),
            image,
        });
        Ok(Self {
            default_session: Mutex::new(EngineSession::new(shared.clone())),
            shared,
        })
    }

    /// Replace the bootstrap administrator before accepting connections.
    pub fn set_admin_credentials(&self, username: &str, password: &str) -> Result<()> {
        if username.is_empty() {
            return Err(anyhow!("administrator username must not be empty"));
        }
        let mut session = self.default_session.lock();
        let _lease = self.shared.acquire()?;
        let mut state = self.shared.committed.lock().clone();
        state.catalog.set_admin_credentials(username, password);
        let identity = state.catalog.identity(username)?;
        self.shared.publish(state)?;
        session.identity = identity;
        Ok(())
    }
    pub fn session(&self) -> EngineSession {
        EngineSession::new(self.shared.clone())
    }
    pub fn compatibility_profile(&self) -> CompatibilityProfile {
        self.shared.cfg.compatibility_profile
    }
    pub fn execute_sql(&self, sql: &str) -> Result<Vec<QueryResult>> {
        self.default_session
            .lock()
            .run(sql, Ok(sql.to_owned()), true)
    }
    pub fn execute_sql_with_params(&self, sql: &str, params: &[Value]) -> Result<Vec<QueryResult>> {
        self.default_session
            .lock()
            .run(sql, substitute_params(sql, params), true)
    }
    pub fn execute_statement(&self, statement: Statement) -> Result<QueryResult> {
        self.execute_sql(&statement.to_string())?
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("empty statement"))
    }
    pub fn subscribe_query_events(&self, options: QueryEventOptions) -> QueryEventStream {
        self.shared.committed.lock().databases["app"].subscribe_query_events(options)
    }
    pub fn snapshot(&self) -> Snapshot {
        self.shared.committed.lock().databases["app"].snapshot()
    }
    pub fn restore_snapshot(&self, snapshot: Snapshot) -> Result<()> {
        if snapshot.version != 1 {
            return Err(anyhow!(
                "unsupported snapshot version: {}",
                snapshot.version
            ));
        }
        self.mutate(|raw| {
            raw.apply_snapshot(snapshot);
            Ok(())
        })
    }
    pub fn drift_report(&self) -> Value {
        self.shared.committed.lock().databases["app"].drift_report()
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
        let _lease = self.shared.acquire_database("app")?;
        let mut state = self.shared.committed.lock().clone();
        let raw = state.databases["app"].fork()?;
        let value = operation(&raw)?;
        state.databases.insert("app".into(), Arc::new(raw));
        self.shared.publish_database("app", state)?;
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
            .image
            .as_ref()
            .is_some_and(TransactionImageStore::is_poisoned)
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
        self.publish_locked(current)
    }
    fn publish(&self, state: Committed) -> Result<()> {
        let _publication = self.publication.lock();
        self.publish_locked(state)
    }
    fn publish_locked(&self, state: Committed) -> Result<()> {
        self.check()?;
        if let Some(image) = &self.image {
            image.commit(&Image {
                catalog: state.catalog.clone(),
                databases: state
                    .databases
                    .iter()
                    .map(|(name, raw)| (name.clone(), raw.snapshot()))
                    .collect(),
            })?;
        }
        *self.committed.lock() = state;
        Ok(())
    }
}

impl EngineSession {
    fn new(shared: Arc<Coordinator>) -> Self {
        let identity = shared.committed.lock().catalog.administrator_identity();
        Self {
            shared,
            database: "app".into(),
            identity,
            transaction: None,
            transaction_read: false,
            observed_tables: Some(BTreeSet::new()),
            observed_columns: BTreeMap::new(),
            savepoints: Vec::new(),
            autocommit: true,
            session_state: None,
            variables: HashMap::new(),
            prepared: HashMap::new(),
            session_id: uuid::Uuid::new_v4(),
        }
    }
    pub(crate) fn system_variable(&self, name: &str) -> Option<Value> {
        match name.to_ascii_lowercase().as_str() {
            "autocommit" => Some(Value::Number(u64::from(self.autocommit).into())),
            "transaction_isolation" | "tx_isolation" => {
                Some(Value::String("REPEATABLE-READ".into()))
            }
            key => self.variables.get(key).cloned(),
        }
    }
    pub fn current_database(&self) -> &str {
        &self.database
    }
    pub fn authenticate(&mut self, username: &str, salt: &[u8], response: &[u8]) -> bool {
        self.rollback();
        self.release_advisory_locks();
        self.session_state = None;
        self.variables.clear();
        self.prepared.clear();
        self.autocommit = true;
        self.identity = Identity::unauthenticated();
        let state = self.shared.committed.lock();
        if self.shared.check().is_err() || !state.catalog.authenticate(username, salt, response) {
            return false;
        }
        let Ok(identity) = state.catalog.identity(username) else {
            return false;
        };
        self.identity = identity;
        true
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
            .map(|(_, state)| state.clone())
            .unwrap_or_else(|| self.shared.committed.lock().clone());
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
        if let Some(session) = &self.session_state {
            raw.copy_session_from(session);
        } else {
            raw.clear_session();
        }
        raw.execute_sql_internal(&sql, &sql, false, false)
    }
    pub fn is_in_transaction(&self) -> bool {
        self.transaction.is_some()
    }
    pub fn autocommit(&self) -> bool {
        self.autocommit
    }
    pub fn execute_sql(&mut self, sql: &str) -> Result<Vec<QueryResult>> {
        self.run(sql, Ok(sql.to_owned()), true)
    }
    pub fn execute_sql_with_params(
        &mut self,
        sql: &str,
        params: &[Value],
    ) -> Result<Vec<QueryResult>> {
        self.run(sql, substitute_params(sql, params), true)
    }
    pub(crate) fn execute_sql_for_wire(&mut self, sql: &str) -> Result<Vec<QueryResult>> {
        self.run(sql, Ok(sql.to_owned()), false)
    }
    pub(crate) fn execute_sql_with_params_for_wire(
        &mut self,
        sql: &str,
        params: &[Value],
    ) -> Result<Vec<QueryResult>> {
        self.run(sql, substitute_params(sql, params), false)
    }
    fn run(
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
    fn authorize(&self, sql: &str) -> Result<String> {
        let state = self.shared.committed.lock();
        match state
            .catalog
            .authorize_and_normalize(&self.identity, &self.database, sql)
        {
            Ok(sql) => Ok(sql),
            Err(error) => {
                // Compatibility rewrites still pass the same complete authorization
                // visitor; parser failure never grants a bypass to the raw executor.
                if catalog::parse_session_statement(sql).is_ok() {
                    return Err(error);
                }
                let raw = state.databases.get(&self.database).ok_or(error)?;
                let rewritten = raw.rewrite_sql_for_parser(sql);
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
        if name != "GET_LOCK" && name != "RELEASE_LOCK" {
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
        let value = if name == "RELEASE_LOCK" {
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
            rows: vec![Map::from_iter([(column, value)])],
            ..QueryResult::default()
        }))
    }
    fn begin(&mut self) -> Result<()> {
        self.commit()?;
        let state = self.shared.committed.lock().clone();
        state
            .catalog
            .check_database(&self.identity, &self.database)?;
        self.transaction = Some((None, state));
        self.transaction_read = false;
        self.observed_tables = Some(BTreeSet::new());
        self.observed_columns.clear();
        Ok(())
    }
    fn commit(&mut self) -> Result<()> {
        if let Some((Some(lease), state)) = self.transaction.take() {
            self.shared.publish_database(&self.database, state)?;
            drop(lease);
        }
        self.savepoints.clear();
        Ok(())
    }
    fn rollback(&mut self) {
        self.transaction = None;
        self.savepoints.clear();
    }
    fn execute(&mut self, sql: &str, normalize: bool, emit: bool) -> Result<Vec<QueryResult>> {
        self.shared.check()?;
        let mut results = Vec::new();
        for statement in split_sql_statements(sql) {
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
                        let mut raw = RawEngine::with_storage(
                            self.shared.cfg.clone(),
                            Arc::new(PrivateStorage),
                        )?;
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
            if matches!(ast, Some(Statement::Query(query)) if query.locks.iter().any(|lock| lock.nonblock.is_some() || lock.of.is_some()))
            {
                return Err(anyhow!(
                    "unsupported SQL feature: SELECT locking clauses other than plain FOR UPDATE"
                ));
            }
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
                    let rows = state
                        .catalog
                        .database_names()
                        .filter(|name| state.catalog.check_database(&self.identity, name).is_ok())
                        .map(|name| {
                            Map::from_iter([("Database".into(), Value::String(name.into()))])
                        })
                        .collect();
                    results.push(QueryResult {
                        columns: vec!["Database".into()],
                        rows,
                        ..QueryResult::default()
                    });
                    continue;
                }
                Some(Statement::StartTransaction { modes, .. }) => {
                    if !modes.is_empty() {
                        return Err(anyhow!(
                            "transaction modes are not supported; use REPEATABLE READ"
                        ));
                    }
                    self.begin()?;
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
                    if self.is_in_transaction()
                        || snapshot.is_some()
                        || modes.len() != 1
                        || modes[0].to_string() != "ISOLATION LEVEL REPEATABLE READ"
                    {
                        return Err(anyhow!(
                            "only REPEATABLE READ isolation is supported, before starting a transaction"
                        ));
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
                || matches!(ast, Some(Statement::Query(query)) if query.locks.is_empty())
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
                if let Some((None, snapshot)) = &mut self.transaction {
                    *snapshot = self.shared.committed.lock().clone();
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
            let mut state = self
                .transaction
                .as_ref()
                .map(|(_, state)| state.clone())
                .unwrap_or_else(|| self.shared.committed.lock().clone());
            // A read-only snapshot must not block independent cancellation/lease
            // updates. Upgrade only before the first write, then serialize writers.
            // A changed snapshot cannot safely overwrite newer committed state.
            let lease = if let Some((transaction_lease, snapshot)) = &mut self.transaction {
                if let Some(lease) = lease {
                    let current = self.shared.committed.lock().clone();
                    let unchanged = snapshot
                        .databases
                        .get(&self.database)
                        .zip(current.databases.get(&self.database))
                        .is_some_and(|(before, after)| {
                            Arc::ptr_eq(before, after)
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
            let normalized = self.authorize(text)?;
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
            if let Some(session) = &self.session_state {
                raw.copy_session_from(session);
            } else {
                raw.clear_session();
            }
            let mut out = raw.execute_sql_internal(text, &normalized, normalize, emit)?;
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
            self.session_state = Some(raw.clone());
            if !read {
                state.databases.insert(self.database.clone(), raw);
                if let Some((_, pending)) = &mut self.transaction {
                    *pending = state;
                } else {
                    self.shared.publish_database(&self.database, state)?;
                }
            }
            drop(lease);
            results.extend(out);
        }
        Ok(results)
    }
}

impl Drop for EngineSession {
    fn drop(&mut self) {
        self.release_advisory_locks();
    }
}

// Only direct table reads have a bounded dependency set here. Views, metadata,
// functions and nested queries conservatively retain whole-database validation.
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
    fn same_table_state(
        &self,
        other: &Self,
        table: &str,
        columns: Option<&BTreeSet<String>>,
    ) -> bool {
        self.schemas.get(table).as_deref() == other.schemas.get(table).as_deref()
            && match (self.rows.get(table), other.rows.get(table), columns) {
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
    fn fork(&self) -> Result<Self> {
        // Copying a private working version is engine bookkeeping, not logical
        // SQL row access. Keep diagnostics scoped to the requested operation.
        let _metrics = QueryMetricsGuard::install(Rc::new(QueryMetricsRecorder::new(false)));
        // ponytail: whole-database copies bound this engine to development-sized
        // datasets. Use table-level copy-on-write before pursuing large datasets.
        let mut raw = Self::with_storage(self.cfg.clone(), Arc::new(PrivateStorage))?;
        raw.database_name = self.database_name.clone();
        // Committed engines are immutable. Clone their owned indexes along with
        // rows instead of re-evaluating every indexed value on every SELECT.
        raw.schemas = self.schemas.clone();
        raw.rows = self.rows.clone();
        raw.auto_inc = self.auto_inc.clone();
        raw.indexes = self.indexes.clone();
        raw.views = self.views.clone();
        raw.index_comments = self.index_comments.clone();
        raw.copy_session_from(self);
        raw.query_event_subscribers = self.query_event_subscribers.clone();
        raw.next_query_id = self.next_query_id.clone();
        raw.read_query_count = self.read_query_count.clone();
        raw.write_query_count = self.write_query_count.clone();
        Ok(raw)
    }
    fn clear_session(&self) {
        self.user_variables.clear();
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

// Private engines never publish intermediate row/index changes to Lux. The
// coordinator's durable image is the sole authoritative persistence boundary.
struct PrivateStorage;
impl RedisStore for PrivateStorage {
    fn is_persistent(&self) -> bool {
        false
    }
    fn hset(&self, _: &str, _: &str, _: &str) -> Result<()> {
        Ok(())
    }
    fn hdel(&self, _: &str, _: &str) -> Result<()> {
        Ok(())
    }
    fn hgetall(&self, _: &str) -> Result<BTreeMap<String, String>> {
        Ok(BTreeMap::new())
    }
    fn sadd(&self, _: &str, _: &str) -> Result<()> {
        Ok(())
    }
    fn srem(&self, _: &str, _: &str) -> Result<()> {
        Ok(())
    }
    fn smembers(&self, _: &str) -> Result<BTreeSet<String>> {
        Ok(BTreeSet::new())
    }
    fn del(&self, _: &str) -> Result<()> {
        Ok(())
    }
    fn keys(&self, _: &str) -> Result<Vec<String>> {
        Ok(Vec::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
