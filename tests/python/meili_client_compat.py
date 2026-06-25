import sys
import time
import json
from urllib import request

import meilisearch
from meilisearch.errors import MeilisearchApiError


host = sys.argv[1] if len(sys.argv) > 1 else "http://127.0.0.1:3407"
api_key = sys.argv[2] if len(sys.argv) > 2 else "masterKey"


def wait_for_health():
    last_error = None
    for _ in range(600):
        try:
            req = request.Request(
                f"{host}/health",
                headers={"Authorization": f"Bearer {api_key}"},
            )
            with request.urlopen(req, timeout=1) as response:
                if response.status == 200:
                    return
        except Exception as error:  # pragma: no cover - surfaced below
            last_error = error
        time.sleep(0.05)
    raise RuntimeError(f"server did not become healthy: {last_error}")


def task_uid(task):
    if isinstance(task, dict):
        return task["taskUid"]
    return task.task_uid


def wait_task(client, task, expected_type, index_uid):
    resolved = client.wait_for_task(task_uid(task), timeout_in_ms=2000, interval_in_ms=5)
    assert resolved.status == "succeeded"
    assert resolved.type == expected_type
    if index_uid is not None:
        assert resolved.index_uid == index_uid
    return resolved


def doc_get(document, key):
    if isinstance(document, dict):
        return document.get(key)
    return getattr(document, key)


def doc_has(document, key):
    if isinstance(document, dict):
        return key in document
    return hasattr(document, key)


def raw_json(path, method="GET", body=None, authorization=None):
    data = None if body is None else json.dumps(body).encode("utf-8")
    req = request.Request(
        f"{host}{path}",
        data=data,
        method=method,
        headers={
            "Authorization": authorization or f"Bearer {api_key}",
            "X-Meili-API-Key": api_key,
            "Content-Type": "application/json",
        },
    )
    with request.urlopen(req, timeout=2) as response:
        if response.status == 204:
            return None
        return json.loads(response.read().decode("utf-8"))


wait_for_health()
client = meilisearch.Client(host, api_key)

assert client.health()["status"] == "available"
assert client.is_healthy() is True
assert isinstance(client.get_version()["pkgVersion"], str)

for uid in ("py_books", "py_left", "py_right"):
    try:
        client.delete_index(uid)
    except Exception:
        pass

wait_task(
    client,
    client.create_index("py_books", {"primaryKey": "id"}),
    "indexCreation",
    "py_books",
)
index = client.index("py_books")

wait_task(
    client,
    index.add_documents(
        [
            {
                "id": "1",
                "title": "Dune",
                "description": "desert planet spice",
                "genre": "sci-fi",
                "rating": 10,
            },
            {
                "id": "2",
                "title": "Foundation",
                "description": "galactic empire history",
                "genre": "sci-fi",
                "rating": 8,
            },
            {
                "id": "3",
                "title": "Hamlet",
                "description": "royal drama",
                "genre": "drama",
                "rating": 12,
            },
        ]
    ),
    "documentAdditionOrUpdate",
    "py_books",
)

wait_task(
    client,
    index.update_searchable_attributes(["title", "description"]),
    "settingsUpdate",
    "py_books",
)
assert index.get_searchable_attributes() == ["title", "description"]

search = index.search(
    "desert spice",
    {
        "filter": 'genre = "sci-fi"',
        "facets": ["genre", "rating"],
        "attributesToRetrieve": ["id", "title"],
        "showRankingScore": True,
    },
)
assert len(search["hits"]) == 1
assert search["hits"][0]["id"] == "1"
assert "rating" not in search["hits"][0]
assert isinstance(search["hits"][0]["_rankingScore"], float)
assert search["facetDistribution"]["genre"]["sci-fi"] == 1
assert search["facetStats"]["rating"]["min"] == 10

wait_task(
    client,
    index.update_synonyms({"car": ["automobile", "transport"]}),
    "settingsUpdate",
    "py_books",
)
formatting = index.search(
    "spice",
    {
        "attributesToHighlight": ["description"],
        "attributesToCrop": ["description"],
        "cropLength": 24,
        "showMatchesPosition": True,
    },
)
assert "<em>spice</em>" in formatting["hits"][0]["_formatted"]["description"]
assert isinstance(formatting["hits"][0]["_matchesPosition"]["description"], list)

facet_search = raw_json(
    "/indexes/py_books/facet-search",
    method="POST",
    body={"facetName": "genre", "facetQuery": "sci"},
)
assert facet_search["facetHits"][0]["value"] == "sci-fi"

dump = raw_json("/dumps", method="POST")
assert dump["status"] == "done"
assert raw_json(f"/dumps/{dump['uid']}/status")["uid"] == dump["uid"]

webhook = raw_json(
    "/webhooks",
    method="POST",
    body={"url": "https://example.com/hook", "events": ["task.succeeded"]},
)
assert webhook["url"] == "https://example.com/hook"
assert raw_json("/webhooks")["total"] >= 1
raw_json(f"/webhooks/{webhook['uid']}", method="PATCH", body={"isEnabled": False})
raw_json(f"/webhooks/{webhook['uid']}", method="DELETE")
raw_json("/health", authorization="Bearer tenant-token-shaped-value")

documents = index.get_documents(
    {"fields": ["id", "title"], "filter": 'genre = "sci-fi"', "sort": ["rating:asc"]}
)
assert documents.total == 2
assert [doc_get(document, "id") for document in documents.results] == ["2", "1"]
assert not doc_has(documents.results[0], "genre")

document = index.get_document("1", {"fields": ["title"]})
assert doc_get(document, "title") == "Dune"
assert not doc_has(document, "genre")

wait_task(
    client,
    index.delete_documents(["3"]),
    "documentDeletion",
    "py_books",
)
assert len(index.search("royal")["hits"]) == 0

multi = client.multi_search(
    [
        {"indexUid": "py_books", "q": "foundation", "facets": ["genre"]},
        {"indexUid": "py_books", "q": "dune", "attributesToRetrieve": ["id"]},
    ]
)
assert len(multi["results"]) == 2
assert multi["results"][0]["facetDistribution"]["genre"]["sci-fi"] == 1
assert multi["results"][1]["hits"][0]["id"] == "1"

tasks = client.get_tasks(
    {
        "indexUids": ["py_books"],
        "statuses": ["succeeded"],
        "types": ["documentAdditionOrUpdate"],
    }
)
assert len(tasks.results) >= 1

try:
    index.search("anything", {"sort": [42]})
    raise AssertionError("invalid sort should throw")
except MeilisearchApiError as error:
    assert error.code == "invalid_payload"

wait_task(client, index.delete_all_documents(), "documentDeletion", "py_books")
assert len(index.search("foundation")["hits"]) == 0

wait_task(client, client.delete_index("py_books"), "indexDeletion", "py_books")
