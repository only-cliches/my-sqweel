# Strict schemas and compatibility limits

Every `EngineConfig` uses strict schema behavior. Tables and columns must be
declared before writes, values must satisfy their declared types and
constraints, and unique and foreign-key conflicts are rejected.

`EngineConfig::mysql_strict()` remains as a compatibility alias for
`EngineConfig::default()` so existing callers can upgrade without changing
their constructors. It does not select a separate mode.

The former `compatibility_profile` and `unique_mode` configuration fields and
the `--mysql-strict` and `--unique-mode` CLI options have been removed.

## Appropriate uses

- embedded development databases
- local integration, QA, and fixture environments
- migration and ORM development through the wire protocol
- deterministic error, retry, seed, and reset workflows
- local document, facet, text, and vector-search experiments

## Use MariaDB instead when you require

- fine-grained locking or high write concurrency
- full MySQL/MariaDB permission semantics and security boundaries
- replication, clustering, production backups, or availability guarantees
- optimizer fidelity at production data scale
- a MariaDB feature not listed in the root README's compatibility matrix

The root [README](../README.md#mariadb-compatibility) is the authoritative
feature and differential-verification matrix. Pair it with integration tests
for the SQL and driver behaviors your application depends on.
