# Server, wire protocol, and local HTTP surfaces

## MariaDB-compatible wire server

`server::run` opens the built-in MySQL/MariaDB-compatible server. The default
bind is `127.0.0.1:3307` and the default database is `app`. It accepts all wire
connections unless an authentication policy is configured.

```rust,no_run
use my_sqweel::server::{self, ServerConfig};

fn main() -> anyhow::Result<()> {
    server::run(ServerConfig::default())
}
```

To embed an already configured engine, use `open_engine`, `run_with_engine`,
or `spawn_with_engine`. `ServerHandle` stops the listener and debug server when
dropped.

```rust,no_run
use std::sync::Arc;
use my_sqweel::server::{self, ServerConfig};
use my_sqweel::sql::engine::Engine;

let engine = Arc::new(Engine::default());
let handle = server::spawn_with_engine(ServerConfig::default(), engine)?;
# drop(handle);
# Ok::<(), anyhow::Error>(())
```

`ServerConfig::validate` rejects non-loopback addresses unless `allow_remote`
is set. Treat that override as a deliberate development decision.

## Client setup

Use a normal MySQL/MariaDB driver with:

```text
host: 127.0.0.1
port: 3307
database: app
username: root
password: (any value with the default permissive policy)
```

The wire protocol is intended for applications, ORMs, migration tools, and
local integration tests. Check [compatibility and limits](compatibility.md)
before relying on a specific MariaDB behavior.

## Wire authentication and scopes

The default wire policy is `Authentication::AllowAll`, intended for local
fixtures. It accepts any MySQL username/password handshake and grants full
scope. Configure an explicit policy before exposing a server to other users.

Static users authenticate with the standard MySQL native-password exchange.
Their scopes are enforced by the same catalog checks used for SQL-created
accounts. `AuthScope::All` includes database administration; a database scope
permits only the listed DML operations.

```rust,no_run
use my_sqweel::server::{Authentication, ServerConfig, StaticUser};
use my_sqweel::sql::engine::{AuthPrivilege, AuthScope};

let config = ServerConfig {
    authentication: Authentication::static_users([StaticUser::new(
        "reporter",
        "correct-horse-battery-staple",
        [AuthScope::database("app", [AuthPrivilege::Select])],
    )]),
    ..ServerConfig::default()
};
```

Use `Authentication::EngineAccounts` to authenticate SQL-created accounts
such as `root` and users created with `CREATE USER` / `GRANT`.

For an external identity service, implement `AsyncAuthenticator`. The callback
receives the username, MySQL challenge/response, and requested database; it
returns `None` to deny the connection or an `AuthenticatedUser` with scopes.
The client password is not sent in clear text. Use
`verify_mysql_native_password` when the external service stores a password or
delegate verification to the service that owns the password hash.

```rust,no_run
use my_sqweel::server::{
    AsyncAuthenticator, AuthenticatedUser, Authentication, AuthenticationRequest,
    verify_mysql_native_password,
};
use my_sqweel::sql::engine::{AuthPrivilege, AuthScope};

struct Directory;

impl AsyncAuthenticator for Directory {
    async fn authenticate(
        &self,
        request: AuthenticationRequest,
    ) -> anyhow::Result<Option<AuthenticatedUser>> {
        if request.username == "analyst"
            && verify_mysql_native_password("directory-secret", &request.challenge, &request.response)
        {
            return Ok(Some(AuthenticatedUser::new(
                "analyst",
                [AuthScope::database("app", [AuthPrivilege::Select])],
            )));
        }
        Ok(None)
    }
}

let authentication = Authentication::callback(Directory);
```

An external authenticator can also own account-management SQL. Override the
optional `account_operation` method and return `Handled` after committing the
operation in the external system. MySqweel then returns a successful empty
result without changing its local account catalog. Returning `Continue` keeps
the built-in catalog behavior. The callback receives `CREATE USER`, `ALTER
USER`, `DROP USER`, `RENAME USER`, `SET PASSWORD`, `GRANT`, and `REVOKE` as an
`AccountOperation` with its kind and original SQL. Only an `AuthScope::All`
principal can submit these operations.

```rust,no_run
use my_sqweel::server::{
    AccountOperation, AccountOperationAction, AsyncAuthenticator,
    AuthenticatedUser, AuthenticationRequest,
};

struct ExternalDirectory;

impl AsyncAuthenticator for ExternalDirectory {
    async fn authenticate(
        &self,
        _request: AuthenticationRequest,
    ) -> anyhow::Result<Option<AuthenticatedUser>> {
        // Authentication implementation omitted.
        Ok(None)
    }

    async fn account_operation(
        &self,
        operation: AccountOperation,
    ) -> anyhow::Result<AccountOperationAction> {
        // Send operation.kind and operation.sql to the directory service.
        let _ = operation;
        Ok(AccountOperationAction::Handled)
    }
}
```

## Debug and search HTTP server

The server starts its debug/search endpoint at port `wire_port + 100` by
default, so `3407` for the default wire port. Override it with
`ServerConfig::debug_addr`. This local API provides health, schema drift,
snapshots, document ingestion, text/facet/vector search, and task-like
responses for development workflows.

The endpoint map and request examples are maintained in the root
[README](../README.md#http-endpoint-map). Keep it on loopback unless
`allow_remote` is explicitly set.

## CLI

The `sqwl` binary wraps the same server and engine:

```sh
sqwl serve
sqwl repl
sqwl explain "SELECT * FROM users"
```

See `sqwl help` and the root README for durable state, snapshots, maintenance,
and failure-injection flags.
