#!/usr/bin/env bash
# Start an isolated, disposable MySQL/MariaDB instance for local parity tests.
set -euo pipefail

MYSQL_DATA_DIR="${MYSQL_DATA_DIR:-/tmp/my_sqweel_mysql_test}"
MYSQL_PORT="${MYSQL_TEST_PORT:-3307}"
MYSQL_SOCKET="${MYSQL_TEST_SOCKET:-/tmp/mysql-test.sock}"
MYSQL_PID_FILE="${MYSQL_PID_FILE:-$MYSQL_DATA_DIR/mysqld.pid}"
MYSQL_LOG_FILE="${MYSQL_LOG_FILE:-$MYSQL_DATA_DIR/mysqld.log}"
MYSQL_DATABASE="${MYSQL_DATABASE:-app}"
RUN_COMMAND=false
STARTED_BY_HELPER=false
COMMAND=()

usage() {
  cat <<'USAGE'
Usage:
  mysql-test-server
    Start a local server and wait for it. Stop it with `mysql-test-server stop`.

  mysql-test-server --run command [args...]
    Start a local server, run the command, then always stop that server.

Environment:
  MYSQL_DATA_DIR     Data directory (default: /tmp/my_sqweel_mysql_test)
  MYSQL_TEST_PORT    TCP port (default: 3307)
  MYSQL_TEST_SOCKET  Unix socket path (default: /tmp/mysql-test.sock)
  MYSQL_DATABASE     Database created before the command (default: app; `test` too)
USAGE
}

if [[ "${1:-}" == "--help" || "${1:-}" == "-h" ]]; then
  usage
  exit 0
fi

if [[ -x /usr/sbin/mariadbd ]]; then
  MYSQLD_BIN=/usr/sbin/mariadbd
elif [[ -x /usr/sbin/mysqld ]]; then
  MYSQLD_BIN=/usr/sbin/mysqld
elif command -v mariadbd >/dev/null 2>&1; then
  MYSQLD_BIN="$(command -v mariadbd)"
elif command -v mysqld >/dev/null 2>&1; then
  MYSQLD_BIN="$(command -v mysqld)"
else
  echo "mysql/mariadb server binary not found" >&2
  exit 1
fi

MYSQL_BIN="$(command -v mariadb || command -v mysql || true)"
MYSQLADMIN_BIN="$(command -v mariadb-admin || command -v mysqladmin || true)"
if [[ -z "$MYSQL_BIN" || -z "$MYSQLADMIN_BIN" ]]; then
  echo "mysql/mariadb client tools not found" >&2
  exit 1
fi

if [[ "${1:-}" == "stop" ]]; then
  if [[ ! -r "$MYSQL_PID_FILE" ]]; then
    echo "no helper PID file at $MYSQL_PID_FILE" >&2
    exit 1
  fi
  pid="$(cat "$MYSQL_PID_FILE")"
  if [[ "$pid" =~ ^[0-9]+$ ]] && kill "$pid" 2>/dev/null; then
    wait "$pid" 2>/dev/null || true
  fi
  rm -f "$MYSQL_PID_FILE" "$MYSQL_SOCKET"
  exit 0
fi

if [[ "${1:-}" == "--run" ]]; then
  shift
  if (( $# == 0 )); then
    echo "--run requires a command" >&2
    exit 1
  fi
  RUN_COMMAND=true
  COMMAND=("$@")
fi

cleanup() {
  if [[ "$STARTED_BY_HELPER" == true && -r "$MYSQL_PID_FILE" ]]; then
    pid="$(cat "$MYSQL_PID_FILE")"
    if [[ "$pid" =~ ^[0-9]+$ ]]; then
      kill "$pid" 2>/dev/null || true
      wait "$pid" 2>/dev/null || true
    fi
    rm -f "$MYSQL_PID_FILE" "$MYSQL_SOCKET"
  fi
}

if [[ "$RUN_COMMAND" == true ]]; then
  trap cleanup EXIT INT TERM
fi

mkdir -p "$MYSQL_DATA_DIR"
if [[ ! -d "$MYSQL_DATA_DIR/mysql" ]]; then
  if command -v mariadb-install-db >/dev/null 2>&1; then
    mariadb-install-db --datadir="$MYSQL_DATA_DIR" >/dev/null
  elif command -v mysql_install_db >/dev/null 2>&1; then
    mysql_install_db --datadir="$MYSQL_DATA_DIR" >/dev/null
  else
    echo "no MySQL/MariaDB data-directory initialization tool found" >&2
    exit 1
  fi
fi

if [[ -S "$MYSQL_SOCKET" ]] \
  && "$MYSQLADMIN_BIN" --protocol=socket --socket="$MYSQL_SOCKET" ping --silent >/dev/null 2>&1; then
  echo "a server is already using $MYSQL_SOCKET; choose a fresh MYSQL_TEST_SOCKET" >&2
  exit 1
fi
rm -f "$MYSQL_SOCKET" "$MYSQL_PID_FILE"

"$MYSQLD_BIN" \
  --no-defaults \
  --datadir="$MYSQL_DATA_DIR" \
  --socket="$MYSQL_SOCKET" \
  --port="$MYSQL_PORT" \
  --bind-address=127.0.0.1 \
  --pid-file="$MYSQL_PID_FILE" \
  --log-error="$MYSQL_LOG_FILE" \
  --skip-grant-tables \
  --skip-name-resolve \
  >"$MYSQL_LOG_FILE" 2>&1 &
server_pid=$!
printf '%s\n' "$server_pid" > "$MYSQL_PID_FILE"
STARTED_BY_HELPER=true

for _ in $(seq 1 60); do
  if "$MYSQLADMIN_BIN" --protocol=socket --socket="$MYSQL_SOCKET" ping --silent >/dev/null 2>&1; then
    break
  fi
  sleep 1
done
if ! "$MYSQLADMIN_BIN" --protocol=socket --socket="$MYSQL_SOCKET" ping --silent >/dev/null 2>&1; then
  tail -n 50 "$MYSQL_LOG_FILE" >&2 || true
  exit 1
fi

"$MYSQL_BIN" --protocol=socket --socket="$MYSQL_SOCKET" --user=root \
  -e "CREATE DATABASE IF NOT EXISTS \`$MYSQL_DATABASE\`; CREATE DATABASE IF NOT EXISTS \`test\`;" >/dev/null

if [[ "$RUN_COMMAND" == true ]]; then
  "${COMMAND[@]}"
  exit $?
fi

echo "MariaDB ready at mysql://root@127.0.0.1:$MYSQL_PORT/$MYSQL_DATABASE?socket=$MYSQL_SOCKET"
echo "Use MYSQL_DATA_DIR=$MYSQL_DATA_DIR mysql-test-server stop when finished."
wait "$server_pid"
