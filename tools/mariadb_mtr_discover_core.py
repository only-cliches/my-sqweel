#!/usr/bin/env python3
"""Inventory pinned MariaDB MTR tests and build exhaustive or batched audit manifests.

Discovery is intentionally non-gating. It identifies complete upstream files
that are plausible external-server compatibility candidates, while the strict
manifest remains limited to cases proven to pass on both MariaDB and MySqweel.
"""

from __future__ import annotations

import argparse
import json
import re
from collections import Counter
from dataclasses import asdict, dataclass
from pathlib import Path

try:
    from tools.mariadb_mtr_core import (
        TEST_NAME,
        sha256_file,
        sql_statement_count,
    )
except ModuleNotFoundError:  # Direct execution adds tools/, not the repository root, to sys.path.
    from mariadb_mtr_core import TEST_NAME, sha256_file, sql_statement_count


DEPENDENT_DIRECTIVE = re.compile(
    r"(?im)^\s*(?:--\s*)?(?:source|include|let|eval|exec|system|perl|connect|connection|"
    r"send|reap|sleep|real_sleep|shutdown|restart|write_file|append_file|remove_file|"
    r"copy_file|move_file|chmod|mkdir|rmdir|cat_file)\b"
)
UNSAFE_HARNESS_DIRECTIVE = re.compile(
    r"(?im)^\s*(?:--\s*)?(?:exec|system|perl|shutdown|restart|write_file|append_file|"
    r"remove_file|copy_file|move_file|chmod|mkdir|rmdir|cat_file)\b"
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
    r"(?is)\b(?:PARTITION(?:ING)?|FULLTEXT|SPATIAL)\b|"
    r"\bENGINE\s*=\s*(?:ARCHIVE|ARIA|BLACKHOLE|CSV|MEMORY|HEAP|MYISAM|FEDERATED|NDB)\b|"
    r"\bLOAD\s+(?:DATA|XML)\s+(?:LOCAL\s+)?INFILE\b|"
    r"\bINTO\s+(?:OUTFILE|DUMPFILE)\b"
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


def without_mtr_comments(text: str) -> str:
    return "\n".join(
        line
        for line in text.splitlines()
        if not line.lstrip().startswith(("#", "--"))
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
    Dynamic source paths are left to the baseline execution audit.
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
            if source_file in seen or not source_file.is_file():
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
    if parts and parts[0] == "suite" and "t" in parts[1:-1]:
        test_directory = parts.index("t", 1)
        suite = "/".join(parts[1:test_directory])
        return f"{suite}/{test_file.stem}"
    return None


def result_file_for_test(mysql_test_root: Path, test_file: Path, layout: str = "mariadb") -> Path:
    relative = test_file.relative_to(mysql_test_root)
    parts = list(relative.parts)
    if layout == "mariadb" and parts[0] == "main":
        return test_file.with_suffix(".result")
    test_directory = parts.index("t")
    parts[test_directory] = "r"
    return mysql_test_root.joinpath(*parts).with_suffix(".result")


def companion_file_exists(test_file: Path) -> bool:
    stem = test_file.with_suffix("")
    companions = (
        stem.with_suffix(".opt"),
        stem.with_suffix(".cnf"),
        test_file.with_name(f"{test_file.stem}-master.opt"),
        test_file.with_name(f"{test_file.stem}-slave.opt"),
    )
    return any(path.exists() for path in companions)


def exclusion_reason(
    name: str,
    text: str,
    sql: str,
    statements: int,
    result_file: Path,
    test_file: Path,
    max_statements: int,
    include_safe_harness: bool = False,
) -> str | None:
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
    patterns = [mysql_test_root / ("main" if layout == "mariadb" else "t")]
    if scope == "all":
        patterns.extend((mysql_test_root / "suite").glob("**/t"))
    cases: list[DiscoveryCase] = []
    for test_dir in patterns:
        if not test_dir.is_dir():
            continue
        for test_file in sorted(test_dir.glob("*.test")):
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
            sql = without_mtr_comments(analysis_text)
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
                )
            )
    return cases


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
    lines = [
        f"# {report['baseline_label']} MTR discovery inventory",
        "",
        f"- Source revision: `{report['source_revision']}`",
        f"- Scope: `{report['scope']}`",
        f"- Test files inspected: {counts['inspected']}",
        f"- Static audit candidates: {counts['candidates']}",
        f"- Candidate SQL statements: {counts['candidate_statements']}",
        f"- Tests selected in this batch: {counts['selected']}",
        f"- SQL statements selected in this batch: {counts['selected_statements']}",
        "",
        "Static candidacy only means a complete file appears viable in external-server mode. "
        "A case is promotable only after the generated batch passes against both the baseline and MySqweel.",
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
            "## Static exclusions",
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
    candidates = [case for case in cases if case.exclusion is None]
    selected = rotating_selection(candidates, args.offset, args.limit)
    exclusions = Counter(case.exclusion for case in cases if case.exclusion)
    feature_coverage: dict[str, dict[str, int]] = {}
    for case in candidates:
        coverage = feature_coverage.setdefault(case.feature, {"tests": 0, "statements": 0})
        coverage["tests"] += 1
        coverage["statements"] += case.statements
    report = {
        "schema": "my-sqweel.mtr-discovery.v2",
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
        "inventory": [asdict(case) for case in cases],
        "candidates": [asdict(case) for case in candidates],
        "selected": [asdict(case) for case in selected],
    }
    manifest = output_dir / "mariadb-mtr-discovery-manifest.txt"
    manifest.write_text(manifest_text(selected, args.source_revision))
    (output_dir / "mariadb-mtr-discovery.json").write_text(json.dumps(report, indent=2) + "\n")
    markdown = render_discovery_markdown(report)
    (output_dir / "mariadb-mtr-discovery.md").write_text(markdown)
    print(markdown, end="")
    return 0


def write_promotion_manifest(args: argparse.Namespace) -> int:
    report = json.loads(args.compat_report.read_text())
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
