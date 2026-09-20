#!/usr/bin/env python3
import unittest

from tools.mariadb_mtr_scope import classify_scope


class MariaDBScopeClassificationTests(unittest.TestCase):
    def test_routine_mixed_with_query_is_not_silently_excluded(self):
        result = classify_scope(
            "SELECT 1;\n"
            "CREATE PROCEDURE p()\n"
            "BEGIN\n"
            "SELECT 2;\n"
            "END;\n"
        )
        self.assertEqual(result["status"], "mixed")
        self.assertFalse(result["reviewed"])
        self.assertIn(
            {"kind": "in-scope", "feature": "queries", "line": 1},
            result["evidence"],
        )
        self.assertIn(
            {
                "kind": "outside-contract",
                "feature": "routines-triggers-events",
                "line": 2,
            },
            result["evidence"],
        )

    def test_transactions_and_basic_provisioning_are_supported(self):
        result = classify_scope(
            "START TRANSACTION;\n"
            "SET TRANSACTION ISOLATION LEVEL REPEATABLE READ;\n"
            "CREATE USER 'app'@'%';\n"
            "GRANT SELECT, INSERT ON appdb.* TO 'app'@'%';\n"
            "REVOKE ALL PRIVILEGES ON appdb.* FROM 'app'@'%';\n"
            "ROLLBACK TO SAVEPOINT before_write;\n"
            "COMMIT;\n"
            "DROP USER 'app'@'%';\n"
        )
        self.assertEqual(result["status"], "in-scope")
        self.assertEqual(
            {row["feature"] for row in result["evidence"]},
            {"transactions", "provisioning"},
        )
        self.assertEqual(
            {row["feature"]: row["line"] for row in result["evidence"]},
            {"transactions": 1, "provisioning": 3},
        )

    def test_database_provisioning_is_not_inherited_from_old_exclusions(self):
        result = classify_scope(
            "CREATE DATABASE appdb;\n"
            "DROP DATABASE appdb;\n"
        )
        self.assertEqual(result["status"], "in-scope")
        self.assertEqual(
            result["evidence"],
            [
                {"kind": "in-scope", "feature": "ddl", "line": 1},
                {"kind": "in-scope", "feature": "provisioning", "line": 1},
            ],
        )

    def test_pure_outside_evidence_requires_review(self):
        result = classify_scope("CREATE TRIGGER audit_row BEFORE INSERT ON t FOR EACH ROW SET @x = 1;\n")
        self.assertEqual(result["status"], "review-required")
        self.assertFalse(result["reviewed"])
        self.assertEqual(
            result["evidence"],
            [
                {
                    "kind": "outside-contract",
                    "feature": "routines-triggers-events",
                    "line": 1,
                }
            ],
        )

    def test_unknown_sql_requires_review_without_guessing(self):
        result = classify_scope("FLURBLE OPERATION;\n")
        self.assertEqual(result["status"], "review-required")
        self.assertEqual(result["evidence"], [])
        self.assertFalse(result["reviewed"])

    def test_masked_literals_and_comments_cannot_create_scope_evidence(self):
        # The caller's masker preserves newlines while replacing quoted text and
        # comments with spaces.  The classifier consumes that representation.
        masked = "SELECT                       ;\n"
        masked += "                         \n"
        masked += "                         ;\n"
        result = classify_scope(masked)
        self.assertEqual(result["status"], "in-scope")
        self.assertEqual(
            result["evidence"],
            [{"kind": "in-scope", "feature": "queries", "line": 1}],
        )

    def test_savepoint_rollback_and_for_update_are_not_false_outside_signals(self):
        result = classify_scope(
            "START TRANSACTION;\n"
            "SELECT id FROM t FOR UPDATE;\n"
            "ROLLBACK TO SAVEPOINT before_write;\n"
        )
        self.assertEqual(result["status"], "in-scope")
        self.assertNotIn(
            "unsupported-isolation-xa-locking",
            {row["feature"] for row in result["evidence"] if row["kind"] == "outside-contract"},
        )
        self.assertEqual(
            {row["feature"] for row in result["evidence"]},
            {"transactions", "queries"},
        )
        self.assertEqual(
            {row["feature"]: row["line"] for row in result["evidence"]},
            {"transactions": 1, "queries": 2},
        )

    def test_window_partitions_are_supported_but_table_partition_is_outside(self):
        window = classify_scope(
            "SELECT SUM(value) OVER (PARTITION BY group_id ORDER BY id) FROM t;\n"
            "SELECT SUM(value) OVER win FROM t WINDOW win AS (PARTITION BY group_id);\n"
        )
        self.assertEqual(window["status"], "in-scope")
        self.assertNotIn(
            "physical-storage",
            {row["feature"] for row in window["evidence"] if row["kind"] == "outside-contract"},
        )

        mixed = classify_scope(
            "SELECT SUM(value) OVER (PARTITION BY group_id) FROM t;\n"
            "CREATE TABLE p (id INT) PARTITION BY HASH(id);\n"
        )
        self.assertEqual(mixed["status"], "mixed")
        self.assertIn(
            {"kind": "outside-contract", "feature": "physical-storage", "line": 2},
            mixed["evidence"],
        )

    def test_unknown_statement_keeps_known_sql_at_review(self):
        result = classify_scope("SELECT 1;\nFLURBLE OPERATION;\n")
        self.assertEqual(result["status"], "review-required")
        self.assertIn({"kind": "in-scope", "feature": "queries", "line": 1}, result["evidence"])


    def test_session_setting_detection_is_case_insensitive(self):
        result = classify_scope("set names utf8mb4;\n")
        self.assertEqual(result["status"], "in-scope")
        self.assertEqual(
            result["evidence"],
            [{"kind": "in-scope", "feature": "session-settings", "line": 1}],
        )


    def test_xa_rollback_is_outside_but_savepoint_rollback_is_transactional(self):
        result = classify_scope("XA ROLLBACK xid;\n")
        self.assertEqual(result["status"], "review-required")
        self.assertEqual(
            result["evidence"],
            [
                {
                    "kind": "outside-contract",
                    "feature": "unsupported-isolation-xa-locking",
                    "line": 1,
                }
            ],
        )

if __name__ == "__main__":
    unittest.main()
