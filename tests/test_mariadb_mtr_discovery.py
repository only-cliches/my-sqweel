#!/usr/bin/env python3
import json
import tempfile
import unittest
from argparse import Namespace
from pathlib import Path

from tools.mariadb_mtr_discover_core import (
    discover_cases,
    has_table_partition,
    write_promotion_manifest,
)


class DiscoveryRegressionTests(unittest.TestCase):
    def write_case(self, root: Path, relative: str, sql: str, result: str = "") -> None:
        test = root / "mysql-test" / relative
        test.parent.mkdir(parents=True, exist_ok=True)
        test.write_text(sql)
        test.with_suffix(".result").write_text(result)

    def test_window_and_named_window_partitions_are_not_table_partitions(self):
        self.assertFalse(
            has_table_partition(
                "SELECT SUM(value) OVER (PARTITION BY group_id ORDER BY id) FROM t;"
            )
        )
        self.assertFalse(
            has_table_partition(
                "SELECT SUM(value) OVER win FROM t "
                "WINDOW win AS (PARTITION BY group_id ORDER BY id);"
            )
        )
        self.assertFalse(
            has_table_partition(
                "SELECT SUM(value) OVER (PARTITION BY group_id) FROM t; "
                "CREATE TABLE p (id INT) AS SELECT id FROM t;"
            )
        )
        self.assertTrue(
            has_table_partition(
                "SELECT SUM(value) OVER (PARTITION BY group_id) FROM t; "
                "CREATE TABLE p (id INT) PARTITION BY HASH(id);"
            )
        )
        self.assertTrue(has_table_partition("CREATE TABLE t (id INT) PARTITION BY HASH (id);"))
        self.assertTrue(has_table_partition("ALTER TABLE t DROP PARTITION p0;"))
        self.assertTrue(has_table_partition("SELECT * FROM `t` PARTITION (p0);"))

    def test_scope_evidence_preserves_lines_across_comments_and_literals(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            self.write_case(root, "main/lines.test", "--echo ignored\n/* comment\nSELECT fake; */\nSELECT 'text\nGRANT fake';\nCREATE TABLE t (id INT);\n")
            case = discover_cases(root, "all", 100, include_safe_harness=True)[0]
            query = next(item for item in case.project_scope["evidence"] if item["feature"] == "queries")
            self.assertEqual(query["line"], 4)
            ddl = next(item for item in case.project_scope["evidence"] if item["feature"] == "ddl")
            self.assertEqual(ddl["line"], 6)
            self.assertFalse(any(item["kind"] == "outside-contract" for item in case.project_scope["evidence"]))

    def test_partition_words_in_literals_and_comments_are_ignored_but_executable_comments_remain(self):
        self.assertFalse(
            has_table_partition(
                "SELECT 'PARTITION BY fake', \"DROP PARTITION\", `PARTITION` "
                "/* CREATE TABLE x PARTITION BY HASH(id) */;"
            )
        )
        self.assertTrue(has_table_partition("/*!40101 CREATE TABLE x PARTITION BY HASH(id) */;"))
        self.assertTrue(has_table_partition("/*M!100100 CREATE TABLE x PARTITION BY HASH(id) */;"))

    def test_all_inventory_records_flat_plugin_helper_and_nested_layouts(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            self.write_case(root, "main/plain.test", "SELECT 1;", "1\n")
            self.write_case(root, "suite/json/flat.test", "SELECT 1;", "1\n")
            self.write_case(root, "suite/json/t/nested.test", "SELECT 1;", "1\n")
            self.write_case(root, "plugin/example/mtr/t/plugin.test", "SELECT 1;", "1\n")
            self.write_case(root, "include/helper.test", "SELECT 1;", "1\n")
            self.write_case(root, "suite/json/deep/path.test", "SELECT 1;", "1\n")
            cases = discover_cases(root, "all", 20)
            by_file = {Path(case.test_file).relative_to(root).as_posix(): case for case in cases}
            self.assertEqual(len(by_file), 6)
            self.assertIsNone(by_file["mysql-test/main/plain.test"].exclusion)
            self.assertIsNone(by_file["mysql-test/suite/json/flat.test"].exclusion)
            self.assertEqual(by_file["mysql-test/plugin/example/mtr/t/plugin.test"].exclusion, "plugin-layout")
            self.assertEqual(by_file["mysql-test/include/helper.test"].exclusion, "helper-layout")
            self.assertEqual(by_file["mysql-test/suite/json/deep/path.test"].exclusion, "nested-suite-layout")

    def test_nested_suite_stem_does_not_collapse_into_executable_name(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            self.write_case(root, "main/foo.test", "SELECT 1;", "1\n")
            self.write_case(root, "suite/json/t/nested/foo.test", "SELECT 1;", "1\n")
            cases = discover_cases(root, "all", 20)
            by_file = {
                Path(case.test_file).relative_to(root).as_posix(): case for case in cases
            }
            self.assertIsNone(by_file["mysql-test/main/foo.test"].exclusion)
            self.assertEqual(
                by_file["mysql-test/suite/json/t/nested/foo.test"].exclusion,
                "nested-suite-layout",
            )
            self.assertNotEqual(
                by_file["mysql-test/suite/json/t/nested/foo.test"].name,
                "foo",
            )

    def test_duplicate_execution_names_are_rejected(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            self.write_case(root, "main/same.test", "SELECT 1;", "1\n")
            self.write_case(root, "t/same.test", "SELECT 1;", "1\n")
            cases = discover_cases(root, "all", 20)
            self.assertEqual({case.exclusion for case in cases}, {"ambiguous-execution-name"})
            self.assertEqual({case.name for case in cases}, {"same"})

    def test_unresolved_include_and_dynamic_eval_are_not_safe_candidates(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            self.write_case(root, "main/missing.test", "--source include/nope.inc\nSELECT 1;", "1\n")
            self.write_case(root, "main/dynamic.test", "--let $query=SELECT 1\nSELECT 1;", "1\n")
            cases = {case.name: case for case in discover_cases(root, "main", 20, include_safe_harness=True)}
            self.assertEqual(cases["missing"].exclusion, "unresolved-include")
            self.assertEqual(cases["dynamic"].exclusion, "unresolved-dynamic")

    def test_derived_reports_cannot_be_promoted(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            report = root / "derived.json"
            manifest = root / "promoted.txt"
            report.write_text(
                json.dumps(
                    {
                        "coverage_kind": "derived-scenarios",
                        "results": [
                            {
                                "test": "x",
                                "feature": "query",
                                "test_sha256": "a" * 64,
                                "result_sha256": "b" * 64,
                                "baseline": "pass",
                                "mysqweel": "pass",
                            }
                        ],
                    }
                )
            )
            with self.assertRaisesRegex(ValueError, "only complete-upstream"):
                write_promotion_manifest(Namespace(compat_report=report, promote_manifest=manifest))
            self.assertFalse(manifest.exists())


if __name__ == "__main__":
    unittest.main()
