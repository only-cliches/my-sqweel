#!/usr/bin/env python3
"""Report auditable SQL feature and semantic coverage witnesses."""
from __future__ import annotations

import argparse
import hashlib
import json
from pathlib import Path
import sys
import tomllib

ROOT = Path(__file__).resolve().parents[1]
DEFAULT_SPEC = ROOT / "tests/sql-coverage-spec.toml"
DEFAULT_FIXTURES = ROOT / "tests/query_cases"
DIMENSIONS = {"feature", "interaction", "three_way_interaction", "data_boundary", "error_behavior", "execution_path", "stateful"}


class CoverageError(ValueError):
    pass


def _strings(value, label):
    if not isinstance(value, list) or any(not isinstance(item, str) or not item for item in value):
        raise CoverageError(f"{label} must be an array of nonempty strings")
    return value


def load_spec(path: Path) -> list[dict]:
    try:
        with path.open("rb") as stream:
            spec = tomllib.load(stream)
    except (OSError, tomllib.TOMLDecodeError) as error:
        raise CoverageError(f"cannot read coverage spec: {error}") from error
    if set(spec) != {"obligations"} or not isinstance(spec["obligations"], list):
        raise CoverageError("spec must contain only an [[obligations]] array")
    obligations, ids = [], set()
    allowed = {"id", "dimension", "status", "case", "test", "generated", "sql_contains", "setup_contains", "expect_error", "error_sql_contains"}
    for index, item in enumerate(spec["obligations"]):
        label = f"obligations[{index}]"
        if not isinstance(item, dict) or set(item) - allowed or not {"id", "dimension", "status"} <= set(item):
            raise CoverageError(f"{label} has missing or unknown fields")
        if any(not isinstance(item[key], str) or not item[key] for key in ("id", "dimension", "status")):
            raise CoverageError(f"{label} id, dimension, and status must be nonempty strings")
        if item.get("case") is not None and (not isinstance(item["case"], str) or not item["case"]):
            raise CoverageError(f"{label}.case must be a nonempty string when provided")
        for source in ("test", "generated"):
            if item.get(source) is not None and (not isinstance(item[source], str) or not item[source]):
                raise CoverageError(f"{label}.{source} must be a nonempty string when provided")
        if sum(bool(item.get(source)) for source in ("case", "test", "generated")) > 1:
            raise CoverageError(f"{label} must choose one witness source")
        if item["id"] in ids:
            raise CoverageError(f"duplicate obligation id: {item['id']}")
        ids.add(item["id"])
        if item["dimension"] not in DIMENSIONS:
            raise CoverageError(f"unknown dimension: {item['dimension']}")
        if item["status"] != "supported":
            raise CoverageError(f"unknown status: {item['status']}")
        if "expect_error" in item and (item["expect_error"] is not True or item["dimension"] != "error_behavior"):
            raise CoverageError(f"{label}.expect_error must be true on an error_behavior obligation")
        for key in ("sql_contains", "setup_contains", "error_sql_contains"):
            if key in item:
                _strings(item[key], f"{label}.{key}")
        if "error_sql_contains" in item and item["dimension"] != "error_behavior":
            raise CoverageError(f"{label}.error_sql_contains is only valid for error_behavior")
        if item.get("case") and not (item.get("sql_contains") or item.get("setup_contains")):
            raise CoverageError(f"{label} needs SQL or setup evidence")
        if item["dimension"] == "data_boundary" and item.get("case") and not (item.get("sql_contains") and item.get("setup_contains")):
            raise CoverageError(f"{label} needs both SQL and setup evidence")
        if item["dimension"] in {"interaction", "three_way_interaction"} and item.get("case") and not item.get("sql_contains"):
            raise CoverageError(f"{label} needs SQL interaction evidence")
        if item["dimension"] == "error_behavior" and item.get("case") and not (item.get("expect_error") and item.get("error_sql_contains")):
            raise CoverageError(f"{label} needs an expected error statement")
        obligations.append(item)
    return obligations


def load_fixtures(directory: Path) -> dict[str, dict]:
    fixtures = {}
    for path in sorted(directory.glob("*.json")):
        try:
            fixture = json.loads(path.read_text(encoding="utf-8"))
        except (OSError, ValueError) as error:
            raise CoverageError(f"cannot read fixture {path.name}: {error}") from error
        if not isinstance(fixture, dict) or not isinstance(fixture.get("id"), str) or not fixture["id"]:
            raise CoverageError(f"fixture {path.name} has no valid id")
        if fixture["id"] != path.stem:
            raise CoverageError(f"fixture id does not match filename: {path.name}")
        if fixture["id"] in fixtures:
            raise CoverageError(f"duplicate fixture id: {fixture['id']}")
        fixtures[fixture["id"]] = fixture
    return fixtures


def load_path_results(directory: Path | None) -> dict[str, dict]:
    if directory is None:
        return {}
    results = {}
    for path in sorted(directory.glob("*.json")):
        try:
            row = json.loads(path.read_text(encoding="utf-8"))
        except (OSError, ValueError) as error:
            raise CoverageError(f"cannot read path result {path.name}: {error}") from error
        if not isinstance(row, dict) or not isinstance(row.get("covered"), list) or any(not isinstance(value, str) for value in row["covered"]) or not isinstance(row.get("reference_verified"), bool):
            raise CoverageError(f"invalid path result {path.name}")
        results[path.stem] = row
    return results


def load_optional_json(path: Path | None, label: str) -> dict | None:
    if path is None or not path.is_file():
        return None
    try:
        value = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, ValueError) as error:
        raise CoverageError(f"cannot read {label}: {error}") from error
    if not isinstance(value, dict):
        raise CoverageError(f"invalid {label}")
    return value


def build_report(spec_path: Path = DEFAULT_SPEC, fixture_dir: Path = DEFAULT_FIXTURES, results_path: Path | None = None, path_results: Path | None = None, typed_results: Path | None = None, code_coverage: Path | None = None, mutation_results: Path | None = None, fuzz_results: Path | None = None) -> dict:
    obligations = load_spec(spec_path)
    fixtures = load_fixtures(fixture_dir)
    test_results = load_path_results(path_results)
    generated = load_optional_json(typed_results, "generated coverage") or {}
    if generated and (any(not isinstance(generated.get(key), list) for key in ("data_conditions", "features", "interactions", "failures")) or not isinstance(generated.get("differential_checked"), bool)):
        raise CoverageError("invalid generated coverage report")
    code_stats = load_optional_json(code_coverage, "code coverage")
    if code_stats is not None and (code_stats.get("kind") != "llvm_source_coverage" or not isinstance(code_stats.get("branches"), dict) or not isinstance(code_stats.get("regions"), dict)):
        raise CoverageError("invalid code coverage summary")
    mutation_stats = load_optional_json(mutation_results, "mutation results")
    if mutation_stats is not None and (mutation_stats.get("kind") != "cargo_mutants_sample" or not isinstance(mutation_stats.get("totals"), dict)):
        raise CoverageError("invalid mutation summary")
    fuzz_stats = load_optional_json(fuzz_results, "fuzz results")
    if fuzz_stats is not None and (fuzz_stats.get("kind") != "libfuzzer_runs" or not isinstance(fuzz_stats.get("totals"), dict) or not isinstance(fuzz_stats.get("runs"), list)):
        raise CoverageError("invalid fuzz summary")
    results = {}
    if results_path is not None:
        try:
            raw_results = json.loads(results_path.read_text(encoding="utf-8"))
        except (OSError, ValueError) as error:
            raise CoverageError(f"cannot read parity results: {error}") from error
        if not isinstance(raw_results, list) or any(not isinstance(row, dict) or not isinstance(row.get("id"), str) for row in raw_results):
            raise CoverageError("parity results must be an array of rows with ids")
        results = {row["id"]: row.get("result", {}) for row in raw_results if isinstance(row.get("result"), dict)}
    mismatched_cases = sorted(case for case, result in results.items() if result.get("status") == "mismatch")
    unknown_cases = sorted({item["case"] for item in obligations if item.get("case")} - fixtures.keys())
    if unknown_cases:
        raise CoverageError(f"unknown cases: {', '.join(unknown_cases)}")

    by_dimension = {
        dimension: {"total": 0, "fixture_evidenced": 0, "test_evidenced": 0, "parity_verified": 0, "uncovered": [], "awaiting_parity": []}
        for dimension in sorted(DIMENSIONS)
    }
    for item in obligations:
        dimension = by_dimension[item["dimension"]]
        dimension["total"] += 1
        if item.get("test"):
            result = test_results.get(item["test"], {})
            if item["id"] in result.get("covered", []):
                dimension["test_evidenced"] += 1
                if result.get("reference_verified"):
                    dimension["parity_verified"] += 1
                else:
                    dimension["awaiting_parity"].append(item["id"])
            else:
                dimension["uncovered"].append(item["id"])
            continue
        if item.get("generated"):
            covered = item["generated"] in generated.get("data_conditions", []) + generated.get("features", []) + generated.get("interactions", [])
            if covered:
                dimension["test_evidenced"] += 1
                if generated["differential_checked"] and not generated["failures"]:
                    dimension["parity_verified"] += 1
                else:
                    dimension["awaiting_parity"].append(item["id"])
            else:
                dimension["uncovered"].append(item["id"])
            continue
        fixture = fixtures.get(item.get("case"), {})
        steps = fixture.get("steps", [])
        checks = fixture.get("checks", [])
        substantive = isinstance(steps, list) and bool(steps) and all(
            isinstance(step, dict) and isinstance(step.get("sql"), str) and step["sql"].strip()
            for step in steps
        )
        statements = steps + checks if isinstance(steps, list) and isinstance(checks, list) else []
        sql = "\n".join(step["sql"] for step in statements if isinstance(step, dict) and isinstance(step.get("sql"), str))
        setup_statements = fixture.get("setup", [])
        setup = "\n".join(value for value in setup_statements if isinstance(value, str)) if isinstance(setup_statements, list) else ""
        matching_errors = [step for step in statements if isinstance(step, dict) and step.get("expect_error") is True]
        error_witnessed = "expect_error" not in item or any(
            all(text in step.get("sql", "") for text in item.get("error_sql_contains", item.get("sql_contains", [])))
            for step in matching_errors
        )
        structure_witnessed = item["dimension"] not in {"feature", "interaction", "three_way_interaction", "execution_path"} or any(
            all(text in step.get("sql", "") for text in item.get("sql_contains", []))
            for step in statements
            if isinstance(step, dict)
        )
        # shortcut: text witnesses need review, add AST/data instrumentation when the spec grows.
        witnessed = bool(item.get("case")) and substantive and all(text in sql for text in item.get("sql_contains", [])) and all(
            text in setup for text in item.get("setup_contains", [])
        ) and error_witnessed and structure_witnessed
        if witnessed:
            dimension["fixture_evidenced"] += 1
            result = results.get(item["case"], {})
            if result.get("status") == "pass" and "mysqweel" in result:
                dimension["parity_verified"] += 1
            else:
                dimension["awaiting_parity"].append(item["id"])
        else:
            dimension["uncovered"].append(item["id"])
    total = len(obligations)
    witnessed = sum(value["fixture_evidenced"] for value in by_dimension.values())
    tested = sum(value["test_evidenced"] for value in by_dimension.values())
    verified = sum(value["parity_verified"] for value in by_dimension.values())
    return {
        "schema": "my-sqweel.sql-coverage.v1",
        "spec_sha256": hashlib.sha256(spec_path.read_bytes()).hexdigest(),
        "claim": "Declared witnesses are auditable baseline evidence, not proof of full SQL semantic coverage.",
        "fixtures": len(fixtures),
        "differential_cases": sum(1 for result in results.values() if "mysqweel" in result),
        "generated_seeds": len(generated.get("seeds", [])),
        "generated_structures": len({case["shape"] if isinstance(case.get("shape"), str) else tuple(case["features"]) for case in generated.get("cases", []) if isinstance(case, dict) and isinstance(case.get("features"), list) and all(isinstance(f, str) for f in case["features"])}),
        "implementation": {"code_coverage": code_stats, "mutation": mutation_stats, "fuzz": fuzz_stats},
        "mismatched_cases": mismatched_cases,
        "totals": {"obligations": total, "fixture_evidenced": witnessed, "test_evidenced": tested, "uncovered": total - witnessed - tested, "parity_verified": verified, "awaiting_parity": witnessed + tested - verified},
        "dimensions": by_dimension,
    }


def render_markdown(report: dict) -> str:
    lines = [
        "# SQL compatibility coverage",
        "",
        report["claim"],
        "",
        "| Dimension | Declared | Fixture evidence | Test evidence | MariaDB parity | Gaps |",
        "| --- | ---: | ---: | ---: | ---: | ---: |",
    ]
    for name, data in report["dimensions"].items():
        lines.append(f"| {name} | {data['total']} | {data['fixture_evidenced']} | {data['test_evidenced']} | {data['parity_verified']} | {len(data['uncovered'])} |")
    lines.extend(["", "## Uncovered obligations", ""])
    for name, data in report["dimensions"].items():
        if data["uncovered"]:
            lines.append(f"- {name}: {', '.join(data['uncovered'])}")
    if report["totals"]["uncovered"] == 0:
        lines.append("None.")
    lines.extend(["", "## Awaiting MariaDB parity", ""])
    for name, data in report["dimensions"].items():
        if data["awaiting_parity"]:
            lines.append(f"- {name}: {', '.join(data['awaiting_parity'])}")
    if report["totals"]["awaiting_parity"] == 0:
        lines.append("None.")
    lines.extend(["", f"Differential cases: {report['differential_cases']}; generated seeds: {report['generated_seeds']}; generated structures: {report['generated_structures']}; unresolved mismatches: {len(report['mismatched_cases'])}."])
    code = report["implementation"]["code_coverage"]
    if code:
        if code.get("status") == "missing":
            lines.append("SQL implementation coverage: missing instrumentation output.")
        else:
            branches, regions = code["branches"], code["regions"]
            lines.append(f"SQL implementation coverage: {branches.get('covered', 0)}/{branches.get('total', 0)} branches; {regions.get('covered', 0)}/{regions.get('total', 0)} regions.")
    mutation = report["implementation"]["mutation"]
    if mutation:
        if mutation.get("status") == "missing":
            lines.append("Mutation sample: missing tool output.")
        else:
            totals = mutation["totals"]
            lines.append(f"Mutation sample: {totals.get('caught', 0)} caught, {totals.get('missed', 0)} missed, {totals.get('timeout', 0)} timed out.")
    fuzz = report["implementation"]["fuzz"]
    if fuzz:
        totals = fuzz["totals"]
        lines.append(f"Fuzz targets: {totals.get('completed', 0)}/{totals.get('targets', 0)} completed; {totals.get('crashes', 0)} crashes; {totals.get('failed', 0)} failed.")
    if report["mismatched_cases"]:
        lines.append(", ".join(report["mismatched_cases"]))
    return "\n".join(lines) + "\n"


def main(argv=None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--spec", type=Path, default=DEFAULT_SPEC)
    parser.add_argument("--fixtures", type=Path, default=DEFAULT_FIXTURES)
    parser.add_argument("--results", type=Path, help="QUERY_COVERAGE_REPORT JSON for parity status")
    parser.add_argument("--path-results", type=Path, help="directory of execution-path test results")
    parser.add_argument("--typed-results", type=Path, help="generated typed SQL coverage report")
    parser.add_argument("--code-coverage", type=Path, help="LLVM source coverage summary")
    parser.add_argument("--mutation-results", type=Path, help="cargo-mutants sample summary")
    parser.add_argument("--fuzz-results", type=Path, help="bounded libFuzzer summary")
    parser.add_argument("--format", choices=("json", "markdown"), default="json")
    args = parser.parse_args(argv)
    try:
        report = build_report(args.spec, args.fixtures, args.results, args.path_results, args.typed_results, args.code_coverage, args.mutation_results, args.fuzz_results)
    except CoverageError as error:
        print(f"sql coverage: {error}", file=sys.stderr)
        return 2
    print(render_markdown(report) if args.format == "markdown" else json.dumps(report, indent=2, sort_keys=True), end="" if args.format == "markdown" else "\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
