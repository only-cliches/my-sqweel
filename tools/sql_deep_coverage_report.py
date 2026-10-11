#!/usr/bin/env python3
"""Summarize scheduled LLVM and cargo-mutants evidence for the SQL dashboard."""

from __future__ import annotations

import argparse
import json
import tempfile
from pathlib import Path
from typing import Any


def read_json(path: Path) -> dict[str, Any]:
    try:
        value = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        raise ValueError(f"cannot read JSON report {path}: {error}") from error
    if not isinstance(value, dict):
        raise ValueError(f"expected a JSON object in {path}")
    return value


def metric_summary(files: list[dict[str, Any]], key: str) -> dict[str, int | float | None]:
    covered = total = 0
    for file in files:
        summary = file.get("summary", {}).get(key, {})
        if not isinstance(summary, dict) or "count" not in summary or "covered" not in summary:
            continue
        total += int(summary["count"])
        covered += int(summary["covered"])
    return {
        "covered": covered,
        "total": total,
        "percent": round(covered * 100 / total, 2) if total else None,
    }


def llvm_report(source: dict[str, Any]) -> dict[str, Any]:
    data = source.get("data")
    if not isinstance(data, list):
        raise ValueError("LLVM report has no data array")
    files = [
        file
        for entry in data
        for file in entry.get("files", [])
        if isinstance(file, dict)
        and ("/src/sql/" in file.get("filename", "/") or file.get("filename", "").startswith("src/sql/"))
    ]
    if not files:
        raise ValueError("LLVM report contains no src/sql files")
    return {
        "schema": "my-sqweel.sql-code-coverage.v1",
        "kind": "llvm_source_coverage",
        "scope": "src/sql/**",
        "source_file_count": len(files),
        "regions": metric_summary(files, "regions"),
        "branches": metric_summary(files, "branches"),
    }


def missing_llvm_report() -> dict[str, Any]:
    empty = {"covered": 0, "total": 0, "percent": None}
    return {
        "schema": "my-sqweel.sql-code-coverage.v1",
        "kind": "llvm_source_coverage",
        "scope": "src/sql/**",
        "status": "missing",
        "source_file_count": 0,
        "regions": empty,
        "branches": empty,
    }


def mutation_report(source: dict[str, Any], exit_code: int | None) -> dict[str, Any]:
    counts = {
        name: int(source.get(name, 0))
        for name in ("total_mutants", "caught", "missed", "timeout", "unviable", "success")
    }
    decisive = counts["caught"] + counts["missed"]
    status = "missing" if not source else "complete" if exit_code in (0, 2, 3) else "partial"
    return {
        "schema": "my-sqweel.sql-mutation-coverage.v1",
        "kind": "cargo_mutants_sample",
        "tool": "cargo-mutants",
        "scope": "src/sql/engine/**/*.rs",
        "shard": "0/100",
        "status": status,
        "totals": {
            "tested": counts["total_mutants"],
            "caught": counts["caught"],
            "missed": counts["missed"],
            "timeout": counts["timeout"],
            "unviable": counts["unviable"],
            "success": counts["success"],
        },
        "mutation_score": round(counts["caught"] * 100 / decisive, 2) if decisive else None,
        "exit_code": exit_code,
    }


def fuzz_report(directory: Path) -> dict[str, Any]:
    targets = ("parser", "binding", "expression_eval", "query_execution", "dml_transactions", "mysql_wire")
    runs = []
    for target in targets:
        status_path = directory / "status" / f"{target}.txt"
        status = status_path.read_text(encoding="utf-8").strip() if status_path.is_file() else "missing"
        crash_dir = directory / "crashes" / target
        crashes = sum(1 for path in crash_dir.iterdir() if path.is_file()) if crash_dir.is_dir() else 0
        runs.append({"target": target, "status": status, "crashes": crashes})
    return {
        "schema": "my-sqweel.sql-fuzz-coverage.v1",
        "kind": "libfuzzer_runs",
        "budget_seconds_per_target": 45,
        "max_input_bytes": 65536,
        "runs": runs,
        "totals": {
            "targets": len(runs),
            "completed": sum(run["status"] == "passed" for run in runs),
            "failed": sum(run["status"] == "failed" for run in runs),
            "missing": sum(run["status"] == "missing" for run in runs),
            "crashes": sum(run["crashes"] for run in runs),
        },
    }


def self_check() -> None:
    llvm = llvm_report({"data": [{"files": [
        {"filename": "/repo/src/sql/engine.rs", "summary": {
            "regions": {"covered": 3, "count": 4},
            "branches": {"covered": 1, "count": 2},
        }},
        {"filename": "/repo/src/lib.rs", "summary": {
            "regions": {"covered": 99, "count": 100},
            "branches": {"covered": 99, "count": 100},
        }},
    ]}]})
    assert llvm["regions"] == {"covered": 3, "total": 4, "percent": 75.0}
    assert llvm["branches"] == {"covered": 1, "total": 2, "percent": 50.0}
    assert mutation_report({}, None)["mutation_score"] is None
    assert metric_summary([], "regions")["percent"] is None
    assert mutation_report({"caught": 2, "missed": 1}, 2)["mutation_score"] == 66.67
    with tempfile.TemporaryDirectory() as directory:
        assert fuzz_report(Path(directory))["totals"]["missing"] == 6


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("kind", choices=("llvm", "mutants", "fuzz"), nargs="?")
    parser.add_argument("--input", type=Path)
    parser.add_argument("--output", type=Path)
    parser.add_argument("--exit-code-file", type=Path)
    parser.add_argument("--self-check", action="store_true")
    args = parser.parse_args()

    if args.self_check:
        self_check()
        return 0
    if args.kind is None or args.output is None:
        parser.error("kind and --output are required unless --self-check is set")

    if args.kind == "llvm":
        if args.input is None:
            parser.error("--input is required for LLVM reports")
        report = llvm_report(read_json(args.input)) if args.input.is_file() else missing_llvm_report()
    elif args.kind == "fuzz":
        if args.input is None:
            parser.error("--input is required for fuzz reports")
        report = fuzz_report(args.input)
    elif args.input is not None and args.input.is_file():
        exit_code = None
        if args.exit_code_file and args.exit_code_file.is_file():
            exit_code = int(args.exit_code_file.read_text(encoding="utf-8").strip())
        report = mutation_report(read_json(args.input), exit_code)
    else:
        exit_code = None
        if args.exit_code_file and args.exit_code_file.is_file():
            exit_code = int(args.exit_code_file.read_text(encoding="utf-8").strip())
        report = mutation_report({}, exit_code)

    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
