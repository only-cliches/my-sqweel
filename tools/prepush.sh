#!/usr/bin/env bash
set -euo pipefail

MARIADB_DOCKER_IMAGE="mariadb:10.11.7"
MARIADB_DOCKER_PASSWORD="my-sqweel"
MARIADB_DOCKER_DATABASE="test"
MARIADB_CONTAINER=""

cleanup() {
  if [[ -n "$MARIADB_CONTAINER" ]]; then
    docker rm -f "$MARIADB_CONTAINER" >/dev/null 2>&1 || true
  fi
}

trap cleanup EXIT

minimum_open_files=8192
current_open_files="$(ulimit -Sn)"
if [[ "$current_open_files" != "unlimited" ]] \
  && (( current_open_files < minimum_open_files )); then
  if ! ulimit -Sn "$minimum_open_files"; then
    echo "could not raise the open-file limit from $current_open_files to $minimum_open_files" >&2
    exit 1
  fi
fi

if [[ -z "${MARIADB_COMPARE_URL:-}" ]]; then
  if ! command -v docker >/dev/null 2>&1; then
    echo "MariaDB parity requires MARIADB_COMPARE_URL or a working Docker installation" >&2
    exit 1
  fi
  if ! docker info >/dev/null 2>&1; then
    echo "Docker is installed but its daemon is unavailable" >&2
    exit 1
  fi
  if ! docker image inspect "$MARIADB_DOCKER_IMAGE" >/dev/null 2>&1; then
    echo "Pulling the CI comparison image: $MARIADB_DOCKER_IMAGE"
    docker pull "$MARIADB_DOCKER_IMAGE"
  fi

  MARIADB_CONTAINER="my-sqweel-prepush-mariadb-$$"
  echo "Starting the CI comparison image: $MARIADB_DOCKER_IMAGE"
  docker run \
    --detach \
    --rm \
    --name "$MARIADB_CONTAINER" \
    --env "MARIADB_ROOT_PASSWORD=$MARIADB_DOCKER_PASSWORD" \
    --env "MARIADB_DATABASE=$MARIADB_DOCKER_DATABASE" \
    --publish 127.0.0.1::3306 \
    "$MARIADB_DOCKER_IMAGE" >/dev/null

  mariadb_port="$(docker port "$MARIADB_CONTAINER" 3306/tcp | head -n 1)"
  mariadb_port="${mariadb_port##*:}"
  if [[ ! "$mariadb_port" =~ ^[0-9]+$ ]]; then
    echo "could not determine the published MariaDB port" >&2
    exit 1
  fi

  mariadb_ready="false"
  for _ in $(seq 1 90); do
    if docker exec "$MARIADB_CONTAINER" \
      mariadb-admin ping --silent --host 127.0.0.1 --password="$MARIADB_DOCKER_PASSWORD" \
      >/dev/null 2>&1; then
      mariadb_ready="true"
      break
    fi
    sleep 1
  done
  if [[ "$mariadb_ready" != "true" ]]; then
    echo "MariaDB did not become ready within 90 seconds" >&2
    docker logs "$MARIADB_CONTAINER" >&2 || true
    exit 1
  fi

  export MARIADB_COMPARE_URL="mysql://root:$MARIADB_DOCKER_PASSWORD@127.0.0.1:$mariadb_port/$MARIADB_DOCKER_DATABASE"
fi

echo "Running compatibility-report tooling tests"
python3 -m unittest discover -s tests -p 'test_*.py'

echo "Running all targets with MariaDB parity required"
export CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-1}"
MARIADB_PARITY_REQUIRED=1 cargo test --all-targets --locked

# Cargo verifies an unpacked archive, catching missing files and registry/path drift.
echo "Verifying the publishable crate"
cargo package --locked --allow-dirty
