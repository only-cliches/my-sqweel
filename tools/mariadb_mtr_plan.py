#!/usr/bin/env python3
"""Validate an exhaustive MariaDB MTR testing plan and summarize observed coverage.

The plan is deliberately separate from discovery.  Discovery says what exists;
this tool verifies that every discovered path has an explicit decision and that
execution reports cannot turn a partial scenario into whole-file coverage.
"""

from __future__ import annotations

import argparse
import json
import re
import sys
from collections import Counter
from pathlib import Path
from typing import Any

try:
    from tools.mariadb_mtr_core import classify_case_outcome
except ModuleNotFoundError:
    from mariadb_mtr_core import classify_case_outcome


INVENTORY_SCHEMA = "my-sqweel.mtr-discovery.v3"
PLAN_SCHEMA = "my-sqweel.mtr-testing-plan.v1"
COMPATIBILITY_SCHEMA = "my-sqweel.mtr-compatibility.v4"
COVERAGE_SCHEMA = "my-sqweel.mtr-coverage.v1"
SHA256_RE = re.compile(r"^[0-9a-f]{64}$")
SCOPE_STATUSES = {"in-scope", "mixed", "review-required", "out-of-scope"}
SCOPE_EVIDENCE_KINDS = {"in-scope", "outside-contract"}
TESTING_MODES = {"complete-file", "derived-scenarios", "scope-review", "none"}
TESTING_STATES = {"ready", "blocked", "not-required"}
OBSERVED_TARGET_OUTCOMES = {"pass", "sql-mismatch", "unsupported"}
RESULT_STATUSES = {"pass", "sql-mismatch", "unsupported", "baseline-failure", "infrastructure", "not-run"}


class PlanError(ValueError):
    """An invalid inventory, plan, or execution report."""


def _load_json(path: Path, label: str) -> dict[str, Any]:
    try:
        document = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        raise PlanError(f"cannot read {label} {path}: {error}") from error
    if not isinstance(document, dict):
        raise PlanError(f"{label} {path} must contain a JSON object")
    return document


def _required(document: dict[str, Any], key: str, label: str) -> Any:
    if key not in document:
        raise PlanError(f"{label} is missing required field {key!r}")
    return document[key]


def _string(value: Any, field: str, *, nonempty: bool = True) -> str:
    if not isinstance(value, str) or (nonempty and not value):
        raise PlanError(f"{field} must be a non-empty string")
    return value


def _sha(value: Any, field: str, *, allow_empty: bool = False) -> str:
    if allow_empty and value == "":
        return ""
    value = _string(value, field)
    if not SHA256_RE.fullmatch(value):
        raise PlanError(f"{field} must be a lowercase SHA-256 digest")
    return value

def _relative_test_path(value: Any, field: str) -> str:
    value = _string(value, field)
    path = Path(value)
    if path.is_absolute() or ".." in path.parts or path.as_posix() != value:
        raise PlanError(f"{field} must be a normalized relative path")
    if not value.endswith(".test"):
        raise PlanError(f"{field} must name a .test file")
    return value


def _check_counts(document: dict[str, Any], expected: dict[str, int], label: str) -> None:
    counts = _required(document, "counts", label)
    if not isinstance(counts, dict):
        raise PlanError(f"{label}.counts must be an object")
    for key, value in expected.items():
        if key in counts and counts[key] != value:
            raise PlanError(f"{label}.counts.{key} is stale: expected {value}, got {counts[key]!r}")


def _inventory_rows(document: dict[str, Any]) -> tuple[dict[str, dict[str, Any]], set[str]]:
    if document.get("schema") != INVENTORY_SCHEMA:
        raise PlanError(f"inventory schema must be {INVENTORY_SCHEMA}")
    rows = _required(document, "inventory", "inventory")
    if not isinstance(rows, list):
        raise PlanError("inventory.inventory must be a list")
    by_path: dict[str, dict[str, Any]] = {}
    for index, row in enumerate(rows):
        label = f"inventory.inventory[{index}]"
        if not isinstance(row, dict):
            raise PlanError(f"{label} must be an object")
        path = _relative_test_path(_required(row, "path", label), f"{label}.path")
        if path in by_path:
            raise PlanError(f"duplicate inventory path {path!r}")
        _string(_required(row, "name", label), f"{label}.name")
        _sha(_required(row, "test_sha256", label), f"{label}.test_sha256")
        _sha(_required(row, "result_sha256", label), f"{label}.result_sha256", allow_empty=True)
        by_path[path] = row
    if not by_path:
        raise PlanError("inventory.inventory is empty")

    def subset(name: str) -> set[str]:
        subset_rows = _required(document, name, "inventory")
        if not isinstance(subset_rows, list):
            raise PlanError(f"inventory.{name} must be a list")
        paths: set[str] = set()
        for index, row in enumerate(subset_rows):
            label = f"inventory.{name}[{index}]"
            if not isinstance(row, dict):
                raise PlanError(f"{label} must be an object")
            path = _relative_test_path(_required(row, "path", label), f"{label}.path")
            if path in paths:
                raise PlanError(f"duplicate inventory.{name} path {path!r}")
            if path not in by_path:
                raise PlanError(f"inventory.{name} path {path!r} is not in inventory")
            for digest in ("test_sha256", "result_sha256"):
                expected = _sha(
                    _required(row, digest, label),
                    f"{label}.{digest}",
                    allow_empty=digest == "result_sha256",
                )
                if expected != by_path[path][digest]:
                    raise PlanError(f"inventory.{name} has stale pin for {path!r}")
            if row.get("name") != by_path[path]["name"]:
                raise PlanError(f"inventory.{name} has stale name for {path!r}")
            paths.add(path)
        return paths

    candidates = subset("candidates")
    selected = subset("selected")
    if not selected <= candidates:
        raise PlanError("selected paths must be a subset of candidates")
    _check_counts(
        document,
        {"inspected": len(by_path), "candidates": len(candidates), "selected": len(selected)},
        "inventory",
    )
    return by_path, selected


def _validate_scope(scope: Any, label: str) -> dict[str, Any]:
    if not isinstance(scope, dict):
        raise PlanError(f"{label} must be an object")
    status = _string(_required(scope, "status", label), f"{label}.status")
    if status not in SCOPE_STATUSES:
        raise PlanError(f"{label}.status has illegal value {status!r}")
    evidence = _required(scope, "evidence", label)
    if not isinstance(evidence, list):
        raise PlanError(f"{label}.evidence must be a list")
    for index, item in enumerate(evidence):
        item_label = f"{label}.evidence[{index}]"
        if not isinstance(item, dict):
            raise PlanError(f"{item_label} must be an object")
        kind = _string(_required(item, "kind", item_label), f"{item_label}.kind")
        if kind not in SCOPE_EVIDENCE_KINDS:
            raise PlanError(f"{item_label}.kind has illegal value {kind!r}")
        _string(_required(item, "feature", item_label), f"{item_label}.feature")
        line = _required(item, "line", item_label)
        if not isinstance(line, int) or isinstance(line, bool) or line < 1:
            raise PlanError(f"{item_label}.line must be a positive integer")
    rationale = _string(_required(scope, "rationale", label), f"{label}.rationale", nonempty=False)
    reviewed = _required(scope, "reviewed", label)
    if not isinstance(reviewed, bool):
        raise PlanError(f"{label}.reviewed must be boolean")
    if status == "out-of-scope" and (not reviewed or not rationale.strip()):
        raise PlanError(f"{label}: out-of-scope requires reviewed=true and a rationale")
    if status != "out-of-scope" and reviewed and not rationale.strip():
        raise PlanError(f"{label}: reviewed scope decisions require a rationale")
    return scope


def _validate_testing(testing: Any, scope: dict[str, Any], label: str) -> dict[str, Any]:
    if not isinstance(testing, dict):
        raise PlanError(f"{label} must be an object")
    required = _required(testing, "required", label)
    mode = _string(_required(testing, "mode", label), f"{label}.mode")
    state = _string(_required(testing, "state", label), f"{label}.state")
    if mode not in TESTING_MODES or state not in TESTING_STATES:
        raise PlanError(f"{label} has illegal mode/state combination {mode!r}/{state!r}")
    blockers = _required(testing, "blockers", label)
    scenarios = _required(testing, "derived_scenarios", label)
    manual_complete = _required(testing, "manual_complete", label)
    if not isinstance(required, bool):
        raise PlanError(f"{label}.required must be boolean")
    if not isinstance(blockers, list) or any(not isinstance(item, str) or not item for item in blockers):
        raise PlanError(f"{label}.blockers must be a list of non-empty strings")
    if not isinstance(scenarios, list) or any(not isinstance(item, str) or not item for item in scenarios):
        raise PlanError(f"{label}.derived_scenarios must be a list of strings")
    if not isinstance(manual_complete, bool):
        raise PlanError(f"{label}.manual_complete must be boolean")

    status = scope["status"]
    if status == "out-of-scope":
        if required or mode != "none" or state != "not-required" or blockers or scenarios:
            raise PlanError(f"{label}: out-of-scope entries must be non-required/none/not-required")
        if manual_complete:
            raise PlanError(f"{label}: out-of-scope entries cannot be manually complete")
        return testing
    if not required:
        raise PlanError(f"{label}: every non-exempt entry must set required=true")
    if mode == "none" or state == "not-required":
        raise PlanError(f"{label}: required entries cannot use none/not-required")
    if state == "ready" and blockers:
        raise PlanError(f"{label}: ready entries cannot have blockers")
    if state == "blocked" and not blockers:
        raise PlanError(f"{label}: blocked entries require at least one blocker")
    if status == "mixed" and state != "blocked":
        raise PlanError(f"{label}: mixed scope remains blocked even with a derived scenario")
    if status == "review-required" and (mode != "scope-review" or state != "blocked"):
        raise PlanError(f"{label}: review-required scope must use blocked scope-review testing")
    if mode == "derived-scenarios" and not scenarios and status != "mixed":
        raise PlanError(f"{label}: derived-scenarios entries require derived_scenarios")
    return testing


def _canonical_json(value: Any) -> str:
    return json.dumps(value, sort_keys=True, separators=(",", ":"))


def _derived_bindings(plan: dict[str, Any], entries: list[dict[str, Any]]) -> dict[str, dict[str, Any]]:
    raw = _required(plan, "derived_scenarios", "plan")
    if not isinstance(raw, dict):
        raise PlanError("plan.derived_scenarios must be an object")
    bindings: dict[str, dict[str, Any]] = {}
    for scenario_id, binding in raw.items():
        if not isinstance(scenario_id, str) or not scenario_id:
            raise PlanError("plan.derived_scenarios keys must be non-empty strings")
        if not isinstance(binding, dict):
            raise PlanError(f"plan.derived_scenarios[{scenario_id!r}] must be an object")
        provenance = _required(binding, "provenance", f"plan.derived_scenarios[{scenario_id!r}]")
        if not isinstance(provenance, dict) or provenance.get("id") != scenario_id:
            raise PlanError(f"derived scenario {scenario_id!r} has invalid provenance")
        _sha(_required(binding, "test_sha256", f"plan.derived_scenarios[{scenario_id!r}]"), f"derived {scenario_id}.test_sha256")
        _sha(_required(binding, "result_sha256", f"plan.derived_scenarios[{scenario_id!r}]"), f"derived {scenario_id}.result_sha256")
        _sha(_required(provenance, "test_sha256", f"derived {scenario_id}.provenance"), f"derived {scenario_id}.provenance.test_sha256")
        _sha(_required(provenance, "result_sha256", f"derived {scenario_id}.provenance"), f"derived {scenario_id}.provenance.result_sha256")
        bindings[scenario_id] = binding
    for entry in entries:
        for scenario_id in entry["testing"]["derived_scenarios"]:
            if scenario_id not in bindings:
                raise PlanError(f"{entry['path']!r} references unknown derived scenario {scenario_id!r}")
            provenance = bindings[scenario_id]["provenance"]
            if (
                provenance.get("test_path") != entry["path"]
                or provenance["test_sha256"] != entry["test_sha256"]
                or provenance["result_sha256"] != entry["result_sha256"]
            ):
                raise PlanError(f"derived scenario {scenario_id!r} is bound to the wrong source")
    referenced = {scenario for entry in entries for scenario in entry["testing"]["derived_scenarios"]}
    if referenced != bindings.keys():
        raise PlanError("derived bindings must exactly match referenced scenarios")
    return bindings


def validate_plan(inventory: dict[str, Any], plan: dict[str, Any]) -> tuple[list[dict[str, Any]], dict[str, dict[str, Any]], set[str]]:
    inventory_by_path, selected = _inventory_rows(inventory)
    if plan.get("schema") != PLAN_SCHEMA:
        raise PlanError(f"plan schema must be {PLAN_SCHEMA}")
    inventory_revision = _string(_required(inventory, "source_revision", "inventory"), "inventory.source_revision")
    plan_revision = _string(_required(plan, "source_revision", "plan"), "plan.source_revision")
    if inventory_revision != plan_revision:
        raise PlanError("inventory and plan source_revision differ")
    if inventory.get("scope") != "all" or plan.get("inventory_scope") != "all":
        raise PlanError("exhaustive coverage accounting requires the full upstream inventory (--scope all)")
    entries = _required(plan, "entries", "plan")
    if not isinstance(entries, list):
        raise PlanError("plan.entries must be a list")
    by_path: dict[str, dict[str, Any]] = {}
    for index, entry in enumerate(entries):
        label = f"plan.entries[{index}]"
        if not isinstance(entry, dict):
            raise PlanError(f"{label} must be an object")
        path = _relative_test_path(_required(entry, "path", label), f"{label}.path")
        if path in by_path:
            raise PlanError(f"duplicate plan path {path!r}")
        if path not in inventory_by_path:
            raise PlanError(f"plan path {path!r} is not present in inventory")
        name = _string(_required(entry, "name", label), f"{label}.name")
        test_sha = _sha(_required(entry, "test_sha256", label), f"{label}.test_sha256")
        result_sha = _sha(_required(entry, "result_sha256", label), f"{label}.result_sha256", allow_empty=True)
        inventory_row = inventory_by_path[path]
        if name != inventory_row["name"]:
            raise PlanError(f"stale execution name for {path!r}")
        if (test_sha, result_sha) != (inventory_row["test_sha256"], inventory_row["result_sha256"]):
            raise PlanError(f"stale test/result pin for {path!r}")
        scope = _validate_scope(_required(entry, "scope", label), f"{label}.scope")
        testing = _required(entry, "testing", label)
        _validate_testing(testing, scope, f"{label}.testing")
        if not result_sha and testing["state"] == "ready":
            raise PlanError(f"{path!r}: ready entries require a pinned result file")
        by_path[path] = entry
    missing = set(inventory_by_path) - set(by_path)
    extra = set(by_path) - set(inventory_by_path)
    if missing or extra:
        details = []
        if missing:
            details.append("missing=" + ",".join(sorted(missing)))
        if extra:
            details.append("extra=" + ",".join(sorted(extra)))
        raise PlanError("inventory/plan paths are not exact one-to-one: " + "; ".join(details))
    _derived_bindings(plan, entries)
    counts = _required(plan, "counts", "plan")
    if not isinstance(counts, dict):
        raise PlanError("plan.counts must be an object")
    for key in ("inventory", "required", "ready", "blocked", "not_required", "scope", "partial_derived_files"):
        if key not in counts:
            raise PlanError(f"plan.counts is missing required field {key!r}")
    expected_counts = {
        "inventory": len(entries),
        "required": sum(bool(entry["testing"]["required"]) for entry in entries),
        "ready": sum(entry["testing"]["state"] == "ready" for entry in entries),
        "blocked": sum(entry["testing"]["state"] == "blocked" for entry in entries),
        "not_required": sum(entry["testing"]["state"] == "not-required" for entry in entries),
        "partial_derived_files": sum(bool(entry["testing"]["derived_scenarios"]) for entry in entries),
    }
    for key, value in expected_counts.items():
        if counts[key] != value:
            raise PlanError(f"plan.counts.{key} is stale: expected {value}, got {counts[key]!r}")
    expected_scope = dict(sorted(Counter(entry["scope"]["status"] for entry in entries).items()))
    if counts["scope"] != expected_scope:
        raise PlanError(f"plan.counts.scope is stale: expected {expected_scope!r}, got {counts['scope']!r}")
    for path, entry in by_path.items():
        testing = entry["testing"]
        if testing["mode"] == "complete-file" and testing["state"] == "ready" and path not in selected:
            raise PlanError(f"{path!r}: ready complete-file entries must be selected candidates")
    for row in inventory["candidates"]:
        testing = by_path[row["path"]]["testing"]
        if not testing["required"] or testing["mode"] != "complete-file" or testing["state"] != "ready":
            raise PlanError(f"{row['path']!r}: candidate is not required and ready for complete execution")
    return entries, by_path, selected


def _report_result_key(result: dict[str, Any], *, derived: bool) -> tuple[str, ...] | None:
    if not isinstance(result, dict):
        return None
    test = result.get("test")
    test_sha = result.get("test_sha256")
    result_sha = result.get("result_sha256")
    if not isinstance(test, str) or not isinstance(test_sha, str) or not isinstance(result_sha, str):
        return None
    if not SHA256_RE.fullmatch(test_sha) or not SHA256_RE.fullmatch(result_sha):
        return None
    if result.get("status") not in RESULT_STATUSES:
        return None
    if result.get("baseline", "not-run") not in RESULT_STATUSES:
        return None
    if result.get("mysqweel", "not-run") not in RESULT_STATUSES:
        return None
    if derived:
        provenance = result.get("provenance")
        if not isinstance(provenance, dict) or provenance.get("id") != test:
            return None
        return test, test_sha, result_sha, _canonical_json(provenance)
    return test, test_sha, result_sha


def _load_reports(
    paths: list[Path],
    *,
    derived: bool,
    derived_bindings: dict[str, dict[str, Any]] | None = None,
) -> tuple[dict[tuple[str, ...], dict[str, Any]], list[str]]:
    observations: dict[tuple[str, ...], dict[str, Any]] = {}
    invalid: list[str] = []
    for path in paths:
        if not path.is_file():
            invalid.append(f"{path}: missing execution report")
            continue
        report = _load_json(path, "execution report")
        if report.get("schema") != COMPATIBILITY_SCHEMA:
            raise PlanError(f"execution report {path} has unexpected schema")
        if report.get("target") != "both":
            invalid.append(f"{path}: target is not both")
            continue
        results = report.get("results")
        if not isinstance(results, list):
            raise PlanError(f"execution report {path}.results must be a list")
        expected_kind = "derived-scenarios" if derived else "complete-upstream"
        if report.get("coverage_kind") != expected_kind:
            invalid.append(f"{path}: coverage kind does not match report option")
            continue
        if report.get("status") == "invalid":
            invalid.append(f"{path}: invalid baseline/infrastructure report")
        for index, result in enumerate(results):
            key = _report_result_key(result, derived=derived)
            if key is None:
                invalid.append(f"{path}: results[{index}] has no usable name, pins, and provenance")
                continue
            if result["status"] != classify_case_outcome(result.get("baseline", "not-run"), result.get("mysqweel", "not-run")):
                invalid.append(f"{path}: results[{index}] disagrees with its target outcomes")
                continue
            if result["status"] in {"baseline-failure", "infrastructure"}:
                invalid.append(f"{path}: {key[0]} has a {result['status']} outcome")
            if derived:
                binding = (derived_bindings or {}).get(key[0])
                if (
                    binding is None
                    or key[1] != binding["test_sha256"]
                    or key[2] != binding["result_sha256"]
                    or key[3] != _canonical_json(binding["provenance"])
                ):
                    invalid.append(f"{path}: stale derived scenario binding for {key[0]!r}")
                    continue
            previous = observations.get(key)
            if previous is not None:
                if (previous.get("baseline"), previous.get("mysqweel")) != (
                    result.get("baseline"), result.get("mysqweel")
                ):
                    raise PlanError(f"conflicting execution outcomes for {key[0]!r}")
                continue
            observations[key] = result
    return observations, invalid


def _comparison(result: dict[str, Any] | None) -> dict[str, Any]:
    if result is None:
        return {"observed": False, "status": "not-run", "baseline": "not-run", "mysqweel": "not-run"}
    baseline = result.get("baseline", "not-run")
    mysqweel = result.get("mysqweel", "not-run")
    observed = baseline == "pass" and mysqweel in OBSERVED_TARGET_OUTCOMES
    return {"observed": observed, "status": result.get("status", "not-run"), "baseline": baseline, "mysqweel": mysqweel}


def build_coverage(
    inventory: dict[str, Any],
    plan: dict[str, Any],
    complete_reports: list[Path],
    derived_reports: list[Path],
) -> dict[str, Any]:
    entries, by_path, selected = validate_plan(inventory, plan)
    bindings = _derived_bindings(plan, entries)
    complete, complete_invalid = _load_reports(complete_reports, derived=False) if complete_reports else ({}, [])
    derived, derived_invalid = (
        _load_reports(derived_reports, derived=True, derived_bindings=bindings)
        if derived_reports
        else ({}, [])
    )
    execution_reports_supplied = bool(complete_reports or derived_reports)

    scope_counts = Counter(entry["scope"]["status"] for entry in entries)
    mode_counts = Counter(entry["testing"]["mode"] for entry in entries)
    state_counts = Counter(entry["testing"]["state"] for entry in entries)
    complete_cases: list[dict[str, Any]] = []
    derived_cases: list[dict[str, Any]] = []
    unexecuted: list[dict[str, str]] = []
    missing_selected: list[str] = []
    partial_derived_files = 0
    for entry in entries:
        testing = entry["testing"]
        path = entry["path"]
        if not testing["required"]:
            continue
        complete_observed_here = False
        if testing["mode"] == "complete-file":
            key = (entry["name"], entry["test_sha256"], entry["result_sha256"])
            result = complete.get(key)
            comparison = _comparison(result)
            complete_observed_here = comparison["observed"]
            complete_cases.append({"path": path, "name": entry["name"], **comparison})
            if execution_reports_supplied and path in selected and result is None:
                missing_selected.append(path)
        observations = []
        for scenario in testing["derived_scenarios"]:
            binding = bindings[scenario]
            key = (
                scenario, binding["test_sha256"], binding["result_sha256"],
                _canonical_json(binding["provenance"]),
            )
            result = derived.get(key)
            observations.append({"scenario": scenario, **_comparison(result)})
            if execution_reports_supplied and derived_reports and result is None:
                missing_selected.append(f"{path}:{scenario}")
        if testing["mode"] == "derived-scenarios" or observations:
            derived_cases.append({"path": path, "name": entry["name"], "observations": observations})
        derived_observed_here = any(item["observed"] for item in observations)
        if not complete_observed_here:
            if derived_observed_here:
                partial_derived_files += 1
            else:
                unexecuted.append({"path": path, "name": entry["name"], "kind": testing["mode"]})

    complete_observed = sum(case["observed"] for case in complete_cases)
    complete_passed = sum(case["observed"] and case["status"] == "pass" for case in complete_cases)
    complete_mismatches = sum(case["observed"] and case["status"] != "pass" for case in complete_cases)
    derived_observations = [observation for case in derived_cases for observation in case["observations"]]
    derived_observed = sum(item["observed"] for item in derived_observations)
    derived_passed = sum(item["observed"] and item["status"] == "pass" for item in derived_observations)
    derived_mismatches = sum(item["observed"] and item["status"] != "pass" for item in derived_observations)

    invalid_reports = complete_invalid + derived_invalid
    if not execution_reports_supplied:
        status = "planning"
    elif missing_selected or invalid_reports:
        status = "fail"
    elif state_counts["blocked"] or unexecuted or partial_derived_files or state_counts["ready"] == 0:
        status = "blocked"
    else:
        # SQL mismatches and unsupported cases are deliberately non-gating.
        status = "pass"

    return {
        "schema": COVERAGE_SCHEMA,
        "plan_schema": PLAN_SCHEMA,
        "inventory_schema": INVENTORY_SCHEMA,
        "source_revision": plan["source_revision"],
        "status": status,
        "execution_reports_supplied": execution_reports_supplied,
        "counts": {
            "inventory": len(inventory["inventory"]),
            "plan_entries": len(entries),
            "required": sum(bool(entry["testing"]["required"]) for entry in entries),
            "exempt": scope_counts["out-of-scope"],
            "ready": state_counts["ready"],
            "blocked": state_counts["blocked"],
            "unexecuted": len(unexecuted),
            "missing_selected_outcomes": len(missing_selected),
            "complete_file_required": sum(
                entry["testing"]["required"] and entry["testing"]["mode"] == "complete-file"
                for entry in entries
            ),
            "complete_file_observed": complete_observed,
            "complete_file_passed": complete_passed,
            "complete_file_mismatches": complete_mismatches,
            "derived_observations": len(derived_observations),
            "derived_observed": derived_observed,
            "derived_passed": derived_passed,
            "derived_mismatches": derived_mismatches,
            "partial_derived_files": partial_derived_files,
        },
        "scope": {status: scope_counts[status] for status in sorted(SCOPE_STATUSES)},
        "testing": {
            "modes": dict(sorted(mode_counts.items())),
            "states": dict(sorted(state_counts.items())),
        },
        "complete_file": complete_cases,
        "derived": derived_cases,
        "unexecuted_cases": unexecuted,
        "missing_selected_outcomes": sorted(set(missing_selected)),
        "invalid_reports": invalid_reports,
        "selected_paths": sorted(selected),
    }


def render_markdown(report: dict[str, Any]) -> str:
    if "counts" not in report:
        return f"# MariaDB MTR coverage\n\n- Status: **fail**\n- Error: {report.get('error', 'invalid input')}\n"
    counts = report["counts"]
    lines = [
        "# MariaDB MTR coverage",
        "",
        f"- Status: **{report['status']}**",
        f"- Source revision: `{report['source_revision']}`",
        f"- Inventory paths: {counts['inventory']}",
        f"- Required: {counts['required']} (ready {counts['ready']}, blocked {counts['blocked']})",
        f"- Exempt out-of-scope: {counts['exempt']}",
        f"- Execution reports supplied: `{report['execution_reports_supplied']}`",
        f"- Missing selected outcomes: {counts['missing_selected_outcomes']}",
        f"- Unexecuted files: {counts['unexecuted']}; files with only partial derived observations: {counts['partial_derived_files']}",
        "",
        "Planning completeness and execution observations are separate. A derived scenario never counts as complete-file coverage.",
        "Complete-file and unexecuted lists show at most 50 entries each; `mariadb-mtr-coverage.json` contains the exhaustive per-file accounting.",
        "",
        "## Scope categories",
        "",
        "| Scope | Entries |",
        "| --- | ---: |",
    ]
    for status, count in report["scope"].items():
        lines.append(f"| `{status}` | {count} |")
    lines.extend([
        "",
        "## Complete-file observations",
        "",
        f"Observed comparisons: {counts['complete_file_observed']} / {counts['complete_file_required']}",
        f"Passing: {counts['complete_file_passed']}; mismatches or unsupported: {counts['complete_file_mismatches']}",
        "",
        "| Path | Name | Baseline | MySqweel | Observation |",
        "| --- | --- | --- | --- | --- |",
    ])
    for case in report["complete_file"][:50]:
        lines.append(f"| `{case['path']}` | `{case['name']}` | {case['baseline']} | {case['mysqweel']} | {case['status']} |")
    lines.extend([
        "",
        "## Partial derived observations",
        "",
        f"Observed scenarios: {counts['derived_observed']} / {counts['derived_observations']}; passing: {counts['derived_passed']}; mismatches or unsupported: {counts['derived_mismatches']}",
        "",
        "| Path | Scenario | Baseline | MySqweel | Observation |",
        "| --- | --- | --- | --- | --- |",
    ])
    for case in report["derived"]:
        for observation in case["observations"]:
            lines.append(f"| `{case['path']}` | `{observation['scenario']}` | {observation['baseline']} | {observation['mysqweel']} | {observation['status']} |")
    lines.extend(["", "## Unexecuted cases", ""])
    if report["unexecuted_cases"]:
        lines.extend(f"- `{item['kind']}` `{item['path']}` / `{item['name']}`" for item in report["unexecuted_cases"][:50])
    else:
        lines.append("None.")
    if report["invalid_reports"]:
        lines.extend(["", "## Execution report errors", "", *[f"- {item}" for item in report["invalid_reports"]]])
    return "\n".join(lines) + "\n"


def write_coverage(report: dict[str, Any], report_dir: Path) -> None:
    report_dir.mkdir(parents=True, exist_ok=True)
    (report_dir / "mariadb-mtr-coverage.json").write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")
    (report_dir / "mariadb-mtr-coverage.md").write_text(render_markdown(report), encoding="utf-8")


def parser() -> argparse.ArgumentParser:
    result = argparse.ArgumentParser(description=__doc__)
    result.add_argument("--inventory", type=Path, required=True)
    result.add_argument("--plan", type=Path, required=True)
    result.add_argument("--report-dir", type=Path, required=True)
    result.add_argument("--complete-report", type=Path, action="append", default=[])
    result.add_argument("--derived-report", type=Path, action="append", default=[])
    return result


def main(argv: list[str] | None = None) -> int:
    args = parser().parse_args(argv)
    try:
        inventory = _load_json(args.inventory, "inventory")
        plan = _load_json(args.plan, "plan")
        report = build_coverage(inventory, plan, args.complete_report, args.derived_report)
        write_coverage(report, args.report_dir)
    except PlanError as error:
        failure = {
            "schema": COVERAGE_SCHEMA,
            "plan_schema": PLAN_SCHEMA,
            "inventory_schema": INVENTORY_SCHEMA,
            "status": "fail",
            "error": str(error),
        }
        try:
            write_coverage(failure, args.report_dir)
        except OSError:
            pass
        print(f"mariadb-mtr-plan: {error}", file=sys.stderr)
        return 2
    print(render_markdown(report), end="")
    return 1 if report["status"] == "fail" else 0


if __name__ == "__main__":
    raise SystemExit(main())
