#!/usr/bin/env python3
import hashlib
import json
import os
import sys
import tempfile
import unittest
from unittest.mock import patch
from subprocess import CompletedProcess
from argparse import Namespace
from pathlib import Path

from tools.mariadb_mtr_core import (
    Server,
    TestCase,
    configure_case_timezone,
    mtr_case_timezone,
    mtr_command,
    parse_manifest,
    prepare_external_mariadb_runner,
    render_markdown,
    reset_test_database,
    sql_statement_count,
    start_mysqweel,
    validate_cases,
    validate_distinct_servers,
    validate_mtr_runtime,
)
from tools.mariadb_mtr_discover_core import (
    discover_cases,
    rotating_selection,
    write_promotion_manifest,
)

DIGEST_A = "a" * 64
DIGEST_B = "b" * 64


class ManifestTests(unittest.TestCase):
    def test_external_runner_adaptation_changes_only_the_feature_probe(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source = root / "upstream.pl"
            destination = root / "external.pl"
            original = b'before\r\nuse mysql; SHOW VARIABLES\r\nafter\r\n'
            source.write_bytes(original)
            self.assertEqual(prepare_external_mariadb_runner(source, destination), destination)
            self.assertEqual(source.read_bytes(), original)
            adapted = b'before\r\nSHOW VARIABLES\r\nafter\r\n'
            self.assertEqual(destination.read_bytes(), adapted)
            metadata = json.loads(destination.with_suffix(".json").read_text())
            self.assertEqual(metadata["source_sha256"], hashlib.sha256(original).hexdigest())
            self.assertEqual(metadata["adapted_sha256"], hashlib.sha256(adapted).hexdigest())

    def test_external_runner_adaptation_rejects_missing_or_ambiguous_probes(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source = root / "upstream.pl"
            destination = root / "external.pl"
            for count in (0, 2):
                with self.subTest(count=count):
                    source.write_text("use mysql; SHOW VARIABLES\n" * count)
                    with self.assertRaisesRegex(RuntimeError, "unrecognized MariaDB MTR"):
                        prepare_external_mariadb_runner(source, destination)
                    self.assertFalse(destination.exists())

    def test_mysqweel_reset_preserves_embedded_database_and_avoids_global_settings(self):
        with patch("tools.mariadb_mtr_core.subprocess.run") as run:
            run.side_effect = [
                CompletedProcess([], 0, "app\ntest\nleftover\n", ""),
                CompletedProcess([], 0, "", ""),
            ]
            reset_test_database(Server("mysqweel", "mysql://root@localhost/test"), Path("/bin"), mariadb=True)
            sql = run.call_args.args[0][-1]
            self.assertIn("DROP DATABASE IF EXISTS `leftover`", sql)
            self.assertIn("DROP DATABASE IF EXISTS test", sql)
            self.assertNotIn("`app`", sql)
            self.assertNotIn("SET GLOBAL", sql)

    def test_mysqweel_startup_passes_required_timezone(self):
        with tempfile.TemporaryDirectory() as directory, patch(
            "tools.mariadb_mtr_core.subprocess.Popen"
        ) as popen, patch("tools.mariadb_mtr_core.wait_for_port"), patch(
            "tools.mariadb_mtr_core.free_port", return_value=3307
        ):
            start_mysqweel(Path("sqwl"), Path(directory), timezone="-10:00")
            command = popen.call_args.args[0]
            self.assertEqual(command[command.index("--default-time-zone") + 1], "-10:00")
            popen.call_args.kwargs["stdout"].close()

    def test_mysqweel_timezone_must_match_the_upstream_requirement(self):
        case = TestCase("simple", "query", DIGEST_A, DIGEST_B, "manifest")
        with patch("tools.mariadb_mtr_core.mtr_case_timezone", return_value="+00:00"), patch(
            "tools.mariadb_mtr_core.subprocess.run", return_value=CompletedProcess([], 0, "+00:00\n", "")
        ) as run:
            timezone = configure_case_timezone(Server("mysqweel", "mysql://root@localhost/test"), Path("/bin"), Path("/suite"), case)
            self.assertEqual(timezone, "+00:00")
            self.assertEqual(run.call_args.args[0][-1], "--execute=SELECT @@time_zone")
        with patch("tools.mariadb_mtr_core.mtr_case_timezone", return_value="-10:00"), patch(
            "tools.mariadb_mtr_core.subprocess.run", return_value=CompletedProcess([], 0, "+00:00\n", "")
        ):
            with self.assertRaisesRegex(RuntimeError, "does not match required"):
                configure_case_timezone(Server("mysqweel", "mysql://root@localhost/test"), Path("/bin"), Path("/suite"), case)

    def test_same_host_and_port_for_both_servers_is_rejected(self):
        with self.assertRaisesRegex(ValueError, "same host and port"):
            validate_distinct_servers(
                "mysql://root:baseline@127.0.0.1:3306/test",
                "mysql://root@127.0.0.1:3306/test",
            )

    def test_separate_mysqweel_server_is_accepted(self):
        validate_distinct_servers(
            "mysql://root:baseline@127.0.0.1:3306/test",
            "mysql://root@127.0.0.1:3307/test",
        )

    def test_comments_and_duplicates(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "allowlist.txt"
            path.write_text(
                f"# comment\nselect_all query {DIGEST_A} {DIGEST_B} # supported\n"
                f"func_math scalar {DIGEST_B} {DIGEST_A}\n"
            )
            self.assertEqual([case.name for case in parse_manifest(path)], ["select_all", "func_math"])

    def test_empty_manifest_is_rejected(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "allowlist.txt"
            path.write_text("# no tests\n")
            with self.assertRaises(ValueError):
                parse_manifest(path)

    def test_duplicate_manifest_is_rejected(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "allowlist.txt"
            path.write_text(
                f"select_all query {DIGEST_A} {DIGEST_B}\n"
                f"select_all query {DIGEST_A} {DIGEST_B}\n"
            )
            with self.assertRaises(ValueError):
                parse_manifest(path)

    def test_upstream_hash_mismatch_is_rejected(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "mysql-test" / "main").mkdir(parents=True)
            (root / "mysql-test" / "main" / "select_all.test").write_text("SELECT 1;\n")
            (root / "mysql-test" / "main" / "select_all.result").write_text("SELECT 1;\n1\n1\n")
            manifest = root / "allowlist.txt"
            manifest.write_text(f"select_all query {DIGEST_A} {DIGEST_B}\n")
            with self.assertRaisesRegex(ValueError, "test hash mismatch"):
                validate_cases(root, parse_manifest(manifest))

    def test_noop_safe_process_is_rejected(self):
        with tempfile.TemporaryDirectory() as directory:
            bindir = Path(directory)
            safe_process = bindir / "mysqltest_safe_process"
            safe_process.write_text("#!/bin/sh\nexit 0\n")
            os.chmod(safe_process, 0o755)
            with self.assertRaisesRegex(RuntimeError, "did not execute the canary child process"):
                validate_mtr_runtime(bindir, Path(sys.executable))

    def test_suite_test_uses_its_suite_and_basename(self):
        case = TestCase("json/functions", "json", DIGEST_A, DIGEST_B, "manifest")
        command = mtr_command(
            Path("/mysql"),
            Path("/mysql/mysql-test/mysql-test-run.pl"),
            Path("/mysql/bin"),
            Server("mysql", "mysql://root@127.0.0.1:3306/test"),
            case,
            Path("/tmp/vardir"),
        )
        self.assertIn("--suite=json", command)
        self.assertEqual(command[-1], "functions")

    def test_sql_statement_count_ignores_directives_comments_and_quoted_semicolons(self):
        source = """
--disable_warnings
# comment;
CREATE TABLE t1 (value VARCHAR(20));
INSERT INTO t1 VALUES ('a;b'), ("c;d"), (`value`);
/* ignored; */ SELECT * FROM t1;
"""
        self.assertEqual(sql_statement_count(source), 3)

    def test_discovery_separates_static_candidates_from_harness_dependencies(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            test_dir = root / "mysql-test" / "main"
            result_dir = root / "mysql-test" / "main"
            test_dir.mkdir(parents=True)
            (test_dir / "plain.test").write_text("SELECT 1;\n")
            (result_dir / "plain.result").write_text("SELECT 1;\n1\n1\n")
            (test_dir / "sourced.test").write_text("-- source include/have_innodb.inc\nSELECT 1;\n")
            (result_dir / "sourced.result").write_text("SELECT 1;\n1\n1\n")
            cases = {case.name: case for case in discover_cases(root, "main", 200)}
            self.assertIsNone(cases["plain"].exclusion)
            self.assertEqual(cases["plain"].statements, 1)
            self.assertEqual(cases["sourced"].exclusion, "harness-dependency")

    def test_aggressive_discovery_follows_safe_sources_and_rejects_hidden_routines(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            test_dir = root / "mysql-test" / "main"
            result_dir = root / "mysql-test" / "main"
            include_dir = root / "mysql-test" / "include"
            test_dir.mkdir(parents=True)
            include_dir.mkdir(parents=True)
            (include_dir / "query.inc").write_text("SELECT 2;\n")
            (include_dir / "routine.inc").write_text(
                "CREATE PROCEDURE hidden_routine() SELECT 1;\n"
            )
            (test_dir / "sourced.test").write_text(
                "-- source include/query.inc\nSELECT 1;\n"
            )
            (result_dir / "sourced.result").write_text("SELECT 1;\n1\n1\n")
            (test_dir / "routine.test").write_text(
                "source 'include/routine.inc';\nSELECT 1;\n"
            )
            (result_dir / "routine.result").write_text("SELECT 1;\n1\n1\n")

            cases = {
                case.name: case
                for case in discover_cases(
                    root, "main", 200, include_safe_harness=True
                )
            }
            self.assertIsNone(cases["sourced"].exclusion)
            self.assertEqual(cases["sourced"].statements, 2)
            self.assertEqual(cases["routine"].exclusion, "outside-contract")

    def test_aggressive_discovery_rejects_harness_side_effects(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            test_dir = root / "mysql-test" / "main"
            result_dir = root / "mysql-test" / "main"
            test_dir.mkdir(parents=True)
            (test_dir / "writes_file.test").write_text(
                "-- write_file $MYSQL_TMP_DIR/data.txt\nvalue\nEOF\nSELECT 1;\n"
            )
            (result_dir / "writes_file.result").write_text("SELECT 1;\n1\n1\n")
            cases = {
                case.name: case
                for case in discover_cases(
                    root, "main", 200, include_safe_harness=True
                )
            }
            self.assertEqual(cases["writes_file"].exclusion, "harness-side-effect")

    def test_discovery_admits_transaction_commands_and_sourced_savepoints(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            test_dir = root / "mysql-test" / "main"
            include_dir = root / "mysql-test" / "include"
            test_dir.mkdir(parents=True)
            include_dir.mkdir()
            (include_dir / "savepoints.inc").write_text(
                "SAVEPOINT s; ROLLBACK TO SAVEPOINT s; RELEASE SAVEPOINT s;\n"
            )
            sources = {
                "begin": "BEGIN; SELECT 1; COMMIT;",
                "begin_work": "BEGIN WORK; SELECT 1; ROLLBACK WORK;",
                "start": "START TRANSACTION; SELECT 1; ROLLBACK;",
                "autocommit": "SET @@session.autocommit=0; SELECT 1; SET autocommit=1;",
                "savepoints": "--source include/savepoints.inc\nSELECT 1;",
                "repeatable_read": "SET SESSION TRANSACTION ISOLATION LEVEL REPEATABLE READ; BEGIN; COMMIT;",
                "xa": "XA START 'x'; SELECT 1; XA END 'x'; XA COMMIT 'x';",
                "table_lock": "LOCK TABLES t WRITE; UNLOCK TABLES;",
                "shared_lock": "SELECT * FROM t LOCK IN SHARE MODE;",
                "read_committed": "SET TRANSACTION ISOLATION LEVEL READ COMMITTED; SELECT 1;",
                "serializable": "SET TRANSACTION ISOLATION LEVEL SERIALIZABLE; SELECT 1;",
            }
            for name, source in sources.items():
                (test_dir / f"{name}.test").write_text(source)
                (test_dir / f"{name}.result").write_text("")
            cases = {
                case.name: case
                for case in discover_cases(root, "main", 200, include_safe_harness=True)
            }
            for name in ("begin", "begin_work", "start", "autocommit", "savepoints", "repeatable_read"):
                with self.subTest(name=name):
                    self.assertIsNone(cases[name].exclusion)
                    self.assertIn("transactions", cases[name].feature.split("-"))
            for name in ("xa", "table_lock", "shared_lock", "read_committed", "serializable"):
                with self.subTest(name=name):
                    self.assertEqual(cases[name].exclusion, "outside-contract")

    def test_safe_harness_admits_only_reviewed_innodb_cases(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            suite = root / "mysql-test" / "suite" / "innodb"
            (suite / "t").mkdir(parents=True)
            (suite / "r").mkdir()
            for name in ("innodb_bug57255", "unreviewed_transaction"):
                (suite / "t" / f"{name}.test").write_text("BEGIN; SELECT 1; COMMIT;")
                (suite / "r" / f"{name}.result").write_text("")
            cases = {
                case.name: case
                for case in discover_cases(root, "all", 200, include_safe_harness=True)
            }
            self.assertIsNone(cases["innodb/innodb_bug57255"].exclusion)
            self.assertEqual(
                cases["innodb/unreviewed_transaction"].exclusion, "outside-contract-suite"
            )

    def test_mariadb_layout_uses_main_for_top_level_cases(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            test_dir = root / "mysql-test" / "main"
            test_dir.mkdir(parents=True)
            (test_dir / "simple.test").write_text("SELECT 1;\n")
            (test_dir / "simple.result").write_text("SELECT 1;\n1\n")
            cases = discover_cases(root, "main", 200, layout="mariadb")
            self.assertEqual([case.name for case in cases], ["simple"])
            validate_cases(
                root,
                [
                    TestCase(
                        "simple",
                        "query",
                        hashlib.sha256((test_dir / "simple.test").read_bytes()).hexdigest(),
                        hashlib.sha256((test_dir / "simple.result").read_bytes()).hexdigest(),
                        "test",
                    )
                ],
                layout="mariadb",
            )

    def test_mtr_case_timezone_translates_posix_gmt_offset(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            test_dir = root / "mysql-test" / "main"
            test_dir.mkdir(parents=True)
            (test_dir / "timezone4.test").write_text("SELECT FROM_UNIXTIME(0);\n")
            (test_dir / "timezone4-master.opt").write_text("--timezone=GMT+10\n")
            case = TestCase("timezone4", "date-time", DIGEST_A, DIGEST_B, "test")
            self.assertEqual(mtr_case_timezone(root, case, "mariadb"), "-10:00")

    def test_mtr_case_timezone_defaults_to_utc(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            test_dir = root / "mysql-test" / "main"
            test_dir.mkdir(parents=True)
            (test_dir / "simple.test").write_text("SELECT 1;\n")
            case = TestCase("simple", "query", DIGEST_A, DIGEST_B, "test")
            self.assertEqual(mtr_case_timezone(root, case, "mariadb"), "+00:00")

    def test_mtr_case_timezone_rejects_nonportable_server_timezone(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            test_dir = root / "mysql-test" / "main"
            test_dir.mkdir(parents=True)
            (test_dir / "timezone.test").write_text("SELECT NOW();\n")
            (test_dir / "timezone-master.opt").write_text("--timezone=MET\n")
            case = TestCase("timezone", "date-time", DIGEST_A, DIGEST_B, "test")
            with self.assertRaisesRegex(RuntimeError, "fixed GMT offset"):
                mtr_case_timezone(root, case, "mariadb")

    def test_rotating_discovery_selection_wraps(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            test_dir = root / "mysql-test" / "main"
            result_dir = root / "mysql-test" / "main"
            test_dir.mkdir(parents=True)
            for name in ("alpha", "bravo", "charlie"):
                (test_dir / f"{name}.test").write_text("SELECT 1;\n")
                (result_dir / f"{name}.result").write_text("SELECT 1;\n1\n1\n")
            cases = discover_cases(root, "main", 200)
            selected = rotating_selection(cases, offset=2, limit=2)
            self.assertEqual([case.name for case in selected], ["charlie", "alpha"])

    def test_promotion_manifest_contains_only_dual_engine_passes(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            report = root / "report.json"
            manifest = root / "promoted.txt"
            report.write_text(
                json.dumps(
                    {
                        "source_revision": "abc123",
                        "results": [
                            {
                                "test": "passing",
                                "feature": "query",
                                "test_sha256": DIGEST_A,
                                "result_sha256": DIGEST_B,
                                "baseline": "pass",
                                "mysqweel": "pass",
                            },
                            {
                                "test": "failing",
                                "feature": "query",
                                "test_sha256": DIGEST_B,
                                "result_sha256": DIGEST_A,
                                "baseline": "pass",
                                "mysqweel": "fail",
                            },
                        ],
                    }
                )
            )
            write_promotion_manifest(
                Namespace(compat_report=report, promote_manifest=manifest)
            )
            promoted = manifest.read_text()
            self.assertIn("passing query", promoted)
            self.assertNotIn("failing query", promoted)


class ReportTests(unittest.TestCase):
    def test_report_contains_score_and_test_statuses(self):
        report = {
            "baseline_label": "MariaDB",
            "baseline_version": "10.11.7",
            "source_revision": "abc123",
            "status": "fail",
            "score_percent": 50.0,
            "counts": {"included": 2, "passed": 1, "failed": 1, "infrastructure": 0},
            "results": [
                {"test": "select_all", "feature": "select", "baseline": "pass", "mysqweel": "pass"},
                {"test": "join", "feature": "join", "baseline": "pass", "mysqweel": "fail"},
            ],
        }
        markdown = render_markdown(report)
        self.assertIn("Score: 50.0%", markdown)
        self.assertIn("`join` | `join` | 0 | pass | fail", markdown)

    def test_report_includes_threshold(self):
        report = {
            "baseline_label": "MariaDB",
            "baseline_version": "10.11.7",
            "source_revision": "abc123",
            "status": "pass",
            "score_percent": 90.0,
            "minimum_percent": 90.0,
            "counts": {"included": 1, "passed": 1, "failed": 0, "infrastructure": 0},
            "results": [],
        }
        self.assertIn("Status: **pass**", render_markdown(report))

    def test_report_includes_one_representative_failure_per_server(self):
        report = {
            "baseline_label": "MariaDB",
            "baseline_version": "10.11.7",
            "source_revision": "abc123",
            "status": "invalid",
            "score_percent": 0.0,
            "counts": {"included": 1, "passed": 0, "failed": 1, "infrastructure": 0},
            "results": [
                {"test": "select_all", "feature": "select", "baseline": "fail", "mysqweel": "fail"}
            ],
            "invocations": [
                {
                    "test": "select_all",
                    "server": "mariadb",
                    "status": "fail",
                    "returncode": 1,
                    "stdout": "",
                    "stderr": "missing mariadb-import",
                },
                {
                    "test": "select_all",
                    "server": "mysqweel",
                    "status": "fail",
                    "returncode": 1,
                    "stdout": "result mismatch",
                    "stderr": "",
                },
            ],
        }
        markdown = render_markdown(report)
        self.assertIn("Representative failure diagnostics", markdown)
        self.assertIn("### mariadb: `select_all`", markdown)
        self.assertIn("missing mariadb-import", markdown)
        self.assertIn("### mysqweel: `select_all`", markdown)
        self.assertIn("result mismatch", markdown)

    def test_json_report_is_serializable(self):
        report = {"counts": {"included": 1}, "results": []}
        self.assertEqual(json.loads(json.dumps(report))["counts"]["included"], 1)


if __name__ == "__main__":
    unittest.main()
