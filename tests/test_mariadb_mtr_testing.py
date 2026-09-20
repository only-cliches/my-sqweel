import hashlib
import json
import tempfile
import unittest
from pathlib import Path
from types import SimpleNamespace

from tools.mariadb_mtr_testing import REVIEW_SCHEMA, build_testing_plan


class TestingEnrollmentTests(unittest.TestCase):
    def case(self, root, relative="main/basic.test", blocker=None, status="in-scope"):
        path = root / "mysql-test" / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text("SELECT 1;\n")
        result = path.with_suffix(".result")
        result.write_text("SELECT 1;\n1\n1\n")
        return SimpleNamespace(
            name=path.stem, test_file=str(path), result_file=str(result),
            test_sha256=hashlib.sha256(path.read_bytes()).hexdigest(),
            result_sha256=hashlib.sha256(result.read_bytes()).hexdigest(),
            exclusion=blocker,
            project_scope={"status": status, "evidence": [], "rationale": "Fixture scope assessment", "reviewed": False},
        )

    def review(self, root, case, **changes):
        entry = {
            "path": Path(case.test_file).relative_to(root / "mysql-test").as_posix(),
            "test_sha256": case.test_sha256, "result_sha256": case.result_sha256,
            "status": "out-of-scope", "rationale": "Reviewed process-lifecycle-only fixture",
        }
        entry.update(changes)
        path = root / "reviews.json"
        path.write_text(json.dumps({"schema": REVIEW_SCHEMA, "upstream_revision": "a" * 40, "entries": [entry]}))
        return path

    def test_harness_and_layout_filters_never_exempt_required_work(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            blockers = ["outside-contract-suite", "plugin-layout", "nested-suite-layout",
                        "unresolved-include", "unresolved-dynamic", "custom-delimiter"]
            cases = [self.case(root, f"main/case{index}.test", blocker) for index, blocker in enumerate(blockers)]
            plan, runnable = build_testing_plan(root, cases, "test", None, [], None)
            self.assertEqual(plan["counts"]["required"], len(cases))
            self.assertEqual(plan["counts"]["blocked"], len(cases))
            self.assertEqual(runnable, [])
            for entry, blocker in zip(plan["entries"], blockers):
                self.assertTrue(entry["testing"]["required"])
                self.assertIn(blocker, entry["testing"]["blockers"])

    def test_unknown_sql_is_review_work_not_an_exemption(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            case = self.case(root, status="review-required")
            plan, runnable = build_testing_plan(root, [case], "test", None, [], None)
            testing = plan["entries"][0]["testing"]
            self.assertEqual((testing["required"], testing["mode"], testing["state"]), (True, "scope-review", "blocked"))
            self.assertEqual(runnable, [])

    def test_only_matching_review_pins_authorize_exemption(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            case = self.case(root, status="review-required")
            path = Path(case.test_file)
            path.write_text("SHUTDOWN;\n")
            case.test_sha256 = hashlib.sha256(path.read_bytes()).hexdigest()
            review = self.review(root, case)
            plan, runnable = build_testing_plan(root, [case], "test", review, [], None)
            self.assertEqual(plan["counts"]["not_required"], 1)
            self.assertFalse(plan["entries"][0]["testing"]["required"])
            self.assertEqual(runnable, [])
            self.review(root, case, test_sha256="0" * 64)
            with self.assertRaises(ValueError):
                build_testing_plan(root, [case], "test", review, [], None)
            self.review(root, case, rationale=" ")
            with self.assertRaises(ValueError):
                build_testing_plan(root, [case], "test", review, [], None)

    def test_pinned_manual_case_overrides_filter_but_not_changed_content(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            case = self.case(root, blocker="server-options")
            manifest = root / "complete.txt"
            manifest.write_text(f"basic query {case.test_sha256} {case.result_sha256}\n")
            plan, runnable = build_testing_plan(root, [case], "test", None, [manifest], None)
            self.assertEqual(runnable, [case])
            self.assertEqual(plan["entries"][0]["testing"]["state"], "ready")
            case.test_sha256 = "0" * 64
            with self.assertRaises(ValueError):
                build_testing_plan(root, [case], "test", None, [manifest], None)

    def test_derived_slice_does_not_complete_a_mixed_file(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            case = self.case(root, status="mixed", blocker="custom-delimiter")
            manifest = root / "derived.json"
            manifest.write_text(json.dumps({"schema": "my-sqweel.mtr-derived.v1", "scenarios": [{
                "id": "literal-block", "feature": "query", "source_revision": "a" * 40,
                "source_url": "https://github.com/MariaDB/server/blob/" + "a" * 40 + "/mysql-test/main/basic.test",
                "test_path": "main/basic.test", "result_path": "main/basic.result",
                "test_sha256": case.test_sha256, "result_sha256": case.result_sha256,
                "test_lines": [1, 1], "result_lines": [1, 3],
                "dependency_rationale": "Independent literal SELECT, no dependencies",
            }]}))
            plan, runnable = build_testing_plan(root, [case], "test", None, [], manifest)
            testing = plan["entries"][0]["testing"]
            self.assertEqual(testing["derived_scenarios"], ["literal-block"])
            self.assertEqual((testing["mode"], testing["state"]), ("derived-scenarios", "blocked"))
            self.assertTrue(testing["required"])
            self.assertEqual(runnable, [])

    def test_duplicate_paths_and_missing_reviewed_paths_are_rejected(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            case = self.case(root)
            with self.assertRaises(ValueError):
                build_testing_plan(root, [case, case], "test", None, [], None)
            review = self.review(root, case)
            with self.assertRaises(ValueError):
                build_testing_plan(root, [], "test", review, [], None)


if __name__ == "__main__":
    unittest.main()
