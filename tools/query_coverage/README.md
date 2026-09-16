# Continuous query coverage

The controller discovers SQL, constructs reproducible cases, compares strict
MySqweel with MariaDB 10.11.7, and leaves verified local branches for review.
Discovery and status commands never invoke an agent. **Only `run` starts oh-my-pi.**
The worker does not run as part of CI; accepted JSON fixtures do.

## Setup and operation

Use a dedicated Linux worker with Python 3.11+, Rust, Git, Docker socket access,
and your existing oh-my-pi installation/provider configuration. The default exact
model is `llama.cpp/Qwen3.8-27B` with `xhigh` thinking. There is one agent session
at a time. Each invocation selects the same model for default/small/slow/plan roles,
disables extensions and subagent tools, and records its JSON event stream.
Reports record the agent executable's SHA-256 and any model identity emitted in
its event stream. A reported model differing from the configured identity parks
the task; the full event log remains available when a CLI emits no model identity.
If Docker socket access is unavailable, set `container_command = ["podman"]`
to use rootless Podman with the same pinned image and disposable-container lifecycle.

Commit the runner/harness implementation before selecting it as `base_ref`:
worktrees are created from commits, so uncommitted files are not included. Copy
`config.example.toml` to an ignored local path and customize the model ID if needed.
Provide a GitHub token through `GH_TOKEN` or `GITHUB_TOKEN` for code search. The
token is used by discovery and is removed from the agent's environment.

```sh
python3 tools/query_coverage.py --config .cache/coverage.toml discover
python3 tools/query_coverage.py --config .cache/coverage.toml status
# Explicitly starts coding-agent sessions; use only when the model is available:
python3 tools/query_coverage.py --config .cache/coverage.toml run --once
python3 tools/query_coverage.py --config .cache/coverage.toml run
python3 tools/query_coverage.py --config .cache/coverage.toml report
python3 tools/query_coverage.py --config .cache/coverage.toml retry CASE_ID
```

For mixed discovery, prepare the existing pinned upstream MTR package using
`tools/prepare_mariadb_mtr.sh`, then set `mtr_suite_root` and `mtr_args` to its
absolute paths. MTR runs use the existing official runner, complete unmodified
test/result files, and hash manifests. The pinned package/runtime is architecture
dependent; use the repository's documented Ubuntu ARM64 setup or the existing
Docker baseline preparation workflow. An empty MTR configuration leaves GitHub
discovery usable and records missing runtime configuration when applicable.

`run` first qualifies the base with formatting, clippy and `tools/prepush.sh`.
Missing infrastructure or failing checks prevents any case from becoming ready;
discovery continues. Every comparison uses a fresh resource-limited Docker server.
The harness verifies the reference version, creates a unique database per case,
and removes it afterward. It never resets a user-supplied database.

Stop with Ctrl-C. The controller kills active command process groups; interrupted
queue entries are recovered on restart. Each attempt retains its own checkout,
branch, logs and artifacts. Session timeouts and malformed agent output park the
case. Infrastructure failures back off and retry. Set time limits higher for a
slow local model or initial compilation; defaults are 30 minutes per session/check
and three repair attempts. SQL socket reads/writes time out after ten seconds;
the outer process timeout additionally bounds a streaming or hung test process.
Container ownership labels and a recovery journal let restart clean up containers
left by an interrupted worker without touching other workloads.

Use a service manager to restart the foreground command after host restarts if
desired. No background service is installed automatically. Archived worktrees,
branches and logs are intentionally retained; clean them up after review using
normal Git worktree/branch commands. State must live on a local filesystem with
SQLite locking support. Only one worker may own a state directory.
Matching minimized query structures and observations on the same base commit are
grouped under the first repair task. Related original fixtures and failure evidence
are retained in the report, without additional repair sessions. After integrating
the first fix, select the new base and retry grouped cases to qualify their fixtures.

## Case format

Committed fixtures live in `tests/query_cases/*.json`. New fixtures are included
automatically in the explicitly registered `query_coverage` Cargo test target.
Candidates use the identical format through `QUERY_COVERAGE_CASE`.

```json
{
  "version": 1,
  "id": "example",
  "provenance": {
    "inspiration": {
      "source_location": "owner/project@full-commit: queries/report.sql lines 12-18",
      "source_dialect": "postgresql",
      "observed_pattern": "conditional aggregate over optional child rows",
      "mysql_mariadb_translation": "new fixture uses SUM(CASE...) over independently designed parent/child rows"
    }
  },
  "features": ["update", "transaction"],
  "fixture_notes": "Two rows distinguish the changed row from an unaffected row.",
  "determinism_notes": "Inspection uses unique primary-key ordering.",
  "setup": [
    "CREATE TABLE items (id INT PRIMARY KEY, value INT)",
    "INSERT INTO items VALUES (1, 10), (2, 20)"
  ],
  "steps": [
    {"connection": "writer", "sql": "START TRANSACTION"},
    {"connection": "writer", "sql": "UPDATE items SET value = 30 WHERE id = 1"},
    {"connection": "writer", "sql": "COMMIT"}
  ],
  "checks": [
    {"connection": "reader", "sql": "SELECT id, value FROM items ORDER BY id", "ordered": true}
  ]
}
```

Setup statements must succeed. Steps default to connection `main`, unordered rows,
and success. Set `expect_error: true` only for an intentional error; its actual
code and SQLSTATE come from MariaDB. Connections remain open across sequential
steps, permitting session and transaction checks. Concurrent schedules are outside
v1. Use `checks` to inspect every table affected by mutations, including cascades.
Session defaults on both engines are `STRICT_TRANS_TABLES,NO_ENGINE_SUBSTITUTION`,
timezone `+00:00`, charset `utf8mb4`, and collation `utf8mb4_general_ci`. A case may
exercise session changes through explicit steps. Fixture assumptions and deterministic
ordering are reviewed by the agent and checked by repeated reference executions.

The comparator preserves column names and protocol types, typed cells, duplicate
rows, error codes/SQLSTATE, and affected rows. Unordered results are sorted as
multisets. No float tolerance, string/numeric coercion, error-message matching, or
NULL normalization is applied. Protocol-type mismatches are reported even if printed
values look equal. Automatic repairs cannot change these rules. `LIMIT` needs a
documented total ordering; random/time-dependent queries must be adapted explicitly.

To run one case against a **dedicated disposable** reference server manually:

```sh
MARIADB_COMPARE_URL=mysql://root:password@127.0.0.1:3306/test \
MARIADB_PARITY_REQUIRED=1 \
QUERY_COVERAGE_CASE=/absolute/example.json \
QUERY_COVERAGE_REPORT=/absolute/report.json \
cargo test --locked --test query_coverage query_coverage_cases -- --exact --nocapture
```

`QUERY_COVERAGE_MODE=baseline` runs the reference only. External cases always
require reference availability. Normal local tests may skip the database comparison
when Docker is unavailable; the pre-push and CI paths require it and fail closed.

## Review contract and limits

Ready branches contain either a regression and fix, or useful tests that already
passed. Original and minimized cases remain separate. Reports under
`state_dir/tasks/CASE_ID/RUN_ID` include baseline results, the failing comparison,
repeat comparisons, command metadata, model event logs, and the tested commit.
`latest.json` points to the latest attempt. The controller owns commits; it never
pushes, opens PRs, merges, or changes public coverage claims.

Original fixtures, reduced reproducers, source evidence, harness files, existing
tests, and comparison thresholds are protected during repair. Rust changes are
limited to `src/sql` implementation; cases needing a fix elsewhere are parked for
review. Worktrees and these admission checks are **not an OS security sandbox**:
oh-my-pi has shell access on the worker host. Run on a dedicated machine/account
whose writable files and credentials are appropriate for unattended coding.
Downloaded SQL runs only in disposable databases; source application scripts are
never part of the workflow. Only the worker's explicitly configured model is used.

GitHub discovery draws inspiration from MySQL/MariaDB, PostgreSQL, and SQLite source
files. A fixture must be independently authored for MySQL/MariaDB: it records the
source URL/commit, dialect, observed semantic pattern, and explicit translation,
but must not copy an external SQL statement verbatim or reconstruct the source
application's schema. The controller rejects verbatim source statements. This lets
all public sources contribute ideas without becoming copied test code. PostgreSQL and
SQLite patterns are accepted only where their translation is direct; engine-specific
types, operators, routines, extensions, concurrency, and storage features are parked.
Exact duplicates are removed; literal-preserving query variants remain eligible.
Structure fingerprints and feature tags guide scheduling, not a proof of semantic
coverage. Their current tokenizer is a lexical heuristic, not a full dialect parser.
Reports separate discovered and review-ready coverage.

Upstream MTR additions use separate hash manifests under
`tests/query_coverage_mtr`. CI includes these in both baseline and MySqweel MTR
gates after integration. Extracted MTR JSON cases remain local diagnostic artifacts
pending reuse review; they never substitute for the complete upstream file.
Raw query counts are not a guarantee across the entire SQL language. Public
coverage expands after manual integration and CI qualification of the pinned scope.

## Developer verification

```sh
python3 -m unittest discover -s tests -p 'test_*.py'
# Optional: exercise the controller's real container/comparison/cleanup path.
# Neither command starts oh-my-pi or sends a model request.
QUERY_COVERAGE_LIVE_CONTAINER_RUNTIME=podman \
  python3 -m unittest tests.test_query_coverage.LiveControllerTests
```

See [initial qualification notes](qualification-notes.md) for the baseline issues
exposed when the existing suites were first switched to the documented MariaDB
environment variables. Those must be resolved on the selected base before the
worker can enter its repair stage.
