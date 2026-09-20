#!/usr/bin/env bash
set -euo pipefail

# Download and extract the pinned Ubuntu ARM64 MariaDB test runner without
# installing a second server into the runner.  The CI service container is the
# external baseline; these packages provide only the client, mysqltest, MTR,
# and upstream test data used by the compatibility job.

ROOT="${1:?usage: prepare_mariadb_mtr.sh ROOT [--print-env]}"
PRINT_ENV="${2:-}"
PACKAGE_VERSION="${MARIADB_MTR_PACKAGE_VERSION:-1:10.11.7-2ubuntu2}"
PACKAGE_CACHE="${MARIADB_MTR_CACHE_DIR:-$ROOT/packages}"

run_logged() {
  if [[ "$PRINT_ENV" == "--print-env" ]]; then
    "$@" >&2
  else
    "$@"
  fi
}

if [[ "$(dpkg --print-architecture)" != "arm64" ]]; then
  echo "MariaDB MTR packages are pinned to Ubuntu ARM64; this runner is $(dpkg --print-architecture)" >&2
  exit 1
fi

mkdir -p "$ROOT" "$PACKAGE_CACHE"
pushd "$PACKAGE_CACHE" >/dev/null
for package_spec in \
  "mariadb-test=$PACKAGE_VERSION" \
  "mariadb-test-data=$PACKAGE_VERSION" \
  "mariadb-client=$PACKAGE_VERSION" \
  "mariadb-client-core=$PACKAGE_VERSION" \
  "mariadb-common=$PACKAGE_VERSION" \
  "mariadb-server=$PACKAGE_VERSION" \
  "mariadb-server-core=$PACKAGE_VERSION" \
  "libmariadb3=$PACKAGE_VERSION"; do
  package_name="${package_spec%%=*}"
  if ! compgen -G "${package_name}_*.deb" >/dev/null; then
    run_logged apt-get download "$package_spec"
  fi
done
popd >/dev/null

for package_file in "$PACKAGE_CACHE"/*.deb; do
  run_logged dpkg-deb --extract "$package_file" "$ROOT"
done

suite_root="$ROOT/usr/share/mysql"
mysqltest_bin="$ROOT/usr/bin/mysqltest"
client_bindir="$ROOT/usr/bin"
mtr_runner="$suite_root/mysql-test/mariadb-test-run.pl"
safe_process="$suite_root/mysql-test/lib/My/SafeProcess/my_safe_process"

for required_file in \
  "$mysqltest_bin" \
  "$client_bindir/mysql" \
  "$client_bindir/mysqladmin" \
  "$mtr_runner" \
  "$safe_process" \
  "$suite_root/mysql_test_data_timezone.sql" \
  "$suite_root/mysql-test/include/mtr_warnings.sql" \
  "$suite_root/mysql-test/main/1st.test" \
  "$suite_root/mysql-test/main/1st.result"; do
  if [[ ! -e "$required_file" ]]; then
    echo "required MariaDB MTR file is unavailable: $required_file" >&2
    exit 1
  fi
done

if [[ "$PRINT_ENV" == "--print-env" ]]; then
  printf 'MARIADB_MTR_ROOT=%q\n' "$suite_root"
  printf 'MARIADB_MTR_VERSION=%q\n' "${PACKAGE_VERSION#1:}"
  printf 'MYSQLTEST_BIN=%q\n' "$mysqltest_bin"
  printf 'MYSQL_CLIENT_BINDIR=%q\n' "$client_bindir"
  printf 'MTR_RUNNER=%q\n' "$mtr_runner"
  printf 'MTR_SAFE_PROCESS=%q\n' "$safe_process"
  printf 'MTR_BINDIR=%q\n' "$ROOT/usr"
else
  echo "MariaDB MTR package: $PACKAGE_VERSION"
  echo "MTR suite: $suite_root/mysql-test"
  echo "mysqltest: $mysqltest_bin"
  echo "MTR runner: $mtr_runner"
fi
