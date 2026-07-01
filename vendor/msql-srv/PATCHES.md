# Patches for msql-srv

This vendored copy of `msql-srv` is based on `msql-srv 0.11.0` from
crates.io, with small compatibility patches.

## Why this is vendored

MySqweel depends on `msql-srv` for the MySQL wire protocol implementation.

The published `msql-srv 0.11.0` crate does not handle a few protocol commands
that modern MySQL clients and pools can send during connection setup, connection
reuse, or prepared statement cleanup. It also assumes blocking socket behavior
in packet reads and writes.

Those gaps can cause intermittent development-server failures even when the
database itself is healthy. Keeping these patches local makes the embedded dev
runtime more tolerant of real MySQL client behavior.

## Source changes

### `src/commands.rs`

Adds parsing for additional MySQL command bytes:

- `COM_RESET_CONNECTION`
- `COM_STMT_RESET`
- `COM_SET_OPTION`
- `COM_CHANGE_USER`

These commands are used by clients and pools for connection lifecycle and
prepared statement management.

### `src/lib.rs`

Adds handling for the new command variants:

- `COM_STMT_RESET` clears accumulated long-data state for the prepared
  statement, then returns an OK packet.
- `COM_RESET_CONNECTION` returns an OK packet.
- `COM_SET_OPTION` returns an OK packet.
- `COM_CHANGE_USER` returns an OK packet.

This prevents otherwise-valid client commands from being treated as unsupported
wire protocol input.

The session loop also treats unparseable command packets as a closed session
instead of panicking in the connection handler thread.

### `src/packet.rs`

Hardens packet IO for nonblocking streams:

- Retries `flush` when it returns `WouldBlock`.
- Replaces `write_all` with an explicit write loop that retries `WouldBlock`.
- Retries packet reads when they return `WouldBlock`.

This avoids transient IO failures when the embedded runtime is used with stream
types that can report nonblocking readiness behavior.
