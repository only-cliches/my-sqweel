# harness-hat Rust image
#
# Build after harness-hat-base:local:
#   docker build -t harness-hat-mysqweel:local -f my-sqweel.dockerfile .

# Rust's official manifest is pinned and selects the matching target
# architecture. The copied rustup/cargo trees retain side-by-side toolchain
# management without manually downloading rustup-init per architecture.
FROM harness-hat-base:local

USER root

ENV RUSTUP_HOME=/usr/local/rustup
ENV CARGO_HOME=/usr/local/cargo
ENV PATH="${CARGO_HOME}/bin:${PATH}"

# Keep the image's comparison server aligned with the compatibility target.
# Harness Hat currently uses Ubuntu 26.04, while Ubuntu packaged 10.11.7 for
# 24.04; do not mix those distribution repositories. Install MariaDB's
# official, architecture-specific binary tarball instead. Its checksum makes
# the version pin reproducible and causes a failed download to fail the build.
ARG TARGETARCH
ARG MARIADB_VERSION=10.11.7
ARG MARIADB_BINARY_SHA256_AMD64=5ea876f814f270bdb9118f0b6091757278de7d851b367d1213474331bebe8b61
# The upstream client needs ABI 5, which Ubuntu 26.04 no longer ships.
# Pin only the co-installable compatibility libraries; do not add a Jammy repo.
ARG NCURSES_COMPAT_VERSION=6.3-2ubuntu0.3
ARG LIBNCURSES5_SHA256_AMD64=58da01991712c6856af0658dc5e9cb533c1a6854ab950055ade185e35bc9c276
ARG LIBTINFO5_SHA256_AMD64=4df4288404108f1a156d014e8764a064e977e34e6d44931ab60451694c03c90d
ENV MARIADB_HOME=/opt/mariadb
ENV PATH="${MARIADB_HOME}/bin:${CARGO_HOME}/bin:${PATH}"

RUN set -eu; \
    apt-get update -o APT::Update::Error-Mode=any; \
    DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
      build-essential \
      make \
      cmake \
      pkg-config \
      clang \
      lld \
      mold \
      gdb \
      lldb \
      protobuf-compiler \
      sqlite3 \
      libsqlite3-dev \
      libssl-dev \
      libaio1t64 \
      libnuma1 \
      libpcre2-8-0 \
      libpcre2-posix3 \
      libpmem1 \
      liburing2 \
      perl \
      zlib1g \
      jq \
      shellcheck \
      direnv; \
    case "${TARGETARCH:-$(dpkg --print-architecture)}" in \
      amd64|x86_64) mariadb_arch=x86_64; mariadb_sha256="$MARIADB_BINARY_SHA256_AMD64" ;; \
      *) echo "MariaDB ${MARIADB_VERSION} binary tarball is configured only for amd64; got ${TARGETARCH:-$(dpkg --print-architecture)}" >&2; exit 1 ;; \
    esac; \
    install_dir="$(mktemp -d)"; \
    trap 'rm -rf "$install_dir"' EXIT; \
    for package in libtinfo5 libncurses5; do \
      curl -fsSL --retry 5 --retry-all-errors --retry-delay 2 \
        -o "$install_dir/$package.deb" \
        "https://security.ubuntu.com/ubuntu/pool/universe/n/ncurses/${package}_${NCURSES_COMPAT_VERSION}_amd64.deb"; \
    done; \
    printf '%s  %s\n' \
      "$LIBTINFO5_SHA256_AMD64" "$install_dir/libtinfo5.deb" \
      "$LIBNCURSES5_SHA256_AMD64" "$install_dir/libncurses5.deb" \
      | sha256sum -c -; \
    dpkg -i "$install_dir/libtinfo5.deb" "$install_dir/libncurses5.deb"; \
    curl -fsSL --retry 5 --retry-all-errors --retry-delay 2 \
      -o "$install_dir/mariadb.tar.gz" \
      "https://dlm.mariadb.com/3720296/MariaDB/mariadb-${MARIADB_VERSION}/bintar-linux-systemd-${mariadb_arch}/mariadb-${MARIADB_VERSION}-linux-systemd-${mariadb_arch}.tar.gz"; \
    echo "${mariadb_sha256}  $install_dir/mariadb.tar.gz" | sha256sum -c -; \
    tar -xzf "$install_dir/mariadb.tar.gz" -C /opt; \
    mv "/opt/mariadb-${MARIADB_VERSION}-linux-systemd-${mariadb_arch}" "$MARIADB_HOME"; \
    ln -s "$MARIADB_HOME/bin/mariadbd" /usr/local/bin/mariadbd; \
    for tool in mariadb mariadb-admin mysql mysqladmin mysqltest; do \
      if [ -x "$MARIADB_HOME/bin/$tool" ]; then ln -s "$MARIADB_HOME/bin/$tool" "/usr/local/bin/$tool"; fi; \
    done; \
    printf '%s\n' '#!/bin/sh' \
      'exec /opt/mariadb/scripts/mariadb-install-db --basedir=/opt/mariadb "$@"' \
      > /usr/local/bin/mariadb-install-db; \
    chmod 0755 /usr/local/bin/mariadb-install-db; \
    test -x "$MARIADB_HOME/bin/mariadbd"; \
    test -x "$MARIADB_HOME/bin/mysqltest"; \
    test -d "$MARIADB_HOME/mysql-test"; \
    mariadbd --version | grep -F "${MARIADB_VERSION}-MariaDB"; \
    mariadb --version; \
    mysqltest --version; \
    apt-get clean; \
    rm -rf /var/lib/apt/lists/*

# The helper starts a disposable local MariaDB instance for parity and MTR
# runs when no external comparison server is supplied. The workspace remains
# mounted by Harness Hat, but installing this copy makes the image usable on
# its own as well.
COPY vendor/mysql-test-server.sh /usr/local/bin/mysql-test-server
RUN chmod +x /usr/local/bin/mysql-test-server

# Exercise initialization and TCP queries as the runtime user, not just --version.
# The helper stops the server; the trap removes its data, socket, PID, and logs.
USER coder
RUN set -eu; \
    smoke_dir="$(mktemp -d)"; \
    trap 'rm -rf "$smoke_dir"' EXIT; \
    result="$(MYSQL_DATA_DIR="$smoke_dir/data" \
      MYSQL_TEST_SOCKET="$smoke_dir/mysql.sock" \
      MYSQL_TEST_PORT=3307 \
      mysql-test-server --run \
        mariadb --no-defaults --protocol=tcp --host=127.0.0.1 --port=3307 \
        --user=root --database=app --batch --skip-column-names \
        --execute='CREATE TABLE smoke (value INT) ENGINE=InnoDB; INSERT INTO smoke VALUES (42); SELECT value FROM smoke;')"; \
    test "$result" = 42
USER root

# Rust installs as root into the shared RUSTUP_HOME/CARGO_HOME. The container
# runs as `coder` (uid 1000), so hand ownership of both trees to that user in
# this same layer — otherwise `cargo build` (registry/cache writes), `cargo
# install` (writes to cargo/bin), and `rustup` updates all fail on root-owned
# paths. Doing the chown here (not a later layer) avoids duplicating the large
# toolchain with new ownership. a+rX keeps it readable if run under another uid.
# Pinned versions (H5): the official multi-architecture Rust image is pinned
# by manifest digest; rustup verifies component signatures/hashes internally.
# The cargo tools remain exact-version pins. Bump the image tag and digest
# together when updating the toolchain.
ARG RUST_TOOLCHAIN=1.97.0
COPY --from=rust /usr/local/cargo /usr/local/cargo
COPY --from=rust /usr/local/rustup /usr/local/rustup
RUN set -eu; \
    rustup toolchain install "${RUST_TOOLCHAIN}" --profile default; \
    rustup default "${RUST_TOOLCHAIN}"; \
    rustup component add rustfmt clippy rust-src rust-analyzer; \
    cargo install --locked \
      cargo-edit@0.13.11 \
      cargo-watch@8.5.3 \
      cargo-nextest@0.9.140 \
      cargo-audit@0.22.2 \
      cargo-deny@0.20.2; \
    chmod -R a+rX "${RUSTUP_HOME}" "${CARGO_HOME}"; \
    chown -R coder:coder "${RUSTUP_HOME}" "${CARGO_HOME}"

USER coder

ENV RUSTUP_HOME=/usr/local/rustup
ENV CARGO_HOME=/usr/local/cargo
ENV PATH="${CARGO_HOME}/bin:/home/coder/.local/bin:${PATH}"

CMD ["bash"]
