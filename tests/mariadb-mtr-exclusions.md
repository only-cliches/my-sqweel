# MariaDB upstream test exclusions

The strict MTR compatibility percentage uses the explicit manifest in
`tests/mariadb-mtr-allowlist.txt` merged with `tests/query_coverage_mtr/*.txt`.
Each entry names one complete, unmodified upstream file and pins the SHA-256
of both its `.test` and `.result` files.
The full MariaDB suite is not the denominator: many files combine supported SQL
with behavior that MySqweel intentionally does not provide.

| Excluded area | Reason |
| --- | --- |
| Isolation levels other than `REPEATABLE READ`, fine-grained row locking, and XA | Outside the serialized transaction contract. Basic transactions, autocommit, and savepoints are eligible for discovery. |
| DDL and catalog administration inside active transactions | The transactional backend rejects these operations rather than implicitly committing. |
| Replication, binary logging, group replication, and NDB | Require server topology or storage engines that MySqweel does not implement. |
| Users, grants, authentication plugins, and TLS | MySqweel exposes a local development wire endpoint, not MySQL access control. |
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

An excluded test must not be added merely to improve the percentage. Broad
files such as `alter_table`, `select_all`, and `func_str` remain excluded even
when MySqweel supports part of their behavior, because their complete files
also exercise excluded capabilities.

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

The non-gating
[MariaDB MTR discovery workflow](../.github/workflows/mariadb-mtr-discovery.yml)
inventories every `.test` path in the pinned MariaDB 10.11.7 distribution. Flat
suites, plugin trees, helpers, and nested layouts receive explicit inventory
entries and exclusion reasons; ambiguous execution names are rejected.
The static audit follows contained literal MTR `source`/`include` files and
admits safe bookkeeping variables, multiple connections, and asynchronous
send/reap behavior. Missing or dynamic includes and unresolved dynamic SQL are
excluded, as are custom delimiters, process/file-system side effects, server
configuration, topology requirements, and out-of-contract storage behavior.
Physical table partitions remain excluded, including after an earlier window
clause; quoted text and ordinary comments do not trigger SQL exclusions, while
standard and MariaDB executable comments are inspected.

The local pinned inventory accounts for all 7,903 paths: 248 static candidates
and 16,616 direct and sourced statements, plus 7,655 explicitly excluded paths.
The candidate count is smaller than the previous incomplete inventory because
unresolved dynamic SQL and includes are no longer assumed safe. Candidacy is
neither a passing result nor an exhaustive list of supported SQL.

Weekly, manual, relevant push, and pull-request runs select every candidate,
including after SQL-engine, storage, and wire-server changes. MariaDB runs
first; MySqweel runs only when that baseline passes. The workflow publishes the
inventory, complete-file audit, focused audit, and separate derived report.
Reports distinguish pass, SQL mismatch, unsupported/skip, baseline failure,
infrastructure failure, and not-run outcomes. A pass requires the requested
case's MTR pass marker, successful exit, and completed-run summary. Missing
reports, invalid baselines, and infrastructure failures fail CI even though
ordinary SQL incompatibilities remain non-gating in the discovery workflow.

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
