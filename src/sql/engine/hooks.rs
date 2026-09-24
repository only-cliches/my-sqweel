//! Optional, best-effort notifications for trusted embedding applications.
use super::*;
use futures_util::FutureExt;
use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::sync::Weak;
use tokio::sync::{mpsc as async_mpsc, watch};

/// Select event kinds independently. Capacity counts events, not rows or bytes.
#[derive(Debug, Clone, Copy)]
pub struct QueryHookOptions {
    pub read: bool,
    pub write: bool,
    pub delete: bool,
    pub table_created: bool,
    pub table_updated: bool,
    pub table_dropped: bool,
    pub capacity: usize,
}

impl Default for QueryHookOptions {
    fn default() -> Self {
        Self {
            read: false,
            write: true,
            delete: true,
            table_created: false,
            table_updated: false,
            table_dropped: false,
            capacity: 256,
        }
    }
}

/// Named primary-key values, or the engine's stable stored key for a keyless row.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum QueryHookKey {
    Primary(Map<String, Value>),
    Opaque(String),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QueryHookRow {
    pub key: QueryHookKey,
    pub row: Map<String, Value>,
}

/// Row changes are batched by table and commit. Read results describe one request.
#[derive(Debug, Clone)]
pub enum QueryHookEvent {
    Read {
        database: String,
        sql: String,
        results: Vec<QueryResult>,
    },
    Write {
        database: String,
        table: String,
        rows: Vec<QueryHookRow>,
    },
    Delete {
        database: String,
        table: String,
        keys: Vec<QueryHookKey>,
    },
    TableCreated {
        database: String,
        table: String,
        schema: TableSchemaHint,
    },
    TableUpdated {
        database: String,
        table: String,
        schema: TableSchemaHint,
    },
    TableDropped {
        database: String,
        table: String,
    },
}

impl QueryHookOptions {
    fn accepts(&self, event: &QueryHookEvent) -> bool {
        match event {
            QueryHookEvent::Read { .. } => self.read,
            QueryHookEvent::Write { .. } => self.write,
            QueryHookEvent::Delete { .. } => self.delete,
            QueryHookEvent::TableCreated { .. } => self.table_created,
            QueryHookEvent::TableUpdated { .. } => self.table_updated,
            QueryHookEvent::TableDropped { .. } => self.table_dropped,
        }
    }
    fn changes(&self) -> bool {
        self.write || self.delete || self.table_created || self.table_updated || self.table_dropped
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum QueryHookError {
    #[error("query hook queue overflow; resynchronize before subscribing again")]
    Overflow,
    #[error("query hook callback failed: {0}")]
    Callback(String),
    #[error("query hook callback panicked")]
    Panicked,
}

type HookOutcome = std::result::Result<(), QueryHookError>;

struct Listener {
    options: QueryHookOptions,
    sender: async_mpsc::Sender<Arc<QueryHookEvent>>,
    stopped: watch::Sender<Option<HookOutcome>>,
}

impl Listener {
    fn active(&self) -> bool {
        self.stopped.borrow().is_none()
    }
    fn stop(&self, outcome: HookOutcome) {
        self.stopped.send_if_modified(|state| {
            if state.is_some() {
                return false;
            }
            if let Err(error) = &outcome {
                tracing::error!(%error, "query hook stopped");
            }
            *state = Some(outcome);
            true
        });
    }
    fn reserve(&self) -> Option<async_mpsc::OwnedPermit<Arc<QueryHookEvent>>> {
        if !self.active() {
            return None;
        }
        match self.sender.clone().try_reserve_owned() {
            Ok(permit) => Some(permit),
            Err(async_mpsc::error::TrySendError::Full(_)) => {
                self.stop(Err(QueryHookError::Overflow));
                None
            }
            Err(async_mpsc::error::TrySendError::Closed(_)) => {
                self.stop(Ok(()));
                None
            }
        }
    }
    fn send(&self, event: Arc<QueryHookEvent>) {
        if let Some(permit) = self.reserve() {
            permit.send(event);
        }
    }
}

/// Keep this handle alive to receive events. Cancellation discards queued events.
pub struct QueryHookSubscription {
    listener: Arc<Listener>,
    registry: Weak<HookRegistry>,
    status: watch::Receiver<Option<HookOutcome>>,
}

impl QueryHookSubscription {
    pub fn cancel(&self) {
        self.listener.stop(Ok(()));
        if let Some(registry) = self.registry.upgrade() {
            registry.remove(&self.listener);
        }
    }
    /// Wait for cancellation, queue overflow, or callback failure/panic.
    pub async fn wait(&mut self) -> HookOutcome {
        loop {
            if let Some(outcome) = self.status.borrow_and_update().clone() {
                return outcome;
            }
            if self.status.changed().await.is_err() {
                return Ok(());
            }
        }
    }
}

impl Drop for QueryHookSubscription {
    fn drop(&mut self) {
        self.cancel();
    }
}

type Delivery = (
    Arc<QueryHookEvent>,
    Vec<(Arc<Listener>, async_mpsc::OwnedPermit<Arc<QueryHookEvent>>)>,
);

#[derive(Default)]
struct RegistryState {
    listeners: Vec<Arc<Listener>>,
    deferred: Option<Vec<Delivery>>,
}

#[derive(Default)]
pub(crate) struct HookRegistry {
    state: Mutex<RegistryState>,
    pub(crate) external_reads: std::sync::atomic::AtomicBool,
}

impl Drop for HookRegistry {
    fn drop(&mut self) {
        for listener in &self.state.get_mut().listeners {
            listener.stop(Ok(()));
        }
    }
}

impl HookRegistry {
    pub(crate) fn subscribe<F, Fut>(
        self: &Arc<Self>,
        options: QueryHookOptions,
        callback: F,
    ) -> Result<QueryHookSubscription>
    where
        F: Fn(Arc<QueryHookEvent>) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<()>> + Send + 'static,
    {
        anyhow::ensure!(options.capacity > 0, "query hook capacity must be positive");
        let runtime = tokio::runtime::Handle::try_current()?;
        let (sender, mut receiver) = async_mpsc::channel(options.capacity);
        let (stopped, status) = watch::channel(None);
        let listener = Arc::new(Listener {
            options,
            sender,
            stopped,
        });
        let mut stop = status.clone();
        let worker = listener.clone();
        let registry = Arc::downgrade(self);
        let cleanup = registry.clone();
        runtime.spawn(async move {
            let work = async {
                while let Some(event) = receiver.recv().await {
                    if !worker.active() {
                        break;
                    }
                    match AssertUnwindSafe(async { callback(event).await })
                        .catch_unwind()
                        .await
                    {
                        Ok(Ok(())) => {}
                        Ok(Err(error)) => {
                            worker.stop(Err(QueryHookError::Callback(error.to_string())));
                            break;
                        }
                        Err(_) => {
                            worker.stop(Err(QueryHookError::Panicked));
                            break;
                        }
                    }
                }
            };
            tokio::select! { biased; _ = stop.changed() => {}, _ = work => {} }
            if let Some(registry) = cleanup.upgrade() {
                registry.remove(&worker);
            }
        });
        self.state.lock().listeners.push(listener.clone());
        Ok(QueryHookSubscription {
            listener,
            registry,
            status,
        })
    }

    fn remove(&self, listener: &Arc<Listener>) {
        self.state
            .lock()
            .listeners
            .retain(|item| !Arc::ptr_eq(item, listener));
    }

    fn options(&self) -> QueryHookOptions {
        let mut options = QueryHookOptions {
            write: false,
            delete: false,
            ..Default::default()
        };
        for listener in &self.state.lock().listeners {
            if !listener.active() {
                continue;
            }
            options.read |= listener.options.read;
            options.write |= listener.options.write;
            options.delete |= listener.options.delete;
            options.table_created |= listener.options.table_created;
            options.table_updated |= listener.options.table_updated;
            options.table_dropped |= listener.options.table_dropped;
        }
        options
    }

    fn emit(&self, event: QueryHookEvent) {
        let mut state = self.state.lock();
        let listeners = state
            .listeners
            .iter()
            .filter(|s| s.active() && s.options.accepts(&event))
            .cloned()
            .collect::<Vec<_>>();
        if listeners.is_empty() {
            return;
        }
        let event = Arc::new(event);
        if !matches!(&*event, QueryHookEvent::Read { .. }) && state.deferred.is_some() {
            let deliveries = listeners
                .into_iter()
                .filter_map(|listener| listener.reserve().map(|permit| (listener, permit)))
                .collect::<Vec<_>>();
            if !deliveries.is_empty() {
                state.deferred.as_mut().unwrap().push((event, deliveries));
            }
        } else {
            for listener in listeners {
                listener.send(event.clone());
            }
        }
    }

    pub(crate) fn read(&self, database: &str, sql: &str, results: &[QueryResult]) {
        if self.options().read {
            self.emit(QueryHookEvent::Read {
                database: database.into(),
                sql: sql.into(),
                results: results.to_vec(),
            });
        }
    }

    pub(crate) fn automatic_reads(&self) -> bool {
        !self.external_reads.load(AtomicOrdering::Relaxed) && self.options().read
    }

    pub(crate) fn begin_deferred(&self) {
        let mut state = self.state.lock();
        assert!(
            state.deferred.is_none(),
            "overlapping async hook publication"
        );
        state.deferred = Some(Vec::new());
    }

    pub(crate) fn end_deferred(&self, success: bool) {
        let mut state = self.state.lock();
        if let Some(events) = state.deferred.take().filter(|_| success) {
            for (event, listeners) in events {
                for (listener, permit) in listeners {
                    if listener.active() {
                        permit.send(event.clone());
                    }
                }
            }
        }
    }

    pub(super) fn changes(
        &self,
        before: &BTreeMap<String, Arc<RawEngine>>,
        after: &BTreeMap<String, Arc<RawEngine>>,
    ) {
        let options = self.options();
        if !options.changes() {
            return;
        }
        // Feed materialization must not inflate the originating query's metrics.
        let _metrics = QueryMetricsGuard::install(Rc::new(QueryMetricsRecorder::new(false)));
        let mut deletes = Vec::new();
        let mut drops = Vec::new();
        let mut schemas = Vec::new();
        let mut writes = Vec::new();
        for database in before.keys().chain(after.keys()).collect::<BTreeSet<_>>() {
            let old = before.get(database);
            let new = after.get(database);
            if old.zip(new).is_some_and(|(a, b)| Arc::ptr_eq(a, b)) {
                continue;
            }
            let tables = old
                .into_iter()
                .chain(new)
                .flat_map(|raw| {
                    raw.schemas
                        .iter()
                        .map(|s| s.key().clone())
                        .collect::<Vec<_>>()
                })
                .collect::<BTreeSet<_>>();
            for table in tables {
                let a = old.and_then(|raw| raw.schemas.get(&table));
                let b = new.and_then(|raw| raw.schemas.get(&table));
                if a.as_ref().is_some_and(|s| s.temporary)
                    || b.as_ref().is_some_and(|s| s.temporary)
                {
                    continue;
                }
                match (&a, &b) {
                    (Some(_), None) if options.table_dropped => {
                        drops.push(QueryHookEvent::TableDropped {
                            database: database.clone(),
                            table: table.clone(),
                        })
                    }
                    (None, Some(schema)) if options.table_created => {
                        schemas.push(QueryHookEvent::TableCreated {
                            database: database.clone(),
                            table: table.clone(),
                            schema: (***schema).clone(),
                        })
                    }
                    (Some(a), Some(b))
                        if options.table_updated
                            && !a.value().ptr_eq(b.value())
                            && !same_schema(a, b) =>
                    {
                        schemas.push(QueryHookEvent::TableUpdated {
                            database: database.clone(),
                            table: table.clone(),
                            schema: (***b).clone(),
                        })
                    }
                    _ => {}
                }
                if !options.write && !options.delete {
                    continue;
                }
                let old_rows = old.and_then(|raw| raw.rows.get(&table));
                let new_rows = new.and_then(|raw| raw.rows.get(&table));
                let same_shape = a.as_ref().map(|s| (&s.primary_key, &s.columns))
                    == b.as_ref().map(|s| (&s.primary_key, &s.columns));
                if same_shape
                    && old_rows
                        .as_ref()
                        .zip(new_rows.as_ref())
                        .is_some_and(|(a, b)| a.value().ptr_eq(b.value()))
                {
                    continue;
                }
                // ponytail: materialize changed tables linearly; track dirty row keys if this becomes costly.
                let previous = old
                    .map(|raw| hook_rows(raw, &table, a.as_deref().map(|s| &**s)))
                    .unwrap_or_default();
                let current = new
                    .map(|raw| hook_rows(raw, &table, b.as_deref().map(|s| &**s)))
                    .unwrap_or_default();
                let removed = if options.delete {
                    previous
                        .iter()
                        .filter(|(key, _)| !current.contains_key(*key))
                        .map(|(_, row)| row.key.clone())
                        .collect::<Vec<_>>()
                } else {
                    Vec::new()
                };
                let changed = if options.write {
                    current
                        .into_iter()
                        .filter(|(key, row)| previous.get(key) != Some(row))
                        .map(|(_, row)| row)
                        .collect::<Vec<_>>()
                } else {
                    Vec::new()
                };
                if !removed.is_empty() {
                    deletes.push(QueryHookEvent::Delete {
                        database: database.clone(),
                        table: table.clone(),
                        keys: removed,
                    });
                }
                if !changed.is_empty() {
                    writes.push(QueryHookEvent::Write {
                        database: database.clone(),
                        table: table.clone(),
                        rows: changed,
                    });
                }
            }
        }
        for event in deletes
            .into_iter()
            .chain(drops)
            .chain(schemas)
            .chain(writes)
        {
            self.emit(event);
        }
    }
}

fn same_schema(a: &TableSchemaHint, b: &TableSchemaHint) -> bool {
    let mut a = a.clone();
    let mut b = b.clone();
    a.updated_at = None;
    b.updated_at = None;
    a == b
}

fn hook_rows(
    raw: &RawEngine,
    table: &str,
    schema: Option<&TableSchemaHint>,
) -> BTreeMap<String, QueryHookRow> {
    let Some(rows) = raw.rows.get(table) else {
        return BTreeMap::new();
    };
    rows.iter()
        .map(|(id, stored)| {
            let row = hook_row(raw, table, schema, id, stored);
            let identity = serde_json::to_string(&row.key).expect("JSON row key serialization");
            (identity, row)
        })
        .collect()
}

fn hook_row(
    raw: &RawEngine,
    table: &str,
    schema: Option<&TableSchemaHint>,
    id: &str,
    stored: &StoredRow,
) -> QueryHookRow {
    let mut row = raw.current_schema_row(table, &stored.data);
    // The SQL evaluator adds internal markers for dropped columns. A feed exposes
    // only the complete current row, including defaults and generated values.
    if let Some(schema) = schema.filter(|s| !s.columns.is_empty()) {
        row.retain(|column, _| schema.columns.contains_key(column));
    }
    let key = match schema.filter(|s| !s.primary_key.is_empty()) {
        Some(schema) => QueryHookKey::Primary(
            schema
                .primary_key
                .iter()
                .map(|name| (name.clone(), row.get(name).cloned().unwrap_or(Value::Null)))
                .collect(),
        ),
        None => QueryHookKey::Opaque(id.into()),
    };
    QueryHookRow { key, row }
}
