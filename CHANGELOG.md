# Changelog

All notable changes to MySqweel will be documented in this file.

## 0.2.0 - Future

- Fixed primary key metadata reporting so `information_schema.key_column_usage` and related introspection stay consistent after `ALTER TABLE` operations.
- Fixed an issue with spawning connection sessions.
- Added more information_schema coverage to the backend with associated tests.

## 0.1.0 - May 31, 2026

### Added

- Added the `sqwl` binary with `serve`, `serve --repl`, `repl`, and `explain` commands.
- Added a lightweight maintenance REPL for status, drift reports, snapshots, index rebuilds, resets, SQL execution, help, and graceful `Ctrl+C` / `Ctrl+D` exit.
- Added MySQL wire-protocol support for local `mysql2`, Drizzle, and migration workflows.
- Added permissive schema behavior:
  - inserts can create missing tables and columns
  - repeated `CREATE TABLE` statements merge into existing metadata
  - reads return rows shaped to the latest known schema
  - stored rows are not rewritten just because the schema changed
- Added support for schema metadata from `CREATE TABLE`, `ALTER TABLE`, indexes, unique constraints, and advisory foreign keys.
- Added dynamic row materialization against the latest schema metadata.
- Added positional inserts that infer generated `column_1`, `column_2`, etc. columns when needed.
- Added configurable duplicate handling with `--unique-mode overwrite|enforce`.
- Added Lux-backed directory persistence with exclusive data-directory locking.
- Added debug HTTP endpoints for health, drift reporting, table inspection, snapshots, restore, and JSON table seeding.
- Added fault-injection flags for query delay and intermittent read/write failures.
- Added broader query coverage, including joins, grouping, aggregates, scalar expressions, common functions, simple derived tables, and uncorrelated subqueries.
- Added best-effort `information_schema` and MySQL metadata command support.
- Added compatibility smoke tests for `mysql2`, Drizzle, and real-MySQL parity checks.
- Added expanded MySQL parity coverage for functions, NULL predicates, prepared writes, defaults, arithmetic updates, and deletes.
- Added project logo usage in the README.
- Added focused SQL engine submodules under `src/sql/engine/`.

### Notes

- MySqweel remains development-only and is not intended to provide production database correctness, ACID guarantees, or full MySQL compatibility.
