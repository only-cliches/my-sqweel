import assert from "node:assert/strict";
import { Meilisearch, MeilisearchApiError } from "meilisearch";

const host = process.argv[2] ?? process.env.MEILI_HOST ?? "http://127.0.0.1:3407";
const apiKey = process.argv[3] ?? process.env.MEILI_API_KEY ?? "masterKey";

function trace(message) {
  if (process.env.MEILI_COMPAT_TRACE) console.error(message);
}

async function waitForHealth() {
  let lastError;
  for (let attempt = 0; attempt < 200; attempt += 1) {
    try {
      const response = await fetch(`${host}/health`, {
        headers: { Authorization: `Bearer ${apiKey}` }
      });
      if (response.ok) return;
      lastError = new Error(`health returned ${response.status}`);
    } catch (error) {
      lastError = error;
    }
    await new Promise((resolve) => setTimeout(resolve, 50));
  }
  throw lastError ?? new Error("server did not become healthy");
}

function assertTask(task, type, indexUid) {
  assert.equal(typeof task.taskUid, "number");
  assert.equal(task.type, type);
  assert.equal(task.status, "succeeded");
  if (indexUid !== undefined) assert.equal(task.indexUid, indexUid);
}

async function waitForTask(client, task, type, indexUid) {
  assert.equal(typeof task.taskUid, "number");
  const resolved = await client.tasks.waitForTask(task.taskUid, {
    timeout: 2_000,
    interval: 5
  });
  assertTask(resolved, type, indexUid);
  return resolved;
}

async function assertRawContract(path, requiredKeys) {
  const response = await fetch(`${host}${path}`, {
    headers: {
      Authorization: `Bearer ${apiKey}`,
      "X-Meili-API-Key": apiKey
    }
  });
  assert.equal(response.ok, true, `${path} returned ${response.status}`);
  const payload = await response.json();
  for (const key of requiredKeys) {
    assert.ok(Object.hasOwn(payload, key), `${path} missing ${key}`);
  }
  return payload;
}

async function rawJson(path, options = {}) {
  const response = await fetch(`${host}${path}`, {
    method: options.method ?? "GET",
    headers: {
      Authorization: options.authorization ?? `Bearer ${apiKey}`,
      "X-Meili-API-Key": apiKey,
      "Content-Type": "application/json"
    },
    body: options.body === undefined ? undefined : JSON.stringify(options.body)
  });
  assert.equal(response.ok, true, `${path} returned ${response.status}`);
  if (response.status === 204) return null;
  return await response.json();
}

await waitForHealth();
trace("health ok");

const client = new Meilisearch({
  host,
  apiKey,
  defaultWaitOptions: { timeout: 2_000, interval: 5 }
});

assert.deepEqual(await client.health(), { status: "available" });
assert.equal(await client.isHealthy(), true);
assert.equal(typeof (await client.getVersion()).pkgVersion, "string");

await assertRawContract("/version", ["commitSha", "pkgVersion", "buildDate"]);
await assertRawContract("/keys", ["results", "offset", "limit", "total"]);
await assertRawContract("/stats", ["databaseSize", "lastUpdate", "indexes"]);

for (const uid of ["sdk_books", "sdk_left", "sdk_right"]) {
  try {
    await client.deleteIndex(uid);
  } catch {
    // Cleanup is best-effort; fresh in-memory servers will not have these indexes.
  }
}

await waitForTask(
  client,
  await client.createIndex("sdk_books", { primaryKey: "id" }),
  "indexCreation",
  "sdk_books"
);

const index = client.index("sdk_books");
await waitForTask(
  client,
  await index.addDocuments([
    {
      id: "1",
      title: "Dune",
      description: "desert planet spice",
      genre: "sci-fi",
      rating: 10,
      tags: ["space", "classic"]
    },
    {
      id: "2",
      title: "Foundation",
      description: "galactic empire history",
      genre: "sci-fi",
      rating: 8,
      tags: ["space"]
    },
    {
      id: "3",
      title: "Hamlet",
      description: "royal drama",
      genre: "drama",
      rating: 12,
      tags: ["classic"]
    }
  ]),
  "documentAdditionOrUpdate",
  "sdk_books"
);

await waitForTask(
  client,
  await index.updateSearchableAttributes(["title", "description"]),
  "settingsUpdate",
  "sdk_books"
);
assert.deepEqual(await index.getSearchableAttributes(), ["title", "description"]);

const search = await index.search("desert spice", {
  filter: "genre = \"sci-fi\"",
  facets: ["genre", "rating", "tags"],
  sort: ["rating:desc"],
  attributesToRetrieve: ["id", "title"],
  showRankingScore: true
});
assert.equal(search.hits.length, 1);
assert.equal(search.hits[0].id, "1");
assert.equal(search.hits[0].title, "Dune");
assert.equal(search.hits[0].rating, undefined);
assert.equal(typeof search.hits[0]._rankingScore, "number");
assert.equal(search.facetDistribution.genre["sci-fi"], 1);
assert.equal(search.facetStats.rating.min, 10);
assert.equal(search.facetStats.rating.max, 10);
trace("base search ok");

await waitForTask(
  client,
  await index.updateSynonyms({ car: ["automobile", "transport"] }),
  "settingsUpdate",
  "sdk_books"
);
trace("synonyms ok");
const formatting = await index.search("spice", {
  attributesToHighlight: ["description"],
  attributesToCrop: ["description"],
  cropLength: 24,
  showMatchesPosition: true
});
assert.ok(formatting.hits[0]._formatted.description.includes("<em>spice</em>"));
assert.equal(Array.isArray(formatting.hits[0]._matchesPosition.description), true);
trace("formatting ok");

const facetSearch = await rawJson("/indexes/sdk_books/facet-search", {
  method: "POST",
  body: { facetName: "genre", facetQuery: "sci" }
});
assert.equal(facetSearch.facetHits[0].value, "sci-fi");
trace("facet search ok");

const dump = await rawJson("/dumps", { method: "POST" });
assert.equal(dump.status, "done");
const dumpStatus = await rawJson(`/dumps/${dump.uid}/status`);
assert.equal(dumpStatus.uid, dump.uid);
trace("dump ok");

const webhook = await rawJson("/webhooks", {
  method: "POST",
  body: { url: "https://example.com/hook", events: ["task.succeeded"] }
});
assert.equal(webhook.url, "https://example.com/hook");
const webhookList = await rawJson("/webhooks");
assert.equal(webhookList.total >= 1, true);
await rawJson(`/webhooks/${webhook.uid}`, {
  method: "PATCH",
  body: { isEnabled: false }
});
await rawJson(`/webhooks/${webhook.uid}`, { method: "DELETE" });
await rawJson("/health", { authorization: "Bearer tenant-token-shaped-value" });
trace("webhooks ok");

const getSearch = await index.searchGet("galactic", {
  attributesToSearchOn: ["description"],
  attributesToRetrieve: ["id"]
});
assert.deepEqual(
  getSearch.hits.map((hit) => hit.id),
  ["2"]
);

const documents = await index.getDocuments({
  fields: ["id", "title"],
  filter: "genre = \"sci-fi\"",
  sort: ["rating:asc"]
});
assert.equal(documents.total, 2);
assert.deepEqual(
  documents.results.map((doc) => doc.id),
  ["2", "1"]
);
assert.equal(documents.results[0].genre, undefined);

const one = await index.getDocument("1", { fields: ["title"] });
assert.deepEqual(one, { title: "Dune" });

await waitForTask(
  client,
  await index.updateDocuments([{ id: "1", title: "Dune Messiah" }]),
  "documentAdditionOrUpdate",
  "sdk_books"
);
assert.equal((await index.search("messiah")).hits[0].id, "1");

await waitForTask(
  client,
  await index.deleteDocuments(["3"]),
  "documentDeletion",
  "sdk_books"
);
assert.equal((await index.search("royal")).hits.length, 0);

const tasks = await client.tasks.getTasks({
  indexUids: ["sdk_books"],
  statuses: ["succeeded"],
  types: ["documentAdditionOrUpdate"]
});
assert.equal(tasks.results.length >= 2, true);
assert.equal(tasks.results[0].status, "succeeded");

const multi = await client.multiSearch({
  queries: [
    { indexUid: "sdk_books", q: "foundation", facets: ["genre"] },
    { indexUid: "sdk_books", q: "messiah", attributesToRetrieve: ["id"] }
  ]
});
assert.equal(multi.results.length, 2);
assert.equal(multi.results[0].facetDistribution.genre["sci-fi"], 1);
assert.equal(multi.results[1].hits[0].id, "1");

await waitForTask(
  client,
  await client.createIndex("sdk_left", { primaryKey: "id" }),
  "indexCreation",
  "sdk_left"
);
await waitForTask(
  client,
  await client.createIndex("sdk_right", { primaryKey: "id" }),
  "indexCreation",
  "sdk_right"
);
await waitForTask(
  client,
  await client.index("sdk_left").addDocuments([{ id: "l", title: "LeftOnly" }]),
  "documentAdditionOrUpdate",
  "sdk_left"
);
await waitForTask(
  client,
  await client.index("sdk_right").addDocuments([{ id: "r", title: "RightOnly" }]),
  "documentAdditionOrUpdate",
  "sdk_right"
);
await waitForTask(
  client,
  await client.swapIndexes([{ indexes: ["sdk_left", "sdk_right"] }]),
  "indexSwap",
  ""
);
assert.equal((await client.index("sdk_left").search("RightOnly")).hits[0].id, "r");
assert.equal((await client.index("sdk_right").search("LeftOnly")).hits[0].id, "l");

const indexStats = await index.getStats();
assert.equal(typeof indexStats.numberOfDocuments, "number");
assert.ok(indexStats.fieldDistribution.id >= 1);
const globalStats = await client.getStats();
assert.ok(globalStats.indexes.sdk_books);

try {
  await index.search("anything", { sort: [42] });
  assert.fail("invalid sort should throw");
} catch (error) {
  assert.equal(error instanceof MeilisearchApiError, true);
  assert.equal(error.cause?.code, "invalid_payload");
}

await waitForTask(
  client,
  await index.deleteAllDocuments(),
  "documentDeletion",
  "sdk_books"
);
assert.equal((await index.search("foundation")).hits.length, 0);

await waitForTask(client, await client.deleteIndex("sdk_books"), "indexDeletion", "sdk_books");
await waitForTask(client, await client.deleteIndex("sdk_left"), "indexDeletion", "sdk_left");
await waitForTask(client, await client.deleteIndex("sdk_right"), "indexDeletion", "sdk_right");
