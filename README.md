<p align="center">
  <img src="logo.png" alt="MySqweel logo" width="280">
</p>

# MySqweel

**A schema-drift-tolerant, development-only MySQL facade for fast local iteration and database-resilience rehearsals.**

MySqweel is a local MySQL-compatible development server that favors momentum over strictness. It tracks schema shape, accepts drift, and stores rows as flexible documents. 

- Inserts can create missing tables and columns
- Repeated `CREATE TABLE` statements merge into existing metadata
- Reads return rows shaped to the latest known schema
- Stored rows are not rewritten just because the schema changed

It is for prototyping, rapid development, and app behavior testing before you settle on the schema you actually trust.

## The Deal

- Point MySQL clients at `127.0.0.1:3307`.
- Use whatever username/password you want. MySqweel is permissive on purpose.
- Run Drizzle or `mysql2` against it during local development.
- Change tables aggressively.
- Keep existing rows even when the schema moves underneath them.
- Inject query latency and intermittent read/write failures to test rough database conditions.
- Validate retry, timeout, loading, and error handling before your users do it for you.
- Use real MySQL before you trust anything important.

That last bullet is not decorative.

## Not Just Drift

Schema drift is the headline act, but MySqweel also helps test how your app behaves when the database path is unpleasant.

Use it to rehearse:

- delayed statements with `--query-delay-ms`
- intermittent read failures with `--fail-read-every`
- intermittent write failures with `--fail-write-every`
- retry, timeout, fallback, loading-state, and error-reporting behavior around database calls

This is not a packet-loss emulator, a real network chaos rig, or a MySQL performance model. The backing store is memory-based and MySqweel does not use MySQL's optimizer, storage engine, locking, indexing, or query execution behavior. Benchmarking against it is wasted effort; use it to make database calls drag their feet, occasionally fail, and reveal whether your app handles that with dignity.

## Hard Boundary

MySqweel is **not production-safe**.

It does not provide:

- ACID guarantees
- real transaction isolation
- full MySQL compatibility
- production-grade authentication or permissions
- correctness guarantees for complex SQL
- meaningful MySQL performance benchmarks
- a replacement for staging, integration tests, or a real database

If the data matters, use MySQL. If the migration is still wearing sweatpants, MySqweel is your friend.

## Why This Exists

Local app development often looks like this:

1. Change a Drizzle schema.
2. Push a migration.
3. Realize the model is wrong.
4. Change it again.
5. Fight drift, stale rows, dropped columns, and migration bookkeeping.

MySqweel optimizes for that messy middle. It accepts common MySQL DDL/DML, records schema hints, stores rows as documents, and lets older rows survive newer schema ideas.

Example drift:

```ts
const users = mysqlTable("users", {
  id: serial("id").primaryKey(),
  email: varchar("email", { length: 255 }).notNull(),
  name: text("name"),
});
```

Later:

```ts
const users = mysqlTable("users", {
  id: serial("id").primaryKey(),
  email: varchar("email", { length: 255 }).notNull(),
  displayName: text("display_name"),
  avatarUrl: text("avatar_url"),
  role: varchar("role", { length: 50 }).default("user"),
});
```

Existing rows do not explode. New columns are added as schema metadata. Historical fields can remain stored even after the current schema stops caring about them.

## Quick Start

Requirements:

- Rust toolchain for the server binary
- Node.js only if you want to run the `mysql2` and Drizzle smoke tests
- Docker only if you want the real-MySQL parity test to start its own `mysql:8`

Run the server:

```bash
cargo run -- serve
```

Or install the local binary:

```bash
cargo install --path .
sqwl serve
```

By default MySqweel listens on:

```txt
mysql://root@127.0.0.1:3307/app
```

Use persistent Lux-backed storage:

```bash
cargo run -- --data-dir ./sqwl-data serve
```

Use another bind address:

```bash
cargo run -- --bind 127.0.0.1:3310 serve
```

## Client Usage

### mysql2

```ts
import mysql from "mysql2/promise";

const connection = await mysql.createConnection({
  uri: "mysql://root@127.0.0.1:3307/app",
});

await connection.query(`
  CREATE TABLE users (
    id BIGINT PRIMARY KEY AUTO_INCREMENT,
    email VARCHAR(255) UNIQUE,
    name TEXT
  )
`);

await connection.execute(
  "INSERT INTO users (email, name) VALUES (?, ?)",
  ["ada@example.com", "Ada"]
);

const [rows] = await connection.query("SELECT * FROM users");
console.log(rows);
```

### Drizzle

```ts
import mysql from "mysql2/promise";
import { drizzle } from "drizzle-orm/mysql2";

const connection = await mysql.createConnection({
  uri: "mysql://root@127.0.0.1:3307/app",
});

export const db = drizzle(connection);
```

Point Drizzle at MySqweel for local development. Point it at real MySQL before shipping.

## What Works

MySqweel currently supports a pragmatic subset of MySQL, focused on local app and ORM workflows.

### Server

- MySQL-compatible TCP server via `msql-srv`
- text protocol queries
- prepared statement handling for common `mysql2` usage
- permissive auth: any username/password, including no password
- selected database tracking via handshake schema and `USE`
- common `SET`, `@@variable`, `DATABASE()`, and `SCHEMA()` compatibility paths

### DDL and Metadata

- `CREATE TABLE`
- `ALTER TABLE` metadata updates:
  - `ADD COLUMN`
  - `DROP COLUMN`
  - `RENAME COLUMN`
  - `CHANGE COLUMN`
  - `MODIFY COLUMN`
  - best-effort unique, index, and foreign-key metadata
- `CREATE INDEX`
- `DROP INDEX`
- `CREATE DATABASE` and `DROP DATABASE` as compatibility no-ops
- `RENAME TABLE`
- `DROP TABLE`
- `TRUNCATE TABLE`
- advisory foreign-key metadata exposed through `SHOW CREATE TABLE` and `information_schema`

### DML

- `INSERT ... VALUES`
- `INSERT IGNORE`
- `REPLACE INTO`
- `INSERT ... ON DUPLICATE KEY UPDATE`
- `SELECT`
- `UPDATE`
- `DELETE`
- `BEGIN`, `START TRANSACTION`, `COMMIT`, and `ROLLBACK` accepted with no-op transaction semantics

### Query Features

- `WHERE` with `=`, `!=`, `>`, `>=`, `<`, `<=`
- `AND`, `OR`, unary `NOT`
- `IN`
- `IS NULL`, `IS NOT NULL`
- best-effort `LIKE`
- common uncorrelated `IN (SELECT ...)` and `EXISTS (SELECT ...)`
- scalar subqueries such as `SELECT (SELECT COUNT(*) FROM t)`
- no-`FROM` scalar selects such as `SELECT 1 + 2`
- arithmetic expressions: `+`, `-`, `*`, `/`, `%`
- basic `CAST(...)`
- common scalar functions including `CONCAT`, `COALESCE`, `IFNULL`, `LOWER`, `UPPER`, `NOW`, `UUID`, `DATABASE`, `VERSION`, and friends
- `ORDER BY`
- `LIMIT` and `OFFSET`
- `GROUP BY`
- `HAVING` for projected/group aggregate values
- aggregates: `COUNT`, `COUNT(DISTINCT ...)`, `SUM`, `AVG`, `MIN`, `MAX`
- simple derived tables
- naive `INNER JOIN` and `LEFT JOIN`
- equality lookup fast path for simple indexed predicates

### Introspection

Best-effort `information_schema` support includes:

- `information_schema.schemata`
- `information_schema.tables`
- `information_schema.columns`
- `information_schema.table_constraints`
- `information_schema.statistics`
- `information_schema.key_column_usage`
- `information_schema.referential_constraints`

Supported MySQL metadata commands include:

- `SHOW DATABASES`
- `SHOW TABLES`
- `SHOW COLUMNS` / `SHOW FIELDS`
- `DESCRIBE` / `DESC`
- `SHOW INDEX`
- `SHOW CREATE TABLE`

## Storage Model

MySqweel uses an embedded Redis like database as the table and row storage backend.

- schema metadata is stored separately from row data
- rows are stored as flexible field maps
- unknown or historical fields are preserved
- existing rows are not rewritten just because a schema changes
- query results are dynamically shaped to the latest table metadata
- raw row data is hydrated into an in-process cache for query execution
- `--data-dir` enables snapshot/WAL-backed directory persistence
- `--data-dir` takes an exclusive process lock on the data directory; a second MySqweel process using the same directory is refused

Primary key behavior is intentionally practical:

- declared primary keys are preferred
- `id` is used as a fallback when present
- UUIDs are used when no usable key can be inferred

## Schema Drift Behavior

DDL is accepted as metadata. Data is not forced to march in lockstep.

- missing fields are allowed
- unknown fields are retained
- inserted and updated values are best-effort coerced to known column types
- inserts into unknown tables infer schema metadata from named columns, or from positional `column_1`, `column_2`, etc. when no column list is provided
- positional inserts into known tables follow the latest schema column order
- selected rows are best-effort shaped to the latest schema without rewriting stored rows
- `NOT NULL` columns without defaults reject missing or null values
- dropped columns stop being part of current schema metadata, but stored row values can remain
- renamed columns update metadata without rewriting every row

This is the whole point. MySqweel lets local data be a little weird while your schema is still becoming a grown-up.

## Configuration

| Flag | Default | Description |
| --- | --- | --- |
| `--bind <addr>` | `127.0.0.1:3307` | MySQL wire server bind address |
| `--data-dir <dir>` | unset | Locked data directory; unset runs in memory |
| `--allow-remote` | `false` | Allow non-loopback binds |
| `--unique-mode <overwrite|enforce>` | `overwrite` | Duplicate unique/primary-key handling |
| `--debug-http` | `false` | Enable debug HTTP endpoints |
| `--debug-bind <addr>` | MySQL port + 100 | Debug HTTP bind address; also enables debug HTTP |
| `--query-delay-ms <n>` | `0` | Add fixed latency to each SQL statement |
| `--fail-read-every <n>` | `0` | Fail every Nth read statement |
| `--fail-write-every <n>` | `0` | Fail every Nth write statement |
| `--snapshot-dir <path>` | `.my-sqweel/snapshots` | Snapshot location for REPL commands |
| `--log-filter <filter>` | `my_sqweel=info` | tracing filter |

Examples:

```bash
sqwl --bind 127.0.0.1:3307 --data-dir ./sqwl-data serve
sqwl --unique-mode enforce serve
sqwl --debug-http --debug-bind 127.0.0.1:3407 serve
sqwl --query-delay-ms 50 --fail-read-every 10 serve
```

Unique mode has two choices:

- `overwrite`: incoming ordinary inserts or updates remove conflicting rows, while explicit `INSERT IGNORE`, `REPLACE`, and `ON DUPLICATE KEY UPDATE` keep their SQL-specific behavior
- `enforce`: duplicate primary-key or unique values error unless the statement explicitly uses an ignore/upsert/replace mode

## CLI

```bash
sqwl serve
sqwl serve --repl
sqwl repl
sqwl explain "<sql>"
```

Maintenance commands run inside `sqwl repl` or `sqwl serve --repl`:

```txt
status
drift report
drift check
snapshot save <name>
snapshot restore <name>
snapshot list
index rebuild [--all|<table>]
reset [table]
explain <sql>
sql <sql>
help
quit
```

`Ctrl+C` and `Ctrl+D` also exit the REPL. In `serve --repl`, exiting the REPL stops the embedded server accept loop before shutdown.

Use `serve --repl` when you want the server and maintenance commands to share one live engine:

```bash
sqwl --data-dir ./sqwl-data serve --repl
```

Use `repl` for offline maintenance against a data directory that is not already open:

```bash
sqwl --data-dir ./sqwl-data repl
```

Useful REPL examples:

```txt
status
drift report
snapshot save before-the-schema-gets-ideas
explain SELECT email FROM users WHERE id = 1
```

## Debug HTTP

Enable debug HTTP:

```bash
sqwl --debug-http serve
```

If MySQL binds to `127.0.0.1:3307`, debug HTTP defaults to `127.0.0.1:3407`.

| Endpoint | Description |
| --- | --- |
| `GET /_drift/health` | health check |
| `GET /_drift/report` | schema drift summary |
| `GET /_drift/tables` | list known tables |
| `GET /_drift/tables/{table}/rows` | inspect stored rows |
| `POST /_drift/tables/{table}/seed` | append or replace rows from JSON |
| `POST /_drift/snapshot` | export an in-memory snapshot |
| `POST /_drift/restore` | restore a snapshot |

Seed rows with an array or a `{ "rows": [...] }` envelope. `mode` defaults to `append`; use `replace` to clear existing rows first.

```bash
curl -X POST http://127.0.0.1:3407/_drift/tables/users/seed \
  -H 'content-type: application/json' \
  -d '{"mode":"replace","rows":[{"email":"a@example.com","score":10}]}'
```

## Testing

Run the Rust tests:

```bash
cargo test
```

Run Node compatibility smoke tests against a live MySqweel server:

```bash
npm install

# Terminal 1
cargo run -- --bind 127.0.0.1:3307 serve

# Terminal 2
npm run test:mysql2
npm run test:drizzle
npm run test:drizzle:compat
```

Use a custom server URL:

```bash
npm run test:mysql2 -- mysql://root@127.0.0.1:3307/app
npm run test:drizzle -- mysql://root@127.0.0.1:3307/app
npm run test:drizzle:compat -- mysql://root@127.0.0.1:3307/app
```

Run parity tests against real MySQL:

```bash
docker pull mysql:8
cargo test --test mysql_parity
```

Or point at an existing MySQL server:

```bash
MYSQL_COMPARE_URL='mysql://root:password@127.0.0.1:3306/test' cargo test --test mysql_parity
```

Other focused test targets:

```bash
cargo test --test compat_matrix
cargo test --test query_engine
cargo test --test schema_metadata
cargo test --test drift_storage
```

## Known Gaps

MySqweel intentionally does not chase the entire MySQL universe.

Not supported or intentionally limited:

- real transaction isolation
- locks
- users and permissions
- stored procedures
- triggers
- views
- replication and binary logs
- full SQL optimizer behavior
- full collation and numeric precision semantics
- correlated subqueries
- joins against derived tables
- window functions
- complex multi-table mutations
- production correctness guarantees

Future useful work:

- seed export workflows
- broader Drizzle-generated SQL compatibility coverage
- more complete metadata and introspection fidelity
- more compatibility fixtures for common app stacks

## License

MIT