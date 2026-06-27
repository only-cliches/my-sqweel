# MySqweel

> The development database for when your app is still becoming itself.

MySqweel is a **schema-drift-tolerant, development-only MySQL facade** built for fast local iteration.

It speaks enough MySQL for real application code to connect to it, but it does not force your local workflow to behave like production. Tables can be created from select statements, inferred from inserts, extended from seed payloads, inspected through MySQL metadata, searched through a Meilisearch-shaped HTTP API, snapshotted, restored, reset, and intentionally broken for resilience testing.

## Why this exists

Local development often has two bad options:

1. Run the full production-shaped stack locally and pay the complexity tax every day.
2. Mock everything and discover integration problems too late.

MySqweel is a third lane.

It lets your app speak a real protocol, run real SQL-ish flows, and exercise real data access paths while still giving you the softness you want during product development.

That softness matters.

When you are building early, the schema is not sacred. The seed data is not sacred. The search index is not sacred. The whole thing should be resettable, inspectable, scriptable, and forgiving.

MySqweel is built around that belief.

## When MySqweel is a great fit

Use MySqweel when you are:

* building a new app before the schema has settled
* prototyping with MySQL-compatible tooling
* working on ORMs, query builders, migrations, or seed scripts
* building UI flows that need realistic data
* developing search experiences locally
* writing integration tests that need a disposable database
* testing how your app handles slow or failing queries
* teaching, demoing, or experimenting

## When MySqweel is the wrong fit

Do not use MySqweel when you need:

* production durability
* ACID correctness
* real MySQL compatibility across the full grammar
* replication
* permissions
* user management
* query planning
* production-grade indexing
* production search relevance
* secure multi-tenant isolation
* compliance guarantees

Use real MySQL, MariaDB, Postgres, or your actual production database when correctness, durability, security, scale, and operational guarantees matter.

## Install

From source:

```sh
git clone https://github.com/only-cliches/my-sqweel.git
cd my-sqweel
cargo install --path .
```

Or run directly:

```sh
cargo run --bin sqwl -- serve
```

The installed binary is:

```sh
sqwl
```

## Start the server

```sh
sqwl serve
```

By default, MySqweel listens on:

```text
MySQL wire:  127.0.0.1:3307
Debug HTTP:  127.0.0.1:3407
```

Connect with a MySQL client:

```sh
mysql --protocol TCP -h 127.0.0.1 -P 3307 -u root app
```

Then use normal SQL:

```sql
CREATE TABLE users (
  id BIGINT PRIMARY KEY AUTO_INCREMENT,
  email VARCHAR(255),
  display_name TEXT,
  UNIQUE(email)
);

INSERT INTO users (email, display_name)
VALUES
  ('ada@example.com', 'Ada'),
  ('grace@example.com', 'Grace');

SELECT id, email, display_name
FROM users
ORDER BY id;
```

## Use it behind your app

Point your local app at MySqweel:

```sh
export DATABASE_URL="mysql://root@127.0.0.1:3307/app"
```

Then start your app as usual.

That is the core trick. Your code gets a MySQL-shaped database. Your local workflow gets a forgiving development engine.

## Durable local mode

By default, MySqweel uses in-memory embedded storage.

For a local persistent database:

```sh
sqwl --data-dir .my-sqweel/data serve
```

Persistent mode uses embedded Lux storage and protects the data directory with a lock file so the same directory is not opened twice.

## Serve plus REPL

Run the MySQL server and an interactive maintenance shell at the same time:

```sh
sqwl serve --repl
```

Inside the REPL:

```text
sqwl> status
sqwl> drift report
sqwl> snapshot save before-auth-refactor
sqwl> sql SELECT * FROM users ORDER BY id
sqwl> reset users
sqwl> snapshot restore before-auth-refactor
sqwl> quit
```

## CLI

```text
sqwl [options] serve [--repl]
sqwl [options] repl
sqwl explain <sql>
sqwl help
```

Options:

| Option                   | Purpose                                                   |
| ------------------------ | --------------------------------------------------------- |
| `--bind <addr>`          | MySQL bind address. Default: `127.0.0.1:3307`.            |
| `--data-dir <dir>`       | Enable durable embedded storage in a local directory.     |
| `--allow-remote`         | Permit non-loopback bind addresses. Use carefully.        |
| `--unique-mode <mode>`   | `overwrite` or `enforce`. Default: `overwrite`.           |
| `--debug-bind <addr>`    | Debug HTTP bind address. Default: MySQL port plus 100.    |
| `--query-delay-ms <n>`   | Add fixed latency to every SQL statement.                 |
| `--fail-read-every <n>`  | Fail every Nth read statement.                            |
| `--fail-write-every <n>` | Fail every Nth write statement.                           |
| `--snapshot-dir <path>`  | REPL snapshot directory. Default: `.my-sqweel/snapshots`. |
| `--log-filter <filter>`  | Tracing filter. Default: `my_sqweel=info`.                |

## Explain SQL without running it

```sh
sqwl explain "SELECT id, email FROM users WHERE email = 'ada@example.com'"
```

Example output:

```json
{
  "count": 1,
  "statements": [
    {
      "kind": "query",
      "tables": ["users"],
      "normalized": "SELECT id, email FROM users WHERE email = 'ada@example.com'"
    }
  ]
}
```

## Schema drift is a feature

MySqweel keeps schema hints, but it does not panic just because your local data is messy.

It can tell you where reality and declared schema disagree:

```sh
curl http://127.0.0.1:3407/_drift/report
```

You can also ask the REPL:

```text
sqwl> drift check
sqwl> drift report
```

A drift report includes:

* known tables
* row counts
* declared schema columns
* columns missing from rows
* extra row fields not present in schema
* duplicate values for unique constraints

This is useful when you are rapidly changing DTOs, migrations, seed data, fixtures, or product assumptions.

## Seed JSON directly

Seed a table through the debug HTTP API:

```sh
curl -X POST http://127.0.0.1:3407/_drift/tables/users/seed \
  -H 'content-type: application/json' \
  -d '{
    "mode": "replace",
    "rows": [
      {
        "email": "ada@example.com",
        "display_name": "Ada Lovelace",
        "role": "admin"
      },
      {
        "email": "grace@example.com",
        "display_name": "Grace Hopper",
        "role": "engineer"
      }
    ]
  }'
```

If the table or columns do not exist yet, MySqweel can infer the shape from the seed payload.

## Snapshots

Take a snapshot through HTTP:

```sh
curl -X POST http://127.0.0.1:3407/_drift/snapshot
```

Or use the REPL:

```text
sqwl> snapshot save clean-demo
sqwl> snapshot restore clean-demo
sqwl> snapshot list
```

Snapshots make local workflows cheap:

```text
seed fixture
run app
break everything
restore fixture
try again
```

## Meilisearch-shaped local search

MySqweel also exposes a local search API shaped like Meilisearch.

Create an index:

```sh
curl -X POST http://127.0.0.1:3407/indexes \
  -H 'content-type: application/json' \
  -d '{ "uid": "books", "primaryKey": "id" }'
```

Add documents:

```sh
curl -X POST http://127.0.0.1:3407/indexes/books/documents \
  -H 'content-type: application/json' \
  -d '{
    "documents": [
      {
        "id": "1",
        "title": "Dune",
        "genre": "sci-fi",
        "rating": 10,
        "description": "Desert planet politics, spice, prophecy, and power."
      },
      {
        "id": "2",
        "title": "Foundation",
        "genre": "sci-fi",
        "rating": 8,
        "description": "Mathematics, empire, and a long plan."
      }
    ]
  }'
```

Search:

```sh
curl -X POST http://127.0.0.1:3407/indexes/books/search \
  -H 'content-type: application/json' \
  -d '{
    "q": "desert spice",
    "filter": "genre = \"sci-fi\"",
    "sort": ["rating:desc"],
    "attributesToRetrieve": ["id", "title", "rating"],
    "showRankingScore": true
  }'
```

Supported development workflows include:

* index creation
* document add, patch, replace, delete
* search
* filters
* sorting
* facets
* facet search
* multi-search
* task responses
* settings
* stats
* dumps
* webhooks
* API key stubs

This is not a full Meilisearch replacement. It is a local compatibility surface for building and testing app behavior without running another service.

## Vector search

Declare a vector column:

```sql
CREATE TABLE books (
  id TEXT PRIMARY KEY,
  title TEXT,
  embedding VECTOR(3)
);
```

Insert vector data:

```sh
curl -X POST http://127.0.0.1:3407/indexes/books/documents \
  -H 'content-type: application/json' \
  -d '{
    "documents": [
      { "id": "1", "title": "Dune", "embedding": [0.9, 0.1, 0.2] },
      { "id": "2", "title": "Foundation", "embedding": [0.1, 0.9, 0.3] }
    ]
  }'
```

Search by vector:

```sh
curl -X POST http://127.0.0.1:3407/indexes/books/search \
  -H 'content-type: application/json' \
  -d '{
    "vector": [0.95, 0.05, 0.2],
    "vectorField": "embedding",
    "showRankingScore": true
  }'
```

MySqweel uses cosine similarity for local vector ranking.

## SQL support

MySqweel is not a complete MySQL implementation, but it supports a practical subset for development.

### DDL and metadata

* `CREATE TABLE`
* `ALTER TABLE ADD COLUMN`
* metadata capture for primary keys, unique keys, indexes, and foreign keys
* `CREATE INDEX`
* `DROP TABLE`
* `DROP INDEX`
* `TRUNCATE TABLE`
* `RENAME TABLE`
* `SHOW TABLES`
* `SHOW COLUMNS`
* `SHOW INDEX`
* `SHOW CREATE TABLE`
* `DESCRIBE`
* common `information_schema` views

### Writes

* `INSERT ... VALUES`
* `INSERT ... SELECT`
* `INSERT IGNORE`
* `REPLACE`
* `ON DUPLICATE KEY UPDATE`
* `UPDATE`
* `DELETE`
* `RETURNING`
* auto-increment primary keys
* defaults
* basic type coercion
* unique constraint handling

### Reads

* `SELECT`
* `WHERE`
* `ORDER BY`
* `LIMIT`
* `OFFSET`
* `GROUP BY`
* aggregate functions
* scalar functions
* `INNER JOIN`
* `LEFT JOIN`
* implicit cross joins
* derived tables
* scalar subqueries
* `EXISTS`
* `IN`
* `UNION`

### Compatibility helpers

* prepared statements
* `LAST_INSERT_ID()`
* `DATABASE()`
* `SCHEMA()`
* common session variables
* MySQL-ish system metadata
* charset and collation metadata stubs

## Unique constraint modes

By default, MySqweel uses `overwrite` mode:

```sh
sqwl --unique-mode overwrite serve
```

That means incoming rows can replace conflicting rows. This is convenient for local iteration and seed workflows.

For stricter behavior:

```sh
sqwl --unique-mode enforce serve
```

In `enforce` mode, unique constraint violations return errors.

## Failure injection

Test your app against database pain without setting up chaos infrastructure:

```sh
sqwl --query-delay-ms 250 serve
```

Fail every third read:

```sh
sqwl --fail-read-every 3 serve
```

Fail every fifth write:

```sh
sqwl --fail-write-every 5 serve
```

Combine them:

```sh
sqwl \
  --query-delay-ms 100 \
  --fail-read-every 10 \
  --fail-write-every 7 \
  serve
```

This is useful for checking retry behavior, loading states, error handling, idempotency, and unhappy-path UX.

## Debug HTTP endpoints

| Endpoint                           | Purpose                        |
| ---------------------------------- | ------------------------------ |
| `GET /health`                      | Basic health check.            |
| `GET /version`                     | Local version payload.         |
| `GET /_drift/health`               | Drift API health check.        |
| `GET /_drift/report`               | Schema drift report.           |
| `GET /_drift/tables`               | List known tables.             |
| `GET /_drift/tables/{table}/rows`  | Inspect table rows.            |
| `POST /_drift/tables/{table}/seed` | Seed JSON rows into a table.   |
| `POST /_drift/snapshot`            | Return a full engine snapshot. |
| `POST /_drift/restore`             | Restore a snapshot.            |
| `GET /indexes`                     | List search indexes.           |
| `POST /indexes`                    | Create a search index.         |
| `POST /indexes/{uid}/search`       | Search documents.              |
| `POST /multi-search`               | Run multiple searches.         |
| `GET /tasks`                       | List task-style responses.     |
| `GET /stats`                       | Instance stats.                |

There are additional Meilisearch-shaped routes for settings, documents, dumps, webhooks, keys, and per-index stats.

## Development

Run tests:

```sh
cargo test
```

Format:

```sh
cargo fmt
```

Lint:

```sh
cargo clippy --all-targets --all-features
```

Run locally with logs:

```sh
RUST_LOG=my_sqweel=debug cargo run --bin sqwl -- serve --repl
```

## Project layout

```text
src/bin/sqwl.rs              CLI entrypoint
src/lib.rs                   CLI parsing, REPL, help, explain
src/server/mysql_wire.rs     MySQL wire protocol server
src/server/debug_http.rs     Drift API and Meilisearch-shaped HTTP API
src/sql/mod.rs               MySQL dialect parsing
src/sql/engine/              SQL execution engine
src/schema/mod.rs            Schema hint model
src/model.rs                 Stored row model
src/storage/mod.rs           Embedded Lux-backed Redis-like storage layer
```

## Design principles

### 1. Speak the protocol apps already use

The app should not need a fake adapter just because the database is local.

### 2. Make drift observable, not fatal

Early product development is messy. Local tooling should show the mess clearly without blocking every experiment.

### 3. Prefer resettable over precious

Local state should be easy to seed, snapshot, restore, and delete.

### 4. Be useful before being complete

MySqweel does not need to be all of MySQL to unlock real local workflows.

### 5. Stay honest

The production warning is not legal decoration. It is the contract.

## License

MIT
