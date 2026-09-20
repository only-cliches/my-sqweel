#!/usr/bin/env python3
import hashlib
import json
import tempfile
import unittest
from argparse import Namespace
from pathlib import Path
from unittest.mock import patch

from tools.mariadb_mtr_derived import (
    MANIFEST_SCHEMA,
    parse_derived_manifest,
    run,
    stage_derived_suite,
    validate_scenarios,
)


def digest(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


class DerivedManifestTests(unittest.TestCase):
    def make_tree(self):
        temporary = tempfile.TemporaryDirectory()
        root = Path(temporary.name)
        main = root / "mysql-test" / "main"
        main.mkdir(parents=True)
        (root / "mysql-test" / "include").mkdir()
        # The selected block is intentionally independent of all MTR includes.
        test = main / "math.test"
        result = main / "math.result"
        test.write_text("--disable_warnings\nSELECT 1 + 1;\n--enable_warnings\n")
        result.write_text("2\n")
        return temporary, root, test, result
 
    def manifest(self, test: Path, result: Path, **changes):
        entry = {
            "id": "math-block",
            "feature": "math",
            "source_revision": "deadbeef" * 5,
            "source_url": "https://github.com/MariaDB/server/blob/" + "deadbeef" * 5 + "/mysql-test/main/math.test",
            "test_path": "main/math.test",
            "result_path": "main/math.result",
            "test_sha256": digest(test),
            "result_sha256": digest(result),
            "test_lines": [1, 3],
            "result_lines": [1, 1],
            "dependency_rationale": "No source directives; scalar expression only.",
        }
        entry.update(changes)
        return {"schema": MANIFEST_SCHEMA, "scenarios": [entry]}

    def write_manifest(self, root: Path, document):
        path = root / "manifest.json"
        path.write_text(json.dumps(document))
        return path

    def test_revision_pin_must_be_immutable_commit(self):
        temporary, root, test, result = self.make_tree()
        try:
            revision = "a" * 39
            manifest = self.write_manifest(
                root,
                self.manifest(
                    test,
                    result,
                    source_revision=revision,
                    source_url="https://github.com/MariaDB/server/blob/" + revision + "/mysql-test/main/math.test",
                ),
            )
            with self.assertRaisesRegex(ValueError, "immutable 40-hex commit"):
                parse_derived_manifest(manifest)
        finally:
            temporary.cleanup()

    def test_hash_tamper_is_rejected_before_staging(self):
        temporary, root, test, result = self.make_tree()
        try:
            manifest = self.write_manifest(root, self.manifest(test, result))
            test.write_text(test.read_text() + "SELECT 3;\n")
            scenarios = parse_derived_manifest(manifest)
            with self.assertRaisesRegex(ValueError, "test hash mismatch"):
                validate_scenarios(root, scenarios)
        finally:
            temporary.cleanup()

    def test_range_bounds_and_empty_slices_are_rejected(self):
        temporary, root, test, result = self.make_tree()
        try:
            for field, value, pattern in (
                ("test_lines", [1, 8], "exceeds"),
                ("result_lines", [1, 2], "exceeds"),
                ("test_lines", [3, 2], "nonempty inclusive"),
            ):
                with self.assertRaisesRegex(ValueError, pattern):
                    manifest = self.write_manifest(root, self.manifest(test, result, **{field: value}))
                    scenarios = parse_derived_manifest(manifest)
                    validate_scenarios(root, scenarios)
        finally:
            temporary.cleanup()

    def test_include_slice_requires_dependency_closure(self):
        temporary, root, test, result = self.make_tree()
        try:
            test.write_text("--include include/not-pinned.inc\nSELECT 1;\n")
            manifest = self.write_manifest(root, self.manifest(test, result, test_lines=[1, 2], test_sha256=digest(test)))
            with self.assertRaisesRegex(ValueError, "include/source directive"):
                validate_scenarios(root, parse_derived_manifest(manifest))
        finally:
            temporary.cleanup()

    def test_staging_isolated_and_preserves_installed_source(self):
        temporary, root, test, result = self.make_tree()
        try:
            original_test = test.read_bytes()
            original_result = result.read_bytes()
            manifest = self.write_manifest(root, self.manifest(test, result))
            scenarios = parse_derived_manifest(manifest)
            validate_scenarios(root, scenarios)
            with stage_derived_suite(root, scenarios) as (stage_root, provenance):
                staged = stage_root / "mysql-test" / "main" / "math-block.test"
                self.assertEqual(staged.read_text(), test.read_text())
                self.assertTrue((stage_root / "mysql-test" / "include").is_symlink())
                self.assertEqual(provenance["math-block"]["test_line_range"], [1, 3])
            self.assertEqual(test.read_bytes(), original_test)
            self.assertEqual(result.read_bytes(), original_result)
            self.assertFalse((root / "mysql-test" / "main" / "math-block.test").exists())
        finally:
            temporary.cleanup()
    def test_staging_preserves_crlf_and_non_utf8_bytes(self):
        temporary, root, test, result = self.make_tree()
        try:
            test_bytes = bytes([35, 32, 255, 13, 10]) + b"SELECT 1 + 1;\r\n"
            result_bytes = b"2\r\n"
            test.write_bytes(test_bytes)
            result.write_bytes(result_bytes)
            manifest = self.write_manifest(
                root,
                self.manifest(test, result, test_lines=[1, 2], result_lines=[1, 1]),
            )
            scenarios = parse_derived_manifest(manifest)
            validate_scenarios(root, scenarios)
            with stage_derived_suite(root, scenarios) as (stage_root, _):
                self.assertEqual(
                    (stage_root / "mysql-test" / "main" / "math-block.test").read_bytes(),
                    test_bytes,
                )
                self.assertEqual(
                    (stage_root / "mysql-test" / "main" / "math-block.result").read_bytes(),
                    result_bytes,
                )
        finally:
            temporary.cleanup()


    def test_run_writes_derived_kind_provenance_and_separate_metrics(self):
        temporary, root, test, result = self.make_tree()
        try:
            manifest = self.write_manifest(root, self.manifest(test, result))
            report_dir = root / "report"

            def fake_core_run(args):
                report_dir = Path(args.report_dir)
                report_dir.mkdir(parents=True, exist_ok=True)
                (report_dir / "mtr-report.json").write_text(json.dumps({
                    "coverage_kind": "complete-upstream",
                    "results": [{"test": "math-block", "feature": "math"}],
                }))
                (report_dir / "mtr-report.md").write_text("# compatibility\n")
                return 0

            args = Namespace(
                suite_root=root,
                manifest=manifest,
                report_dir=report_dir,
                mtr_runner=None,
            )
            with patch("tools.mariadb_mtr_derived.core.run", side_effect=fake_core_run):
                self.assertEqual(run(args), 0)
            report = json.loads((report_dir / "mtr-report.json").read_text())
            self.assertEqual(report["coverage_kind"], "derived-scenarios")
            self.assertEqual(report["derived_metrics"]["scenario_count"], 1)
            self.assertEqual(report["results"][0]["provenance"]["test_line_range"], [1, 3])
            markdown = (report_dir / "mtr-report.md").read_text()
            self.assertIn("derived-scenarios", markdown)
            self.assertIn("math-block", markdown)
        finally:
            temporary.cleanup()


if __name__ == "__main__":
    unittest.main()
