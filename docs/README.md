# MySqweel library guide

MySqweel is an embeddable MySQL/MariaDB-compatible development database. Use
the synchronous `Engine` for in-process work and the built-in MariaDB wire
server when existing clients need to connect. Use `AsyncEngine` when storage
belongs behind an async boundary or when execution filters are needed.

## Choose an entry point

| Need | Start here |
| --- | --- |
| Execute SQL directly in a Rust process | [Embedding and transactions](embedding.md) |
| Keep local state in a directory | [Persistence and maintenance](operations.md) |
| Run migrations or an ORM through MariaDB protocol | [Server and wire protocol](server.md) |
| Use the local document, facet, or vector search API | [Search and debug HTTP](search.md) |
| Store state in CSV, HTTP, or another application system | [Custom async storage](async-storage.md) |
| Allow, reject, rewrite, or synthesize query results | [Execution filters](filters.md) |
| Understand strict schemas and MariaDB compatibility limits | [Compatibility and limits](compatibility.md) |

## Public API map

- `my_sqweel::sql::engine::Engine`: synchronous embedded engine and sessions.
- `my_sqweel::AsyncEngine<S>`: asynchronous engine using one `AsyncStorage`
  backend.
- `my_sqweel::storage::LuxStorage`: the bundled async backend.
- `my_sqweel::server`: MariaDB wire server and debug/search HTTP server.
- `my_sqweel::model::StoredRow` and `my_sqweel::schema::*`: serializable row
  and schema types used in snapshots and state images.

The root [README](../README.md) lists supported SQL surfaces, CLI commands,
the debug/search HTTP API, and the MariaDB verification matrix.
