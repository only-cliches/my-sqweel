#!/usr/bin/env python3
"""Fetch public GitHub code-search results at immutable commits without printing tokens.

Usage:
  github_sql_search.py search --query 'mysql extension:sql' --output results.json
  github_sql_search.py inventory --cases tests/query_cases --output features.json
"""
from __future__ import annotations

import argparse
import base64
import hashlib
import json
import os
from pathlib import Path
import re
import sys
import time
from urllib.error import HTTPError
from urllib.parse import quote, urlencode
from urllib.request import Request, urlopen


API = "https://api.github.com"
USER_AGENT = "MySqweel-query-coverage-skill"


class GitHubError(RuntimeError):
    pass


class GitHubRateLimitError(GitHubError):
    pass


class GitHubTransientError(GitHubError):
    pass


def token() -> str:
    value = os.environ.get("GH_TOKEN") or os.environ.get("GITHUB_TOKEN")
    if not value:
        raise GitHubError("GH_TOKEN or GITHUB_TOKEN is required for GitHub code search")
    return value


def _retry_delay(error: HTTPError, attempt: int) -> float:
    retry = error.headers.get("Retry-After")
    if retry:
        try:
            return max(0.0, float(retry))
        except ValueError:
            pass
    reset = error.headers.get("X-RateLimit-Reset")
    if reset:
        try:
            return max(0.0, float(reset) - time.time())
        except ValueError:
            pass
    return float(min(2 ** attempt, 30))


def api(path: str, *, max_retries: int = 3, max_wait_seconds: float = 30.0):
    request = Request(
        API + path,
        headers={
            "Accept": "application/vnd.github+json",
            "Authorization": "Bearer " + token(),
            "User-Agent": USER_AGENT,
            "X-GitHub-Api-Version": "2022-11-28",
        },
    )
    for attempt in range(max_retries + 1):
        try:
            with urlopen(request, timeout=30) as response:
                return json.load(response)
        except HTTPError as error:
            if error.code in {403, 429}:
                delay = _retry_delay(error, attempt)
                message = f"GitHub search is rate limited; retry after {max(1, int(delay))} seconds"
                if attempt >= max_retries or delay > max_wait_seconds:
                    raise GitHubRateLimitError(message) from error
                time.sleep(delay)
                continue
            if error.code in {500, 502, 503, 504}:
                if attempt >= max_retries:
                    raise GitHubTransientError(f"GitHub API returned HTTP {error.code}") from error
                time.sleep(min(2 ** attempt, max_wait_seconds))
                continue
            raise GitHubError(f"GitHub API returned HTTP {error.code}") from error
    raise AssertionError("unreachable")


def content(repo: str, path: str, commit: str, options: argparse.Namespace) -> str:
    document = api(
        f"/repos/{repo}/contents/{quote(path, safe='/')}?ref={commit}",
        max_retries=options.max_retries,
        max_wait_seconds=options.max_wait_seconds,
    )
    if document.get("encoding") != "base64" or not isinstance(document.get("content"), str):
        raise GitHubError(f"{repo}/{path}: source content is not base64 text")
    if document.get("size", 0) > 1_000_000:
        raise GitHubError(f"{repo}/{path}: source is larger than 1 MiB")
    return base64.b64decode("".join(document["content"].split()), validate=True).decode("utf-8", errors="replace")


def immutable_commit(repo: str, branch: str, options: argparse.Namespace) -> str:
    return api(
        f"/repos/{repo}/commits/{quote(branch, safe='')}",
        max_retries=options.max_retries,
        max_wait_seconds=options.max_wait_seconds,
    )["sha"]


def source_dialect(query: str, override: str) -> str:
    if override != "auto":
        return override
    query = query.casefold()
    if "postgres" in query or "cockroach" in query:
        return "postgresql"
    if "sqlite" in query:
        return "sqlite"
    return "mysql-mariadb"


def _patterns(values: list[str]) -> list[re.Pattern[str]]:
    try:
        return [re.compile(value, re.IGNORECASE) for value in values]
    except re.error as error:
        raise GitHubError(f"invalid filter regex: {error}") from error


def _path_allowed(path: str, suffixes: list[str]) -> bool:
    return not suffixes or any(path.endswith(suffix) for suffix in suffixes)


SQL_STATEMENT_RE = re.compile(r"(?im)^\s*(?:SELECT|INSERT|UPDATE|DELETE|WITH)\b")


def _query_count(source: str) -> int:
    return len(SQL_STATEMENT_RE.findall(source))


def search(args: argparse.Namespace) -> int:
    if not 1 <= args.limit <= 100:
        raise GitHubError("--limit must be between 1 and 100")
    if args.page < 1:
        raise GitHubError("--page must be at least 1")
    if not 1 <= args.pages <= 10:
        raise GitHubError("--pages must be between 1 and 10")
    if args.min_query_count < 0:
        raise GitHubError("--min-query-count must not be negative")
    if args.max_retries < 0:
        raise GitHubError("--max-retries must not be negative")
    if args.max_wait_seconds < 0:
        raise GitHubError("--max-wait-seconds must not be negative")
    include_patterns = _patterns(args.include_pattern)
    exclude_patterns = _patterns(args.exclude_pattern)
    records = []
    seen = set()
    seen_source_sha = set()
    for page in range(args.page, args.page + args.pages):
        response = api(
            "/search/code?" + urlencode({"q": args.query, "per_page": args.limit, "page": page}),
            max_retries=args.max_retries,
            max_wait_seconds=args.max_wait_seconds,
        )
        for item in response.get("items", []):
            repo = item["repository"]["full_name"]
            path = item["path"]
            if (repo, path) in seen or not _path_allowed(path, args.path_suffix):
                continue
            seen.add((repo, path))
            try:
                branch = item["repository"].get("default_branch") or api(
                    f"/repos/{repo}",
                    max_retries=args.max_retries,
                    max_wait_seconds=args.max_wait_seconds,
                )["default_branch"]
                commit = immutable_commit(repo, branch, args)
                source = content(repo, path, commit, args)
            except (GitHubRateLimitError, GitHubTransientError):
                raise
            except GitHubError as error:
                records.append({"status": "skipped", "repository": repo, "path": path, "reason": str(error)})
                continue
            if args.min_query_count and _query_count(source) < args.min_query_count:
                records.append({"status": "filtered", "repository": repo, "path": path, "reason": "min-query-count"})
                continue
            if any(not pattern.search(source) for pattern in include_patterns):
                records.append({"status": "filtered", "repository": repo, "path": path, "reason": "include-pattern"})
                continue
            if any(pattern.search(source) for pattern in exclude_patterns):
                records.append({"status": "filtered", "repository": repo, "path": path, "reason": "exclude-pattern"})
                continue
            source_sha256 = hashlib.sha256(source.encode()).hexdigest()
            if args.dedupe_source_sha and source_sha256 in seen_source_sha:
                records.append({"status": "filtered", "repository": repo, "path": path, "reason": "duplicate-source-sha256"})
                continue
            seen_source_sha.add(source_sha256)
            records.append({
                "status": "fetched",
                "repository": repo,
                "commit": commit,
                "path": path,
                "url": f"https://github.com/{repo}/blob/{commit}/{quote(path)}",
                "source_sha256": source_sha256,
                "source_dialect": source_dialect(args.query, args.source_dialect),
                "content": source,
            })
    document = {
        "schema": "my-sqweel.github-sql-search.v1",
        "query": args.query,
        "page": args.page,
        "pages": args.pages,
        "limit": args.limit,
        "searched_at_epoch": time.time(),
        "results": records,
    }
    output = args.output.resolve()
    output.parent.mkdir(parents=True, exist_ok=True)
    temporary = output.with_suffix(output.suffix + ".tmp")
    temporary.write_text(json.dumps(document, indent=2) + "\n")
    temporary.replace(output)
    print(f"Wrote {len(records)} GitHub source records to {output}")
    return 0


def inventory(args: argparse.Namespace) -> int:
    root = args.cases.resolve()
    if not root.is_dir():
        raise GitHubError(f"--cases is not a directory: {root}")
    by_feature: dict[str, list[str]] = {}
    case_count = 0
    for path in sorted(root.rglob("*.json")):
        try:
            case = json.loads(path.read_text())
        except (OSError, json.JSONDecodeError) as error:
            raise GitHubError(f"{path}: cannot read query case: {error}") from error
        case_id = case.get("id", path.stem)
        features = case.get("features", [])
        if not isinstance(features, list):
            continue
        case_count += 1
        for feature in features:
            if isinstance(feature, str):
                by_feature.setdefault(feature, []).append(case_id)
    document = {
        "schema": "my-sqweel.query-feature-inventory.v1",
        "cases_root": str(root),
        "case_count": case_count,
        "features": {feature: {"case_count": len(case_ids), "cases": case_ids} for feature, case_ids in sorted(by_feature.items())},
    }
    output = args.output.resolve()
    output.parent.mkdir(parents=True, exist_ok=True)
    temporary = output.with_suffix(output.suffix + ".tmp")
    temporary.write_text(json.dumps(document, indent=2) + "\n")
    temporary.replace(output)
    print(f"Wrote feature inventory for {case_count} query cases to {output}")
    return 0


def main(argv=None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="command", required=True)
    command = sub.add_parser("search", help="search public GitHub code and fetch immutable source files")
    command.add_argument("--query", required=True)
    command.add_argument("--limit", type=int, default=10)
    command.add_argument("--page", type=int, default=1)
    command.add_argument("--pages", type=int, default=1)
    command.add_argument("--min-query-count", type=int, default=0)
    command.add_argument("--path-suffix", action="append", default=[])
    command.add_argument("--include-pattern", action="append", default=[])
    command.add_argument("--exclude-pattern", action="append", default=[])
    command.add_argument("--dedupe-source-sha", action="store_true")
    command.add_argument("--max-retries", type=int, default=3)
    command.add_argument("--max-wait-seconds", type=float, default=30.0)
    command.add_argument("--source-dialect", choices=("auto", "mysql-mariadb", "postgresql", "sqlite"), default="auto")
    command.add_argument("--output", type=Path, required=True)
    inventory_command = sub.add_parser("inventory", help="index feature labels from local query cases")
    inventory_command.add_argument("--cases", type=Path, required=True)
    inventory_command.add_argument("--output", type=Path, required=True)
    args = parser.parse_args(argv)
    if args.command == "search":
        return search(args)
    if args.command == "inventory":
        return inventory(args)
    raise AssertionError("unreachable")


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (GitHubError, OSError, ValueError) as error:
        print(f"github SQL search: {error}", file=sys.stderr)
        raise SystemExit(2)
