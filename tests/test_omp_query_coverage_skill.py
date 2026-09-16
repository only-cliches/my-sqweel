import json
import os
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

from urllib.error import HTTPError

from importlib.util import module_from_spec, spec_from_file_location


ROOT = Path(__file__).resolve().parents[1]
SCRIPT = ROOT / ".omp/skills/mysqweel-query-coverage/scripts/github_sql_search.py"
SKILL = ROOT / ".omp/skills/mysqweel-query-coverage/SKILL.md"
LOCAL_SERVER_HELPER = ROOT / "vendor/mysql-test-server.sh"
DOCKERFILE = ROOT / "my-sqweel.dockerfile"
SPEC = spec_from_file_location("github_sql_search", SCRIPT)
search = module_from_spec(SPEC)
assert SPEC.loader is not None
SPEC.loader.exec_module(search)


class GitHubSqlSearchTests(unittest.TestCase):
    def test_skill_prefers_the_pinned_local_mariadb_helper(self):
        content = SKILL.read_text()
        self.assertIn("mysql-test-server", content)
        self.assertIn("mariadbd --version", content)
        self.assertIn("Ver 10\\.11\\.7-MariaDB", content)
        self.assertLess(content.index("Prefer that\nhelper"), content.index("Use Docker or\nPodman only"))

    def test_local_server_helper_cleans_up_after_run_commands(self):
        content = LOCAL_SERVER_HELPER.read_text()
        self.assertIn("trap cleanup EXIT INT TERM", content)
        self.assertIn('if [[ "$RUN_COMMAND" == true ]]', content)

    def test_development_image_uses_a_checksum_pinned_mariadb_binary(self):
        content = DOCKERFILE.read_text()
        self.assertIn("ARG MARIADB_VERSION=10.11.7", content)
        self.assertIn("MARIADB_BINARY_SHA256_AMD64", content)
        self.assertIn("dlm.mariadb.com", content)
        self.assertIn("sha256sum -c", content)

    def test_token_requires_explicit_environment(self):
        with patch.dict(os.environ, {}, clear=True), self.assertRaises(search.GitHubError):
            search.token()

    def test_search_writes_pinned_attributed_sources(self):
        responses = iter([
            {"items": [{"path": "queries/report.sql", "repository": {"full_name": "owner/repo", "default_branch": "main"}}]},
            {"sha": "a" * 40},
            {"encoding": "base64", "size": 9, "content": "U0VMRUNUIDE="},
        ])
        with tempfile.TemporaryDirectory() as directory, patch.dict(os.environ, {"GH_TOKEN": "secret"}, clear=True), patch.object(search, "api", side_effect=lambda path: next(responses)):
            output = Path(directory) / "result.json"
            self.assertEqual(search.main(["search", "--query", "mysql extension:sql", "--output", str(output)]), 0)
            document = json.loads(output.read_text())
        record = document["results"][0]
        self.assertEqual(record["repository"], "owner/repo")
        self.assertEqual(record["commit"], "a" * 40)
        self.assertNotIn("license", record)
        self.assertEqual(record["source_dialect"], "mysql-mariadb")
        self.assertEqual(record["content"], "SELECT 1")
        self.assertNotIn("secret", output.read_text() if output.exists() else "")

    def test_rate_limit_error_never_echoes_authorization(self):
        error = HTTPError("https://api.github.com/search/code", 429, "limited", {"Retry-After": "3"}, None)
        self.addCleanup(error.close)
        with patch.dict(os.environ, {"GH_TOKEN": "secret"}, clear=True), patch.object(search, "urlopen", side_effect=error), self.assertRaisesRegex(search.GitHubError, "3 seconds") as caught:
            search.api("/search/code?q=mysql")
        self.assertNotIn("secret", str(caught.exception))

    def test_source_dialect_can_be_inferred_or_explicit(self):
        self.assertEqual(search.source_dialect("postgresql extension:sql", "auto"), "postgresql")
        self.assertEqual(search.source_dialect("sqlite extension:sql", "auto"), "sqlite")
        self.assertEqual(search.source_dialect("mysql extension:sql", "auto"), "mysql-mariadb")
        self.assertEqual(search.source_dialect("extension:sql", "postgresql"), "postgresql")


if __name__ == "__main__":
    unittest.main()
