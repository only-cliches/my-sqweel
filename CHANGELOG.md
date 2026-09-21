# Changelog

All notable changes to MySqweel will be documented in this file.

## 0.5.0 Unreleased

### Development environment

- Fixed the Harness Hat image's MariaDB initializer wrapper and installed checksum-pinned ncurses ABI-5 compatibility libraries required by the MariaDB 10.11.7 client.
- Added a non-root MariaDB initialization and InnoDB TCP-query check during image builds. Installer downloads and temporary database files are removed on success or failure, and APT caches are cleared after installation.

### MariaDB 10.11.7 compatibility

- Pre-rendered `JSON_ARRAYAGG`/`JSON_OBJECTAGG` results in MariaDB's aggregate style (array elements joined with `,`, object members joined with `, ` as `key:value`) and pass them verbatim over the wire instead of re-serializing them with `JSON_ARRAY`/`JSON_OBJECT` separators.
- Ordered JSON column values by MariaDB's JSON ordering: type rank string < number < array < false < JSON null < true < object (SQL `NULL` sorts first), numbers compared as canonical decimal text, arrays and objects as compact canonical serialization.
- Reported `CUME_DIST`/`PERCENT_RANK` as `DECIMAL` column metadata with ten fractional digits and rounded their values to ten decimal places, matching MariaDB's fixed-point rendering.
- `SHOW INDEX` now emits MariaDB 10.11.7's 14-column output with a trailing `Ignored` column instead of MySQL 8's `Visible`/`Expression` columns.
- Reported historical integer display widths (`INT(11)`, `BIGINT(20)`, ...) in column metadata, preserving explicit widths and `UNSIGNED`/`ZEROFILL` adjustments.
- Parsed `CREATE DATABASE ... CHARACTER SET`/`COLLATE` and tracked the character set per database; databases without modifiers default to the `latin1`/`latin1_swedish_ci` build default, and `SHOW DATABASES` includes `information_schema` with case-insensitive ordering.
- Preserved decimal scale through nested arithmetic around window aggregates and applied declared text-column semantics to `BETWEEN`, fixing `win_insert_select` and `unique` MariaDB MTR parity cases.
- Preserved duplicate projected values while normalizing wire result headers, fixing null-safe equality joins in `func_equal`.
- Added deterministic, GitHub-attributed query-coverage scenarios for NULL-aware outer and self joins, correlated `EXISTS`/`NOT EXISTS` guards, grouped `HAVING`/`COUNT(DISTINCT)`/conditional aggregates/`GROUP_CONCAT`/`ROLLUP`, set operators with `ALL` semantics, decimal comparisons, `NULLIF`/`COALESCE` guarded arithmetic, monthly `DATE_FORMAT` reporting, and ranking, running-total, `LAG`, and `LEAD` windows.
- Added DML and transaction scenarios for grouped and recursive `INSERT ... SELECT`, `INSERT IGNORE`, `REPLACE`, `ON DUPLICATE KEY UPDATE` (including moved keys and aggregate sources), `UPDATE`/`DELETE` joins and ordered `LIMIT` batches, `RETURNING`, `ROLLBACK`, and nested savepoints; each fixture checks deterministic intermediate or final state and includes minimized regressions where needed.
- Added GitHub-attributed differential coverage for `DELETE ... RETURNING` combined with correlated `NOT EXISTS` cleanup guards and nullable returned columns, including final-state checks that preserve held and active rows.
- Added GitHub-attributed differential coverage for an ordered `SELECT ... FOR UPDATE` inside a transaction, paired balance updates, intermediate-state observation, and rollback to the original ledger state.
- Added GitHub-attributed differential coverage for `UNION ALL` candidate pagination through a derived table, preserving duplicate rows, deterministic outer ordering/`LIMIT`, and MariaDB `INT` metadata; fixed integer-literal width inference and derived set-operation metadata propagation, with a minimized regression fixture.
- Added GitHub-attributed differential coverage for a ranked `UNION ALL` badge report combining window `COUNT`/`RANK`/`ROW_NUMBER` results with an aggregate-derived solo-badge branch; MariaDB 10.11.7 and MySqweel matched without a source fix.
- Added GitHub-attributed PostgreSQL-inspired chained `UPDATE ... JOIN` coverage for a two-level depot/region filter with `CASE` assignment and deterministic final-state checks; MariaDB 10.11.7 and MySqweel matched across repeated baseline and differential runs without a source fix.
- Added GitHub-attributed PostgreSQL-inspired case-insensitive category search coverage using explicit `LOWER(...) LIKE LOWER(...)`, NULL-preserving filtering, and deterministic classification; MariaDB 10.11.7 and MySqweel matched across repeated baseline and differential runs without a source fix.
- Parked a separate GitHub-attributed PostgreSQL JSONB `ILIKE` candidate because JSONB operators require a materially different translation; no fixture was retained.
- Added GitHub-attributed recursive CTE coverage for a window-numbered sensor delta walk, including a minimized mixed ordinary/recursive CTE regression; fixed recursive expansion and preserved integer/date metadata through recursive materialization.
- Added GitHub-attributed `NTILE(3)` window coverage across two exam partitions with deterministic tie ordering; MariaDB 10.11.7 and MySqweel matched without a source fix.
- Added GitHub-attributed named-window coverage reusing partition-only, ordered, and explicit-frame definitions across row numbering, counts, and running sums; MariaDB 10.11.7 and MySqweel matched without a source fix.
- Added GitHub-attributed `DATE_ADD`/`EXTRACT` coverage for deterministic maintenance timestamps, including a minimized negative `HOUR_MINUTE` regression; fixed MySQL composite interval parsing and datetime/integer result metadata.
- Added GitHub-attributed `FIELD` custom-order coverage for a deterministic dispatch queue, including unknown and `NULL` states plus a minimized metadata regression; fixed `FIELD` result metadata to match MariaDB's integer type.
- Added GitHub-attributed `SUBSTRING_INDEX` coverage for expanding comma-separated article labels through a derived `UNION ALL` numbers relation, delimiter counting with `CHAR_LENGTH`/`REPLACE`, NULL and empty-list filtering, and deterministic position ordering; MariaDB 10.11.7 and MySqweel matched without a source fix.
- Added GitHub-attributed `JSON_OBJECTAGG` coverage for grouped catalog attribute maps and a minimized two-key metadata/order regression; matched MariaDB 10.11.7's `BLOB` metadata and input-order JSON serialization by preserving aggregate member order and stripping aggregate wire sentinels for non-JSON result metadata.
- Added GitHub-attributed `STDDEV_POP` coverage for nested daily averages and fixed two-day spread buckets, including a minimized aggregate/metadata regression; fixed population-standard-deviation recognition, MariaDB-compatible precision propagation, and `FLOOR` decimal metadata.
- Added GitHub-attributed `BIT_XOR` coverage for grouped device masks with NULL aggregate input, including a minimized aggregate regression; fixed `BIT_XOR` recognition and evaluation through the existing bitwise aggregate path.
- Added GitHub-attributed `TIMESTAMPDIFF(YEAR)` coverage for grouped team-tenure reporting with `COUNT`/`AVG`/`MIN`/`MAX` and a fixed report date, plus a minimized scalar date-difference case; MariaDB 10.11.7 and MySqweel matched without a source fix.
- Added GitHub-attributed `ELT` query coverage for deterministic `INSERT ... SELECT` status snapshots with NULL and out-of-range indexes, including a minimized string-selection regression; fixed `ELT` evaluation in direct and textual scalar paths to match MariaDB.
- Added GitHub-attributed `FIND_IN_SET` coverage for comma-separated report-label membership with positional projections, OR filtering, NULL lists, and empty lists, including a minimized NULL/missing-membership regression; fixed scalar evaluation and integer metadata to match MariaDB.
- Added GitHub-attributed `MAKE_SET` coverage for bitmask-driven profile feature materialization with `UPDATE` final-state checks, NULL label omission, and zero-mask output, including a minimized scalar regression; fixed `MAKE_SET` scalar evaluation and `VAR_STRING` metadata to match MariaDB.
- Added GitHub-attributed `INTERVAL` coverage for decimal shipment SLA brackets with CASE labels and NULL input, including a minimized threshold regression; fixed parser rewriting for both `INTERVAL(` and `INTERVAL (` forms, MariaDB's `-1` result for NULL input, and integer metadata.
- Added GitHub-attributed PostgreSQL-inspired `REGEXP_REPLACE` coverage for nested whitespace/tag normalization over NULL-aware incident notes, including a minimized scalar regression; fixed three-argument evaluation, `LONG_BLOB` metadata propagation through `TRIM`, and wire `LONG_BLOB` support.
- Added GitHub-attributed bounded `ROWS BETWEEN 2 PRECEDING AND CURRENT ROW` window coverage for three-row `AVG`/`SUM` delivery metrics with NULL input and deterministic date ties; MariaDB 10.11.7 and MySqweel matched without a source fix.
- Added GitHub-attributed `SUM(DISTINCT ...)` coverage for grouped campaign spend through a NULL-preserving left join, including duplicate and NULL amount semantics; MariaDB 10.11.7 and MySqweel matched without a source fix.
- Added GitHub-attributed `AVG(DISTINCT ...)` coverage for grouped regional decimal prices through a NULL-preserving left join, including duplicate, NULL, and empty-group semantics; fixed aggregate metadata argument extraction for ordinary `DISTINCT` calls so MariaDB's input-scale-plus-four decimal rendering matches, with a minimized regression.
- Added GitHub-attributed coverage based on `DeNADev/mobage-jssdk-sample-payment` for transactional sequence reservation with `LAST_INSERT_ID(expr)`, reuse in a following insert, and multi-row `ON DUPLICATE KEY UPDATE`; fixed connection-local `LAST_INSERT_ID` state propagation through DML expressions and transaction sessions, with a minimized regression.
- Added GitHub-attributed `BIT_AND`/`BIT_OR` coverage for grouped team permission masks, including NULL aggregate inputs; MariaDB 10.11.7 and MySqweel matched without a source fix.
- Added GitHub-attributed `VAR_POP` coverage for grouped warehouse load variance with DECIMAL input and NULL readings, including the minimized one-group regression; fixed `VAR_POP`/`VARIANCE` metadata to emit MariaDB's `DOUBLE` and input-scale-plus-four fixed-point rendering.
- Added GitHub-attributed `STDDEV_SAMP` coverage for grouped station voltage samples with DECIMAL input and NULL readings, including the minimized one-group regression; fixed sample-standard-deviation evaluation, NULL/single-sample handling, and MariaDB `DOUBLE` metadata.
- Added GitHub-attributed `VAR_SAMP` coverage for grouped hub package weights with DECIMAL input and NULL readings, including the minimized one-group regression; fixed sample-variance evaluation, NULL/single-sample handling, and MariaDB `DOUBLE` metadata.
- Added GitHub-attributed `CUME_DIST` coverage for partitioned regional score distributions with tied DECIMAL scores and NULL ordering, including the minimized one-partition regression; fixed ranking-window metadata to expose MariaDB's `DOUBLE` with ten fractional digits.
- Added GitHub-attributed `JSON_TABLE` coverage for typed DECIMAL discount rows with `FOR ORDINALITY`, `EXISTS PATH` missing-field detection, and CASE arithmetic, including the minimized regression; fixed JSON_TABLE column metadata and numeric CASE metadata to preserve MariaDB's declared types and decimal scale.
- Added GitHub-attributed `REGEXP_SUBSTR` coverage for deterministic first-match incident-reference extraction with NULL input handling; the source's four-argument occurrence form was narrowed because MariaDB 10.11.7 returns error 1582 for that syntax, and MySqweel now supports the compatible two-argument form with MariaDB's `VAR_STRING` metadata.
- Added GitHub-attributed `JSON_ARRAYAGG` coverage for grouped route package labels through an inner join and `CONCAT_WS`, preserving duplicate array members with deterministic route ordering; MariaDB 10.11.7 and MySqweel matched without a source fix.
- Added GitHub-attributed value-window coverage for partitioned DECIMAL readings using `FIRST_VALUE`, `NTH_VALUE`, and `LAST_VALUE` over explicit full `ROWS` frames with NULL ordering; MariaDB 10.11.7 and MySqweel matched without a source fix.
- Added GitHub-attributed `PERCENT_RANK` coverage for a CTE-filtered regional latency report with peer ties, DECIMAL scores, partitioned windows, and a deterministic percentile threshold; MariaDB 10.11.7 and MySqweel matched without a source fix.
- Added GitHub-attributed `COUNT(DISTINCT CASE WHEN ...)` coverage for a LEFT JOIN studio release report with join-multiplication deduplication, null-preserved zero rows, and conditional published/draft counts; MariaDB 10.11.7 and MySqweel matched without a source fix.
- Added GitHub-attributed `LAST_INSERT_ID()` coverage for same-connection generated-key retrieval after an `AUTO_INCREMENT` insert, with final ledger-state checks and a minimized alias-metadata regression; fixed the wire shortcut to preserve explicit scalar-result aliases.
- Added GitHub-attributed seat-run coverage based on `debabratabar/ankit-bansal-sql-playlist` `Q35.sql`, combining partitioned `ROW_NUMBER`, gap grouping, window `COUNT`/`MIN`/`MAX`, and deterministic run filtering; fixed window `MIN`/`MAX` metadata to preserve integer input types and added unsigned `CRC32` metadata, with a minimized regression.
- Parked GitHub-attributed `ANY_VALUE()` grouping coverage because MariaDB 10.11.7 returns error 1305 / SQLSTATE `42000` (`FUNCTION ANY_VALUE does not exist`); no invalid fixture was retained.
- Added GitHub-attributed `JSON_VALUE` coverage for an `UPDATE JOIN` service-node state transition with JSON payload filtering and final-state checks; narrowed the source's `RETURNING` form after MariaDB 10.11.7 returned syntax error 1064 for it, and fixed declared JSON result columns to use MariaDB's `BLOB` wire metadata with a minimized regression.
- Added GitHub-attributed `JSON_CONTAINS`/`JSON_SET` coverage for a filtered JSON document update that copies a scalar through `JSON_EXTRACT`, including a minimized JSON-value-context regression; fixed JSON mutations to preserve `JSON_EXTRACT` results as JSON values instead of quoted text.
- Added GitHub-attributed `GREATEST`/`LEAST` coverage for a nested derived-table rectangle-overlap report with DECIMAL arithmetic, disjoint-shape CASE guarding, and deterministic intersection-over-union ratios; MariaDB 10.11.7 and MySqweel matched without a source fix.
- Added GitHub-attributed `LPAD`/`RPAD` coverage for a deterministic fulfillment-bin formatting report with NULL-aware label padding, implicit numeric-to-text conversion, and trimmed notes; MariaDB 10.11.7 and MySqweel matched without a source fix.
- Added GitHub-attributed `STR_TO_DATE`/`DATE_FORMAT` coverage for a text-date monthly sales report with duplicate-order deduplication, NULL revenue/date filtering, and deterministic month ordering; MariaDB 10.11.7 and MySqweel matched without a source fix.
- Added GitHub-attributed NULL-safe equality (`<=>`) coverage for a nullable composite route-assignment LEFT JOIN, including both-NULL matching, one-sided-NULL non-matches, and deterministic assignment classification; MariaDB 10.11.7 and MySqweel matched without a source fix.
- Added GitHub-attributed `BIT_COUNT` coverage for a permission-mask audit with scalar bit counts, bitwise intersections, NULL propagation, and deterministic CASE classification, including a minimized metadata regression; fixed `BIT_COUNT` result metadata to match MariaDB's integer type.
- Added GitHub-attributed `DATEDIFF`/`LEAD` coverage for deterministic member check-in gap classification with partitioned windows, same-day duplicates, long/short gaps, and terminal NULLs, including a minimized metadata regression; fixed `DATEDIFF` result metadata to match MariaDB's integer type.
- Added GitHub-attributed recursive CTE coverage for a fixed weekday roster using `DATE_ADD`, `DAYNAME`, `DAYOFWEEK`, and CASE weekend classification; MariaDB 10.11.7 and MySqweel matched without a source fix.
- Added GitHub-attributed `TIME_TO_SEC`/`SEC_TO_TIME` coverage for deterministic appointment end-time arithmetic and morning/afternoon classification, including a minimized TIME metadata regression; fixed `SEC_TO_TIME` result metadata to match MariaDB's `TIME` type.
- Added GitHub-attributed `TIMEDIFF` coverage for an independently authored support-session duration report with TIME-column subtraction, `TIME_TO_SEC` thresholds, NULL open-session propagation, and deterministic CASE classification, including a minimized TIME metadata regression; fixed `TIMEDIFF` result metadata to match MariaDB's `TIME` type.
- Added GitHub-attributed `UNIX_TIMESTAMP`/`STR_TO_DATE` coverage for an independently authored event-duration report with epoch arithmetic, midnight-crossing elapsed times, NULL open-event propagation, and deterministic CASE classification, including a minimized integer metadata regression; fixed `UNIX_TIMESTAMP` result metadata to match MariaDB's `LONGLONG` type.
- Added GitHub-attributed `FROM_UNIXTIME` coverage for an independently authored grouped usage-period fallback report with `MAX`, `DATE_ADD`, `IFNULL`, NULL epoch fallback, and deterministic account ordering, including a minimized temporal-type merge regression; fixed `FROM_UNIXTIME` result metadata and temporal `IFNULL`/`COALESCE` merging to match MariaDB's `DATETIME` type.
- Added GitHub-attributed `FROM_DAYS` coverage for an independently authored ordinal-calendar report with date formatting, weekday/weekend classification, NULL propagation, and deterministic ordering, including a minimized DATE metadata regression; fixed `FROM_DAYS` to return MariaDB-compatible date-only values and DATE wire metadata, including NULL arguments.
- Added GitHub-attributed `CRC32` coverage for an independently authored grouped artifact-checksum audit with MAX aggregation, NULL payload propagation, unsigned checksum classification, and deterministic bundle ordering; MariaDB 10.11.7 and MySqweel matched without a source fix.
- Added GitHub-attributed `GET_FORMAT`/`DATE_FORMAT` coverage for an independently authored locale-aware invoice due-date report with USA/EUR/ISO projections, NULL date propagation, CASE classification, and deterministic ordering; MariaDB 10.11.7 and MySqweel matched without a source fix.
- Added GitHub-attributed `LAST_DAY`/`DATE_FORMAT` coverage for an independently authored invoice billing-window report with leap-year and thirty-day month boundaries, NULL propagation, CASE classification, and deterministic ordering; fixed `LAST_DAY` evaluation and DATE wire metadata to match MariaDB 10.11.7.
- Added the minimized GitHub-attributed `LAST_DAY` scalar regression fixture; MariaDB 10.11.7 and MySqweel now both return `2024-02-29` with DATE metadata across repeated fresh differential runs.
- Added GitHub-attributed `FORMAT` coverage for an independently authored invoice amount export with DECIMAL rounding, thousands separators, negative credits, NULL propagation, CASE classification, and deterministic ordering; fixed `FORMAT` evaluation and VARCHAR wire metadata to match MariaDB 10.11.7.
- Added the minimized GitHub-attributed `FORMAT` scalar regression fixture; MariaDB 10.11.7 and MySqweel now both return `1,234,567.46` with VARCHAR metadata across repeated fresh differential runs.
- Added GitHub-attributed `INET_ATON` coverage for an independently authored IPv4 allocation audit with numeric conversion, malformed and NULL input, unsigned integer output, CASE classification, and deterministic ordering; fixed `INET_ATON` evaluation and LONGLONG metadata to match MariaDB 10.11.7.
- Added the minimized GitHub-attributed `INET_ATON` scalar regression fixture; MariaDB 10.11.7 and MySqweel now both return `3221226001` with LONGLONG metadata across repeated fresh differential runs.
- Added GitHub-attributed `EXPORT_SET` coverage for an independently authored service-account permission audit with low-order bit serialization, explicit separators and width, NULL propagation, CASE classification, and deterministic ordering; fixed `EXPORT_SET` evaluation and VARCHAR wire metadata to match MariaDB 10.11.7.
- Added the minimized GitHub-attributed `EXPORT_SET` scalar regression fixture; MariaDB 10.11.7 and MySqweel now both return `grant|deny|grant|deny` with VARCHAR metadata across repeated fresh differential runs.
- Added GitHub-attributed `OCTET_LENGTH`/`CHAR_LENGTH` coverage for an independently authored UTF-8 message-length audit with ASCII, accented, CJK, empty, and NULL text, CASE classification, and deterministic ordering; fixed integer result metadata for `OCTET_LENGTH`, `CHAR_LENGTH`, and `CHARACTER_LENGTH` to match MariaDB 10.11.7.
- Added the minimized GitHub-attributed `OCTET_LENGTH` scalar regression fixture; MariaDB 10.11.7 and MySqweel now both return `5` with `MYSQL_TYPE_LONG` metadata across repeated fresh differential runs.
- Added GitHub-attributed `STRCMP` coverage for an independently authored catalog-label audit with three-way string comparison, NULL propagation, CASE classification, and deterministic ordering; fixed `STRCMP` integer wire metadata to match MariaDB 10.11.7.
- Added the minimized GitHub-attributed `STRCMP` scalar regression fixture; MariaDB 10.11.7 and MySqweel now both return `-1` with `MYSQL_TYPE_LONG` metadata across repeated fresh differential runs.
- Added GitHub-attributed `BIT_LENGTH` coverage for an independently authored UTF-8 document-fragment audit with ASCII, accented, emoji, empty, and NULL text, character-count comparison, CASE classification, and deterministic ordering; MariaDB 10.11.7 and MySqweel matched without a source fix.
- Added GitHub-attributed `CONV` coverage for an independently authored shipment-label radix audit with binary, decimal, octal, hexadecimal, signed-target, zero, and NULL conversions; fixed scalar base conversion evaluation and VARCHAR wire metadata to match MariaDB 10.11.7.
- Added the minimized GitHub-attributed `CONV` scalar regression fixture; MariaDB 10.11.7 and MySqweel now both return `26` with `MYSQL_TYPE_VAR_STRING` metadata across repeated fresh differential runs.
- Added GitHub-attributed `TO_BASE64`/`FROM_BASE64` coverage for an independently authored message-payload audit with ASCII, UTF-8, empty, and NULL round trips plus deterministic classification; fixed scalar base64 evaluation, binary round-tripping, and MariaDB-compatible `MEDIUM_BLOB` metadata.
- Added the minimized GitHub-attributed `TO_BASE64` scalar regression fixture; MariaDB 10.11.7 and MySqweel now both return `aGVsbG8=` with `VAR_STRING` metadata across repeated fresh differential runs.
- Added GitHub-attributed `QUOTE`/`CONCAT_WS` coverage for an independently authored role-grant preview audit with apostrophe and backslash escaping, empty and NULL identifiers, CASE classification, and deterministic ordering; fixed SQL-literal quoting and `VAR_STRING` metadata to match MariaDB 10.11.7.
- Added the minimized GitHub-attributed `QUOTE` scalar regression fixture; MariaDB 10.11.7 and MySqweel now both return `'o\\'connor'` with `VAR_STRING` metadata across repeated fresh differential runs.
- Added GitHub-attributed `HEX`/`UNHEX` coverage for an independently authored binary-payload normalization audit with canonical uppercase round trips, decoded byte lengths, odd and invalid hex, empty and NULL inputs, CASE classification, and deterministic ordering; fixed binary-aware `LENGTH`/`OCTET_LENGTH` and MariaDB's leading-zero padding for odd-length `UNHEX` input.
- Added the minimized GitHub-attributed odd-length `UNHEX` regression fixture; MariaDB 10.11.7 and MySqweel now both return `0ABC` and `2` with `VAR_STRING`/`LONG` metadata across repeated fresh differential runs.
- Added GitHub-attributed `MONTHNAME`/`QUARTER`/`DAYOFYEAR`/`WEEKDAY` coverage for an independently authored financial milestone calendar report with leap-day and quarter-boundary extraction, year-end classification, NULL timestamps, and deterministic ordering; fixed date-component result metadata to match MariaDB 10.11.7's `MYSQL_TYPE_LONG`.
- Added the minimized GitHub-attributed `QUARTER` metadata regression fixture; MariaDB 10.11.7 and MySqweel now both return `1` with `MYSQL_TYPE_LONG` metadata across repeated fresh differential runs.
- Added GitHub-attributed `TIMESTAMPDIFF(MINUTE)` coverage for an independently authored grouped exam-session duration audit with positive, negative, zero, and NULL durations, `AVG`/`MIN`/`MAX`/`COUNT` aggregates, anomaly classification, and deterministic ordering; MariaDB 10.11.7 and MySqweel matched without a source fix.
- Added GitHub-attributed `UPDATE ... JOIN` coverage for an independently authored monitor-event rollup using a grouped derived aggregate, conditional `SUM(IF(...))`, `COALESCE`, and deterministic final-state checks; fixed UPDATE JOIN execution to materialize supported derived table factors and match MariaDB 10.11.7.
- Added the minimized GitHub-attributed grouped-derived-aggregate `UPDATE ... JOIN` regression fixture; MariaDB 10.11.7 and MySqweel now both update the sentinel rollup to `2` across repeated fresh differential runs.
- Added GitHub-attributed `YEARWEEK(..., 3)`/`WEEKOFYEAR` coverage for an independently authored ISO week boundary audit with week 53, year transitions, NULL propagation, CASE classification, and deterministic ordering; fixed ISO week evaluation and integer wire metadata to match MariaDB 10.11.7.
- Added the minimized GitHub-attributed ISO week scalar regression fixture; MariaDB 10.11.7 and MySqweel now both return `202053` and `53` across repeated fresh differential runs.
- Added GitHub-attributed `CAST(... AS DECIMAL)` coverage for an independently authored settlement ledger with grouped decimal totals, textual numeric conversion, negative half-step rounding, NULL-only groups, and deterministic ordering; fixed exact fixed-scale DECIMAL rounding to match MariaDB 10.11.7.
- Added the minimized GitHub-attributed negative DECIMAL cast regression fixture; MariaDB 10.11.7 and MySqweel now both return `-5.00` with `MYSQL_TYPE_NEWDECIMAL` metadata across repeated fresh differential runs.
- Added GitHub-attributed grouped daily moving-window coverage from a Transact-SQL time-series pattern, independently authored as a MariaDB CTE that aggregates sparse regional shipment days before partitioned two-row `AVG`/`SUM` windows; MariaDB 10.11.7 and MySqweel matched across repeated fresh differential runs without a source fix.
- Added GitHub-attributed `JSON_UNQUOTE`/`JSON_EXTRACT` coverage for an independently authored filtered asset-region update with nested paths, empty and missing JSON-value guards, and deterministic final-state checks; fixed JSON scalar comparison typing and `JSON_EXTRACT` wire rendering to match MariaDB 10.11.7.
- Added the minimized GitHub-attributed empty-string JSON filter regression fixture; MariaDB 10.11.7 and MySqweel now both preserve the preexisting `legacy` region and match across repeated fresh differential runs.
- Added GitHub-attributed `INET6_ATON`/`UNHEX` coverage for an independently authored IPv4/IPv6 interface normalization update with a `LEFT JOIN`, binary MAC conversion, nullable address handling, unknown-class preservation, and deterministic final-state checks; fixed IPv4/IPv6 binary conversion, `VARBINARY` length accounting, and result metadata to match MariaDB 10.11.7.
- Added the minimized GitHub-attributed `INET6_ATON` scalar regression fixture; MariaDB 10.11.7 and MySqweel now both return `20010DB8000000000000000000000042` across repeated fresh differential runs.
- Added GitHub-attributed recursive hashtag extraction coverage for an independently authored dated message-tag report using `WITH RECURSIVE`, `SUBSTRING_INDEX`, `LOCATE`, `REVERSE`, grouped counts, and deterministic tie ordering; MariaDB 10.11.7 and MySqweel matched across repeated fresh differential runs without a source fix.
- Added GitHub-attributed `INSERT ... SELECT` coverage from a derived `UNION ALL` source with scalar `JSON_ARRAY` values, `ON DUPLICATE KEY UPDATE`, a secondary unique-key conflict, and deterministic semantic JSON checks; fixed `JSON_ARRAY`, `JSON_LENGTH`, and `JSON_UNQUOTE` metadata plus direct JSON-array wire rendering to match MariaDB 10.11.7.
- Added the minimized GitHub-attributed `JSON_ARRAY` scalar regression fixture; MariaDB 10.11.7 and MySqweel now both return the spaced JSON array with `MYSQL_TYPE_VAR_STRING` metadata across repeated fresh differential runs.
- Parked the GitHub-attributed `VALUES(...)` upsert seed candidate from `tsxcw/mtab` because existing coverage already exercises multi-row and `INSERT ... SELECT` `ON DUPLICATE KEY UPDATE` semantics; no duplicate fixture was retained.
- Added GitHub-attributed `JSON_ARRAYAGG(DISTINCT ...)` coverage for an independently authored region/route scan summary with `COUNT(DISTINCT ...)`, a `LEFT JOIN`, conditional delivered-scan counts, and deterministic grouping; MariaDB 10.11.7 and MySqweel matched across repeated fresh differential runs without a source fix.
- Parked the GitHub-attributed `LOCATE`/`INSTR` candidate from `DataLinkDC/dinky` because the matching rows were Flink planner UDF documentation inserts rather than executable MySQL/MariaDB query coverage; no fixture was retained.
- Parked the GitHub-attributed `UUID_TO_BIN` seed-data candidate because MariaDB 10.11.7 returns error 1305 / SQLSTATE `42000` (`FUNCTION UUID_TO_BIN does not exist`); no invalid fixture was retained.
- Parked the GitHub-attributed `JSON_STORAGE_SIZE`/`JSON_STORAGE_FREE` payload-audit candidate because MariaDB 10.11.7 returns error 1305 / SQLSTATE `42000` for `JSON_STORAGE_SIZE`; no invalid fixture was retained.
- Parked the GitHub-attributed `REGEXP_LIKE` email-validation candidate because MariaDB 10.11.7 returns error 1305 / SQLSTATE `42000` for `REGEXP_LIKE`; no invalid fixture was retained.
- Parked GitHub-attributed `GROUPING()` plus `GROUP BY ... WITH ROLLUP` coverage from the MariaDB seed query because MariaDB 10.11.7 returns error 1305 / SQLSTATE `42000` for `GROUPING()`; no invalid fixture was retained.
- Parked an `UPDATE ... RETURNING` coverage candidate because MariaDB 10.11.7 returns syntax error 1064 for that form; no invalid fixture was retained.
- Parked the GitHub-attributed `SHA2` candidate from `DataLinkDC/dinky` because the matching rows were Flink function-documentation seed inserts rather than executable MySQL/MariaDB query coverage; no duplicate fixture was retained.
- Added GitHub-attributed `SHA2` coverage for an independently authored credential-event import with variable 224/256/384/512-bit digest lengths, `INSERT ... SELECT`, and NULL propagation; fixed SHA2 hash-length dispatch to match MariaDB 10.11.7, including a minimized scalar regression.
- Added GitHub-attributed `JSON_TYPE` coverage for an independently authored payload audit spanning object, array, string, integer, decimal, boolean, JSON `null`, and SQL `NULL` values with deterministic `UPDATE` final state; fixed boolean type classification and no-op `UPDATE` affected-row accounting to match MariaDB 10.11.7.
- Added the minimized GitHub-attributed `JSON_TYPE('true')` scalar regression; MariaDB 10.11.7 and MySqweel now both return `BOOLEAN` across repeated fresh differential runs.
- Parked the GitHub-attributed `GROUP_CONCAT ... SEPARATOR` candidate from `Miazzy/oa-front-service` because existing `group_concat_project_tags.json` already covers the same ordered grouped concatenation semantics; no duplicate fixture was retained.
- Parked the GitHub-attributed `BoardGameArchive` candidate because its second query mixes SQL Server-only `TOP`, `GETDATE()`, `DATEDIFF`, and `DATEADD` forms with the MySQL-labelled source; translating it would change semantics, so no fixture was retained.
- Added GitHub-attributed CTE qualification coverage for an independently authored member-activity report combining distinct category counts across multiple `LEFT JOIN`s, `HAVING`, a date-filtered CTE, and an `IN` semi-join; MariaDB 10.11.7 and MySqweel matched across repeated fresh differential runs without a source fix.
- Parked the GitHub-attributed `INSERT ... SELECT ... WHERE NOT EXISTS` candidates from Cloudberry/InferX because the matching files depend on PostgreSQL/Cloudberry types, extensions, and partial-index syntax rather than executable MySQL/MariaDB coverage; no fixture was retained.
- Parked the GitHub-attributed `REGEXP_LIKE` candidate from `fishercoder1534/Leetcode` because MariaDB 10.11.7 does not provide `REGEXP_LIKE`; the compatible `REGEXP` operator candidate was retained instead.
- Added GitHub-attributed `REGEXP` coverage for an independently authored support-ticket routing update with word-boundary token matching, CASE precedence, NULL preservation, and deterministic final-state checks; fixed REGEXP predicate evaluation and MariaDB integer result metadata to match 10.11.7.
- Added the minimized GitHub-attributed scalar `REGEXP` word-boundary regression; MariaDB 10.11.7 and MySqweel now both return integer `1` across repeated fresh differential runs.
- Parked the GitHub-attributed executable `CHECK (... REGEXP ...)` candidate from `devcamps/camps` because existing `ticket_code_routing.json` already covers MariaDB-compatible `REGEXP` predicate evaluation; no duplicate fixture was retained.
- Added GitHub-attributed CTE-backed `DELETE` coverage for an independently authored stale-job archive cleanup using a date-filtered CTE inside an `IN` subquery; the MariaDB-compatible translation preserves completed-before-cutoff, non-completed, and NULL-date rows, and MariaDB 10.11.7 matched MySqweel across repeated fresh differential runs without a source fix.
- Added GitHub-attributed MariaDB QA coverage for independently authored `INSERT ... SET` upserts combining `ON DUPLICATE KEY UPDATE`, column `DEFAULT` assignments, and `RETURNING` on both conflict and insert paths; fixed parser normalization to preserve the duplicate clause and materialize column defaults in duplicate updates.
- Added the minimized GitHub-attributed `INSERT ... SET ... ON DUPLICATE KEY UPDATE ... RETURNING` regression; MariaDB 10.11.7 and MySqweel now return and persist the defaulted value across repeated fresh differential runs.
- Added GitHub-attributed temporary-table staging coverage for an independently authored dispatch rollup combining `CREATE TEMPORARY TABLE ... SELECT`, grouped aggregation, `TRUNCATE`, `INSERT ... SELECT`, and `DROP TEMPORARY TABLE`; fixed CTAS affected-row reporting to match MariaDB 10.11.7.
- Added the minimized GitHub-attributed temporary CTAS regression; MariaDB 10.11.7 and MySqweel now both report one materialized row across repeated fresh differential runs.
- Added GitHub-attributed numeric `RANGE` window coverage for an independently authored warehouse movement audit with peer-date frames, partitioned `SUM`/`COUNT`, NULL input handling, and deterministic ordering; MariaDB 10.11.7 and MySqweel matched without a source fix.
- Parked the GitHub-attributed `CREATE TABLE ... LIKE` search candidate because the fetched executable patterns were ordinary CTAS or documentation, while existing temporary CTAS coverage already exercises materialized-table semantics; no duplicate fixture was retained.
- Parked the GitHub-attributed `MATCH ... AGAINST` candidate from [bheckel/code](https://github.com/bheckel/code/blob/0df855b47761ae4f2fc6253ad6a3718b80b0e630/database/mysql.sql) at commit `0df855b47761ae4f2fc6253ad6a3718b80b0e630`: MariaDB 10.11.7 returned indexed natural-language relevance rows, while MySqweel returned error 1235 / SQLSTATE `42000`; full-text indexing/search requires storage architecture, so no fixture was retained.
- Added GitHub-attributed UNION segmentation coverage from [Prashant-4527/mercaridb-mysql-30days](https://github.com/Prashant-4527/mercaridb-mysql-30days/blob/f72849f9ba3414412387bd9930b703258bed2b80/day12_union_intersect_except.sql) for an independently authored member engagement report with mutually exclusive correlated `EXISTS`/`NOT EXISTS` branches; MariaDB 10.11.7 and MySqweel matched across repeated fresh differential runs without a source fix.
- Parked the GitHub-attributed `UPDATE ... JOIN ... ORDER BY` search candidate from [avvrnk/BazyDanych](https://github.com/avvrnk/BazyDanych/blob/1d4fe6af2ce58f9305d2cb1edb9c0359bbdcc7ed/update.sql) because the fetched executable content was a standalone `UPDATE` plus separate ordered `SELECT` statements, while existing update-join and update-order-limit fixtures already cover the semantics; no duplicate fixture was retained.
- Parked the GitHub-attributed `FIELD(...)` custom-order search candidate from [TheDataDisciple/mysql-topics](https://github.com/TheDataDisciple/mysql-topics/blob/a8ccd328e7ce0272e5b36b90c2a63e16829db176/MySQL%20Tutorial.com/order%20by.sql) because existing `field_priority_dispatch.json` already covers custom ordering, rank metadata, and unknown/NULL behavior; no duplicate fixture was retained.
- Added GitHub-attributed `JSON_MERGE_PATCH` coverage from [zhao1jin4/Record](https://github.com/zhao1jin4/Record/blob/560eef9486626c34c7c6b37d0f944c5f3a2b8aab/Record/Program-Record/MySQL_Devlop.sql) for an independently authored nested preference-patch audit covering object updates, array replacement, JSON-null key removal, and path extraction; fixed `JSON_CONTAINS_PATH` result metadata to report MariaDB's integer column type.
- Added the minimized GitHub-attributed JSON merge-patch metadata regression; MariaDB 10.11.7 and MySqweel now both report `MYSQL_TYPE_LONG` for the merged-key path predicate across repeated fresh differential runs.
- Parked the GitHub-attributed `JSON_STORAGE_SIZE` candidate from [opengauss-mirror/Plugin](https://github.com/opengauss-mirror/Plugin/blob/15611cfb458bdddf4b36043ff4f6e62ef9d1213a/contrib/dolphin/sql/json_storage_size.sql) because MariaDB 10.11.7 returned error 1305 (`42000`, function unavailable) in both repeated baselines; no fixture was retained.
- Added GitHub-attributed `JSON_OVERLAPS` coverage from [ZhiQingWu00/special](https://github.com/ZhiQingWu00/special/blob/283b45983920f3e09a5bd44177c6b6290555fa7d/mysql_design.sql) for an independently authored array/object overlap audit; fixed `JSON_OVERLAPS` result metadata to report MariaDB's integer column type.
- Added the minimized GitHub-attributed JSON overlap metadata regression; MariaDB 10.11.7 and MySqweel now both report `MYSQL_TYPE_LONG` across repeated fresh differential runs.
- Added GitHub-attributed CTE/window coverage from [wenshao/sql-dialects](https://github.com/wenshao/sql-dialects/blob/acc6698bf87709c16156862d295b2a4bb290cec1/query/cte/mysql.sql) for an independently authored monthly billing-growth audit combining three CTE layers, `DATE_FORMAT`, `SUM` over `DECIMAL`, `LAG`, and rounded percentage arithmetic; fixed `ROUND` metadata to preserve its requested decimal scale.
- Added the minimized GitHub-attributed monthly-growth decimal-scale regression; MariaDB 10.11.7 and MySqweel now both report `MYSQL_TYPE_NEWDECIMAL` values with two fractional digits across repeated fresh differential runs.
- Added GitHub-attributed `UPDATE ... JOIN` coverage from [wenshao/sql-dialects](https://github.com/wenshao/sql-dialects/blob/acc6698bf87709c16156862d295b2a4bb290cec1/dml/update/mysql.sql) for an independently authored account-status refresh combining joined filtering, CASE assignments, decimal comparisons, and a NULL branch; MariaDB 10.11.7 and MySqweel matched across repeated fresh differential runs without a source fix.
- Added GitHub-attributed `INSERT ... RETURNING` coverage from [Dicklesworthstone/sqlmodel_rust](https://github.com/Dicklesworthstone/sqlmodel_rust/blob/5017764ba8b26da23afd6c9c5b06384b4e7f2236/crates/sqlmodel-e2e/golden/mysql/insert_returning.sql) for an independently authored multi-row stock-receipt intake with explicit returned columns and nullable data; MariaDB 10.11.7 and MySqweel matched across repeated fresh differential runs without a source fix.
- Added GitHub-attributed `NATURAL JOIN` coverage from [wenshao/sql-dialects](https://github.com/wenshao/sql-dialects/blob/acc6698bf87709c16156862d295b2a4bb290cec1/query/joins/mysql.sql) for an independently authored warehouse-bin report that matches on multiple shared columns; fixed column-scope validation to merge NATURAL JOIN columns like `USING`.
- Added the minimized GitHub-attributed NATURAL JOIN regression; MariaDB 10.11.7 and MySqweel now both return the shared columns and reject mismatched multi-column pairs across repeated fresh differential runs.
- Added GitHub-attributed `REGEXP_INSTR` coverage from [OpenTenBase/OpenTenBase](https://github.com/OpenTenBase/OpenTenBase/blob/b612d77cbfd4d762f20c54c35f7caf09d57ef098/contrib/opentenbase_ora_package_function/sql/regexp_instr.sql) for an independently authored incident-code position report with match, no-match, and NULL behavior, including a minimized scalar regression; fixed two-argument `REGEXP_INSTR` evaluation and MariaDB `MYSQL_TYPE_LONG` metadata.
- Added GitHub-attributed `JSON_KEYS` coverage from [HTTPArchive/almanac.httparchive.org](https://github.com/HTTPArchive/almanac.httparchive.org/blob/e835299e6d9f6cc66188064c5d566006d493f521/sql/2025/accessibility/common_aria_role.sql) for an independently authored role-usage report combining JSON object-key extraction, key counts, and JSON path projections, including a minimized scalar metadata regression; preserved JSON object key order, matched MariaDB's `VAR_STRING`/`LONG_BLOB` result metadata by argument type, and reproduced MariaDB's spaced JSON_KEYS wire rendering.
- Added GitHub-attributed `JSON_SEARCH` coverage from [composer/packagist](https://github.com/composer/packagist/blob/ab32b23d8a3c6b6c48e6776b0cf117700a08e003/migrations/2026_06_user_frozen.sql) for an independently authored guarded role-cleanup update combining `JSON_SEARCH`, `JSON_UNQUOTE`, `JSON_REMOVE`, and `JSON_CONTAINS`, including a minimized scalar metadata regression; fixed `JSON_CONTAINS` result metadata to report MariaDB's `MYSQL_TYPE_LONG`.

### Upstream corpus coverage

- Inventory every packaged MariaDB `.test` path, including flat suites, plugins, nested layouts, and helpers, with explicit exclusions instead of silently omitting layouts. Distinguish window `PARTITION BY` from physical table partitioning, including executable comments and mixed statements.
- Add a separate upstream-derived scenario runner with immutable source provenance, full-file hashes, and byte-preserving contiguous test/result ranges. Derived scenarios cannot inflate complete-file coverage or enter complete-file promotion manifests.
- Retain all fourteen newly exposed window files in the focused audit, including thirteen known MySqweel failures. Add repeatedly passing `win_percent_cume`, `win_std`, and `innodb/innodb_bug57255` to the merged strict gate.
- Report SQL mismatches, unsupported execution, baseline failures, infrastructure failures, and unexecuted cases separately. Require an actual completed MTR pass, preserve setup-failure reports, and broaden discovery CI triggers to engine and wire changes.
- Restore six-decimal `UNIX_TIMESTAMP` rendering for dynamic text arguments without deriving precision from the first result row; preserve integer results for whole-second literals and `STR_TO_DATE` formats. Cover empty results and a NULL first row with a regression.
- Separate upstream semantic scope, required testing intent, harness eligibility, and execution evidence. Enroll all 7,903 inventoried files, retaining 7,901 as required and allowing only two explicitly reviewed, hash-pinned exemptions; keep mixed and unresolved files visible as blocked work.
- Generate an exhaustive per-path testing plan and validate it in discovery CI. Reject missing entries, stale pins, unreviewed exemptions, missing selected outcomes, and inconsistent reports; bind partial derived observations to their exact source and byte-range provenance without claiming whole-file coverage.
- Restore native MTR baseline prerequisites in CI: official helper procedures, pinned timezone data, Performance Schema instrumentation, and socket-based `root@localhost` authentication. Keep MTR's empty-password test account confined to disposable services with loopback-only published ports; retain password authentication for feature parity.
- Classify nested-include mysqltest SQL diagnostics without misreporting unsupported SQL or wrong error codes as infrastructure failures. Keep startup failures and incomplete runs gating, and avoid nondeterministic TCP fallback when MTR's external feature probe receives socket options.

## 0.4.4 Sep 9, 2026

### Transactions and persistence

- Added independent `Engine::session()` connections with atomic statements, `BEGIN`, `COMMIT`, `ROLLBACK`, savepoints, autocommit handling, and rollback on disconnect. A failed statement preserves earlier successful work in its transaction without publishing partial row/index changes.
- Added committed-state reads and per-database writer leases covering transactions and `SELECT ... FOR UPDATE`. The supported isolation level is `REPEATABLE READ`; writer acquisition times out after five seconds. Independent database commits merge under a serialized publication lock; catalog administration remains globally exclusive. DDL and catalog administration inside a transaction are rejected.
- Deferred transaction snapshots until the first read. First writes refresh unobserved state; upgrades validate observed rows, columns, and schema and merge unrelated committed changes. Prewrite savepoints follow the refreshed state, and read-only commits cannot republish stale data.
- Replaced whole-server commit images with locked, incremental embedded Lux persistence. Commits write changed rows, schemas, auto-increment counters, views, and index comments plus the database/account catalog; unchanged databases and shared, unmodified tables are skipped. SQL state is published only after the storage batch succeeds.
- Added the version-2 Lux storage layout and private data-directory permissions on Unix. Legacy transaction-image files, nonempty unversioned Lux stores, and unsupported storage versions are rejected with reset guidance; there is no migration path.
- SQL statement atomicity and session isolation remain in memory; persistence does not promise crash-atomic multi-key commits or synchronous durability. Storage batches can partially apply before an error; command and I/O failures retain the previous visible SQL state and block further operations until reopen. Reopening does not guarantee recovery of an entire transaction.
- Statement working copies share immutable table rows, indexes, and schema metadata, detaching only modified values. Writes can still copy an entire affected table, and persistence compares changed tables linearly, so large single-table workloads remain a limitation. There is no XA, replication, fine-grained row lock manager, or production concurrency qualification.
- Made Lux startup and shutdown safe inside an existing Tokio runtime, and propagated command-level errors returned within storage pipelines. Added regressions for incremental deltas, storage-error handling, legacy-image rejection, directory locking, and async-runtime reopen, plus an ignored manual DDL benchmark.

### Performance

- Replaced eager row, index, and schema copies with copy-on-write sharing across statement working copies and transaction snapshots. Shared-table identity also avoids unnecessary snapshot comparisons and persistence scans of unchanged tables.
- Reused the parsed-SELECT syntax cache across statement working copies; plans, evaluated results, and session values are not cached. Reduced private directory shard allocations and construct working copies directly instead of initializing a fresh storage-backed engine.
- Added exact primary-key lookups for foreign-key validation, including composite keys, with a scan fallback to preserve case-insensitive and numeric-coercion matches. Release directory guards before inspecting related tables so foreign-key operations work when parent and child share a shard.
- Added regressions for shared-state detachment, schema and query-cache behavior after DDL, foreign-key lookup/coercion and shard collisions, failed multirow updates, and repeated savepoint rollback of rows and indexes. Added an ignored manual benchmark for private-directory shard allocation costs.
- Reduced bulk-INSERT parsing and allocation work: command recognizers skip clearly unrelated leading keywords, authorization tracks qualifier rewrites without cloning the entire statement, and VALUES execution borrows parsed expressions. Commented or ambiguous prefixes still use full parsing, and trailing-command rejection remains enforced.
- Avoided collecting returned-row copies when INSERT or upsert has no RETURNING clause and skipped redundant per-statement persistence buffers for private transaction state. Committed changes still flow through the coordinator’s incremental persistence path.
- Limited information-schema column construction to the requested table for supported literal `table_name = '...'` predicates, including conditions joined by AND. Case matching and the full predicate evaluation are retained; OR and row-dependent expressions continue through ordinary filtering.
- Replaced the wire listener’s 50 ms accept polling with socket-readiness notifications. The timer now checks shutdown, and the blocking listener API uses a separate worker when called inside an existing Tokio runtime.
- Added regressions for INSERT/upsert RETURNING and rollback, metadata filtering with case differences and OR/row expressions, qualifier normalization, commented commands and trailing-command rejection, and listener shutdown/port release inside Tokio. Added ignored manual benchmarks for command recognition and metadata filtering.

### Databases, accounts, and wire sessions

- Added connection-owned advisory locks for the development provisioner, scoped database catalog enumeration, and the `8.0.0-my-sqweel` version identity. `VERSION()`, session defaults, and variable metadata now use this shorter identifier consistently.
- Corrected Drizzle introspection ordering, empty result metadata, primary-key labels, and distinct named indexes over the same columns. Snapshot upgrades preserve concurrent committed data; stale observed snapshots abort with retryable error 1213.
- Avoided index reconstruction during private state copies and disabled TCP Nagle buffering for local request/response SQL traffic.
- Added independent logical databases, bootstrap administrator credentials, MySQL native-password verification, and database-wide `SELECT`, `INSERT`, `UPDATE`, and `DELETE` grants. Fresh local engines default to `root` with an empty password; embedders can call `set_admin_credentials()` before accepting connections.
- Added the provisioning subset of `CREATE/DROP DATABASE`, `CREATE/DROP USER`, `GRANT`, and `REVOKE ALL PRIVILEGES`. Revocation affects existing sessions, and replacing an account requires fresh authentication. SQL cannot grant administrator privileges.
- Enforced the selected-database boundary across nested queries and schema references. Cross-database SQL and switching databases inside a transaction are rejected. SQL accounts accept the `%` host form only; this is not the complete MySQL permissions system.
- Made session settings and transaction status connection-owned; unsupported global/integrity settings fail explicitly. Unsupported wire commands, including `COM_RESET_CONNECTION`, return an error rather than pretending to reset state; reconnect instead.
- Integrated the updated `vendor/msql-srv` dependency and retained warning-count support alongside connection-owned transaction flags in OK and EOF packets, including prepared-statement results.
- Added focused transaction, wire, database-isolation, account-persistence, and storage-recovery tests.

### Administrative and diagnostic surfaces

- Routed HTTP maintenance and search through committed default-`app` database state. These remain trusted administrative surfaces; SQL account grants do not authenticate or restrict HTTP callers.
- Clarified that query lifecycle events are diagnostics, including statements in transactions that can subsequently roll back. They are not commit notifications or safe external-effect triggers.

### Fixed

- Preserved explicitly selected information-schema column names over prepared wire queries and restored authorization for table-level `ALTER TABLE ... AUTO_INCREMENT`, rename-with-`DISABLE KEYS`, prefix-key ALTER operations, and `CREATE VIEW IF NOT EXISTS` without bypassing database, privilege, or transaction restrictions.

- Added a validated `--default-time-zone` startup offset inherited by new sessions, and pass each upstream MTR case’s required timezone when launching MySqweel. This fixes non-UTC cases such as `timezone4` without enabling global SQL settings or editing upstream tests.

- Fixed unique-constraint name rewriting so identifiers ending in `unique` retain their explicit names in index metadata; updated empty-column metadata coverage to include `collation_name`.

- Restored supported MariaDB syntax through transaction authorization, including `CHECK TABLE`, `CREATE OR REPLACE INDEX`, `EXPLAIN FORMAT=JSON`, `INSERT`/`REPLACE ... SELECT ... RETURNING`, and user-variable assignments inside expressions. These statements retain privilege checks, cross-database rejection, and transaction DDL restrictions.
- Preserved the original SQL through parser-only rewrites so interval conversion warnings reach clients. Retained `IF EXISTS` when normalizing index drops and report note 1091 for missing indexes.
- Added connection-owned `optimizer_switch` compatibility settings and `SET NAMES` handling.
- Corrected `STD`/`STDDEV` window metadata for text and approximate inputs and return MariaDB error 4014 (`HY000`) for invalid window-frame bounds.

### Compatibility and CI

- Embedded the patched wire-server implementation and its upstream licenses in the published crate, fixing package builds that previously substituted the unpatched registry dependency. Added unpacked-crate verification to the pre-push and CI checks.

- Enabled upstream discovery of basic transactions, autocommit, and savepoints while retaining exclusions for unsupported isolation levels, table/shared locks, and XA. Safe-harness discovery admits reviewed InnoDB cases without opening the entire engine suite. The pinned inventory now contains 319 candidates and 20,082 direct and sourced SQL statements across 5,585 inspected files.
- Added the complete, hash-pinned `innodb/innodb_bug57255` transaction case to the focused MTR audit. Its 18 direct SQL statements include a transaction with 743 inserted rows followed by cascading deletes. It passes locally against both MariaDB 10.11.7 and MySqweel and remains audit-only pending CI qualification and strict-manifest promotion.
- Fixed MTR startup for external servers without a selectable `mysql` database by staging a runner copy whose feature probe uses `SHOW VARIABLES` without `USE mysql`. Both engines use the same narrowly checked adaptation; upstream test/result files and the official `mysqltest` binary are unchanged. Per-invocation source and adapted runner hashes are recorded and uploaded with CI artifacts.
- Updated MTR setup to preserve MySqweel's default `app` database, recreate the separate `test` database, and avoid unsupported global settings. Discovery also runs when transaction backend and wire tests change.
- Updated external-server timezone handling for transactional sessions, which reject global settings. Hash-pinned tests with fixed POSIX `GMT` offsets apply the equivalent SQL timezone to MariaDB. For MySqweel, the runner verifies the session default and fails explicitly if it differs from the required timezone.
- Removed environment-dependent `information_schema.schemata` parity assumptions by creating an explicit `utf8mb4` fixture database and cleaning it up after the test.

### Verification status
- Requalified the current 0.5.0 repair locally with MariaDB parity required: 58 Python harness tests, all Rust targets, and publishable-crate verification pass. Targeted probes for `win_insert_select`, `unique`, and `func_equal` match MariaDB; the strict full-suite MTR run remains a CI gate.

- The results below predate the current storage, execution, command-dispatch, metadata, and wire-listener changes. They remain historical checkpoints; the current working tree has not been requalified by these runs.

- The focused upstream audit contains 25 complete files and 339 direct SQL statements. MariaDB 10.11.7 and the transactional MySqweel backend both pass all 25 files and 339 statements locally, with no infrastructure failures. All 12 previously failing focused-audit files now pass; this does not claim a full MariaDB-suite or strict-manifest CI qualification.
- Verified the complete local pre-push check with MariaDB parity required: 281 Rust tests, 29 Python harness tests, and all benchmark targets passed. This includes the ORM introspection regression and transaction authorization checks.
- Reverified the complete strict manifest locally against MariaDB 10.11.7 and the transactional backend: all 32 files and 381 statements pass on both engines, with zero infrastructure failures. This is a local verification result, not a claim about a subsequent CI run.

## 0.4.3 Aug 24, 2026

### Compatibility

- Improved `information_schema` compatibility for ORM introspection: empty virtual tables retain result-set columns, wire metadata uses MySQL's declared column casing while preserving explicit aliases and selected column labels, and constraint/index metadata exposes primary, unique, and foreign-key columns consistently.
- Added correlated subquery evaluation inside aggregate expressions, including correlated `COUNT` and `NOT EXISTS` queries.
- Accepted plain `SELECT ... FOR UPDATE` syntax for transaction-oriented clients while explicitly rejecting unsupported locking extensions. Transaction and writer-lock semantics are added in 0.4.4.

### Compatibility and CI

- Consolidated external compatibility verification on pinned MariaDB 10.11.7 across the differential corpus, exact parity and error-code suites, pre-push provisioning, CI, and MTR discovery tooling; removed the Oracle MySQL comparison paths.
- Updated the MTR harness tests for the renamed MariaDB tooling modules.

### Fixed

- Corrected generated unique-index names and retained explicitly configured index names.

## 0.4.2 Aug 15, 2026

### Performance

- Optimized `ORDER BY ... LIMIT/OFFSET` execution by selecting only the best `OFFSET + LIMIT` rows before sorting, instead of fully sorting every matching row. Stable input-order tie-breaking preserves the previous result ordering at page boundaries.
- Avoided the table scan's default primary-key sort when an explicit `ORDER BY` already contains the complete primary key and therefore defines a total order. Window projections retain the previous base ordering.
- Changed `LIMIT/OFFSET` pagination to trim result vectors in place rather than allocating replacement vectors.
- Removed per-cell lowercase `String` allocations from declared SQL type matching while retaining case-insensitive coercion behavior.
- Added a direct parser path for ordinary `SELECT` statements that cannot require MySQL compatibility rewrites, avoiding repeated string replacements and case conversions on every execution.
- Preallocated single-table scan result vectors from known row or index-match counts.
- Reused a per-query row materialization plan across table scans, including resolved schema column order, parsed generated-column expressions, and exact/case-insensitive column lookup sets.
- Expanded the release benchmark with full-scan projection and scalar-query cases. Across the original 4,000-row cases, compound ordering with pagination improved from 67.63 to 153.31 queries per second (127% faster), while filtered ordering with pagination improved from 109.41 to 154.40 queries per second (41% faster).

### Tests

- Added regression coverage for exact ordered pagination, total primary-key ordering, applying `DISTINCT` before bounded ordering, direct-parser eligibility, and schema-drift row materialization.
- Added regression coverage for ALTER-added auto-increment metadata and `LAST_INSERT_ID()` across ignored inserts, upserts, and `REPLACE`.

### Compatibility and CI

- Added a one-command Docker wrapper for running the pinned MariaDB 10.11.7 MTR baseline locally, including isolated service provisioning, ARM64 execution on macOS, package caching, report generation, and automatic cleanup.
- Expanded the strict, hash-pinned MariaDB 10.11.7 MTR gate from 23 files and 280 SQL statements to 32 files and 381 statements, adding upstream coverage for view lifecycle, null-safe equality, ALTER constraint transactions, auto-increment lowering, Unix-timestamp decimal metadata, generated-column prefix indexes, InnoDB grouping, and unique/subquery edge cases.
- Verified the expanded strict gate at 100%: all 32 upstream files and all 381 statements pass against both MariaDB 10.11.7 and MySqweel.
- Replaced rotating 100-file MariaDB MTR discovery samples with an exhaustive safe-harness audit that follows literal upstream includes and runs all 308 current candidates, covering 19,517 direct and sourced SQL statements from a 5,585-file inventory.
- Refined MariaDB compatibility for foreign-key-aware `DELETE IGNORE` warnings and row skipping, supported `MATCH FULL`/`MATCH PARTIAL` clauses, affected-row reporting, and `LIMIT 0` metadata queries against system tables.
- Recorded the exhaustive audit as non-gating: 31 candidates pass, while 277 fail and 140 encounter infrastructure failures; unsupported or infrastructure-bound cases remain outside the strict promotion gate.
- Fixed persistent single-table deletes so removed rows, primary-key membership, and secondary indexes are updated in Lux storage before the engine is reopened.
- Fixed `ALTER TABLE ... ADD COLUMN ... AUTO_INCREMENT` metadata so the new column remains visible in `information_schema.columns` and `key_column_usage`; parity helpers now also assert `last_insert_id` for write statements.
- Fixed MySQL wire prepared-statement metadata probing so INSERT, UPDATE, and DELETE statements execute only once, during `COM_STMT_EXECUTE`, preserving auto-increment and affected-row semantics.
- Matched MariaDB's `ON DUPLICATE KEY UPDATE` insert-ID behavior by returning the existing row's auto-increment value when an update resolves a unique-key conflict.
- Matched MariaDB insert-ID metadata when an `INSERT` explicitly supplies the value of an auto-increment column.
- Changed MariaDB parity comparisons to collect value mismatches through the full scenario and report them together, instead of stopping at the first mismatch.

## 0.4.1 Aug 13, 2026

### Fixed

- Fixed `JSON_SET` so it replaces existing values correctly, including values addressed by nested array indexes; previously those paths behaved like insert-only updates.
- Fixed `JSON_SEARCH(..., 'one', ...)` to recursively search scalar values when no explicit JSON path is supplied, returning matching nested paths such as `$.name`.
- Fixed `JSON_SEARCH` result encoding so matching paths retain their JSON string representation over the MySQL wire protocol.
- Fixed `SET GLOBAL time_zone` and `SET SESSION time_zone` variable-name normalization, and aligned whole-second `UNIX_TIMESTAMP` wire metadata with MariaDB's integer output.
- Fixed expression lookup precedence so SQL string literals that match column names remain literals, preserving typed column values and keys in nested `JSON_OBJECT` expressions.

### Compatibility and CI

- Fixed the ARM64 MariaDB MTR preparation flow by extracting the pinned `mariadb-server` package for its required `myisamlog` test utility without installing or starting a second database server.
- Bumped the MariaDB MTR package cache key so CI refreshes stale package archives and runs the corrected upstream baseline checks.
- Added external-server timezone handling to the MTR compatibility runner. Hash-pinned tests with fixed POSIX `GMT` offsets now apply the equivalent SQL timezone to both MariaDB and MySqweel before each case.
- Preserved upstream warning checks in source builds by sending MySqweel query warning counts through the vendored MySQL protocol writer, while retaining compatibility with the published dependency during package verification.
- Added a fail-closed `tools/prepush.sh` check and opt-in Git pre-push hook, and made CI use that same entry point for its real-MySQL differential suite; local checks refuse to report success when neither MySQL nor Docker is available.
- Made the pre-push check raise low host file-descriptor limits before running parallel Lux-backed tests, preventing macOS defaults from causing unrelated `Too many open files` failures.
- Removed a timing-dependent port rebind from the parity harness so parallel local checks connect to the already-bound MySqweel listener reliably.
- Added regression coverage for JSON array-index mutation and recursive JSON path search.

## 0.4.0 Aug 12, 2026

- Repositioned MySqweel as streamlined, embeddable MySQL for applications, testing, and QA, with a prominent in-process `Engine` workflow and an explicit transactions/atomicity compatibility boundary.
- Added an opt-in query event stream for embedded Rust users, with unique query IDs, received/completed lifecycle events, query text, execution duration, result-set counts and row sizes, and failure details.
- Added optional full query-result payloads to completion events; result payloads remain disabled by default to avoid copying large result sets.
- Expanded MySQL sorting parity across primitive and compound ordering, including exact integer/decimal handling, FLOAT rounding, temporal, binary, text collation, JSON, ENUM, and SET behavior across window, aggregate, DML, derived-table, and set-operation paths.
- Added regression coverage for sorting edge cases, compound-key stress, aliases and expressions, `GROUP_CONCAT` ordering, `DELETE ... ORDER BY`, windows, derived tables, and `UNION` results.
- Expanded MySQL wire compatibility with declared result-column types, nullability, unsigned and decimal metadata, character sets and collations, warning propagation, `SHOW WARNINGS`, zero date and datetime values, and additional MySQL error-code mappings.
- Expanded query compatibility with correlated `EXISTS`, nested joins, `DUAL`, user variables, aggregate expressions, `EXPLAIN`, `CREATE TABLE ... AS SELECT`, ordered and limited deletes, and stricter MySQL value, safe-update, auto-increment, and decimal semantics.
- Expanded DDL and metadata coverage for case-insensitive schemas, foreign-key indexes, index comments, `SHOW FULL COLUMNS`, `SHOW TABLE STATUS`, `information_schema` engines, process lists, variables, indexes, constraints, and richer table/index introspection.
- Added MySQL date/time and scalar compatibility for `TIME_FORMAT`, `STR_TO_DATE`, `GET_FORMAT`, `FROM_UNIXTIME`, `INTERVAL`, `SOUNDEX`, `MD5`, `SHA`, `SHA2`, `CRC32`, `BIT_LENGTH`, and expanded aggregate support including variance, standard deviation, and bitwise aggregates.
- Added a persistent engine performance benchmark covering filtered and compound `ORDER BY`/`LIMIT` queries over 4,000 rows.
- Improved query execution by resolving sort keys once per row, reusing stored schema column order, avoiding unnecessary row/schema/projection/window/aggregate allocations, and using hash-based lookup and deduplication paths where ordering is not required.
- Added a no-subscriber fast path that avoids query-event timing, IDs, and result processing when query lifecycle events are not being observed.
- Added logical row/cell read and physical row/cell write metrics to query completion events, including filtered rows and aggregate multi-statement totals.
- Expanded JSON compatibility across document construction, extraction, validation, mutation, merge, search, overlap, type/length/depth, quoting/pretty-printing, storage metrics, `JSON_VALUE`, JSON Schema checks/reports, `JSON_ARRAYAGG`, `JSON_OBJECTAGG`, arrow extraction, wildcard paths, and basic `JSON_TABLE` projections.
- Added focused engine and MySQL differential coverage for the expanded JSON surface. `JSON_VALUE` optional clauses and nested `JSON_TABLE` columns remain parser/execution boundaries until their upstream AST support is available.
- Switched the upstream MTR gate from the x86-only Oracle MySQL package flow to pinned Ubuntu ARM64 MariaDB 10.11.7 packages and MariaDB's `mariadb-test-run.pl`, keeping the baseline and MySqweel runs independent with a 100% requirement.
- Added MariaDB MTR layout and runtime canaries, including validation of MariaDB's `main/` suite layout and `my_safe_process` helper so incomplete or no-op runs cannot produce false-positive compatibility reports.
- Added a scheduled MariaDB upstream MTR discovery audit that measures SQL-statement coverage, rotates complete-file candidate batches across MariaDB and MySqweel, and generates promotion manifests for dual-engine passes.
- Added a focused, non-gating MariaDB MTR scope of 21 complete files and 305 SQL statements covering DDL, DML, aggregates, subqueries, date/time, window functions, and JSON; only dual-engine passes can enter the strict manifest.
- Raised the differential MySQL query-corpus requirement from 95% to 100%.

## 0.3.2 July 24, 2026

### Added

- Added the `mysql-test-server` helper under `vendor/` and installed MySQL client/server packages in the Rust image (`vendor/rust.dockerfile`) so image-based test flows can provision a local MySQL instance for parity checks.
- Added dedicated MySQL parity coverage for conditional and null-control expressions (`COALESCE`, `NULLIF`, `IF`, `CASE`) including prepared-statement execution.
- Added MySQL parity coverage for `CASE` combined with window functions under prepared execution.
- Added MySQL parity coverage for `CASE` combined with ranking window functions (`ROW_NUMBER`, `RANK`) under prepared execution.
- Added MySQL parity coverage for JSON/datetime expressions, including prepared JSON construction and extraction paths.
- Added MySQL parity coverage for JSON collection-path semantics (nested arrays/objects, multi-path extraction, index mutation/removal, and prepared JSON mutation/composition).

### Fixed

- Improved `mysql-test-server` startup behavior to be deterministic for already-running servers vs. servers started by the helper, and removed `MYSQL_ROOT_PASSWORD`-specific URL/user setup from helper flow.
- Fixed SQL projection metadata inference so computed expressions now default to nullable metadata when value-based inference cannot prove non-nullability, preventing MySQL protocol `NOT NULL` false positives (for example `NULLIF(...) AS not_alice`).

## 0.3.1 July 24, 2026

### Added

- Added `engine` (`InnoDB`) and live `table_rows` values to `information_schema.tables` metadata.

### Fixed

- Fixed MySQL wire results to accept MySQL, ISO 8601, and RFC 3339 datetime values, and to return a MySQL error for invalid non-null, date/time, or numeric result values before beginning a result set.
- Fixed mysql2 prepared `LIMIT` and `OFFSET` parameters encoded as integral floating-point values.
- Fixed integer and `BIGINT` comparisons/casts to preserve exact 64-bit integer semantics for parseable integral values, avoiding lossy float coercion and overflow in unary arithmetic.
- Fixed SQL statement splitting when quoted values contain backslash-escaped quotes, including mysql2 query-protocol JSON payloads.
- Fixed `DEFAULT` handling in inserts and updates: it no longer conflicts with the literal string `DEFAULT`, nullable columns without declared defaults receive `NULL`, and explicit `NULL` values remain unchanged.
- Fixed single-table `DELETE` predicates that qualify columns with the table name or its alias.
- Fixed aggregate `HAVING` evaluation to correctly use both grouped aggregate aliases and base-row expressions (including predicates that combine grouped and non-grouped terms).
- Fixed SQL function-call parsing to only treat `name(...)` as a function when the closing parenthesis is the terminal wrapper, avoiding false positives while parsing function-like text.
- Fixed MySQL decimal result encoding to avoid panicking when per-column scale metadata is missing by defaulting to zero scale.

## 0.3.0 July 13, 2026

### Added

- Added an opt-in `--mysql-strict` compatibility profile. Strict mode disables implicit table and column creation, enforces declared types, ranges, lengths, nullability, defaults, generated columns, unique keys, and foreign keys, and returns MySQL wire error numbers for common failures. The default profile remains drift tolerant.
- Added declared and inferred result-column metadata for integer widths, unsigned values, floating point and decimal types, date/time, binary, JSON, nullability, source tables, character sets, collations, and scale. Prepared statements now expose this metadata before execution.
- Added typed MySQL wire result encoding for signed and unsigned numeric widths, floats, doubles, decimals, `DATE`, `DATETIME`, `TIMESTAMP`, `TIME` (including negative and fractional values), JSON, and binary data, plus prepared-parameter decoding for native binary date/time values.
- Added qualified wildcards; `RIGHT`, `CROSS`, `USING`, `NATURAL`, and derived-table joins; and nonrecursive common table expressions with optional column aliases.
- Added `UNION`, `INTERSECT`, and `EXCEPT` set semantics, including `ALL`/`DISTINCT` handling, left-branch column names, and branch-arity validation.
- Added named and inline window specifications, common `ROWS` and peer-aware `RANGE` frames, aggregate windows, and `ROW_NUMBER`, `RANK`, `DENSE_RANK`, `PERCENT_RANK`, `CUME_DIST`, `NTILE`, `LAG`, `LEAD`, `FIRST_VALUE`, `LAST_VALUE`, and `NTH_VALUE`.
- Added MySQL multi-table `DELETE` target-list and `DELETE ... USING` forms.
- Added migration-oriented DDL support for temporary tables; virtual and stored generated columns; prefix indexes; `ALTER TABLE` add, drop, rename, change, modify, default/type/nullability, column positioning, table rename, and index operations; and matching `SHOW CREATE TABLE` output.
- Added foreign-key schema validation and insert/update/delete enforcement with `CASCADE`, `SET NULL`, `RESTRICT`, and `NO ACTION` behavior for referenced rows.
- Added a fail-closed SQL support validator so unsupported operators, expressions, functions, query modifiers, table factors, and join forms return explicit errors instead of partial results.
- Added a deterministic 2,500-query differential corpus with a 95% compatibility floor, exact row/result parity tests, MySQL wire error-code checks, and ORM-shaped migration, prepared CRUD, relation, and introspection coverage for Diesel, Drizzle/Knex, Prisma, and SeaORM patterns.
- Added GitHub Actions coverage against MySQL 8.0.43. Real-MySQL parity is non-skippable in CI, and local compatibility tests provision the same image when Docker and the image are available.

### Changed

- Aligned expression evaluation with MySQL three-valued logic, numeric-prefix coercion, case-insensitive string equality, `DIV` and bitwise behavior, byte versus character string lengths, and NULL propagation through unary, comparison, `IN`, `BETWEEN`, and logical operators.
- Aligned `SELECT DISTINCT`, nested aggregate expressions, empty aggregate sets, scalar-subquery cardinality errors, qualified-name resolution, derived columns, ordering aliases, and set-result naming with MySQL behavior.
- Improved `SHOW COLUMNS`, `SHOW INDEX`, `SHOW CREATE TABLE`, and `information_schema` metadata for data types, nullability, ordinal positions, primary/unique/secondary keys, index prefix lengths, generated columns, and referential constraints.
- Aligned affected-row counts with MySQL: changed `ON DUPLICATE KEY UPDATE` rows report two, unchanged duplicate updates report zero, and replacements of existing rows report two.
- Changed declared schemas to be authoritative for reads: unknown tables, ambiguous references, and columns removed by `ALTER TABLE` now return errors instead of silently producing NULL values.
- Reworked the README around an accurate quick start, compatibility-profile comparison, verified SQL contract, local-data and search workflows, security warning, and explicit limitations.

### Fixed

- Fixed foreign-key insert validation deadlocking when child and parent tables occupied the same internal map shard.
- Fixed `ALTER TABLE RENAME COLUMN` and `CHANGE COLUMN` to move existing stored values to the new column name and keep primary, unique, index, and local foreign-key column metadata consistent.
- Fixed peer-aware default `RANGE` window frames and `CUME_DIST`/`PERCENT_RANK` result typing so fractional results are not truncated based on the first row.
- Fixed prepared binary `DATE`, `DATETIME`, and signed fractional `TIME` parameter handling and result encoding.

### Compatibility notes

- Recursive CTEs, `FULL JOIN`, stored programs, and transaction semantics remain outside the supported compatibility surface and now fail explicitly where parsed.

## 0.2.4 - Jun 30, 2026

- Added broader MySQL date/time scalar support, including `DATE_ADD`/`ADDDATE`, `DATE_SUB`/`SUBDATE`, `TIMESTAMPADD`, `TIMESTAMPDIFF`, `DATEDIFF`, `ADDTIME`, `SUBTIME`, `TIMEDIFF`, `EXTRACT`, current/UTC date-time functions, date/time part functions, and expanded `DATE_FORMAT` tokens.
- Added common JSON scalar support for `JSON_EXTRACT`, `JSON_UNQUOTE`, `JSON_OBJECT`, `JSON_ARRAY`, `JSON_CONTAINS`, `JSON_SET`, and `JSON_REMOVE`.
- Added more string and numeric scalar functions, including `LEFT`, `RIGHT`, `LPAD`, `RPAD`, `LOCATE`, `INSTR`, `POSITION`, `REVERSE`, `REPEAT`, `ASCII`/`ORD`, `GREATEST`, `LEAST`, `SIGN`, `SQRT`, `LOG`, `EXP`, `TRUNCATE`, and function-form `MOD`.
- Improved `CAST`/`CONVERT` handling for date, time, datetime, JSON, and signed numeric conversions.
- Improved aggregate compatibility for `GROUP_CONCAT(... ORDER BY ... SEPARATOR ...)` and multi-expression `COUNT(DISTINCT ...)`.
- Split SQL evaluator helpers into focused date/time, JSON, scalar, and common helper modules.
- Expanded real-MySQL parity coverage for date/time, JSON, string, numeric, and aggregate function behavior.
- Switched the MySQL wire-protocol dependency to the vendored `msql-srv` copy under `vendor/msql-srv`.
- Patched the vendored `msql-srv` session loop to acknowledge `COM_CHANGE_USER` and avoid panicking on unsupported command parse failures.
- Fixed floating-point column coercion so integral inserts into `DOUBLE`, `FLOAT`, and `REAL` columns are stored and reported as floating-point values instead of integer metadata.
- Added regression coverage for MySQL date/time, JSON, string, numeric, conversion, and aggregate function evaluation.

## 0.2.3 - Jun 29, 2026

- Fixed `UPDATE ... JOIN` evaluation so `WHERE` clauses and assignment expressions use the joined row context, including table aliases.
- Fixed `UPDATE ... LEFT JOIN` handling so unmatched right-side rows are null-extended for predicates such as `joined_table.id IS NULL`.
- Fixed `INSERT ... SELECT` so source `ORDER BY` and `LIMIT` clauses are preserved.
- Added support for single-table `DELETE ... ORDER BY ... LIMIT`.
- Added explicit errors for unsupported `UPDATE ... FROM`, multi-table `DELETE`, `DELETE ... USING`, `DELETE` joins, and qualified correlated subqueries so they cannot be silently mis-evaluated.
- Improved `ON DUPLICATE KEY UPDATE` evaluation for expressions that mix existing row values with `VALUES(...)`.
- Added expanded regression and MySQL parity coverage for DML edge cases, including `UPDATE ... JOIN`, `UPDATE ... LEFT JOIN`, `INSERT ... SELECT` modifiers, limited deletes, duplicate-key update expressions, and unsupported syntax guards.

## 0.2.2 - Jun 27, 2026

- Refreshed the README branding and corrected the introductory project description.

## 0.2.1 - Jun 27, 2026

- Added SQL `RETURNING` support for `INSERT`, `UPDATE`, and `DELETE`, including projection expressions and aliases.
- Fixed `DELETE` predicate evaluation so subqueries do not deadlock while rows are being removed.

## 0.2.0 - Jun 25, 2026

- Added an always-on Meilisearch-compatible HTTP API on the debug HTTP port. - Meilisearch indexes map to MySqweel/MySQL tables. - Meilisearch documents map to stored rows. - The MySQL/table engine remains the source of truth.
- Added synchronous Tantivy-backed text search for the Meilisearch-compatible API. - Document and table mutations rebuild the derived search index before reporting task success. - Search falls back to row-scan compatibility for edge cases where Tantivy produces no candidates.
- Added Meilisearch-compatible index, document, search, multi-search, settings, task, key, stats, and swap-index endpoints.
- Added support for Meilisearch search options including filters, sort, pagination, `attributesToRetrieve`, `attributesToSearchOn`, `showRankingScore`, and `showRankingScoreDetails`.
- Added facet support for Meilisearch search responses, including `facetDistribution`, numeric `facetStats`, array facet values, and `facets: ["*"]`.
- Added a 90/10 Meilisearch compatibility pass for previously missing feature areas: - query-time synonym and typo-tolerance fallback matching - highlighting, cropping, and match-position metadata in search hits - `POST /indexes/:uid/facet-search` - synchronous in-memory dump status/download endpoints - webhook CRUD compatibility endpoints - permissive bearer/API-key handling for tenant-token-shaped local client requests
- Added task compatibility improvements: - write APIs return Meilisearch-shaped tasks - tasks include both `taskUid` and `uid` - task durations are serialized as strings - task listing supports `uids`, `types`, `statuses`, `indexUids`, ranges, pagination, `from`, and `next`
- Added official Meilisearch JavaScript client compatibility coverage via `tests/node/meili-js-client-compat.mjs` and `cargo test --test meili_js_client`.
- Added optional official Meilisearch Python client compatibility coverage via `tests/python/meili_client_compat.py` and `cargo test --test meili_python_client`.
- Added direct Meilisearch handler coverage for synonyms, typo tolerance, formatting, facet search, dumps, and webhooks.
- Added `npm run test:meili` and `requirements-dev.txt` for running SDK compatibility smoke tests outside Cargo.
- Fixed `sqwl serve` panic caused by nesting Tokio runtimes inside the synchronous server path.
- Fixed Meilisearch filter handling for multi-value `IN` and `NOT IN` expressions.
- Fixed Meilisearch ranking score metadata being stripped by `attributesToRetrieve`.
- Fixed fallback text search so `searchableAttributes` and `attributesToSearchOn` are respected consistently.
- Fixed primary key metadata reporting so `information_schema.key_column_usage` and related introspection stay consistent after `ALTER TABLE` operations.
- Fixed an issue with spawning connection sessions.
- Added more information_schema coverage to the backend with associated tests.

## 0.1.0 - May 31, 2026

### Added

- Added the `sqwl` binary with `serve`, `serve --repl`, `repl`, and `explain` commands.
- Added a lightweight maintenance REPL for status, drift reports, snapshots, index rebuilds, resets, SQL execution, help, and graceful `Ctrl+C` / `Ctrl+D` exit.
- Added MySQL wire-protocol support for local `mysql2`, Drizzle, and migration workflows.
- Added permissive schema behavior: - inserts can create missing tables and columns - repeated `CREATE TABLE` statements merge into existing metadata - reads return rows shaped to the latest known schema - stored rows are not rewritten just because the schema changed
- Added support for schema metadata from `CREATE TABLE`, `ALTER TABLE`, indexes, unique constraints, and advisory foreign keys.
- Added dynamic row materialization against the latest schema metadata.
- Added positional inserts that infer generated `column_1`, `column_2`, etc. columns when needed.
- Added configurable duplicate handling with `--unique-mode overwrite|enforce`.
- Added Lux-backed directory persistence with exclusive data-directory locking.
- Added debug HTTP endpoints for health, drift reporting, table inspection, snapshots, restore, and JSON table seeding.
- Added fault-injection flags for query delay and intermittent read/write failures.
- Added broader query coverage, including joins, grouping, aggregates, scalar expressions, common functions, simple derived tables, and uncorrelated subqueries.
- Added best-effort `information_schema` and MySQL metadata command support.
- Added compatibility smoke tests for `mysql2`, Drizzle, and real-MySQL parity checks.
- Added expanded MySQL parity coverage for functions, NULL predicates, prepared writes, defaults, arithmetic updates, and deletes.
- Added project logo usage in the README.
- Added focused SQL engine submodules under `src/sql/engine/`.

### Notes

- MySqweel does not provide ACID guarantees, transaction semantics, or full MySQL compatibility.
