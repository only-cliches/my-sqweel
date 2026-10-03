# Execution filters

`Engine` exposes two mutable, ordered pipelines:

- `engine.query_filters()` runs after initial authorization and before SQL execution.
- `engine.result_filters()` runs after successful execution, before the caller
  receives results.

Filters are native async traits. `QueryFilters` and `ResultFilters` erase the
implementation type only at the pipeline boundary, so an application can add
different filter types in one ordered chain.

## Query filters

A query filter receives a mutable `QueryRequest` and returns one action:

| Action | Effect |
| --- | --- |
| `Continue` | Continue with the request, including any changes made to `request.sql`. |
| `Reject(message)` | Do not execute SQL; return an error. |
| `Return(results)` | Do not execute SQL; return synthetic results. |

```rust
use anyhow::Result;
use my_sqweel::{QueryFilter, QueryFilterAction, QueryRequest};

struct TenantRewrite;

impl QueryFilter for TenantRewrite {
    async fn filter(&self, request: &mut QueryRequest) -> Result<QueryFilterAction> {
        if request.sql == "SELECT current_tenant" {
            request.sql = "SELECT 'acme' AS tenant".into();
        }
        Ok(QueryFilterAction::Continue)
    }
}
```

Parameterized calls bind their values before the filter runs. `request.sql` is
therefore the exact SQL MySqweel will execute; `request.parameters` retains the
input values for logging or policy decisions. Prepared callers are marked with
`request.prepared`.

## Result filters

Result filters can mutate the returned vector or replace/reject it. Query
effects have already occurred when a result filter runs. A rejected result
blocks it from the caller but does not roll back a successful SQL statement.

```rust
use anyhow::Result;
use my_sqweel::{ResultFilter, ResultFilterAction};
use my_sqweel::sql::engine::QueryResult;

struct LimitRows;

impl ResultFilter for LimitRows {
    async fn filter(
        &self,
        _request: &my_sqweel::QueryRequest,
        results: &mut Vec<QueryResult>,
    ) -> Result<ResultFilterAction> {
        for result in results {
            result.rows.truncate(100);
        }
        Ok(ResultFilterAction::Continue)
    }
}
```

Synthetic results from a query filter also pass through the result pipeline.

## Order and mutation

Filters run in insertion order. Calling `push` adds the next filter; `clear`
removes the chain. The pipeline copies its current ordered filter list before
awaiting a filter, so application code can update the pipeline without holding
an internal lock across an await.

```rust,no_run
# use my_sqweel::{Engine, QueryFilter, ResultFilter};
# struct Audit; impl QueryFilter for Audit { async fn filter(&self, _: &mut my_sqweel::QueryRequest) -> anyhow::Result<my_sqweel::QueryFilterAction> { Ok(my_sqweel::QueryFilterAction::Continue) } }
# struct Redact; impl ResultFilter for Redact { async fn filter(&self, _: &my_sqweel::QueryRequest, _: &mut Vec<my_sqweel::sql::engine::QueryResult>) -> anyhow::Result<my_sqweel::ResultFilterAction> { Ok(my_sqweel::ResultFilterAction::Continue) } }
# fn configure(db: &Engine) {
db.query_filters().push(Audit);
db.result_filters().push(Redact);
# }
```

## Scope

The same filters run for synchronous calls, asynchronous calls, and every SQL endpoint. `request.context()` exposes an immutable session ID, current database, authenticated username and optional endpoint ID. Rewritten SQL is authorized again before execution; synthetic results require the original request to be authorized.

Filters are trusted application code. They run on the engine's callback runtime. Calling database operations from inside a query, result or storage callback is rejected to prevent reentrant deadlocks. Query-hook subscribers run separately after publication and may query the engine.

Read hooks see successfully filtered results. Result-filter rejection cannot undo an already committed write.
