# SQL endpoints

Each clone of an `Engine` shares the database and can create independent MySQL/MariaDB endpoints. Creating an endpoint binds its socket before returning.

```rust,no_run
use my_sqweel::{Engine, EngineConfig, Storage, SqlEndpointConfig};
use my_sqweel::server::Authentication;
use my_sqweel::sql::engine::{AuthScope, AuthPrivilege};

fn main() -> anyhow::Result<()> {
    let engine = Engine::open(EngineConfig::default(), Storage::RocksDb("./data".into()))?;
    let mut reports = engine.spawn_sql(
        SqlEndpointConfig::new("127.0.0.1:3307".parse()?, Authentication::EngineAccounts)
            .with_scopes([AuthScope::table("app", "reports", [AuthPrivilege::Select])])
    )?;
    println!("SQL listening on {}", reports.local_addr());
    // Keep the handle alive while clients use this endpoint.
    reports.shutdown()?;
    Ok(())
}
```

Use `spawn_sql_async` and `shutdown_async` from Tokio tasks. SQL endpoint creation never starts HTTP. The CLI explicitly starts its debug/search HTTP server; embedded applications can explicitly call `server::spawn_debug_http`.

## Wire authentication and scopes

Every endpoint configuration requires an authentication policy:

- `Authentication::EngineAccounts` verifies accounts created in the SQL catalog.
- `Authentication::static_users([StaticUser::new(user, password, scopes)])` verifies configured credentials.
- `Authentication::callback(authenticator)` invokes an `AsyncAuthenticator`.
- `Authentication::AllowAll` explicitly accepts any supplied credentials.

An authenticator returns an `AuthenticatedUser` with grants. The endpoint's `scopes` are a ceiling: effective access is the intersection of those grants and the ceiling, including for catalog administrators and `AllowAll`. An empty ceiling denies access. Select the initial database with `with_default_database`; the handshake database is also checked.

`AuthScope::database(name, privileges)` covers a database. `AuthScope::table(database, table, privileges)` covers one table. Privileges are `Select`, `Insert`, `Update`, `Delete`, `Create`, `Alter`, `Drop`, `Index`, and `References`. `AuthScope::All` includes global administration. Scoped database creation/deletion requires database-wide `Create`/`Drop`; table grants cannot authorize deleting a database. Account administration requires unrestricted administrative access. Global settings remain unsupported.

Authorization checks SQL before query filters and again after rewrites. It covers prepared executions, views, metadata and indirect row changes. Tables outside the effective scope are omitted from metadata before aggregation. Cross-database statements remain unsupported; use a session's selected database.

Non-loopback binds require `allow_remote = true`. Callbacks receive MySQL native-password challenge/response data, not plaintext passwords; `verify_mysql_native_password` can validate a supplied secret.

## Endpoint lifetime

Keep each `SqlEndpoint` handle alive. `shutdown`/`shutdown_async` stop accepting, disconnect clients and wait for session cleanup, including rollback of uncommitted transactions. Pending authentication and filter futures are cancelled. A statement already executing, including its storage commit, may finish before shutdown completes.

Dropping a handle closes its port and disconnects clients without waiting for arbitrary storage callbacks. Other endpoints and embedded sessions remain usable. Dropping an Engine clone does not destroy endpoints that still own the shared database.

## Callback account operations

`AsyncAuthenticator::account_operation` may return `Continue` to use the catalog or `Handled` after processing an account operation externally. These operations still require unrestricted administrative access under the endpoint ceiling.
