use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::sync::{LazyLock, Mutex};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use cot::Error;
use cot::http::Method;
use cot::http::header::CONTENT_LENGTH;
use cot::request::Request;
use cot::response::Response;
use sqlx::PgPool;
use tower::{Layer, Service};

use crate::config::AppConfig;

const HTTP_BUCKETS: &[f64] = &[
    0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0,
];
const JOB_BUCKETS: &[f64] = &[0.1, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0, 300.0, 600.0];
const FILE_BUCKETS: &[f64] = &[
    0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 15.0, 60.0,
];

static REGISTRY: LazyLock<Registry> = LazyLock::new(Registry::default);
static ACTIVE_USERS: LazyLock<Mutex<HashMap<i64, Instant>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

#[derive(Default)]
struct Registry {
    counters: Mutex<BTreeMap<MetricKey, f64>>,
    gauges: Mutex<BTreeMap<MetricKey, f64>>,
    histograms: Mutex<BTreeMap<MetricKey, HistogramState>>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct MetricKey {
    name: &'static str,
    labels: Vec<(&'static str, String)>,
}

#[derive(Debug, Clone)]
struct HistogramState {
    buckets: Vec<f64>,
    counts: Vec<u64>,
    sum: f64,
    count: u64,
}

#[derive(Debug, Clone, Copy)]
pub struct MetricsLayer;

#[derive(Debug, Clone)]
pub struct MetricsService<S> {
    inner: S,
}

impl<S> Layer<S> for MetricsLayer {
    type Service = MetricsService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        MetricsService { inner }
    }
}

impl<S> Service<Request> for MetricsService<S>
where
    S: Service<Request, Response = Response, Error = Error> + Send + 'static,
    S::Future: Send + 'static,
{
    type Response = Response;
    type Error = Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, request: Request) -> Self::Future {
        let method = request.method().clone();
        let route = known_http_route(request.uri().path()).map(str::to_owned);
        let request_bytes = request
            .headers()
            .get(CONTENT_LENGTH)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<f64>().ok())
            .unwrap_or(0.0);
        if let Some(route) = &route {
            let labels = http_labels(&method, route, "in_flight");
            REGISTRY.inc_gauge("furumusic_http_in_flight_requests", labels, 1.0);
            REGISTRY.inc_counter(
                "furumusic_http_request_body_bytes_total",
                vec![
                    ("method", method.as_str().to_owned()),
                    ("route", route.clone()),
                ],
                request_bytes,
            );
        }

        let start = Instant::now();
        let fut = self.inner.call(request);
        Box::pin(async move {
            let result = fut.await;
            let Some(route) = route else {
                return result;
            };
            let elapsed = start.elapsed().as_secs_f64();
            REGISTRY.inc_gauge(
                "furumusic_http_in_flight_requests",
                http_labels(&method, &route, "in_flight"),
                -1.0,
            );

            match result {
                Ok(response) => {
                    let status = response.status().as_u16().to_string();
                    let labels = http_labels(&method, &route, &status);
                    REGISTRY.inc_counter("furumusic_http_requests_total", labels.clone(), 1.0);
                    REGISTRY.observe_histogram(
                        "furumusic_http_request_duration_seconds",
                        labels,
                        elapsed,
                        HTTP_BUCKETS,
                    );
                    if let Some(length) = response
                        .headers()
                        .get(CONTENT_LENGTH)
                        .and_then(|value| value.to_str().ok())
                        .and_then(|value| value.parse::<f64>().ok())
                    {
                        REGISTRY.inc_counter(
                            "furumusic_http_response_body_bytes_total",
                            vec![
                                ("method", method.as_str().to_owned()),
                                ("route", route.clone()),
                                ("status", status),
                            ],
                            length,
                        );
                    }
                    Ok(response)
                }
                Err(error) => {
                    let labels = http_labels(&method, &route, "500");
                    REGISTRY.inc_counter("furumusic_http_requests_total", labels.clone(), 1.0);
                    REGISTRY.observe_histogram(
                        "furumusic_http_request_duration_seconds",
                        labels,
                        elapsed,
                        HTTP_BUCKETS,
                    );
                    Err(error)
                }
            }
        })
    }
}

pub fn record_active_user(user_id: i64) {
    let mut users = ACTIVE_USERS.lock().expect("active user lock");
    users.insert(user_id, Instant::now());
}

pub fn active_user_last_seen_ms() -> HashMap<i64, i64> {
    let users = ACTIVE_USERS.lock().expect("active user lock");
    users
        .iter()
        .map(|(user_id, last_seen)| (*user_id, last_seen.elapsed().as_millis() as i64))
        .collect()
}

pub fn record_auth_attempt(method: &'static str, outcome: &'static str, reason: &'static str) {
    REGISTRY.inc_counter(
        "furumusic_auth_login_attempts_total",
        vec![
            ("method", method.to_owned()),
            ("outcome", outcome.to_owned()),
            ("reason", reason.to_owned()),
        ],
        1.0,
    );
}

pub fn record_session_created(method: &'static str) {
    REGISTRY.inc_counter(
        "furumusic_auth_sessions_created_total",
        vec![("method", method.to_owned())],
        1.0,
    );
}

pub fn record_authorization_denied(kind: &'static str) {
    REGISTRY.inc_counter(
        "furumusic_auth_denied_total",
        vec![("kind", kind.to_owned())],
        1.0,
    );
}

pub fn record_play_history(duration_listened: Option<i32>, completed: bool) {
    REGISTRY.inc_counter(
        "furumusic_listens_total",
        vec![(
            "completed",
            if completed { "true" } else { "false" }.to_owned(),
        )],
        1.0,
    );
    if let Some(seconds) = duration_listened {
        REGISTRY.inc_counter(
            "furumusic_listened_seconds_total",
            Vec::new(),
            seconds.max(0) as f64,
        );
    }
}

pub fn record_stream_request(range: bool, bytes: u64) {
    REGISTRY.inc_counter(
        "furumusic_stream_requests_total",
        vec![("range", if range { "true" } else { "false" }.to_owned())],
        1.0,
    );
    REGISTRY.inc_counter("furumusic_stream_bytes_total", Vec::new(), bytes as f64);
}

pub fn record_agent_discover_run(outcome: &'static str, duration: Duration) {
    REGISTRY.inc_counter(
        "furumusic_agent_discover_runs_total",
        vec![("outcome", outcome.to_owned())],
        1.0,
    );
    REGISTRY.observe_histogram(
        "furumusic_agent_discover_duration_seconds",
        vec![("outcome", outcome.to_owned())],
        duration.as_secs_f64(),
        JOB_BUCKETS,
    );
}

pub fn record_agent_discover_files(
    seen: u64,
    queued: u64,
    skipped_hash: u64,
    skipped_existing: u64,
) {
    REGISTRY.inc_counter(
        "furumusic_agent_discover_files_seen_total",
        Vec::new(),
        seen as f64,
    );
    REGISTRY.inc_counter(
        "furumusic_agent_discover_files_queued_total",
        Vec::new(),
        queued as f64,
    );
    REGISTRY.inc_counter(
        "furumusic_agent_discover_files_skipped_total",
        vec![("reason", "hash_known".to_owned())],
        skipped_hash as f64,
    );
    REGISTRY.inc_counter(
        "furumusic_agent_discover_files_skipped_total",
        vec![("reason", "already_queued".to_owned())],
        skipped_existing as f64,
    );
}

pub fn record_agent_file_hash(duration: Duration, bytes: i64, outcome: &'static str) {
    REGISTRY.observe_histogram(
        "furumusic_agent_discover_hash_duration_seconds",
        vec![("outcome", outcome.to_owned())],
        duration.as_secs_f64(),
        FILE_BUCKETS,
    );
    if bytes > 0 {
        REGISTRY.inc_counter(
            "furumusic_agent_discover_file_bytes_total",
            vec![("outcome", outcome.to_owned())],
            bytes as f64,
        );
    }
}

pub fn record_agent_metadata(duration: Duration, outcome: &'static str) {
    REGISTRY.observe_histogram(
        "furumusic_agent_discover_metadata_duration_seconds",
        vec![("outcome", outcome.to_owned())],
        duration.as_secs_f64(),
        FILE_BUCKETS,
    );
}

pub fn record_agent_folder_batch(outcome: &'static str, size: usize, duration: Duration) {
    REGISTRY.inc_counter(
        "furumusic_agent_folder_batches_total",
        vec![("outcome", outcome.to_owned())],
        1.0,
    );
    REGISTRY.observe_histogram(
        "furumusic_agent_folder_batch_duration_seconds",
        vec![("outcome", outcome.to_owned())],
        duration.as_secs_f64(),
        JOB_BUCKETS,
    );
    REGISTRY.observe_histogram(
        "furumusic_agent_folder_batch_size",
        vec![("outcome", outcome.to_owned())],
        size as f64,
        &[1.0, 2.0, 5.0, 10.0, 20.0, 50.0, 100.0],
    );
}

pub fn record_agent_file_processed(outcome: &'static str, decision: &'static str) {
    REGISTRY.inc_counter(
        "furumusic_agent_files_processed_total",
        vec![
            ("outcome", outcome.to_owned()),
            ("decision", decision.to_owned()),
        ],
        1.0,
    );
}

pub fn record_agent_failed(stage: &'static str) {
    REGISTRY.inc_counter(
        "furumusic_agent_failed_total",
        vec![("stage", stage.to_owned())],
        1.0,
    );
}

pub fn observe_agent_confidence(confidence: f64) {
    REGISTRY.observe_histogram(
        "furumusic_agent_confidence",
        Vec::new(),
        confidence,
        &[0.1, 0.2, 0.4, 0.6, 0.75, 0.85, 0.95, 1.0],
    );
}

pub fn record_agent_llm(
    model: &str,
    outcome: &'static str,
    duration: Duration,
    prompt_tokens: u64,
    completion_tokens: u64,
    batch_size: usize,
    estimated_tokens: Option<u64>,
) {
    let model = normalize_model_label(model);
    REGISTRY.inc_counter(
        "furumusic_agent_llm_requests_total",
        vec![("model", model.clone()), ("outcome", outcome.to_owned())],
        1.0,
    );
    REGISTRY.observe_histogram(
        "furumusic_agent_llm_duration_seconds",
        vec![("model", model.clone()), ("outcome", outcome.to_owned())],
        duration.as_secs_f64(),
        JOB_BUCKETS,
    );
    REGISTRY.inc_counter(
        "furumusic_agent_llm_tokens_total",
        vec![("model", model.clone()), ("type", "prompt".to_owned())],
        prompt_tokens as f64,
    );
    REGISTRY.inc_counter(
        "furumusic_agent_llm_tokens_total",
        vec![("model", model.clone()), ("type", "completion".to_owned())],
        completion_tokens as f64,
    );
    REGISTRY.observe_histogram(
        "furumusic_agent_llm_batch_size",
        vec![("model", model.clone())],
        batch_size as f64,
        &[1.0, 2.0, 5.0, 10.0, 20.0, 50.0, 100.0],
    );
    if let Some(estimated) = estimated_tokens {
        REGISTRY.observe_histogram(
            "furumusic_agent_llm_context_estimated_tokens",
            vec![("model", model)],
            estimated as f64,
            &[512.0, 1024.0, 2048.0, 4096.0, 8192.0, 16384.0, 32768.0],
        );
    }
}

pub fn record_agent_llm_split(reason: &'static str) {
    REGISTRY.inc_counter(
        "furumusic_agent_llm_batch_splits_total",
        vec![("reason", reason.to_owned())],
        1.0,
    );
}

pub fn record_agent_llm_parse_failure(model: &str) {
    REGISTRY.inc_counter(
        "furumusic_agent_llm_parse_failures_total",
        vec![("model", normalize_model_label(model))],
        1.0,
    );
}

pub fn record_agent_rag(
    kind: &'static str,
    outcome: &'static str,
    duration: Duration,
    results: usize,
) {
    REGISTRY.inc_counter(
        "furumusic_agent_rag_queries_total",
        vec![("kind", kind.to_owned()), ("outcome", outcome.to_owned())],
        1.0,
    );
    REGISTRY.observe_histogram(
        "furumusic_agent_rag_duration_seconds",
        vec![("kind", kind.to_owned()), ("outcome", outcome.to_owned())],
        duration.as_secs_f64(),
        FILE_BUCKETS,
    );
    REGISTRY.observe_histogram(
        "furumusic_agent_rag_results",
        vec![("kind", kind.to_owned())],
        results as f64,
        &[0.0, 1.0, 2.0, 5.0, 10.0],
    );
}

pub fn record_agent_cover_lookup(source: &'static str, outcome: &'static str, bytes: usize) {
    REGISTRY.inc_counter(
        "furumusic_agent_cover_lookup_total",
        vec![
            ("source", source.to_owned()),
            ("outcome", outcome.to_owned()),
        ],
        1.0,
    );
    REGISTRY.inc_counter(
        "furumusic_agent_cover_bytes_total",
        vec![("source", source.to_owned())],
        bytes as f64,
    );
}

pub fn record_agent_cover_variant(
    variant: &'static str,
    outcome: &'static str,
    duration: Duration,
) {
    REGISTRY.inc_counter(
        "furumusic_agent_cover_variant_generation_total",
        vec![
            ("variant", variant.to_owned()),
            ("outcome", outcome.to_owned()),
        ],
        1.0,
    );
    REGISTRY.observe_histogram(
        "furumusic_agent_cover_variant_duration_seconds",
        vec![
            ("variant", variant.to_owned()),
            ("outcome", outcome.to_owned()),
        ],
        duration.as_secs_f64(),
        FILE_BUCKETS,
    );
}

pub fn record_scheduler_job(job: &str, trigger: &str, outcome: &'static str, duration_ms: i64) {
    let labels = vec![
        ("job", job.to_owned()),
        ("trigger", trigger.to_owned()),
        ("outcome", outcome.to_owned()),
    ];
    REGISTRY.inc_counter("furumusic_scheduler_job_runs_total", labels.clone(), 1.0);
    REGISTRY.observe_histogram(
        "furumusic_scheduler_job_duration_seconds",
        labels,
        (duration_ms.max(0) as f64) / 1000.0,
        JOB_BUCKETS,
    );
}

pub fn record_torrent_download(outcome: &'static str, selected_bytes: u64, duration: Duration) {
    REGISTRY.inc_counter(
        "furumusic_torrent_downloads_total",
        vec![("outcome", outcome.to_owned())],
        1.0,
    );
    REGISTRY.inc_counter(
        "furumusic_torrent_selected_bytes_total",
        vec![("outcome", outcome.to_owned())],
        selected_bytes as f64,
    );
    REGISTRY.observe_histogram(
        "furumusic_torrent_download_duration_seconds",
        vec![("outcome", outcome.to_owned())],
        duration.as_secs_f64(),
        JOB_BUCKETS,
    );
}

pub async fn render(pool: &PgPool, config: &AppConfig) -> String {
    let mut out = String::new();
    emit_static_gauge(
        &mut out,
        "furumusic_build_info",
        &[("version", env!("CARGO_PKG_VERSION"))],
        1.0,
    );
    render_active_users(&mut out);
    render_storage(&mut out, config);
    render_db_metrics(&mut out, pool).await;
    out.push_str(&REGISTRY.render());
    out
}

fn render_active_users(out: &mut String) {
    let mut users = ACTIVE_USERS.lock().expect("active user lock");
    let now = Instant::now();
    users.retain(|_, seen| now.duration_since(*seen) <= Duration::from_secs(3600));
    for (window, seconds) in [("5m", 300), ("15m", 900), ("1h", 3600)] {
        let count = users
            .values()
            .filter(|seen| now.duration_since(**seen) <= Duration::from_secs(seconds))
            .count();
        emit_static_gauge(
            out,
            "furumusic_active_users",
            &[("window", window)],
            count as f64,
        );
    }
}

fn render_storage(out: &mut String, config: &AppConfig) {
    for (kind, path) in [
        ("inbox", config.agent_inbox_dir.as_str()),
        ("library", config.agent_storage_dir.as_str()),
    ] {
        if let Some(usage) = disk_usage(Path::new(path.trim())) {
            emit_static_gauge(
                out,
                "furumusic_storage_free_bytes",
                &[("path_kind", kind)],
                usage.free_bytes as f64,
            );
            emit_static_gauge(
                out,
                "furumusic_storage_total_bytes",
                &[("path_kind", kind)],
                usage.total_bytes as f64,
            );
        }
    }
}

async fn render_db_metrics(out: &mut String, pool: &PgPool) {
    render_group_counts(
        out,
        pool,
        "furumusic_users_total",
        "SELECT role::text AS label, COUNT(*) AS count FROM furumusic__user GROUP BY role",
        "role",
    )
    .await;
    render_single_count(
        out,
        pool,
        "furumusic_library_tracks_total",
        "SELECT COUNT(*) FROM furumusic__track",
    )
    .await;
    render_single_count(
        out,
        pool,
        "furumusic_library_releases_total",
        "SELECT COUNT(*) FROM furumusic__release",
    )
    .await;
    render_single_count(
        out,
        pool,
        "furumusic_library_artists_total",
        "SELECT COUNT(*) FROM furumusic__artist",
    )
    .await;
    render_single_count(
        out,
        pool,
        "furumusic_library_playlists_total",
        "SELECT COUNT(*) FROM furumusic__playlist",
    )
    .await;
    render_group_counts(out, pool, "furumusic_media_files_total", "SELECT file_type::text AS label, COUNT(*) AS count FROM furumusic__media_file GROUP BY file_type", "type").await;
    render_group_sums(out, pool, "furumusic_media_file_bytes_total", "SELECT file_type::text AS label, COALESCE(SUM(file_size_bytes), 0)::bigint AS value FROM furumusic__media_file GROUP BY file_type", "type").await;
    render_group_counts(out, pool, "furumusic_agent_reviews_total", "SELECT status::text AS label, COUNT(*) AS count FROM furumusic__pending_review GROUP BY status", "status").await;
    render_group_counts(out, pool, "furumusic_agent_queue_depth", "SELECT status::text AS label, COUNT(*) AS count FROM furumusic__pending_review GROUP BY status", "status").await;
    render_group_counts(out, pool, "furumusic_scheduler_job_running", "SELECT job_name::text AS label, COUNT(*) AS count FROM furumusic__job_run WHERE status = 'running' GROUP BY job_name", "job").await;
    render_group_sums(out, pool, "furumusic_scheduler_job_enabled", "SELECT name::text AS label, (CASE WHEN enabled THEN 1 ELSE 0 END)::bigint AS value FROM furumusic__scheduled_job", "job").await;
    render_group_counts(out, pool, "furumusic_torrent_sessions_total", "SELECT status::text AS label, COUNT(*) AS count FROM furumusic__torrent_session GROUP BY status", "status").await;
    render_single_count(
        out,
        pool,
        "furumusic_play_history_total",
        "SELECT COUNT(*) FROM furumusic__play_history",
    )
    .await;
}

async fn render_single_count(out: &mut String, pool: &PgPool, metric: &'static str, sql: &str) {
    if let Ok(value) = sqlx::query_scalar::<_, i64>(sql).fetch_one(pool).await {
        emit_static_gauge(out, metric, &[], value as f64);
    }
}

async fn render_group_counts(
    out: &mut String,
    pool: &PgPool,
    metric: &'static str,
    sql: &str,
    label_name: &'static str,
) {
    if let Ok(rows) = sqlx::query_as::<_, (String, i64)>(sql)
        .fetch_all(pool)
        .await
    {
        for (label, count) in rows {
            emit_static_gauge(out, metric, &[(label_name, label.as_str())], count as f64);
        }
    }
}

async fn render_group_sums(
    out: &mut String,
    pool: &PgPool,
    metric: &'static str,
    sql: &str,
    label_name: &'static str,
) {
    if let Ok(rows) = sqlx::query_as::<_, (String, i64)>(sql)
        .fetch_all(pool)
        .await
    {
        for (label, value) in rows {
            emit_static_gauge(out, metric, &[(label_name, label.as_str())], value as f64);
        }
    }
}

impl Registry {
    fn inc_counter(&self, name: &'static str, labels: Vec<(&'static str, String)>, value: f64) {
        if value <= 0.0 {
            return;
        }
        let mut counters = self.counters.lock().expect("counter lock");
        *counters.entry(MetricKey::new(name, labels)).or_default() += value;
    }

    fn inc_gauge(&self, name: &'static str, labels: Vec<(&'static str, String)>, value: f64) {
        let mut gauges = self.gauges.lock().expect("gauge lock");
        *gauges.entry(MetricKey::new(name, labels)).or_default() += value;
    }

    fn observe_histogram(
        &self,
        name: &'static str,
        labels: Vec<(&'static str, String)>,
        value: f64,
        buckets: &[f64],
    ) {
        let mut histograms = self.histograms.lock().expect("histogram lock");
        let state = histograms
            .entry(MetricKey::new(name, labels))
            .or_insert_with(|| HistogramState {
                buckets: buckets.to_vec(),
                counts: vec![0; buckets.len()],
                sum: 0.0,
                count: 0,
            });
        for (index, bucket) in state.buckets.iter().enumerate() {
            if value <= *bucket {
                state.counts[index] += 1;
            }
        }
        state.sum += value;
        state.count += 1;
    }

    fn render(&self) -> String {
        let mut out = String::new();
        for (key, value) in self.counters.lock().expect("counter lock").iter() {
            emit_type(&mut out, key.name, "counter");
            emit_metric(&mut out, key.name, &key.labels, *value);
        }
        for (key, value) in self.gauges.lock().expect("gauge lock").iter() {
            emit_type(&mut out, key.name, "gauge");
            emit_metric(&mut out, key.name, &key.labels, (*value).max(0.0));
        }
        for (key, state) in self.histograms.lock().expect("histogram lock").iter() {
            emit_type(&mut out, key.name, "histogram");
            for (bucket, count) in state.buckets.iter().zip(state.counts.iter()) {
                let mut labels = key.labels.clone();
                labels.push(("le", bucket.to_string()));
                emit_metric(
                    &mut out,
                    &format!("{}_bucket", key.name),
                    &labels,
                    *count as f64,
                );
            }
            let mut inf_labels = key.labels.clone();
            inf_labels.push(("le", "+Inf".to_owned()));
            emit_metric(
                &mut out,
                &format!("{}_bucket", key.name),
                &inf_labels,
                state.count as f64,
            );
            emit_metric(
                &mut out,
                &format!("{}_sum", key.name),
                &key.labels,
                state.sum,
            );
            emit_metric(
                &mut out,
                &format!("{}_count", key.name),
                &key.labels,
                state.count as f64,
            );
        }
        out
    }
}

impl MetricKey {
    fn new(name: &'static str, mut labels: Vec<(&'static str, String)>) -> Self {
        labels.sort_by(|a, b| a.0.cmp(b.0));
        Self { name, labels }
    }
}

fn http_labels(method: &Method, route: &str, status: &str) -> Vec<(&'static str, String)> {
    vec![
        ("method", method.as_str().to_owned()),
        ("route", route.to_owned()),
        ("status", status.to_owned()),
    ]
}

fn known_http_route(path: &str) -> Option<&'static str> {
    let path = canonicalize_http_path(path);
    KNOWN_HTTP_ROUTES
        .iter()
        .copied()
        .find(|pattern| route_pattern_matches(pattern, &path))
}

fn canonicalize_http_path(path: &str) -> String {
    let without_trailing = path.trim_end_matches('/');
    if without_trailing.is_empty() {
        "/".to_owned()
    } else {
        without_trailing.to_owned()
    }
}

fn route_pattern_matches(pattern: &str, path: &str) -> bool {
    if pattern == "/" {
        return path == "/";
    }

    let mut pattern_segments = pattern.trim_start_matches('/').split('/');
    let mut path_segments = path.trim_start_matches('/').split('/');

    loop {
        match (pattern_segments.next(), path_segments.next()) {
            (None, None) => return true,
            (Some(pattern_segment), Some(path_segment)) => {
                if path_segment.is_empty() {
                    return false;
                }
                if is_route_param(pattern_segment) {
                    continue;
                }
                if pattern_segment != path_segment {
                    return false;
                }
            }
            _ => return false,
        }
    }
}

fn is_route_param(segment: &str) -> bool {
    segment.starts_with('{') && segment.ends_with('}')
}

const KNOWN_HTTP_ROUTES: &[&str] = &[
    // Keep this allowlist in sync with Cot route declarations. Unknown paths are
    // intentionally skipped so bot traffic cannot create high-cardinality labels.
    "/",
    "/admin",
    "/swagger",
    "/swagger/openapi.json",
    "/share/track/{id}",
    "/share/release/{id}",
    "/share/playlist/{token}",
    "/metrics",
    "/login",
    "/logout",
    "/set-lang",
    "/auth/oidc/start",
    "/auth/oidc/callback",
    "/api/me",
    "/admin/setup",
    "/admin/v2",
    "/admin/v2/api/dashboard",
    "/admin/v2/api/reviews",
    "/admin/v2/api/reviews/bulk",
    "/admin/v2/api/users",
    "/admin/v2/api/users/{id}",
    "/admin/v2/api/reviews/{id}/approve",
    "/admin/v2/api/jobs",
    "/admin/v2/api/jobs/metadata_backfill/run-options",
    "/admin/v2/api/jobs/artwork_backfill/run-options",
    "/admin/v2/api/jobs/{name}/run",
    "/admin/v2/api/settings",
    "/admin/v2/api/settings/probe",
    "/admin/v2/api/jobs/{name}/toggle",
    "/admin/v2/api/jobs/{name}/runs",
    "/admin/v2/api/jobs/{name}/runs/{run_id}",
    "/admin/v2/api/library",
    "/admin/v2/api/library/item",
    "/admin/v2/api/library/item/detail",
    "/admin/v2/api/library/item/image",
    "/admin/v2/api/library/item/upload-image",
    "/admin/v2/api/library/bulk",
    "/admin/v2/api/library/releases/merge",
    "/admin/debug",
    "/admin/settings",
    "/admin/settings/probe",
    "/admin/users",
    "/admin/users/new",
    "/admin/users/{id}/edit",
    "/admin/users/{id}/delete",
    "/admin/artists",
    "/admin/artists/new",
    "/admin/artists/{id}/edit",
    "/admin/artists/{id}/delete",
    "/admin/artists/{id}/available-covers",
    "/admin/artists/{id}/set-image",
    "/admin/artists/{id}/upload-image",
    "/admin/releases",
    "/admin/releases/new",
    "/admin/releases/{id}/edit",
    "/admin/releases/{id}/delete",
    "/admin/media-files",
    "/admin/media-files/{id}/delete",
    "/admin/jobs",
    "/admin/jobs/metadata_backfill/run-options",
    "/admin/jobs/{name}/run",
    "/admin/jobs/{name}/toggle",
    "/admin/jobs/{name}/cron",
    "/admin/jobs/{name}/runs/{run_id}",
    "/admin/jobs/{name}",
    "/admin/reviews/clear",
    "/admin/reviews/bulk",
    "/admin/reviews",
    "/admin/reviews/{id}",
    "/admin/reviews/{id}/approve",
    "/admin/reviews/{id}/reject",
    "/admin/reviews/{id}/requeue",
    "/api/player/me",
    "/api/player/lastfm/status",
    "/api/player/lastfm/connect",
    "/api/player/lastfm/callback",
    "/api/player/lastfm/disconnect",
    "/api/player/lastfm/now-playing",
    "/api/player/lastfm/scrobble",
    "/api/player/agent-queue",
    "/api/player/offline/manifest",
    "/api/player/youtube",
    "/api/player/youtube/preview",
    "/api/player/youtube/start",
    "/api/player/youtube/{id}/retry",
    "/api/player/youtube/{id}/cancel",
    "/api/player/youtube/{id}",
    "/api/player/uploads/local",
    "/api/player/uploads/local/history",
    "/api/player/uploads/local/history/{id}",
    "/api/player/torrents",
    "/api/player/torrents/session/{id}",
    "/api/player/torrents/preview",
    "/api/player/uploads/tracks",
    "/api/player/uploads/tracks/{track_id}",
    "/api/player/uploads/bulk-tracks",
    "/api/player/uploads/releases/{id}",
    "/api/player/uploads/reviews/{id}",
    "/api/player/uploads/reviews/{id}/approve",
    "/api/player/torrents/{id}/start",
    "/api/player/torrents/{id}/pause",
    "/api/player/torrents/{id}/status",
    "/api/player/artists",
    "/api/player/artists/{id}",
    "/api/player/releases/{id}",
    "/api/player/radio/{kind}/{id}",
    "/api/player/playlists",
    "/api/player/share-playlist",
    "/api/player/share-playlist/{id}",
    "/api/player/playlists/{id}",
    "/api/player/playlists/{id}/tracks",
    "/api/player/likes",
    "/api/player/likes/toggle/{track_id}",
    "/api/player/likes/release/{id}",
    "/api/player/follows",
    "/api/player/follows/toggle/{id}",
    "/api/player/stream/{track_id}",
    "/api/player/cover/{media_file_id}/{variant}",
    "/api/player/cover/{media_file_id}",
    "/api/player/devices/heartbeat",
    "/api/player/devices/poll",
    "/api/player/devices/active",
    "/api/player/devices/command",
    "/api/player/jams/users",
    "/api/player/jams",
    "/api/player/jams/join",
    "/api/player/jams/invite",
    "/api/player/jams/leave",
    "/api/player/state",
    "/api/player/history",
    "/api/player/search",
    "/api/player/tracks-by-ids",
];

#[cfg(test)]
mod tests {
    use super::known_http_route;

    #[test]
    fn known_http_route_matches_declared_dynamic_routes() {
        assert_eq!(
            known_http_route("/api/player/stream/42"),
            Some("/api/player/stream/{track_id}")
        );
        assert_eq!(
            known_http_route("/admin/jobs/metadata_backfill/runs/123"),
            Some("/admin/jobs/{name}/runs/{run_id}")
        );
        assert_eq!(
            known_http_route("/share/playlist/abcDEF123"),
            Some("/share/playlist/{token}")
        );
        assert_eq!(
            known_http_route("/share/release/42"),
            Some("/share/release/{id}")
        );
        assert_eq!(
            known_http_route("/api/player/youtube/start"),
            Some("/api/player/youtube/start")
        );
        assert_eq!(
            known_http_route("/api/player/youtube/job-42/retry"),
            Some("/api/player/youtube/{id}/retry")
        );
        assert_eq!(
            known_http_route("/api/player/youtube/job-42/cancel"),
            Some("/api/player/youtube/{id}/cancel")
        );
        assert_eq!(
            known_http_route("/api/player/uploads/local/history/upload-42"),
            Some("/api/player/uploads/local/history/{id}")
        );
    }

    #[test]
    fn known_http_route_skips_unknown_bot_paths() {
        assert_eq!(known_http_route("/wp-login.php"), None);
        assert_eq!(
            known_http_route("/api/player/not-a-real-endpoint/123"),
            None
        );
        assert_eq!(known_http_route("/static/random-bot-path.js"), None);
    }

    #[test]
    fn known_http_route_uses_stable_canonical_labels() {
        assert_eq!(known_http_route("/admin/"), Some("/admin"));
        assert_eq!(known_http_route("/login/"), Some("/login"));
    }
}

fn normalize_model_label(value: &str) -> String {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        "unknown".to_owned()
    } else {
        trimmed.chars().take(80).collect()
    }
}

fn emit_static_gauge(out: &mut String, name: &str, labels: &[(&str, &str)], value: f64) {
    emit_type(out, name, "gauge");
    let labels = labels
        .iter()
        .map(|(key, value)| (*key, (*value).to_owned()))
        .collect::<Vec<_>>();
    emit_metric(out, name, &labels, value);
}

fn emit_type(out: &mut String, name: &str, metric_type: &str) {
    let _ = (out, name, metric_type);
}

fn emit_metric(out: &mut String, name: &str, labels: &[(&str, String)], value: f64) {
    out.push_str(name);
    if !labels.is_empty() {
        out.push('{');
        for (index, (key, value)) in labels.iter().enumerate() {
            if index > 0 {
                out.push(',');
            }
            out.push_str(key);
            out.push_str("=\"");
            escape_label(out, value);
            out.push('"');
        }
        out.push('}');
    }
    out.push(' ');
    out.push_str(&format!("{value:.6}"));
    out.push('\n');
}

fn escape_label(out: &mut String, value: &str) {
    for ch in value.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            _ => out.push(ch),
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct DiskUsage {
    free_bytes: u64,
    total_bytes: u64,
}

#[cfg(windows)]
fn disk_usage(path: &Path) -> Option<DiskUsage> {
    use std::os::windows::ffi::OsStrExt;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetDiskFreeSpaceExW(
            lpDirectoryName: *const u16,
            lpFreeBytesAvailableToCaller: *mut u64,
            lpTotalNumberOfBytes: *mut u64,
            lpTotalNumberOfFreeBytes: *mut u64,
        ) -> i32;
    }

    let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
    wide.push(0);
    let mut free_available = 0_u64;
    let mut total = 0_u64;
    let mut total_free = 0_u64;
    let ok = unsafe {
        GetDiskFreeSpaceExW(
            wide.as_ptr(),
            &mut free_available,
            &mut total,
            &mut total_free,
        )
    };
    (ok != 0).then_some(DiskUsage {
        free_bytes: free_available,
        total_bytes: total,
    })
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn disk_usage(path: &Path) -> Option<DiskUsage> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    #[repr(C)]
    struct Statvfs {
        f_bsize: std::ffi::c_ulong,
        f_frsize: std::ffi::c_ulong,
        f_blocks: std::ffi::c_ulong,
        f_bfree: std::ffi::c_ulong,
        f_bavail: std::ffi::c_ulong,
        f_files: std::ffi::c_ulong,
        f_ffree: std::ffi::c_ulong,
        f_favail: std::ffi::c_ulong,
        f_fsid: std::ffi::c_ulong,
        f_flag: std::ffi::c_ulong,
        f_namemax: std::ffi::c_ulong,
        __f_spare: [std::ffi::c_int; 6],
    }

    unsafe extern "C" {
        fn statvfs(path: *const std::ffi::c_char, buf: *mut Statvfs) -> std::ffi::c_int;
    }

    let c_path = CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut stat = std::mem::MaybeUninit::<Statvfs>::uninit();
    let ok = unsafe { statvfs(c_path.as_ptr(), stat.as_mut_ptr()) };
    if ok != 0 {
        return None;
    }
    let stat = unsafe { stat.assume_init() };
    let fragment_size = if stat.f_frsize > 0 {
        stat.f_frsize as u64
    } else {
        stat.f_bsize as u64
    };
    Some(DiskUsage {
        free_bytes: stat.f_bavail as u64 * fragment_size,
        total_bytes: stat.f_blocks as u64 * fragment_size,
    })
}

#[cfg(not(any(windows, target_os = "linux", target_os = "android")))]
fn disk_usage(_path: &Path) -> Option<DiskUsage> {
    None
}
