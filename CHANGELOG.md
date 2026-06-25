# Changelog

All notable changes to MySqweel will be documented in this file.

## 0.2.0 - Jun 25, 2026

- Added an always-on Meilisearch-compatible HTTP API on the debug HTTP port.
  - Meilisearch indexes map to MySqweel/MySQL tables.
  - Meilisearch documents map to stored rows.
  - The MySQL/table engine remains the source of truth.
- Added synchronous Tantivy-backed text search for the Meilisearch-compatible API.
  - Document and table mutations rebuild the derived search index before reporting task success.
  - Search falls back to row-scan compatibility for edge cases where Tantivy produces no candidates.
- Added Meilisearch-compatible index, document, search, multi-search, settings, task, key, stats, and swap-index endpoints.
- Added support for Meilisearch search options including filters, sort, pagination, `attributesToRetrieve`, `attributesToSearchOn`, `showRankingScore`, and `showRankingScoreDetails`.
- Added facet support for Meilisearch search responses, including `facetDistribution`, numeric `facetStats`, array facet values, and `facets: ["*"]`.
- Added a 90/10 Meilisearch compatibility pass for previously missing feature areas:
  - query-time synonym and typo-tolerance fallback matching
  - highlighting, cropping, and match-position metadata in search hits
  - `POST /indexes/:uid/facet-search`
  - synchronous in-memory dump status/download endpoints
  - webhook CRUD compatibility endpoints
  - permissive bearer/API-key handling for tenant-token-shaped local client requests
- Added task compatibility improvements:
  - write APIs return Meilisearch-shaped tasks
  - tasks include both `taskUid` and `uid`
  - task durations are serialized as strings
  - task listing supports `uids`, `types`, `statuses`, `indexUids`, ranges, pagination, `from`, and `next`
- Added official Meilisearch JavaScript client compatibility coverage via `tests/node/meili-js-client-compat.mjs` and `cargo test --test meili_js_client`.
- Added optional official Meilisearch Python client compatibility coverage via `tests/python/meili_client_compat.py` and `cargo test --test meili_python_client`.
- Added direct Meilisearch handler coverage for synonyms, typo tolerance, formatting, facet search, dumps, and webhooks.
- Added `npm run test:meili` and `requirements-dev.txt` for running SDK compatibility smoke tests outside Cargo.
- Fixed `sqwl serve` panic caused by nesting Tokio runtimes inside the synchronous server path.
- Fixed Meilisearch filter handling for multi-value `IN` and `NOT IN` expressions.
- Fixed Meilisearch ranking score metadata being stripped by `attributesToRetrieve`.
- Fixed fallback text search so `searchableAttributes` and `attributesToSearchOn` are respected consistently.
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
