#!/usr/bin/env python3
import json
import tempfile
import unittest
from pathlib import Path

from tools.mariadb_mtr_plan import PlanError, build_coverage, validate_plan

TEST_SHA = "a" * 64
RESULT_SHA = "b" * 64


def inventory(*paths, selected=None):
    selected = set(paths if selected is None else selected)
    rows = [
        {"path": path, "name": Path(path).stem, "test_sha256": TEST_SHA, "result_sha256": RESULT_SHA}
        for path in paths
    ]
    return {
        "schema": "my-sqweel.mtr-discovery.v3",
        "source_revision": "mariadb-test-revision",
        "scope": "all",
        "counts": {"inspected": len(rows), "candidates": len(selected), "selected": len(selected)},
        "inventory": rows,
        "candidates": [row for row in rows if row["path"] in selected],
        "selected": [row for row in rows if row["path"] in selected],
    }


def entry(
    path,
    *,
    name=None,
    scope_status="in-scope",
    mode="complete-file",
    state="ready",
    required=True,
    manual=False,
    scenarios=None,
    blockers=None,
    reviewed=False,
    rationale="",
):
    return {
        "path": path,
        "name": name or Path(path).stem,
        "test_sha256": TEST_SHA,
        "result_sha256": RESULT_SHA,
        "scope": {
            "status": scope_status,
            "evidence": [{"kind": "in-scope", "feature": "ordinary SQL", "line": 1}],
            "rationale": rationale,
            "reviewed": reviewed,
        },
        "testing": {
            "required": required,
            "mode": mode,
            "state": state,
            "blockers": blockers if blockers is not None else ([] if state == "ready" else ["not selected"]),
            "derived_scenarios": scenarios or [],
            "manual_complete": manual,
        },
    }


def plan(entries):
    scope = {}
    derived = {}
    for item in entries:
        status = item["scope"]["status"]
        scope[status] = scope.get(status, 0) + 1
        for scenario in item["testing"]["derived_scenarios"]:
            derived[scenario] = {
                "provenance": {"id": scenario, "test_path": item["path"], "test_sha256": TEST_SHA, "result_sha256": RESULT_SHA},
                "test_sha256": TEST_SHA,
                "result_sha256": RESULT_SHA,
            }
    return {
        "schema": "my-sqweel.mtr-testing-plan.v1",
        "source_revision": "mariadb-test-revision",
        "inventory_scope": "all",
        "derived_scenarios": derived,
        "counts": {
            "inventory": len(entries),
            "required": sum(item["testing"]["required"] for item in entries),
            "ready": sum(item["testing"]["state"] == "ready" for item in entries),
            "blocked": sum(item["testing"]["state"] == "blocked" for item in entries),
            "not_required": sum(item["testing"]["state"] == "not-required" for item in entries),
            "scope": scope,
            "partial_derived_files": sum(bool(item["testing"]["derived_scenarios"]) for item in entries),
        },
        "entries": entries,
    }


def report(*results, coverage_kind="complete-upstream"):
    return {
        "schema": "my-sqweel.mtr-compatibility.v4",
        "coverage_kind": coverage_kind,
        "status": "pass",
        "target": "both",
        "results": list(results),
    }


class MariaDBMTRPlanTests(unittest.TestCase):
    def test_partial_inventory_cannot_claim_exhaustive_accounting(self):
        inv = inventory("main/one.test")
        document = plan([entry("main/one.test")])
        inv["scope"] = document["inventory_scope"] = "main"
        with self.assertRaises(PlanError):
            validate_plan(inv, document)

    def test_missing_entry_is_rejected(self):
        inv = inventory("main/one.test", "main/two.test")
        with self.assertRaisesRegex(PlanError, "exact one-to-one"):
            validate_plan(inv, plan([entry("main/one.test")]))

    def test_stale_hash_is_rejected(self):
        inv = inventory("main/one.test")
        stale = plan([entry("main/one.test")])
        stale["entries"][0]["test_sha256"] = "c" * 64
        with self.assertRaisesRegex(PlanError, "stale test/result pin"):
            validate_plan(inv, stale)

    def test_unreviewed_exemption_is_rejected(self):
        inv = inventory("main/one.test")
        exempt = entry(
            "main/one.test",
            scope_status="out-of-scope",
            mode="none",
            state="not-required",
            required=False,
        )
        with self.assertRaisesRegex(PlanError, "out-of-scope requires"):
            validate_plan(inv, plan([exempt]))

    def test_partial_derived_observation_is_not_complete_file_coverage(self):
        inv = inventory("main/one.test", selected=[])
        derived_entry = entry(
            "main/one.test",
            mode="complete-file", state="blocked", scenarios=["scenario-one"],
            blockers=["unresolved-include"],
        )
        derived_result = {
            "test": "scenario-one",
            "test_sha256": TEST_SHA,
            "result_sha256": RESULT_SHA,
            "provenance": {"id": "scenario-one", "test_path": "main/one.test", "test_sha256": TEST_SHA, "result_sha256": RESULT_SHA},
            "baseline": "pass",
            "mysqweel": "pass",
            "status": "pass",
        }
        with tempfile.TemporaryDirectory() as directory:
            report_path = Path(directory) / "derived.json"
            report_path.write_text(json.dumps(report(derived_result, coverage_kind="derived-scenarios")))
            coverage = build_coverage(inv, plan([derived_entry]), [], [report_path])
        self.assertEqual(coverage["counts"]["derived_observed"], 1)
        self.assertEqual(coverage["counts"]["complete_file_observed"], 0)
        self.assertEqual(coverage["counts"]["complete_file_required"], 1)
        self.assertEqual(coverage["counts"]["partial_derived_files"], 1)
        self.assertEqual(coverage["status"], "blocked")

    def test_unknown_and_unextracted_mixed_files_remain_unexecuted(self):
        items = [
            entry("main/unknown.test", scope_status="review-required", mode="scope-review", state="blocked"),
            entry("main/mixed.test", scope_status="mixed", mode="derived-scenarios", state="blocked"),
        ]
        result = build_coverage(inventory(*(item["path"] for item in items), selected=[]), plan(items), [], [])
        self.assertEqual(result["counts"]["required"], 2)
        self.assertEqual({item["path"] for item in result["unexecuted_cases"]}, {item["path"] for item in items})

    def test_derived_binding_must_match_the_enrolled_source(self):
        one = entry("main/one.test", mode="complete-file", state="blocked", scenarios=["slice"])
        document = plan([one])
        document["derived_scenarios"]["slice"]["provenance"]["test_path"] = "main/other.test"
        with self.assertRaises(PlanError):
            validate_plan(inventory("main/one.test", selected=[]), document)

    def test_invalid_baseline_report_preserves_other_observed_comparisons(self):
        rows = [
            {"test": "one", "test_sha256": TEST_SHA, "result_sha256": RESULT_SHA,
             "baseline": "pass", "mysqweel": "pass", "status": "pass"},
            {"test": "two", "test_sha256": TEST_SHA, "result_sha256": RESULT_SHA,
             "baseline": "sql-mismatch", "mysqweel": "not-run", "status": "baseline-failure"},
        ]
        document = report(*rows)
        document["status"] = "invalid"
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "invalid.json"
            path.write_text(json.dumps(document))
            result = build_coverage(inventory("main/one.test", "main/two.test"),
                                    plan([entry("main/one.test"), entry("main/two.test")]), [path], [])
        self.assertEqual(result["status"], "fail")
        self.assertEqual(result["counts"]["complete_file_observed"], 1)
        self.assertEqual(result["counts"]["missing_selected_outcomes"], 0)
        self.assertEqual(result["counts"]["unexecuted"], 1)

    def test_contradictory_pass_cannot_inflate_coverage(self):
        document = report(
            {"test": "one", "test_sha256": TEST_SHA, "result_sha256": RESULT_SHA,
             "baseline": "pass", "mysqweel": "sql-mismatch", "status": "pass"},
        )
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "contradictory.json"
            path.write_text(json.dumps(document))
            result = build_coverage(inventory("main/one.test"), plan([entry("main/one.test")]), [path], [])
        self.assertEqual(result["status"], "fail")
        self.assertEqual(result["counts"]["complete_file_passed"], 0)

    def test_selected_case_missing_outcome_is_failure(self):
        inv = inventory("main/one.test")
        complete_entry = entry("main/one.test")
        with tempfile.TemporaryDirectory() as directory:
            report_path = Path(directory) / "complete.json"
            report_path.write_text(json.dumps(report()))
            coverage = build_coverage(inv, plan([complete_entry]), [report_path], [])
        self.assertEqual(coverage["status"], "fail")
        self.assertEqual(coverage["missing_selected_outcomes"], ["main/one.test"])


if __name__ == "__main__":
    unittest.main()
