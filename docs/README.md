# MySqweel library guide

One cloneable `Engine` supports sync and async queries, memory or RocksDB storage, custom async storage, and independently scoped SQL endpoints.

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

- `my_sqweel::{Engine, EngineSession, Storage}`: shared embedded database and sessions.
- `my_sqweel::{SqlEndpoint, SqlEndpointConfig}`: owned SQL listeners.
- `my_sqweel::server`: authentication and explicit debug/search HTTP APIs.
- `my_sqweel::model::StoredRow` and `my_sqweel::schema::*`: serializable row
  and schema types used in snapshots and state images.

The root [README](../README.md) lists supported SQL surfaces, CLI commands,
the debug/search HTTP API, and the MariaDB verification matrix.
