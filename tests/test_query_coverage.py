"""Default tests use fakes; the opt-in live test uses containers but never a model."""
import contextlib
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import time
import unittest
from unittest.mock import patch
import urllib.error

from tools import query_coverage as coverage
from tools.mariadb_mtr_core import TestCase as MtrCase, merge_manifests


def fixture(case_id="example"):
    return {
        "version": 1, "id": case_id,
        "provenance": {"inspiration": {
            "source_location": "line 1", "source_dialect": "postgresql",
            "observed_pattern": "derived scalar expression",
            "mysql_mariadb_translation": "independently authored scalar expression",
        }},
        "features": ["expression"], "fixture_notes": "Tests integer arithmetic.",
        "determinism_notes": "One scalar value with no ordering ambiguity.",
        "steps": [{"sql": "SELECT 1 + 1 AS value"}],
    }


class QueueTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.path = Path(self.temp.name)
        self.queue = coverage.Queue(self.path / "queue.db")
        self.addCleanup(self.queue.db.close)

    def test_literal_variants_survive_and_structures_group(self):
        a = self.queue.add("github", "SELECT x FROM t WHERE x = 1", {})
        b = self.queue.add("github", "select x from t where x = 2", {})
        self.assertNotEqual(a, b)
        self.assertEqual(self.queue.get(a)["shape"], self.queue.get(b)["shape"])
        self.assertEqual(a, self.queue.add("github", "SELECT x FROM t WHERE x = 1 -- comment", {}))
        self.assertNotEqual(a, self.queue.add("github", "SELECT x FROM T WHERE x = 1", {}))
        first = coverage.sql_shape("SELECT x FROM t WHERE x = 'hello world'")
        second = coverage.sql_shape("SELECT y FROM u WHERE y = 'a b c'")
        self.assertEqual(first[1], second[1])
        self.assertNotEqual(coverage.sql_shape("SELECT 1")[0], coverage.sql_shape("SELECT 1--1")[0])
        self.assertNotEqual(coverage.sql_shape("SELECT 1")[0], coverage.sql_shape("SELECT 1 /*! + 1 */")[0])

    def test_alternation_novelty_and_restart(self):
        a = self.queue.add("github", "SELECT 1", {})
        self.queue.set(a, "ready")
        old = self.queue.add("github", "SELECT 2", {})
        novel = self.queue.add("github", "SELECT a FROM a LEFT JOIN b ON a.id=b.id", {})
        mtr = self.queue.add("mtr", "SELECT 3", {})
        self.queue.meta("last_source", "mtr")
        self.assertEqual(self.queue.claim()["id"], novel)
        self.queue.set(novel, "running", attempts=2)
        self.assertEqual(self.queue.claim()["id"], mtr)
        self.queue.recover()
        self.assertEqual(self.queue.get(novel)["state"], "queued")
        self.assertEqual(self.queue.get(novel)["attempts"], 2)
        self.queue.set(novel, "parked", "attempt budget exhausted", attempts=3)
        self.queue.retry(novel)
        self.assertEqual(self.queue.get(novel)["attempts"], 0)
        self.queue.set(old, "queued", next_run=time.time() + 60)
        self.queue.set(novel, "parked")
        self.queue.set(mtr, "parked")
        self.assertIsNone(self.queue.claim())
        self.assertEqual(self.queue.report()["ready_structures"], 1)

    def test_worker_lock_excludes_second_owner(self):
        with coverage.worker_lock(self.path):
            with self.assertRaises(coverage.InfrastructureError):
                with coverage.worker_lock(self.path):
                    self.fail("second worker acquired the lock")

    def test_rate_limit_is_persistent_and_does_not_repeat_request(self):
        client = coverage.GitHub(self.path, self.queue)
        error = urllib.error.HTTPError("url", 429, "limited", {"Retry-After": "120"}, None)
        self.addCleanup(error.close)
        with patch("urllib.request.urlopen", side_effect=error) as request:
            with self.assertRaises(coverage.RateLimited):
                client.get("/search/code?q=test")
            with self.assertRaises(coverage.RateLimited):
                client.get("/search/code?q=test")
            self.assertEqual(request.call_count, 1)
        self.assertGreater(self.queue.meta("github_backoff"), time.time())

    def test_github_cache_is_used_without_network(self):
        client = coverage.GitHub(self.path, self.queue)
        endpoint = "/repos/example/project/contents/a.sql?ref=abc"
        coverage.write_json(client.cache / (coverage.digest(endpoint) + ".json"), {"cached": True})
        with patch("urllib.request.urlopen", side_effect=AssertionError("network access")):
            self.assertEqual(client.get(endpoint), {"cached": True})

    def test_failure_groups_are_scoped_to_base_commit(self):
        first = self.queue.add("github", "SELECT 1", {})
        second = self.queue.add("github", "SELECT 2", {})
        self.assertIsNone(self.queue.group_failure("same", "base1", first, self.path))
        self.assertEqual(self.queue.group_failure("same", "base1", second, self.path), first)
        self.assertIsNone(self.queue.group_failure("same", "base2", second, self.path))
        self.assertEqual(self.queue.report()["related_failures"][0]["parent_id"], first)


class FixtureTests(unittest.TestCase):
    def test_rejects_invalid_or_unverifiable_cases(self):
        with tempfile.TemporaryDirectory() as root:
            path = Path(root) / "example.json"
            for change in [
                {"version": 2}, {"steps": [{"sql": "SELECT 1 LIMIT 1"}]},
                {"steps": [{"sql": "SELECT RAND()"}]},
                {"steps": [{"sql": "UPDATE items SET value=1"}]},
                {"steps": [{"sql": "CREATE PROCEDURE p() SELECT 1"}]},
                {"steps": [{"sql": "SELECT 1", "ordered": "false"}]},
                {"checks": [{"sql": "DELETE FROM items"}]},
                {"steps": [None]}, {"setup": [3]},
            ]:
                coverage.write_json(path, {**fixture(), **change})
                with self.subTest(change=change), self.assertRaises(coverage.Parked):
                    coverage.validate_case(path, "example")
            path.write_text("broken JSON")
            with self.assertRaises(coverage.Parked):
                coverage.validate_case(path, "example")

    def test_accepts_final_state_and_ignores_keywords_in_literals(self):
        with tempfile.TemporaryDirectory() as root:
            path = Path(root) / "example.json"
            case = fixture()
            case["steps"] = [{"sql": "UPDATE items SET value='CREATE PROCEDURE ignored'"}]
            case["checks"] = [{"sql": "SELECT * FROM items"}]
            coverage.write_json(path, case)
            self.assertEqual(coverage.validate_case(path, "example"), case)

    def test_committed_cases_validate(self):
        for path in (coverage.ROOT / "tests/query_cases").glob("*.json"):
            coverage.validate_case(path, path.stem)

    def test_manifest_merge_preserves_pins(self):
        a, b = "a" * 64, "b" * 64
        original = MtrCase("one", "query", a, b, "original")
        with tempfile.TemporaryDirectory() as root:
            directory = Path(root)
            (directory / "case.txt").write_text(f"one query {a} {b}\ntwo query {a} {b}\n")
            result = merge_manifests([original], directory)
            self.assertEqual([case.name for case in result], ["one", "two"])
            self.assertEqual(result[0].source, "original")
            (directory / "conflict.txt").write_text(f"one query {b} {b}\n")
            with self.assertRaisesRegex(ValueError, "conflicting"):
                merge_manifests([original], directory)


FAKE_AGENT = '''
import json, pathlib, re, sys
prompt = pathlib.Path(sys.argv[-1][1:]).read_text()
case_id = re.search(r"Task ID: ([a-f0-9]+)", prompt).group(1)
task = pathlib.Path(re.search(r"Write outputs under (.*?)\\. Write agent-result", prompt).group(1))
cwd = pathlib.Path(next(a.split("=", 1)[1] for a in sys.argv if a.startswith("--cwd=")))
if "# Repair" in prompt:
    (cwd / "src/sql/example.rs").write_text("pub fn value() -> i32 { 2 }\\n")
else:
    minimal = "# Minimize" in prompt
    identifier = case_id + ("-minimal" if minimal else "")
    case = {
        "version": 1, "id": identifier,
        "provenance": {"inspiration": {
            "source_location": "line 1", "source_dialect": "postgresql",
            "observed_pattern": "derived scalar expression",
            "mysql_mariadb_translation": "independently authored scalar expression",
        }},
        "features": ["expression"], "fixture_notes": "Scalar fixture",
        "determinism_notes": "One row", "steps": [{"sql": "SELECT 1 + 1 AS value"}]
    }
    (task / (identifier + ".json")).write_text(json.dumps(case))
(task / "agent-result.json").write_text(json.dumps({"status": "complete"}))
print(json.dumps({"type": "fake-agent", "model": "test-double"}))
'''


class WorkerTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        root = Path(self.temp.name)
        self.repo, self.state = root / "repo", root / "state"
        self.repo.mkdir()
        self.state.mkdir()
        (self.repo / "src/sql").mkdir(parents=True)
        (self.repo / "src/sql/example.rs").write_text("pub fn value() -> i32 { 1 }\n")
        (self.repo / "tests/query_cases").mkdir(parents=True)
        (self.repo / "tests/query_cases/.keep").write_text("")
        (self.repo / "tests/query_coverage.rs").write_text("// protected comparator\n")
        coverage.git(self.repo, "init", "--quiet")
        coverage.git(self.repo, "add", ".")
        coverage.git(self.repo, "-c", "user.name=Test", "-c", "user.email=test@localhost", "commit", "--quiet", "-m", "base")
        fake = root / "fake_agent.py"
        fake.write_text(FAKE_AGENT)
        self.config = {**coverage.DEFAULTS, "agent_command": [sys.executable, str(fake)], "model": "test-double", "state_dir": str(self.state)}
        self.queue = coverage.Queue(self.state / "queue.db")
        self.addCleanup(self.queue.db.close)
        self.worker = coverage.Worker(self.repo, self.state, self.config, self.queue)

    def row(self, source_dialect="postgresql"):
        key = self.queue.add("github", "SELECT source_value::INTEGER FROM external_report WHERE source_value IS NOT NULL", {"kind": "github", "repository": "example/project", "commit": "abc", "path": "query.sql", "source_dialect": source_dialect})
        self.queue.set(key, "running")
        return self.queue.get(key)

    def test_complete_passing_and_repair_workflows_use_only_fake_agent(self):
        for needs_fix in (False, True):
            with self.subTest(needs_fix=needs_fix):
                row = self.row()
                def compare(checkout, task, fixture_path, label, baseline_only=False):
                    coverage.validate_case(fixture_path, fixture_path.stem)
                    fixed = "{ 2 }" in (checkout / "src/sql/example.rs").read_text()
                    return {"status": "mismatch" if needs_fix and not baseline_only and not fixed else "pass"}
                with patch.object(self.worker, "compare", side_effect=compare), patch.object(self.worker, "checks", return_value=True):
                    self.worker.process(row)
                result = self.queue.get(row["id"])
                self.assertEqual(result["state"], "ready")
                head = coverage.git(self.repo, "rev-parse", result["branch"])
                self.assertNotEqual(head, self.worker.base)
                self.assertIn(row["id"], coverage.git(self.repo, "show", "--format=%s", "--no-patch", head))
                report = json.loads(Path(result["note"].split("see ", 1)[1]).read_text())
                self.assertEqual(report["tested_commit"], head)
                self.assertEqual(report["kind"], "fix" if needs_fix else "test-only")
                self.assertEqual(coverage.git(self.repo, "rev-parse", "HEAD"), self.worker.base)

    def test_missing_reference_prevents_agent_invocation(self):
        self.row()
        self.queue.meta("last_discovery", time.time())
        with patch.object(self.worker, "checks", side_effect=coverage.InfrastructureError("Docker unavailable")), patch.object(self.worker, "agent", side_effect=AssertionError("agent must not run")):
            self.worker.run(once=True)
        self.assertFalse(self.queue.meta("qualification")["passed"])
        self.assertEqual(self.queue.report()["states"], {"queued": 1})

    def test_unlicensed_sources_are_used_as_inspiration(self):
        row = self.row()
        with patch.object(self.worker, "compare", return_value={"status": "pass"}), patch.object(self.worker, "checks", return_value=True):
            self.worker.process(row)
        self.assertEqual(self.queue.get(row["id"])["state"], "ready")

    def test_verbatim_source_statement_is_rejected(self):
        case = fixture()
        case["steps"] = [{"sql": "SELECT 1 + 1 AS calculated_value"}]
        with self.assertRaisesRegex(coverage.Parked, "copies"):
            coverage.validate_inspiration(case, "SELECT 1 + 1 AS calculated_value", {"source_dialect": "postgresql"})

    def test_inspiration_requires_a_matching_dialect_and_complete_evidence(self):
        case = fixture()
        with self.assertRaisesRegex(coverage.Parked, "dialect"):
            coverage.validate_inspiration(case, "SELECT source_value", {"source_dialect": "sqlite"})
        del case["provenance"]["inspiration"]["observed_pattern"]
        with self.assertRaisesRegex(coverage.Parked, "incomplete"):
            coverage.validate_inspiration(case, "SELECT source_value", {"source_dialect": "postgresql"})

    def test_protected_comparator_and_embedded_tests(self):
        checkout = self.worker.checkout("protected")
        (checkout / "tests/query_coverage.rs").write_text("// weakened\n")
        with self.assertRaisesRegex(coverage.Parked, "protected"):
            self.worker.protected(checkout, self.worker.base)

    def test_invalid_agent_response_and_timeout_park(self):
        task = self.state / "task"
        task.mkdir()
        (task / "agent-result.json").write_text("[]")
        def fake_command(argv, cwd, log, seconds, env):
            log.write_text("")
            return subprocess.CompletedProcess([], 0)
        with patch.object(coverage, "command", side_effect=fake_command), self.assertRaisesRegex(coverage.Parked, "object"):
            self.worker.agent(self.repo, task, "fixture", "fake task")
        with patch.object(coverage, "command", side_effect=subprocess.TimeoutExpired("fake", 1)), self.assertRaisesRegex(coverage.Parked, "budget"):
            self.worker.agent(self.repo, task, "fixture", "fake task")

    def test_exhausted_repairs_never_promote(self):
        row = self.row()
        def compare(checkout, task, fixture_path, label, baseline_only=False):
            return {"status": "pass" if baseline_only else "mismatch"}
        with patch.object(self.worker, "compare", side_effect=compare), self.assertRaisesRegex(coverage.Parked, "exhausted"):
            self.worker.process(row)
        self.assertEqual(self.queue.get(row["id"])["attempts"], 3)
        self.assertNotEqual(self.queue.get(row["id"])["state"], "ready")

    def test_changed_reproducer_is_rejected(self):
        row = self.row()
        real_agent = self.worker.agent
        def agent(checkout, task, stage, instruction):
            real_agent(checkout, task, stage, instruction)
            if stage == "repair":
                (task / (row["id"] + "-minimal.json")).write_text("{}")
        with patch.object(self.worker, "agent", side_effect=agent), patch.object(self.worker, "compare", side_effect=lambda *args: {"status": "pass" if len(args) > 4 and args[4] else "mismatch"}), self.assertRaisesRegex(coverage.Parked, "minimized"):
            self.worker.process(row)


class ProcessTests(unittest.TestCase):
    def test_timeout_terminates_process(self):
        with tempfile.TemporaryDirectory() as root:
            path = Path(root)
            with self.assertRaises(subprocess.TimeoutExpired):
                coverage.command([sys.executable, "-c", "import time; time.sleep(60)"], path, path / "process.log", 0.05)

    def test_container_recovery_checks_ownership(self):
        with tempfile.TemporaryDirectory() as root:
            state = Path(root)
            name = "mysqweel-coverage-" + "a" * 16
            marker = state / "containers" / (name + ".json")
            coverage.write_json(marker, {"name": name})
            config = {**coverage.DEFAULTS, "state_dir": str(state), "container_command": ["fake-container-runtime"]}
            with patch("subprocess.run", return_value=subprocess.CompletedProcess([], 0, "another-owner", "")) as run:
                with self.assertRaisesRegex(coverage.InfrastructureError, "another worker"):
                    coverage.recover_containers(config)
                self.assertEqual(run.call_count, 1)
            owner = coverage.digest(str(state.resolve()))
            with patch("subprocess.run", side_effect=[subprocess.CompletedProcess([], 0, owner, ""), subprocess.CompletedProcess([], 0)]) as run:
                coverage.recover_containers(config)
                self.assertEqual(run.call_args.args[0], ["fake-container-runtime", "rm", "-f", name])
            self.assertFalse(marker.exists())


@unittest.skipUnless(os.environ.get("QUERY_COVERAGE_LIVE_CONTAINER_RUNTIME"), "opt-in disposable-container test")
class LiveControllerTests(unittest.TestCase):
    def test_real_reference_comparison_and_container_cleanup_without_agent(self):
        with tempfile.TemporaryDirectory() as root:
            state = Path(root)
            config = {**coverage.DEFAULTS, "state_dir": str(state),
                      "container_command": [os.environ["QUERY_COVERAGE_LIVE_CONTAINER_RUNTIME"]]}
            queue = coverage.Queue(state / "queue.db")
            self.addCleanup(queue.db.close)
            worker = coverage.Worker(coverage.ROOT, state, config, queue)
            fixture_path = coverage.ROOT / "tests/query_cases/rollback_state.json"
            with patch.object(worker, "agent", side_effect=AssertionError("model invocation prohibited")):
                baseline = worker.compare(coverage.ROOT, state, fixture_path, "baseline", True)
                result = worker.compare(coverage.ROOT, state, fixture_path, "both")
            self.assertEqual(result["status"], "pass")
            self.assertEqual(result["baseline"], baseline["baseline"])
            self.assertEqual(list((state / "containers").glob("*.json")), [])


if __name__ == "__main__":
    unittest.main()
