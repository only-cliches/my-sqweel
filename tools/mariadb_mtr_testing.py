"""Enroll every upstream path independently of runnable-manifest eligibility."""

from __future__ import annotations

import hashlib
import json
import re
from collections import Counter
from pathlib import Path, PurePosixPath

try:
    from tools.mariadb_mtr_core import parse_manifest
    from tools.mariadb_mtr_derived import _slice, parse_derived_manifest, validate_scenarios
except ModuleNotFoundError:
    from mariadb_mtr_core import parse_manifest
    from mariadb_mtr_derived import _slice, parse_derived_manifest, validate_scenarios


PLAN_SCHEMA = "my-sqweel.mtr-testing-plan.v1"
REVIEW_SCHEMA = "my-sqweel.mtr-scope-reviews.v1"


def load_reviews(path: Path | None) -> dict[str, dict]:
    if path is None:
        return {}
    document = json.loads(path.read_text())
    if document.get("schema") != REVIEW_SCHEMA or not isinstance(document.get("entries"), list):
        raise ValueError(f"invalid scope review manifest: {path}")
    if not re.fullmatch(r"[0-9a-f]{40}", document.get("upstream_revision", "")):
        raise ValueError(f"scope reviews must pin an immutable upstream revision: {path}")
    reviews = {}
    for entry in document["entries"]:
        relative = entry.get("path", "")
        parts = PurePosixPath(relative)
        if not relative or parts.is_absolute() or ".." in parts.parts or not relative.endswith(".test"):
            raise ValueError(f"invalid reviewed test path: {relative!r}")
        if relative in reviews:
            raise ValueError(f"duplicate scope review: {relative}")
        if entry.get("status") not in {"in-scope", "mixed", "out-of-scope"}:
            raise ValueError(f"invalid reviewed scope: {relative}")
        if not isinstance(entry.get("rationale"), str) or not entry["rationale"].strip():
            raise ValueError(f"scope review has no rationale: {relative}")
        if not re.fullmatch(r"[0-9a-f]{64}", entry.get("test_sha256", "")):
            raise ValueError(f"scope review has no test pin: {relative}")
        if entry.get("result_sha256") != "" and not re.fullmatch(r"[0-9a-f]{64}", entry.get("result_sha256", "")):
            raise ValueError(f"scope review has invalid result pin: {relative}")
        reviews[relative] = entry
    return reviews


def build_testing_plan(
    suite_root: Path,
    cases: list,
    source_revision: str,
    reviews_path: Path | None,
    complete_manifests: list[Path],
    derived_manifest: Path | None,
    inventory_scope: str = "all",
) -> tuple[dict, list]:
    """Return all-path testing intent and its runnable complete-file subset.

    Unknown semantics fail closed into the required backlog. Only an explicit,
    hash-matching review may exempt a path. Existing reviewed complete manifests
    are execution overrides, not evidence inferred from a current passing result.
    """
    root = suite_root / "mysql-test"
    paths = {Path(case.test_file).relative_to(root).as_posix(): case for case in cases}
    if len(paths) != len(cases):
        raise ValueError("duplicate inventory test paths")
    reviews = load_reviews(reviews_path)
    manual = {}
    for manifest in complete_manifests:
        for case in parse_manifest(manifest):
            previous = manual.get(case.name)
            if previous and (previous.test_sha256, previous.result_sha256) != (case.test_sha256, case.result_sha256):
                raise ValueError(f"conflicting reviewed complete-file pins: {case.name}")
            manual[case.name] = case
    if inventory_scope == "all":
        missing_reviews = reviews.keys() - paths.keys()
        missing_manual = manual.keys() - {case.name for case in cases}
        if missing_reviews or missing_manual:
            raise ValueError(f"reviewed files absent from inventory: {sorted(missing_reviews | missing_manual)}")

    derived = {}
    derived_bindings = {}
    if derived_manifest is not None:
        scenarios = parse_derived_manifest(derived_manifest)
        validate_scenarios(suite_root, scenarios)
        for scenario in scenarios:
            if scenario.test_path not in paths:
                if inventory_scope == "all":
                    raise ValueError(f"derived source absent from inventory: {scenario.test_path}")
                continue
            case = paths[scenario.test_path]
            if (case.test_sha256, case.result_sha256) != (scenario.test_sha256, scenario.result_sha256):
                raise ValueError(f"derived source pins disagree with inventory: {scenario.test_path}")
            derived.setdefault(scenario.test_path, []).append(scenario.scenario_id)
            derived_bindings[scenario.scenario_id] = {
                "provenance": scenario.provenance(),
                "test_sha256": hashlib.sha256(_slice(
                    root / scenario.test_path, scenario.test_start, scenario.test_end,
                    f"{scenario.scenario_id} test",
                )).hexdigest(),
                "result_sha256": hashlib.sha256(_slice(
                    root / scenario.result_path, scenario.result_start, scenario.result_end,
                    f"{scenario.scenario_id} result",
                )).hexdigest(),
            }

    entries = []
    runnable = []
    for relative, case in paths.items():
        assessment = dict(case.project_scope)
        blockers = [case.exclusion] if case.exclusion else []
        reviewed_complete = manual.get(case.name)
        if reviewed_complete:
            if (case.test_sha256, case.result_sha256) != (reviewed_complete.test_sha256, reviewed_complete.result_sha256):
                raise ValueError(f"reviewed complete-file pin mismatch: {relative}")
            if case.exclusion == "ambiguous-execution-name":
                raise ValueError(f"reviewed complete-file name is ambiguous: {relative}")
            assessment.update(status="in-scope", reviewed=True,
                              rationale=f"Selected as a complete-file audit in {reviewed_complete.source}.")
            blockers = []
        review = reviews.get(relative)
        if review:
            if (case.test_sha256, case.result_sha256) != (review["test_sha256"], review["result_sha256"]):
                raise ValueError(f"scope review pin mismatch: {relative}")
            assessment.update(status=review["status"], rationale=review["rationale"], reviewed=True)
        status = assessment["status"]
        scenario_ids = sorted(derived.get(relative, []))
        if status == "out-of-scope":
            if reviewed_complete or scenario_ids:
                raise ValueError(f"out-of-scope review conflicts with selected coverage: {relative}")
            required, mode, state, blockers = False, "none", "not-required", []
        else:
            required = True
            mode = {"mixed": "derived-scenarios", "review-required": "scope-review", "in-scope": "complete-file"}[status]
            if status == "mixed":
                blockers.append("mixed-file-needs-derived-coverage")
            elif status == "review-required":
                blockers.append("semantic-scope-review")
            state = "blocked" if blockers else "ready"
            if state == "ready":
                runnable.append(case)
        entries.append({
            "path": relative,
            "name": case.name,
            "test_sha256": case.test_sha256,
            "result_sha256": case.result_sha256,
            "scope": assessment,
            "testing": {
                "required": required, "mode": mode, "state": state,
                "blockers": sorted(set(blockers)), "derived_scenarios": scenario_ids,
                "manual_complete": reviewed_complete is not None,
            },
        })
    scope_counts = Counter(entry["scope"]["status"] for entry in entries)
    state_counts = Counter(entry["testing"]["state"] for entry in entries)
    return {
        "schema": PLAN_SCHEMA,
        "source_revision": source_revision,
        "inventory_scope": inventory_scope,
        "policy": "Every path remains required unless an explicit hash-pinned scope review exempts it.",
        "counts": {
            "inventory": len(entries),
            "required": sum(entry["testing"]["required"] for entry in entries),
            "ready": state_counts["ready"], "blocked": state_counts["blocked"],
            "not_required": state_counts["not-required"],
            "scope": dict(sorted(scope_counts.items())),
            "partial_derived_files": sum(bool(entry["testing"]["derived_scenarios"]) for entry in entries),
        },
        "entries": entries,
        "derived_scenarios": derived_bindings,
    }, runnable
