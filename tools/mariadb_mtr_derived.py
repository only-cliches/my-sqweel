#!/usr/bin/env python3
"""Run contiguous, hash-pinned slices of upstream MariaDB MTR files.

Derived scenarios are deliberately a separate coverage track.  A manifest points
at immutable upstream files and line ranges; this module stages only those ranges
in a disposable MTR suite view, then delegates execution and comparison to the
normal MTR core runner.  No upstream file is copied into the repository or edited.
"""

from __future__ import annotations

import argparse
import copy
import hashlib
import json
import re
import shutil
import tempfile
from contextlib import contextmanager
from dataclasses import dataclass
from pathlib import Path
from typing import Iterator
from urllib.parse import urlsplit
try:
    from tools import mariadb_mtr_core as core
except ModuleNotFoundError:  # direct ``python tools/mariadb_mtr_derived.py`` invocation
    import mariadb_mtr_core as core


DEFAULT_MANIFEST = Path("tests/mariadb-mtr-derived.json")
DEFAULT_REPORT_DIR = Path("artifacts/mariadb-mtr-derived")
REVISION_RE = re.compile(r"^[0-9a-f]{40}$")
MANIFEST_SCHEMA = "my-sqweel.mtr-derived.v1"
ID_RE = re.compile(r"^[A-Za-z0-9_.-]+$")
SHA_RE = re.compile(r"^[0-9a-f]{64}$")


@dataclass(frozen=True)
class DerivedScenario:
    scenario_id: str
    feature: str
    source_revision: str
    source_url: str
    test_path: str
    result_path: str
    test_sha256: str
    result_sha256: str
    test_start: int
    test_end: int
    result_start: int
    result_end: int
    dependency_rationale: str

    @property
    def test_name(self) -> str:
        return self.scenario_id

    def provenance(self) -> dict[str, object]:
        return {
            "id": self.scenario_id,
            "feature": self.feature,
            "source_revision": self.source_revision,
            "source_url": self.source_url,
            "test_path": self.test_path,
            "result_path": self.result_path,
            "test_sha256": self.test_sha256,
            "result_sha256": self.result_sha256,
            "test_line_range": [self.test_start, self.test_end],
            "result_line_range": [self.result_start, self.result_end],
            "dependency_rationale": self.dependency_rationale,
        }


def _required_string(raw: object, label: str) -> str:
    if not isinstance(raw, str) or not raw.strip():
        raise ValueError(f"{label} must be a nonempty string")
    return raw.strip()


def _relative_source_path(value: object, label: str, suffix: str) -> str:
    path = _required_string(value, label).replace("\\", "/")
    candidate = Path(path)
    if candidate.is_absolute() or ".." in candidate.parts or not path.endswith(suffix):
        raise ValueError(f"{label} must be a relative {suffix} path without '..'")
    if not path.startswith("main/") and not path.startswith("suite/"):
        raise ValueError(f"{label} must be under mysql-test/main or mysql-test/suite")
    return path


def _line_range(raw: object, label: str) -> tuple[int, int]:
    if not isinstance(raw, list) or len(raw) != 2 or any(isinstance(v, bool) or not isinstance(v, int) for v in raw):
        raise ValueError(f"{label} must be [first,last] line numbers")
    first, last = raw
    if first < 1 or last < first:
        raise ValueError(f"{label} must be a nonempty inclusive range starting at 1 or later")
    return first, last


def _validate_url(url: str, label: str) -> None:
    parsed = urlsplit(url)
    if parsed.scheme not in {"http", "https"} or not parsed.netloc:
        raise ValueError(f"{label} must be an http(s) URL")

def _parse_scenario(raw: object, index: int) -> DerivedScenario:
    if not isinstance(raw, dict):
        raise ValueError(f"scenarios[{index}] must be an object")
    prefix = f"scenarios[{index}]"
    scenario_id = _required_string(raw.get("id"), f"{prefix}.id")
    if not ID_RE.fullmatch(scenario_id):
        raise ValueError(f"{prefix}.id contains unsupported characters")
    feature = _required_string(raw.get("feature"), f"{prefix}.feature")
    if not ID_RE.fullmatch(feature):
        raise ValueError(f"{prefix}.feature contains unsupported characters")
    revision = _required_string(raw.get("source_revision"), f"{prefix}.source_revision")
    if not REVISION_RE.fullmatch(revision):
        raise ValueError(f"{prefix}.source_revision must be an immutable 40-hex commit")
    source_url = _required_string(raw.get("source_url"), f"{prefix}.source_url")
    _validate_url(source_url, f"{prefix}.source_url")
    if revision not in source_url:
        raise ValueError(f"{prefix}.source_url must contain pinned source_revision")
    test_path = _relative_source_path(raw.get("test_path"), f"{prefix}.test_path", ".test")
    result_path = _relative_source_path(raw.get("result_path"), f"{prefix}.result_path", ".result")
    test_hash = _required_string(raw.get("test_sha256"), f"{prefix}.test_sha256")
    result_hash = _required_string(raw.get("result_sha256"), f"{prefix}.result_sha256")
    if not SHA_RE.fullmatch(test_hash) or not SHA_RE.fullmatch(result_hash):
        raise ValueError(f"{prefix} test_sha256/result_sha256 must be lowercase SHA-256 values")
    test_start, test_end = _line_range(raw.get("test_lines"), f"{prefix}.test_lines")
    result_start, result_end = _line_range(raw.get("result_lines"), f"{prefix}.result_lines")
    rationale = _required_string(raw.get("dependency_rationale"), f"{prefix}.dependency_rationale")
    return DerivedScenario(
        scenario_id, feature, revision, source_url, test_path, result_path,
        test_hash, result_hash, test_start, test_end, result_start, result_end, rationale,
    )


def parse_derived_manifest(path: Path) -> list[DerivedScenario]:
    """Parse and structurally validate the JSON manifest, without touching sources."""
    try:
        document = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        raise ValueError(f"cannot read derived manifest {path}: {error}") from error
    if not isinstance(document, dict) or document.get("schema") != MANIFEST_SCHEMA:
        raise ValueError(f"{path}: expected schema {MANIFEST_SCHEMA!r}")
    scenarios = document.get("scenarios")
    if not isinstance(scenarios, list) or not scenarios:
        raise ValueError(f"{path}: scenarios must be a nonempty array")
    parsed = [_parse_scenario(raw, index) for index, raw in enumerate(scenarios)]
    ids = [scenario.scenario_id for scenario in parsed]
    if len(set(ids)) != len(ids):
        raise ValueError(f"{path}: scenario ids must be unique")
    return parsed


def _source_file(suite_root: Path, relative: str) -> Path:
    mysql_test = (suite_root / "mysql-test").resolve()
    source = (mysql_test / relative).resolve()
    if mysql_test != source and mysql_test not in source.parents:
        raise ValueError(f"source path escapes mysql-test: {relative}")
    return source


def _slice(path: Path, first: int, last: int, label: str) -> bytes:
    """Return exact byte lines; hashes and staged files must not normalize text."""
    lines = path.read_bytes().splitlines(keepends=True)
    if last > len(lines):
        raise ValueError(f"{label} range {first}-{last} exceeds {path} ({len(lines)} lines)")
    content = b"".join(lines[first - 1:last])
    if not content.strip():
        raise ValueError(f"{label} range {first}-{last} is empty")
    return content


def validate_scenarios(suite_root: Path, scenarios: list[DerivedScenario]) -> None:
    """Validate full upstream hashes, inclusive bounds, and absence of includes."""
    names: set[str] = set()
    for scenario in scenarios:
        if scenario.scenario_id in names:
            raise ValueError(f"duplicate scenario id {scenario.scenario_id}")
        names.add(scenario.scenario_id)
        test_path = _source_file(suite_root, scenario.test_path)
        result_path = _source_file(suite_root, scenario.result_path)
        if not test_path.is_file() or not result_path.is_file():
            raise ValueError(f"missing source files for {scenario.scenario_id}: {test_path}, {result_path}")
        actual_test = core.sha256_file(test_path)
        actual_result = core.sha256_file(result_path)
        if actual_test != scenario.test_sha256:
            raise ValueError(f"upstream test hash mismatch for {scenario.scenario_id}: expected {scenario.test_sha256}, got {actual_test}")
        if actual_result != scenario.result_sha256:
            raise ValueError(f"upstream result hash mismatch for {scenario.scenario_id}: expected {scenario.result_sha256}, got {actual_result}")
        test_slice = _slice(test_path, scenario.test_start, scenario.test_end, f"{scenario.scenario_id} test")
        _slice(result_path, scenario.result_start, scenario.result_end, f"{scenario.scenario_id} result")
        # Decode only for directive inspection; staged bytes remain untouched.
        test_text = test_slice.decode("utf-8", errors="replace")
        if re.search(r"(?im)^\s*(?:--\s*)?(?:source|source_include|include)\b", test_text):
            raise ValueError(f"{scenario.scenario_id} test slice contains an include/source directive; choose a self-contained no-include slice")

 

def _symlink_runtime(source_mysql_test: Path, staged_mysql_test: Path) -> None:
    staged_mysql_test.mkdir(parents=True, exist_ok=True)
    # The generated main directory is the only mutable part of the view.
    # Official MTR runtime files and data are exposed through symlinks.
    for child in source_mysql_test.iterdir():
        if child.name == "main":
            continue
        target = staged_mysql_test / child.name
        if not target.exists() and not target.is_symlink():
            target.symlink_to(child.resolve(), target_is_directory=child.is_dir())
    # The core resolves the runner path before invoking it. Keep a private
    # executable copy so Perl's dirname-based runtime lookup stays staged;
    # its lib/include/std_data children remain symlinks above.
    runner_source = source_mysql_test / "mariadb-test-run.pl"
    runner_destination = staged_mysql_test / "mariadb-test-run.pl"
    if runner_source.is_file():
        if runner_destination.is_symlink():
            runner_destination.unlink()
        shutil.copy2(runner_source, runner_destination)


@contextmanager
def stage_derived_suite(suite_root: Path, scenarios: list[DerivedScenario]) -> Iterator[tuple[Path, dict[str, dict[str, object]]]]:
    """Yield an isolated suite view and provenance keyed by staged test name."""
    source_mysql_test = (suite_root / "mysql-test").resolve()
    if not source_mysql_test.is_dir():
        raise ValueError(f"MTR suite root has no mysql-test directory: {suite_root}")
    with tempfile.TemporaryDirectory(prefix="mysqweel-mtr-derived-") as temporary:
        stage_root = Path(temporary)
        # MTR resolves binaries, charsets, and shared data relative to the
        # installation prefix, not just mysql-test. Preserve that layout.
        for child in suite_root.resolve().iterdir():
            if child.name != "mysql-test":
                (stage_root / child.name).symlink_to(
                    child.resolve(), target_is_directory=child.is_dir()
                )
        staged_mysql_test = stage_root / "mysql-test"
        _symlink_runtime(source_mysql_test, staged_mysql_test)
        staged_main = staged_mysql_test / "main"
        staged_main.mkdir()
        provenance: dict[str, dict[str, object]] = {}
        for scenario in scenarios:
            test_source = _source_file(suite_root, scenario.test_path)
            result_source = _source_file(suite_root, scenario.result_path)
            (staged_main / f"{scenario.test_name}.test").write_bytes(
                _slice(test_source, scenario.test_start, scenario.test_end, f"{scenario.scenario_id} test")
            )
            (staged_main / f"{scenario.test_name}.result").write_bytes(
                _slice(result_source, scenario.result_start, scenario.result_end, f"{scenario.scenario_id} result")
            )
            provenance[scenario.scenario_id] = scenario.provenance()
        yield stage_root, provenance


def _runtime_args(args: argparse.Namespace, stage_root: Path) -> argparse.Namespace:
    delegated = copy.copy(args)
    delegated.suite_root = stage_root
    delegated.allowlist = stage_root / "derived-allowlist.txt"
    # Complete-file manifests cannot be merged into this separate track.
    delegated.additional_allowlist_dir = None


    delegated.allowlist.write_text(
        "".join(
            f"{scenario.scenario_id} {scenario.feature} {core.sha256_file(stage_root / 'mysql-test' / 'main' / (scenario.scenario_id + '.test'))} "
            f"{core.sha256_file(stage_root / 'mysql-test' / 'main' / (scenario.scenario_id + '.result'))}\n"
            for scenario in args._derived_scenarios
        ),
        encoding="utf-8",
    )
    if not getattr(args, "mtr_runner", None):
        delegated.mtr_runner = stage_root / "mysql-test" / "mariadb-test-run.pl"
    delegated.coverage_kind = "derived-scenarios"
    return delegated


def _decorate_report(report_dir: Path, provenance: dict[str, dict[str, object]], manifest: Path) -> None:
    report_path = report_dir / "mtr-report.json"
    report = json.loads(report_path.read_text(encoding="utf-8"))
    report["coverage_kind"] = "derived-scenarios"
    report["derived_manifest"] = str(manifest)
    report["derived_metrics"] = {
        "scenario_count": len(provenance),
        "source_file_count": len({item["test_path"] for item in provenance.values()} | {item["result_path"] for item in provenance.values()}),
        "line_slices": sum(item["test_line_range"][1] - item["test_line_range"][0] + 1 for item in provenance.values()),
    }
    for result in report.get("results", []):
        item = provenance.get(result.get("test"))
        if item is not None:
            result["provenance"] = item
    report_path.write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")
    markdown_path = report_dir / "mtr-report.md"
    markdown = markdown_path.read_text(encoding="utf-8")
    lines = [markdown.rstrip(), "", "## Derived scenario provenance", "", "- Coverage kind: **derived-scenarios**", "- Manifest: `" + str(manifest) + "`", "", "| Scenario | Source | Test lines | Result lines |", "| --- | --- | ---: | ---: |"]
    for item in provenance.values():
        lines.append(f"| `{item['id']}` | [{item['test_path']}]({item['source_url']}) | {item['test_line_range'][0]}–{item['test_line_range'][1]} | {item['result_line_range'][0]}–{item['result_line_range'][1]} |")
    markdown_path.write_text("\n".join(lines) + "\n", encoding="utf-8")


def run(args: argparse.Namespace) -> int:
    manifest = args.manifest.resolve()
    scenarios = parse_derived_manifest(manifest)
    validate_scenarios(args.suite_root.resolve(), scenarios)
    args._derived_scenarios = scenarios
    with stage_derived_suite(args.suite_root.resolve(), scenarios) as (stage_root, provenance):
        result = core.run(_runtime_args(args, stage_root))
        _decorate_report(args.report_dir.resolve(), provenance, manifest)
        return result


def parser() -> argparse.ArgumentParser:
    result = core.parser()
    result.description = __doc__
    result.add_argument("--manifest", type=Path, default=DEFAULT_MANIFEST)
    result.set_defaults(report_dir=DEFAULT_REPORT_DIR)
    return result


if __name__ == "__main__":
    try:
        raise SystemExit(run(parser().parse_args()))
    except (OSError, ValueError, RuntimeError) as error:
        parser().error(str(error))
