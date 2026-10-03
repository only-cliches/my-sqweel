//! Shared query filters and execution context.
use crate::sql::engine::QueryResult;
use anyhow::{Result, anyhow};
use serde_json::Value;
use std::{future::Future, pin::Pin, sync::Arc};

#[derive(Debug, Clone)]
pub struct QueryContext {
    pub session_id: uuid::Uuid,
    pub database: String,
    pub username: String,
    pub endpoint_id: Option<uuid::Uuid>,
}
impl QueryRequest {
    pub fn context(&self) -> &QueryContext {
        &self.context
    }
}
/// A SQL request at the filter boundary.
///
/// Parameterized statements are bound before filters run.  Rewriting `sql`
/// therefore applies to the exact statement that will execute.
#[derive(Debug, Clone)]
pub struct QueryRequest {
    pub sql: String,
    pub parameters: Vec<Value>,
    pub prepared: bool,
    pub(crate) context: QueryContext,
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

/// Trusted application hook invoked after authorization and before execution.
///
/// Native async traits keep implementations straightforward.  Use
/// [`QueryFilters`] when several independently typed filters need to form an
/// ordered pipeline.
#[allow(async_fn_in_trait)]
pub trait QueryFilter: Send + Sync + 'static {
    fn filter(
        &self,
        request: &mut QueryRequest,
    ) -> impl Future<Output = Result<QueryFilterAction>> + Send;
}

/// Hook invoked for every completed SQL execution.
#[allow(async_fn_in_trait)]
pub trait ResultFilter: Send + Sync + 'static {
    fn filter(
        &self,
        request: &QueryRequest,
        results: &mut Vec<QueryResult>,
    ) -> impl Future<Output = Result<ResultFilterAction>> + Send;
}

trait ErasedQueryFilter: Send + Sync {
    fn filter<'a>(
        &'a self,
        request: &'a mut QueryRequest,
    ) -> Pin<Box<dyn Future<Output = Result<QueryFilterAction>> + Send + 'a>>;
}

impl<T: QueryFilter> ErasedQueryFilter for T {
    fn filter<'a>(
        &'a self,
        request: &'a mut QueryRequest,
    ) -> Pin<Box<dyn Future<Output = Result<QueryFilterAction>> + Send + 'a>> {
        Box::pin(QueryFilter::filter(self, request))
    }
}

trait ErasedResultFilter: Send + Sync {
    fn filter<'a>(
        &'a self,
        request: &'a QueryRequest,
        results: &'a mut Vec<QueryResult>,
    ) -> Pin<Box<dyn Future<Output = Result<ResultFilterAction>> + Send + 'a>>;
}

impl<T: ResultFilter> ErasedResultFilter for T {
    fn filter<'a>(
        &'a self,
        request: &'a QueryRequest,
        results: &'a mut Vec<QueryResult>,
    ) -> Pin<Box<dyn Future<Output = Result<ResultFilterAction>> + Send + 'a>> {
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

    pub(crate) async fn apply(
        &self,
        request: &mut QueryRequest,
    ) -> Result<Option<Vec<QueryResult>>> {
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

    pub(crate) async fn apply(
        &self,
        request: &QueryRequest,
        results: &mut Vec<QueryResult>,
    ) -> Result<()> {
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
