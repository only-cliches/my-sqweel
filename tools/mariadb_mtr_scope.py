#!/usr/bin/env python3
"""Conservative SQL-scope classification for MariaDB MTR files.

The caller supplies SQL that has already had quoted literals, ordinary comments,
and MTR directive comments replaced with whitespace.  Whitespace replacement is
important: line numbers in evidence then refer to the original ``.test`` file.
This module deliberately does not inspect paths, MTR directives, expected
results, or execution outcomes.
"""

from __future__ import annotations

import re
from collections.abc import Iterator
from dataclasses import dataclass


# These names are part of the small, stable evidence vocabulary consumed by the
# discovery/planning layer.  Keep the expressions statement-oriented: matching
# an opcode substring (for example, ``ROLLBACK`` inside another word) is unsafe.
_OUTSIDE_PATTERNS: tuple[tuple[str, re.Pattern[str]], ...] = (
    (
        "routines-triggers-events",
        re.compile(
            r"\b(?:CREATE\s+(?:OR\s+REPLACE\s+)?(?:DEFINER\s*=\s*(?:\S+\s+)?"
            r")?(?:PROCEDURE|FUNCTION|TRIGGER|EVENT)|"
            r"ALTER\s+(?:PROCEDURE|FUNCTION|TRIGGER|EVENT)|"
            r"DROP\s+(?:PROCEDURE|FUNCTION|TRIGGER|EVENT)|CALL)\b",
            re.IGNORECASE,
        ),
    ),
    (
        "replication-topology",
        re.compile(
            r"\b(?:CHANGE\s+(?:MASTER|REPLICATION\s+SOURCE)|START\s+(?:SLAVE|REPLICA)|"
            r"STOP\s+(?:SLAVE|REPLICA)|RESET\s+(?:MASTER|SLAVE|REPLICA)|"
            r"SHOW\s+(?:MASTER\s+STATUS|SLAVE\s+STATUS|REPLICA\s+STATUS)|"
            r"PURGE\s+BINARY\s+LOGS|CREATE\s+SERVER|ALTER\s+SERVER|DROP\s+SERVER|"
            r"GROUP_REPLICATION|NDBCLUSTER)\b",
            re.IGNORECASE,
        ),
    ),
    (
        "physical-storage",
        re.compile(
            r"\b(?:ANALYZE\s+TABLE|OPTIMIZE\s+TABLE|REPAIR\s+TABLE|"
            r"(?:DISCARD|IMPORT)\s+TABLESPACE|(?:DATA|INDEX)\s+DIRECTORY|"
            r"LOAD\s+(?:DATA|XML)\s+(?:LOCAL\s+)?INFILE|"
            r"INTO\s+(?:OUTFILE|DUMPFILE)|SHOW\s+ENGINE\b|"
            r"ENGINE\s*=\s*(?:ARCHIVE|ARIA|BLACKHOLE|CSV|MEMORY|HEAP|MYISAM|"
            r"FEDERATED|NDB|ROCKSDB)|FULLTEXT)\b",
            re.IGNORECASE,
        ),
    ),
    (
        "unsupported-isolation-xa-locking",
        re.compile(
            r"\b(?:SET\s+(?:(?:SESSION|LOCAL)\s+)?TRANSACTION\s+"
            r"ISOLATION\s+LEVEL\s+(?:READ\s+(?:COMMITTED|UNCOMMITTED)|SERIALIZABLE)|"
            r"XA\s+(?:START|BEGIN|END|PREPARE|COMMIT|ROLLBACK|RECOVER)|"
            r"LOCK\s+TABLES?|UNLOCK\s+TABLES?|LOCK\s+IN\s+SHARE\s+MODE|"
            r"FOR\s+SHARE\b|NOWAIT\b|SKIP\s+LOCKED)\b",
            re.IGNORECASE,
        ),
    ),
    (
        "optimizer-plan-assertions",
        re.compile(
            r"\b(?:EXPLAIN(?:\s+ANALYZE)?|SHOW\s+EXPLAIN|OPTIMIZER_TRACE|"
            r"(?:USE|FORCE|IGNORE)\s+INDEX|STRAIGHT_JOIN|OPTIMIZER_SWITCH)\b",
            re.IGNORECASE,
        ),
    ),
    (
        "unsupported-query-forms",
        re.compile(r"\bFULL\s+JOIN\b", re.IGNORECASE),
    ),
    (
        "plugins-tls-auth-extensions",
        re.compile(
            r"\b(?:INSTALL\s+(?:PLUGIN|COMPONENT)|UNINSTALL\s+(?:PLUGIN|COMPONENT)|"
            r"CREATE\s+(?:OR\s+REPLACE\s+)?(?:PLUGIN|COMPONENT)|"
            r"ALTER\s+USER|CREATE\s+ROLE|DROP\s+ROLE|SET\s+PASSWORD|"
            r"REQUIRE\s+(?:SSL|X509)|IDENTIFIED\s+(?:VIA|WITH)|"
            r"GRANT\s+ALL\s+PRIVILEGES|WITH\s+GRANT\s+OPTION|"
            r"GRANT\s+FILE\b|GRANT\s+(?:USAGE|CREATE\s+USER|SUPER)\b|"
            r"SHOW\s+PLUGINS)\b",
            re.IGNORECASE,
        ),
    ),
    (
        "gis",
        re.compile(
            r"\b(?:SPATIAL|GEOMETRY(?:COLLECTION)?|POINT|LINESTRING|POLYGON|"
            r"MULTIPOINT|MULTILINESTRING|MULTIPOLYGON|ST_[A-Z0-9_]+)\b",
            re.IGNORECASE,
        ),
    ),
    (
        "server-configuration",
        re.compile(
            r"\b(?:SET\s+(?:@@)?GLOBAL|SET\s+PERSIST(?:_ONLY)?|FLUSH\s+"
            r"(?:LOGS|TABLES)|SHUTDOWN|RESTART)\b",
            re.IGNORECASE,
        ),
    ),
)

_SUPPORTED_PROVISIONING = re.compile(
    r"\b(?:CREATE|DROP)\s+(?:USER|DATABASE)\b|"
    r"\bGRANT\s+(?:SELECT|INSERT|UPDATE|DELETE)(?:\s*,\s*(?:SELECT|INSERT|UPDATE|DELETE))*"
    r"\s+ON\s+(?:(?:[A-Z0-9_$`]+\s*)?\.\s*)?\*\s+TO\b|"
    r"\bREVOKE\s+ALL\s+PRIVILEGES(?:\s+ON\s+(?:(?:[A-Z0-9_$`]+\s*)?\.\s*)?\*)?\s+FROM\b",
    re.IGNORECASE,
)
_UNSUPPORTED_GRANT_OR_REVOKE = re.compile(r"\b(?:GRANT|REVOKE)\b", re.IGNORECASE)
_SUPPORTED_TRANSACTION = re.compile(
    r"\b(?:START\s+TRANSACTION|BEGIN(?:\s+WORK)?|COMMIT|ROLLBACK(?:\s+TO\s+SAVEPOINT)?|"
    r"SAVEPOINT|RELEASE\s+SAVEPOINT)\b|"
    r"\bSET\s+(?:(?:SESSION|LOCAL)\s+)?TRANSACTION\s+ISOLATION\s+LEVEL\s+"
    r"REPEATABLE\s+READ\b|\bSET\s+(?:(?:SESSION|LOCAL)\s+)?AUTOCOMMIT\s*=",
    re.IGNORECASE,
)
_SUPPORTED_SESSION = re.compile(
    r"^(?:SET\b(?!\s+(?:@@)?GLOBAL\b)(?!\s+PERSIST(?:_ONLY)?\b)"
    r"(?!\s+TRANSACTION\b)(?!\s+AUTOCOMMIT\b)|USE\b)",
    re.IGNORECASE,
)
_SUPPORTED_METADATA = re.compile(
    r"^(?:SHOW\b|DESCRIBE\b|DESC\b)|\bINFORMATION_SCHEMA\b|\bPERFORMANCE_SCHEMA\b",
    re.IGNORECASE,
)
_SUPPORTED_QUERY = re.compile(r"^(?:SELECT|WITH|VALUES)\b", re.IGNORECASE)
_SUPPORTED_DML = re.compile(r"^(?:INSERT|UPDATE|DELETE|REPLACE)\b", re.IGNORECASE)
_SUPPORTED_DDL = re.compile(
    r"^(?:CREATE\s+(?:(?:OR\s+REPLACE\s+)?(?:TEMPORARY\s+)?"
    r"(?:TABLE|VIEW|INDEX|DATABASE|SCHEMA)|UNIQUE\s+INDEX)|"
    r"ALTER\s+(?:TABLE|VIEW|DATABASE|INDEX)|DROP\s+(?:TABLE|VIEW|INDEX|DATABASE|SCHEMA)|"
    r"TRUNCATE\s+TABLE|RENAME\s+TABLE)\b",
    re.IGNORECASE,
)


@dataclass(frozen=True)
class _Evidence:
    kind: str
    feature: str
    line: int




def _statements(masked_sql: str) -> Iterator[tuple[str, int]]:
    """Yield semicolon-delimited SQL and its incremental base source line.

    The iterator keeps only one statement slice alive at a time and advances
    the line counter incrementally; this avoids rescanning a large corpus for
    every evidence match.
    """
    start = 0
    current_line = 1
    for match in re.finditer(r";", masked_sql):
        statement = masked_sql[start : match.start()]
        yield statement, current_line
        current_line += statement.count("\n")
        start = match.end()
    statement = masked_sql[start:]
    yield statement, current_line



def classify_scope(masked_sql: str) -> dict:
    """Classify SQL against MySqweel's documented compatibility boundary.

    The return value has ``status`` (``in-scope``, ``mixed``, or
    ``review-required``), line-grounded ``evidence``, a deterministic
    ``rationale``, and ``reviewed=False``.  It never returns ``out-of-scope``:
    a file containing only outside or unknown evidence still needs explicit
    human review, and an in-scope family with an unimplemented edge is a
    coverage gap rather than an exclusion.
    """
    if not isinstance(masked_sql, str):
        raise TypeError("masked_sql must be str")

    evidence: dict[tuple[str, str], _Evidence] = {}

    def add(kind: str, feature: str, line: int) -> None:
        key = (kind, feature)
        current = evidence.get(key)
        if current is None or line < current.line:
            evidence[key] = _Evidence(kind, feature, line)

    unknown_statement = False
    try:
        from tools.mariadb_mtr_discover_core import (
            PARTITION_BY_SQL,
            TABLE_PARTITION_OPERATION,
            has_table_partition,
            window_partition_at,
        )
    except ModuleNotFoundError:
        # Direct execution from tools/ may not expose the package import path.
        from mariadb_mtr_discover_core import (
            PARTITION_BY_SQL,
            TABLE_PARTITION_OPERATION,
            has_table_partition,
            window_partition_at,
        )

    partition_by = PARTITION_BY_SQL
    partition_operation = TABLE_PARTITION_OPERATION

    for statement, base_line in _statements(masked_sql):
        stripped = statement.strip()
        if not stripped:
            continue
        leading = len(statement) - len(statement.lstrip())
        line = base_line + statement.count("\n", 0, leading)
        recognized_here = False
        for feature, pattern in _OUTSIDE_PATTERNS:
            match = pattern.search(statement)
            if match:
                add(
                    "outside-contract",
                    feature,
                    base_line + statement.count("\n", 0, match.start()),
                )
                recognized_here = True

        # Reuse discovery's context-aware partition logic: PARTITION BY in an
        # OVER()/WINDOW clause is a relational window feature, not storage.
        if has_table_partition(statement):
            partition_match = None
            for match in partition_by.finditer(statement):
                if not window_partition_at(statement, match.start()):
                    partition_match = match
                    break
            if partition_match is None:
                partition_match = partition_operation.search(statement)
            if partition_match is not None:
                add(
                    "outside-contract",
                    "physical-storage",
                    base_line + statement.count("\n", 0, partition_match.start()),
                )
                recognized_here = True

        # Supported provisioning is intentionally checked before generic DDL.
        # CREATE/DROP USER and database-wide basic grants are documented support,
        # while other account/privilege forms remain explicit review evidence.
        provisioning = _SUPPORTED_PROVISIONING.search(statement)
        if provisioning:
            add(
                "in-scope",
                "provisioning",
                base_line + statement.count("\n", 0, provisioning.start()),
            )
            recognized_here = True
        elif _UNSUPPORTED_GRANT_OR_REVOKE.search(statement):
            grant = _UNSUPPORTED_GRANT_OR_REVOKE.search(statement)
            assert grant is not None
            add(
                "outside-contract",
                "plugins-tls-auth-extensions",
                base_line + statement.count("\n", 0, grant.start()),
            )
            recognized_here = True

        transaction = _SUPPORTED_TRANSACTION.search(statement)
        if transaction and not re.match(r"XA\b", stripped, re.IGNORECASE):
            add(
                "in-scope",
                "transactions",
                base_line + statement.count("\n", 0, transaction.start()),
            )
            recognized_here = True
        session = _SUPPORTED_SESSION.search(stripped)
        if session:
            add("in-scope", "session-settings", line)
            recognized_here = True
        metadata = _SUPPORTED_METADATA.search(stripped)
        if metadata and not re.match(
            r"SHOW\s+(?:EXPLAIN|MASTER\s+STATUS|SLAVE\s+STATUS|REPLICA\s+STATUS|"
            r"PLUGINS|ENGINE)\b",
            stripped,
            re.IGNORECASE,
        ):
            add("in-scope", "metadata", line)
            recognized_here = True
        query = _SUPPORTED_QUERY.search(stripped)
        if query:
            add("in-scope", "queries", line)
            recognized_here = True
        dml = _SUPPORTED_DML.search(stripped)
        if dml:
            add("in-scope", "dml", line)
            recognized_here = True

        # Routine and account statements are not ordinary DDL.  For physical
        # table features, retaining DDL evidence correctly yields ``mixed``.
        if not re.search(
            r"\b(?:CREATE|ALTER|DROP)\s+(?:OR\s+REPLACE\s+)?(?:DEFINER\s*=\s*(?:\S+\s+)?"
            r")?(?:PROCEDURE|FUNCTION|TRIGGER|EVENT|USER|ROLE|PLUGIN|COMPONENT|SERVER)\b",
            statement,
            re.IGNORECASE,
        ) and _SUPPORTED_DDL.search(stripped):
            add("in-scope", "ddl", line)
            recognized_here = True

        if not recognized_here:
            unknown_statement = True

    ordered = sorted(evidence.values(), key=lambda item: (item.line, item.kind, item.feature))
    evidence_rows = [
        {"kind": item.kind, "feature": item.feature, "line": item.line} for item in ordered
    ]
    has_supported = any(item.kind == "in-scope" for item in ordered)
    has_outside = any(item.kind == "outside-contract" for item in ordered)
    if has_supported and has_outside:
        status = "mixed"
        rationale = "Supported SQL families and explicit outside-contract evidence require separate coverage review."
    elif has_supported and unknown_statement:
        status = "review-required"
        rationale = "Known supported SQL and an unknown statement coexist; the complete file requires review."
    elif has_supported:
        status = "in-scope"
        rationale = "Only documented MySqweel SQL families were detected; implementation gaps remain coverage work."
    elif has_outside:
        status = "review-required"
        rationale = "Outside-contract evidence was detected, but automatic exclusion is never permitted."
    else:
        status = "review-required"
        rationale = "No known supported SQL family was detected; an uncertain file remains required for review."
    return {
        "status": status,
        "evidence": evidence_rows,
        "rationale": rationale,
        "reviewed": False,
    }


__all__ = ["classify_scope"]
