# MariaDB upstream test exclusions

The strict MTR compatibility percentage uses the explicit manifest in
`tests/mariadb-mtr-allowlist.txt` merged with `tests/query_coverage_mtr/*.txt`.
Each entry names one complete, unmodified upstream file and pins the SHA-256
of both its `.test` and `.result` files.
The full MariaDB suite is not the denominator: many files combine supported SQL
with behavior that MySqweel intentionally does not provide.

| Whole-file qualification boundary | Reason |
| --- | --- |
| Isolation levels other than `REPEATABLE READ`, fine-grained row locking, and XA | Outside the serialized transaction contract. Basic transactions, autocommit, and savepoints are eligible for discovery. |
| DDL and catalog administration inside active transactions | The transactional backend rejects these operations rather than implicitly committing. |
| Replication, binary logging, group replication, and NDB | Require server topology or storage engines that MySqweel does not implement. |
| Full privilege-system behavior, authentication plugins, and TLS | Outside the development provisioning subset. `CREATE/DROP USER`, database-wide DML grants, and `REVOKE ALL PRIVILEGES` remain in scope. |
| Stored procedures, stored functions, triggers, and events | Outside the supported SQL surface. |
| Tests whose main path requires stored-function creation (`create`, `func_math`) | The allowlist measures the supported SQL surface, not routines. |
| File output/removal tests (`distinct`) | MTR file-system side effects are outside the SQL wire compatibility contract. |
| Optimizer plans, hints, index statistics, and performance tests | Exact optimizer behavior is not part of the contract. |
| GIS, full-text indexes, physical table partitioning, and specialized storage engines | Not implemented by the in-memory engine. Window `PARTITION BY` is not table partitioning. |
| Platform, crash, debug, and resource-limit tests | Environment or process behavior is not SQL compatibility. |

## Admission rules

An upstream test is admitted only when all of the following are true:

1. The complete, unmodified file passes against MariaDB 10.11.7 through MTR's
   external-server mode.
2. Every statement and expected side effect in the file is inside MySqweel's
   documented compatibility boundary.
3. The complete file passes against MySqweel using the same official
   MariaDB `mysqltest`-compatible binary and upstream expected result.
4. The manifest pins the exact upstream test and result hashes from Ubuntu
   package revision `1:10.11.7-2ubuntu2`.

A file must not enter the strict gate merely to improve the percentage. Broad
files such as `alter_table`, `select_all`, and `func_str` can remain outside that
gate while still being required for testing: mixed files need derived scenarios
for their relevant SQL. A whole-file execution blocker is not a scope exemption.

## Current upstream coverage

The merged strict gate passes 35 complete files and 441 direct SQL statements
locally against both engines, with no infrastructure failures. These are local
qualification results; both CI gates independently verify the same merged pins.

| Area | Upstream files | Feature evidence |
| --- | --- | --- |
| DDL | `create_drop_index`, `create_replace_tmp`, `key_primary`, `alter_table_autoinc-5574`, `alter_table_trans`, `create_drop_view` | Index lifecycle, temporary-table replacement, primary keys, ALTER constraint behavior, auto-increment lowering, and view lifecycle. |
| DML | `bulk_replace`, `insert_update_autoinc-7150`, `insert_returning_datatypes`, `replace_returning_datatypes` | Multi-row replacement, auto-increment conflict updates, and typed `INSERT`/`REPLACE ... RETURNING`. |
| Metadata | `show_row_order-9226` | Stable `SHOW COLUMNS` ordering across large `ENUM` definitions. |
| Aggregation | `group_by_null`, `sum_distinct`, `innodb_group` | Grouping with null-producing expressions, distinct aggregates, and InnoDB aggregate edge cases. |
| Subqueries | `subselect_nulls`, `subselect_nulls_innodb`, `in_datetime_241` | Correlated `IN`/`EXISTS`, null-safe joins, row comparisons, date-valued scalar subqueries, and three-valued null logic. |
| Ordering | `order_by-mdev-10122` | Aggregate ordering inside parenthesized queries and `UNION` operands. |
| Date/time | `adddate_454`, `timezone4`, `datetime_456`, `str_to_datetime_457`, `func_timestamp`, `type_interval` | Interval arithmetic, Unix timestamps, boundary values, temporal casts, warnings, decimal timestamp rendering, and interval extraction. |
| Windows | `win_empty_over`, `win_insert_select`, `win_std`, `win_percent_cume` | Empty `OVER()` clauses, aggregates, ranking, variance, cumulative distributions, and windowed `INSERT ... SELECT`. |
| JSON | `json_equals` | Structural equality, Unicode, numeric precision, nesting limits, recursive construction, and character sets. |
| Generated columns | `vcol/delayed`, `vcol/mrr`, `gcol/innodb_prefix_index_check` | Generated indexes, indexed predicates, optimizer-switch independence, `REPLACE DELAYED`, and generated-column prefix indexes. |
| Uniqueness | `unique` | Unique-key insertion, nullable duplicates, and indexed deletes. |
| Scalar comparisons | `func_equal` | Upstream equality-comparison assertions. |
| Transactions | `innodb/innodb_bug57255` | Committing parent/child inserts and cascading deletes. |

The focused non-gating SQL audit in
[`tests/mariadb-mtr-scope.txt`](mariadb-mtr-scope.txt) contains 39 complete files and
693 direct statements. All 39 pass MariaDB. MySqweel passes 26, with nine SQL
mismatches and four unsupported cases; there are no baseline or infrastructure
failures. The thirteen failures are retained, not removed to improve the score.

Correcting the window-partition filter exposes fourteen files. Each passed two
standalone MariaDB baselines, and three differential runs produced the same
pass/fail outcomes. `win_percent_cume` passes; the remaining failures are:

- Unsupported: `union_innodb` (correlated subquery shape), `win_bit` (`BIT_OR`
  window function), `win_lead_lag`, and `win_nth_value` (window argument handling).
- SQL mismatches: `win_as_arg_to_aggregate_func`, `win_avg`,
  `win_first_last_value`, `win_min_max`, `win_ntile`, `win_orderby`,
  `win_percentile`, `win_rank`, and `win_sum`. Wrong expected error codes,
  including a returned 1235 instead of expected 1064, remain SQL mismatches.

`win_percent_cume`, `win_std`, and `innodb/innodb_bug57255` each passed three
complete-file comparisons before entering the additional strict manifest.
The transaction case has 18 direct statements and inserts 257 parent and
486 child rows before committing and exercising cascading deletes.

The timestamp repair qualifies value rendering and engine metadata, not every
wire metadata field: raw column-definition `Decimals` still reports zero for
the six-decimal `UNIX_TIMESTAMP` text-input probe.

The runner stages a copy of MariaDB's MTR script for each invocation and changes
only its external-server feature probe from `USE mysql; SHOW VARIABLES` to
`SHOW VARIABLES`. Server variables do not depend on the selected database; this
allows servers without a selectable `mysql` schema to reach test execution.
The same adaptation is applied to both engines, and an unexpected or ambiguous
probe causes the run to fail. Upstream `.test`/`.result` files and the official
`mysqltest` binary remain unchanged. Each invocation records source and adapted
runner SHA-256 hashes in `mariadb-test-run.json` alongside the staged script.

The discovery filter admits basic transaction commands and labels their feature
category `transactions`; the InnoDB suite remains restricted to explicitly
reviewed cases in safe-harness mode.

Broader transaction coverage still needs qualifying complete files. In particular,
`commit` combines transactions with unsupported isolation levels, chaining, routines,
and XA; `rollback` requires nontransactional MyISAM behavior; and
`innodb/temp_table_savepoint` requires routines and file-system side effects.
`innodb/mvcc_secondary` is not in the executable scope because its additional
`localhost` connection uses a local socket instead of the configured external
endpoint. Savepoint and rollback behavior remains covered by the focused backend
and wire regression suites until suitable complete upstream files qualify.

Features without a suitable complete upstream file can be exercised by the
differential corpus, focused parity tests, or the separate derived track below.
None of that evidence is represented as a complete upstream-file pass.

## Automated discovery

The [MariaDB MTR discovery workflow](../.github/workflows/mariadb-mtr-discovery.yml)
inventories every `.test` path in the pinned MariaDB 10.11.7 distribution,
including flat suites, plugin trees, helpers, and nested layouts.
`mariadb-mtr-testing-plan.json` has exactly one entry per inventoried path, with
full test/result hashes and separate `scope` and `testing` records.

Enrollment is fail-closed: every file remains required unless an explicit
out-of-scope review in `tests/mariadb-mtr-scope-reviews.json` matches its current
hashes and supplies a rationale. Suite names, unsupported harness operations,
missing results, and inability to execute are never sufficient exemptions.
The two current exemptions are `main/flush_ssl.test` (TLS certificate reload)
and `main/plugin_loaderr.test` (plugin startup failures), reviewed against
MariaDB commit `87e13722a95af5d9378d990caf48cb6874439347`.

| Scope status | Testing intent | Pinned inventory |
| --- | --- | ---: |
| `in-scope` | Required complete-file comparison; blocked until executable | 1,432 |
| `mixed` | Required derived coverage of relevant SQL; extraction remains blocked work | 4,686 |
| `review-required` | Required semantic review; never silently excluded | 1,783 |
| `out-of-scope` | Not required, only with a matching explicit review | 2 |

These are conservative automatic scope signals, not 7,903 completed human
reviews. Evidence records feature families and source line numbers. Unknown SQL
and unsupported features do not establish that a whole file lacks relevant SQL.
Existing hash-pinned complete-file manifests provide reviewed overrides;
test success alone does not determine scope.

The local inventory therefore marks **7,901 files required**, with **155 ready**
and **7,746 blocked**. The ready set contains **5,207 direct and sourced statements**
and preserves all 48 distinct files in the strict and focused manifests. The
previous 248 static candidates plus manually selected `timezone4` are not an
exhaustive testing plan: 94 of those files now have explicit mixed/uncertain
scope blockers instead of being automatically queued. They remain required.

Execution eligibility is narrower than scope. The static audit follows contained
literal MTR includes and permits reviewed bookkeeping, connections, and send/reap
operations. Dynamic SQL/includes, custom delimiters, filesystem/process effects,
configuration, topology, and unsupported layouts remain execution blockers.
Window `PARTITION BY` is not physical table partitioning. Quoted text and ordinary
comments are masked; executable comments and original evidence line numbers are
preserved. Old inventory `exclusion` labels describe execution filters only.

Regenerate and validate the exhaustive plan:

```sh
python3 tools/mariadb_mtr_discover.py \
  --suite-root "$MARIADB_MTR_ROOT" --scope all --include-safe-harness \
  --limit 100000 --max-statements 100000 \
  --source-revision 10.11.7-2ubuntu2 \
  --output-dir artifacts/mariadb-mtr-discovery
python3 tools/mariadb_mtr_plan.py \
  --inventory artifacts/mariadb-mtr-discovery/mariadb-mtr-discovery.json \
  --plan artifacts/mariadb-mtr-discovery/mariadb-mtr-testing-plan.json \
  --report-dir artifacts/mariadb-mtr-discovery/coverage-planning
```

Weekly, manual, relevant push, and pull-request runs select every ready candidate.
MariaDB runs first; MySqweel runs only when that baseline passes. CI checks exact
path accounting, pins, reviewed exemptions, candidate selection, and derived
source bindings before execution. Its final coverage command adds repeatable
`--complete-report` and `--derived-report` arguments for canonical MTR reports.
Missing selected outcomes, malformed or stale inputs, invalid baselines, and
infrastructure failures fail CI. Ordinary SQL incompatibilities remain non-gating.

The final report separates complete-file observations, partial derived
observations, and unexecuted files. Blocked work remains visibly `blocked`, not
covered or passed. Markdown gives bounded previews; JSON retains every path.
A derived observation cannot complete its source file, including when attached
to a file that still requires a complete comparison or semantic review.
Planning-only reports say `planning`, never claim execution.

The fresh 155-file local audit contains 4,079 direct statements and records
36 passes, 50 SQL mismatches, 58 unsupported/skipped outcomes, six baseline
failures, and five infrastructure outcomes. The 58 include eight MariaDB skips
and 50 MySqweel unsupported cases. This audit is **invalid**, not a passing gate.
The accounting report retains 136 completed whole-file comparisons and one
partial derived observation; 7,764 required files have no completed comparison.
All 155 selected files have explicit outcomes. Baseline and harness failures
remain reported rather than being reclassified as scope exemptions.

Promotion output includes only complete files that passed both engines.
Derived reports are rejected by the promotion command. Review against the
admission rules is still required before merging any generated candidate.

## Upstream-derived scenarios

`tools/mariadb_mtr_derived.py` consumes `tests/mariadb-mtr-derived.json`. Each
entry pins an immutable upstream commit and URL, full `.test`/`.result` hashes,
inclusive contiguous line ranges, and a dependency rationale. The runner
verifies pins and bounds, rejects includes, and stages byte-identical slices
in a disposable installation view. Runtime files are shared through symlinks;
installed SQL and expected-result files are never edited. Session-state and
other dependency closure must be reviewed by the author, not inferred from
the presence of a rationale string.

The initial scenario selects `func_math.test` lines 51–56 and its result lines
130–147 from [MariaDB commit
87e13722a95af5d9378d990caf48cb6874439347](https://github.com/MariaDB/server/blob/87e13722a95af5d9378d990caf48cb6874439347/mysql-test/main/func_math.test).
This six-statement scalar block has no tables, includes, or optimizer-plan
assertions. Two MariaDB baseline runs pass. Three differential runs retain the
same unsupported `ACOS` failure; the range and oracle were not weakened.
Reports use `coverage_kind: derived-scenarios`, include original provenance,
and keep scenario metrics separate from complete-file qualification.
