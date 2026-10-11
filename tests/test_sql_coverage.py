import json
from pathlib import Path
import tempfile
import unittest

from tools.sql_coverage import DEFAULT_SPEC, CoverageError, build_report, load_spec, render_markdown


class CommittedSqlCoverageTests(unittest.TestCase):
    def test_committed_witnesses_are_valid(self):
        report = build_report()
        self.assertGreater(report["totals"]["obligations"], 0)
        self.assertEqual(
            report["totals"]["fixture_evidenced"],
            sum(bool(item.get("case")) for item in load_spec(DEFAULT_SPEC)),
        )
        self.assertIn("| data_boundary |", render_markdown(report))
        self.assertIn("null_join_key", render_markdown(report))


class SqlCoverageTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.fixtures = self.root / "query_cases"
        self.fixtures.mkdir()
        self.spec = self.root / "spec.toml"
        self.write_fixture()

    def write_fixture(self, **changes):
        fixture = {
            "version": 1, "id": "case_one", "setup": ["CREATE TABLE t (id INT)"],
            "steps": [{"sql": "INSERT INTO t VALUES (1)"}, {"sql": "SELECT id FROM t"}],
            "checks": [{"sql": "SELECT id FROM t", "ordered": True}],
        }
        fixture.update(changes)
        (self.fixtures / "case_one.json").write_text(json.dumps(fixture), encoding="utf-8")

    def write_spec(self, obligations):
        lines = []
        for item in obligations:
            lines.append("[[obligations]]")
            for key, value in item.items():
                lines.append(f"{key} = {json.dumps(value)}")
        self.spec.write_text("\n".join(lines), encoding="utf-8")

    def test_witnesses_and_optional_parity_results(self):
        self.write_spec([
            {"id": "insert", "dimension": "feature", "status": "supported", "case": "case_one", "sql_contains": ["INSERT INTO"], "setup_contains": ["CREATE TABLE"]},
            {"id": "read", "dimension": "execution_path", "status": "supported", "case": "case_one", "sql_contains": ["SELECT id"]},
        ])
        report = build_report(self.spec, self.fixtures)
        self.assertEqual(report["totals"], {"obligations": 2, "fixture_evidenced": 2, "test_evidenced": 0, "uncovered": 0, "parity_verified": 0, "awaiting_parity": 2})
        self.assertEqual(report["fixtures"], 1)
        results = self.root / "results.json"
        results.write_text(json.dumps([{"id": "case_one", "result": {"status": "pass"}}]))
        self.assertEqual(build_report(self.spec, self.fixtures, results)["totals"]["parity_verified"], 0)
        self.assertEqual(build_report(self.spec, self.fixtures, results)["totals"]["awaiting_parity"], 2)
        results.write_text(json.dumps([{"id": "case_one", "result": {"status": "pass", "mysqweel": []}}]))
        self.assertEqual(build_report(self.spec, self.fixtures, results)["totals"]["parity_verified"], 2)
        results.write_text(json.dumps([{"id": "case_one", "result": {"status": "mismatch", "mysqweel": []}}]))
        report = build_report(self.spec, self.fixtures, results)
        self.assertEqual(report["totals"]["parity_verified"], 0)
        self.assertEqual(report["mismatched_cases"], ["case_one"])

    def test_sql_setup_and_substantive_step_are_required(self):
        self.write_spec([{"id": "missing", "dimension": "feature", "status": "supported", "case": "case_one", "sql_contains": ["DELETE"], "setup_contains": ["CREATE INDEX"]}])
        report = build_report(self.spec, self.fixtures)
        self.assertEqual(report["dimensions"]["feature"]["uncovered"], ["missing"])
        self.write_fixture(steps=[])
        self.assertEqual(build_report(self.spec, self.fixtures)["dimensions"]["feature"]["uncovered"], ["missing"])

    def test_error_obligation_requires_matching_error_step(self):
        self.write_spec([{"id": "error", "dimension": "error_behavior", "status": "supported", "case": "case_one", "sql_contains": ["INSERT INTO"], "error_sql_contains": ["INSERT INTO"], "expect_error": True}])
        self.assertEqual(build_report(self.spec, self.fixtures)["totals"]["uncovered"], 1)
        self.write_fixture(steps=[{"sql": "INSERT INTO t VALUES (1)", "expect_error": True}])
        self.assertEqual(build_report(self.spec, self.fixtures)["totals"]["fixture_evidenced"], 1)

    def test_malformed_spec_duplicate_unknowns_and_cases_fail(self):
        good = {"id": "x", "dimension": "feature", "status": "supported", "case": "case_one", "sql_contains": ["SELECT id"]}
        for obligations in ([good, good], [{**good, "dimension": "unknown"}], [{**good, "status": "planned"}], [{**good, "case": "absent"}], [{**good, "sql_contains": []}]):
            with self.subTest(obligations=obligations):
                self.write_spec(obligations)
                with self.assertRaises(CoverageError):
                    build_report(self.spec, self.fixtures)

    def test_missing_case_is_a_reported_gap(self):
        self.write_spec([{"id": "known_gap", "dimension": "data_boundary", "status": "supported"}])
        report = build_report(self.spec, self.fixtures)
        self.assertEqual(report["dimensions"]["data_boundary"]["uncovered"], ["known_gap"])

    def test_interaction_requires_one_statement_and_boundary_requires_data(self):
        self.write_spec([{
            "id": "combined", "dimension": "interaction", "status": "supported", "case": "case_one",
            "sql_contains": ["INSERT INTO", "SELECT id"],
        }])
        self.assertEqual(build_report(self.spec, self.fixtures)["totals"]["uncovered"], 1)
        self.write_spec([{
            "id": "nullable", "dimension": "data_boundary", "status": "supported", "case": "case_one",
            "sql_contains": ["SELECT id"],
        }])
        with self.assertRaises(CoverageError):
            build_report(self.spec, self.fixtures)

    def test_executed_path_and_generated_reports_close_gaps(self):
        self.write_spec([
            {"id": "embedded_sync", "dimension": "execution_path", "status": "supported", "test": "execution_paths"},
            {"id": "null_join_key", "dimension": "data_boundary", "status": "supported", "generated": "null_join_key"},
        ])
        paths = self.root / "paths"
        paths.mkdir()
        (paths / "execution_paths.json").write_text(json.dumps({"covered": ["embedded_sync"], "reference_verified": True}))
        typed = self.root / "typed.json"
        typed.write_text(json.dumps({"data_conditions": ["null_join_key"], "features": [], "interactions": [], "differential_checked": True, "failures": []}))
        report = build_report(self.spec, self.fixtures, path_results=paths, typed_results=typed)
        self.assertEqual(report["totals"]["test_evidenced"], 2)
        self.assertEqual(report["totals"]["parity_verified"], 2)
        self.assertEqual(report["totals"]["uncovered"], 0)

    def test_code_and_mutation_summaries_are_separate_from_sql_obligations(self):
        self.write_spec([{"id": "read", "dimension": "feature", "status": "supported", "case": "case_one", "sql_contains": ["SELECT id"]}])
        code = self.root / "code.json"
        code.write_text(json.dumps({"kind": "llvm_source_coverage", "branches": {"covered": 7, "total": 10}, "regions": {"covered": 9, "total": 12}}))
        mutation = self.root / "mutation.json"
        mutation.write_text(json.dumps({"kind": "cargo_mutants_sample", "totals": {"caught": 3, "missed": 1, "timeout": 0}}))
        fuzz = self.root / "fuzz.json"
        fuzz.write_text(json.dumps({"kind": "libfuzzer_runs", "runs": [{"target": "parser", "status": "completed", "crashes": 0}], "totals": {"targets": 1, "completed": 1, "failed": 0, "crashes": 0}}))
        report = build_report(self.spec, self.fixtures, code_coverage=code, mutation_results=mutation, fuzz_results=fuzz)
        self.assertEqual(report["totals"]["obligations"], 1)
        self.assertIn("7/10 branches", render_markdown(report))
        self.assertIn("3 caught, 1 missed", render_markdown(report))
        self.assertIn("Fuzz targets: 1/1 completed", render_markdown(report))


if __name__ == "__main__":
    unittest.main()
