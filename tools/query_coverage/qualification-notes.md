# Initial qualification findings — 2026-09-15

Verification used a working checkout based on
`90cb3504450129047ba35cc38eded342aff7ef46`, with the coverage implementation and
the shared comparison helper's MariaDB environment-variable fix applied.
No oh-my-pi session or model request was used for implementation verification.

The previous helper only consumed `MYSQL_COMPARE_URL` and
`MYSQL_PARITY_REQUIRED`, while CI and pre-push configured `MARIADB_*`.
The helper now prefers the MariaDB URL, honors either required flag, and defaults
to `mariadb:10.11.7`; the legacy MySQL URL remains a fallback. Consequently, a
MariaDB qualification now exercises the existing tests against the declared target.

## Passing verification

- The new `query_coverage` target passed against the pinned MariaDB 10.11.7 image,
  including both committed scenarios and comparator behavior tests.
- The controller's default Python tests use fake agent processes and mocked services.
  They exercise passing and repair branches, failure/timeout handling, and queue recovery.
- `cargo clippy --all-targets --all-features --locked` completed with existing warnings.
- `cargo package --locked --allow-dirty` built and verified the publishable package.
- The modified/new Rust files pass rustfmt, and the patch passes `git diff --check`.

## Base is not qualified

`tools/prepush.sh` was run with a disposable MariaDB reference whose version was
`10.11.7-MariaDB-1:10.11.7+maria~ubu2204`.

The existing 2,500-query corpus passed 2,498 queries and failed two:

| Case | Reference observation |
| --- | --- |
| `window-lag` | MariaDB rejects `LAG(amount, 1, 0)` with error 1064; this is a MySQL-oriented three-argument form. |
| `window-cume-dist-peers` | MariaDB returns `0.5000000000` / `1.0000000000`; MySqweel returns `0.5` / `1`. The existing comparator compares their text. |

The remaining test targets were then run with comparison required. Four of the
seven existing `mysql_parity` tests failed: information-schema metadata, JSON
expressions, supported semantics, and primitive/compound sorting. For example,
MariaDB reports `bigint(20)` in column metadata while MySqweel reports `bigint`.
The other selected targets passed: error compatibility, SQL semantics, new features,
ORM compatibility, query engine, schema metadata, wire transactions, and new coverage.

The repository-wide formatting check also reports pre-existing formatting
differences in server, engine-test, and vendored wire-server files. Those files
were left unchanged by this implementation.

These findings are not waived by the controller. Discovery can run independently,
but base qualification must pass before autonomous fixtures/repairs start. Resolving
the existing suite's MySQL-vs-MariaDB assumptions and underlying compatibility
differences is separate from accepting new discovered cases. Do not lower the
coverage floor or silently suppress the failing cases to qualify a base.
