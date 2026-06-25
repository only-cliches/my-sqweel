use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
use std::sync::{Arc, Mutex};

use anyhow::{Result, anyhow};
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use tantivy::Index as TantivyIndex;
use tantivy::TantivyDocument;
use tantivy::collector::TopDocs;
use tantivy::query::QueryParser;
use tantivy::schema::{Field, STORED, STRING, Schema, TEXT, Value as TantivyValue};

use crate::model::StoredRow;
use crate::schema::TableSchemaHint;
use crate::sql::engine::{SeedMode, SharedEngine, Snapshot};

const DEFAULT_LIMIT: u64 = 20;

#[derive(Clone)]
struct MeiliState {
    engine: SharedEngine,
    tasks: Arc<Mutex<BTreeMap<u64, MeiliTask>>>,
    index_settings: Arc<Mutex<BTreeMap<String, Map<String, Value>>>>,
    search_indexes: Arc<TantivySearchManager>,
    dumps: Arc<Mutex<BTreeMap<String, Value>>>,
    webhooks: Arc<Mutex<BTreeMap<String, Value>>>,
    task_seq: Arc<AtomicU64>,
}

impl MeiliState {
    fn new(engine: SharedEngine) -> Self {
        Self {
            engine,
            tasks: Arc::new(Mutex::new(BTreeMap::new())),
            index_settings: Arc::new(Mutex::new(BTreeMap::new())),
            search_indexes: Arc::new(TantivySearchManager::new()),
            dumps: Arc::new(Mutex::new(BTreeMap::new())),
            webhooks: Arc::new(Mutex::new(BTreeMap::new())),
            task_seq: Arc::new(AtomicU64::new(1)),
        }
    }

    fn next_task_uid(&self) -> u64 {
        self.task_seq.fetch_add(1, AtomicOrdering::AcqRel)
    }

    fn push_task(
        &self,
        index_uid: String,
        task_type: &'static str,
        status: &'static str,
        details: Option<Value>,
    ) -> MeiliTask {
        let now = Utc::now().to_rfc3339();
        let task_uid = self.next_task_uid();
        let task = MeiliTask {
            task_uid,
            uid: task_uid,
            index_uid,
            status: status.to_string(),
            task_type: task_type.to_string(),
            enqueued_at: now.clone(),
            started_at: now.clone(),
            finished_at: now,
            duration: "PT0S".to_string(),
            error: None,
            details,
        };

        let mut tasks = self.tasks.lock().unwrap_or_else(|err| err.into_inner());
        tasks.insert(task.task_uid, task.clone());
        task
    }

    fn upsert_default_settings(&self, uid: &str) {
        let mut settings = self
            .index_settings
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        settings
            .entry(uid.to_string())
            .or_insert_with(default_meili_settings);
    }
}

#[derive(Default)]
struct TantivySearchManager {
    indexes: Mutex<BTreeMap<String, TantivySearchIndex>>,
}

impl TantivySearchManager {
    fn new() -> Self {
        Self::default()
    }

    fn rebuild(&self, uid: &str, rows: &BTreeMap<String, StoredRow>) -> Result<()> {
        let search_index = TantivySearchIndex::from_rows(rows)?;
        let mut indexes = self.indexes.lock().unwrap_or_else(|err| err.into_inner());
        indexes.insert(uid.to_string(), search_index);
        Ok(())
    }

    fn remove(&self, uid: &str) {
        let mut indexes = self.indexes.lock().unwrap_or_else(|err| err.into_inner());
        indexes.remove(uid);
    }

    fn swap(&self, left: &str, right: &str) {
        let mut indexes = self.indexes.lock().unwrap_or_else(|err| err.into_inner());
        let left_index = indexes.remove(left);
        let right_index = indexes.remove(right);
        if let Some(index) = left_index {
            indexes.insert(right.to_string(), index);
        }
        if let Some(index) = right_index {
            indexes.insert(left.to_string(), index);
        }
    }

    fn search(
        &self,
        uid: &str,
        query_text: &str,
        search_fields: Option<&[String]>,
        searchable_attributes: Option<Vec<String>>,
        limit: usize,
    ) -> Result<Option<Vec<TantivyHit>>> {
        let indexes = self.indexes.lock().unwrap_or_else(|err| err.into_inner());
        let Some(index) = indexes.get(uid) else {
            return Ok(None);
        };
        index.search(query_text, search_fields, searchable_attributes, limit)
    }
}

struct TantivySearchIndex {
    index: TantivyIndex,
    id_field: Field,
    all_text_field: Field,
    field_map: BTreeMap<String, Field>,
}

#[derive(Clone)]
struct TantivyHit {
    id: String,
    score: f32,
}

impl TantivySearchIndex {
    fn from_rows(rows: &BTreeMap<String, StoredRow>) -> Result<Self> {
        let mut field_names = BTreeMap::new();
        for row in rows.values() {
            let document = row_to_document(row);
            for key in document.keys() {
                field_names.insert(key.clone(), ());
            }
        }

        let mut schema_builder = Schema::builder();
        let id_field = schema_builder.add_text_field("__sqw_id", STRING | STORED);
        let all_text_field = schema_builder.add_text_field("__sqw_all", TEXT);
        let mut field_map = BTreeMap::new();
        for (idx, field_name) in field_names.keys().enumerate() {
            let tantivy_field_name = format!("__sqw_f_{idx}");
            let field = schema_builder.add_text_field(&tantivy_field_name, TEXT);
            field_map.insert(field_name.clone(), field);
        }

        let index = TantivyIndex::create_in_ram(schema_builder.build());
        let mut writer = index.writer(50_000_000)?;

        for row in rows.values() {
            let document = row_to_document(row);
            let document_id = document_id_for_row(row);
            let mut search_doc = TantivyDocument::new();
            search_doc.add_text(id_field, &document_id);

            let mut all_text = Vec::new();
            for (field_name, value) in &document {
                if let Some(text) = value_search_text(value).filter(|text| !text.is_empty()) {
                    all_text.push(text.clone());
                    if let Some(field) = field_map.get(field_name) {
                        search_doc.add_text(*field, text);
                    }
                }
            }
            if !all_text.is_empty() {
                search_doc.add_text(all_text_field, all_text.join(" "));
            }

            writer.add_document(search_doc)?;
        }

        writer.commit()?;

        Ok(Self {
            index,
            id_field,
            all_text_field,
            field_map,
        })
    }

    fn search(
        &self,
        query_text: &str,
        search_fields: Option<&[String]>,
        searchable_attributes: Option<Vec<String>>,
        limit: usize,
    ) -> Result<Option<Vec<TantivyHit>>> {
        let fields = self.query_fields(search_fields, searchable_attributes);
        if fields.is_empty() {
            return Ok(Some(Vec::new()));
        }

        let reader = self.index.reader()?;
        let searcher = reader.searcher();
        let mut parser = QueryParser::for_index(&self.index, fields);
        parser.set_conjunction_by_default();
        let (query, _) = parser.parse_query_lenient(query_text);
        let top_docs =
            searcher.search(&query, &TopDocs::with_limit(limit.max(1)).order_by_score())?;

        let mut hits = Vec::with_capacity(top_docs.len());
        for (score, doc_address) in top_docs {
            let retrieved = searcher.doc::<TantivyDocument>(doc_address)?;
            let Some(id) = retrieved
                .get_first(self.id_field)
                .and_then(|value| value.as_str())
            else {
                continue;
            };
            hits.push(TantivyHit {
                id: id.to_string(),
                score,
            });
        }

        Ok(Some(hits))
    }

    fn query_fields(
        &self,
        search_fields: Option<&[String]>,
        searchable_attributes: Option<Vec<String>>,
    ) -> Vec<Field> {
        if let Some(fields) = search_fields
            .filter(|fields| !fields.is_empty() && !fields.iter().any(|field| field == "*"))
        {
            return fields
                .iter()
                .filter_map(|field| self.field_map.get(field).copied())
                .collect();
        }

        if let Some(attributes) = searchable_attributes {
            if !attributes.iter().any(|field| field == "*") {
                let fields = attributes
                    .iter()
                    .filter_map(|field| self.field_map.get(field).copied())
                    .collect::<Vec<_>>();
                if !fields.is_empty() {
                    return fields;
                }
            }
        }

        let fields = self.field_map.values().copied().collect::<Vec<_>>();
        if fields.is_empty() {
            vec![self.all_text_field]
        } else {
            fields
        }
    }
}

#[derive(Debug, Clone, Serialize)]
struct MeiliTask {
    #[serde(rename = "taskUid")]
    task_uid: u64,
    uid: u64,
    #[serde(rename = "indexUid")]
    index_uid: String,
    status: String,
    #[serde(rename = "type")]
    task_type: String,
    #[serde(rename = "enqueuedAt")]
    enqueued_at: String,
    #[serde(rename = "startedAt")]
    started_at: String,
    #[serde(rename = "finishedAt")]
    finished_at: String,
    duration: String,
    error: Option<Value>,
    details: Option<Value>,
}

#[derive(Deserialize)]
struct CreateIndexRequest {
    uid: String,
    #[serde(rename = "primaryKey")]
    primary_key: Option<String>,
}

#[derive(Deserialize)]
struct UpdateIndexRequest {
    #[serde(rename = "primaryKey")]
    primary_key: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct OffsetLimitQuery {
    offset: Option<u64>,
    limit: Option<u64>,
}

#[derive(Debug, Default, Deserialize)]
struct DocumentsQuery {
    q: Option<String>,
    offset: Option<u64>,
    limit: Option<u64>,
    page: Option<u64>,
    #[serde(rename = "hitsPerPage")]
    hits_per_page: Option<u64>,
    filter: Option<String>,
    sort: Option<String>,
    #[serde(rename = "attributesToRetrieve", alias = "fields")]
    attributes_to_retrieve: Option<String>,
    #[serde(rename = "attributesToSearchOn")]
    attributes_to_search_on: Option<String>,
    #[serde(rename = "showRankingScore")]
    show_ranking_score: Option<bool>,
    #[serde(rename = "showRankingScoreDetails")]
    show_ranking_score_details: Option<bool>,
    #[serde(rename = "attributesToHighlight")]
    attributes_to_highlight: Option<String>,
    #[serde(rename = "highlightPreTag")]
    highlight_pre_tag: Option<String>,
    #[serde(rename = "highlightPostTag")]
    highlight_post_tag: Option<String>,
    #[serde(rename = "attributesToCrop")]
    attributes_to_crop: Option<String>,
    #[serde(rename = "cropLength")]
    crop_length: Option<u64>,
    #[serde(rename = "cropMarker")]
    crop_marker: Option<String>,
    #[serde(rename = "showMatchesPosition")]
    show_matches_position: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
struct FetchDocumentsBody {
    offset: Option<u64>,
    limit: Option<u64>,
    filter: Option<String>,
    sort: Option<Value>,
    #[serde(rename = "attributesToRetrieve", alias = "fields")]
    attributes_to_retrieve: Option<Value>,
}

#[derive(Debug, Default, Deserialize)]
struct SearchBody {
    q: Option<String>,
    vector: Option<Value>,
    #[serde(rename = "vectorField")]
    vector_field: Option<String>,
    offset: Option<u64>,
    limit: Option<u64>,
    page: Option<u64>,
    #[serde(rename = "hitsPerPage")]
    hits_per_page: Option<u64>,
    filter: Option<String>,
    sort: Option<Value>,
    #[serde(rename = "attributesToRetrieve", alias = "fields")]
    attributes_to_retrieve: Option<Value>,
    #[serde(rename = "attributesToSearchOn")]
    attributes_to_search_on: Option<Value>,
    facets: Option<Value>,
    #[serde(rename = "showRankingScore")]
    show_ranking_score: Option<bool>,
    #[serde(rename = "showRankingScoreDetails")]
    show_ranking_score_details: Option<bool>,
    #[serde(rename = "attributesToHighlight")]
    attributes_to_highlight: Option<Value>,
    #[serde(rename = "highlightPreTag")]
    highlight_pre_tag: Option<String>,
    #[serde(rename = "highlightPostTag")]
    highlight_post_tag: Option<String>,
    #[serde(rename = "attributesToCrop")]
    attributes_to_crop: Option<Value>,
    #[serde(rename = "cropLength")]
    crop_length: Option<u64>,
    #[serde(rename = "cropMarker")]
    crop_marker: Option<String>,
    #[serde(rename = "showMatchesPosition")]
    show_matches_position: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
struct MultiSearchQuery {
    #[serde(rename = "indexUid")]
    index_uid: String,
    q: Option<String>,
    vector: Option<Value>,
    #[serde(rename = "vectorField")]
    vector_field: Option<String>,
    offset: Option<u64>,
    limit: Option<u64>,
    page: Option<u64>,
    #[serde(rename = "hitsPerPage")]
    hits_per_page: Option<u64>,
    filter: Option<String>,
    sort: Option<Value>,
    #[serde(rename = "attributesToRetrieve", alias = "fields")]
    attributes_to_retrieve: Option<Value>,
    #[serde(rename = "attributesToSearchOn")]
    attributes_to_search_on: Option<Value>,
    facets: Option<Value>,
    #[serde(rename = "showRankingScore")]
    show_ranking_score: Option<bool>,
    #[serde(rename = "showRankingScoreDetails")]
    show_ranking_score_details: Option<bool>,
    #[serde(rename = "attributesToHighlight")]
    attributes_to_highlight: Option<Value>,
    #[serde(rename = "highlightPreTag")]
    highlight_pre_tag: Option<String>,
    #[serde(rename = "highlightPostTag")]
    highlight_post_tag: Option<String>,
    #[serde(rename = "attributesToCrop")]
    attributes_to_crop: Option<Value>,
    #[serde(rename = "cropLength")]
    crop_length: Option<u64>,
    #[serde(rename = "cropMarker")]
    crop_marker: Option<String>,
    #[serde(rename = "showMatchesPosition")]
    show_matches_position: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
struct FacetSearchBody {
    #[serde(rename = "facetName")]
    facet_name: String,
    #[serde(rename = "facetQuery")]
    facet_query: Option<String>,
    q: Option<String>,
    filter: Option<String>,
    #[serde(rename = "attributesToSearchOn")]
    attributes_to_search_on: Option<Value>,
    limit: Option<u64>,
}

#[derive(Debug, Default, Deserialize)]
struct WebhookRequest {
    url: Option<String>,
    headers: Option<Value>,
    events: Option<Value>,
    #[serde(rename = "isEnabled")]
    is_enabled: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
struct TaskQuery {
    #[serde(rename = "indexUid")]
    index_uid: Option<String>,
    #[serde(rename = "indexUids")]
    index_uids: Option<String>,
    #[serde(rename = "type")]
    task_type: Option<String>,
    #[serde(rename = "types")]
    types: Option<String>,
    uids: Option<String>,
    from: Option<u64>,
    #[serde(rename = "to")]
    to: Option<u64>,
    until: Option<u64>,
    status: Option<String>,
    #[serde(rename = "statuses")]
    statuses: Option<String>,
    offset: Option<u64>,
    limit: Option<u64>,
}

#[derive(Debug, Default, Deserialize)]
struct DocumentQuery {
    #[serde(rename = "fields", alias = "attributesToRetrieve")]
    fields: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct SwapIndexesRequest {
    indexes: Vec<String>,
}

#[derive(Debug, Clone)]
enum FilterOp {
    Eq,
    NotEq,
    Lt,
    Lte,
    Gt,
    Gte,
    In,
    NotIn,
}

#[derive(Debug, Clone)]
struct FilterCondition {
    field: String,
    op: FilterOp,
    values: Vec<Value>,
}

#[derive(Debug)]
enum DocumentDeleteSelection {
    Ids(Vec<String>),
    Filter {
        filter: String,
        conditions: Vec<FilterCondition>,
    },
}

#[derive(Debug, Clone)]
struct SortCriterion {
    field: String,
    ascending: bool,
}

#[derive(Debug, Clone)]
struct SearchRequest {
    q: Option<String>,
    vector: Option<Vec<f64>>,
    vector_field: Option<String>,
    filter: Option<Vec<FilterCondition>>,
    sort: Option<Vec<SortCriterion>>,
    attributes_to_retrieve: Option<Vec<String>>,
    attributes_to_search_on: Option<Vec<String>>,
    facets: Option<Vec<String>>,
    show_ranking_score: bool,
    show_ranking_score_details: bool,
    attributes_to_highlight: Option<Vec<String>>,
    highlight_pre_tag: String,
    highlight_post_tag: String,
    attributes_to_crop: Option<Vec<String>>,
    crop_length: usize,
    crop_marker: String,
    show_matches_position: bool,
}

pub fn spawn(addr: SocketAddr, engine: SharedEngine) {
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build();
        let Ok(rt) = rt else {
            tracing::warn!("failed to build debug http runtime");
            return;
        };

        rt.block_on(async move {
            let state = MeiliState::new(engine);

            let app = Router::new()
                .route("/_drift/health", get(drift_health))
                .route("/_drift/report", get(drift_report))
                .route("/_drift/tables", get(tables))
                .route("/_drift/tables/{table}/rows", get(table_rows))
                .route("/_drift/tables/{table}/seed", post(seed_table))
                .route("/_drift/snapshot", post(snapshot))
                .route("/_drift/restore", post(restore))
                .route("/health", get(health))
                .route("/version", get(version))
                .route("/indexes", get(list_indexes).post(create_index))
                .route(
                    "/indexes/{uid}",
                    get(get_index).patch(update_index).delete(delete_index),
                )
                .route(
                    "/indexes/{uid}/documents",
                    get(list_documents)
                        .post(add_documents)
                        .patch(add_documents)
                        .put(add_documents)
                        .delete(delete_all_documents),
                )
                .route("/indexes/{uid}/documents/fetch", post(fetch_documents))
                .route(
                    "/indexes/{uid}/documents/{document_id}",
                    get(get_document)
                        .put(replace_document)
                        .patch(patch_document)
                        .delete(delete_document),
                )
                .route(
                    "/indexes/{uid}/documents/delete-batch",
                    post(delete_documents_batch),
                )
                .route(
                    "/indexes/{uid}/documents/delete",
                    post(delete_documents_filter),
                )
                .route(
                    "/indexes/{uid}/settings",
                    get(get_settings)
                        .patch(update_settings)
                        .put(update_settings)
                        .delete(reset_settings),
                )
                .route(
                    "/indexes/{uid}/settings/{name}",
                    get(get_setting)
                        .patch(update_setting_patch)
                        .put(update_setting_put)
                        .delete(reset_setting),
                )
                .route(
                    "/indexes/{uid}/search",
                    get(search_documents_get).post(search_documents_post),
                )
                .route("/indexes/{uid}/facet-search", post(facet_search))
                .route("/indexes/{uid}/stats", get(index_stats))
                .route("/stats", get(instance_stats))
                .route("/multi-search", post(multi_search))
                .route("/swap-indexes", post(swap_indexes))
                .route("/dumps", post(create_dump))
                .route("/dumps/{uid}/status", get(get_dump_status))
                .route("/dumps/{uid}/download", get(download_dump))
                .route("/webhooks", get(list_webhooks).post(create_webhook))
                .route(
                    "/webhooks/{uid}",
                    get(get_webhook)
                        .patch(update_webhook)
                        .delete(delete_webhook),
                )
                .route("/keys", get(list_keys))
                .route("/keys/{uid}", get(get_key))
                .route("/tasks", get(list_tasks))
                .route("/tasks/{uid}", get(get_task))
                .with_state(state);

            let listener = match tokio::net::TcpListener::bind(addr).await {
                Ok(l) => l,
                Err(err) => {
                    tracing::warn!(error = %err, "debug http bind failed");
                    return;
                }
            };

            if let Err(err) = axum::serve(listener, app).await {
                tracing::warn!(error = %err, "debug http serve failed");
            }
        });
    });
}

async fn health() -> Json<Value> {
    Json(json!({ "status": "available" }))
}

async fn version() -> Json<Value> {
    Json(json!({
        "commitSha": "local",
        "pkgVersion": env!("CARGO_PKG_VERSION"),
        "buildDate": Utc::now().to_rfc3339(),
    }))
}

async fn drift_health() -> Json<Value> {
    Json(json!({ "ok": true }))
}

async fn tables(State(state): State<MeiliState>) -> Json<Value> {
    let snap = state.engine.snapshot();
    let tables: Vec<String> = snap.schemas.keys().cloned().collect();
    Json(json!({ "tables": tables }))
}

async fn drift_report(State(state): State<MeiliState>) -> Json<Value> {
    Json(state.engine.drift_report())
}

async fn table_rows(Path(table): Path<String>, State(state): State<MeiliState>) -> Json<Value> {
    let snap = state.engine.snapshot();
    let rows = snap.rows.get(&table).cloned().unwrap_or_default();
    Json(json!({ "table": table, "rows": rows }))
}

async fn seed_table(
    Path(table): Path<String>,
    State(state): State<MeiliState>,
    Json(payload): Json<Value>,
) -> (StatusCode, Json<Value>) {
    let result = parse_seed_payload(payload).and_then(|request| {
        state
            .engine
            .seed_json_rows(&table, request.rows, request.mode)
    });

    match result {
        Ok(report) => {
            if let Err(err) = rebuild_search_index(&state, &table) {
                return search_index_error(err);
            }
            (
                StatusCode::OK,
                Json(json!({
                "table": report.table,
                "mode": report.mode.as_str(),
                "rowsSeeded": report.rows_seeded,
                "rowsAffected": report.rows_affected,
                "lastInsertId": report.last_insert_id,
                })),
            )
        }
        Err(err) => (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": err.to_string(),
            })),
        ),
    }
}

async fn snapshot(State(state): State<MeiliState>) -> Json<Snapshot> {
    Json(state.engine.snapshot())
}

async fn restore(State(state): State<MeiliState>, Json(snapshot): Json<Snapshot>) -> Json<Value> {
    state.engine.restore_snapshot(snapshot);
    if let Err(err) = rebuild_all_search_indexes(&state) {
        tracing::warn!(error = %err, "failed to rebuild search indexes after restore");
    }
    Json(json!({ "restored": true }))
}

async fn list_indexes(
    State(state): State<MeiliState>,
    Query(query): Query<OffsetLimitQuery>,
) -> Json<Value> {
    let (offset, limit) = normalize_offset_limit(query.offset, query.limit);
    let snap = state.engine.snapshot();
    let mut indexes: Vec<Value> = snap
        .schemas
        .iter()
        .map(|(uid, schema)| render_index_entry(uid, schema))
        .collect();
    indexes.sort_by(|left, right| {
        let left_uid = left["uid"].as_str().unwrap_or_default();
        let right_uid = right["uid"].as_str().unwrap_or_default();
        left_uid.cmp(right_uid)
    });

    let total = indexes.len() as u64;
    let page = paginate_values(indexes, offset, limit);

    Json(json!({
        "results": page,
        "offset": offset,
        "limit": limit,
        "total": total,
    }))
}

async fn create_index(
    State(state): State<MeiliState>,
    Json(payload): Json<CreateIndexRequest>,
) -> (StatusCode, Json<Value>) {
    if payload.uid.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "message": "index uid is required", "code": "invalid_index" })),
        );
    }

    if let Err(err) = validate_identifier(&payload.uid) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "message": err.to_string(), "code": "invalid_index" })),
        );
    }

    if let Some(primary_key) = payload.primary_key.as_deref() {
        if let Err(err) = validate_identifier(primary_key) {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "message": err.to_string(), "code": "invalid_primary_key" })),
            );
        }
    }

    let table = payload.uid.trim().to_string();
    let snapshot = state.engine.snapshot();
    if snapshot.schemas.contains_key(&table) || snapshot.rows.contains_key(&table) {
        state.upsert_default_settings(&table);
        if let Err(err) = rebuild_search_index(&state, &table) {
            return search_index_error(err);
        }
        let details = json!({
            "uid": table,
            "primaryKey": table_primary_key(&snapshot, &table),
            "alreadyExists": true,
        });
        let task = state.push_task(table, "indexCreation", "succeeded", Some(details));
        return (StatusCode::ACCEPTED, Json(json!(task)));
    }

    let sql = build_create_index_sql(&table, payload.primary_key.as_deref());
    match state.engine.execute_sql(&sql) {
        Ok(_) => {
            state.upsert_default_settings(&table);
            if let Err(err) = rebuild_search_index(&state, &table) {
                return search_index_error(err);
            }
            let details = json!({
                "uid": table,
                "primaryKey": payload.primary_key,
            });
            let task = state.push_task(table, "indexCreation", "succeeded", Some(details));
            (StatusCode::ACCEPTED, Json(json!(task)))
        }
        Err(err) => (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": err.to_string() })),
        ),
    }
}

async fn get_index(
    Path(uid): Path<String>,
    State(state): State<MeiliState>,
) -> (StatusCode, Json<Value>) {
    let schema = state.engine.snapshot().schemas.get(&uid).cloned();
    let Some(schema) = schema else {
        return (
            StatusCode::NOT_FOUND,
            Json(
                json!({ "message": format!("index `{uid}` not found"), "code": "index_not_found" }),
            ),
        );
    };

    (StatusCode::OK, Json(render_index_entry(&uid, &schema)))
}

async fn update_index(
    Path(uid): Path<String>,
    State(state): State<MeiliState>,
    Json(payload): Json<UpdateIndexRequest>,
) -> (StatusCode, Json<Value>) {
    let snapshot = state.engine.snapshot();
    let schema = snapshot.schemas.get(&uid).cloned();
    let Some(schema) = schema else {
        return (
            StatusCode::NOT_FOUND,
            Json(
                json!({ "message": format!("index `{uid}` not found"), "code": "index_not_found" }),
            ),
        );
    };

    if let Some(primary_key) = payload.primary_key {
        if validate_identifier(&primary_key).is_err() {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "message": "invalid primary key", "code": "invalid_primary_key" })),
            );
        }

        let current = schema.primary_key.first().cloned();
        let details = json!({
            "uid": uid,
            "primaryKey": primary_key,
            "currentPrimaryKey": current,
        });
        let task = state.push_task(uid, "indexUpdate", "succeeded", Some(details));
        return (StatusCode::ACCEPTED, Json(json!(task)));
    }

    let details = Some(json!({ "uid": uid }));
    let task = state.push_task(uid, "indexUpdate", "succeeded", details);
    (StatusCode::ACCEPTED, Json(json!(task)))
}

async fn delete_index(
    Path(uid): Path<String>,
    State(state): State<MeiliState>,
) -> (StatusCode, Json<Value>) {
    let snapshot = state.engine.snapshot();
    if !snapshot.schemas.contains_key(&uid) && !snapshot.rows.contains_key(&uid) {
        return (
            StatusCode::NOT_FOUND,
            Json(
                json!({ "message": format!("index `{uid}` not found"), "code": "index_not_found" }),
            ),
        );
    }

    let table = quote_identifier(&uid);
    match table {
        Err(err) => (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": err.to_string() })),
        ),
        Ok(table) => match state.engine.execute_sql(&format!("DROP TABLE {table}")) {
            Ok(_) => {
                {
                    let mut settings = state
                        .index_settings
                        .lock()
                        .unwrap_or_else(|err| err.into_inner());
                    settings.remove(&uid);
                }
                state.search_indexes.remove(&uid);
                let task = state.push_task(
                    uid,
                    "indexDeletion",
                    "succeeded",
                    Some(json!({ "deleted": true })),
                );
                (StatusCode::ACCEPTED, Json(json!(task)))
            }
            Err(err) => (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": err.to_string() })),
            ),
        },
    }
}

async fn list_documents(
    Path(uid): Path<String>,
    State(state): State<MeiliState>,
    Query(query): Query<DocumentsQuery>,
) -> (StatusCode, Json<Value>) {
    let filter = match parse_filter_conditions(query.filter) {
        Ok(filter) => filter,
        Err(err) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "message": err, "code": "invalid_payload" })),
            );
        }
    };

    let sort = match parse_sort(query.sort.map(Value::String)) {
        Ok(sort) => sort,
        Err(err) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "message": err, "code": "invalid_payload" })),
            );
        }
    };

    let attributes_to_retrieve =
        match parse_field_list(query.attributes_to_retrieve.map(Value::String)) {
            Ok(attributes_to_retrieve) => attributes_to_retrieve,
            Err(err) => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({ "message": err, "code": "invalid_payload" })),
                );
            }
        };

    let attributes_to_search_on =
        match parse_field_list(query.attributes_to_search_on.map(Value::String)) {
            Ok(attributes_to_search_on) => attributes_to_search_on,
            Err(err) => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({ "message": err, "code": "invalid_payload" })),
                );
            }
        };

    list_documents_query(
        state,
        uid,
        query.q,
        query.offset,
        query.limit,
        query.page,
        query.hits_per_page,
        filter,
        sort,
        attributes_to_retrieve,
        attributes_to_search_on,
    )
}

async fn fetch_documents(
    Path(uid): Path<String>,
    State(state): State<MeiliState>,
    Json(payload): Json<FetchDocumentsBody>,
) -> (StatusCode, Json<Value>) {
    let filter = match parse_filter_conditions(payload.filter) {
        Ok(filter) => filter,
        Err(err) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "message": err, "code": "invalid_payload" })),
            );
        }
    };

    let sort = match parse_sort(payload.sort) {
        Ok(sort) => sort,
        Err(err) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "message": err, "code": "invalid_payload" })),
            );
        }
    };

    let attributes_to_retrieve = match parse_field_list(payload.attributes_to_retrieve) {
        Ok(attributes_to_retrieve) => attributes_to_retrieve,
        Err(err) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "message": err, "code": "invalid_payload" })),
            );
        }
    };

    list_documents_query(
        state,
        uid,
        None,
        payload.offset,
        payload.limit,
        None,
        None,
        filter,
        sort,
        attributes_to_retrieve,
        None,
    )
}

fn list_documents_query(
    state: MeiliState,
    uid: String,
    q: Option<String>,
    offset: Option<u64>,
    limit: Option<u64>,
    page: Option<u64>,
    hits_per_page: Option<u64>,
    filter: Option<Vec<FilterCondition>>,
    sort: Option<Vec<SortCriterion>>,
    attributes_to_retrieve: Option<Vec<String>>,
    attributes_to_search_on: Option<Vec<String>>,
) -> (StatusCode, Json<Value>) {
    if !index_exists(&state, &uid) {
        return (
            StatusCode::NOT_FOUND,
            Json(
                json!({ "message": format!("index `{uid}` not found"), "code": "index_not_found" }),
            ),
        );
    }

    let (offset, limit, _) = normalize_search_offset_limit(offset, limit, page, hits_per_page);
    let snap = state.engine.snapshot();
    let rows = snap.rows.get(&uid).cloned().unwrap_or_default();
    let searchable_attributes = searchable_attributes_for_index(&state, &uid);
    let settings = read_settings(&state, &uid);
    let fallback_search_fields = effective_search_fields(
        attributes_to_search_on.as_deref(),
        searchable_attributes.as_deref(),
    );
    let query_text = q
        .as_deref()
        .map(str::trim)
        .filter(|query| !query.is_empty());
    let text_hits = if let Some(query_text) = query_text {
        match state.search_indexes.search(
            &uid,
            query_text,
            attributes_to_search_on.as_deref(),
            searchable_attributes.clone(),
            rows.len(),
        ) {
            Ok(hits) => hits,
            Err(err) => {
                tracing::warn!(index = %uid, error = %err, "tantivy document query failed; falling back to row scan");
                None
            }
        }
    } else {
        None
    };

    let mut hits: Vec<Map<String, Value>> =
        if let Some(text_hits) = text_hits.filter(|hits| !hits.is_empty()) {
            text_hits
                .into_iter()
                .filter_map(|hit| {
                    let document = row_to_document(find_row_by_id(&rows, &hit.id)?);
                    matches_filter(&document, filter.as_deref()).then_some(document)
                })
                .collect()
        } else {
            rows.values()
                .map(row_to_document)
                .filter(|document| {
                    matches_filter(document, filter.as_deref())
                        && matches_query_with_settings(
                            document,
                            q.as_deref(),
                            fallback_search_fields.as_deref(),
                            &settings,
                        )
                })
                .collect()
        };

    if let Some(sort) = sort.as_deref() {
        sort_by_criteria(&mut hits, sort);
    }

    let hits: Vec<Value> = hits.into_iter().map(Value::Object).collect();

    let hits = match attributes_to_retrieve {
        Some(attributes_to_retrieve) => hits
            .into_iter()
            .map(|document| filter_document_attributes(document, &attributes_to_retrieve))
            .collect(),
        None => hits,
    };

    let total = hits.len() as u64;
    let results = paginate_values(hits, offset, limit);

    (
        StatusCode::OK,
        Json(json!({
            "results": results,
            "offset": offset,
            "limit": limit,
            "total": total,
        })),
    )
}

async fn get_document(
    Path((uid, document_id)): Path<(String, String)>,
    State(state): State<MeiliState>,
    Query(query): Query<DocumentQuery>,
) -> (StatusCode, Json<Value>) {
    let snap = state.engine.snapshot();
    let rows = snap.rows.get(&uid);
    let Some(rows) = rows else {
        return (
            StatusCode::NOT_FOUND,
            Json(
                json!({ "message": format!("index `{uid}` not found"), "code": "index_not_found" }),
            ),
        );
    };

    let Some(row) = find_row_by_id(rows, &document_id) else {
        return (
            StatusCode::NOT_FOUND,
            Json(
                json!({ "message": format!("document `{document_id}` not found"), "code": "document_not_found" }),
            ),
        );
    };

    let document = row_to_document(row);

    let document = match parse_field_list(query.fields.map(Value::String)) {
        Ok(Some(attributes_to_retrieve)) => {
            filter_document_attributes(Value::Object(document), &attributes_to_retrieve)
        }
        Ok(None) => Value::Object(document),
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "message": "invalid fields", "code": "invalid_payload" })),
            );
        }
    };

    (StatusCode::OK, Json(document))
}

async fn replace_document(
    Path((uid, document_id)): Path<(String, String)>,
    State(state): State<MeiliState>,
    Json(payload): Json<Value>,
) -> (StatusCode, Json<Value>) {
    upsert_single_document(state, uid, document_id, payload, true)
}

async fn patch_document(
    Path((uid, document_id)): Path<(String, String)>,
    State(state): State<MeiliState>,
    Json(payload): Json<Value>,
) -> (StatusCode, Json<Value>) {
    upsert_single_document(state, uid, document_id, payload, false)
}

fn upsert_single_document(
    state: MeiliState,
    uid: String,
    document_id: String,
    payload: Value,
    replace: bool,
) -> (StatusCode, Json<Value>) {
    let snap = state.engine.snapshot();
    let rows = match snap.rows.get(&uid) {
        Some(rows) => rows,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(
                    json!({ "message": format!("index `{uid}` not found"), "code": "index_not_found" }),
                ),
            );
        }
    };

    let existing = if replace {
        None
    } else {
        find_row_by_id(rows, &document_id)
    };

    if !replace && existing.is_none() {
        return (
            StatusCode::NOT_FOUND,
            Json(
                json!({ "message": format!("document `{document_id}` not found"), "code": "document_not_found" }),
            ),
        );
    }

    let row = match payload {
        Value::Object(mut object) => {
            let primary_key = table_primary_key(&snap, &uid);
            let document_id = Value::String(document_id.clone());

            if let Some(pk) = primary_key.as_deref() {
                if pk != "id"
                    && let Some(existing) = existing
                {
                    object.remove(pk);
                    object.remove("id");
                    for (key, value) in existing.data.iter() {
                        if key != "id" && key != &pk {
                            if !object.contains_key(key) {
                                object.insert(key.clone(), value.clone());
                            }
                        }
                    }

                    if let Some(stored_value) = existing.data.get(pk) {
                        object.insert(pk.to_string(), stored_value.clone());
                    } else {
                        object.insert(pk.to_string(), document_id.clone());
                    }
                }
            }

            object.insert("id".to_string(), document_id.clone());
            normalize_document(&mut object, primary_key.as_deref());

            vec![object]
        }
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": "document payload must be an object" })),
            );
        }
    };

    let table = match quote_identifier(&uid) {
        Ok(table) => table,
        Err(err) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": err.to_string() })),
            );
        }
    };

    if !replace && !index_exists(&state, &uid) {
        return (
            StatusCode::NOT_FOUND,
            Json(
                json!({ "message": format!("index `{uid}` not found"), "code": "index_not_found" }),
            ),
        );
    }

    if let Err(err) = state.engine.execute_sql_with_params(
        &format!("DELETE FROM {table} WHERE id = ?"),
        &[Value::String(document_id.clone())],
    ) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": err.to_string() })),
        );
    }

    match state.engine.seed_json_rows(&uid, row, SeedMode::Append) {
        Ok(report) => {
            if let Err(err) = rebuild_search_index(&state, &uid) {
                return search_index_error(err);
            }
            let details = json!({
                "documentId": document_id,
                "indexedDocuments": report.rows_affected,
            });
            let task = state.push_task(uid, "documentAdditionOrUpdate", "succeeded", Some(details));
            (StatusCode::ACCEPTED, Json(json!(task)))
        }
        Err(err) => (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": err.to_string() })),
        ),
    }
}

async fn delete_document(
    Path((uid, document_id)): Path<(String, String)>,
    State(state): State<MeiliState>,
) -> (StatusCode, Json<Value>) {
    let snapshot = state.engine.snapshot();
    let Some(rows) = snapshot.rows.get(&uid) else {
        return (
            StatusCode::NOT_FOUND,
            Json(
                json!({ "message": format!("index `{uid}` not found"), "code": "index_not_found" }),
            ),
        );
    };

    if find_row_by_id(rows, &document_id).is_none() {
        return (
            StatusCode::NOT_FOUND,
            Json(
                json!({ "message": format!("document `{document_id}` not found"), "code": "document_not_found" }),
            ),
        );
    }

    let table = match quote_identifier(&uid) {
        Ok(table) => table,
        Err(err) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": err.to_string() })),
            );
        }
    };

    if let Err(err) = state.engine.execute_sql_with_params(
        &format!("DELETE FROM {table} WHERE id = ?"),
        &[Value::String(document_id.clone())],
    ) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": err.to_string() })),
        );
    }

    if let Err(err) = rebuild_search_index(&state, &uid) {
        return search_index_error(err);
    }

    let task = state.push_task(
        uid,
        "documentDeletion",
        "succeeded",
        Some(json!({ "documentId": document_id, "deletedDocuments": 1 })),
    );
    (StatusCode::ACCEPTED, Json(json!(task)))
}

async fn delete_all_documents(
    Path(uid): Path<String>,
    State(state): State<MeiliState>,
) -> (StatusCode, Json<Value>) {
    let snapshot = state.engine.snapshot();
    let Some(rows) = snapshot.rows.get(&uid) else {
        return (
            StatusCode::NOT_FOUND,
            Json(
                json!({ "message": format!("index `{uid}` not found"), "code": "index_not_found" }),
            ),
        );
    };

    let table = match quote_identifier(&uid) {
        Ok(table) => table,
        Err(err) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": err.to_string() })),
            );
        }
    };

    if let Err(err) = state.engine.execute_sql(&format!("TRUNCATE TABLE {table}")) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": err.to_string() })),
        );
    }

    if let Err(err) = rebuild_search_index(&state, &uid) {
        return search_index_error(err);
    }

    let task = state.push_task(
        uid,
        "documentDeletion",
        "succeeded",
        Some(json!({ "deletedDocuments": rows.len() as u64 })),
    );
    (StatusCode::ACCEPTED, Json(json!(task)))
}

async fn search_documents_post(
    Path(uid): Path<String>,
    State(state): State<MeiliState>,
    Json(payload): Json<SearchBody>,
) -> (StatusCode, Json<Value>) {
    let request = match parse_search_request(
        payload.q,
        payload.vector,
        payload.vector_field,
        payload.filter,
        payload.sort,
        payload.attributes_to_retrieve,
        payload.attributes_to_search_on,
        payload.facets,
        payload.show_ranking_score,
        payload.show_ranking_score_details,
        payload.attributes_to_highlight,
        payload.highlight_pre_tag,
        payload.highlight_post_tag,
        payload.attributes_to_crop,
        payload.crop_length,
        payload.crop_marker,
        payload.show_matches_position,
    ) {
        Ok(values) => values,
        Err(err) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "message": err, "code": "invalid_payload" })),
            );
        }
    };

    do_search_documents(
        uid,
        state,
        request,
        payload.offset,
        payload.limit,
        payload.page,
        payload.hits_per_page,
    )
}

async fn add_documents(
    Path(uid): Path<String>,
    State(state): State<MeiliState>,
    Json(payload): Json<Value>,
) -> (StatusCode, Json<Value>) {
    let mut rows = match parse_documents_payload(payload) {
        Ok(rows) => rows,
        Err(err) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": err.to_string() })),
            );
        }
    };

    if !index_exists(&state, &uid) {
        return (
            StatusCode::NOT_FOUND,
            Json(
                json!({ "message": format!("index `{uid}` not found"), "code": "index_not_found" }),
            ),
        );
    }

    let primary_key = table_primary_key(&state.engine.snapshot(), &uid);
    for row in &mut rows {
        normalize_document(row, primary_key.as_deref());
    }

    match state.engine.seed_json_rows(&uid, rows, SeedMode::Append) {
        Ok(report) => {
            if let Err(err) = rebuild_search_index(&state, &uid) {
                return search_index_error(err);
            }
            let details = json!({
                "receivedDocuments": report.rows_seeded,
                "indexedDocuments": report.rows_affected,
            });
            let task = state.push_task(uid, "documentAdditionOrUpdate", "succeeded", Some(details));
            (StatusCode::ACCEPTED, Json(json!(task)))
        }
        Err(err) => (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": err.to_string() })),
        ),
    }
}

async fn delete_documents_batch(
    Path(uid): Path<String>,
    State(state): State<MeiliState>,
    Json(payload): Json<Value>,
) -> (StatusCode, Json<Value>) {
    if !index_exists(&state, &uid) {
        return (
            StatusCode::NOT_FOUND,
            Json(
                json!({ "message": format!("index `{uid}` not found"), "code": "index_not_found" }),
            ),
        );
    }

    let ids = match parse_document_ids_payload(payload) {
        Ok(ids) => ids,
        Err(err) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "message": err.to_string(), "code": "invalid_payload" })),
            );
        }
    };

    let deleted = delete_rows_by_id_strings(&state, &uid, &ids);
    if let Err(err) = rebuild_search_index(&state, &uid) {
        return search_index_error(err);
    }
    let details = json!({
        "deletedDocuments": deleted,
        "documentIds": ids,
    });
    let task = state.push_task(uid, "documentDeletion", "succeeded", Some(details));
    (StatusCode::ACCEPTED, Json(json!(task)))
}

async fn delete_documents_filter(
    Path(uid): Path<String>,
    State(state): State<MeiliState>,
    Json(payload): Json<Value>,
) -> (StatusCode, Json<Value>) {
    if !index_exists(&state, &uid) {
        return (
            StatusCode::NOT_FOUND,
            Json(
                json!({ "message": format!("index `{uid}` not found"), "code": "index_not_found" }),
            ),
        );
    }

    let selection = match parse_delete_filter_payload(payload) {
        Ok(selection) => selection,
        Err(err) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "message": err.to_string(), "code": "invalid_payload" })),
            );
        }
    };

    let details = match selection {
        DocumentDeleteSelection::Ids(ids) => json!({
            "deletedDocuments": delete_rows_by_id_strings(&state, &uid, &ids),
            "documentIds": ids,
        }),
        DocumentDeleteSelection::Filter { filter, conditions } => {
            let (deleted, ids) = delete_rows_by_filter(&state, &uid, &conditions);
            json!({
                "deletedDocuments": deleted,
                "filter": filter,
                "documentIds": ids,
            })
        }
    };
    if let Err(err) = rebuild_search_index(&state, &uid) {
        return search_index_error(err);
    }
    let task = state.push_task(uid, "documentDeletion", "succeeded", Some(details));
    (StatusCode::ACCEPTED, Json(json!(task)))
}

async fn get_settings(
    Path(uid): Path<String>,
    State(state): State<MeiliState>,
) -> (StatusCode, Json<Value>) {
    if !index_exists(&state, &uid) {
        return (
            StatusCode::NOT_FOUND,
            Json(
                json!({ "message": format!("index `{uid}` not found"), "code": "index_not_found" }),
            ),
        );
    }

    let settings = read_settings(&state, &uid);
    (StatusCode::OK, Json(settings))
}

async fn update_settings(
    Path(uid): Path<String>,
    State(state): State<MeiliState>,
    Json(payload): Json<Value>,
) -> (StatusCode, Json<Value>) {
    if !index_exists(&state, &uid) {
        return (
            StatusCode::NOT_FOUND,
            Json(
                json!({ "message": format!("index `{uid}` not found"), "code": "index_not_found" }),
            ),
        );
    }

    let patch = match payload {
        Value::Object(patch) => patch,
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                Json(
                    json!({ "message": "settings payload must be an object", "code": "invalid_payload" }),
                ),
            );
        }
    };

    let details = with_settings_mutation(&state, &uid, move |settings| {
        merge_settings(settings, patch);
    });
    let task = state.push_task(uid, "settingsUpdate", "succeeded", Some(details));
    (StatusCode::ACCEPTED, Json(json!(task)))
}

async fn reset_settings(
    Path(uid): Path<String>,
    State(state): State<MeiliState>,
) -> (StatusCode, Json<Value>) {
    if !index_exists(&state, &uid) {
        return (
            StatusCode::NOT_FOUND,
            Json(
                json!({ "message": format!("index `{uid}` not found"), "code": "index_not_found" }),
            ),
        );
    }

    let details = with_settings_mutation(&state, &uid, |settings| {
        *settings = default_meili_settings();
    });
    let task = state.push_task(uid, "settingsUpdate", "succeeded", Some(details));
    (StatusCode::ACCEPTED, Json(json!(task)))
}

async fn get_setting(
    Path((uid, name)): Path<(String, String)>,
    State(state): State<MeiliState>,
) -> (StatusCode, Json<Value>) {
    if !index_exists(&state, &uid) {
        return (
            StatusCode::NOT_FOUND,
            Json(
                json!({ "message": format!("index `{uid}` not found"), "code": "index_not_found" }),
            ),
        );
    }

    let settings = read_settings(&state, &uid);
    let value = settings
        .get(&name)
        .cloned()
        .or_else(|| default_meili_settings().get(&name).cloned());
    (StatusCode::OK, Json(value.unwrap_or(Value::Null)))
}

async fn update_setting_put(
    Path((uid, name)): Path<(String, String)>,
    State(state): State<MeiliState>,
    Json(payload): Json<Value>,
) -> (StatusCode, Json<Value>) {
    set_single_setting(uid, name, payload, false, state)
}

async fn update_setting_patch(
    Path((uid, name)): Path<(String, String)>,
    State(state): State<MeiliState>,
    Json(payload): Json<Value>,
) -> (StatusCode, Json<Value>) {
    set_single_setting(uid, name, payload, true, state)
}

async fn reset_setting(
    Path((uid, name)): Path<(String, String)>,
    State(state): State<MeiliState>,
) -> (StatusCode, Json<Value>) {
    if !index_exists(&state, &uid) {
        return (
            StatusCode::NOT_FOUND,
            Json(
                json!({ "message": format!("index `{uid}` not found"), "code": "index_not_found" }),
            ),
        );
    }

    let default = default_meili_settings();
    let restored = default.get(&name).cloned().unwrap_or(Value::Null);
    let details = with_settings_mutation(&state, &uid, |settings| {
        settings.insert(name.clone(), restored.clone());
    });
    let task = state.push_task(uid, "settingsUpdate", "succeeded", Some(details));
    (StatusCode::ACCEPTED, Json(json!(task)))
}

async fn multi_search(
    State(state): State<MeiliState>,
    Json(payload): Json<Value>,
) -> (StatusCode, Json<Value>) {
    let body = match parse_multi_search_payload(payload) {
        Ok(queries) => queries,
        Err(err) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "message": err.to_string(), "code": "invalid_payload" })),
            );
        }
    };

    let results: Vec<Value> = body
        .into_iter()
        .map(|query| {
            let request = match parse_search_request(
                query.q,
                query.vector,
                query.vector_field,
                query.filter,
                query.sort,
                query.attributes_to_retrieve,
                query.attributes_to_search_on,
                query.facets,
                query.show_ranking_score,
                query.show_ranking_score_details,
                query.attributes_to_highlight,
                query.highlight_pre_tag,
                query.highlight_post_tag,
                query.attributes_to_crop,
                query.crop_length,
                query.crop_marker,
                query.show_matches_position,
            ) {
                Ok(request) => request,
                Err(err) => {
                    return json!({
                        "error": err,
                        "code": "invalid_payload",
                        "indexUid": query.index_uid,
                    });
                }
            };

            let (_, payload) = do_search_documents_result(
                query.index_uid,
                state.clone(),
                request,
                query.offset,
                query.limit,
                query.page,
                query.hits_per_page,
            );
            payload
        })
        .collect();

    (StatusCode::OK, Json(json!({ "results": results })))
}

async fn list_keys() -> Json<Value> {
    Json(json!({
        "results": [default_key_entry()],
        "offset": 0,
        "limit": 1,
        "total": 1,
    }))
}

async fn get_key(Path(uid): Path<String>) -> (StatusCode, Json<Value>) {
    if uid != default_key_entry()["uid"].as_str().unwrap_or_default() {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "message": format!("key `{uid}` not found"), "code": "key_not_found" })),
        );
    }

    (StatusCode::OK, Json(default_key_entry()))
}

async fn instance_stats(State(state): State<MeiliState>) -> (StatusCode, Json<Value>) {
    let snap = state.engine.snapshot();
    let mut indexes = Map::new();

    for uid in snap.schemas.keys() {
        if let Some(rows) = snap.rows.get(uid) {
            indexes.insert(uid.clone(), index_stats_payload(rows));
        }
    }

    let last_update = snap
        .schemas
        .values()
        .filter_map(|schema| schema.updated_at)
        .max()
        .unwrap_or_else(Utc::now)
        .to_rfc3339();

    (
        StatusCode::OK,
        Json(json!({
            "databaseSize": 0,
            "lastUpdate": last_update,
            "indexes": indexes,
        })),
    )
}

async fn swap_indexes(
    State(state): State<MeiliState>,
    Json(payload): Json<Value>,
) -> (StatusCode, Json<Value>) {
    let swaps = match parse_swap_indexes_payload(payload) {
        Ok(swaps) => swaps,
        Err(err) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "message": err.to_string(), "code": "invalid_payload" })),
            );
        }
    };

    if swaps.is_empty() {
        let task = state.push_task(
            "".to_string(),
            "indexSwap",
            "succeeded",
            Some(json!({ "swaps": [] })),
        );
        return (StatusCode::ACCEPTED, Json(json!(task)));
    }

    let snap = state.engine.snapshot();
    let mut seen = std::collections::HashSet::new();
    for (first, second) in swaps.iter() {
        if !snap.schemas.contains_key(first) || !snap.schemas.contains_key(second) {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "message": "index_not_found", "code": "index_not_found" })),
            );
        }

        if !seen.insert(first.clone()) || !seen.insert(second.clone()) {
            return (
                StatusCode::BAD_REQUEST,
                Json(
                    json!({ "message": "duplicate index in swap payload", "code": "invalid_payload" }),
                ),
            );
        }
    }

    for (left, right) in swaps.iter() {
        let tmp = format!("__sqw_swap_tmp_{}", uuid::Uuid::new_v4());
        let tmp = match quote_identifier(&tmp) {
            Ok(table) => table,
            Err(err) => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({ "error": err.to_string() })),
                );
            }
        };

        let left_table = match quote_identifier(left) {
            Ok(table) => table,
            Err(err) => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({ "error": err.to_string() })),
                );
            }
        };

        let right_table = match quote_identifier(right) {
            Ok(table) => table,
            Err(err) => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({ "error": err.to_string() })),
                );
            }
        };

        if let Err(err) = state
            .engine
            .execute_sql(&format!("RENAME TABLE {left_table} TO {tmp}"))
        {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": err.to_string() })),
            );
        }

        if let Err(err) = state
            .engine
            .execute_sql(&format!("RENAME TABLE {right_table} TO {left_table}"))
        {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": err.to_string() })),
            );
        }

        if let Err(err) = state
            .engine
            .execute_sql(&format!("RENAME TABLE {tmp} TO {right_table}"))
        {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": err.to_string() })),
            );
        }
    }

    {
        let mut settings = state
            .index_settings
            .lock()
            .unwrap_or_else(|err| err.into_inner());

        for (first, second) in swaps.iter() {
            let first_settings = settings.remove(first);
            let second_settings = settings.remove(second);
            if let Some(value) = first_settings {
                settings.insert(second.clone(), value);
            }
            if let Some(value) = second_settings {
                settings.insert(first.clone(), value);
            }
        }
    }

    for (first, second) in swaps.iter() {
        state.search_indexes.swap(first, second);
    }

    let task = state.push_task(
        "".to_string(),
        "indexSwap",
        "succeeded",
        Some(json!({ "swaps": swaps })),
    );
    (StatusCode::ACCEPTED, Json(json!(task)))
}

async fn search_documents_get(
    Path(uid): Path<String>,
    State(state): State<MeiliState>,
    Query(query): Query<DocumentsQuery>,
) -> (StatusCode, Json<Value>) {
    let request = match parse_search_request(
        query.q,
        None,
        None,
        query.filter,
        query.sort.map(Value::String),
        query
            .attributes_to_retrieve
            .map(|attribute| Value::String(attribute)),
        query
            .attributes_to_search_on
            .map(|attribute| Value::String(attribute)),
        None,
        query.show_ranking_score,
        query.show_ranking_score_details,
        query
            .attributes_to_highlight
            .map(|attribute| Value::String(attribute)),
        query.highlight_pre_tag,
        query.highlight_post_tag,
        query
            .attributes_to_crop
            .map(|attribute| Value::String(attribute)),
        query.crop_length,
        query.crop_marker,
        query.show_matches_position,
    ) {
        Ok(request) => request,
        Err(err) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "message": err, "code": "invalid_payload" })),
            );
        }
    };

    do_search_documents(
        uid,
        state,
        request,
        query.offset,
        query.limit,
        query.page,
        query.hits_per_page,
    )
}

fn do_search_documents_result(
    uid: String,
    state: MeiliState,
    request: SearchRequest,
    offset: Option<u64>,
    limit: Option<u64>,
    page: Option<u64>,
    hits_per_page: Option<u64>,
) -> (StatusCode, Value) {
    if !index_exists(&state, &uid) {
        return (
            StatusCode::NOT_FOUND,
            json!({ "message": format!("index `{uid}` not found"), "code": "index_not_found" }),
        );
    }

    let (offset, limit, page) = normalize_search_offset_limit(offset, limit, page, hits_per_page);
    let start = std::time::Instant::now();
    let snap = state.engine.snapshot();
    let rows = snap.rows.get(&uid).cloned().unwrap_or_default();
    let query_text = request.q.clone().unwrap_or_default();
    let searchable_attributes = searchable_attributes_for_index(&state, &uid);
    let settings = read_settings(&state, &uid);
    let fallback_search_fields = effective_search_fields(
        request.attributes_to_search_on.as_deref(),
        searchable_attributes.as_deref(),
    );
    let text_hits = if request.vector.is_none() && !query_text.trim().is_empty() {
        match state.search_indexes.search(
            &uid,
            &query_text,
            request.attributes_to_search_on.as_deref(),
            searchable_attributes.clone(),
            rows.len(),
        ) {
            Ok(hits) => hits,
            Err(err) => {
                tracing::warn!(index = %uid, error = %err, "tantivy search failed; falling back to row scan");
                None
            }
        }
    } else {
        None
    };

    let mut hits: Vec<Map<String, Value>> =
        if let Some(text_hits) = text_hits.filter(|hits| !hits.is_empty()) {
            let mut seen = std::collections::HashSet::new();
            let mut hits = text_hits
                .into_iter()
                .filter_map(|hit| {
                    seen.insert(hit.id.clone());
                    let mut document = row_to_document(find_row_by_id(&rows, &hit.id)?);
                    if !matches_filter(&document, request.filter.as_deref()) {
                        return None;
                    }
                    if request.show_ranking_score {
                        document.insert("_rankingScore".to_string(), json!(hit.score));
                    }
                    if request.show_ranking_score_details {
                        document.insert(
                            "_rankingScoreDetails".to_string(),
                            json!({ "text": hit.score }),
                        );
                    }
                    Some(document)
                })
                .collect::<Vec<_>>();

            if !query_text.trim().is_empty()
                && (settings_have_synonyms(&settings) || typo_tolerance_enabled(&settings))
            {
                hits.extend(
                    rows.values()
                        .filter(|row| !seen.contains(&document_id_for_row(row)))
                        .map(row_to_document)
                        .filter(|document| matches_filter(document, request.filter.as_deref()))
                        .filter(|document| {
                            matches_query_with_settings(
                                document,
                                Some(&query_text),
                                fallback_search_fields.as_deref(),
                                &settings,
                            )
                        }),
                );
            }

            hits
        } else {
            rows.values()
                .map(row_to_document)
                .filter(|document| matches_filter(document, request.filter.as_deref()))
                .filter(|document| {
                    matches_query_with_settings(
                        document,
                        Some(&query_text),
                        fallback_search_fields.as_deref(),
                        &settings,
                    )
                })
                .collect()
        };

    if let Some(sort) = request.sort.as_deref() {
        sort_by_criteria(&mut hits, sort);
    }

    let hits: Vec<Value> = if let Some(query_vector) = request.vector.as_deref() {
        let vector_column = vector_search_field(&uid, &snap, request.vector_field.clone());
        if let Some(column_name) = vector_column {
            let mut scored_hits: Vec<(f64, Map<String, Value>)> = hits
                .into_iter()
                .filter_map(|mut document| {
                    let vector_value = document.get(&column_name)?;
                    let candidate = as_float_vector(vector_value)?;
                    cosine_similarity(&query_vector, &candidate).map(|score| {
                        if request.show_ranking_score {
                            document.insert("_rankingScore".to_string(), Value::from(score));
                        }
                        if request.show_ranking_score_details {
                            document.insert(
                                "_rankingScoreDetails".to_string(),
                                json!({ "vector": score }),
                            );
                        }
                        (score, document)
                    })
                })
                .collect();

            scored_hits.sort_by(|left, right| {
                right
                    .0
                    .partial_cmp(&left.0)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });

            scored_hits
                .into_iter()
                .map(|(_, document)| Value::Object(document))
                .collect()
        } else {
            hits.into_iter().map(Value::Object).collect()
        }
    } else {
        hits.into_iter().map(Value::Object).collect()
    };

    let facet_documents = hits.clone();
    let (facet_distribution, facet_stats) =
        compute_facets(&facet_documents, request.facets.as_deref());
    let format_documents = hits.clone();

    let hits = if let Some(retrieve) = request.attributes_to_retrieve.as_deref() {
        hits.into_iter()
            .map(|document| filter_document_attributes(document, retrieve))
            .collect::<Vec<_>>()
    } else {
        hits
    };

    let hits: Vec<Value> = hits
        .into_iter()
        .zip(format_documents.iter())
        .map(|(hit, source)| apply_search_formatting(hit, source, &request, &query_text, &settings))
        .collect();

    let hits: Vec<Value> = hits
        .into_iter()
        .map(|value| {
            let mut value = value;
            if request.show_ranking_score_details && !request.show_ranking_score {
                value.as_object_mut().map(|object| {
                    if let Some(score) = object.get("_rankingScore").cloned() {
                        object.insert(
                            "_rankingScoreDetails".to_string(),
                            json!({ "score": score }),
                        );
                    }
                });
            }
            value
        })
        .collect();

    let total = hits.len() as u64;
    let results = paginate_values(hits, offset, limit);
    let processing_time_ms = start.elapsed().as_millis() as u64;

    let mut response = json!({
        "hits": results,
        "query": query_text,
        "offset": offset,
        "limit": limit,
        "nbHits": total,
        "estimatedTotalHits": total,
        "totalHits": total,
        "page": page,
        "hitsPerPage": limit,
        "totalPages": if limit == 0 { 0 } else { total.div_ceil(limit) },
        "exhaustiveNbHits": false,
        "processingTimeMs": processing_time_ms,
    });
    if let Value::Object(payload) = &mut response {
        payload.insert("indexUid".to_string(), Value::String(uid));
        if let Some(facet_distribution) = facet_distribution {
            payload.insert("facetDistribution".to_string(), facet_distribution);
        }
        if let Some(facet_stats) = facet_stats {
            payload.insert("facetStats".to_string(), facet_stats);
        }
    }

    (StatusCode::OK, response)
}

fn do_search_documents(
    uid: String,
    state: MeiliState,
    request: SearchRequest,
    offset: Option<u64>,
    limit: Option<u64>,
    page: Option<u64>,
    hits_per_page: Option<u64>,
) -> (StatusCode, Json<Value>) {
    let (status, payload) =
        do_search_documents_result(uid, state, request, offset, limit, page, hits_per_page);
    (status, Json(payload))
}

async fn index_stats(
    Path(uid): Path<String>,
    State(state): State<MeiliState>,
) -> (StatusCode, Json<Value>) {
    let snap = state.engine.snapshot();
    let rows = snap.rows.get(&uid);
    let Some(rows) = rows else {
        return (
            StatusCode::NOT_FOUND,
            Json(
                json!({ "message": format!("index `{uid}` not found"), "code": "index_not_found" }),
            ),
        );
    };

    let payload = index_stats_payload(rows);
    (StatusCode::OK, Json(payload))
}

async fn facet_search(
    Path(uid): Path<String>,
    State(state): State<MeiliState>,
    Json(payload): Json<FacetSearchBody>,
) -> (StatusCode, Json<Value>) {
    if !index_exists(&state, &uid) {
        return (
            StatusCode::NOT_FOUND,
            Json(
                json!({ "message": format!("index `{uid}` not found"), "code": "index_not_found" }),
            ),
        );
    }

    if payload.facet_name.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "message": "facetName is required", "code": "invalid_payload" })),
        );
    }

    let filter = match parse_filter_conditions(payload.filter) {
        Ok(filter) => filter,
        Err(err) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "message": err, "code": "invalid_payload" })),
            );
        }
    };
    let attributes_to_search_on = match parse_field_list(payload.attributes_to_search_on) {
        Ok(fields) => fields,
        Err(err) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "message": err, "code": "invalid_payload" })),
            );
        }
    };

    let snap = state.engine.snapshot();
    let rows = snap.rows.get(&uid).cloned().unwrap_or_default();
    let settings = read_settings(&state, &uid);
    let searchable_attributes = searchable_attributes_for_index(&state, &uid);
    let search_fields = effective_search_fields(
        attributes_to_search_on.as_deref(),
        searchable_attributes.as_deref(),
    );
    let facet_query = payload.facet_query.unwrap_or_default();
    let facet_query_lower = facet_query.to_ascii_lowercase();
    let mut counts: BTreeMap<String, u64> = BTreeMap::new();

    for document in rows.values().map(row_to_document) {
        if !matches_filter(&document, filter.as_deref()) {
            continue;
        }
        if !matches_query_with_settings(
            &document,
            payload.q.as_deref(),
            search_fields.as_deref(),
            &settings,
        ) {
            continue;
        }
        let Some(value) = document.get(&payload.facet_name) else {
            continue;
        };
        collect_facet_values(value, &mut |value| {
            let key = facet_value_key(value);
            if facet_query_lower.is_empty() || key.to_ascii_lowercase().contains(&facet_query_lower)
            {
                *counts.entry(key).or_insert(0) += 1;
            }
        });
    }

    let mut facet_hits = counts.into_iter().collect::<Vec<_>>();
    facet_hits.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(&right.0)));
    let limit = payload.limit.unwrap_or(100).max(1).min(1000) as usize;
    let facet_hits = facet_hits
        .into_iter()
        .take(limit)
        .map(|(value, count)| json!({ "value": value, "count": count }))
        .collect::<Vec<_>>();

    (
        StatusCode::OK,
        Json(json!({
            "facetHits": facet_hits,
            "facetQuery": facet_query,
            "processingTimeMs": 0,
        })),
    )
}

async fn create_dump(State(state): State<MeiliState>) -> (StatusCode, Json<Value>) {
    let uid = uuid::Uuid::new_v4().to_string();
    let now = Utc::now().to_rfc3339();
    let snapshot = state.engine.snapshot();
    let dump = json!({
        "uid": uid,
        "dumpUid": uid,
        "status": "done",
        "createdAt": now,
        "startedAt": now,
        "finishedAt": now,
        "error": Value::Null,
        "snapshot": snapshot,
    });

    {
        let mut dumps = state.dumps.lock().unwrap_or_else(|err| err.into_inner());
        dumps.insert(uid.clone(), dump.clone());
    }

    (
        StatusCode::ACCEPTED,
        Json(json!({
            "uid": uid,
            "dumpUid": uid,
            "status": "done",
            "createdAt": now,
            "startedAt": now,
            "finishedAt": now,
        })),
    )
}

async fn get_dump_status(
    Path(uid): Path<String>,
    State(state): State<MeiliState>,
) -> (StatusCode, Json<Value>) {
    let dump = {
        let dumps = state.dumps.lock().unwrap_or_else(|err| err.into_inner());
        dumps.get(&uid).cloned()
    };

    match dump {
        Some(Value::Object(mut dump)) => {
            dump.remove("snapshot");
            (StatusCode::OK, Json(Value::Object(dump)))
        }
        Some(dump) => (StatusCode::OK, Json(dump)),
        None => (
            StatusCode::NOT_FOUND,
            Json(json!({ "message": format!("dump `{uid}` not found"), "code": "dump_not_found" })),
        ),
    }
}

async fn download_dump(
    Path(uid): Path<String>,
    State(state): State<MeiliState>,
) -> (StatusCode, Json<Value>) {
    let dump = {
        let dumps = state.dumps.lock().unwrap_or_else(|err| err.into_inner());
        dumps.get(&uid).cloned()
    };

    match dump {
        Some(dump) => (StatusCode::OK, Json(dump)),
        None => (
            StatusCode::NOT_FOUND,
            Json(json!({ "message": format!("dump `{uid}` not found"), "code": "dump_not_found" })),
        ),
    }
}

async fn list_webhooks(
    State(state): State<MeiliState>,
    Query(query): Query<OffsetLimitQuery>,
) -> Json<Value> {
    let (offset, limit) = normalize_offset_limit(query.offset, query.limit);
    let mut webhooks = {
        let webhooks = state.webhooks.lock().unwrap_or_else(|err| err.into_inner());
        webhooks.values().cloned().collect::<Vec<_>>()
    };
    webhooks.sort_by(|left, right| {
        left["uid"]
            .as_str()
            .unwrap_or_default()
            .cmp(right["uid"].as_str().unwrap_or_default())
    });
    let total = webhooks.len() as u64;
    let results = paginate_values(webhooks, offset, limit);
    Json(json!({
        "results": results,
        "offset": offset,
        "limit": limit,
        "total": total,
    }))
}

async fn create_webhook(
    State(state): State<MeiliState>,
    Json(payload): Json<WebhookRequest>,
) -> (StatusCode, Json<Value>) {
    let Some(url) = payload.url.filter(|url| !url.trim().is_empty()) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "message": "url is required", "code": "invalid_payload" })),
        );
    };

    let uid = uuid::Uuid::new_v4().to_string();
    let now = Utc::now().to_rfc3339();
    let webhook = json!({
        "uid": uid,
        "url": url,
        "headers": payload.headers.unwrap_or_else(|| json!({})),
        "events": payload.events.unwrap_or_else(|| json!(["*"])),
        "isEnabled": payload.is_enabled.unwrap_or(true),
        "createdAt": now,
        "updatedAt": now,
    });

    {
        let mut webhooks = state.webhooks.lock().unwrap_or_else(|err| err.into_inner());
        webhooks.insert(uid.clone(), webhook.clone());
    }

    (StatusCode::CREATED, Json(webhook))
}

async fn get_webhook(
    Path(uid): Path<String>,
    State(state): State<MeiliState>,
) -> (StatusCode, Json<Value>) {
    let webhook = {
        let webhooks = state.webhooks.lock().unwrap_or_else(|err| err.into_inner());
        webhooks.get(&uid).cloned()
    };

    match webhook {
        Some(webhook) => (StatusCode::OK, Json(webhook)),
        None => (
            StatusCode::NOT_FOUND,
            Json(
                json!({ "message": format!("webhook `{uid}` not found"), "code": "webhook_not_found" }),
            ),
        ),
    }
}

async fn update_webhook(
    Path(uid): Path<String>,
    State(state): State<MeiliState>,
    Json(payload): Json<WebhookRequest>,
) -> (StatusCode, Json<Value>) {
    let mut webhooks = state.webhooks.lock().unwrap_or_else(|err| err.into_inner());
    let Some(Value::Object(webhook)) = webhooks.get_mut(&uid) else {
        return (
            StatusCode::NOT_FOUND,
            Json(
                json!({ "message": format!("webhook `{uid}` not found"), "code": "webhook_not_found" }),
            ),
        );
    };

    if let Some(url) = payload.url {
        webhook.insert("url".to_string(), Value::String(url));
    }
    if let Some(headers) = payload.headers {
        webhook.insert("headers".to_string(), headers);
    }
    if let Some(events) = payload.events {
        webhook.insert("events".to_string(), events);
    }
    if let Some(enabled) = payload.is_enabled {
        webhook.insert("isEnabled".to_string(), Value::Bool(enabled));
    }
    webhook.insert(
        "updatedAt".to_string(),
        Value::String(Utc::now().to_rfc3339()),
    );

    (StatusCode::OK, Json(Value::Object(webhook.clone())))
}

async fn delete_webhook(
    Path(uid): Path<String>,
    State(state): State<MeiliState>,
) -> (StatusCode, Json<Value>) {
    let removed = {
        let mut webhooks = state.webhooks.lock().unwrap_or_else(|err| err.into_inner());
        webhooks.remove(&uid).is_some()
    };

    if removed {
        (StatusCode::OK, Json(json!({ "uid": uid, "deleted": true })))
    } else {
        (
            StatusCode::NOT_FOUND,
            Json(
                json!({ "message": format!("webhook `{uid}` not found"), "code": "webhook_not_found" }),
            ),
        )
    }
}

async fn list_tasks(
    State(state): State<MeiliState>,
    Query(query): Query<TaskQuery>,
) -> Json<Value> {
    let (offset, limit) = normalize_offset_limit(query.offset, query.limit);

    let tasks = {
        let tasks = state
            .tasks
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .values()
            .cloned()
            .collect::<Vec<_>>();
        tasks
    };

    let status_filter = combine_csv_filters(&query.status, &query.statuses).map(|items| {
        items
            .into_iter()
            .map(|item| item.to_ascii_lowercase())
            .collect::<Vec<_>>()
    });
    let type_filter = combine_csv_filters(&query.task_type, &query.types);
    let uid_filter = query.index_uids.as_deref().map(|uids| {
        uids.split(',')
            .map(|item| item.trim().to_string())
            .collect::<Vec<_>>()
    });
    let task_uid_filter = match query.uids.as_deref() {
        Some(raw_uids) => match parse_uids_filter(raw_uids) {
            Ok(uids) => Some(uids),
            Err(_) => {
                return Json(json!({ "message": "invalid uid filter", "code": "invalid_payload" }));
            }
        },
        None => None,
    };

    let until = query.until.or(query.to);

    let mut results: Vec<MeiliTask> = tasks
        .into_iter()
        .filter(|task| {
            if let Some(index_uid) = query.index_uid.as_deref() {
                if task.index_uid != index_uid {
                    return false;
                }
            }

            if let Some(statuses) = &status_filter {
                let task_status = task.status.to_ascii_lowercase();
                if !statuses.iter().any(|status| status == &task_status) {
                    return false;
                }
            }

            if let Some(index_uids) = &uid_filter {
                if !index_uids.iter().any(|uid| uid == &task.index_uid) {
                    return false;
                }
            }

            if let Some(types) = &type_filter {
                if !types
                    .iter()
                    .any(|ty| ty.to_ascii_lowercase() == task.task_type.to_ascii_lowercase())
                {
                    return false;
                }
            }

            if let Some(from) = query.from {
                if task.task_uid < from {
                    return false;
                }
            }
            if let Some(until) = until {
                if task.task_uid > until {
                    return false;
                }
            }
            if let Some(uids) = &task_uid_filter {
                if !uids.contains(&task.task_uid) {
                    return false;
                }
            }

            true
        })
        .collect();

    results.sort_by(|left, right| right.task_uid.cmp(&left.task_uid));
    let total = results.len() as u64;
    let page = paginate_values(results, offset, limit);
    let from_uid = page.first().map(|task| task.task_uid);
    let next_uid = if offset.saturating_add(limit) < total {
        page.last().map(|task| task.task_uid)
    } else {
        None
    };

    Json(json!({
        "results": page,
        "offset": offset,
        "limit": limit,
        "total": total,
        "from": from_uid,
        "next": next_uid,
    }))
}

async fn get_task(
    Path(uid): Path<u64>,
    State(state): State<MeiliState>,
) -> (StatusCode, Json<Value>) {
    let task = {
        let tasks = state.tasks.lock().unwrap_or_else(|err| err.into_inner());
        tasks.get(&uid).cloned()
    };

    match task {
        Some(task) => (StatusCode::OK, Json(json!(task))),
        None => (
            StatusCode::NOT_FOUND,
            Json(json!({ "message": format!("task `{uid}` not found"), "code": "task_not_found" })),
        ),
    }
}

struct SeedRequest {
    mode: SeedMode,
    rows: Vec<Map<String, Value>>,
}

fn parse_seed_payload(payload: Value) -> Result<SeedRequest> {
    match payload {
        Value::Array(rows) => Ok(SeedRequest {
            mode: SeedMode::Append,
            rows: parse_seed_rows(Value::Array(rows))?,
        }),
        Value::Object(mut object) if is_seed_envelope(&object) => {
            let mode = object
                .remove("mode")
                .map(parse_seed_mode)
                .transpose()?
                .unwrap_or_default();
            let rows = object
                .remove("rows")
                .ok_or_else(|| anyhow!("seed request must include rows"))?;
            Ok(SeedRequest {
                mode,
                rows: parse_seed_rows(rows)?,
            })
        }
        Value::Object(row) => Ok(SeedRequest {
            mode: SeedMode::Append,
            rows: vec![row],
        }),
        _ => Err(anyhow!(
            "seed request must be a row object, row array, or object with rows"
        )),
    }
}

fn is_seed_envelope(object: &Map<String, Value>) -> bool {
    object.contains_key("mode")
        || object
            .get("rows")
            .is_some_and(|rows| rows.is_array() || rows.is_object())
}

fn parse_seed_rows(value: Value) -> Result<Vec<Map<String, Value>>> {
    match value {
        Value::Array(rows) => rows
            .into_iter()
            .enumerate()
            .map(|(idx, value)| match value {
                Value::Object(row) => Ok(row),
                _ => Err(anyhow!("seed row at index {idx} must be an object")),
            })
            .collect(),
        Value::Object(row) => Ok(vec![row]),
        _ => Err(anyhow!(
            "seed rows must be an object or an array of objects"
        )),
    }
}

fn parse_seed_mode(value: Value) -> Result<SeedMode> {
    let Some(mode) = value.as_str() else {
        return Err(anyhow!("seed mode must be a string"));
    };

    match mode.to_ascii_lowercase().as_str() {
        "append" => Ok(SeedMode::Append),
        "replace" => Ok(SeedMode::Replace),
        _ => Err(anyhow!("seed mode must be append or replace")),
    }
}

fn parse_documents_payload(payload: Value) -> Result<Vec<Map<String, Value>>> {
    match payload {
        Value::Array(values) => values
            .into_iter()
            .enumerate()
            .map(|(idx, value)| match value {
                Value::Object(row) => Ok(row),
                _ => Err(anyhow!("document at index {idx} must be an object")),
            })
            .collect(),
        Value::Object(object) => {
            if let Some(documents) = object.get("documents") {
                parse_documents_payload(documents.clone())
            } else {
                Ok(vec![object])
            }
        }
        _ => Err(anyhow!(
            "documents payload must be an object, object with documents, or array of objects"
        )),
    }
}

fn parse_multi_search_payload(payload: Value) -> Result<Vec<MultiSearchQuery>> {
    let queries = match payload {
        Value::Object(mut object) => object.remove("queries").unwrap_or_else(|| Value::Null),
        Value::Array(_) => payload,
        _ => Value::Null,
    };

    let Value::Array(queries) = queries else {
        return Err(anyhow!(
            "multi-search payload must include a `queries` array"
        ));
    };

    queries
        .into_iter()
        .map(|query| {
            let body = serde_json::from_value::<MultiSearchQuery>(query.clone())
                .map_err(|err| anyhow!("invalid multi-search query: {err}"))?;
            if body.index_uid.trim().is_empty() {
                return Err(anyhow!("multi-search query requires indexUid"));
            }
            Ok(body)
        })
        .collect()
}

fn parse_swap_indexes_payload(payload: Value) -> Result<Vec<(String, String)>> {
    let swaps = match payload {
        Value::Array(entries) => entries,
        Value::Object(mut object) => {
            if let Some(swaps) = object.remove("swaps") {
                return parse_swap_entries(swaps);
            }

            if let Some(indexes) = object.remove("indexes") {
                if !object.is_empty() {
                    return Err(anyhow!("swap request body may only contain `indexes`"));
                }
                return parse_single_swap_pair(indexes).map(|pair| vec![pair]);
            }

            return Err(anyhow!("swap payload must include `swaps` or `indexes`"));
        }
        _ => return Err(anyhow!("swap payload must be an array or object")),
    };

    parse_swap_entries(Value::Array(swaps))
}

fn parse_swap_entries(payload: Value) -> Result<Vec<(String, String)>> {
    let Value::Array(entries) = payload else {
        return Err(anyhow!("`swaps` must be an array"));
    };

    entries.into_iter().map(parse_single_swap_pair).collect()
}

fn parse_single_swap_pair(payload: Value) -> Result<(String, String)> {
    let indexes = match payload {
        Value::Array(values) => parse_swap_index_array(values)
            .ok_or_else(|| anyhow!("swap entry must be two index names in an array"))?,
        Value::Object(value) => {
            let request: SwapIndexesRequest = serde_json::from_value(Value::Object(value))
                .map_err(|_| {
                    anyhow!("swap entry must be an object with `indexes: [\"left\", \"right\"]`")
                })?;

            if request.indexes.len() != 2 {
                return Err(anyhow!("`indexes` must contain exactly two index names"));
            }
            [request.indexes[0].clone(), request.indexes[1].clone()]
        }
        _ => {
            return Err(anyhow!(
                "swap entry must be an array or object with `indexes`"
            ));
        }
    };

    let left = indexes[0].trim().to_string();
    let right = indexes[1].trim().to_string();
    if left.is_empty() || right.is_empty() {
        return Err(anyhow!("index names in swap payload cannot be empty"));
    }

    Ok((left, right))
}

fn parse_swap_index_array(values: Vec<Value>) -> Option<[String; 2]> {
    if values.len() != 2 {
        return None;
    }
    let left = match values.first().and_then(Value::as_str) {
        Some(value) => value.to_string(),
        None => return None,
    };
    let right = match values.get(1).and_then(Value::as_str) {
        Some(value) => value.to_string(),
        None => return None,
    };
    Some([left, right])
}

fn index_stats_payload(rows: &BTreeMap<String, StoredRow>) -> Value {
    let mut field_distribution = Map::new();
    for row in rows.values() {
        for field in row_to_document(row).keys() {
            let count = field_distribution
                .entry(field.clone())
                .or_insert_with(|| Value::Number(serde_json::Number::from(0_u64)));
            if let Some(current) = count.as_u64() {
                *count = json!(current + 1);
            }
        }
    }

    json!({
        "isIndexing": false,
        "fieldDistribution": field_distribution,
        "numberOfDocuments": rows.len(),
    })
}

fn parse_document_ids_payload(payload: Value) -> Result<Vec<String>> {
    match payload {
        Value::Object(mut object) if object.len() == 1 && object.contains_key("ids") => {
            let ids = object
                .remove("ids")
                .unwrap_or_else(|| Value::Array(Vec::new()));
            parse_document_id_values(ids)
        }
        Value::Array(_) => parse_document_id_values(payload),
        _ => Err(anyhow!(
            "delete-batch payload must be an array of ids or {{\"ids\": [...]}}"
        )),
    }
}

fn parse_delete_filter_payload(payload: Value) -> Result<DocumentDeleteSelection> {
    match payload {
        Value::Object(mut object) => {
            if let Some(ids) = object.remove("ids") {
                return parse_document_id_values(ids).map(DocumentDeleteSelection::Ids);
            }

            if let Some(filter) = object.remove("filter") {
                let Some(filter) = filter.as_str() else {
                    return Err(anyhow!("filter payload must be a string"));
                };

                if let Ok(ids) = parse_filter_ids(filter) {
                    return Ok(DocumentDeleteSelection::Ids(ids));
                }

                let filter = filter.trim().to_string();
                return parse_filter_conditions(Some(filter.clone()))
                    .and_then(|conditions| {
                        if let Some(conditions) = conditions {
                            Ok(DocumentDeleteSelection::Filter { filter, conditions })
                        } else {
                            Err("invalid filter expression".to_string())
                        }
                    })
                    .map_err(|_| anyhow!("unsupported filter syntax"));
            }

            Err(anyhow!("delete payload requires `filter` or `ids`"))
        }
        Value::Array(_) => parse_document_id_values(payload).map(DocumentDeleteSelection::Ids),
        _ => Err(anyhow!("delete payload must be filter string or ids array")),
    }
}

fn parse_document_id_values(payload: Value) -> Result<Vec<String>> {
    let Value::Array(values) = payload else {
        return Err(anyhow!("ids payload must be an array"));
    };

    let ids: Vec<String> = values
        .into_iter()
        .enumerate()
        .map(|(index, value)| document_id_from_value(index, value))
        .collect::<Result<_>>()?;

    if ids.is_empty() {
        return Err(anyhow!("ids payload cannot be empty"));
    }

    Ok(ids)
}

fn parse_filter_ids(filter: &str) -> Result<Vec<String>> {
    let filter = filter.trim();

    if let Some((left, right)) = filter.split_once("=") {
        let key = left.trim();
        if key.eq_ignore_ascii_case("id") {
            let value = right.trim().trim_matches('"').trim_matches('\'');
            if value.is_empty() {
                return Err(anyhow!("filter must provide a value"));
            }
            return Ok(vec![value.to_string()]);
        }
    }

    let lower = filter.to_ascii_lowercase();
    if let Some(in_pos) = lower.find(" in ") {
        let key = lower[..in_pos].trim();
        if key != "id" {
            return Err(anyhow!("currently only id-based filters are supported"));
        }

        let list_expression = filter[in_pos + 4..].trim();
        return parse_filter_in_list(list_expression);
    }

    if let Some(in_pos) = lower.find(" in(") {
        let key = filter[..in_pos].trim();
        if key.eq_ignore_ascii_case("id") {
            return parse_filter_in_list(&filter[in_pos + 3..]);
        }
    }

    if let Some(in_pos) = lower.find(" in") {
        let key = filter[..in_pos].trim();
        if key.eq_ignore_ascii_case("id") {
            return parse_filter_in_list(&filter[in_pos + 2..]);
        }
    }

    Err(anyhow!("unsupported filter syntax"))
}

fn parse_filter_in_list(raw: &str) -> Result<Vec<String>> {
    let list_expression = raw.trim().trim();
    let Some(open) = list_expression.find('[') else {
        return Err(anyhow!("unsupported id IN filter format"));
    };

    let close = list_expression
        .rfind(']')
        .ok_or_else(|| anyhow!("unsupported id IN filter format"))?;
    if open >= close {
        return Err(anyhow!("unsupported id IN filter format"));
    }

    let inner = &list_expression[open + 1..close];
    let ids: Vec<String> = inner
        .split(',')
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .map(document_id_from_filter_token)
        .collect();

    if ids.is_empty() {
        return Err(anyhow!("filter did not match any ids"));
    }

    Ok(ids)
}

fn document_id_from_filter_token(token: &str) -> String {
    let no_quotes = token
        .trim_matches('"')
        .trim_matches('\'')
        .trim_matches('`')
        .trim();
    no_quotes.to_string()
}

fn document_id_from_value(index: usize, value: Value) -> Result<String> {
    match value {
        Value::String(value) => Ok(value),
        Value::Null => Err(anyhow!("document id at index {index} cannot be null")),
        value => Ok(value.to_string()),
    }
}

fn delete_rows_by_id_strings(state: &MeiliState, uid: &str, ids: &[String]) -> u64 {
    let Ok(table) = quote_identifier(uid) else {
        return 0;
    };

    let mut deleted = 0_u64;
    for id in ids {
        if let Ok(results) = state.engine.execute_sql_with_params(
            &format!("DELETE FROM {table} WHERE id = ?"),
            &[Value::String(id.clone())],
        ) {
            if let Some(result) = results.first() {
                deleted = deleted.saturating_add(result.rows_affected);
            }
        }
    }

    deleted
}

fn delete_rows_by_filter(
    state: &MeiliState,
    uid: &str,
    filter: &[FilterCondition],
) -> (u64, Vec<String>) {
    let snap = state.engine.snapshot();
    let Some(rows) = snap.rows.get(uid) else {
        return (0, Vec::new());
    };

    let ids = rows
        .values()
        .filter_map(|row| {
            let document = row_to_document(row);
            matches_filter(&document, Some(filter)).then_some(match row.id.as_str() {
                Some(id) => id.to_string(),
                None => row.id.to_string(),
            })
        })
        .collect::<Vec<_>>();

    let deleted = delete_rows_by_id_strings(state, uid, &ids);
    (deleted, ids)
}

fn read_settings(state: &MeiliState, uid: &str) -> Value {
    let settings = state
        .index_settings
        .lock()
        .unwrap_or_else(|err| err.into_inner())
        .get(uid)
        .cloned()
        .unwrap_or_else(default_meili_settings);

    Value::Object(settings)
}

fn with_settings_mutation<F>(state: &MeiliState, uid: &str, mutate: F) -> Value
where
    F: FnOnce(&mut Map<String, Value>),
{
    let mut settings = state
        .index_settings
        .lock()
        .unwrap_or_else(|err| err.into_inner());
    let entry = settings
        .entry(uid.to_string())
        .or_insert_with(default_meili_settings);

    mutate(entry);

    Value::Object(entry.clone())
}

fn merge_settings(settings: &mut Map<String, Value>, patch: Map<String, Value>) {
    for (key, value) in patch {
        match (settings.get_mut(&key), value) {
            (Some(Value::Object(existing)), Value::Object(incoming)) => {
                merge_settings(existing, incoming);
            }
            (Some(existing), replacement) => {
                *existing = replacement;
            }
            (None, replacement) => {
                settings.insert(key, replacement);
            }
        }
    }
}

fn set_single_setting(
    uid: String,
    name: String,
    payload: Value,
    merge: bool,
    state: MeiliState,
) -> (StatusCode, Json<Value>) {
    if !index_exists(&state, &uid) {
        return (
            StatusCode::NOT_FOUND,
            Json(
                json!({ "message": format!("index `{uid}` not found"), "code": "index_not_found" }),
            ),
        );
    }
    let details = with_settings_mutation(&state, &uid, |settings| {
        if merge {
            if let Some(Value::Object(current)) = settings.get(&name).cloned() {
                if let Some(incoming) = payload.as_object().cloned() {
                    merge_settings_value(settings, &name, current, incoming);
                    return;
                }
            }
        }

        settings.insert(name.clone(), payload.clone());
    });

    let task = state.push_task(uid, "settingsUpdate", "succeeded", Some(details));
    (StatusCode::ACCEPTED, Json(json!(task)))
}

fn merge_settings_value(
    settings: &mut Map<String, Value>,
    key: &str,
    mut current: Map<String, Value>,
    incoming: Map<String, Value>,
) {
    for (name, incoming_value) in incoming {
        match (current.get_mut(&name), incoming_value) {
            (Some(Value::Object(current)), Value::Object(incoming)) => {
                merge_settings(current, incoming);
            }
            (Some(current), incoming) => {
                *current = incoming;
            }
            (None, incoming) => {
                current.insert(name, incoming);
            }
        }
    }

    settings.insert(key.to_string(), Value::Object(current));
}

fn default_meili_settings() -> Map<String, Value> {
    let mut settings = Map::new();
    settings.insert("displayedAttributes".to_string(), json!(["*"]));
    settings.insert("searchableAttributes".to_string(), json!(["*"]));
    settings.insert("filterableAttributes".to_string(), json!([]));
    settings.insert("sortableAttributes".to_string(), json!([]));
    settings.insert(
        "rankingRules".to_string(),
        json!([
            "words",
            "typo",
            "proximity",
            "attribute",
            "sort",
            "exactness"
        ]),
    );
    settings.insert("stopWords".to_string(), json!([]));
    settings.insert("distinctAttribute".to_string(), Value::Null);
    settings.insert(
        "typoTolerance".to_string(),
        json!({
            "enabled": true,
            "minWordSizeForTypos": {
                "oneTypo": 4,
                "twoTypos": 8,
            },
            "disableOnWords": [],
            "disableOnAttributes": [],
        }),
    );
    settings.insert("synonyms".to_string(), json!({}));
    settings.insert("separatorTokens".to_string(), json!([]));
    settings.insert("nonSeparatorTokens".to_string(), json!([]));
    settings.insert("dictionary".to_string(), json!([]));
    settings.insert("faceting".to_string(), json!({ "maxValuesPerFacet": 100 }));
    settings
}

fn default_key_entry() -> Value {
    let now = Utc::now().to_rfc3339();
    let uid = "default-admin-key";
    json!({
        "uid": uid,
        "name": "Default Admin API Key",
        "description": "Built-in local admin key",
        "key": "masterKey",
        "actions": ["*"],
        "indexes": ["*"],
        "expiresAt": Value::Null,
        "createdAt": now,
        "updatedAt": now,
    })
}

fn normalize_document(document: &mut Map<String, Value>, primary_key: Option<&str>) {
    match primary_key {
        Some(pk) if pk == "id" => {
            let value = document
                .get("id")
                .cloned()
                .filter(|value| !value.is_null())
                .unwrap_or_else(|| Value::String(uuid::Uuid::new_v4().to_string()));
            document.insert(pk.to_string(), value);
        }
        Some(pk) => {
            let mut value = document
                .get(pk)
                .cloned()
                .or_else(|| document.get("id").cloned().filter(|value| !value.is_null()))
                .unwrap_or_else(|| Value::String(uuid::Uuid::new_v4().to_string()));
            if value.is_null() {
                value = Value::String(uuid::Uuid::new_v4().to_string());
            }
            document.insert(pk.to_string(), value.clone());
            if !document.contains_key("id") {
                document.insert("id".to_string(), value.clone());
            }
        }
        None => {
            document
                .entry("id".to_string())
                .or_insert_with(|| Value::String(uuid::Uuid::new_v4().to_string()));
        }
    }
}

fn render_index_entry(uid: &str, schema: &TableSchemaHint) -> Value {
    let updated_at = schema.updated_at.unwrap_or_else(Utc::now);
    let updated_at = updated_at.to_rfc3339();
    let created_at = schema.updated_at.unwrap_or_else(Utc::now).to_rfc3339();

    json!({
        "uid": uid,
        "name": uid,
        "primaryKey": schema.primary_key.first().cloned(),
        "createdAt": created_at,
        "updatedAt": updated_at,
    })
}

fn table_primary_key(snapshot: &Snapshot, uid: &str) -> Option<String> {
    snapshot
        .schemas
        .get(uid)
        .and_then(|schema| schema.primary_key.first().cloned())
}

fn table_exists(state: &MeiliState, uid: &str) -> bool {
    let snapshot = state.engine.snapshot();
    snapshot.schemas.contains_key(uid) || snapshot.rows.contains_key(uid)
}

fn index_exists(state: &MeiliState, uid: &str) -> bool {
    table_exists(state, uid)
}

fn rebuild_search_index(state: &MeiliState, uid: &str) -> Result<()> {
    let snapshot = state.engine.snapshot();
    let rows = snapshot.rows.get(uid).cloned().unwrap_or_default();
    state.search_indexes.rebuild(uid, &rows)
}

fn rebuild_all_search_indexes(state: &MeiliState) -> Result<()> {
    let snapshot = state.engine.snapshot();
    let mut uids = snapshot.schemas.keys().cloned().collect::<Vec<_>>();
    for uid in snapshot.rows.keys() {
        if !uids.contains(uid) {
            uids.push(uid.clone());
        }
    }
    for uid in uids {
        let rows = snapshot.rows.get(&uid).cloned().unwrap_or_default();
        state.search_indexes.rebuild(&uid, &rows)?;
    }
    Ok(())
}

fn search_index_error(err: anyhow::Error) -> (StatusCode, Json<Value>) {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({ "message": err.to_string(), "code": "search_index_error" })),
    )
}

fn row_to_document(row: &StoredRow) -> Map<String, Value> {
    let mut document = row.data.clone();
    document.insert("id".to_string(), row.id.clone());
    document
}

fn document_id_for_row(row: &StoredRow) -> String {
    row.id
        .as_str()
        .map(ToString::to_string)
        .unwrap_or_else(|| row.id.to_string())
}

fn value_search_text(value: &Value) -> Option<String> {
    match value {
        Value::Null => None,
        Value::Bool(value) => Some(value.to_string()),
        Value::Number(value) => Some(value.to_string()),
        Value::String(value) => Some(value.clone()),
        Value::Array(values) => {
            let text = values
                .iter()
                .filter_map(value_search_text)
                .collect::<Vec<_>>()
                .join(" ");
            (!text.is_empty()).then_some(text)
        }
        Value::Object(values) => {
            let text = values
                .values()
                .filter_map(value_search_text)
                .collect::<Vec<_>>()
                .join(" ");
            (!text.is_empty()).then_some(text)
        }
    }
}

fn searchable_attributes_for_index(state: &MeiliState, uid: &str) -> Option<Vec<String>> {
    let settings = read_settings(state, uid);
    settings
        .get("searchableAttributes")
        .and_then(Value::as_array)
        .map(|attributes| {
            attributes
                .iter()
                .filter_map(Value::as_str)
                .map(ToString::to_string)
                .collect::<Vec<_>>()
        })
}

fn effective_search_fields(
    request_fields: Option<&[String]>,
    settings_fields: Option<&[String]>,
) -> Option<Vec<String>> {
    if let Some(fields) = request_fields.filter(|fields| !fields.is_empty()) {
        if fields.iter().any(|field| field == "*") {
            return None;
        }
        return Some(fields.to_vec());
    }

    if let Some(fields) = settings_fields.filter(|fields| !fields.is_empty()) {
        if fields.iter().any(|field| field == "*") {
            return None;
        }
        return Some(fields.to_vec());
    }

    None
}

fn find_row_by_id<'a>(
    rows: &'a BTreeMap<String, StoredRow>,
    document_id: &str,
) -> Option<&'a StoredRow> {
    rows.get(document_id)
        .or_else(|| rows.get(&json!(document_id).to_string()))
}

fn parse_search_request(
    q: Option<String>,
    vector: Option<Value>,
    vector_field: Option<String>,
    filter: Option<String>,
    sort: Option<Value>,
    attributes_to_retrieve: Option<Value>,
    attributes_to_search_on: Option<Value>,
    facets: Option<Value>,
    show_ranking_score: Option<bool>,
    show_ranking_score_details: Option<bool>,
    attributes_to_highlight: Option<Value>,
    highlight_pre_tag: Option<String>,
    highlight_post_tag: Option<String>,
    attributes_to_crop: Option<Value>,
    crop_length: Option<u64>,
    crop_marker: Option<String>,
    show_matches_position: Option<bool>,
) -> Result<SearchRequest, String> {
    let vector = parse_search_vector(vector)?;
    let filter = parse_filter_conditions(filter)?;
    let sort = parse_sort(sort)?;
    let attributes_to_retrieve = parse_field_list(attributes_to_retrieve)?;
    let attributes_to_search_on = parse_field_list(attributes_to_search_on)?;
    let facets = parse_field_list(facets)?;
    let attributes_to_highlight = parse_field_list(attributes_to_highlight)?;
    let attributes_to_crop = parse_field_list(attributes_to_crop)?;

    Ok(SearchRequest {
        q,
        vector,
        vector_field,
        filter,
        sort,
        attributes_to_retrieve,
        attributes_to_search_on,
        facets,
        show_ranking_score: show_ranking_score.unwrap_or(false),
        show_ranking_score_details: show_ranking_score_details.unwrap_or(false),
        attributes_to_highlight,
        highlight_pre_tag: highlight_pre_tag.unwrap_or_else(|| "<em>".to_string()),
        highlight_post_tag: highlight_post_tag.unwrap_or_else(|| "</em>".to_string()),
        attributes_to_crop,
        crop_length: crop_length
            .and_then(|length| usize::try_from(length).ok())
            .unwrap_or(200)
            .max(1),
        crop_marker: crop_marker.unwrap_or_else(|| "...".to_string()),
        show_matches_position: show_matches_position.unwrap_or(false),
    })
}

fn parse_filter_conditions(input: Option<String>) -> Result<Option<Vec<FilterCondition>>, String> {
    let Some(raw) = input
        .as_deref()
        .map(str::trim)
        .filter(|raw| !raw.is_empty())
    else {
        return Ok(None);
    };

    let parts = split_filter_expressions(raw);
    if parts.is_empty() {
        return Ok(None);
    }

    let mut conditions = Vec::with_capacity(parts.len());
    for part in parts {
        conditions.push(parse_filter_condition(part)?);
    }
    Ok(Some(conditions))
}

fn parse_uids_filter(raw: &str) -> Result<Vec<u64>, String> {
    let ids = raw
        .split(',')
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(|item| {
            item.parse::<u64>()
                .map_err(|_| format!("invalid task uid `{item}`"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    if ids.is_empty() {
        Err("uid filter cannot be empty".to_string())
    } else {
        Ok(ids)
    }
}

fn combine_csv_filters(
    primary: &Option<String>,
    secondary: &Option<String>,
) -> Option<Vec<String>> {
    let mut filters = Vec::new();

    for raw in [primary, secondary] {
        let Some(raw) = raw.as_ref() else {
            continue;
        };

        for value in raw
            .split(',')
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            if value.eq_ignore_ascii_case("*") {
                return None;
            }
            filters.push(value.to_string());
        }
    }

    if filters.is_empty() {
        None
    } else {
        Some(filters)
    }
}

fn split_filter_expressions(raw: &str) -> Vec<&str> {
    let lower = raw.to_ascii_lowercase();
    let mut parts = Vec::new();
    let mut start = 0usize;

    while let Some(index) = lower[start..].find(" and ") {
        let separator = start + index;
        let part = raw[start..separator].trim();
        if !part.is_empty() {
            parts.push(part);
        }
        start = separator + 5;
    }

    let tail = raw[start..].trim();
    if !tail.is_empty() {
        parts.push(tail);
    }

    parts
}

fn parse_filter_condition(raw: &str) -> Result<FilterCondition, String> {
    let Some((field, operator, right)) =
        parse_named_filter_condition(raw, " not in ", FilterOp::NotIn)
            .or_else(|| parse_named_filter_condition(raw, " in ", FilterOp::In))
            .or_else(|| parse_scalar_filter_condition(raw, "!=", FilterOp::NotEq))
            .or_else(|| parse_scalar_filter_condition(raw, ">=", FilterOp::Gte))
            .or_else(|| parse_scalar_filter_condition(raw, "<=", FilterOp::Lte))
            .or_else(|| parse_scalar_filter_condition(raw, ">", FilterOp::Gt))
            .or_else(|| parse_scalar_filter_condition(raw, "<", FilterOp::Lt))
            .or_else(|| parse_scalar_filter_condition(raw, "=", FilterOp::Eq))
    else {
        return Err(format!("unsupported filter condition: `{raw}`"));
    };

    Ok(FilterCondition {
        field: field.to_string(),
        op: operator,
        values: right,
    })
}

fn parse_named_filter_condition(
    raw: &str,
    token: &str,
    op: FilterOp,
) -> Option<(String, FilterOp, Vec<Value>)> {
    let left_right = split_once_ci(raw, token)?;
    let field = left_right.0.trim();
    if field.is_empty() {
        return None;
    }
    let right = left_right
        .1
        .trim_start_matches(|c: char| c.is_ascii_whitespace());
    let right = if right.is_empty() { raw } else { right };
    Some((field.to_string(), op, parse_filter_values(right)))
}

fn split_once_ci<'a>(raw: &'a str, token: &str) -> Option<(&'a str, &'a str)> {
    let start = raw.to_ascii_lowercase().find(token)?;
    let left = &raw[..start];
    let right = &raw[start + token.len()..];
    Some((left, right))
}

fn parse_scalar_filter_condition(
    raw: &str,
    token: &str,
    op: FilterOp,
) -> Option<(String, FilterOp, Vec<Value>)> {
    let left_right = raw.split_once(token)?;
    let field = left_right.0.trim();
    if field.is_empty() {
        return None;
    }
    let value = parse_filter_value(left_right.1.trim())?;
    Some((field.to_string(), op, vec![value]))
}

fn parse_filter_values(raw: &str) -> Vec<Value> {
    let value = raw.trim();
    let Some(open) = value.find('[') else {
        return Vec::new();
    };
    let Some(close) = value.rfind(']') else {
        return Vec::new();
    };
    if open >= close {
        return Vec::new();
    }

    parse_csv_values(&value[open + 1..close])
        .into_iter()
        .filter_map(|entry| parse_filter_value(&entry))
        .collect()
}

fn parse_filter_value(raw: &str) -> Option<Value> {
    let value = raw.trim();
    if value.is_empty() {
        return None;
    }
    if value.eq_ignore_ascii_case("null") {
        return Some(Value::Null);
    }
    if value.eq_ignore_ascii_case("true") {
        return Some(Value::Bool(true));
    }
    if value.eq_ignore_ascii_case("false") {
        return Some(Value::Bool(false));
    }

    let quoted = quoted_value(value);
    if let Some(quoted) = quoted {
        return Some(Value::String(quoted.to_string()));
    }

    if let Ok(int_value) = value.parse::<i64>() {
        return Some(json!(int_value));
    }

    if let Ok(float_value) = value.parse::<f64>() {
        return Some(json!(float_value));
    }

    Some(Value::String(value.to_string()))
}

fn parse_csv_values(raw: &str) -> Vec<String> {
    let mut entries = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;

    for ch in raw.chars() {
        match (quote, ch) {
            (Some(quote_char), c) if c == quote_char => {
                quote = None;
                current.push(c);
            }
            (None, c @ '\'') | (None, c @ '"') => {
                quote = Some(c);
                current.push(c);
            }
            (None, ',') => {
                let token = current.trim();
                if !token.is_empty() {
                    entries.push(token.to_string());
                }
                current.clear();
            }
            _ => current.push(ch),
        }
    }

    let token = current.trim();
    if !token.is_empty() {
        entries.push(token.to_string());
    }

    entries
}

fn quoted_value(value: &str) -> Option<&str> {
    if value.len() >= 2 {
        let first = value.chars().next()?;
        let last = value.chars().last()?;
        if (first == '\'' && last == '\'') || (first == '"' && last == '"') {
            return Some(&value[1..value.len().saturating_sub(1)]);
        }
    }
    None
}

fn parse_sort(input: Option<Value>) -> Result<Option<Vec<SortCriterion>>, String> {
    let Some(raw) = input else {
        return Ok(None);
    };

    let mut values = match raw {
        Value::String(raw) => vec![raw],
        Value::Array(raw) => raw
            .into_iter()
            .map(|entry| match entry {
                Value::String(value) => Ok(value),
                _ => Err("sort entries must be strings".to_string()),
            })
            .collect::<Result<Vec<_>, _>>()?,
        _ => {
            return Err("invalid sort payload: expected string or array of strings".to_string());
        }
    };
    if values.is_empty() {
        return Ok(None);
    }

    let sort = values
        .drain(..)
        .map(|entry| {
            let value = entry.trim().to_string();
            if value.is_empty() {
                return Err(format!("invalid sort entry `{entry}`"));
            }

            if let Some(field) = value.strip_prefix('-') {
                Ok(SortCriterion {
                    field: field.to_string(),
                    ascending: false,
                })
            } else if let Some(field) = value.strip_prefix('+') {
                Ok(SortCriterion {
                    field: field.to_string(),
                    ascending: true,
                })
            } else if let Some((field, direction)) = value.split_once(':') {
                let direction = direction.trim();
                match direction {
                    "asc" => Ok(SortCriterion {
                        field: field.trim().to_string(),
                        ascending: true,
                    }),
                    "desc" => Ok(SortCriterion {
                        field: field.trim().to_string(),
                        ascending: false,
                    }),
                    _ => Err(format!("invalid sort direction in `{value}`")),
                }
            } else {
                Ok(SortCriterion {
                    field: value,
                    ascending: true,
                })
            }
        })
        .collect::<Result<Vec<_>, _>>()?;

    Ok(Some(sort))
}

fn parse_field_list(raw: Option<Value>) -> Result<Option<Vec<String>>, String> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    let values = match raw {
        Value::String(values) => values
            .split(',')
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToString::to_string)
            .collect::<Vec<_>>(),
        Value::Array(values) => values
            .into_iter()
            .map(|value| match value {
                Value::String(value) => {
                    let value = value.trim().to_string();
                    if value.is_empty() {
                        Err("field name cannot be empty".to_string())
                    } else {
                        Ok(value)
                    }
                }
                _ => Err("field list values must be strings".to_string()),
            })
            .collect::<Result<Vec<_>, _>>()?,
        _ => {
            return Err("invalid fields payload: expected string or array of strings".to_string());
        }
    };

    if values.is_empty() {
        return Ok(None);
    }

    Ok(Some(values))
}

fn parse_search_vector(value: Option<Value>) -> Result<Option<Vec<f64>>, String> {
    let Some(value) = value else {
        return Ok(None);
    };

    let values = value
        .as_array()
        .ok_or_else(|| "invalid vector payload: expected an array of numbers".to_string())?;

    if values.is_empty() {
        return Ok(None);
    }

    let mut vector = Vec::with_capacity(values.len());
    for (index, item) in values.iter().enumerate() {
        let Some(number) = item.as_f64() else {
            return Err(format!(
                "invalid vector payload at index {index}: expected a number"
            ));
        };
        vector.push(number);
    }

    Ok(Some(vector))
}

fn matches_filter(document: &Map<String, Value>, filter: Option<&[FilterCondition]>) -> bool {
    let Some(filter) = filter else {
        return true;
    };

    filter.iter().all(|condition| {
        let Some(value) = document.get(&condition.field) else {
            return matches!(condition.op, FilterOp::NotEq | FilterOp::NotIn);
        };
        matches_filter_condition(value, condition)
    })
}

fn matches_filter_condition(value: &Value, condition: &FilterCondition) -> bool {
    match condition.op {
        FilterOp::In | FilterOp::NotIn => {
            let found = condition
                .values
                .iter()
                .any(|expected| values_equivalent(value, expected));
            if matches!(condition.op, FilterOp::In) {
                found
            } else {
                !found
            }
        }
        FilterOp::Eq => condition
            .values
            .first()
            .is_some_and(|expected| values_equivalent(value, expected)),
        FilterOp::NotEq => condition
            .values
            .first()
            .is_none_or(|expected| !values_equivalent(value, expected)),
        FilterOp::Lt => condition.values.first().is_some_and(|expected| {
            compare_filter_values(value, expected).map_or(false, |cmp| cmp.is_lt())
        }),
        FilterOp::Lte => condition.values.first().is_some_and(|expected| {
            compare_filter_values(value, expected).map_or(false, |cmp| cmp.is_le())
        }),
        FilterOp::Gt => condition.values.first().is_some_and(|expected| {
            compare_filter_values(value, expected).map_or(false, |cmp| cmp.is_gt())
        }),
        FilterOp::Gte => condition.values.first().is_some_and(|expected| {
            compare_filter_values(value, expected).map_or(false, |cmp| cmp.is_ge())
        }),
    }
}

fn compare_filter_values(left: &Value, right: &Value) -> Option<std::cmp::Ordering> {
    match (left, right) {
        (Value::Number(left), Value::Number(right)) => {
            left.as_f64().zip(right.as_f64()).map(|(left, right)| {
                left.partial_cmp(&right)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
        }
        (Value::String(left), Value::String(right)) => Some(left.cmp(right)),
        _ => None,
    }
}

fn values_equivalent(left: &Value, right: &Value) -> bool {
    match (left, right) {
        (Value::Number(left), Value::Number(right)) => left
            .as_f64()
            .zip(right.as_f64())
            .is_some_and(|(left, right)| (left - right).abs() <= f64::EPSILON),
        _ => left == right,
    }
}

fn sort_by_criteria(items: &mut [Map<String, Value>], criteria: &[SortCriterion]) {
    if criteria.is_empty() {
        return;
    }

    items.sort_by(|left, right| {
        for criterion in criteria {
            let left_value = left.get(&criterion.field);
            let right_value = right.get(&criterion.field);

            let order = compare_values(left_value, right_value);
            if order != std::cmp::Ordering::Equal {
                return if criterion.ascending {
                    order
                } else {
                    order.reverse()
                };
            }
        }
        std::cmp::Ordering::Equal
    });
}

fn compare_values(left: Option<&Value>, right: Option<&Value>) -> std::cmp::Ordering {
    match (left, right) {
        (None, None) => std::cmp::Ordering::Equal,
        (None, Some(_)) => std::cmp::Ordering::Less,
        (Some(_), None) => std::cmp::Ordering::Greater,
        (Some(Value::Number(left)), Some(Value::Number(right))) => left
            .as_f64()
            .zip(right.as_f64())
            .map(|(left, right)| {
                left.partial_cmp(&right)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .unwrap_or(std::cmp::Ordering::Equal),
        (Some(Value::Bool(left)), Some(Value::Bool(right))) => left.cmp(right),
        (Some(Value::String(left)), Some(Value::String(right))) => left.cmp(right),
        _ => {
            let left = left.map(|value| value.to_string()).unwrap_or_default();
            let right = right.map(|value| value.to_string()).unwrap_or_default();
            left.cmp(&right)
        }
    }
}

fn filter_document_attributes(document: Value, attributes_to_retrieve: &[String]) -> Value {
    if attributes_to_retrieve.iter().any(|field| field == "*") {
        return document;
    }

    let Value::Object(values) = document else {
        return Value::Object(Map::new());
    };

    let filtered = values
        .into_iter()
        .filter(|(key, _)| {
            attributes_to_retrieve.contains(key)
                || key == "_rankingScore"
                || key == "_rankingScoreDetails"
        })
        .collect::<Map<_, _>>();
    Value::Object(filtered)
}

fn compute_facets(
    documents: &[Value],
    facets: Option<&[String]>,
) -> (Option<Value>, Option<Value>) {
    let Some(fields) = facets else {
        return (None, None);
    };

    let fields = resolve_facet_fields(documents, fields);
    let mut distribution = Map::new();
    let mut stats = Map::new();

    for field in fields {
        let mut counts: BTreeMap<String, u64> = BTreeMap::new();
        let mut min_number: Option<f64> = None;
        let mut max_number: Option<f64> = None;

        for document in documents {
            let Some(value) = document.as_object().and_then(|object| object.get(&field)) else {
                continue;
            };

            collect_facet_values(value, &mut |value| {
                let key = facet_value_key(value);
                *counts.entry(key).or_insert(0) += 1;

                if let Some(number) = value.as_f64() {
                    min_number = Some(min_number.map_or(number, |min| min.min(number)));
                    max_number = Some(max_number.map_or(number, |max| max.max(number)));
                }
            });
        }

        let values = counts
            .into_iter()
            .map(|(value, count)| (value, json!(count)))
            .collect::<Map<_, _>>();
        distribution.insert(field.clone(), Value::Object(values));

        if let (Some(min), Some(max)) = (min_number, max_number) {
            stats.insert(field, json!({ "min": min, "max": max }));
        }
    }

    let distribution = (!distribution.is_empty()).then_some(Value::Object(distribution));
    let stats = (!stats.is_empty()).then_some(Value::Object(stats));
    (distribution, stats)
}

fn resolve_facet_fields(documents: &[Value], fields: &[String]) -> Vec<String> {
    if !fields.iter().any(|field| field == "*") {
        return fields.to_vec();
    }

    let mut resolved = BTreeMap::new();
    for document in documents {
        if let Some(object) = document.as_object() {
            for (field, value) in object {
                if field.starts_with('_') {
                    continue;
                }
                if value.is_null() || value.is_object() {
                    continue;
                }
                resolved.insert(field.clone(), ());
            }
        }
    }
    resolved.into_keys().collect()
}

fn collect_facet_values<'a>(value: &'a Value, emit: &mut impl FnMut(&'a Value)) {
    match value {
        Value::Array(values) => {
            for value in values {
                collect_facet_values(value, emit);
            }
        }
        Value::Null | Value::Object(_) => {}
        value => emit(value),
    }
}

fn facet_value_key(value: &Value) -> String {
    match value {
        Value::String(value) => value.clone(),
        Value::Bool(value) => value.to_string(),
        Value::Number(value) => value.to_string(),
        _ => value.to_string(),
    }
}

fn vector_search_field(
    uid: &str,
    snap: &crate::sql::engine::Snapshot,
    requested: Option<String>,
) -> Option<String> {
    let schema = snap.schemas.get(uid)?;

    if let Some(field) = requested {
        if schema
            .columns
            .get(&field)
            .and_then(|hint| hint.sql_type.as_ref())
            .is_some_and(|sql_type| sql_type.to_ascii_lowercase().starts_with("vector("))
        {
            return Some(field);
        }

        return None;
    }

    schema.columns.iter().find_map(|(column, hint)| {
        hint.sql_type
            .as_ref()
            .is_some_and(|sql_type| sql_type.to_ascii_lowercase().starts_with("vector("))
            .then(|| column.clone())
    })
}

fn as_float_vector(value: &Value) -> Option<Vec<f64>> {
    let Value::Array(values) = value else {
        return None;
    };

    values
        .iter()
        .map(|value| value.as_f64())
        .collect::<Option<Vec<_>>>()
}

fn cosine_similarity(left: &[f64], right: &[f64]) -> Option<f64> {
    if left.len() != right.len() || left.is_empty() {
        return None;
    }

    let mut dot = 0.0_f64;
    let mut left_norm = 0.0_f64;
    let mut right_norm = 0.0_f64;

    for (left_value, right_value) in left.iter().zip(right.iter()) {
        dot += left_value * right_value;
        left_norm += left_value * left_value;
        right_norm += right_value * right_value;
    }

    if left_norm == 0.0 || right_norm == 0.0 {
        return None;
    }

    Some(dot / (left_norm.sqrt() * right_norm.sqrt()))
}

fn matches_query(
    document: &Map<String, Value>,
    q: Option<&str>,
    search_fields: Option<&[String]>,
) -> bool {
    let Some(query) = q.map(str::trim) else {
        return true;
    };
    if query.is_empty() {
        return true;
    }
    let query = query.to_ascii_lowercase();
    let haystack = if let Some(fields) = search_fields.filter(|fields| !fields.is_empty()) {
        fields
            .iter()
            .filter_map(|field| document.get(field))
            .map(|value| value.to_string().to_ascii_lowercase())
            .collect::<Vec<_>>()
            .join(" ")
    } else {
        document
            .values()
            .map(|value| value.to_string().to_ascii_lowercase())
            .collect::<Vec<_>>()
            .join(" ")
    };
    haystack.contains(&query)
}

fn matches_query_with_settings(
    document: &Map<String, Value>,
    q: Option<&str>,
    search_fields: Option<&[String]>,
    settings: &Value,
) -> bool {
    if matches_query(document, q, search_fields) {
        return true;
    }

    let Some(query) = q.map(str::trim).filter(|query| !query.is_empty()) else {
        return true;
    };
    let query_tokens = text_tokens(query);
    if query_tokens.is_empty() {
        return true;
    }

    let document_text = searchable_document_text(document, search_fields);
    if document_text.is_empty() {
        return false;
    }
    let document_tokens = text_tokens(&document_text);
    if document_tokens.is_empty() {
        return false;
    }

    query_tokens.iter().all(|query_token| {
        let alternatives = synonym_alternatives(settings, query_token);
        alternatives.iter().any(|alternative| {
            let alternative_lower = alternative.to_ascii_lowercase();
            document_text.contains(&alternative_lower)
                || document_tokens.iter().any(|document_token| {
                    document_token == &alternative_lower
                        || typo_matches(settings, query_token, document_token)
                })
        })
    })
}

fn searchable_document_text(
    document: &Map<String, Value>,
    search_fields: Option<&[String]>,
) -> String {
    if let Some(fields) = search_fields.filter(|fields| !fields.is_empty()) {
        fields
            .iter()
            .filter_map(|field| document.get(field))
            .filter_map(value_search_text)
            .map(|value| value.to_ascii_lowercase())
            .collect::<Vec<_>>()
            .join(" ")
    } else {
        document
            .values()
            .filter_map(value_search_text)
            .map(|value| value.to_ascii_lowercase())
            .collect::<Vec<_>>()
            .join(" ")
    }
}

fn text_tokens(text: &str) -> Vec<String> {
    text.split(|ch: char| !ch.is_alphanumeric())
        .map(str::trim)
        .filter(|token| !token.is_empty())
        .map(|token| token.to_ascii_lowercase())
        .collect()
}

fn settings_have_synonyms(settings: &Value) -> bool {
    settings
        .get("synonyms")
        .and_then(Value::as_object)
        .is_some_and(|synonyms| !synonyms.is_empty())
}

fn synonym_alternatives(settings: &Value, term: &str) -> Vec<String> {
    let mut alternatives = BTreeMap::new();
    alternatives.insert(term.to_ascii_lowercase(), ());
    let Some(synonyms) = settings.get("synonyms").and_then(Value::as_object) else {
        return alternatives.into_keys().collect();
    };

    let term_lower = term.to_ascii_lowercase();
    for (key, values) in synonyms {
        let key_lower = key.to_ascii_lowercase();
        let values = values
            .as_array()
            .map(|values| {
                values
                    .iter()
                    .filter_map(Value::as_str)
                    .map(|value| value.to_ascii_lowercase())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        if key_lower == term_lower || values.iter().any(|value| value == &term_lower) {
            alternatives.insert(key_lower, ());
            for value in values {
                alternatives.insert(value, ());
            }
        }
    }

    alternatives.into_keys().collect()
}

fn typo_tolerance_enabled(settings: &Value) -> bool {
    settings
        .get("typoTolerance")
        .and_then(|typo| typo.get("enabled"))
        .and_then(Value::as_bool)
        .unwrap_or(true)
}

fn typo_matches(settings: &Value, query_token: &str, document_token: &str) -> bool {
    if !typo_tolerance_enabled(settings) {
        return false;
    }
    if query_token == document_token {
        return true;
    }
    if typo_disabled_for_word(settings, query_token) {
        return false;
    }

    let allowed = allowed_typos(settings, query_token);
    if allowed == 0 {
        return false;
    }
    levenshtein_at_most(query_token, document_token, allowed)
}

fn typo_disabled_for_word(settings: &Value, query_token: &str) -> bool {
    settings
        .get("typoTolerance")
        .and_then(|typo| typo.get("disableOnWords"))
        .and_then(Value::as_array)
        .is_some_and(|words| {
            words
                .iter()
                .filter_map(Value::as_str)
                .any(|word| word.eq_ignore_ascii_case(query_token))
        })
}

fn allowed_typos(settings: &Value, token: &str) -> usize {
    let one_typo = settings
        .get("typoTolerance")
        .and_then(|typo| typo.get("minWordSizeForTypos"))
        .and_then(|sizes| sizes.get("oneTypo"))
        .and_then(Value::as_u64)
        .unwrap_or(4) as usize;
    let two_typos = settings
        .get("typoTolerance")
        .and_then(|typo| typo.get("minWordSizeForTypos"))
        .and_then(|sizes| sizes.get("twoTypos"))
        .and_then(Value::as_u64)
        .unwrap_or(8) as usize;

    let len = token.chars().count();
    if len >= two_typos {
        2
    } else if len >= one_typo {
        1
    } else {
        0
    }
}

fn levenshtein_at_most(left: &str, right: &str, max_distance: usize) -> bool {
    let left_chars = left.chars().collect::<Vec<_>>();
    let right_chars = right.chars().collect::<Vec<_>>();
    if left_chars.len().abs_diff(right_chars.len()) > max_distance {
        return false;
    }

    let mut previous = (0..=right_chars.len()).collect::<Vec<_>>();
    let mut current = vec![0; right_chars.len() + 1];

    for (left_index, left_char) in left_chars.iter().enumerate() {
        current[0] = left_index + 1;
        let mut row_min = current[0];
        for (right_index, right_char) in right_chars.iter().enumerate() {
            let cost = usize::from(left_char != right_char);
            current[right_index + 1] = (previous[right_index + 1] + 1)
                .min(current[right_index] + 1)
                .min(previous[right_index] + cost);
            row_min = row_min.min(current[right_index + 1]);
        }
        if row_min > max_distance {
            return false;
        }
        std::mem::swap(&mut previous, &mut current);
    }

    previous[right_chars.len()] <= max_distance
}

fn apply_search_formatting(
    mut hit: Value,
    source: &Value,
    request: &SearchRequest,
    query: &str,
    settings: &Value,
) -> Value {
    let highlight_fields = request.attributes_to_highlight.as_deref();
    let crop_fields = request.attributes_to_crop.as_deref();
    let needs_formatted = highlight_fields.is_some() || crop_fields.is_some();
    if !needs_formatted && !request.show_matches_position {
        return hit;
    }

    let Some(source_object) = source.as_object() else {
        return hit;
    };
    let query_terms = formatting_terms(settings, query);
    let mut formatted = Map::new();
    let mut positions = Map::new();

    for (field, value) in source_object {
        if field.starts_with('_') {
            continue;
        }
        let Some(text) = value.as_str() else {
            continue;
        };

        let should_highlight = field_selected(highlight_fields, field);
        let should_crop = field_selected(crop_fields, field);
        if should_highlight || should_crop {
            let mut rendered = text.to_string();
            if should_crop {
                rendered = crop_text(
                    &rendered,
                    &query_terms,
                    request.crop_length,
                    &request.crop_marker,
                );
            }
            if should_highlight {
                rendered = highlight_text(
                    &rendered,
                    &query_terms,
                    &request.highlight_pre_tag,
                    &request.highlight_post_tag,
                );
            }
            formatted.insert(field.clone(), Value::String(rendered));
        }

        if request.show_matches_position {
            let matches = match_positions(text, &query_terms);
            if !matches.is_empty() {
                positions.insert(field.clone(), Value::Array(matches));
            }
        }
    }

    if let Some(object) = hit.as_object_mut() {
        if !formatted.is_empty() {
            object.insert("_formatted".to_string(), Value::Object(formatted));
        }
        if !positions.is_empty() {
            object.insert("_matchesPosition".to_string(), Value::Object(positions));
        }
    }

    hit
}

fn formatting_terms(settings: &Value, query: &str) -> Vec<String> {
    let mut terms = BTreeMap::new();
    for token in text_tokens(query) {
        for alternative in synonym_alternatives(settings, &token) {
            terms.insert(alternative, ());
        }
    }
    terms.into_keys().collect()
}

fn field_selected(fields: Option<&[String]>, field: &str) -> bool {
    fields.is_some_and(|fields| {
        fields
            .iter()
            .any(|selected| selected == "*" || selected == field)
    })
}

fn crop_text(text: &str, terms: &[String], crop_length: usize, marker: &str) -> String {
    if text.chars().count() <= crop_length {
        return text.to_string();
    }

    let lower = text.to_ascii_lowercase();
    let first_match = terms
        .iter()
        .filter(|term| !term.is_empty())
        .filter_map(|term| lower.find(term))
        .min()
        .unwrap_or(0);
    let half = crop_length / 2;
    let mut start = first_match.saturating_sub(half);
    while start > 0 && !text.is_char_boundary(start) {
        start -= 1;
    }
    let mut end = (start + crop_length).min(text.len());
    while end < text.len() && !text.is_char_boundary(end) {
        end += 1;
    }

    let mut cropped = String::new();
    if start > 0 {
        cropped.push_str(marker);
    }
    cropped.push_str(&text[start..end]);
    if end < text.len() {
        cropped.push_str(marker);
    }
    cropped
}

fn highlight_text(text: &str, terms: &[String], pre_tag: &str, post_tag: &str) -> String {
    let ranges = match_ranges(text, terms);
    if ranges.is_empty() {
        return text.to_string();
    }

    let mut output = String::new();
    let mut cursor = 0;
    for (start, end) in ranges {
        if start < cursor {
            continue;
        }
        output.push_str(&text[cursor..start]);
        output.push_str(pre_tag);
        output.push_str(&text[start..end]);
        output.push_str(post_tag);
        cursor = end;
    }
    output.push_str(&text[cursor..]);
    output
}

fn match_positions(text: &str, terms: &[String]) -> Vec<Value> {
    match_ranges(text, terms)
        .into_iter()
        .map(|(start, end)| json!({ "start": start, "length": end.saturating_sub(start) }))
        .collect()
}

fn match_ranges(text: &str, terms: &[String]) -> Vec<(usize, usize)> {
    let lower = text.to_ascii_lowercase();
    let mut ranges = Vec::new();
    for term in terms {
        if term.is_empty() {
            continue;
        }
        let mut offset = 0;
        while let Some(index) = lower[offset..].find(term) {
            let start = offset + index;
            let end = start + term.len();
            if text.is_char_boundary(start) && text.is_char_boundary(end) {
                ranges.push((start, end));
            }
            offset = end;
            if offset >= lower.len() {
                break;
            }
        }
    }
    ranges.sort_by(|left, right| left.0.cmp(&right.0).then_with(|| right.1.cmp(&left.1)));
    let mut merged: Vec<(usize, usize)> = Vec::new();
    for range in ranges {
        if let Some(last) = merged.last_mut()
            && range.0 <= last.1
        {
            last.1 = last.1.max(range.1);
            continue;
        }
        merged.push(range);
    }
    merged
}

fn normalize_offset_limit(offset: Option<u64>, limit: Option<u64>) -> (u64, u64) {
    let offset = offset.unwrap_or(0);
    let limit = limit.unwrap_or(DEFAULT_LIMIT).max(1).min(1000);
    (offset, limit)
}

fn normalize_search_offset_limit(
    offset: Option<u64>,
    limit: Option<u64>,
    page: Option<u64>,
    hits_per_page: Option<u64>,
) -> (u64, u64, u64) {
    let page = page.unwrap_or(1).max(1);
    let limit = limit
        .or(hits_per_page)
        .unwrap_or(DEFAULT_LIMIT)
        .max(1)
        .min(1000);

    let offset = match offset {
        Some(offset) => offset,
        None => (page.saturating_sub(1)).saturating_mul(limit),
    };

    let response_page = if limit == 0 {
        1
    } else if offset < limit {
        1
    } else {
        offset / limit + 1
    };

    (offset, limit, response_page)
}

fn paginate_values<T>(items: Vec<T>, offset: u64, limit: u64) -> Vec<T>
where
    T: Clone,
{
    let len = items.len() as u64;
    let start = usize::try_from(offset.min(len)).unwrap_or_default();
    let end = usize::try_from((offset.saturating_add(limit)).min(len)).unwrap_or_default();
    items[start..end].to_vec()
}

fn build_create_index_sql(uid: &str, primary_key: Option<&str>) -> String {
    let table = quote_identifier(uid).unwrap_or_else(|_| format!("`{uid}`"));
    let id = quote_identifier("id").unwrap_or_else(|_| "`id`".to_string());

    if let Some(primary_key) = primary_key {
        let key = quote_identifier(primary_key).unwrap_or_else(|_| format!("`{primary_key}`"));
        if key == id {
            format!("CREATE TABLE {table} ({key} TEXT)")
        } else {
            format!("CREATE TABLE {table} ({id} TEXT, {key} TEXT, PRIMARY KEY ({key}))")
        }
    } else {
        format!("CREATE TABLE {table} ({id} TEXT)")
    }
}

fn quote_identifier(identifier: &str) -> Result<String> {
    validate_identifier(identifier)?;
    Ok(format!("`{identifier}`"))
}

fn validate_identifier(identifier: &str) -> Result<()> {
    if identifier.is_empty() {
        return Err(anyhow!("identifier cannot be empty"));
    }

    if !identifier
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-')
    {
        return Err(anyhow!("invalid identifier: {identifier}"));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use serde_json::json;

    use super::*;
    use crate::sql::engine::{Engine, EngineConfig};

    #[test]
    fn parses_seed_payload_shapes() {
        let request = parse_seed_payload(json!([
            { "email": "a@example.com" },
            { "email": "b@example.com" },
        ]))
        .unwrap();
        assert_eq!(request.mode, SeedMode::Append);
        assert_eq!(request.rows.len(), 2);

        let request = parse_seed_payload(json!({
            "mode": "replace",
            "rows": { "email": "c@example.com" }
        }))
        .unwrap();
        assert_eq!(request.mode, SeedMode::Replace);
        assert_eq!(request.rows.len(), 1);

        let request = parse_seed_payload(json!({
            "rows": { "email": "d@example.com" }
        }))
        .unwrap();
        assert_eq!(request.mode, SeedMode::Append);
        assert_eq!(request.rows.len(), 1);
    }

    #[test]
    fn seed_table_extends_schema_and_replaces_rows() {
        let state = meili_state("seed-table");
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        let (status, _) = rt.block_on(seed_table(
            Path("seeded_users".to_string()),
            State(state.clone()),
            Json(json!({
                "rows": [
                    { "email": "a@example.com" },
                    { "email": "b@example.com", "score": 20 }
                ]
            })),
        ));
        assert_eq!(status, StatusCode::OK);

        let rows = state
            .engine
            .execute_sql("SELECT email, score FROM seeded_users ORDER BY email")
            .unwrap();
        assert_eq!(rows[0].rows.len(), 2);
        assert_eq!(
            rows[0].rows[1].get("score").and_then(Value::as_i64),
            Some(20)
        );

        let (status, _) = rt.block_on(seed_table(
            Path("seeded_users".to_string()),
            State(state.clone()),
            Json(json!({
                "mode": "replace",
                "rows": [
                    { "email": "c@example.com", "score": 30 }
                ]
            })),
        ));
        assert_eq!(status, StatusCode::OK);

        let rows = state
            .engine
            .execute_sql("SELECT email, score FROM seeded_users")
            .unwrap();
        assert_eq!(rows[0].rows.len(), 1);
        assert_eq!(
            rows[0].rows[0].get("email").and_then(Value::as_str),
            Some("c@example.com")
        );
    }

    #[test]
    fn meili_create_and_list_indexes() {
        let state = meili_state("indexes");
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        let (status, _) = rt.block_on(create_index(
            State(state.clone()),
            Json(CreateIndexRequest {
                uid: "books".to_string(),
                primary_key: Some("id".to_string()),
            }),
        ));
        assert_eq!(status, StatusCode::ACCEPTED);

        let response = rt.block_on(list_indexes(
            State(state.clone()),
            Query(OffsetLimitQuery::default()),
        ));
        assert_eq!(response.0["results"].as_array().unwrap().len(), 1);
        assert_eq!(response.0["results"][0]["uid"], "books");
    }

    #[test]
    fn meili_documents_can_be_indexed_and_found() {
        let state = meili_state("document-search");
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        let (create_status, _) = rt.block_on(create_index(
            State(state.clone()),
            Json(CreateIndexRequest {
                uid: "books".to_string(),
                primary_key: Some("id".to_string()),
            }),
        ));
        assert_eq!(create_status, StatusCode::ACCEPTED);

        let (add_status, add_task) = rt.block_on(add_documents(
            Path("books".to_string()),
            State(state.clone()),
            Json(json!({
                "documents": [
                    { "id": "1", "title": "Dune" },
                    { "id": "2", "title": "Foundation" }
                ]
            })),
        ));
        assert_eq!(add_status, StatusCode::ACCEPTED);
        assert!(add_task.0["taskUid"].as_u64().is_some());

        let (search_status, search) = rt.block_on(search_documents_post(
            Path("books".to_string()),
            State(state.clone()),
            Json(SearchBody {
                q: Some("Dune".to_string()),
                vector: None,
                vector_field: None,
                offset: None,
                limit: None,
                page: None,
                hits_per_page: None,
                ..Default::default()
            }),
        ));
        assert_eq!(search_status, StatusCode::OK);
        assert_eq!(search.0["hits"][0]["title"], "Dune");

        let (get_status, get_document_body) = rt.block_on(get_document(
            Path(("books".to_string(), "2".to_string())),
            State(state.clone()),
            Query(DocumentQuery::default()),
        ));
        assert_eq!(get_status, StatusCode::OK);
        assert_eq!(get_document_body.0["title"], "Foundation");
    }

    #[test]
    fn meili_search_falls_back_to_exact_string_without_vector() {
        let state = meili_state("vector-fallback");
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        let (create_status, _) = rt.block_on(create_index(
            State(state.clone()),
            Json(CreateIndexRequest {
                uid: "books".to_string(),
                primary_key: Some("id".to_string()),
            }),
        ));
        assert_eq!(create_status, StatusCode::ACCEPTED);

        state
            .engine
            .execute_sql("ALTER TABLE books ADD COLUMN embedding VECTOR(2)")
            .unwrap();

        let (add_status, _) = rt.block_on(add_documents(
            Path("books".to_string()),
            State(state.clone()),
            Json(json!({
                "documents": [
                    { "id": "1", "title": "Dune", "embedding": [0.9, 0.1] },
                    { "id": "2", "title": "Foundation", "embedding": [0.1, 0.9] },
                ]
            })),
        ));
        assert_eq!(add_status, StatusCode::ACCEPTED);

        let (search_status, search_result) = rt.block_on(search_documents_post(
            Path("books".to_string()),
            State(state.clone()),
            Json(SearchBody {
                q: Some("Dune".to_string()),
                vector: None,
                vector_field: None,
                offset: None,
                limit: None,
                page: None,
                hits_per_page: None,
                ..Default::default()
            }),
        ));
        assert_eq!(search_status, StatusCode::OK);
        assert_eq!(search_result.0["hits"][0]["title"], "Dune");

        let (vector_status, vector_search_result) = rt.block_on(search_documents_post(
            Path("books".to_string()),
            State(state.clone()),
            Json(SearchBody {
                q: None,
                vector: Some(json!([0.95, 0.05])),
                vector_field: None,
                offset: None,
                limit: None,
                page: None,
                hits_per_page: None,
                show_ranking_score: Some(true),
                ..Default::default()
            }),
        ));
        assert_eq!(vector_status, StatusCode::OK);
        let hits = vector_search_result.0["hits"].as_array().unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0]["id"], "1");
        assert!(hits[0].get("_rankingScore").is_some());
        assert!(
            hits[0]["_rankingScore"]
                .as_f64()
                .is_some_and(|score| score > 0.0)
        );
    }

    #[test]
    fn meili_search_uses_tantivy_and_sync_updates() {
        let state = meili_state("tantivy-search");
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        let (create_status, _) = rt.block_on(create_index(
            State(state.clone()),
            Json(CreateIndexRequest {
                uid: "books".to_string(),
                primary_key: Some("id".to_string()),
            }),
        ));
        assert_eq!(create_status, StatusCode::ACCEPTED);

        let (add_status, _) = rt.block_on(add_documents(
            Path("books".to_string()),
            State(state.clone()),
            Json(json!({
                "documents": [
                    { "id": "1", "title": "Sea stories", "description": "A whale migration journal" },
                    { "id": "2", "title": "Mountain notes", "description": "Granite and snow" }
                ]
            })),
        ));
        assert_eq!(add_status, StatusCode::ACCEPTED);

        let (search_status, search_result) = rt.block_on(search_documents_post(
            Path("books".to_string()),
            State(state.clone()),
            Json(SearchBody {
                q: Some("sea whale".to_string()),
                show_ranking_score: Some(true),
                ..Default::default()
            }),
        ));
        assert_eq!(search_status, StatusCode::OK);
        assert_eq!(search_result.0["hits"].as_array().unwrap().len(), 1);
        assert_eq!(search_result.0["hits"][0]["id"], "1");
        assert!(search_result.0["hits"][0]["_rankingScore"].is_number());

        let (delete_status, _) = rt.block_on(delete_document(
            Path(("books".to_string(), "1".to_string())),
            State(state.clone()),
        ));
        assert_eq!(delete_status, StatusCode::ACCEPTED);

        let (after_delete_status, after_delete) = rt.block_on(search_documents_post(
            Path("books".to_string()),
            State(state.clone()),
            Json(SearchBody {
                q: Some("sea whale".to_string()),
                ..Default::default()
            }),
        ));
        assert_eq!(after_delete_status, StatusCode::OK);
        assert_eq!(after_delete.0["hits"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn meili_search_matches_meili_paging_fields() {
        let state = meili_state("paging-behavior");
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        let (create_status, _) = rt.block_on(create_index(
            State(state.clone()),
            Json(CreateIndexRequest {
                uid: "books".to_string(),
                primary_key: Some("id".to_string()),
            }),
        ));
        assert_eq!(create_status, StatusCode::ACCEPTED);

        let (add_status, _) = rt.block_on(add_documents(
            Path("books".to_string()),
            State(state.clone()),
            Json(json!({
                "documents": [
                    { "id": "1", "title": "Dune" },
                    { "id": "2", "title": "Foundation" },
                    { "id": "3", "title": "Hyperion" },
                ]
            })),
        ));
        assert_eq!(add_status, StatusCode::ACCEPTED);

        let (search_status, search_result) = rt.block_on(search_documents_post(
            Path("books".to_string()),
            State(state.clone()),
            Json(SearchBody {
                q: None,
                vector: None,
                vector_field: None,
                offset: None,
                limit: None,
                page: Some(2),
                hits_per_page: Some(1),
                ..Default::default()
            }),
        ));
        assert_eq!(search_status, StatusCode::OK);
        assert_eq!(search_result.0["page"], 2);
        assert_eq!(search_result.0["hitsPerPage"], 1);
        assert_eq!(search_result.0["totalPages"], 3);
        assert_eq!(search_result.0["hits"][0]["id"], "2");

        let (get_search_status, get_search_result) = rt.block_on(search_documents_get(
            Path("books".to_string()),
            State(state.clone()),
            Query(DocumentsQuery {
                q: None,
                offset: None,
                limit: None,
                page: Some(2),
                hits_per_page: Some(1),
                ..Default::default()
            }),
        ));
        assert_eq!(get_search_status, StatusCode::OK);
        assert_eq!(get_search_result.0["page"], 2);
        assert_eq!(get_search_result.0["hitsPerPage"], 1);
        assert_eq!(get_search_result.0["hits"][0]["id"], "2");
    }

    #[test]
    fn meili_task_endpoints_are_available() {
        let state = meili_state("tasks");
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        let (create_status, task) = rt.block_on(create_index(
            State(state.clone()),
            Json(CreateIndexRequest {
                uid: "films".to_string(),
                primary_key: Some("id".to_string()),
            }),
        ));
        assert_eq!(create_status, StatusCode::ACCEPTED);

        let list = rt.block_on(list_tasks(
            State(state.clone()),
            Query(TaskQuery::default()),
        ));
        assert_eq!(list.0["results"].as_array().unwrap().len(), 1);

        let task_uid = task.0["taskUid"].as_u64().unwrap();
        let (status, _) = rt.block_on(get_task(Path(task_uid), State(state.clone())));
        assert_eq!(status, StatusCode::OK);
    }

    #[test]
    fn meili_settings_endpoints_are_available() {
        let state = meili_state("settings");
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        let (create_status, _) = rt.block_on(create_index(
            State(state.clone()),
            Json(CreateIndexRequest {
                uid: "books".to_string(),
                primary_key: Some("id".to_string()),
            }),
        ));
        assert_eq!(create_status, StatusCode::ACCEPTED);

        let (patch_status, _) = rt.block_on(update_settings(
            Path("books".to_string()),
            State(state.clone()),
            Json(json!({ "rankingRules": ["sort", "exactness"] })),
        ));
        assert_eq!(patch_status, StatusCode::ACCEPTED);

        let (get_status, get_settings_body) = rt.block_on(get_settings(
            Path("books".to_string()),
            State(state.clone()),
        ));
        assert_eq!(get_status, StatusCode::OK);
        assert_eq!(
            get_settings_body.0["rankingRules"],
            json!(["sort", "exactness"])
        );

        let (specific_status, specific_value) = rt.block_on(get_setting(
            Path(("books".to_string(), "searchableAttributes".to_string())),
            State(state.clone()),
        ));
        assert_eq!(specific_status, StatusCode::OK);
        assert_eq!(specific_value.0, json!(["*"]));

        let (put_status, _) = rt.block_on(update_setting_put(
            Path(("books".to_string(), "searchableAttributes".to_string())),
            State(state.clone()),
            Json(json!(["title", "author"])),
        ));
        assert_eq!(put_status, StatusCode::ACCEPTED);

        let (delete_status, _) = rt.block_on(reset_settings(
            Path("books".to_string()),
            State(state.clone()),
        ));
        assert_eq!(delete_status, StatusCode::ACCEPTED);

        let (reset_status, reset_settings_body) = rt.block_on(get_setting(
            Path(("books".to_string(), "searchableAttributes".to_string())),
            State(state.clone()),
        ));
        assert_eq!(reset_status, StatusCode::OK);
        assert_eq!(reset_settings_body.0, json!(["*"]));
    }

    #[test]
    fn meili_delete_batch_filter_endpoints_and_multi_search_work() {
        let state = meili_state("delete-and-search");
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        let (create_status, _) = rt.block_on(create_index(
            State(state.clone()),
            Json(CreateIndexRequest {
                uid: "movies".to_string(),
                primary_key: Some("id".to_string()),
            }),
        ));
        assert_eq!(create_status, StatusCode::ACCEPTED);

        let (add_status, _) = rt.block_on(add_documents(
            Path("movies".to_string()),
            State(state.clone()),
            Json(json!({
                "documents": [
                    { "id": "m1", "title": "Interstellar" },
                    { "id": "m2", "title": "Moonlight" },
                    { "id": "m3", "title": "Arrival" },
                ]
            })),
        ));
        assert_eq!(add_status, StatusCode::ACCEPTED);

        let (batch_status, batch_task) = rt.block_on(delete_documents_batch(
            Path("movies".to_string()),
            State(state.clone()),
            Json(json!({ "ids": ["m1", "m3"] })),
        ));
        assert_eq!(batch_status, StatusCode::ACCEPTED);
        assert_eq!(batch_task.0["details"]["deletedDocuments"], 2);

        let (filter_status, filter_task) = rt.block_on(delete_documents_filter(
            Path("movies".to_string()),
            State(state.clone()),
            Json(json!({ "filter": "id IN [\"m2\"]" })),
        ));
        assert_eq!(filter_status, StatusCode::ACCEPTED);
        assert_eq!(filter_task.0["details"]["deletedDocuments"], 1);

        let (post_status, post_task) = rt.block_on(delete_documents_filter(
            Path("movies".to_string()),
            State(state.clone()),
            Json(json!({ "filter": "id = \"m2\"" })),
        ));
        assert_eq!(post_status, StatusCode::ACCEPTED);
        assert_eq!(post_task.0["details"]["deletedDocuments"], 0);

        let (add_status, _) = rt.block_on(add_documents(
            Path("movies".to_string()),
            State(state.clone()),
            Json(json!({
                "documents": [
                    { "id": "m1", "title": "Space" },
                    { "id": "m2", "title": "Sea" },
                    { "id": "m3", "title": "Mountain" },
                ]
            })),
        ));
        assert_eq!(add_status, StatusCode::ACCEPTED);

        let (search_status, search_payload) = rt.block_on(multi_search(
            State(state.clone()),
            Json(json!({
                "queries": [
                    {
                        "indexUid": "movies",
                        "q": "Sea",
                        "limit": 5
                    },
                    {
                        "indexUid": "movies",
                        "q": "Space",
                        "limit": 5
                    }
                ]
            })),
        ));
        assert_eq!(search_status, StatusCode::OK);
        assert_eq!(search_payload.0["results"].as_array().unwrap().len(), 2);
        assert_eq!(search_payload.0["results"][0]["indexUid"], "movies");
        assert_eq!(search_payload.0["results"][1]["indexUid"], "movies");
    }

    #[test]
    fn meili_put_documents_is_supported() {
        let state = meili_state("put-documents");
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        let (create_status, _) = rt.block_on(create_index(
            State(state.clone()),
            Json(CreateIndexRequest {
                uid: "books".to_string(),
                primary_key: Some("id".to_string()),
            }),
        ));
        assert_eq!(create_status, StatusCode::ACCEPTED);

        let (put_status, _) = rt.block_on(add_documents(
            Path("books".to_string()),
            State(state.clone()),
            Json(json!({
                "documents": [
                    { "id": "1", "title": "The Hobbit" },
                    { "id": "2", "title": "The Silmarillion" }
                ]
            })),
        ));
        assert_eq!(put_status, StatusCode::ACCEPTED);

        let (search_status, search_result) = rt.block_on(search_documents_post(
            Path("books".to_string()),
            State(state.clone()),
            Json(SearchBody {
                q: Some("Hobbit".to_string()),
                vector: None,
                vector_field: None,
                offset: None,
                limit: None,
                page: None,
                hits_per_page: None,
                ..Default::default()
            }),
        ));
        assert_eq!(search_status, StatusCode::OK);
        assert_eq!(search_result.0["hits"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn meili_search_supports_filter_sort_and_attribute_selection() {
        let state = meili_state("search-filters");
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        let (create_status, _) = rt.block_on(create_index(
            State(state.clone()),
            Json(CreateIndexRequest {
                uid: "books".to_string(),
                primary_key: Some("id".to_string()),
            }),
        ));
        assert_eq!(create_status, StatusCode::ACCEPTED);

        let (add_status, _) = rt.block_on(add_documents(
            Path("books".to_string()),
            State(state.clone()),
            Json(json!({
                "documents": [
                    { "id": "1", "title": "Alpha", "rating": 10 },
                    { "id": "2", "title": "Beta", "rating": 8 },
                    { "id": "3", "title": "Gamma", "rating": 12 },
                ]
            })),
        ));
        assert_eq!(add_status, StatusCode::ACCEPTED);

        let (search_status, search_result) = rt.block_on(search_documents_post(
            Path("books".to_string()),
            State(state.clone()),
            Json(SearchBody {
                q: Some("A".to_string()),
                filter: Some("rating >= 9".to_string()),
                sort: Some(json!(["rating:desc"])),
                attributes_to_retrieve: Some(json!(["id", "title"])),
                show_ranking_score: Some(true),
                ..Default::default()
            }),
        ));
        assert_eq!(search_status, StatusCode::OK);
        assert_eq!(search_result.0["hits"].as_array().unwrap().len(), 2);
        assert_eq!(search_result.0["hits"][0]["id"], "3");
        assert!(search_result.0["hits"][0].get("rating").is_none());
    }

    #[test]
    fn meili_search_supports_facets_and_stats() {
        let state = meili_state("search-facets");
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        let (create_status, _) = rt.block_on(create_index(
            State(state.clone()),
            Json(CreateIndexRequest {
                uid: "books".to_string(),
                primary_key: Some("id".to_string()),
            }),
        ));
        assert_eq!(create_status, StatusCode::ACCEPTED);

        let (add_status, _) = rt.block_on(add_documents(
            Path("books".to_string()),
            State(state.clone()),
            Json(json!({
                "documents": [
                    { "id": "1", "title": "Dune", "genre": "sci-fi", "rating": 10, "tags": ["space", "classic"] },
                    { "id": "2", "title": "Foundation", "genre": "sci-fi", "rating": 8, "tags": ["space"] },
                    { "id": "3", "title": "Hamlet", "genre": "drama", "rating": 12, "tags": ["classic"] }
                ]
            })),
        ));
        assert_eq!(add_status, StatusCode::ACCEPTED);

        let (search_status, search) = rt.block_on(search_documents_post(
            Path("books".to_string()),
            State(state.clone()),
            Json(SearchBody {
                facets: Some(json!(["genre", "rating", "tags"])),
                attributes_to_retrieve: Some(json!(["id"])),
                ..Default::default()
            }),
        ));
        assert_eq!(search_status, StatusCode::OK);
        assert_eq!(search.0["facetDistribution"]["genre"]["sci-fi"], 2);
        assert_eq!(search.0["facetDistribution"]["genre"]["drama"], 1);
        assert_eq!(search.0["facetDistribution"]["tags"]["space"], 2);
        assert_eq!(search.0["facetDistribution"]["tags"]["classic"], 2);
        assert_eq!(search.0["facetStats"]["rating"]["min"], 8.0);
        assert_eq!(search.0["facetStats"]["rating"]["max"], 12.0);
        assert!(search.0["hits"][0].get("genre").is_none());

        let (filtered_status, filtered) = rt.block_on(search_documents_post(
            Path("books".to_string()),
            State(state.clone()),
            Json(SearchBody {
                filter: Some("genre = \"sci-fi\"".to_string()),
                facets: Some(json!(["genre", "rating"])),
                ..Default::default()
            }),
        ));
        assert_eq!(filtered_status, StatusCode::OK);
        assert_eq!(filtered.0["facetDistribution"]["genre"]["sci-fi"], 2);
        assert!(
            filtered.0["facetDistribution"]["genre"]
                .get("drama")
                .is_none()
        );
        assert_eq!(filtered.0["facetStats"]["rating"]["max"], 10.0);

        let (wildcard_status, wildcard) = rt.block_on(search_documents_post(
            Path("books".to_string()),
            State(state.clone()),
            Json(SearchBody {
                facets: Some(json!(["*"])),
                ..Default::default()
            }),
        ));
        assert_eq!(wildcard_status, StatusCode::OK);
        assert_eq!(wildcard.0["facetDistribution"]["genre"]["sci-fi"], 2);
        assert_eq!(wildcard.0["facetDistribution"]["rating"]["8"], 1);
    }

    #[test]
    fn meili_multi_search_includes_facets() {
        let state = meili_state("multi-search-facets");
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        let (create_status, _) = rt.block_on(create_index(
            State(state.clone()),
            Json(CreateIndexRequest {
                uid: "books".to_string(),
                primary_key: Some("id".to_string()),
            }),
        ));
        assert_eq!(create_status, StatusCode::ACCEPTED);

        let (add_status, _) = rt.block_on(add_documents(
            Path("books".to_string()),
            State(state.clone()),
            Json(json!({
                "documents": [
                    { "id": "1", "title": "Dune", "genre": "sci-fi" },
                    { "id": "2", "title": "Foundation", "genre": "sci-fi" }
                ]
            })),
        ));
        assert_eq!(add_status, StatusCode::ACCEPTED);

        let (search_status, search) = rt.block_on(multi_search(
            State(state.clone()),
            Json(json!({
                "queries": [
                    {
                        "indexUid": "books",
                        "facets": ["genre"]
                    }
                ]
            })),
        ));
        assert_eq!(search_status, StatusCode::OK);
        assert_eq!(
            search.0["results"][0]["facetDistribution"]["genre"]["sci-fi"],
            2
        );
    }

    #[test]
    fn meili_search_supports_attributes_to_search_on() {
        let state = meili_state("searchable-fields");
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        let (create_status, _) = rt.block_on(create_index(
            State(state.clone()),
            Json(CreateIndexRequest {
                uid: "books".to_string(),
                primary_key: Some("id".to_string()),
            }),
        ));
        assert_eq!(create_status, StatusCode::ACCEPTED);

        let (add_status, _) = rt.block_on(add_documents(
            Path("books".to_string()),
            State(state.clone()),
            Json(json!({
                "documents": [
                    { "id": "1", "title": "Title Match", "description": "Ignore this" },
                    { "id": "2", "title": "Other", "description": "search target" },
                ]
            })),
        ));
        assert_eq!(add_status, StatusCode::ACCEPTED);

        let (search_status, search_result) = rt.block_on(search_documents_post(
            Path("books".to_string()),
            State(state.clone()),
            Json(SearchBody {
                q: Some("search target".to_string()),
                attributes_to_search_on: Some(json!(["description"])),
                ..Default::default()
            }),
        ));
        assert_eq!(search_status, StatusCode::OK);
        assert_eq!(search_result.0["hits"].as_array().unwrap().len(), 1);
        assert_eq!(search_result.0["hits"][0]["id"], "2");
    }

    #[test]
    fn meili_searchable_attributes_settings_restrict_text_search_and_fallback() {
        let state = meili_state("searchable-settings");
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        let (create_status, _) = rt.block_on(create_index(
            State(state.clone()),
            Json(CreateIndexRequest {
                uid: "books".to_string(),
                primary_key: Some("id".to_string()),
            }),
        ));
        assert_eq!(create_status, StatusCode::ACCEPTED);

        let (add_status, _) = rt.block_on(add_documents(
            Path("books".to_string()),
            State(state.clone()),
            Json(json!({
                "documents": [
                    { "id": "1", "title": "Visible title", "description": "hidden needle" }
                ]
            })),
        ));
        assert_eq!(add_status, StatusCode::ACCEPTED);

        let (settings_status, _) = rt.block_on(update_setting_put(
            Path(("books".to_string(), "searchableAttributes".to_string())),
            State(state.clone()),
            Json(json!(["title"])),
        ));
        assert_eq!(settings_status, StatusCode::ACCEPTED);

        let (hidden_status, hidden) = rt.block_on(search_documents_post(
            Path("books".to_string()),
            State(state.clone()),
            Json(SearchBody {
                q: Some("hidden needle".to_string()),
                ..Default::default()
            }),
        ));
        assert_eq!(hidden_status, StatusCode::OK);
        assert_eq!(hidden.0["hits"].as_array().unwrap().len(), 0);

        let (short_status, short) = rt.block_on(search_documents_post(
            Path("books".to_string()),
            State(state.clone()),
            Json(SearchBody {
                q: Some("h".to_string()),
                ..Default::default()
            }),
        ));
        assert_eq!(short_status, StatusCode::OK);
        assert_eq!(short.0["hits"].as_array().unwrap().len(), 0);

        let (visible_status, visible) = rt.block_on(search_documents_post(
            Path("books".to_string()),
            State(state.clone()),
            Json(SearchBody {
                q: Some("visible".to_string()),
                ..Default::default()
            }),
        ));
        assert_eq!(visible_status, StatusCode::OK);
        assert_eq!(visible.0["hits"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn meili_document_patch_replace_and_delete_all_update_search_synchronously() {
        let state = meili_state("sync-mutations");
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        let (create_status, _) = rt.block_on(create_index(
            State(state.clone()),
            Json(CreateIndexRequest {
                uid: "books".to_string(),
                primary_key: Some("id".to_string()),
            }),
        ));
        assert_eq!(create_status, StatusCode::ACCEPTED);

        let (add_status, _) = rt.block_on(add_documents(
            Path("books".to_string()),
            State(state.clone()),
            Json(json!({
                "documents": [
                    { "id": "1", "title": "Old title", "description": "original body" },
                    { "id": "2", "title": "Other title", "description": "other body" }
                ]
            })),
        ));
        assert_eq!(add_status, StatusCode::ACCEPTED);

        let (patch_status, _) = rt.block_on(patch_document(
            Path(("books".to_string(), "1".to_string())),
            State(state.clone()),
            Json(json!({ "title": "Patched title" })),
        ));
        assert_eq!(patch_status, StatusCode::ACCEPTED);

        let (patched_status, patched) = rt.block_on(search_documents_post(
            Path("books".to_string()),
            State(state.clone()),
            Json(SearchBody {
                q: Some("patched".to_string()),
                ..Default::default()
            }),
        ));
        assert_eq!(patched_status, StatusCode::OK);
        assert_eq!(patched.0["hits"].as_array().unwrap().len(), 1);
        assert_eq!(patched.0["hits"][0]["id"], "1");

        let (replace_status, _) = rt.block_on(replace_document(
            Path(("books".to_string(), "1".to_string())),
            State(state.clone()),
            Json(json!({ "title": "Replacement title" })),
        ));
        assert_eq!(replace_status, StatusCode::ACCEPTED);

        let (old_status, old) = rt.block_on(search_documents_post(
            Path("books".to_string()),
            State(state.clone()),
            Json(SearchBody {
                q: Some("original".to_string()),
                ..Default::default()
            }),
        ));
        assert_eq!(old_status, StatusCode::OK);
        assert_eq!(old.0["hits"].as_array().unwrap().len(), 0);

        let (delete_status, delete_task) = rt.block_on(delete_all_documents(
            Path("books".to_string()),
            State(state.clone()),
        ));
        assert_eq!(delete_status, StatusCode::ACCEPTED);
        assert_eq!(delete_task.0["details"]["deletedDocuments"], 2);

        let (after_status, after) = rt.block_on(search_documents_post(
            Path("books".to_string()),
            State(state.clone()),
            Json(SearchBody {
                q: Some("replacement".to_string()),
                ..Default::default()
            }),
        ));
        assert_eq!(after_status, StatusCode::OK);
        assert_eq!(after.0["hits"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn meili_get_document_and_index_pagination_are_compatible() {
        let state = meili_state("document-fields-and-index-pages");
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        for uid in ["alpha", "beta", "gamma"] {
            let (status, _) = rt.block_on(create_index(
                State(state.clone()),
                Json(CreateIndexRequest {
                    uid: uid.to_string(),
                    primary_key: Some("id".to_string()),
                }),
            ));
            assert_eq!(status, StatusCode::ACCEPTED);
        }

        let page = rt.block_on(list_indexes(
            State(state.clone()),
            Query(OffsetLimitQuery {
                offset: Some(1),
                limit: Some(1),
            }),
        ));
        assert_eq!(page.0["total"], 3);
        assert_eq!(page.0["results"].as_array().unwrap().len(), 1);
        assert_eq!(page.0["results"][0]["uid"], "beta");

        let (add_status, _) = rt.block_on(add_documents(
            Path("alpha".to_string()),
            State(state.clone()),
            Json(json!({
                "documents": [
                    { "id": "1", "title": "Visible", "secret": "Hidden" }
                ]
            })),
        ));
        assert_eq!(add_status, StatusCode::ACCEPTED);

        let (get_status, document) = rt.block_on(get_document(
            Path(("alpha".to_string(), "1".to_string())),
            State(state.clone()),
            Query(DocumentQuery {
                fields: Some("title".to_string()),
            }),
        ));
        assert_eq!(get_status, StatusCode::OK);
        assert_eq!(document.0["title"], "Visible");
        assert!(document.0.get("secret").is_none());
    }

    #[test]
    fn meili_get_search_supports_filter_sort_fields_and_scores() {
        let state = meili_state("get-search-compat");
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        let (create_status, _) = rt.block_on(create_index(
            State(state.clone()),
            Json(CreateIndexRequest {
                uid: "books".to_string(),
                primary_key: Some("id".to_string()),
            }),
        ));
        assert_eq!(create_status, StatusCode::ACCEPTED);

        let (add_status, _) = rt.block_on(add_documents(
            Path("books".to_string()),
            State(state.clone()),
            Json(json!({
                "documents": [
                    { "id": "1", "title": "Alpha guide", "rating": 7, "genre": "sci-fi" },
                    { "id": "2", "title": "Alpha manual", "rating": 10, "genre": "sci-fi" },
                    { "id": "3", "title": "Beta guide", "rating": 12, "genre": "drama" }
                ]
            })),
        ));
        assert_eq!(add_status, StatusCode::ACCEPTED);

        let (search_status, search) = rt.block_on(search_documents_get(
            Path("books".to_string()),
            State(state.clone()),
            Query(DocumentsQuery {
                q: Some("alpha".to_string()),
                filter: Some("genre = \"sci-fi\"".to_string()),
                sort: Some("rating:desc".to_string()),
                attributes_to_retrieve: Some("id,title".to_string()),
                show_ranking_score: Some(true),
                ..Default::default()
            }),
        ));
        assert_eq!(search_status, StatusCode::OK);
        assert_eq!(search.0["hits"].as_array().unwrap().len(), 2);
        assert_eq!(search.0["hits"][0]["id"], "2");
        assert!(search.0["hits"][0].get("rating").is_none());
        assert!(search.0["hits"][0]["_rankingScore"].is_number());
    }

    #[test]
    fn meili_filters_cover_in_not_in_ranges_and_missing_fields() {
        let state = meili_state("filter-operators");
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        let (create_status, _) = rt.block_on(create_index(
            State(state.clone()),
            Json(CreateIndexRequest {
                uid: "books".to_string(),
                primary_key: Some("id".to_string()),
            }),
        ));
        assert_eq!(create_status, StatusCode::ACCEPTED);

        let (add_status, _) = rt.block_on(add_documents(
            Path("books".to_string()),
            State(state.clone()),
            Json(json!({
                "documents": [
                    { "id": "1", "genre": "sci-fi", "rating": 7 },
                    { "id": "2", "genre": "fantasy", "rating": 10 },
                    { "id": "3", "genre": "drama", "rating": 12 },
                    { "id": "4", "rating": 3 }
                ]
            })),
        ));
        assert_eq!(add_status, StatusCode::ACCEPTED);

        let (in_status, in_result) = rt.block_on(search_documents_post(
            Path("books".to_string()),
            State(state.clone()),
            Json(SearchBody {
                filter: Some("genre IN [\"sci-fi\", \"fantasy\"] and rating >= 7".to_string()),
                sort: Some(json!(["rating:asc"])),
                ..Default::default()
            }),
        ));
        assert_eq!(in_status, StatusCode::OK);
        assert_eq!(in_result.0["hits"].as_array().unwrap().len(), 2);
        assert_eq!(in_result.0["hits"][0]["id"], "1");

        let (not_status, not_result) = rt.block_on(search_documents_post(
            Path("books".to_string()),
            State(state.clone()),
            Json(SearchBody {
                filter: Some("genre NOT IN [\"drama\"] and rating < 11".to_string()),
                sort: Some(json!(["rating:desc"])),
                ..Default::default()
            }),
        ));
        assert_eq!(not_status, StatusCode::OK);
        assert_eq!(not_result.0["hits"].as_array().unwrap().len(), 3);
        assert_eq!(not_result.0["hits"][0]["id"], "2");
        assert_eq!(not_result.0["hits"][2]["id"], "4");
    }

    #[test]
    fn meili_task_filters_support_ranges_uids_and_pagination() {
        let state = meili_state("task-filter-ranges");
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        let (create_status, create_task) = rt.block_on(create_index(
            State(state.clone()),
            Json(CreateIndexRequest {
                uid: "books".to_string(),
                primary_key: Some("id".to_string()),
            }),
        ));
        assert_eq!(create_status, StatusCode::ACCEPTED);

        let (add_status, add_task) = rt.block_on(add_documents(
            Path("books".to_string()),
            State(state.clone()),
            Json(json!({ "documents": [{ "id": "1", "title": "Dune" }] })),
        ));
        assert_eq!(add_status, StatusCode::ACCEPTED);

        let (delete_status, delete_task) = rt.block_on(delete_document(
            Path(("books".to_string(), "1".to_string())),
            State(state.clone()),
        ));
        assert_eq!(delete_status, StatusCode::ACCEPTED);

        let create_uid = create_task.0["taskUid"].as_u64().unwrap();
        let add_uid = add_task.0["taskUid"].as_u64().unwrap();
        let delete_uid = delete_task.0["taskUid"].as_u64().unwrap();

        let filtered = rt.block_on(list_tasks(
            State(state.clone()),
            Query(TaskQuery {
                from: Some(add_uid),
                to: Some(delete_uid),
                statuses: Some("succeeded".to_string()),
                index_uids: Some("books".to_string()),
                limit: Some(10),
                ..Default::default()
            }),
        ));
        assert_eq!(filtered.0["results"].as_array().unwrap().len(), 2);
        assert_eq!(filtered.0["results"][0]["taskUid"], delete_uid);
        assert_eq!(filtered.0["results"][1]["taskUid"], add_uid);

        let by_uid = rt.block_on(list_tasks(
            State(state.clone()),
            Query(TaskQuery {
                uids: Some(format!("{create_uid},{delete_uid}")),
                offset: Some(1),
                limit: Some(1),
                ..Default::default()
            }),
        ));
        assert_eq!(by_uid.0["results"].as_array().unwrap().len(), 1);
        assert_eq!(by_uid.0["results"][0]["taskUid"], create_uid);
    }

    #[test]
    fn meili_swap_indexes_keeps_search_indexes_in_sync() {
        let state = meili_state("swap-search-sync");
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        for uid in ["left", "right"] {
            let (status, _) = rt.block_on(create_index(
                State(state.clone()),
                Json(CreateIndexRequest {
                    uid: uid.to_string(),
                    primary_key: Some("id".to_string()),
                }),
            ));
            assert_eq!(status, StatusCode::ACCEPTED);
        }

        let (left_add, _) = rt.block_on(add_documents(
            Path("left".to_string()),
            State(state.clone()),
            Json(json!({ "documents": [{ "id": "1", "title": "LeftOnly" }] })),
        ));
        assert_eq!(left_add, StatusCode::ACCEPTED);

        let (right_add, _) = rt.block_on(add_documents(
            Path("right".to_string()),
            State(state.clone()),
            Json(json!({ "documents": [{ "id": "2", "title": "RightOnly" }] })),
        ));
        assert_eq!(right_add, StatusCode::ACCEPTED);

        let (swap_status, _) = rt.block_on(swap_indexes(
            State(state.clone()),
            Json(json!([{ "indexes": ["left", "right"] }])),
        ));
        assert_eq!(swap_status, StatusCode::ACCEPTED);

        let (left_search_status, left_search) = rt.block_on(search_documents_post(
            Path("left".to_string()),
            State(state.clone()),
            Json(SearchBody {
                q: Some("RightOnly".to_string()),
                ..Default::default()
            }),
        ));
        assert_eq!(left_search_status, StatusCode::OK);
        assert_eq!(left_search.0["hits"].as_array().unwrap().len(), 1);
        assert_eq!(left_search.0["hits"][0]["id"], "2");

        let (right_search_status, right_search) = rt.block_on(search_documents_post(
            Path("right".to_string()),
            State(state.clone()),
            Json(SearchBody {
                q: Some("LeftOnly".to_string()),
                ..Default::default()
            }),
        ));
        assert_eq!(right_search_status, StatusCode::OK);
        assert_eq!(right_search.0["hits"].as_array().unwrap().len(), 1);
        assert_eq!(right_search.0["hits"][0]["id"], "1");
    }

    #[test]
    fn meili_multi_search_preserves_per_query_errors() {
        let state = meili_state("multi-search-errors");
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        let (create_status, _) = rt.block_on(create_index(
            State(state.clone()),
            Json(CreateIndexRequest {
                uid: "books".to_string(),
                primary_key: Some("id".to_string()),
            }),
        ));
        assert_eq!(create_status, StatusCode::ACCEPTED);

        let (search_status, search) = rt.block_on(multi_search(
            State(state.clone()),
            Json(json!({
                "queries": [
                    { "indexUid": "books", "sort": [42] },
                    { "indexUid": "missing", "q": "anything" }
                ]
            })),
        ));
        assert_eq!(search_status, StatusCode::OK);
        assert_eq!(search.0["results"].as_array().unwrap().len(), 2);
        assert_eq!(search.0["results"][0]["code"], "invalid_payload");
        assert_eq!(search.0["results"][1]["code"], "index_not_found");
    }

    #[test]
    fn meili_task_list_filters_supports_aliases() {
        let state = meili_state("task-list-filters");
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        let (create_status, _) = rt.block_on(create_index(
            State(state.clone()),
            Json(CreateIndexRequest {
                uid: "books".to_string(),
                primary_key: Some("id".to_string()),
            }),
        ));
        assert_eq!(create_status, StatusCode::ACCEPTED);

        let single = rt.block_on(list_tasks(
            State(state.clone()),
            Query(TaskQuery {
                index_uids: Some("books".to_string()),
                task_type: Some("indexCreation".to_string()),
                ..Default::default()
            }),
        ));
        assert_eq!(single.0["results"].as_array().unwrap().len(), 1);
        assert_eq!(single.0["results"][0]["indexUid"], "books");

        let types = rt.block_on(list_tasks(
            State(state.clone()),
            Query(TaskQuery {
                types: Some("indexCreation".to_string()),
                index_uids: Some("books".to_string()),
                ..Default::default()
            }),
        ));
        assert_eq!(types.0["results"].as_array().unwrap().len(), 1);

        let status = rt.block_on(list_tasks(
            State(state.clone()),
            Query(TaskQuery {
                statuses: Some("succeeded".to_string()),
                index_uids: Some("books".to_string()),
                ..Default::default()
            }),
        ));
        assert_eq!(status.0["results"].as_array().unwrap().len(), 1);

        let wildcard = rt.block_on(list_tasks(
            State(state.clone()),
            Query(TaskQuery {
                types: Some("*".to_string()),
                uids: Some("1,2,3,9999".to_string()),
                ..Default::default()
            }),
        ));
        assert_eq!(wildcard.0["results"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn meili_fetch_documents_supports_filter_sort_and_fields() {
        let state = meili_state("fetch-compat");
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        let (create_status, _) = rt.block_on(create_index(
            State(state.clone()),
            Json(CreateIndexRequest {
                uid: "books".to_string(),
                primary_key: Some("id".to_string()),
            }),
        ));
        assert_eq!(create_status, StatusCode::ACCEPTED);

        let (add_status, _) = rt.block_on(add_documents(
            Path("books".to_string()),
            State(state.clone()),
            Json(json!({
                "documents": [
                    { "id": "1", "title": "Dune", "rating": 10, "genre": "sci-fi" },
                    { "id": "2", "title": "Foundation", "rating": 8, "genre": "sci-fi" },
                    { "id": "3", "title": "Hamlet", "rating": 12, "genre": "drama" },
                ]
            })),
        ));
        assert_eq!(add_status, StatusCode::ACCEPTED);

        let (list_status, list) = rt.block_on(list_documents(
            Path("books".to_string()),
            State(state.clone()),
            Query(DocumentsQuery {
                q: None,
                offset: None,
                limit: None,
                page: None,
                hits_per_page: None,
                filter: Some("genre = \"sci-fi\"".to_string()),
                sort: Some("rating:desc".to_string()),
                attributes_to_retrieve: Some("id,title".to_string()),
                attributes_to_search_on: None,
                show_ranking_score: None,
                show_ranking_score_details: None,
                ..Default::default()
            }),
        ));
        assert_eq!(list_status, StatusCode::OK);
        assert_eq!(list.0["results"].as_array().unwrap().len(), 2);
        assert_eq!(list.0["results"][0]["title"], "Dune");
        assert!(list.0["results"][0].get("rating").is_none());

        let (fetch_status, fetch) = rt.block_on(fetch_documents(
            Path("books".to_string()),
            State(state.clone()),
            Json(FetchDocumentsBody {
                offset: None,
                limit: Some(2),
                filter: Some("genre = \"sci-fi\"".to_string()),
                sort: Some(json!(["rating:asc"])),
                attributes_to_retrieve: Some(json!(["title"])),
            }),
        ));
        assert_eq!(fetch_status, StatusCode::OK);
        assert_eq!(fetch.0["total"], 2);
        assert_eq!(fetch.0["results"][0]["title"], "Foundation");
    }

    #[test]
    fn meili_stats_endpoints_return_distributions_and_last_update() {
        let state = meili_state("stats-compat");
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        let (create_status, _) = rt.block_on(create_index(
            State(state.clone()),
            Json(CreateIndexRequest {
                uid: "books".to_string(),
                primary_key: Some("id".to_string()),
            }),
        ));
        assert_eq!(create_status, StatusCode::ACCEPTED);

        let (add_status, _) = rt.block_on(add_documents(
            Path("books".to_string()),
            State(state.clone()),
            Json(json!({
                "documents": [
                    { "id": "1", "title": "Dune", "genre": "sci-fi" },
                    { "id": "2", "title": "Foundation", "genre": "sci-fi" },
                ]
            })),
        ));
        assert_eq!(add_status, StatusCode::ACCEPTED);

        let (index_stats_status, index_stats) =
            rt.block_on(index_stats(Path("books".to_string()), State(state.clone())));
        assert_eq!(index_stats_status, StatusCode::OK);
        let snapshot = state.engine.snapshot();
        let rows = snapshot.rows.get("books").unwrap();
        assert!(!rows.is_empty());
        assert_eq!(index_stats.0["numberOfDocuments"], 2);
        assert_eq!(index_stats.0["fieldDistribution"]["genre"], 2);
        assert_eq!(index_stats.0["fieldDistribution"]["id"], 2);

        let (global_status, global) = rt.block_on(instance_stats(State(state.clone())));
        assert_eq!(global_status, StatusCode::OK);
        assert!(global.0.get("lastUpdate").is_some());
        assert!(global.0["indexes"]["books"].is_object());
        assert_eq!(global.0["indexes"]["books"]["numberOfDocuments"], 2);
    }

    #[test]
    fn meili_swap_indexes_swaps_tables_and_settings() {
        let state = meili_state("swap-compat");
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        let (create_left_status, _) = rt.block_on(create_index(
            State(state.clone()),
            Json(CreateIndexRequest {
                uid: "left".to_string(),
                primary_key: Some("id".to_string()),
            }),
        ));
        assert_eq!(create_left_status, StatusCode::ACCEPTED);

        let (create_right_status, _) = rt.block_on(create_index(
            State(state.clone()),
            Json(CreateIndexRequest {
                uid: "right".to_string(),
                primary_key: Some("id".to_string()),
            }),
        ));
        assert_eq!(create_right_status, StatusCode::ACCEPTED);

        let (settings_status, _) = rt.block_on(update_settings(
            Path("left".to_string()),
            State(state.clone()),
            Json(json!({ "searchableAttributes": ["title"] })),
        ));
        assert_eq!(settings_status, StatusCode::ACCEPTED);

        let (add_status, _) = rt.block_on(add_documents(
            Path("left".to_string()),
            State(state.clone()),
            Json(json!({
                "documents": [
                    { "id": "1", "title": "Dune" }
                ]
            })),
        ));
        assert_eq!(add_status, StatusCode::ACCEPTED);
        let (pre_swap_left_status, _pre_swap_left) = rt.block_on(list_documents(
            Path("left".to_string()),
            State(state.clone()),
            Query(DocumentsQuery::default()),
        ));
        assert_eq!(pre_swap_left_status, StatusCode::OK);
        let (pre_swap_right_status, _pre_swap_right) = rt.block_on(list_documents(
            Path("right".to_string()),
            State(state.clone()),
            Query(DocumentsQuery::default()),
        ));
        assert_eq!(pre_swap_right_status, StatusCode::OK);

        let (swap_status, swap_task) = rt.block_on(swap_indexes(
            State(state.clone()),
            Json(json!({
                "swaps": [
                    { "indexes": ["left", "right"] }
                ]
            })),
        ));
        assert_eq!(swap_status, StatusCode::ACCEPTED);
        assert_eq!(swap_task.0["type"], "indexSwap");

        let (right_status, right_list) = rt.block_on(list_documents(
            Path("right".to_string()),
            State(state.clone()),
            Query(DocumentsQuery::default()),
        ));
        assert_eq!(right_status, StatusCode::OK);
        assert_eq!(right_list.0["total"], 1);
        assert_eq!(right_list.0["results"][0]["id"], "1");

        let (left_status, left_list) = rt.block_on(list_documents(
            Path("left".to_string()),
            State(state.clone()),
            Query(DocumentsQuery::default()),
        ));
        assert_eq!(left_status, StatusCode::OK);
        assert_eq!(left_list.0["total"], 0);

        let (left_stats_status, _left_stats) =
            rt.block_on(index_stats(Path("left".to_string()), State(state.clone())));
        assert_eq!(left_stats_status, StatusCode::OK);

        let (left_settings_status, left_settings) = rt.block_on(get_setting(
            Path(("right".to_string(), "searchableAttributes".to_string())),
            State(state.clone()),
        ));
        assert_eq!(left_settings_status, StatusCode::OK);
        assert_eq!(left_settings.0, json!(["title"]));

        let (right_settings_status, right_settings) = rt.block_on(get_setting(
            Path(("left".to_string(), "searchableAttributes".to_string())),
            State(state.clone()),
        ));
        assert_eq!(right_settings_status, StatusCode::OK);
        assert_eq!(right_settings.0, json!(["*"]));
    }

    #[test]
    fn meili_search_uses_synonyms_and_typo_tolerance_in_fallback() {
        let state = meili_state("synonyms-typos");
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        let (create_status, _) = rt.block_on(create_index(
            State(state.clone()),
            Json(CreateIndexRequest {
                uid: "books".to_string(),
                primary_key: Some("id".to_string()),
            }),
        ));
        assert_eq!(create_status, StatusCode::ACCEPTED);

        let (add_status, _) = rt.block_on(add_documents(
            Path("books".to_string()),
            State(state.clone()),
            Json(json!({
                "documents": [
                    { "id": "1", "title": "Automobile handbook" },
                    { "id": "2", "title": "Dune" }
                ]
            })),
        ));
        assert_eq!(add_status, StatusCode::ACCEPTED);

        let (settings_status, _) = rt.block_on(update_settings(
            Path("books".to_string()),
            State(state.clone()),
            Json(json!({ "synonyms": { "car": ["automobile"] } })),
        ));
        assert_eq!(settings_status, StatusCode::ACCEPTED);

        let (synonym_status, synonym) = rt.block_on(search_documents_post(
            Path("books".to_string()),
            State(state.clone()),
            Json(SearchBody {
                q: Some("car".to_string()),
                ..Default::default()
            }),
        ));
        assert_eq!(synonym_status, StatusCode::OK);
        assert_eq!(synonym.0["hits"].as_array().unwrap().len(), 1);
        assert_eq!(synonym.0["hits"][0]["id"], "1");

        let (typo_status, typo) = rt.block_on(search_documents_post(
            Path("books".to_string()),
            State(state.clone()),
            Json(SearchBody {
                q: Some("automoblie".to_string()),
                ..Default::default()
            }),
        ));
        assert_eq!(typo_status, StatusCode::OK);
        assert_eq!(typo.0["hits"].as_array().unwrap().len(), 1);
        assert_eq!(typo.0["hits"][0]["id"], "1");
    }

    #[test]
    fn meili_search_supports_highlight_crop_and_match_positions() {
        let state = meili_state("formatting");
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        let (create_status, _) = rt.block_on(create_index(
            State(state.clone()),
            Json(CreateIndexRequest {
                uid: "books".to_string(),
                primary_key: Some("id".to_string()),
            }),
        ));
        assert_eq!(create_status, StatusCode::ACCEPTED);

        let (add_status, _) = rt.block_on(add_documents(
            Path("books".to_string()),
            State(state.clone()),
            Json(json!({
                "documents": [
                    {
                        "id": "1",
                        "title": "Dune",
                        "description": "A long desert planet story about spice, politics, and power"
                    }
                ]
            })),
        ));
        assert_eq!(add_status, StatusCode::ACCEPTED);

        let (search_status, search) = rt.block_on(search_documents_post(
            Path("books".to_string()),
            State(state.clone()),
            Json(SearchBody {
                q: Some("spice".to_string()),
                attributes_to_retrieve: Some(json!(["id", "title"])),
                attributes_to_highlight: Some(json!(["description"])),
                attributes_to_crop: Some(json!(["description"])),
                crop_length: Some(28),
                show_matches_position: Some(true),
                ..Default::default()
            }),
        ));
        assert_eq!(search_status, StatusCode::OK);
        let hit = &search.0["hits"][0];
        assert!(hit.get("description").is_none());
        assert!(
            hit["_formatted"]["description"]
                .as_str()
                .unwrap()
                .contains("<em>spice</em>")
        );
        assert!(
            hit["_matchesPosition"]["description"]
                .as_array()
                .unwrap()
                .first()
                .unwrap()["start"]
                .as_u64()
                .is_some()
        );
    }

    #[test]
    fn meili_facet_search_returns_matching_facet_hits() {
        let state = meili_state("facet-search");
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        let (create_status, _) = rt.block_on(create_index(
            State(state.clone()),
            Json(CreateIndexRequest {
                uid: "books".to_string(),
                primary_key: Some("id".to_string()),
            }),
        ));
        assert_eq!(create_status, StatusCode::ACCEPTED);

        let (add_status, _) = rt.block_on(add_documents(
            Path("books".to_string()),
            State(state.clone()),
            Json(json!({
                "documents": [
                    { "id": "1", "title": "Dune", "genre": "sci-fi" },
                    { "id": "2", "title": "Foundation", "genre": "sci-fi" },
                    { "id": "3", "title": "Hamlet", "genre": "drama" }
                ]
            })),
        ));
        assert_eq!(add_status, StatusCode::ACCEPTED);

        let (facet_status, facet) = rt.block_on(facet_search(
            Path("books".to_string()),
            State(state.clone()),
            Json(FacetSearchBody {
                facet_name: "genre".to_string(),
                facet_query: Some("sci".to_string()),
                ..Default::default()
            }),
        ));
        assert_eq!(facet_status, StatusCode::OK);
        assert_eq!(facet.0["facetHits"][0]["value"], "sci-fi");
        assert_eq!(facet.0["facetHits"][0]["count"], 2);
    }

    #[test]
    fn meili_dump_endpoints_return_status_and_snapshot_payload() {
        let state = meili_state("dumps");
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        let (create_status, _) = rt.block_on(create_index(
            State(state.clone()),
            Json(CreateIndexRequest {
                uid: "books".to_string(),
                primary_key: Some("id".to_string()),
            }),
        ));
        assert_eq!(create_status, StatusCode::ACCEPTED);

        let (dump_status, dump) = rt.block_on(create_dump(State(state.clone())));
        assert_eq!(dump_status, StatusCode::ACCEPTED);
        assert_eq!(dump.0["status"], "done");
        let uid = dump.0["uid"].as_str().unwrap().to_string();

        let (status_status, status) =
            rt.block_on(get_dump_status(Path(uid.clone()), State(state.clone())));
        assert_eq!(status_status, StatusCode::OK);
        assert_eq!(status.0["uid"], uid);
        assert!(status.0.get("snapshot").is_none());

        let (download_status, download) =
            rt.block_on(download_dump(Path(uid), State(state.clone())));
        assert_eq!(download_status, StatusCode::OK);
        assert!(download.0["snapshot"]["schemas"]["books"].is_object());
    }

    #[test]
    fn meili_webhook_crud_endpoints_are_available() {
        let state = meili_state("webhooks");
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        let (create_status, created) = rt.block_on(create_webhook(
            State(state.clone()),
            Json(WebhookRequest {
                url: Some("https://example.com/hook".to_string()),
                headers: Some(json!({ "x-test": "1" })),
                events: Some(json!(["task.succeeded"])),
                is_enabled: Some(true),
            }),
        ));
        assert_eq!(create_status, StatusCode::CREATED);
        let uid = created.0["uid"].as_str().unwrap().to_string();

        let list = rt.block_on(list_webhooks(
            State(state.clone()),
            Query(OffsetLimitQuery::default()),
        ));
        assert_eq!(list.0["total"], 1);

        let (get_status, webhook) =
            rt.block_on(get_webhook(Path(uid.clone()), State(state.clone())));
        assert_eq!(get_status, StatusCode::OK);
        assert_eq!(webhook.0["url"], "https://example.com/hook");

        let (update_status, updated) = rt.block_on(update_webhook(
            Path(uid.clone()),
            State(state.clone()),
            Json(WebhookRequest {
                url: None,
                headers: None,
                events: None,
                is_enabled: Some(false),
            }),
        ));
        assert_eq!(update_status, StatusCode::OK);
        assert_eq!(updated.0["isEnabled"], false);

        let (delete_status, _) = rt.block_on(delete_webhook(Path(uid), State(state.clone())));
        assert_eq!(delete_status, StatusCode::OK);
    }

    fn meili_state(name: &str) -> MeiliState {
        MeiliState::new(test_engine(name))
    }

    fn test_engine(name: &str) -> Arc<Engine> {
        let dir = std::env::temp_dir().join(format!(
            "my-sqweel-debug-http-{name}-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        Arc::new(
            Engine::open_with_data_dir(EngineConfig::default(), Some(&dir.to_string_lossy()))
                .unwrap(),
        )
    }
}
