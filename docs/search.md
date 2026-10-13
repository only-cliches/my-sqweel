# Search and debug HTTP

The Meilisearch-compatible HTTP endpoint exposes documents and search over
the default `app` database. SQL tables are search indexes, rows are documents,
and columns are document fields. Both the MySQL-compatible TCP endpoint and
the search endpoint read and write the same database. MySqweel uses an internal
Tantivy text search index to emulate supported Meilisearch behavior; SQL rows
remain the source of truth, and the index refreshes when committed rows change.

An embedded application can start one or more search endpoints on its engine:

```rust,no_run
use my_sqweel::{Engine, MeilisearchEndpointConfig};

fn main() -> anyhow::Result<()> {
    let engine = Engine::default();
    engine.execute_sql("CREATE TABLE books (id TEXT PRIMARY KEY, title TEXT)")?;
    let mut search = engine.spawn_meilisearch(
        MeilisearchEndpointConfig::new("127.0.0.1:7700".parse()?),
    )?;
    println!("Search listening on {}", search.local_addr());
    // Keep the handle alive while clients use the endpoint.
    search.shutdown()?;
    Ok(())
}
```

The engine keeps one shared in-memory Tantivy index manager while at least
one search endpoint is active. It releases the indexes after the last endpoint
stops. `spawn_meilisearch_async` and `shutdown_async` are available for Tokio
applications. These routes have no authentication; the default config accepts
only loopback binds, and `allow_remote = true` is required for other addresses.

The CLI's debug HTTP listener adds schema-drift and snapshot operations to the
same search routes. Start it at `127.0.0.1:3407` with:

```sh
sqwl serve
```

## Document search

Create an index, add documents, then search it:

```sh
curl -X POST http://127.0.0.1:3407/indexes \
  -H 'content-type: application/json' \
  -d '{"uid":"books","primaryKey":"id"}'

curl -X POST http://127.0.0.1:3407/indexes/books/documents \
  -H 'content-type: application/json' \
  -d '{"documents":[
    {"id":"1","title":"Dune","genre":"sci-fi","rating":10},
    {"id":"2","title":"Foundation","genre":"sci-fi","rating":8}
  ]}'

curl -X POST http://127.0.0.1:3407/indexes/books/search \
  -H 'content-type: application/json' \
  -d '{"q":"desert","filter":"genre = \"sci-fi\"","sort":["rating:desc"]}'
```

The search surface includes index and document CRUD, filtering, sorting, facets,
facet search, multi-search, settings, stats, dumps, webhooks, keys, and
task-shaped responses. It is useful for development and client integration,
but does not claim production Meilisearch parity. Authentication is permissive
and task execution is local.

## Vector search

Define a `VECTOR(n)` column in SQL, load vectors through the document API, and
query by vector:

```sql
CREATE TABLE books (
  id TEXT PRIMARY KEY,
  title TEXT,
  embedding VECTOR(3)
);
```

```sh
curl -X POST http://127.0.0.1:3407/indexes/books/search \
  -H 'content-type: application/json' \
  -d '{"vector":[0.95,0.05,0.2],"vectorField":"embedding","showRankingScore":true}'
```

Local vector ranking uses cosine similarity.

## Drift and snapshot operations

Use the drift routes to inspect tables and rows, seed JSON, and export or
restore snapshots:

| Endpoint | Purpose |
| --- | --- |
| `GET /health` | Process health |
| `GET /_drift/report` | Schema-drift report |
| `GET /_drift/tables` | Known tables |
| `GET /_drift/tables/{table}/rows` | Inspect stored rows |
| `POST /_drift/tables/{table}/seed` | Seed JSON rows |
| `POST /_drift/snapshot` | Export an app snapshot |
| `POST /_drift/restore` | Restore an app snapshot |

The SQL account and grant system does not protect these HTTP routes. The
`/_drift` routes are available on the CLI debug listener, not on endpoints
started with `Engine::spawn_meilisearch`.

