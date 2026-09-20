#!/usr/bin/env python3
"""Inventory pinned MariaDB MTR tests and build exhaustive or batched audit manifests.

Every upstream path receives testing intent independently of whether the external
runner can execute it. Unknown and mixed files remain a required backlog; only
explicit hash-pinned scope reviews can exempt a file.
"""

from __future__ import annotations

import argparse
import json
import re
from collections import Counter
from dataclasses import dataclass, replace
from pathlib import Path

try:
    from tools.mariadb_mtr_core import (
        TEST_NAME,
        sha256_file,
        sql_statement_count,
    )
    from tools.mariadb_mtr_scope import classify_scope
    from tools.mariadb_mtr_testing import build_testing_plan
except ModuleNotFoundError:  # Direct execution adds tools/, not the repository root, to sys.path.
    from mariadb_mtr_core import TEST_NAME, sha256_file, sql_statement_count
    from mariadb_mtr_scope import classify_scope
    from mariadb_mtr_testing import build_testing_plan


DEPENDENT_DIRECTIVE = re.compile(
    r"(?im)^\s*(?:--\s*)?(?:source|include|let|eval|exec|system|perl|connect|connection|"
    r"send|reap|sleep|real_sleep|shutdown|restart|write_file|append_file|remove_file|"
    r"copy_file|move_file|chmod|mkdir|rmdir|cat_file)\b"
)
UNSAFE_HARNESS_DIRECTIVE = re.compile(
    r"(?im)^\s*(?:--\s*)?(?:exec|system|perl|shutdown|restart|write_file|append_file|"
    r"remove_file|copy_file|move_file|chmod|mkdir|rmdir|cat_file)\b"
)
DYNAMIC_HARNESS_DIRECTIVE = re.compile(
    r"(?im)^\s*(?:--\s*)?eval\b"
)
LET_DIRECTIVE = re.compile(r"(?im)^\s*(?:--\s*)?let\b")
SQL_IN_DYNAMIC_VALUE = re.compile(
    r"(?:`|\$|\b(?:SELECT|INSERT|UPDATE|DELETE|REPLACE|WITH|CREATE|ALTER|DROP|CALL|DO|SET)\b)",
    re.IGNORECASE,
)
SOURCE_DIRECTIVE = re.compile(
    r"(?im)^\s*(?:--\s*)?(?:source|include)\s+['\"]?([^'\"\s;]+)"
)
DELIMITER_DIRECTIVE = re.compile(r"(?im)^\s*(?:--\s*)?delimiter\b")
UNSUPPORTED_SQL = re.compile(
    r"(?is)\b(?:ISOLATION\s+LEVEL\s+(?:READ\s+(?:COMMITTED|UNCOMMITTED)|SERIALIZABLE)|"
    r"LOCK\s+IN\s+SHARE\s+MODE|LOCK\s+TABLES?|UNLOCK\s+TABLES?|XA\s|GRANT\s|REVOKE\s|"
    r"CREATE\s+USER|ALTER\s+USER|DROP\s+USER|CREATE\s+(?:DEFINER\s*=\s*\S+\s+)?"
    r"(?:PROCEDURE|FUNCTION|TRIGGER|EVENT)|DROP\s+(?:PROCEDURE|FUNCTION|TRIGGER|EVENT)|"
    r"CHANGE\s+(?:MASTER|REPLICATION\s+SOURCE)|START\s+(?:SLAVE|REPLICA)|"
    r"STOP\s+(?:SLAVE|REPLICA)|RESET\s+(?:MASTER|REPLICA|SLAVE)|"
    r"CREATE\s+SERVER|ALTER\s+SERVER|DROP\s+SERVER|"
    r"INSTALL\s+(?:PLUGIN|COMPONENT)|UNINSTALL\s+(?:PLUGIN|COMPONENT)|"
    r"CREATE\s+RESOURCE\s+GROUP|ALTER\s+RESOURCE\s+GROUP|CLONE\s+INSTANCE)\b"
)
SPECIALIZED_SQL = re.compile(
    r"\b(?:FULLTEXT|SPATIAL)\b|"
    r"\bENGINE\s*=\s*(?:ARCHIVE|ARIA|BLACKHOLE|CSV|MEMORY|HEAP|MYISAM|FEDERATED|NDB)\b|"
    r"\bLOAD\s+(?:DATA|XML)\s+(?:LOCAL\s+)?INFILE\b|"
    r"\bINTO\s+(?:OUTFILE|DUMPFILE)\b",
    re.IGNORECASE,
)
PARTITION_BY_SQL = re.compile(r"\bPARTITION\s+BY\b", re.IGNORECASE)
TABLE_PARTITION_OPERATION = re.compile(
    r"\b(?:REORGANIZE|ADD|DROP|COALESCE|EXCHANGE|ANALYZE|CHECK|OPTIMIZE|REPAIR)\s+PARTITION\b|"
    r"\bTRUNCATE\s+(?:TABLE\s+[^\s;(),]+\s+)?PARTITION\b|"
    r"\bREMOVE\s+PARTITIONING\b|"
    r"\bPARTITION\s*\(",
    re.IGNORECASE,
)
SERVER_CONFIGURATION_SQL = re.compile(
    r"(?is)\b(?:SET\s+(?:@@)?GLOBAL|FLUSH\s|SHUTDOWN\b|RESTART\b|"
    r"SET\s+PERSIST(?:_ONLY)?\b)"
)
TOPOLOGY_NAME = re.compile(
    r"(?i)(?:^|[/_])(?:binlog|rpl|replication|replica|slave|master|group_replication|ndb)(?:[/_]|$)"
)
COMPATIBILITY_SUITES = {
    "collations",
    "funcs_1",
    "funcs_2",
    "gcol",
    "information_schema",
    "innodb",
    "jp",
    "json",
}
SAFE_HARNESS_SUITES = (COMPATIBILITY_SUITES - {"innodb"}) | {"vcol"}
# Admit reviewed InnoDB transaction cases without opening the whole engine suite
# (which also exercises physical storage, row locks, and debug instrumentation).
SAFE_HARNESS_CASES = {"innodb/innodb_bug57255"}
TRANSACTION_SQL = re.compile(
    r"(?im)(?:^|;)\s*(?:BEGIN(?:\s+WORK)?\s*;|START\s+TRANSACTION\b|"
    r"COMMIT\b|ROLLBACK\b|(?:RELEASE\s+)?SAVEPOINT\b)|"
    r"\bSET\s+(?:(?:SESSION|LOCAL)\s+|@@(?:(?:session|local)\.)?)?autocommit\s*=",
)



@dataclass(frozen=True)
class DiscoveryCase:
    name: str
    feature: str
    statements: int
    test_sha256: str
    result_sha256: str
    test_file: str
    result_file: str
    exclusion: str | None
    project_scope: dict


def sql_code(text: str) -> str:
    """Return executable SQL with comments and quoted literals masked.

    MTR executable comments (``/*! ... */`` and MariaDB ``/*M! ... */``)
    are retained because the server executes their body. Everything else that
    can contain SQL-looking words is replaced with spaces, preserving offsets
    and statement boundaries.
    """
    output: list[str] = []
    index = 0
    quote: str | None = None
    escaped = False
    while index < len(text):
        character = text[index]
        following = text[index + 1] if index + 1 < len(text) else ""
        if quote:
            if escaped:
                escaped = False
            elif character == "\\":
                escaped = True
            elif character == quote:
                if following == quote:
                    output.extend((" ", " "))
                    index += 2
                    continue
                quote = None
            output.append("\n" if character == "\n" else " ")
            index += 1
            continue
        if character in ("'", '"', "`"):
            quote = character
            output.append(" ")
            index += 1
            continue
        if character == "/" and following == "*":
            marker = "!" if text[index + 2 : index + 3] == "!" else text[index + 2 : index + 4].lower()
            executable = marker in {"!", "m!"}
            end = text.find("*/", index + 2)
            if end < 0:
                end = len(text)
            if executable:
                body_start = index + (3 if marker == "!" else 4)
                body = re.sub(r"^\d{5,6}", lambda match: " " * len(match[0]), text[body_start:end])
                output.append(" " * (body_start - index))
                output.append(sql_code(body))
                output.append(" " * (min(len(text), end + 2) - end))
            else:
                output.append(re.sub(r"[^\n]", " ", text[index:min(len(text), end + 2)]))
            index = min(len(text), end + 2)
            continue
        if character == "#":
            end = text.find("\n", index)
            end = len(text) if end < 0 else end
            output.append(" " * (end - index))
            index = end
            continue
        if character == "-" and following == "-" and (
            index + 2 == len(text) or text[index + 2].isspace()
        ):
            end = text.find("\n", index)
            end = len(text) if end < 0 else end
            output.append(" " * (end - index))
            index = end
            continue
        output.append(character)
        index += 1
    return "".join(output)


def window_partition_at(code: str, match_start: int) -> bool:
    """Return whether a PARTITION BY lies inside OVER()/WINDOW ... AS()."""
    stack: list[int] = []
    index = 0
    while index < match_start:
        if code[index] == "(":
            stack.append(index)
        elif code[index] == ")" and stack:
            stack.pop()
        index += 1
    if not stack:
        return False
    opening = stack[-1]
    before = code[:opening]
    if re.search(r"\bOVER\s*$", before, re.IGNORECASE):
        return True
    statement = before.rsplit(";", 1)[-1]
    return bool(re.search(r"\bWINDOW\b.*\bAS\s*$", statement, re.IGNORECASE | re.DOTALL))


def has_table_partition(sql: str) -> bool:
    """Identify table partitioning without rejecting window partitions."""
    code = sql_code(sql)
    if TABLE_PARTITION_OPERATION.search(code):
        return True
    for match in PARTITION_BY_SQL.finditer(code):
        if not window_partition_at(code, match.start()):
            # A PARTITION BY outside an OVER()/WINDOW ... AS() clause is table
            # partitioning. This intentionally errs on the side of exclusion.
            return True
    return False


def unresolved_source_reason(
    mysql_test_root: Path,
    text: str,
    visited: frozenset[Path] = frozenset(),
) -> str | None:
    """Reject dynamic, missing, or out-of-tree source/include dependencies."""
    root = mysql_test_root.resolve()
    seen = set(visited)
    pending = [text]
    while pending:
        current = pending.pop()
        for match in SOURCE_DIRECTIVE.finditer(current):
            source = match.group(1)
            if "$" in source:
                return "unresolved-include"
            source_file = (root / source).resolve()
            if not source_file.is_relative_to(root) or not source_file.is_file():
                return "unresolved-include"
            if source_file in seen:
                continue
            seen.add(source_file)
            pending.append(source_file.read_text(encoding="utf-8", errors="replace"))
    return None


def has_unresolved_dynamic_harness(text: str) -> bool:
    if DYNAMIC_HARNESS_DIRECTIVE.search(text):
        return True
    for line in text.splitlines():
        if not LET_DIRECTIVE.match(line):
            continue
        _, separator, value = line.partition("=")
        if separator and SQL_IN_DYNAMIC_VALUE.search(value):
            return True
    return False


def without_mtr_comments(text: str) -> str:
    return "\n".join(
        "" if line.lstrip().startswith(("#", "--")) else line
        for line in text.splitlines()
    )


def expanded_mtr_text(
    mysql_test_root: Path,
    text: str,
    visited: frozenset[Path] = frozenset(),
) -> str:
    """Append literal MTR source/include contents for conservative SQL classification.

    MTR remains responsible for executing the original files. Expansion is used only
    to prevent a harmless-looking wrapper from hiding an excluded feature such as a
    stored routine, replication setup, or file-system side effect in an ``.inc`` file.
    Dynamic and missing source paths are explicitly rejected by discovery rather
    than treated as safe candidates.
    """
    expanded = [text]
    pending = [text]
    seen = set(visited)
    while pending:
        current = pending.pop()
        for match in SOURCE_DIRECTIVE.finditer(current):
            source = match.group(1)
            if "$" in source:
                continue
            source_file = (mysql_test_root / source).resolve()
            if (
                not source_file.is_relative_to(mysql_test_root.resolve())
                or source_file in seen
                or not source_file.is_file()
            ):
                continue
            seen.add(source_file)
            source_text = source_file.read_text(encoding="utf-8", errors="replace")
            expanded.append(source_text)
            pending.append(source_text)
    return "\n".join(expanded)


def classify_feature(sql: str) -> str:
    categories: set[str] = {"transactions"} if TRANSACTION_SQL.search(sql) else set()
    keyword_categories = (
        (r"\b(?:CREATE|ALTER|DROP|TRUNCATE|RENAME)\s", "ddl"),
        (r"\b(?:INSERT|REPLACE)\s", "insert"),
        (r"\bUPDATE\s", "update"),
        (r"\bDELETE\s", "delete"),
        (r"\b(?:SELECT|WITH)\s", "query"),
        (r"\b(?:SHOW|DESCRIBE|DESC|EXPLAIN)\s", "metadata"),
        (r"\bSET\s", "session"),
    )
    for pattern, category in keyword_categories:
        if re.search(pattern, sql, re.IGNORECASE):
            categories.add(category)
    if not categories:
        return "other"
    return "-".join(sorted(categories))


def test_name(mysql_test_root: Path, test_file: Path, layout: str = "mariadb") -> str | None:
    relative = test_file.relative_to(mysql_test_root)
    parts = relative.parts
    if layout == "mariadb" and len(parts) == 2 and parts[0] == "main":
        return test_file.stem
    if len(parts) == 2 and parts[0] == "t":
        return test_file.stem
    if parts and parts[0] == "suite":
        # MariaDB carries both suite/name.test and suite/t/name.test forms.
        if len(parts) == 3:
            return f"{parts[1]}/{test_file.stem}"
        if len(parts) == 4 and parts[2] == "t":
            return f"{parts[1]}/{test_file.stem}"
    # Keep every non-executable layout in the inventory with a stable,
    # path-derived identity instead of silently dropping it.
    return "/".join((*parts[:-1], test_file.stem))


def result_file_for_test(mysql_test_root: Path, test_file: Path, layout: str = "mariadb") -> Path:
    relative = test_file.relative_to(mysql_test_root)
    parts = list(relative.parts)
    if layout == "mariadb" and len(parts) == 2 and parts[0] == "main":
        return test_file.with_suffix(".result")
    test_directories = [index for index, part in enumerate(parts[:-1]) if part == "t"]
    if test_directories:
        parts[test_directories[-1]] = "r"
        return mysql_test_root.joinpath(*parts).with_suffix(".result")
    return test_file.with_suffix(".result")


def companion_file_exists(test_file: Path) -> bool:
    stem = test_file.with_suffix("")
    companions = (
        stem.with_suffix(".opt"),
        stem.with_suffix(".cnf"),
        test_file.with_name(f"{test_file.stem}-master.opt"),
        test_file.with_name(f"{test_file.stem}-slave.opt"),
    )
    return any(path.exists() for path in companions)


def layout_exclusion_reason(
    mysql_test_root: Path,
    test_file: Path,
    name: str,
) -> str | None:
    parts = test_file.relative_to(mysql_test_root).parts
    if not parts:
        return "unsupported-layout"
    if "include" in parts:
        return "helper-layout"
    if parts[0] == "plugin":
        return "plugin-layout"
    if parts[0] == "suite":
        if len(parts) == 3 or (len(parts) == 4 and parts[2] == "t"):
            return None
        return "nested-suite-layout"
    if parts[0] in {"main", "t"} and len(parts) == 2:
        return None
    return "unsupported-layout"


def duplicate_names(cases: list[DiscoveryCase]) -> set[str]:
    by_name: dict[str, int] = Counter(case.name for case in cases)
    return {name for name, count in by_name.items() if count > 1}


def apply_duplicate_exclusions(cases: list[DiscoveryCase]) -> list[DiscoveryCase]:
    duplicates = duplicate_names(cases)
    if not duplicates:
        return cases
    return [
        replace(case, exclusion="ambiguous-execution-name")
        if case.name in duplicates
        else case
        for case in cases
    ]


def exclusion_reason(
    name: str,
    text: str,
    sql: str,
    statements: int,
    result_file: Path,
    test_file: Path,
    max_statements: int,
    include_safe_harness: bool = False,
    mysql_test_root: Path | None = None,
) -> str | None:
    if mysql_test_root is not None:
        path_reason = layout_exclusion_reason(mysql_test_root, test_file, name)
        if path_reason:
            return path_reason
    if name.count("/") > 1:
        return "nested-suite-layout"
    if not TEST_NAME.fullmatch(name):
        return "invalid-manifest-name"
    suites = SAFE_HARNESS_SUITES if include_safe_harness else COMPATIBILITY_SUITES
    if (
        "/" in name
        and name.split("/", 1)[0] not in suites
        and not (include_safe_harness and name in SAFE_HARNESS_CASES)
    ):
        return "outside-contract-suite"
    if not result_file.is_file():
        return "missing-result"
    if statements == 0:
        return "no-direct-sql"
    if statements > max_statements:
        return "over-statement-limit"
    if DELIMITER_DIRECTIVE.search(text):
        return "custom-delimiter"
    if include_safe_harness:
        if unresolved_source_reason(mysql_test_root or test_file.parent, text):
            return "unresolved-include"
        if has_unresolved_dynamic_harness(text):
            return "unresolved-dynamic"
        if UNSAFE_HARNESS_DIRECTIVE.search(text):
            return "harness-side-effect"
    elif DEPENDENT_DIRECTIVE.search(text):
        return "harness-dependency"
    if companion_file_exists(test_file):
        return "server-options"
    if TOPOLOGY_NAME.search(name):
        return "topology-suite"
    if UNSUPPORTED_SQL.search(sql) or SPECIALIZED_SQL.search(sql):
        return "outside-contract"
    if has_table_partition(sql):
        return "outside-contract"
    if SERVER_CONFIGURATION_SQL.search(sql):
        return "server-configuration"
    return None

def discover_cases(
    suite_root: Path,
    scope: str,
    max_statements: int,
    layout: str = "mariadb",
    include_safe_harness: bool = False,
) -> list[DiscoveryCase]:
    mysql_test_root = suite_root / "mysql-test"
    if scope == "main":
        test_files = sorted(
            (mysql_test_root / ("main" if layout == "mariadb" else "t")).glob("*.test")
        )
    elif scope == "all":
        # Inventory every upstream .test file, including helpers and layouts
        # that cannot be executed by the external MTR contract.
        test_files = sorted(mysql_test_root.rglob("*.test"))
    else:
        raise ValueError(f"unsupported discovery scope: {scope}")
    cases: list[DiscoveryCase] = []
    for test_file in test_files:
        name = test_name(mysql_test_root, test_file, layout)
        if not name:
            continue
        result_file = result_file_for_test(mysql_test_root, test_file, layout)
        text = test_file.read_text(encoding="utf-8", errors="replace")
        analysis_text = (
            expanded_mtr_text(mysql_test_root, text, frozenset({test_file.resolve()}))
            if include_safe_harness
            else text
        )
        direct_sql = sql_code(without_mtr_comments(text))
        sql = direct_sql if analysis_text == text else sql_code(without_mtr_comments(analysis_text))
        statements = sql_statement_count(analysis_text)
        reason = exclusion_reason(
            name,
            analysis_text,
            sql,
            statements,
            result_file,
            test_file,
            max_statements,
            include_safe_harness,
            mysql_test_root,
        )
        cases.append(
            DiscoveryCase(
                name=name,
                feature=classify_feature(sql),
                statements=statements,
                test_sha256=sha256_file(test_file),
                result_sha256=sha256_file(result_file) if result_file.is_file() else "",
                test_file=str(test_file),
                result_file=str(result_file),
                exclusion=reason,
                project_scope=classify_scope(direct_sql),
            )
        )
    return apply_duplicate_exclusions(cases)


def rotating_selection(cases: list[DiscoveryCase], offset: int, limit: int) -> list[DiscoveryCase]:
    if not cases or limit <= 0:
        return []
    ordered = sorted(cases, key=lambda case: case.name)
    start = offset % len(ordered)
    count = min(limit, len(ordered))
    return [ordered[(start + index) % len(ordered)] for index in range(count)]


def manifest_text(cases: list[DiscoveryCase], revision: str) -> str:
    lines = [
        "# Generated MTR discovery batch; not the strict compatibility manifest.",
        f"# Pinned source revision: {revision}",
        "# Columns: test feature test_sha256 result_sha256",
    ]
    lines.extend(
        f"{case.name} {case.feature} {case.test_sha256} {case.result_sha256}"
        for case in cases
    )
    return "\n".join(lines) + "\n"


def render_discovery_markdown(report: dict) -> str:
    counts = report["counts"]
    planned = report["testing_plan_counts"]
    lines = [
        f"# {report['baseline_label']} MTR discovery inventory",
        "",
        f"- Source revision: `{report['source_revision']}`",
        f"- Scope: `{report['scope']}`",
        f"- Test files inspected: {counts['inspected']}",
        f"- Runnable complete-file candidates: {counts['candidates']}",
        f"- Candidate SQL statements: {counts['candidate_statements']}",
        f"- Tests selected in this batch: {counts['selected']}",
        f"- SQL statements selected in this batch: {counts['selected_statements']}",
        "",
        "A runnable candidate is not a passing test. Testing intent is recorded for every "
        "path in `mariadb-mtr-testing-plan.json`; blocked and unresolved files are not discarded.",
        "",
        "## Required testing backlog",
        "",
        f"- Required files: {planned['required']}",
        f"- Ready for complete-file execution: {planned['ready']}",
        f"- Blocked pending harness, scope review, or derived extraction: {planned['blocked']}",
        f"- Explicit hash-reviewed scope exemptions: {planned['not_required']}",
        f"- Files with partial derived scenarios: {planned['partial_derived_files']}",
        "",
        "Scope categories: " + ", ".join(
            f"`{status}`: {count}" for status, count in sorted(planned["scope"].items())
        ),
        "",
        "Enrollment is exhaustive; semantic review and execution coverage are not. "
        "A directory, harness limitation, or unknown SQL never authorizes an exemption.",
        "",
        "## Candidate test-shape coverage",
        "",
        "| Feature | Tests | SQL statements |",
        "| --- | ---: | ---: |",
    ]
    for feature, coverage in sorted(report["feature_coverage"].items()):
        lines.append(f"| `{feature}` | {coverage['tests']} | {coverage['statements']} |")
    lines.extend(
        [
            "",
            "## Execution filters before reviewed overrides",
            "",
            "| Reason | Tests |",
            "| --- | ---: |",
        ]
    )
    for reason, count in sorted(report["exclusions"].items(), key=lambda item: (-item[1], item[0])):
        lines.append(f"| `{reason}` | {count} |")
    lines.extend(
        [
            "",
            "## Selected audit batch",
            "",
            "| Test | Feature | SQL statements |",
            "| --- | --- | ---: |",
        ]
    )
    for case in report["selected"]:
        lines.append(f"| `{case['name']}` | `{case['feature']}` | {case['statements']} |")
    return "\n".join(lines) + "\n"


def inventory_record(case: DiscoveryCase, suite_root: Path) -> dict:
    return {
        "path": Path(case.test_file).relative_to(suite_root / "mysql-test").as_posix(),
        "name": case.name, "feature": case.feature, "statements": case.statements,
        "test_sha256": case.test_sha256, "result_sha256": case.result_sha256,
        "test_file": case.test_file, "result_file": case.result_file,
        "exclusion": case.exclusion,
    }


def write_inventory(args: argparse.Namespace) -> int:
    suite_root = args.suite_root.resolve()
    output_dir = args.output_dir.resolve()
    output_dir.mkdir(parents=True, exist_ok=True)
    cases = discover_cases(
        suite_root,
        args.scope,
        args.max_statements,
        args.mtr_layout,
        args.include_safe_harness,
    )
    complete_manifests = args.complete_manifest
    if complete_manifests is None:
        complete_manifests = [
            Path("tests/mariadb-mtr-allowlist.txt"),
            Path("tests/mariadb-mtr-scope.txt"),
            *sorted(Path("tests/query_coverage_mtr").glob("*.txt")),
        ]
    testing_plan, candidates = build_testing_plan(
        suite_root, cases, args.source_revision, args.scope_reviews,
        complete_manifests, args.derived_manifest, args.scope,
    )
    selected = rotating_selection(candidates, args.offset, args.limit)
    exclusions = Counter(case.exclusion for case in cases if case.exclusion)
    feature_coverage: dict[str, dict[str, int]] = {}
    for case in candidates:
        coverage = feature_coverage.setdefault(case.feature, {"tests": 0, "statements": 0})
        coverage["tests"] += 1
        coverage["statements"] += case.statements
    report = {
        "schema": "my-sqweel.mtr-discovery.v3",
        "baseline_label": args.baseline_label,
        "source_revision": args.source_revision,
        "scope": args.scope,
        "offset": args.offset,
        "limit": args.limit,
        "max_statements": args.max_statements,
        "include_safe_harness": args.include_safe_harness,
        "counts": {
            "inspected": len(cases),
            "candidates": len(candidates),
            "candidate_statements": sum(case.statements for case in candidates),
            "selected": len(selected),
            "selected_statements": sum(case.statements for case in selected),
        },
        "exclusions": dict(exclusions),
        "feature_coverage": feature_coverage,
        "inventory": [inventory_record(case, suite_root) for case in cases],
        "candidates": [inventory_record(case, suite_root) for case in candidates],
        "selected": [inventory_record(case, suite_root) for case in selected],
        "testing_plan_counts": testing_plan["counts"],
    }
    manifest = output_dir / "mariadb-mtr-discovery-manifest.txt"
    manifest.write_text(manifest_text(selected, args.source_revision))
    (output_dir / "mariadb-mtr-testing-plan.json").write_text(
        json.dumps(testing_plan, indent=2) + "\n"
    )
    (output_dir / "mariadb-mtr-discovery.json").write_text(json.dumps(report, indent=2) + "\n")
    markdown = render_discovery_markdown(report)
    (output_dir / "mariadb-mtr-discovery.md").write_text(markdown)
    print(markdown, end="")
    return 0


def write_promotion_manifest(args: argparse.Namespace) -> int:
    report = json.loads(args.compat_report.read_text())
    coverage_kind = report.get("coverage_kind", "complete-upstream")
    if coverage_kind != "complete-upstream":
        raise ValueError(
            f"cannot promote {coverage_kind!r} reports; only complete-upstream reports are promotable"
        )
    promoted = [
        result
        for result in report.get("results", [])
        if result.get("baseline") == "pass"
        and result.get("mysqweel") == "pass"
    ]
    lines = [
        "# Generated candidates that passed the audited MariaDB and MySqweel targets.",
        f"# Pinned source revision: {report.get('source_revision', 'unknown')}",
        "# Review compatibility-boundary fit before merging into the strict manifest.",
        "# Columns: test feature test_sha256 result_sha256",
    ]
    lines.extend(
        f"{result['test']} {result['feature']} {result['test_sha256']} "
        f"{result['result_sha256']}"
        for result in promoted
    )
    args.promote_manifest.parent.mkdir(parents=True, exist_ok=True)
    args.promote_manifest.write_text("\n".join(lines) + "\n")
    print(f"Promotable complete upstream files: {len(promoted)}")
    return 0


def parser() -> argparse.ArgumentParser:
    result = argparse.ArgumentParser(description=__doc__)
    result.add_argument("--suite-root", type=Path)
    result.add_argument("--output-dir", type=Path, default=Path("artifacts/mariadb-mtr-discovery"))
    result.add_argument("--scope", choices=("main", "all"), default="main")
    result.add_argument("--offset", type=int, default=0)
    result.add_argument("--limit", type=int, default=100)
    result.add_argument("--max-statements", type=int, default=200)
    result.add_argument(
        "--include-safe-harness",
        action="store_true",
        help=(
            "follow literal source/include files and allow non-mutating MTR harness "
            "directives while retaining contract and side-effect exclusions"
        ),
    )
    result.add_argument("--source-revision", default="mariadb-10.11.7-2ubuntu2")
    result.add_argument("--mtr-layout", choices=("mariadb",), default="mariadb")
    result.add_argument("--baseline-label", default="MariaDB")
    result.add_argument("--scope-reviews", type=Path, default=Path("tests/mariadb-mtr-scope-reviews.json"))
    result.add_argument("--complete-manifest", type=Path, action="append",
                        help="reviewed complete manifest; repeat to override the default strict/focused manifests")
    result.add_argument("--derived-manifest", type=Path, default=Path("tests/mariadb-mtr-derived.json"))
    result.add_argument("--compat-report", type=Path)
    result.add_argument("--promote-manifest", type=Path)
    return result


def main(args: argparse.Namespace) -> int:
    if args.compat_report or args.promote_manifest:
        if not args.compat_report or not args.promote_manifest:
            raise ValueError("--compat-report and --promote-manifest must be used together")
        return write_promotion_manifest(args)
    if not args.suite_root:
        raise ValueError("--suite-root is required for discovery")
    return write_inventory(args)


if __name__ == "__main__":
    try:
        raise SystemExit(main(parser().parse_args()))
    except (FileNotFoundError, ValueError) as error:
        print(f"MTR discovery: {error}", file=__import__("sys").stderr)
        raise SystemExit(2)
