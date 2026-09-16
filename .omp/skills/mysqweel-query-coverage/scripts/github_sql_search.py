#!/usr/bin/env python3
"""Fetch public GitHub code-search results at immutable commits without printing tokens.

Usage:
  github_sql_search.py search --query 'mysql extension:sql' --output results.json
"""
from __future__ import annotations

import argparse
import base64
import hashlib
import json
import os
from pathlib import Path
import sys
import time
from urllib.error import HTTPError
from urllib.parse import quote, urlencode
from urllib.request import Request, urlopen


API = "https://api.github.com"
USER_AGENT = "MySqweel-query-coverage-skill"


class GitHubError(RuntimeError):
    pass


def token() -> str:
    value = os.environ.get("GH_TOKEN") or os.environ.get("GITHUB_TOKEN")
    if not value:
        raise GitHubError("GH_TOKEN or GITHUB_TOKEN is required for GitHub code search")
    return value


def api(path: str):
    request = Request(
        API + path,
        headers={
            "Accept": "application/vnd.github+json",
            "Authorization": "Bearer " + token(),
            "User-Agent": USER_AGENT,
            "X-GitHub-Api-Version": "2022-11-28",
        },
    )
    try:
        with urlopen(request, timeout=30) as response:
            return json.load(response)
    except HTTPError as error:
        if error.code in {403, 429}:
            reset = error.headers.get("X-RateLimit-Reset")
            retry = error.headers.get("Retry-After")
            delay = float(retry) if retry else max(0, float(reset or 0) - time.time())
            raise GitHubError(f"GitHub search is rate limited; retry after {max(1, int(delay))} seconds") from error
        raise GitHubError(f"GitHub API returned HTTP {error.code}") from error


def content(repo: str, path: str, commit: str) -> str:
    document = api(f"/repos/{repo}/contents/{quote(path, safe='/')}?ref={commit}")
    if document.get("encoding") != "base64" or not isinstance(document.get("content"), str):
        raise GitHubError(f"{repo}/{path}: source content is not base64 text")
    if document.get("size", 0) > 1_000_000:
        raise GitHubError(f"{repo}/{path}: source is larger than 1 MiB")
    return base64.b64decode("".join(document["content"].split()), validate=True).decode("utf-8", errors="replace")


def immutable_commit(repo: str, branch: str) -> str:
    return api(f"/repos/{repo}/commits/{quote(branch, safe='')}")["sha"]


def source_dialect(query: str, override: str) -> str:
    if override != "auto":
        return override
    query = query.casefold()
    if "postgres" in query or "cockroach" in query:
        return "postgresql"
    if "sqlite" in query:
        return "sqlite"
    return "mysql-mariadb"


def search(args: argparse.Namespace) -> int:
    if not 1 <= args.limit <= 100:
        raise GitHubError("--limit must be between 1 and 100")
    response = api("/search/code?" + urlencode({"q": args.query, "per_page": args.limit, "page": args.page}))
    records = []
    seen = set()
    for item in response.get("items", []):
        repo = item["repository"]["full_name"]
        path = item["path"]
        if (repo, path) in seen:
            continue
        seen.add((repo, path))
        try:
            branch = item["repository"].get("default_branch") or api(f"/repos/{repo}")["default_branch"]
            commit = immutable_commit(repo, branch)
            source = content(repo, path, commit)
        except GitHubError as error:
            records.append({"status": "skipped", "repository": repo, "path": path, "reason": str(error)})
            continue
        records.append({
            "status": "fetched",
            "repository": repo,
            "commit": commit,
            "path": path,
            "url": f"https://github.com/{repo}/blob/{commit}/{quote(path)}",
            "source_sha256": hashlib.sha256(source.encode()).hexdigest(),
            "source_dialect": source_dialect(args.query, args.source_dialect),
            "content": source,
        })
    document = {
        "schema": "my-sqweel.github-sql-search.v1",
        "query": args.query,
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


def main(argv=None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="command", required=True)
    command = sub.add_parser("search", help="search public GitHub code and fetch immutable source files")
    command.add_argument("--query", required=True)
    command.add_argument("--limit", type=int, default=10)
    command.add_argument("--page", type=int, default=1)
    command.add_argument("--source-dialect", choices=("auto", "mysql-mariadb", "postgresql", "sqlite"), default="auto")
    command.add_argument("--output", type=Path, required=True)
    args = parser.parse_args(argv)
    if args.command == "search":
        return search(args)
    raise AssertionError("unreachable")


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (GitHubError, OSError, ValueError) as error:
        print(f"github SQL search: {error}", file=sys.stderr)
        raise SystemExit(2)
