# Search and debug HTTP

The debug HTTP listener is a trusted local administrative API. It exposes
schema-drift inspection and a Meilisearch-shaped document/search surface over
the default `app` database. SQL tables remain the source of truth; search
indexes are derived from committed table state.

Start the server with the default debug listener at `127.0.0.1:3407`:

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

The local surface includes index and document CRUD, filtering, sorting, facets,
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

Do not expose this listener publicly without an application-owned security
boundary. The SQL account and grant system does not protect these HTTP routes.

