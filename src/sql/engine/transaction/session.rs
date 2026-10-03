use super::*;

/// One connection's state, shared by its synchronous and asynchronous calls.
pub struct EngineSession {
    pub(super) inner: Arc<Mutex<SessionState>>,
    pub(super) shared: Arc<Coordinator>,
}
impl EngineSession {
    pub(crate) fn set_endpoint(
        &mut self,
        id: uuid::Uuid,
        scopes: Vec<AuthScope>,
        stop: Arc<std::sync::atomic::AtomicBool>,
    ) {
        let mut state = self.inner.lock();
        state.endpoint_id = Some(id);
        state.endpoint_stop = Some(stop);
        state.ceiling = Some(scopes.clone());
        state.identity.set_ceiling(scopes);
    }
    pub fn current_database(&self) -> String {
        self.inner.lock().database.clone()
    }
    pub fn is_in_transaction(&self) -> bool {
        self.inner.lock().is_in_transaction()
    }
    pub fn autocommit(&self) -> bool {
        self.inner.lock().autocommit()
    }
    pub fn authenticate(&mut self, username: &str, salt: &[u8], response: &[u8]) -> bool {
        self.inner.lock().authenticate(username, salt, response)
    }
    pub(crate) fn authenticate_external(
        &mut self,
        username: String,
        scopes: Vec<AuthScope>,
    ) -> Result<()> {
        self.inner.lock().authenticate_external(username, scopes)
    }
    pub(crate) fn require_administrator(&self) -> Result<()> {
        self.inner.lock().require_administrator()
    }
    pub fn use_database(&mut self, database: &str) -> Result<()> {
        self.inner.lock().use_database(database)
    }
    pub(crate) fn set_client_host(&mut self, host: String) {
        self.inner.lock().set_client_host(host);
    }
    pub(crate) fn prepare_sql_with_params_for_wire(
        &mut self,
        sql: &str,
        params: &[Value],
    ) -> Result<Vec<QueryResult>> {
        self.inner
            .lock()
            .prepare_sql_with_params_for_wire(sql, params)
    }
    pub fn execute_sql(&mut self, sql: &str) -> Result<Vec<QueryResult>> {
        self.execute_sql_with_params(sql, &[])
    }
    pub fn execute_sql_with_params(
        &mut self,
        sql: &str,
        params: &[Value],
    ) -> Result<Vec<QueryResult>> {
        self.execute(sql, params, true, &mut |_| Ok(None))
    }
    fn execute(
        &mut self,
        sql: &str,
        params: &[Value],
        normalize: bool,
        special: &mut dyn FnMut(&str) -> Result<Option<Vec<QueryResult>>>,
    ) -> Result<Vec<QueryResult>> {
        crate::runtime::check_reentry()?;
        let mut session = self.inner.lock();
        self.shared.check()?;
        ensure!(
            !session
                .endpoint_stop
                .as_ref()
                .is_some_and(|stop| stop.load(AtomicOrdering::Acquire)),
            "SQL endpoint stopped"
        );
        session.last_query_read = false;
        let original_sql = sql.to_owned();
        let sql = substitute_params(sql, params)?;
        if !self.shared.query_filters.is_empty()
            && !self
                .shared
                .committed
                .lock()
                .catalog
                .is_admin(&session.identity)
        {
            session.authorize_request(&sql)?;
        }
        let request = crate::QueryRequest {
            sql,
            parameters: params.to_vec(),
            prepared: !params.is_empty(),
            context: crate::QueryContext {
                session_id: session.session_id,
                database: session.database.clone(),
                username: session.identity.username.clone(),
                endpoint_id: session.endpoint_id,
            },
        };
        let filters = self.shared.query_filters.clone();
        let (request, synthetic) = if filters.is_empty() {
            (request, None)
        } else {
            self.shared
                .runtime
                .run_cancellable(session.endpoint_stop.clone(), async move {
                    let mut request = request;
                    let results = filters.apply(&mut request).await?;
                    Ok((request, results))
                })?
        };
        if !self
            .shared
            .committed
            .lock()
            .catalog
            .is_admin(&session.identity)
            && (!self.shared.query_filters.is_empty() || synthetic.is_some())
        {
            session.authorize_request(&request.sql)?;
        }
        let synthetic = match synthetic {
            Some(results) => Some(results),
            None => special(&request.sql)?,
        };
        let synthetic_read = synthetic.is_some()
            && request
                .sql
                .trim_start()
                .to_ascii_uppercase()
                .starts_with("SELECT");
        let results = match synthetic {
            Some(results) => results,
            None => session.run(&original_sql, Ok(request.sql.clone()), normalize)?,
        };
        let read = synthetic_read || session.last_query_read;
        let database = request.context.database.clone();
        let event_sql = request.sql.clone();
        let filters = self.shared.result_filters.clone();
        let results = if filters.is_empty() {
            results
        } else {
            self.shared
                .runtime
                .run_cancellable(session.endpoint_stop.clone(), async move {
                    let mut results = results;
                    filters.apply(&request, &mut results).await?;
                    Ok(results)
                })?
        };
        if read {
            self.shared.hooks.read(&database, &event_sql, &results);
        }
        Ok(results)
    }
    pub async fn execute_sql_async(&mut self, sql: impl Into<String>) -> Result<Vec<QueryResult>> {
        self.execute_sql_with_params_async(sql, Vec::new()).await
    }
    pub async fn execute_sql_with_params_async(
        &mut self,
        sql: impl Into<String>,
        params: Vec<Value>,
    ) -> Result<Vec<QueryResult>> {
        crate::runtime::check_reentry()?;
        let sql = sql.into();
        let mut session = Self {
            inner: self.inner.clone(),
            shared: self.shared.clone(),
        };
        tokio::task::spawn_blocking(move || session.execute_sql_with_params(&sql, &params)).await?
    }
    pub(crate) fn execute_wire_with(
        &mut self,
        sql: &str,
        params: &[Value],
        mut special: impl FnMut(&str) -> Result<Option<Vec<QueryResult>>>,
    ) -> Result<Vec<QueryResult>> {
        self.execute(sql, params, false, &mut special)
    }
}
