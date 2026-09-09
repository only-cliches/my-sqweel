#!/usr/bin/env python3
"""Run a pinned, allowlisted MTR surface against a baseline and MySqweel.

The upstream test tree and its mysqltest-compatible binary are intentionally supplied by
the caller.  They are not vendored in this repository because the upstream
test sources are GPL-licensed.  The runner uses MTR's --extern mode, so the
same upstream test and expected result are executed against the baseline and
MySqweel independently.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import shlex
import shutil
import socket
import subprocess
import sys
import tempfile
import time
from dataclasses import asdict, dataclass
from pathlib import Path
from urllib.parse import unquote, urlsplit


DEFAULT_ALLOWLIST = Path("tests/mariadb-mtr-allowlist.txt")
TEST_NAME = re.compile(r"^[A-Za-z0-9_.-]+(?:/[A-Za-z0-9_.-]+)?$")


@dataclass(frozen=True)
class Server:
    name: str
    url: str


@dataclass(frozen=True)
class TestCase:
    name: str
    feature: str
    test_sha256: str
    result_sha256: str
    source: str


@dataclass
class Invocation:
    test: str
    server: str
    status: str
    returncode: int | None
    command: list[str]
    stdout: str
    stderr: str
    artifact_dir: str


def parse_manifest(path: Path) -> list[TestCase]:
    cases: list[TestCase] = []
    seen: set[str] = set()
    for line_number, raw_line in enumerate(
        path.read_text(encoding="utf-8", errors="replace").splitlines(), 1
    ):
        line = raw_line.split("#", 1)[0].strip()
        if not line:
            continue
        fields = line.split()
        if len(fields) != 4:
            raise ValueError(
                f"{path}:{line_number}: expected test, feature, test SHA-256, "
                "and result SHA-256"
            )
        name = fields[0]
        if not TEST_NAME.fullmatch(name):
            raise ValueError(f"{path}:{line_number}: invalid MTR test name {name!r}")
        if name in seen:
            raise ValueError(f"{path}:{line_number}: duplicate MTR test {name!r}")
        feature, test_sha256, result_sha256 = fields[1:]
        if not TEST_NAME.fullmatch(feature):
            raise ValueError(f"{path}:{line_number}: invalid feature name {feature!r}")
        for label, digest in (("test", test_sha256), ("result", result_sha256)):
            if not re.fullmatch(r"[0-9a-f]{64}", digest):
                raise ValueError(f"{path}:{line_number}: invalid {label} SHA-256 {digest!r}")
        seen.add(name)
        cases.append(
            TestCase(
                name=name,
                feature=feature,
                test_sha256=test_sha256,
                result_sha256=result_sha256,
                source=str(path),
            )
        )
    if not cases:
        raise ValueError(f"{path}: allowlist is empty")
    return cases


def mysql_test_file(suite_root: Path, name: str, layout: str = "mariadb") -> Path:
    mysql_test = suite_root / "mysql-test"
    if "/" not in name:
        main_directory = "main" if layout == "mariadb" else "t"
        return mysql_test / main_directory / f"{name}.test"
    suite, test = name.split("/", 1)
    return mysql_test / "suite" / suite / "t" / f"{test}.test"


def mysql_result_file(suite_root: Path, name: str, layout: str = "mariadb") -> Path:
    mysql_test = suite_root / "mysql-test"
    if "/" not in name:
        main_directory = "main" if layout == "mariadb" else "r"
        return mysql_test / main_directory / f"{name}.result"
    suite, test = name.split("/", 1)
    return mysql_test / "suite" / suite / "r" / f"{test}.result"


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def sql_statement_count(text: str) -> int:
    """Count semicolon-terminated SQL statements in an MTR test file.

    mysqltest directives and comments are ignored. Semicolons inside quoted
    strings, identifiers, and block comments do not count. Discovery rejects
    tests that change the delimiter, so this deliberately models the normal
    MTR SQL surface rather than trying to parse stored-program bodies.
    """
    filtered_lines = []
    for line in text.splitlines():
        stripped = line.lstrip()
        if stripped.startswith("#") or stripped.startswith("--"):
            continue
        filtered_lines.append(line)
    source = "\n".join(filtered_lines)
    count = 0
    quote: str | None = None
    escaped = False
    block_comment = False
    index = 0
    while index < len(source):
        character = source[index]
        following = source[index + 1] if index + 1 < len(source) else ""
        if block_comment:
            if character == "*" and following == "/":
                block_comment = False
                index += 2
                continue
            index += 1
            continue
        if quote:
            if escaped:
                escaped = False
            elif character == "\\":
                escaped = True
            elif character == quote:
                if following == quote:
                    index += 2
                    continue
                quote = None
            index += 1
            continue
        if character == "/" and following == "*":
            block_comment = True
            index += 2
            continue
        if character in ("'", '"', "`"):
            quote = character
        elif character == ";":
            count += 1
        index += 1
    return count


def validate_cases(suite_root: Path, cases: list[TestCase], layout: str = "mariadb") -> None:
    missing = [
        case.name
        for case in cases
        if not mysql_test_file(suite_root, case.name, layout).is_file()
        or not mysql_result_file(suite_root, case.name, layout).is_file()
    ]
    if missing:
        joined = ", ".join(missing)
        raise ValueError(f"allowlisted MTR test or result files are missing from {suite_root}: {joined}")
    for case in cases:
        test_file = mysql_test_file(suite_root, case.name, layout)
        result_file = mysql_result_file(suite_root, case.name, layout)
        actual_test = sha256_file(test_file)
        actual_result = sha256_file(result_file)
        if actual_test != case.test_sha256:
            raise ValueError(
                f"upstream test hash mismatch for {case.name}: "
                f"expected {case.test_sha256}, got {actual_test}"
            )
        if actual_result != case.result_sha256:
            raise ValueError(
                f"upstream result hash mismatch for {case.name}: "
                f"expected {case.result_sha256}, got {actual_result}"
            )


def parse_server_url(url: str) -> dict[str, str]:
    parsed = urlsplit(url)
    if parsed.scheme != "mysql" or not parsed.hostname:
        raise ValueError(f"expected a mysql:// URL, got {url!r}")
    return {
        "host": parsed.hostname,
        "port": str(parsed.port or 3306),
        "user": unquote(parsed.username or "root"),
        "password": unquote(parsed.password or ""),
        "database": parsed.path.lstrip("/") or "test",
    }


def validate_distinct_servers(baseline_url: str, mysqweel_url: str | None) -> None:
    """Reject a comparison that would run both MTR sides against one server."""
    if not mysqweel_url:
        return
    baseline = parse_server_url(baseline_url)
    mysqweel = parse_server_url(mysqweel_url)
    if (baseline["host"], baseline["port"]) == (mysqweel["host"], mysqweel["port"]):
        raise ValueError(
            "--baseline-url and --mysqweel-url point to the same host and port; "
            "use --mysqweel-bin or a separately running MySqweel server"
        )


def validate_mtr_runtime(
    client_bindir: Path,
    mysqltest_path: Path,
    safe_process_path: Path | None = None,
) -> None:
    if not mysqltest_path.is_file():
        raise FileNotFoundError(f"mysqltest not found: {mysqltest_path}")
    safe_process = safe_process_path or client_bindir / "mysqltest_safe_process"
    if not safe_process.is_file():
        raise FileNotFoundError(f"MTR safe-process helper not found: {safe_process}")
    with tempfile.TemporaryDirectory() as directory:
        marker = Path(directory) / "safe-process-canary"
        try:
            canary = subprocess.run(
                [
                    str(safe_process),
                    "--",
                    sys.executable,
                    "-c",
                    "from pathlib import Path; Path(__import__('sys').argv[1]).write_text('ok')",
                    str(marker),
                ],
                capture_output=True,
                text=True,
                errors="replace",
                timeout=10,
                check=False,
            )
        except subprocess.TimeoutExpired as error:
            raise RuntimeError("MTR safe-process execution canary timed out") from error
        if canary.returncode != 0 or not marker.is_file() or marker.read_text() != "ok":
            raise RuntimeError(
                "MTR safe-process did not execute the canary child process; "
                f"exit code {canary.returncode}"
            )


def wait_for_port(host: str, port: int, process: subprocess.Popen[str], timeout: float = 30) -> None:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if process.poll() is not None:
            raise RuntimeError(f"MySqweel exited before opening {host}:{port}")
        try:
            with socket.create_connection((host, port), timeout=0.25):
                return
        except OSError:
            time.sleep(0.1)
    raise RuntimeError(f"timed out waiting for MySqweel at {host}:{port}")


def free_port(host: str) -> int:
    with socket.socket() as sock:
        sock.bind((host, 0))
        return int(sock.getsockname()[1])


def ensure_mtr_database(server: Server, client_bindir: Path, mariadb: bool = False) -> None:
    connection = parse_server_url(server.url)
    mysql = client_bindir / "mysql"
    setup_sql = (
        "CREATE DATABASE IF NOT EXISTS test; "
        "CREATE DATABASE IF NOT EXISTS mtr; "
        "SET GLOBAL log_bin_trust_function_creators = 1; "
    )
    if mariadb:
        # The packaged MariaDB MTR suite assumes its historical test-server
        # defaults when it is run against an external server.  Reproduce
        # those defaults explicitly so the baseline is independent of the
        # container image's application-oriented configuration.
        setup_sql += (
            "SET GLOBAL default_storage_engine = 'MyISAM'; "
            "SET GLOBAL character_set_server = 'latin1'; "
            "SET GLOBAL collation_server = 'latin1_swedish_ci'; "
        )
    if not mariadb:
        setup_sql += (
            "DROP PROCEDURE IF EXISTS mtr.add_suppression; "
            "CREATE PROCEDURE mtr.add_suppression(IN message TEXT) BEGIN END"
        )
    command = [
        str(mysql),
        "--no-defaults",
        f"--user={connection['user']}",
        f"--password={connection['password']}",
        f"--host={connection['host']}",
        f"--port={connection['port']}",
        "--protocol=TCP",
        f"--execute={setup_sql}",
    ]
    completed = subprocess.run(
        command,
        capture_output=True,
        text=True,
        errors="replace",
        timeout=30,
        check=False,
        env=os.environ.copy(),
    )
    if completed.returncode:
        raise RuntimeError(
            "could not provision the MTR helper database on the baseline: "
            f"{completed.stderr.strip()}"
        )


def reset_test_database(server: Server, client_bindir: Path, mariadb: bool = False) -> None:
    connection = parse_server_url(server.url)
    base_command = [
        str(client_bindir / "mysql"),
        "--no-defaults",
        f"--user={connection['user']}",
        f"--password={connection['password']}",
        f"--host={connection['host']}",
        f"--port={connection['port']}",
        "--protocol=TCP",
    ]

    def query(sql: str, tabular: bool = False) -> subprocess.CompletedProcess[str]:
        command = [*base_command]
        if tabular:
            command.extend(("--batch", "--skip-column-names"))
        command.append(f"--execute={sql}")
        return subprocess.run(
            command,
            capture_output=True,
            text=True,
            errors="replace",
            timeout=30,
            check=False,
        )

    databases = query("SHOW DATABASES", tabular=True)
    users = None
    if not mariadb:
        users = query(
            "SELECT CONCAT(user, CHAR(9), host) FROM mysql.user "
            "WHERE user NOT IN ('root', 'mysql.infoschema', 'mysql.session', 'mysql.sys')",
            tabular=True,
        )
    if databases.returncode or (users is not None and users.returncode):
        stderr = (databases.stderr or (users.stderr if users else "")).strip()
        raise RuntimeError(f"could not inspect baseline state before an MTR case: {stderr}")

    statements = []
    protected_databases = {"information_schema", "mysql", "performance_schema", "sys", "mtr", "test"}
    if server.name == "mysqweel":
        # The embedded catalog keeps its default database for the server's lifetime.
        # MTR cases use the separate, recreated `test` database.
        protected_databases.add("app")
    for database in databases.stdout.splitlines():
        database = database.strip()
        if database and database not in protected_databases:
            escaped = database.replace("`", "``")
            statements.append(f"DROP DATABASE IF EXISTS `{escaped}`")
    if users is not None:
        for user_host in users.stdout.splitlines():
            user, _, host = user_host.partition("\t")
            if user and host:
                escaped_user = user.replace("'", "''")
                escaped_host = host.replace("'", "''")
                statements.append(f"DROP USER IF EXISTS '{escaped_user}'@'{escaped_host}'")
    if server.name != "mysqweel":
        statements.append("SET GLOBAL log_bin_trust_function_creators = 1")
    statements.extend(
        (
            "DROP DATABASE IF EXISTS test",
            "CREATE DATABASE test",
        )
    )
    completed = query("; ".join(statements))
    if completed.returncode:
        raise RuntimeError(
            "could not reset the baseline MTR test database: " f"{completed.stderr.strip()}"
        )


def mtr_command(
    suite_root: Path,
    mysqltest_runner: Path,
    client_bindir: Path,
    server: Server,
    case: TestCase,
    vardir: Path,
) -> list[str]:
    connection = parse_server_url(server.url)
    suite, test = (case.name.split("/", 1) if "/" in case.name else ("main", case.name))
    command = [
        "perl",
        str(mysqltest_runner),
        f"--vardir={vardir}",
        f"--client-bindir={client_bindir}",
        "--retry=0",
        "--skip-rpl",
        f"--suite={suite}",
    ]
    for key in ("host", "port", "user", "password", "database"):
        command.append(f"--extern={key}={connection[key]}")
    command.append(test)
    return command


def mtr_case_timezone(
    suite_root: Path,
    case: TestCase,
    layout: str = "mariadb",
) -> str:
    """Return the SQL timezone equivalent of an upstream MTR server option.

    MTR normally applies ``--timezone`` by restarting the server with a TZ
    environment variable. In ``--extern`` mode it cannot restart the server,
    so fixed POSIX GMT offsets must be applied through SQL instead. POSIX GMT
    signs are reversed: ``GMT+10`` means UTC-10.
    """
    test_file = mysql_test_file(suite_root, case.name, layout)
    option_file = test_file.with_name(f"{test_file.stem}-master.opt")
    if not option_file.is_file():
        return "+00:00"

    tokens = shlex.split(option_file.read_text(encoding="utf-8", errors="replace"))
    timezone: str | None = None
    for index, token in enumerate(tokens):
        if token.startswith("--timezone="):
            timezone = token.split("=", 1)[1]
            break
        if token == "--timezone" and index + 1 < len(tokens):
            timezone = tokens[index + 1]
            break
    if timezone is None:
        return "+00:00"

    if timezone in ("UTC", "GMT", "GMT0"):
        return "+00:00"
    if re.fullmatch(r"[+-]\d{1,2}:\d{2}", timezone):
        sign = timezone[0]
        hours, minutes = timezone[1:].split(":", 1)
    else:
        match = re.fullmatch(r"GMT([+-])(\d{1,2})(?::(\d{2}))?", timezone)
        if not match:
            raise RuntimeError(
                f"{case.name}: external-server MTR cannot reproduce timezone "
                f"option {timezone!r}; use a fixed GMT offset"
            )
        sign = "-" if match.group(1) == "+" else "+"
        hours = match.group(2)
        minutes = match.group(3) or "00"

    hour = int(hours)
    minute = int(minutes)
    if hour > 13 or minute > 59:
        raise RuntimeError(f"{case.name}: invalid MTR timezone offset {timezone!r}")
    return f"{sign}{hour:02d}:{minute:02d}"


def configure_case_timezone(
    server: Server,
    client_bindir: Path,
    suite_root: Path,
    case: TestCase,
    layout: str = "mariadb",
) -> str:
    timezone = mtr_case_timezone(suite_root, case, layout)
    connection = parse_server_url(server.url)
    # MySqweel has a fixed UTC session default and no mutable global settings.
    # Verify that default rather than silently ignoring a requested timezone.
    verify_timezone = server.name == "mysqweel"
    command = [
        str(client_bindir / "mysql"),
        "--no-defaults",
        f"--user={connection['user']}",
        f"--password={connection['password']}",
        f"--host={connection['host']}",
        f"--port={connection['port']}",
        "--protocol=TCP",
        *(["--batch", "--skip-column-names"] if verify_timezone else []),
        (
            "--execute=SELECT @@time_zone"
            if verify_timezone
            else f"--execute=SET GLOBAL time_zone = '{timezone}'"
        ),
    ]
    completed = subprocess.run(
        command,
        capture_output=True,
        text=True,
        errors="replace",
        timeout=30,
        check=False,
    )
    if completed.returncode:
        raise RuntimeError(
            f"could not configure {server.name} timezone for {case.name}: "
            f"{completed.stderr.strip()}"
        )
    if verify_timezone and completed.stdout.strip() != timezone:
        raise RuntimeError(
            f"{case.name}: MySqweel session timezone {completed.stdout.strip()!r} "
            f"does not match required {timezone!r}"
        )
    return timezone


def prepare_external_mariadb_runner(runner: Path, destination: Path) -> Path:
    """Keep the feature probe in --extern's database without editing upstream files.

    SHOW VARIABLES is database-independent. MariaDB 10.11's probe unnecessarily
    selects mysql, which prevents external servers without that schema from
    reaching any test. Apply the same narrowly checked adaptation to both engines.
    """
    original = runner.read_bytes()
    probe = b'use mysql; SHOW VARIABLES'
    if original.count(probe) != 1:
        raise RuntimeError("unrecognized MariaDB MTR external feature probe; review runner adaptation")
    adapted = original.replace(probe, b'SHOW VARIABLES', 1)
    destination.write_bytes(adapted)
    destination.with_suffix(".json").write_text(json.dumps({
        "source": str(runner),
        "source_sha256": hashlib.sha256(original).hexdigest(),
        "adapted_sha256": hashlib.sha256(adapted).hexdigest(),
        "adaptation": "Run SHOW VARIABLES in the configured external database",
    }, indent=2) + "\n")
    return destination


def run_case(
    suite_root: Path,
    mysqltest_runner: Path,
    client_bindir: Path,
    server: Server,
    case: TestCase,
    artifact_dir: Path,
    mysqltest_bin: Path,
    layout: str = "mariadb",
    timeout: int = 300,
) -> Invocation:
    case_artifact = artifact_dir / server.name / case.name.replace("/", "_")
    case_artifact.mkdir(parents=True, exist_ok=True)
    if layout == "mariadb":
        mysqltest_runner = prepare_external_mariadb_runner(
            mysqltest_runner, case_artifact / "mariadb-test-run.pl"
        )
    vardir = Path(tempfile.mkdtemp(prefix="mysqweel-mtr-", dir="/tmp"))
    (vardir / "log").mkdir()
    # MTR copies its roughly 500 MiB std_data directory into every vardir by
    # default.  The allowlist runs against an external server, so the data is
    # read-only input; linking it keeps the report bounded and avoids making a
    # separate copy for every case.
    std_data = suite_root / "mysql-test" / "std_data"
    vardir_std_data = vardir / "std_data"
    if std_data.is_dir() and not vardir_std_data.exists():
        vardir_std_data.symlink_to(std_data.resolve(), target_is_directory=True)
    command = mtr_command(suite_root, mysqltest_runner, client_bindir, server, case, vardir)

    def preserve_vardir() -> None:
        try:
            shutil.copytree(
                vardir,
                case_artifact / "vardir",
                dirs_exist_ok=True,
                symlinks=True,
                ignore=shutil.ignore_patterns("std_data"),
            )
        finally:
            shutil.rmtree(vardir, ignore_errors=True)

    environment = os.environ.copy()
    environment["MYSQL_TEST"] = str(mysqltest_bin)
    try:
        completed = subprocess.run(
            command,
            cwd=suite_root / "mysql-test",
            env=environment,
            capture_output=True,
            text=True,
            errors="replace",
            timeout=timeout,
            check=False,
        )
        stdout = completed.stdout
        stderr = completed.stderr
        status = "pass" if completed.returncode == 0 else "fail"
        suite, test = (
            case.name.split("/", 1) if "/" in case.name else ("main", case.name)
        )
        qualified_test = re.escape(f"{suite}.{test}")
        if layout == "mariadb":
            pass_line = re.compile(
                rf"^\s*(?:\[\s*\d+%\]\s+)?{qualified_test}\s+\[\s*pass\s*\]",
                re.MULTILINE,
            )
        else:
            pass_line = re.compile(
                rf"^\[\s*\d+%\]\s+{qualified_test}\s+\[\s*pass\s*\]",
                re.MULTILINE,
            )
        if completed.returncode == 0 and (
            not pass_line.search(stdout) or "Completed: All" not in stdout
        ):
            status = "infrastructure"
            stderr = (
                f"{stderr}\nMTR execution canary failed: the runner did not report "
                f"a completed pass for {case.name}"
            ).strip()
        preserve_vardir()
        (case_artifact / "stdout.log").write_text(stdout)
        (case_artifact / "stderr.log").write_text(stderr)
        return Invocation(
            test=case.name,
            server=server.name,
            status=status,
            returncode=completed.returncode,
            command=command,
            stdout=stdout,
            stderr=stderr,
            artifact_dir=str(case_artifact),
        )
    except subprocess.TimeoutExpired as error:
        stdout = error.stdout or ""
        stderr = error.stderr or ""
        if isinstance(stdout, bytes):
            stdout = stdout.decode(errors="replace")
        if isinstance(stderr, bytes):
            stderr = stderr.decode(errors="replace")
        preserve_vardir()
        (case_artifact / "stdout.log").write_text(stdout)
        (case_artifact / "stderr.log").write_text(stderr)
        return Invocation(
            test=case.name,
            server=server.name,
            status="infrastructure",
            returncode=None,
            command=command,
            stdout=stdout,
            stderr=f"MTR test timed out after {timeout} seconds\n{stderr}",
            artifact_dir=str(case_artifact),
        )


def start_mysqweel(binary: Path, report_dir: Path) -> tuple[Server, subprocess.Popen[str]]:
    host = "127.0.0.1"
    port = free_port(host)
    report_dir.mkdir(parents=True, exist_ok=True)
    log = report_dir / "mysqweel.log"
    stream = log.open("w")
    process = subprocess.Popen(
        [str(binary), "--bind", f"{host}:{port}", "--mysql-strict", "serve"],
        stdout=stream,
        stderr=subprocess.STDOUT,
        text=True,
        errors="replace",
    )
    try:
        wait_for_port(host, port, process)
    except Exception:
        process.terminate()
        process.wait(timeout=5)
        stream.close()
        raise
    return Server("mysqweel", f"mysql://root@{host}:{port}/test"), process


def render_markdown(report: dict) -> str:
    counts = report["counts"]
    baseline_label = report.get("baseline_label", "Baseline")
    baseline_version = report.get("baseline_version", "unknown")
    lines = [
        f"# {baseline_label} {baseline_version} upstream compatibility",
        "",
        f"- Source revision: `{report['source_revision']}`",
        f"- Target: `{report.get('target', 'both')}`",
        f"- Included tests: {counts['included']}",
        f"- Test-file SQL statements: {counts.get('statements', 0)}",
        f"- Statements in passing tests: {counts.get('passed_statements', 0)}",
        f"- Passed: {counts['passed']}",
        f"- Failed: {counts['failed']}",
        f"- Infrastructure failures: {counts['infrastructure']}",
        f"- Score: {report['score_percent']:.1f}%",
        f"- Required floor: {report.get('minimum_percent', 90.0):.1f}%",
        f"- Status: **{report['status']}**",
        "",
        (
            "A compatibility test is counted only when the unmodified, hash-pinned upstream "
            "MTR test passes for every server evaluated by this report."
        ),
        "",
        f"| Test | Feature | Statements | {baseline_label} baseline | MySqweel |",
        "| --- | --- | ---: | --- | --- |",
    ]
    for result in report["results"]:
        lines.append(
            f"| `{result['test']}` | `{result['feature']}` | {result.get('statements', 0)} | "
            f"{result.get('baseline', 'not-run')} | {result['mysqweel']} |"
        )

    if report.get("runner_adaptation"):
        lines.extend(["", f"Runner adaptation: {report['runner_adaptation']}"])

    failed_by_server: dict[str, dict] = {}
    for invocation in report.get("invocations", []):
        if invocation.get("status") == "pass":
            continue
        failed_by_server.setdefault(invocation["server"], invocation)
    if failed_by_server:
        lines.extend(["", "## Representative failure diagnostics", ""])
        for server, invocation in failed_by_server.items():
            streams = []
            for stream_name in ("stdout", "stderr"):
                output = (invocation.get(stream_name) or "").strip()
                if len(output) > 1_000:
                    output = "... output truncated ...\n" + output[-1_000:]
                if output:
                    streams.append(f"{stream_name}:\n{output}")
            output = "\n\n".join(streams)
            lines.extend(
                [
                    f"### {server}: `{invocation['test']}`",
                    "",
                    f"Return code: `{invocation.get('returncode')}`",
                    "",
                ]
            )
            lines.extend(f"    {line}" for line in (output or "No output captured.").splitlines())
    return "\n".join(lines) + "\n"


def run(args: argparse.Namespace) -> int:
    suite_root = args.suite_root.resolve()
    allowlist = args.allowlist.resolve()
    report_dir = args.report_dir.resolve()
    report_dir.mkdir(parents=True, exist_ok=True)
    cases = parse_manifest(allowlist)
    validate_cases(suite_root, cases, args.mtr_layout)

    runner_name = "mariadb-test-run.pl" if args.mtr_layout == "mariadb" else "mysql-test-run.pl"
    runner = (args.mtr_runner or suite_root / "mysql-test" / runner_name).resolve()
    if not runner.is_file():
        raise FileNotFoundError(f"MTR runner not found: {runner}")
    mysqltest = args.mysqltest_bin or shutil.which("mysqltest")
    if not mysqltest:
        raise FileNotFoundError("mysqltest-compatible binary not found; pass --mysqltest-bin")
    mysqltest_path = Path(mysqltest).resolve()
    client_bindir = args.client_bindir.resolve() if args.client_bindir else mysqltest_path.parent
    if not (client_bindir / "mysql").exists():
        raise FileNotFoundError(f"MariaDB client not found in {client_bindir}")
    safe_process = (
        args.safe_process_bin.resolve()
        if args.safe_process_bin
        else client_bindir / "mysqltest_safe_process"
    )
    if not safe_process.is_file() and args.mtr_layout == "mariadb":
        safe_process = suite_root / "mysql-test" / "lib" / "My" / "SafeProcess" / "my_safe_process"
    validate_mtr_runtime(client_bindir, mysqltest_path, safe_process)

    run_baseline = args.target in ("baseline", "both")
    run_mysqweel = args.target in ("mysqweel", "both")
    baseline_server: Server | None = None
    baseline_url = args.baseline_url or os.environ.get("MARIADB_COMPARE_URL")
    baseline_name = re.sub(r"[^a-z0-9]+", "-", args.baseline_label.lower()).strip("-") or "baseline"
    if run_baseline:
        if not baseline_url:
            raise ValueError("--baseline-url or MARIADB_COMPARE_URL is required for the baseline target")
        baseline_server = Server(baseline_name, baseline_url)
        ensure_mtr_database(
            baseline_server,
            client_bindir,
            mariadb=args.mtr_layout == "mariadb",
        )
    if baseline_url:
        validate_distinct_servers(baseline_url, args.mysqweel_url)
    binary = (args.mysqweel_bin or Path("target/debug/sqwl")).resolve()
    if run_mysqweel and not args.mysqweel_url and not binary.is_file():
        raise FileNotFoundError(f"MySqweel binary not found: {binary}")

    results: list[dict] = []
    invocations: list[Invocation] = []
    for case in cases:
        baseline_result: Invocation | None = None
        mysqweel_result: Invocation | None = None
        if baseline_server is not None:
            reset_test_database(
                baseline_server,
                client_bindir,
                mariadb=args.mtr_layout == "mariadb",
            )
            configure_case_timezone(
                baseline_server,
                client_bindir,
                suite_root,
                case,
                args.mtr_layout,
            )
            baseline_result = run_case(
                suite_root,
                runner,
                client_bindir,
                baseline_server,
                case,
                report_dir,
                mysqltest_path,
                args.mtr_layout,
                args.case_timeout,
            )
            invocations.append(baseline_result)

        mysqweel_process: subprocess.Popen[str] | None = None
        run_case_on_mysqweel = run_mysqweel and not (
            args.skip_mysqweel_after_baseline_failure
            and baseline_result is not None
            and baseline_result.status != "pass"
        )
        if run_case_on_mysqweel:
            if args.mysqweel_url:
                mysqweel_server = Server("mysqweel", args.mysqweel_url)
            else:
                mysqweel_server, mysqweel_process = start_mysqweel(
                    binary, report_dir / "mysqweel" / case.name.replace("/", "_")
                )
            try:
                reset_test_database(
                    mysqweel_server,
                    client_bindir,
                    mariadb=args.mtr_layout == "mariadb",
                )
                configure_case_timezone(
                    mysqweel_server,
                    client_bindir,
                    suite_root,
                    case,
                    args.mtr_layout,
                )
                mysqweel_result = run_case(
                    suite_root,
                    runner,
                    client_bindir,
                    mysqweel_server,
                    case,
                    report_dir,
                    mysqltest_path,
                    args.mtr_layout,
                    args.case_timeout,
                )
                invocations.append(mysqweel_result)
            finally:
                if mysqweel_process is not None:
                    mysqweel_process.terminate()
                    try:
                        mysqweel_process.wait(timeout=5)
                    except subprocess.TimeoutExpired:
                        mysqweel_process.kill()
                        mysqweel_process.wait()

        evaluated = [result for result in (baseline_result, mysqweel_result) if result is not None]
        results.append(
            {
                "test": case.name,
                "feature": case.feature,
                "test_sha256": case.test_sha256,
                "result_sha256": case.result_sha256,
                "statements": sql_statement_count(
                    mysql_test_file(suite_root, case.name, args.mtr_layout).read_text(
                        encoding="utf-8", errors="replace"
                    )
                ),
                "baseline": baseline_result.status if baseline_result else "not-run",
                "mysqweel": mysqweel_result.status if mysqweel_result else "not-run",
                "status": "pass"
                if evaluated and all(result.status == "pass" for result in evaluated)
                else "fail",
            }
        )

    baseline_failures = sum(
        result["baseline"] != "pass" for result in results if result["baseline"] != "not-run"
    )
    infrastructure = sum(
        invocation.status == "infrastructure" for invocation in invocations
    )
    passed = sum(result["status"] == "pass" for result in results)
    failed = len(results) - passed
    statement_count = sum(result["statements"] for result in results)
    passed_statement_count = sum(
        result["statements"] for result in results if result["status"] == "pass"
    )
    status = (
        "invalid"
        if baseline_failures or infrastructure
        else ("pass" if passed * 100.0 / len(results) >= args.minimum_percent else "fail")
    )
    report = {
        "schema": "my-sqweel.mtr-compatibility.v3",
        "status": status,
        "target": args.target,
        "baseline_label": args.baseline_label,
        "baseline_version": args.baseline_version,
        "source_revision": args.source_revision,
        "runner_adaptation": (
            "The external feature probe runs SHOW VARIABLES in the configured database. "
            "Both engines use the same adaptation; per-invocation mariadb-test-run.json "
            "records the original and adapted runner hashes. Upstream test and result files "
            "and the mysqltest binary are unchanged."
            if args.mtr_layout == "mariadb" else None
        ),
        "minimum_percent": args.minimum_percent,
        "counts": {
            "included": len(results),
            "passed": passed,
            "failed": failed,
            "infrastructure": infrastructure,
            "baseline_failures": baseline_failures,
            "statements": statement_count,
            "passed_statements": passed_statement_count,
        },
        "score_percent": passed * 100.0 / len(results),
        "results": results,
        "invocations": [asdict(invocation) for invocation in invocations],
    }
    (report_dir / "mtr-report.json").write_text(json.dumps(report, indent=2) + "\n")
    (report_dir / "mtr-report.md").write_text(render_markdown(report))
    print(render_markdown(report), end="")
    return 0 if status == "pass" else 1


def parser() -> argparse.ArgumentParser:
    result = argparse.ArgumentParser(description=__doc__)
    result.add_argument("--suite-root", type=Path, required=True)
    result.add_argument("--allowlist", type=Path, default=DEFAULT_ALLOWLIST)
    result.add_argument("--report-dir", type=Path, default=Path("artifacts/mariadb-mtr"))
    result.add_argument("--target", choices=("baseline", "mysqweel", "both"), default="both")
    result.add_argument("--baseline-url", "--mysql-url", dest="baseline_url")
    result.add_argument("--mysqweel-url")
    result.add_argument("--mysqweel-bin", type=Path)
    result.add_argument("--mtr-runner", type=Path)
    result.add_argument("--mysqltest-bin", type=Path)
    result.add_argument("--client-bindir", type=Path)
    result.add_argument("--safe-process-bin", type=Path)
    result.add_argument("--mtr-layout", choices=("mariadb",), default="mariadb")
    result.add_argument("--baseline-label", default="MariaDB")
    result.add_argument("--baseline-version", "--mysql-version", dest="baseline_version", default="10.11.7")
    result.add_argument("--source-revision", default="mariadb-10.11.7-2ubuntu2")
    result.add_argument("--minimum-percent", type=float, default=100.0)
    result.add_argument("--case-timeout", type=int, default=300)
    result.add_argument(
        "--skip-mysqweel-after-baseline-failure",
        action="store_true",
        help="with --target both, do not run a case on MySqweel unless its baseline passes",
    )
    return result


if __name__ == "__main__":
    try:
        raise SystemExit(run(parser().parse_args()))
    except (FileNotFoundError, RuntimeError, ValueError) as error:
        print(f"MTR compatibility runner: {error}", file=sys.stderr)
        raise SystemExit(2)
