# MariaDB upstream test exclusions

The MTR compatibility percentage uses the explicit manifest in
`tests/mariadb-mtr-allowlist.txt`. Each entry names one complete, unmodified
upstream file and pins the SHA-256 of both its `.test` and `.result` files.
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
| GIS, full-text indexes, partitioning, and specialized storage engines | Not implemented by the in-memory engine. |
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

The strict gate contains 32 complete files and 381 SQL statements:

| Area | Upstream files | Feature evidence |
| --- | --- | --- |
| DDL | `create_drop_index`, `create_replace_tmp`, `key_primary`, `alter_table_autoinc-5574`, `alter_table_trans`, `create_drop_view` | Index lifecycle, temporary-table replacement, primary keys, ALTER constraint behavior, auto-increment lowering, and view lifecycle. |
| DML | `bulk_replace`, `insert_update_autoinc-7150`, `insert_returning_datatypes`, `replace_returning_datatypes` | Multi-row replacement, auto-increment conflict updates, and typed `INSERT`/`REPLACE ... RETURNING`. |
| Metadata | `show_row_order-9226` | Stable `SHOW COLUMNS` ordering across large `ENUM` definitions. |
| Aggregation | `group_by_null`, `sum_distinct`, `innodb_group` | Grouping with null-producing expressions, distinct aggregates, and InnoDB aggregate edge cases. |
| Subqueries | `subselect_nulls`, `subselect_nulls_innodb`, `in_datetime_241` | Correlated `IN`/`EXISTS`, null-safe joins, row comparisons, date-valued scalar subqueries, and three-valued null logic. |
| Ordering | `order_by-mdev-10122` | Aggregate ordering inside parenthesized queries and `UNION` operands. |
| Date/time | `adddate_454`, `timezone4`, `datetime_456`, `str_to_datetime_457`, `func_timestamp`, `type_interval` | Interval arithmetic, Unix timestamps, boundary values, temporal casts, warnings, decimal timestamp metadata, and interval extraction. |
| Windows | `win_empty_over`, `win_insert_select` | Empty `OVER()` clauses, window aggregates, ranking, and windowed `INSERT ... SELECT`. |
| JSON | `json_equals` | Structural equality, Unicode, numeric precision, nesting limits, recursive construction, and character sets. |
| Generated columns | `vcol/delayed`, `vcol/mrr`, `gcol/innodb_prefix_index_check` | Generated indexes, indexed predicates, optimizer-switch independence, `REPLACE DELAYED`, and generated-column prefix indexes. |
| Uniqueness | `unique` | Unique-key insertion, nullable duplicates, and indexed deletes. |

The focused non-gating audit scope in
[`tests/mariadb-mtr-scope.txt`](mariadb-mtr-scope.txt) contains 25 complete files and 339 SQL
statements across DDL, DML, aggregates, subqueries, date/time, window functions, JSON, and
generated columns, plus transactions. Its results are reported separately from the strict manifest. `win_std` now passes in full
against both engines locally and remains audit-only pending CI qualification and promotion.

The transaction audit adds `innodb/innodb_bug57255`: one complete upstream file
with 18 direct SQL statements, including a transaction that inserts 257 parent
and 486 child rows before committing and exercising cascading deletes. The complete
file passes locally against both MariaDB 10.11.7 and MySqweel. It remains in the
non-gating audit pending CI qualification and strict-manifest promotion. The
25-file focused scope now passes in full against both MariaDB and the transactional
MySqweel backend (339 direct SQL statements), with no infrastructure failures.
All 12 previously failing files pass after fixes to authorization parsing, session
settings, warning propagation, and window result/error metadata. The manifest and
upstream expected results were not reduced or weakened to achieve this result.

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

Features without a suitable complete upstream file are covered by the
differential corpus and focused parity tests. They are not represented as an
upstream MTR pass until a complete qualifying file is found or MySqweel grows
to support the rest of the relevant upstream file.

## Automated discovery

The non-gating
[MariaDB MTR discovery workflow](../.github/workflows/mariadb-mtr-discovery.yml)
inventories the test files in the pinned MariaDB 10.11.7 ARM64 distribution. The
static audit follows literal MTR `source`/`include` files and admits safe variables,
multiple connections, and asynchronous send/reap behavior. It excludes missing results,
custom delimiters, process or file-system side effects, server options and configuration,
topology requirements, storage engines, and behavior outside the compatibility contract.

Each weekly, manual, or relevant push run inventories all 5,585 packaged files and executes every
safely classifiable candidate rather than sampling a rotating batch. The current pinned inventory
contains 319 candidates and 20,082 direct and sourced SQL statements. Each file is validated
against MariaDB first, then run against MySqweel when the external-server baseline is valid. The
workflow publishes the inventory, complete execution reports, and a generated promotion manifest
containing only files that passed both engines. Promotion into the strict manifest still requires
review against the admission rules above.
