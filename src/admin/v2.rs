use std::collections::{HashMap, HashSet};
use std::path::Path;

use cot::db::{Database, Model};
use cot::html::Html;
use cot::http::StatusCode;
use cot::http::header::CONTENT_TYPE;
use cot::json::Json;
use cot::response::IntoResponse;
use cot::session::Session;
use cot::{Body, Template};
use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize};
use sqlx::{PgPool, Postgres, QueryBuilder};

use super::BUILD_INFO;
use crate::agent;
use crate::auth::{self, AuthenticatedUser, Role};
use crate::config::{AppConfig, ConfigEntry, ConfigSources, DownloadProxy};
use crate::i18n::{I18n, Translations};
use crate::scheduler::{self, JobRegistry, JobRun, ScheduledJob};

#[derive(Debug, Template)]
#[template(path = "admin/v2.html")]
struct AdminV2Template {
    t: &'static Translations,
    user_name: String,
    user_role: String,
    version: &'static str,
}

#[derive(Debug, Deserialize)]
pub(super) struct ReviewsQuery {
    pub(super) status: Option<String>,
    pub(super) search: Option<String>,
    pub(super) limit: Option<i64>,
    pub(super) offset: Option<i64>,
}

#[derive(Debug, Deserialize)]
pub(super) struct LibraryQuery {
    pub(super) kind: Option<String>,
    pub(super) search: Option<String>,
    pub(super) limit: Option<i64>,
    pub(super) offset: Option<i64>,
}

#[derive(Debug, Deserialize)]
pub(super) struct UsersQuery {
    pub(super) search: Option<String>,
    pub(super) limit: Option<i64>,
    pub(super) offset: Option<i64>,
}

#[derive(Debug, Deserialize)]
pub(super) struct BulkReviewsRequest {
    action: String,
    mode: Option<String>,
    ids: Option<Vec<i64>>,
    filter: Option<ReviewFilter>,
}

#[derive(Debug, Deserialize)]
pub(super) struct BulkLibraryRequest {
    action: String,
    kind: String,
    mode: Option<String>,
    ids: Option<Vec<i64>>,
    filter: Option<LibraryFilter>,
}

#[derive(Debug, Deserialize)]
pub(super) struct MergeReleasesRequest {
    release_ids: Vec<i64>,
    target_release_id: i64,
    title: String,
    release_type: String,
    #[serde(default, deserialize_with = "deserialize_optional_stringish")]
    year: Option<String>,
    hidden: bool,
    cover_file_id: Option<i64>,
    #[serde(default)]
    artist_ids: Vec<i64>,
    tracks: Vec<ReleaseTrackUpdateRequest>,
}

#[derive(Debug, Deserialize)]
pub struct MetadataBackfillRunRequest {
    #[serde(default = "default_true")]
    audio_bitrate: bool,
    #[serde(default = "default_true")]
    audio_sample_rate: bool,
    #[serde(default = "default_true")]
    audio_bit_depth: bool,
    #[serde(default = "default_true")]
    duration_seconds: bool,
    #[serde(default = "default_true")]
    local_genres: bool,
    #[serde(default = "default_true")]
    lastfm_tags: bool,
    #[serde(default = "default_true")]
    musicbrainz_tags: bool,
    #[serde(default)]
    overwrite: bool,
}

#[derive(Debug, Deserialize)]
pub struct ArtworkBackfillRunRequest {
    #[serde(default)]
    overwrite_existing: bool,
}

#[derive(Debug, Deserialize)]
pub(super) struct UpdateLibraryItemRequest {
    kind: String,
    id: i64,
    title: String,
    hidden: bool,
    release_type: Option<String>,
    #[serde(default, deserialize_with = "deserialize_optional_stringish")]
    year: Option<String>,
    release_id: Option<i64>,
    #[serde(default, deserialize_with = "deserialize_optional_stringish")]
    track_number: Option<String>,
    #[serde(default, deserialize_with = "deserialize_optional_stringish")]
    disc_number: Option<String>,
    artist_ids: Option<Vec<i64>>,
    release_tracks: Option<Vec<ReleaseTrackUpdateRequest>>,
}

#[derive(Debug, Deserialize)]
pub(super) struct LibraryItemDetailQuery {
    kind: String,
    id: i64,
}

#[derive(Debug, Deserialize)]
pub(super) struct TrackSearchQuery {
    search: Option<String>,
    limit: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct ReleaseTrackUpdateRequest {
    id: i64,
    #[serde(default, deserialize_with = "deserialize_optional_stringish")]
    track_number: Option<String>,
    #[serde(default, deserialize_with = "deserialize_optional_stringish")]
    disc_number: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(super) struct SetLibraryImageRequest {
    kind: String,
    id: i64,
    media_file_id: Option<i64>,
}

#[derive(Debug, Deserialize)]
pub(super) struct UploadLibraryImageRequest {
    kind: String,
    id: i64,
    data: String,
    filename: String,
    mime_type: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
struct ReviewFilter {
    status: Option<String>,
    search: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema)]
struct LibraryFilter {
    search: Option<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
struct AdminUserDto {
    id: i64,
    name: String,
    role: String,
}

#[derive(Debug, Serialize, JsonSchema)]
struct BuildDto {
    package: &'static str,
    version: &'static str,
    profile: &'static str,
    target: &'static str,
    rustc_version: &'static str,
}

#[derive(Debug, Serialize, JsonSchema)]
struct AdminDashboardDto {
    user: AdminUserDto,
    build: BuildDto,
    stats: OverviewStatsDto,
    runtime: RuntimeOverviewDto,
    reviews: ReviewPageDto,
    jobs: Vec<JobDto>,
    recent_runs: Vec<JobRunDto>,
    library: LibraryOverviewDto,
}

#[derive(Debug, Serialize, JsonSchema)]
struct AdminUsersPageDto {
    items: Vec<AdminUserRowDto>,
    total: i64,
    limit: i64,
    offset: i64,
    search: Option<String>,
    online_count: i64,
}

#[derive(Debug, Serialize, JsonSchema)]
struct AdminUserRowDto {
    id: i64,
    username: String,
    display_name: Option<String>,
    email: Option<String>,
    role: String,
    is_active: bool,
    is_online: bool,
    last_seen_ms: Option<i64>,
}

#[derive(Debug, Serialize, JsonSchema)]
struct AdminUserDetailDto {
    user: AdminUserRowDto,
    stats: AdminUserStatsDto,
    recent_plays: Vec<AdminUserPlayDto>,
}

#[derive(Debug, Serialize, JsonSchema)]
struct AdminUserStatsDto {
    plays: i64,
    completed_plays: i64,
    listened_seconds: i64,
    liked_tracks: i64,
    followed_artists: i64,
    own_playlists: i64,
    saved_playlists: i64,
    uploaded_tracks: i64,
    torrent_sessions: i64,
    lastfm_connected: bool,
}

#[derive(Debug, Serialize, JsonSchema)]
struct AdminUserPlayDto {
    history_id: i64,
    played_at: String,
    duration_listened: Option<i32>,
    completed: bool,
    track_id: i64,
    title: String,
    artists: String,
    release_id: i64,
    release_title: String,
    release_year: Option<i32>,
    cover_url: Option<String>,
    track_duration_seconds: f64,
    uploader_name: String,
    audio_format: Option<String>,
    audio_bitrate: Option<i32>,
}

#[derive(Debug, Serialize, JsonSchema)]
struct OverviewStatsDto {
    tracks: i64,
    releases: i64,
    artists: i64,
    playlists: i64,
    hidden_tracks: i64,
    hidden_releases: i64,
    hidden_artists: i64,
}

#[derive(Debug, Serialize, JsonSchema)]
struct RuntimeOverviewDto {
    agent: AgentStatusDto,
    storage: Vec<StoragePathDto>,
    node: NodeStatsDto,
}

#[derive(Debug, Serialize, JsonSchema)]
struct AgentStatusDto {
    status: String,
    enabled: bool,
    llm_configured: bool,
    model: String,
    concurrency: u64,
}

#[derive(Debug, Serialize, JsonSchema)]
struct StoragePathDto {
    label: String,
    path: String,
    exists: bool,
    free_bytes: Option<u64>,
    total_bytes: Option<u64>,
}

#[derive(Debug, Serialize, JsonSchema)]
struct NodeStatsDto {
    hostname: String,
    os: &'static str,
    arch: &'static str,
    pid: u32,
    cpu_count: usize,
}

#[derive(Debug, Serialize, JsonSchema)]
struct StatusCountDto {
    status: String,
    count: i64,
}

#[derive(Debug, Serialize, JsonSchema)]
struct TagDto {
    label: String,
    kind: String,
}

#[derive(Debug, Serialize, JsonSchema)]
struct MetadataTagDto {
    name: String,
    source: String,
    weight: f64,
    updated_at: String,
}

#[derive(Debug, Serialize, JsonSchema)]
struct ReviewPageDto {
    items: Vec<ReviewDto>,
    total: i64,
    total_all: i64,
    limit: i64,
    offset: i64,
    status: Option<String>,
    search: Option<String>,
    status_counts: Vec<StatusCountDto>,
}

#[derive(Debug, Serialize, JsonSchema)]
struct ReviewDto {
    id: i64,
    job_run_id: i64,
    review_type: String,
    input_path: String,
    display_path: String,
    filename: String,
    status: String,
    confidence: Option<f64>,
    model_name: Option<String>,
    llm_duration_ms: Option<i64>,
    token_count: Option<i64>,
    tags: Vec<TagDto>,
    error_message: Option<String>,
    normalized: ReviewEditDto,
    created_at: String,
    updated_at: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub(super) struct ReviewEditDto {
    title: String,
    artist: String,
    album: String,
    year: String,
    track_number: String,
    genre: String,
    featured_artists: String,
    release_type: String,
    notes: String,
}

#[derive(Debug, Serialize, JsonSchema)]
struct JobDto {
    name: String,
    description: String,
    cron_expression: String,
    enabled: bool,
    health: String,
    is_running: bool,
    last_run_at: Option<String>,
    next_run_at: Option<String>,
    recent_runs: Vec<JobRunDto>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
struct JobRunDto {
    id: i64,
    job_name: String,
    status: String,
    started_at: String,
    finished_at: Option<String>,
    duration_ms: Option<i64>,
    trigger: String,
    error_message: Option<String>,
    log_excerpt: String,
}

#[derive(Debug, Serialize, JsonSchema)]
struct JobRunStartedDto {
    ok: bool,
    run_id: i64,
}

#[derive(Debug, Serialize, JsonSchema)]
struct JobRunsDto {
    job_name: String,
    runs: Vec<JobRunDto>,
}

#[derive(Debug, Serialize, JsonSchema)]
struct JobRunDetailDto {
    run: JobRunDto,
    log_output: String,
}

#[derive(Debug, Serialize, JsonSchema)]
struct BulkReviewsResponse {
    ok: bool,
    affected: u64,
}

#[derive(Debug, Serialize, JsonSchema)]
struct MutationResponse {
    ok: bool,
    affected: u64,
}

#[derive(Debug, Serialize, JsonSchema)]
struct MergeReleasesResponse {
    ok: bool,
    merged_releases: u64,
    moved_tracks: u64,
    item: LibraryItemDto,
}

#[derive(Debug, Serialize, JsonSchema)]
struct AdminSettingsDto {
    values: AdminSettingsValues,
    sources: AdminSettingsSources,
    lastfm_api_key_configured: bool,
    lastfm_shared_secret_configured: bool,
    lastfm_scrobbling_configured: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
struct AdminSettingsValues {
    auth_password_enabled: bool,
    auth_sso_enabled: bool,
    oidc_button_text: String,
    oidc_issuer: String,
    oidc_client_id: String,
    oidc_client_secret: String,
    oidc_admin_groups: String,
    oidc_user_groups: String,
    swagger_enabled: bool,
    lastfm_api_key: String,
    lastfm_shared_secret: String,
    agent_enabled: bool,
    agent_inbox_dir: String,
    agent_storage_dir: String,
    agent_llm_url: String,
    agent_llm_model: String,
    agent_llm_auth: String,
    agent_confidence_threshold: String,
    agent_context_limit: String,
    agent_concurrency: String,
    #[serde(default)]
    federation_enabled: bool,
    #[serde(default)]
    federation_network_id: String,
    #[serde(default)]
    federation_save_on_listen: bool,
    #[serde(default)]
    similarity_enabled: bool,
    #[serde(default = "default_similarity_model")]
    similarity_model: String,
    #[serde(default = "default_similarity_profile")]
    similarity_profile: String,
    #[serde(default = "default_similarity_workers")]
    similarity_workers: String,
    #[serde(default = "default_true")]
    downloads_enabled: bool,
    #[serde(default = "default_true")]
    torrent_downloads_enabled: bool,
    #[serde(default = "default_true")]
    youtube_downloads_enabled: bool,
    #[serde(default)]
    download_proxies: Vec<AdminDownloadProxy>,
    #[serde(default)]
    torrent_proxy_id: String,
    #[serde(default)]
    youtube_proxy_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
struct AdminDownloadProxy {
    id: String,
    address: String,
    #[serde(default)]
    username: String,
    #[serde(default)]
    password: String,
}

impl From<DownloadProxy> for AdminDownloadProxy {
    fn from(proxy: DownloadProxy) -> Self {
        Self {
            id: proxy.id,
            address: proxy.address,
            username: proxy.username,
            password: proxy.password,
        }
    }
}

impl From<AdminDownloadProxy> for DownloadProxy {
    fn from(proxy: AdminDownloadProxy) -> Self {
        Self {
            id: proxy.id,
            address: proxy.address,
            username: proxy.username,
            password: proxy.password,
        }
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
struct AdminSettingsSources {
    auth_password_enabled: &'static str,
    auth_sso_enabled: &'static str,
    oidc_button_text: &'static str,
    oidc_issuer: &'static str,
    oidc_client_id: &'static str,
    oidc_client_secret: &'static str,
    oidc_admin_groups: &'static str,
    oidc_user_groups: &'static str,
    swagger_enabled: &'static str,
    lastfm_api_key: &'static str,
    lastfm_shared_secret: &'static str,
    agent_enabled: &'static str,
    agent_inbox_dir: &'static str,
    agent_storage_dir: &'static str,
    agent_llm_url: &'static str,
    agent_llm_model: &'static str,
    agent_llm_auth: &'static str,
    agent_confidence_threshold: &'static str,
    agent_context_limit: &'static str,
    agent_concurrency: &'static str,
    federation_enabled: &'static str,
    federation_network_id: &'static str,
    federation_save_on_listen: &'static str,
    similarity_enabled: &'static str,
    similarity_model: &'static str,
    similarity_profile: &'static str,
    similarity_workers: &'static str,
    downloads_enabled: &'static str,
    torrent_downloads_enabled: &'static str,
    youtube_downloads_enabled: &'static str,
    download_proxies: &'static str,
    torrent_proxy_id: &'static str,
    youtube_proxy_id: &'static str,
}

#[derive(Debug, Deserialize)]
pub(super) struct UpdateSettingsRequest {
    auth_password_enabled: bool,
    auth_sso_enabled: bool,
    oidc_button_text: String,
    oidc_issuer: String,
    oidc_client_id: String,
    oidc_client_secret: String,
    oidc_admin_groups: String,
    oidc_user_groups: String,
    swagger_enabled: bool,
    lastfm_api_key: String,
    lastfm_shared_secret: String,
    agent_enabled: bool,
    agent_inbox_dir: String,
    agent_storage_dir: String,
    agent_llm_url: String,
    agent_llm_model: String,
    agent_llm_auth: String,
    agent_confidence_threshold: String,
    agent_context_limit: String,
    agent_concurrency: String,
    #[serde(default)]
    federation_enabled: bool,
    #[serde(default)]
    federation_network_id: String,
    #[serde(default)]
    federation_save_on_listen: bool,
    #[serde(default)]
    similarity_enabled: bool,
    #[serde(default = "default_similarity_model")]
    similarity_model: String,
    #[serde(default = "default_similarity_profile")]
    similarity_profile: String,
    #[serde(default = "default_similarity_workers")]
    similarity_workers: String,
    #[serde(default = "default_true")]
    downloads_enabled: bool,
    #[serde(default = "default_true")]
    torrent_downloads_enabled: bool,
    #[serde(default = "default_true")]
    youtube_downloads_enabled: bool,
    #[serde(default)]
    download_proxies: Vec<AdminDownloadProxy>,
    #[serde(default)]
    torrent_proxy_id: String,
    #[serde(default)]
    youtube_proxy_id: String,
}

fn default_similarity_model() -> String {
    crate::similarity::DEFAULT_MODEL_ID.to_owned()
}

fn default_similarity_profile() -> String {
    crate::similarity::DEFAULT_PROFILE_ID.to_owned()
}

fn default_similarity_workers() -> String {
    "1".to_owned()
}

#[derive(Debug, Serialize, JsonSchema)]
struct AgentProbeDto {
    status: String,
    ok: bool,
    model_intro: String,
    model_name: String,
    prompt_tokens: Option<u32>,
    completion_tokens: Option<u32>,
    tokens_per_sec: Option<f64>,
    latency_ms: u64,
    error: String,
}

#[derive(Debug, Serialize, JsonSchema)]
struct LibraryOverviewDto {
    artists: i64,
    releases: i64,
    tracks: i64,
    playlists: i64,
}

#[derive(Debug, Serialize, JsonSchema)]
struct LibraryPageDto {
    kind: String,
    items: Vec<LibraryItemDto>,
    total: i64,
    limit: i64,
    offset: i64,
    search: Option<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
struct LibraryItemDto {
    id: i64,
    kind: String,
    title: String,
    subtitle: String,
    is_hidden: Option<bool>,
    tags: Vec<TagDto>,
    updated_at: Option<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
struct LibraryItemDetailDto {
    item: LibraryItemDto,
    title: String,
    hidden: bool,
    release_type: Option<String>,
    year: Option<i32>,
    release_id: Option<i64>,
    track_number: Option<i32>,
    disc_number: Option<i32>,
    current_image_file_id: Option<i64>,
    current_image_url: Option<String>,
    selected_artist_ids: Vec<i64>,
    artists: Vec<ArtistOptionDto>,
    releases: Vec<ReleaseOptionDto>,
    release_tracks: Vec<ReleaseTrackDto>,
    available_covers: Vec<AvailableCoverDto>,
    metadata_tags: Vec<MetadataTagDto>,
}

#[derive(Debug, Serialize, JsonSchema)]
struct ArtistOptionDto {
    id: i64,
    name: String,
}

#[derive(Debug, Serialize, JsonSchema)]
struct ReleaseOptionDto {
    id: i64,
    title: String,
    subtitle: String,
}

#[derive(Debug, Serialize, JsonSchema)]
struct ReleaseTrackDto {
    id: i64,
    title: String,
    artists: String,
    release_id: Option<i64>,
    release_title: Option<String>,
    track_number: Option<i32>,
    disc_number: Option<i32>,
    duration_seconds: f64,
    is_hidden: bool,
}

#[derive(Debug, Serialize, JsonSchema)]
struct AvailableCoverDto {
    media_file_id: i64,
    release_title: String,
    cover_url: String,
}

#[derive(Debug, sqlx::FromRow)]
struct IdRow {
    id: i64,
}

#[derive(Debug, sqlx::FromRow)]
struct CountRow {
    count: i64,
}

#[derive(Debug, sqlx::FromRow)]
struct StatusCountRow {
    status: String,
    count: i64,
}

#[derive(Debug, sqlx::FromRow)]
struct ReviewRow {
    id: i64,
    job_run_id: i64,
    review_type: String,
    input_path: Option<String>,
    context_json: Option<String>,
    result_json: Option<String>,
    status: String,
    created_at: String,
    updated_at: String,
    error_message: Option<String>,
}

#[derive(Debug, sqlx::FromRow)]
struct ReviewStatsRow {
    pending_review_id: i64,
    model_name: String,
    llm_duration_ms: i64,
    prompt_tokens: i64,
    completion_tokens: i64,
}

#[derive(Debug, Clone, sqlx::FromRow)]
struct ReviewMediaRow {
    sha256_hash: String,
    original_filename: String,
    file_size_bytes: i64,
    audio_format: Option<String>,
    audio_bitrate: Option<i32>,
    audio_sample_rate: Option<i32>,
    audio_bit_depth: Option<i32>,
}

#[derive(Debug, Clone, sqlx::FromRow)]
struct JobRunRow {
    id: i64,
    job_name: String,
    status: String,
    started_at: String,
    finished_at: Option<String>,
    duration_ms: Option<i64>,
    trigger: String,
    error_message: Option<String>,
    log_excerpt: String,
}

#[derive(Debug, sqlx::FromRow)]
struct JobRunDetailRow {
    id: i64,
    job_name: String,
    status: String,
    started_at: String,
    finished_at: Option<String>,
    duration_ms: Option<i64>,
    trigger: String,
    error_message: Option<String>,
    log_excerpt: String,
    log_output: Option<String>,
}

#[derive(Debug, sqlx::FromRow)]
struct LibraryItemRow {
    id: i64,
    title: String,
    subtitle: Option<String>,
    is_hidden: Option<bool>,
    primary_count: i64,
    secondary_count: i64,
    tertiary_count: i64,
    updated_at: Option<String>,
}

#[derive(Debug, sqlx::FromRow)]
struct ReleaseTrackRow {
    id: i64,
    title: String,
    artists: String,
    release_id: Option<i64>,
    release_title: Option<String>,
    track_number: Option<i32>,
    disc_number: Option<i32>,
    duration_seconds: f64,
    is_hidden: bool,
}

pub async fn page(admin: AuthenticatedUser, i18n: I18n) -> cot::Result<Html> {
    let template = AdminV2Template {
        t: i18n.t,
        user_name: admin.name,
        user_role: admin.role.code().to_owned(),
        version: BUILD_INFO.pkg_version,
    };
    Ok(Html::new(template.render()?))
}

pub async fn dashboard(
    session: Session,
    db: Database,
    pool: &PgPool,
    registry: &JobRegistry,
) -> cot::Result<cot::response::Response> {
    let admin = match require_admin_json(&session, &db).await {
        Ok(admin) => admin,
        Err(response) => return Ok(response),
    };

    sync_registered_jobs(&db, registry).await;
    let reviews_query = ReviewsQuery {
        status: None,
        search: None,
        limit: Some(80),
        offset: Some(0),
    };
    let (config, _) = AppConfig::load_with_db(&db).await;
    let runtime = load_runtime_overview(&config);
    let (reviews, stats, jobs, recent_runs, library) = tokio::try_join!(
        load_review_page(pool, reviews_query),
        load_overview_stats(pool),
        load_jobs(&db, pool, registry),
        load_recent_runs(pool, 5),
        load_library_overview(pool),
    )
    .map_err(|e| cot::Error::internal(e.to_string()))?;

    Json(AdminDashboardDto {
        user: AdminUserDto {
            id: admin.id,
            name: admin.name,
            role: admin.role.code().to_owned(),
        },
        build: build_dto(),
        stats,
        runtime,
        reviews,
        jobs,
        recent_runs,
        library,
    })
    .into_response()
}

pub async fn reviews(
    session: Session,
    db: Database,
    pool: &PgPool,
    query: ReviewsQuery,
) -> cot::Result<cot::response::Response> {
    if let Err(response) = require_admin_json(&session, &db).await {
        return Ok(response);
    }

    let page = load_review_page(pool, query)
        .await
        .map_err(|e| cot::Error::internal(e.to_string()))?;
    Json(page).into_response()
}

pub async fn users(
    session: Session,
    db: Database,
    pool: &PgPool,
    query: UsersQuery,
) -> cot::Result<cot::response::Response> {
    if let Err(response) = require_admin_json(&session, &db).await {
        return Ok(response);
    }

    let page = load_admin_users_page(pool, query)
        .await
        .map_err(|e| cot::Error::internal(e.to_string()))?;
    Json(page).into_response()
}

pub async fn user_detail(
    session: Session,
    db: Database,
    pool: &PgPool,
    user_id: i64,
) -> cot::Result<cot::response::Response> {
    if let Err(response) = require_admin_json(&session, &db).await {
        return Ok(response);
    }

    let detail = load_admin_user_detail(pool, user_id)
        .await
        .map_err(|e| cot::Error::internal(e.to_string()))?;
    match detail {
        Some(detail) => Json(detail).into_response(),
        None => Ok(json_error(StatusCode::NOT_FOUND, "user not found")),
    }
}

pub async fn bulk_reviews(
    session: Session,
    db: Database,
    pool: &PgPool,
    Json(body): Json<BulkReviewsRequest>,
) -> cot::Result<cot::response::Response> {
    if let Err(response) = require_admin_json(&session, &db).await {
        return Ok(response);
    }

    let action = body.action.trim();
    if action != "delete" && action != "requeue" {
        return Ok(json_error(StatusCode::BAD_REQUEST, "unknown bulk action"));
    }

    let mode = body.mode.as_deref().unwrap_or("ids");
    let affected = if mode == "filter" {
        apply_review_filter_action(pool, action, body.filter.unwrap_or_default()).await?
    } else {
        let mut ids = body.ids.unwrap_or_default();
        ids.retain(|id| *id > 0);
        ids.sort_unstable();
        ids.dedup();
        if ids.is_empty() {
            0
        } else if action == "delete" {
            crate::scheduler::PendingReview::delete_by_ids(&db, &ids)
                .await
                .map_err(|e| cot::Error::internal(e.to_string()))?;
            ids.len() as u64
        } else {
            crate::scheduler::PendingReview::requeue_by_ids(&db, &ids)
                .await
                .map_err(|e| cot::Error::internal(e.to_string()))?;
            ids.len() as u64
        }
    };

    Json(BulkReviewsResponse { ok: true, affected }).into_response()
}

pub async fn approve_review(
    session: Session,
    db: Database,
    pool: &PgPool,
    review_id: i64,
    Json(body): Json<ReviewEditDto>,
) -> cot::Result<cot::response::Response> {
    if let Err(response) = require_admin_json(&session, &db).await {
        return Ok(response);
    }

    let mut review = crate::scheduler::PendingReview::get_by_id(&db, review_id)
        .await
        .map_err(|e| cot::Error::internal(e.to_string()))?
        .ok_or_else(|| cot::Error::internal("review not found"))?;
    let normalized = normalized_from_review_edit(&body);
    let result_json = serde_json::to_string(&normalized)
        .map_err(|e| cot::Error::internal(format!("failed to serialize review fields: {e}")))?;
    review
        .set_result_json(&db, result_json)
        .await
        .map_err(|e| cot::Error::internal(e.to_string()))?;

    let context: serde_json::Value =
        serde_json::from_str(review.context_json_str()).unwrap_or_default();
    let input_path = review.input_path_str().to_owned();
    let (live_config, _) = AppConfig::load_with_db(&db).await;
    let stats = crate::scheduler::ProcessingStats::get_by_review_id(&db, review_id)
        .await
        .unwrap_or(None);
    let model_name = stats.as_ref().map(|s| s.model_name.to_string());

    match crate::jobs::inbox_process::finalize_approved(
        &db,
        pool,
        &live_config,
        &input_path,
        &normalized,
        &context,
        &live_config.agent_storage_dir,
        model_name.as_deref(),
    )
    .await
    {
        Ok(()) => {
            let _ = review.set_approved(&db).await;
            Json(serde_json::json!({ "ok": true })).into_response()
        }
        Err(error) => {
            tracing::error!(?error, "review approval failed");
            let _ = review.set_rejected(&db).await;
            Ok(json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "review approval failed",
            ))
        }
    }
}

pub async fn jobs(
    session: Session,
    db: Database,
    pool: &PgPool,
    registry: &JobRegistry,
) -> cot::Result<cot::response::Response> {
    if let Err(response) = require_admin_json(&session, &db).await {
        return Ok(response);
    }
    sync_registered_jobs(&db, registry).await;
    let jobs = load_jobs(&db, pool, registry)
        .await
        .map_err(|e| cot::Error::internal(e.to_string()))?;
    Json(jobs).into_response()
}

pub async fn settings(session: Session, db: Database) -> cot::Result<cot::response::Response> {
    if let Err(response) = require_admin_json(&session, &db).await {
        return Ok(response);
    }
    let (config, sources) = AppConfig::load_with_db(&db).await;
    Json(settings_dto(config, sources)).into_response()
}

pub async fn update_settings(
    session: Session,
    db: Database,
    Json(body): Json<UpdateSettingsRequest>,
) -> cot::Result<cot::response::Response> {
    if let Err(response) = require_admin_json(&session, &db).await {
        return Ok(response);
    }
    let similarity_model = body.similarity_model.trim().to_string();
    if crate::similarity::model_by_id(&similarity_model).is_none() {
        return Ok(json_error(
            StatusCode::BAD_REQUEST,
            "unknown similarity model",
        ));
    }
    let similarity_profile = body.similarity_profile.trim().to_string();
    if crate::similarity::profile_by_id(&similarity_profile).is_none() {
        return Ok(json_error(
            StatusCode::BAD_REQUEST,
            "unknown similarity preprocessing profile",
        ));
    }
    let similarity_workers = match body.similarity_workers.trim().parse::<u64>() {
        Ok(workers @ 1..=16) => workers,
        _ => {
            return Ok(json_error(
                StatusCode::BAD_REQUEST,
                "similarity workers must be an integer from 1 to 16",
            ));
        }
    };
    if body.download_proxies.len() > 32 {
        return Ok(json_error(
            StatusCode::BAD_REQUEST,
            "at most 32 download proxies can be saved",
        ));
    }
    let mut download_proxies = Vec::with_capacity(body.download_proxies.len());
    let mut proxy_ids = HashSet::new();
    for (index, proxy) in body.download_proxies.into_iter().enumerate() {
        let proxy = match DownloadProxy::from(proxy).normalized() {
            Ok(proxy) => proxy,
            Err(error) => {
                return Ok(json_error(
                    StatusCode::BAD_REQUEST,
                    &format!("proxy {}: {error}", index + 1),
                ));
            }
        };
        if !proxy_ids.insert(proxy.id.clone()) {
            return Ok(json_error(
                StatusCode::BAD_REQUEST,
                "download proxy ids must be unique",
            ));
        }
        download_proxies.push(proxy);
    }
    let torrent_proxy_id = body.torrent_proxy_id.trim().to_string();
    let youtube_proxy_id = body.youtube_proxy_id.trim().to_string();
    for (method, proxy_id) in [
        ("torrent", torrent_proxy_id.as_str()),
        ("YouTube", youtube_proxy_id.as_str()),
    ] {
        if !proxy_id.is_empty() && !proxy_ids.contains(proxy_id) {
            return Ok(json_error(
                StatusCode::BAD_REQUEST,
                &format!("selected {method} proxy is not in the saved proxy list"),
            ));
        }
    }
    let download_proxies_json = serde_json::to_string(&download_proxies)
        .map_err(|error| cot::Error::internal(error.to_string()))?;
    let fields = [
        (
            "auth_password_enabled",
            body.auth_password_enabled.to_string(),
        ),
        ("auth_sso_enabled", body.auth_sso_enabled.to_string()),
        ("oidc_button_text", body.oidc_button_text.trim().to_string()),
        ("oidc_issuer", body.oidc_issuer.trim().to_string()),
        ("oidc_client_id", body.oidc_client_id.trim().to_string()),
        (
            "oidc_client_secret",
            body.oidc_client_secret.trim().to_string(),
        ),
        (
            "oidc_admin_groups",
            body.oidc_admin_groups.trim().to_string(),
        ),
        ("oidc_user_groups", body.oidc_user_groups.trim().to_string()),
        ("swagger_enabled", body.swagger_enabled.to_string()),
        ("lastfm_api_key", body.lastfm_api_key.trim().to_string()),
        (
            "lastfm_shared_secret",
            body.lastfm_shared_secret.trim().to_string(),
        ),
        ("agent_enabled", body.agent_enabled.to_string()),
        ("agent_inbox_dir", body.agent_inbox_dir.trim().to_string()),
        (
            "agent_storage_dir",
            body.agent_storage_dir.trim().to_string(),
        ),
        ("agent_llm_url", body.agent_llm_url.trim().to_string()),
        ("agent_llm_model", body.agent_llm_model.trim().to_string()),
        ("agent_llm_auth", body.agent_llm_auth.trim().to_string()),
        (
            "agent_confidence_threshold",
            body.agent_confidence_threshold.trim().to_string(),
        ),
        (
            "agent_context_limit",
            body.agent_context_limit.trim().to_string(),
        ),
        (
            "agent_concurrency",
            body.agent_concurrency.trim().to_string(),
        ),
        ("federation_enabled", body.federation_enabled.to_string()),
        (
            "federation_network_id",
            body.federation_network_id.trim().to_string(),
        ),
        (
            "federation_save_on_listen",
            body.federation_save_on_listen.to_string(),
        ),
        ("similarity_enabled", body.similarity_enabled.to_string()),
        ("similarity_model", similarity_model),
        ("similarity_profile", similarity_profile),
        ("similarity_workers", similarity_workers.to_string()),
        ("downloads_enabled", body.downloads_enabled.to_string()),
        (
            "torrent_downloads_enabled",
            body.torrent_downloads_enabled.to_string(),
        ),
        (
            "youtube_downloads_enabled",
            body.youtube_downloads_enabled.to_string(),
        ),
        ("download_proxies", download_proxies_json),
        ("torrent_proxy_id", torrent_proxy_id),
        ("youtube_proxy_id", youtube_proxy_id),
    ];
    for (key, value) in fields {
        let mut entry = ConfigEntry::new(key.to_string(), value);
        entry
            .save(&db)
            .await
            .map_err(|e| cot::Error::internal(e.to_string()))?;
    }
    // Federation applies on the fly: (re)start or stop the node to match
    // the freshly saved settings — no server restart involved.
    let (fresh, _) = AppConfig::load_with_db(&db).await;
    tokio::spawn(async move {
        crate::similarity::handle().apply(&fresh);
        crate::federation::handle().apply(&fresh).await;
    });
    Json(serde_json::json!({ "ok": true })).into_response()
}

// ---------------------------------------------------------------------------
// Federation (status + manual controls)
// ---------------------------------------------------------------------------

pub async fn federation_status(
    session: Session,
    db: Database,
) -> cot::Result<cot::response::Response> {
    if let Err(response) = require_admin_json(&session, &db).await {
        return Ok(response);
    }
    Json(crate::federation::handle().status().await).into_response()
}

pub async fn similarity_status(
    session: Session,
    db: Database,
) -> cot::Result<cot::response::Response> {
    if let Err(response) = require_admin_json(&session, &db).await {
        return Ok(response);
    }
    let manager = crate::similarity::handle();
    let status = manager.status();
    let model_id = if status.model.is_empty() {
        crate::similarity::DEFAULT_MODEL_ID
    } else {
        &status.model
    };
    let profiles = crate::similarity::PROFILES
        .iter()
        .map(|profile| {
            serde_json::json!({
                "id": profile.id,
                "title": profile.title,
                "details": crate::similarity::profile_details(
                    profile.id,
                    model_id,
                ).unwrap_or_default(),
            })
        })
        .collect::<Vec<_>>();
    Json(serde_json::json!({
        "status": status,
        "models": crate::similarity::MODELS,
        "profiles": profiles,
    }))
    .into_response()
}

pub async fn similarity_clear(
    session: Session,
    db: Database,
) -> cot::Result<cot::response::Response> {
    if let Err(response) = require_admin_json(&session, &db).await {
        return Ok(response);
    }
    match crate::similarity::handle().clear().await {
        Ok(()) => Json(serde_json::json!({ "ok": true })).into_response(),
        Err(error) => Ok(json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("embedding cleanup failed: {error:#}"),
        )),
    }
}

pub async fn federation_sync(
    session: Session,
    db: Database,
) -> cot::Result<cot::response::Response> {
    if let Err(response) = require_admin_json(&session, &db).await {
        return Ok(response);
    }
    let fed = crate::federation::handle();
    if let Err(err) = fed.sync_now().await {
        return Ok(json_error(
            StatusCode::BAD_REQUEST,
            &format!("sync failed: {err:#}"),
        ));
    }
    Json(fed.status().await).into_response()
}

pub async fn federation_ticket(
    session: Session,
    db: Database,
) -> cot::Result<cot::response::Response> {
    if let Err(response) = require_admin_json(&session, &db).await {
        return Ok(response);
    }
    match crate::federation::handle().ticket().await {
        Ok(ticket) => Json(serde_json::json!({ "ticket": ticket })).into_response(),
        Err(err) => Ok(json_error(StatusCode::BAD_REQUEST, &format!("{err:#}"))),
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
pub(super) struct FederationConnectRequest {
    ticket: String,
}

pub async fn federation_connect(
    session: Session,
    db: Database,
    Json(body): Json<FederationConnectRequest>,
) -> cot::Result<cot::response::Response> {
    if let Err(response) = require_admin_json(&session, &db).await {
        return Ok(response);
    }
    match crate::federation::handle().connect(&body.ticket).await {
        Ok(peer) => Json(serde_json::json!({ "connected": peer })).into_response(),
        Err(err) => Ok(json_error(StatusCode::BAD_REQUEST, &format!("{err:#}"))),
    }
}

pub async fn settings_probe(
    session: Session,
    db: Database,
) -> cot::Result<cot::response::Response> {
    if let Err(response) = require_admin_json(&session, &db).await {
        return Ok(response);
    }
    let (config, _) = AppConfig::load_with_db(&db).await;
    let probe = if config.agent_enabled && !config.agent_llm_url.is_empty() {
        agent::probe_llm(
            &config.agent_llm_url,
            &config.agent_llm_model,
            &config.agent_llm_auth,
        )
        .await
    } else {
        agent::AgentProbeResult::default()
    };
    let status = if !config.agent_enabled {
        "disabled"
    } else if config.agent_llm_url.is_empty() {
        "not_configured"
    } else if probe.ok {
        "ok"
    } else {
        "error"
    };
    Json(AgentProbeDto {
        status: status.to_string(),
        ok: probe.ok,
        model_intro: probe.model_intro,
        model_name: probe.model_name,
        prompt_tokens: probe.prompt_tokens,
        completion_tokens: probe.completion_tokens,
        tokens_per_sec: probe.tokens_per_sec,
        latency_ms: probe.latency_ms,
        error: probe.error,
    })
    .into_response()
}

fn settings_dto(config: AppConfig, sources: ConfigSources) -> AdminSettingsDto {
    let download_proxies = config
        .parsed_download_proxies()
        .unwrap_or_else(|error| {
            tracing::warn!(%error, "ignoring invalid saved download proxy list");
            Vec::new()
        })
        .into_iter()
        .map(AdminDownloadProxy::from)
        .collect();
    AdminSettingsDto {
        lastfm_api_key_configured: !config.lastfm_api_key.trim().is_empty(),
        lastfm_shared_secret_configured: !config.lastfm_shared_secret.trim().is_empty(),
        lastfm_scrobbling_configured: !config.lastfm_api_key.trim().is_empty()
            && !config.lastfm_shared_secret.trim().is_empty(),
        values: AdminSettingsValues {
            auth_password_enabled: config.auth_password_enabled,
            auth_sso_enabled: config.auth_sso_enabled,
            oidc_button_text: config.oidc_button_text,
            oidc_issuer: config.oidc_issuer,
            oidc_client_id: config.oidc_client_id,
            oidc_client_secret: config.oidc_client_secret,
            oidc_admin_groups: config.oidc_admin_groups,
            oidc_user_groups: config.oidc_user_groups,
            swagger_enabled: config.swagger_enabled,
            lastfm_api_key: config.lastfm_api_key,
            lastfm_shared_secret: config.lastfm_shared_secret,
            agent_enabled: config.agent_enabled,
            agent_inbox_dir: config.agent_inbox_dir,
            agent_storage_dir: config.agent_storage_dir,
            agent_llm_url: config.agent_llm_url,
            agent_llm_model: config.agent_llm_model,
            agent_llm_auth: config.agent_llm_auth,
            agent_confidence_threshold: config.agent_confidence_threshold.to_string(),
            agent_context_limit: config.agent_context_limit.to_string(),
            agent_concurrency: config.agent_concurrency.to_string(),
            federation_enabled: config.federation_enabled,
            federation_network_id: config.federation_network_id,
            federation_save_on_listen: config.federation_save_on_listen,
            similarity_enabled: config.similarity_enabled,
            similarity_model: config.similarity_model,
            similarity_profile: config.similarity_profile,
            similarity_workers: config.similarity_workers.to_string(),
            downloads_enabled: config.downloads_enabled,
            torrent_downloads_enabled: config.torrent_downloads_enabled,
            youtube_downloads_enabled: config.youtube_downloads_enabled,
            download_proxies,
            torrent_proxy_id: config.torrent_proxy_id,
            youtube_proxy_id: config.youtube_proxy_id,
        },
        sources: AdminSettingsSources {
            auth_password_enabled: sources.auth_password_enabled.code(),
            auth_sso_enabled: sources.auth_sso_enabled.code(),
            oidc_button_text: sources.oidc_button_text.code(),
            oidc_issuer: sources.oidc_issuer.code(),
            oidc_client_id: sources.oidc_client_id.code(),
            oidc_client_secret: sources.oidc_client_secret.code(),
            oidc_admin_groups: sources.oidc_admin_groups.code(),
            oidc_user_groups: sources.oidc_user_groups.code(),
            swagger_enabled: sources.swagger_enabled.code(),
            lastfm_api_key: sources.lastfm_api_key.code(),
            lastfm_shared_secret: sources.lastfm_shared_secret.code(),
            agent_enabled: sources.agent_enabled.code(),
            agent_inbox_dir: sources.agent_inbox_dir.code(),
            agent_storage_dir: sources.agent_storage_dir.code(),
            agent_llm_url: sources.agent_llm_url.code(),
            agent_llm_model: sources.agent_llm_model.code(),
            agent_llm_auth: sources.agent_llm_auth.code(),
            agent_confidence_threshold: sources.agent_confidence_threshold.code(),
            agent_context_limit: sources.agent_context_limit.code(),
            agent_concurrency: sources.agent_concurrency.code(),
            federation_enabled: sources.federation_enabled.code(),
            federation_network_id: sources.federation_network_id.code(),
            federation_save_on_listen: sources.federation_save_on_listen.code(),
            similarity_enabled: sources.similarity_enabled.code(),
            similarity_model: sources.similarity_model.code(),
            similarity_profile: sources.similarity_profile.code(),
            similarity_workers: sources.similarity_workers.code(),
            downloads_enabled: sources.downloads_enabled.code(),
            torrent_downloads_enabled: sources.torrent_downloads_enabled.code(),
            youtube_downloads_enabled: sources.youtube_downloads_enabled.code(),
            download_proxies: sources.download_proxies.code(),
            torrent_proxy_id: sources.torrent_proxy_id.code(),
            youtube_proxy_id: sources.youtube_proxy_id.code(),
        },
    }
}

pub async fn run_job(
    session: Session,
    db: Database,
    handle_cell: &std::sync::Arc<
        tokio::sync::OnceCell<std::sync::Arc<crate::scheduler::SchedulerHandle>>,
    >,
    job_name: &str,
) -> cot::Result<cot::response::Response> {
    if let Err(response) = require_admin_json(&session, &db).await {
        return Ok(response);
    }

    let Some(handle) = handle_cell.get() else {
        return Ok(json_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "scheduler is not ready",
        ));
    };

    match std::sync::Arc::clone(handle)
        .trigger_job_now_background(job_name)
        .await
    {
        Ok(run_id) => Json(JobRunStartedDto { ok: true, run_id }).into_response(),
        Err(e) => Ok(json_error(StatusCode::BAD_REQUEST, &e.to_string())),
    }
}

pub async fn run_metadata_backfill(
    session: Session,
    db: Database,
    pool: &PgPool,
    Json(body): Json<MetadataBackfillRunRequest>,
) -> cot::Result<cot::response::Response> {
    if let Err(response) = require_admin_json(&session, &db).await {
        return Ok(response);
    }

    let options = crate::jobs::metadata_backfill::MetadataBackfillOptions {
        audio_bitrate: body.audio_bitrate,
        audio_sample_rate: body.audio_sample_rate,
        audio_bit_depth: body.audio_bit_depth,
        duration_seconds: body.duration_seconds,
        local_genres: body.local_genres,
        lastfm_tags: body.lastfm_tags,
        musicbrainz_tags: body.musicbrainz_tags,
        overwrite: body.overwrite,
    };
    if !options.any_field() {
        return Ok(json_error(
            StatusCode::BAD_REQUEST,
            "select at least one metadata field",
        ));
    }

    let mut run = JobRun::create_running(&db, "metadata_backfill", "manual")
        .await
        .map_err(|e| cot::Error::internal(format!("failed to create job run: {e}")))?;
    let run_id = run.id_val();
    let (live_config, _) = AppConfig::load_with_db(&db).await;
    let db_for_task = db.clone();
    let pool_for_task = pool.clone();

    tokio::spawn(async move {
        let start = std::time::Instant::now();
        let ctx = scheduler::JobContext {
            config: std::sync::Arc::new(live_config),
            db: db_for_task.clone(),
            pool: pool_for_task.clone(),
            run_id,
            registry: std::sync::Arc::new(JobRegistry::new()),
        };
        let mut log = scheduler::JobLog::with_live_flush(pool_for_task.clone(), run_id);
        let result =
            crate::jobs::metadata_backfill::run_with_options(&ctx, &mut log, options).await;
        let duration_ms = start.elapsed().as_millis() as i64;
        match result {
            Ok(()) => {
                let _ = run
                    .set_completed(&db_for_task, duration_ms, &log.output())
                    .await;
            }
            Err(err) => {
                let _ = run
                    .set_failed(&db_for_task, duration_ms, &log.output(), &err.to_string())
                    .await;
            }
        }
    });

    Json(JobRunStartedDto { ok: true, run_id }).into_response()
}

pub async fn run_artwork_backfill(
    session: Session,
    db: Database,
    pool: &PgPool,
    Json(body): Json<ArtworkBackfillRunRequest>,
) -> cot::Result<cot::response::Response> {
    if let Err(response) = require_admin_json(&session, &db).await {
        return Ok(response);
    }

    let options = crate::jobs::artwork_backfill::ArtworkBackfillOptions {
        overwrite_existing: body.overwrite_existing,
    };
    let mut run = JobRun::create_running(&db, "artwork_backfill", "manual")
        .await
        .map_err(|e| cot::Error::internal(format!("failed to create job run: {e}")))?;
    let run_id = run.id_val();
    let (live_config, _) = AppConfig::load_with_db(&db).await;
    let db_for_task = db.clone();
    let pool_for_task = pool.clone();

    tokio::spawn(async move {
        let start = std::time::Instant::now();
        let ctx = scheduler::JobContext {
            config: std::sync::Arc::new(live_config),
            db: db_for_task.clone(),
            pool: pool_for_task.clone(),
            run_id,
            registry: std::sync::Arc::new(JobRegistry::new()),
        };
        let mut log = scheduler::JobLog::with_live_flush(pool_for_task.clone(), run_id);
        let result = crate::jobs::artwork_backfill::run_with_options(&ctx, &mut log, options).await;
        let duration_ms = start.elapsed().as_millis() as i64;
        match result {
            Ok(()) => {
                let _ = run
                    .set_completed(&db_for_task, duration_ms, &log.output())
                    .await;
            }
            Err(err) => {
                let _ = run
                    .set_failed(&db_for_task, duration_ms, &log.output(), &err.to_string())
                    .await;
            }
        }
    });

    Json(JobRunStartedDto { ok: true, run_id }).into_response()
}

pub async fn toggle_job(
    session: Session,
    db: Database,
    handle_cell: &std::sync::Arc<
        tokio::sync::OnceCell<std::sync::Arc<crate::scheduler::SchedulerHandle>>,
    >,
    job_name: &str,
) -> cot::Result<cot::response::Response> {
    if let Err(response) = require_admin_json(&session, &db).await {
        return Ok(response);
    }

    let job = ScheduledJob::get_by_name(&db, job_name)
        .await
        .map_err(|e| cot::Error::internal(e.to_string()))?
        .ok_or_else(|| cot::Error::internal("job not found"))?;
    let Some(handle) = handle_cell.get() else {
        return Ok(json_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "scheduler is not ready",
        ));
    };
    let enabled = !job.enabled;
    if let Err(e) = handle.toggle_job(job_name, enabled).await {
        return Ok(json_error(StatusCode::BAD_REQUEST, &e.to_string()));
    }

    Json(serde_json::json!({ "ok": true, "enabled": enabled })).into_response()
}

pub async fn job_runs(
    session: Session,
    db: Database,
    pool: &PgPool,
    job_name: &str,
) -> cot::Result<cot::response::Response> {
    if let Err(response) = require_admin_json(&session, &db).await {
        return Ok(response);
    }
    let runs = load_runs_for_job(pool, job_name, 40)
        .await
        .map_err(|e| cot::Error::internal(e.to_string()))?;
    Json(JobRunsDto {
        job_name: job_name.to_owned(),
        runs,
    })
    .into_response()
}

pub async fn job_run_detail(
    session: Session,
    db: Database,
    pool: &PgPool,
    run_id: i64,
) -> cot::Result<cot::response::Response> {
    if let Err(response) = require_admin_json(&session, &db).await {
        return Ok(response);
    }
    let row = sqlx::query_as::<_, JobRunDetailRow>(
        "SELECT id, job_name::text AS job_name, status::text AS status, started_at::text AS started_at, \
                finished_at, duration_ms, trigger::text AS trigger, error_message, \
                LEFT(COALESCE(log_output, ''), 1600) AS log_excerpt, log_output \
         FROM furumusic__job_run WHERE id = $1",
    )
    .bind(run_id)
    .fetch_optional(pool)
    .await
    .map_err(|e| cot::Error::internal(e.to_string()))?;

    let Some(row) = row else {
        return Ok(json_error(StatusCode::NOT_FOUND, "run not found"));
    };

    let log_output = row.log_output.clone().unwrap_or_default();
    Json(JobRunDetailDto {
        run: row.into(),
        log_output,
    })
    .into_response()
}

pub async fn library(
    session: Session,
    db: Database,
    pool: &PgPool,
    query: LibraryQuery,
) -> cot::Result<cot::response::Response> {
    if let Err(response) = require_admin_json(&session, &db).await {
        return Ok(response);
    }
    let page = load_library_page(pool, query)
        .await
        .map_err(|e| cot::Error::internal(e.to_string()))?;
    Json(page).into_response()
}

pub async fn library_item_detail(
    session: Session,
    db: Database,
    pool: &PgPool,
    query: LibraryItemDetailQuery,
) -> cot::Result<cot::response::Response> {
    if let Err(response) = require_admin_json(&session, &db).await {
        return Ok(response);
    }
    let kind = normalize_library_kind(Some(query.kind.as_str()));
    if kind == "releases" && query.id == 0 {
        let item = LibraryItemDto {
            id: 0,
            kind: kind.clone(),
            title: String::new(),
            subtitle: String::new(),
            is_hidden: Some(false),
            tags: Vec::new(),
            updated_at: None,
        };
        let detail = load_library_item_detail(pool, &kind, item)
            .await
            .map_err(|e| cot::Error::internal(e.to_string()))?;
        return Json(detail).into_response();
    }
    let Some(item) = fetch_library_item(pool, &kind, query.id)
        .await
        .map_err(|e| cot::Error::internal(e.to_string()))?
    else {
        return Ok(json_error(StatusCode::NOT_FOUND, "library item not found"));
    };
    let detail = load_library_item_detail(pool, &kind, item)
        .await
        .map_err(|e| cot::Error::internal(e.to_string()))?;
    Json(detail).into_response()
}

pub async fn track_search(
    session: Session,
    db: Database,
    pool: &PgPool,
    query: TrackSearchQuery,
) -> cot::Result<cot::response::Response> {
    if let Err(response) = require_admin_json(&session, &db).await {
        return Ok(response);
    }

    let Some(search) = clean_search(query.search.as_deref()) else {
        return Json(Vec::<ReleaseTrackDto>::new()).into_response();
    };
    let tracks = search_tracks(pool, &search, query.limit.unwrap_or(16).clamp(1, 40))
        .await
        .map_err(|e| cot::Error::internal(e.to_string()))?;
    Json(tracks).into_response()
}

pub async fn update_library_item(
    session: Session,
    db: Database,
    pool: &PgPool,
    Json(body): Json<UpdateLibraryItemRequest>,
) -> cot::Result<cot::response::Response> {
    if let Err(response) = require_admin_json(&session, &db).await {
        return Ok(response);
    }

    let kind = normalize_library_kind(Some(body.kind.as_str()));
    let title = body.title.trim();
    if title.is_empty() {
        return Ok(json_error(StatusCode::BAD_REQUEST, "title cannot be empty"));
    }

    let now = now_string();
    if kind == "releases" && body.id == 0 {
        let release_id = create_release_library_item(pool, &body, title, &now)
            .await
            .map_err(|e| cot::Error::internal(e.to_string()))?;
        let Some(item) = fetch_library_item(pool, &kind, release_id)
            .await
            .map_err(|e| cot::Error::internal(e.to_string()))?
        else {
            return Ok(json_error(StatusCode::NOT_FOUND, "library item not found"));
        };
        return Json(item).into_response();
    }

    let affected = match kind.as_str() {
        "artists" => {
            sqlx::query(
                "UPDATE furumusic__artist \
                 SET name = $1, name_sort = $2, is_hidden = $3, updated_at = $4 \
                 WHERE id = $5",
            )
            .bind(title)
            .bind(normalize_name(title))
            .bind(body.hidden)
            .bind(&now)
            .bind(body.id)
            .execute(pool)
            .await
        }
        "releases" => {
            let release_type = body
                .release_type
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .unwrap_or("album");
            let year = body
                .year
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .and_then(|value| value.parse::<i32>().ok());
            sqlx::query(
                "UPDATE furumusic__release \
                 SET title = $1, title_sort = $2, release_type = $3, year = $4, is_hidden = $5, updated_at = $6 \
                 WHERE id = $7",
            )
            .bind(title)
            .bind(normalize_name(title))
            .bind(release_type)
            .bind(year)
            .bind(body.hidden)
            .bind(&now)
            .bind(body.id)
            .execute(pool)
            .await
        }
        "tracks" => {
            let release_id = body.release_id.unwrap_or(0);
            if release_id <= 0 {
                return Ok(json_error(StatusCode::BAD_REQUEST, "release is required"));
            }
            let release_exists: Option<i64> =
                sqlx::query_scalar("SELECT id FROM furumusic__release WHERE id = $1")
                    .bind(release_id)
                    .fetch_optional(pool)
                    .await
                    .map_err(|e| cot::Error::internal(e.to_string()))?;
            if release_exists.is_none() {
                return Ok(json_error(StatusCode::NOT_FOUND, "release not found"));
            }
            let year = parse_optional_admin_i32(body.year.as_deref(), 0, 3000);
            let track_number = parse_optional_admin_i32(body.track_number.as_deref(), 1, 9999);
            let disc_number = parse_optional_admin_i32(body.disc_number.as_deref(), 1, 999);
            sqlx::query(
                "UPDATE furumusic__track \
                 SET title = $1, title_sort = $2, release_id = $3, track_number = $4, disc_number = $5, year = $6, is_hidden = $7, updated_at = $8 \
                 WHERE id = $9",
            )
            .bind(title)
            .bind(normalize_name(title))
            .bind(release_id)
            .bind(track_number)
            .bind(disc_number)
            .bind(year)
            .bind(body.hidden)
            .bind(&now)
            .bind(body.id)
            .execute(pool)
            .await
        }
        "playlists" => {
            sqlx::query(
                "UPDATE furumusic__playlist \
                 SET title = $1, is_public = $2, updated_at = $3 \
                 WHERE id = $4",
            )
            .bind(title)
            .bind(!body.hidden)
            .bind(&now)
            .bind(body.id)
            .execute(pool)
            .await
        }
        _ => unreachable!(),
    }
    .map_err(|e| cot::Error::internal(e.to_string()))?
    .rows_affected();

    if affected == 0 {
        return Ok(json_error(StatusCode::NOT_FOUND, "library item not found"));
    }
    if kind == "releases" || kind == "tracks" {
        if let Some(mut artist_ids) = body.artist_ids {
            let mut seen_artist_ids = HashSet::new();
            artist_ids.retain(|id| *id > 0 && seen_artist_ids.insert(*id));
            if kind == "releases" {
                set_release_artists(pool, body.id, &artist_ids)
                    .await
                    .map_err(|e| cot::Error::internal(e.to_string()))?;
            } else {
                sqlx::query(
                    "DELETE FROM furumusic__track_artist WHERE track_id = $1 AND role = 'main'",
                )
                .bind(body.id)
                .execute(pool)
                .await
                .map_err(|e| cot::Error::internal(e.to_string()))?;
                for (position, artist_id) in artist_ids.iter().enumerate() {
                    sqlx::query(
                        "INSERT INTO furumusic__track_artist (track_id, artist_id, role, position) VALUES ($1, $2, 'main', $3)",
                    )
                    .bind(body.id)
                    .bind(*artist_id)
                    .bind(position as i32)
                    .execute(pool)
                    .await
                    .map_err(|e| cot::Error::internal(e.to_string()))?;
                }
            }
        }
    }

    if kind == "releases" {
        if let Some(release_tracks) = body.release_tracks.as_deref() {
            update_release_tracks(pool, body.id, release_tracks, &now)
                .await
                .map_err(|e| cot::Error::internal(e.to_string()))?;
        }
    }

    let Some(item) = fetch_library_item(pool, &kind, body.id)
        .await
        .map_err(|e| cot::Error::internal(e.to_string()))?
    else {
        return Ok(json_error(StatusCode::NOT_FOUND, "library item not found"));
    };

    Json(item).into_response()
}

pub async fn set_library_item_image(
    session: Session,
    db: Database,
    pool: &PgPool,
    Json(body): Json<SetLibraryImageRequest>,
) -> cot::Result<cot::response::Response> {
    if let Err(response) = require_admin_json(&session, &db).await {
        return Ok(response);
    }
    let kind = normalize_library_kind(Some(body.kind.as_str()));
    if kind != "artists" && kind != "releases" {
        return Ok(json_error(StatusCode::BAD_REQUEST, "unsupported kind"));
    }
    if let Some(fid) = body.media_file_id {
        let exists: Option<i64> = sqlx::query_scalar(
            "SELECT id FROM furumusic__media_file WHERE id = $1 AND file_type = 'cover_art'",
        )
        .bind(fid)
        .fetch_optional(pool)
        .await
        .map_err(|e| cot::Error::internal(e.to_string()))?;
        if exists.is_none() {
            return Ok(json_error(StatusCode::NOT_FOUND, "image not found"));
        }
    }
    let now = now_string();
    let result = if kind == "releases" {
        sqlx::query(
            "UPDATE furumusic__release SET cover_file_id = $1, updated_at = $2 WHERE id = $3",
        )
        .bind(body.media_file_id)
        .bind(&now)
        .bind(body.id)
        .execute(pool)
        .await
    } else {
        sqlx::query(
            "UPDATE furumusic__artist SET image_file_id = $1, updated_at = $2 WHERE id = $3",
        )
        .bind(body.media_file_id)
        .bind(&now)
        .bind(body.id)
        .execute(pool)
        .await
    }
    .map_err(|e| cot::Error::internal(e.to_string()))?;
    if result.rows_affected() == 0 {
        return Ok(json_error(StatusCode::NOT_FOUND, "library item not found"));
    }
    Json(serde_json::json!({ "ok": true })).into_response()
}

pub async fn upload_library_item_image(
    session: Session,
    db: Database,
    pool: &PgPool,
    Json(body): Json<UploadLibraryImageRequest>,
) -> cot::Result<cot::response::Response> {
    if let Err(response) = require_admin_json(&session, &db).await {
        return Ok(response);
    }
    let kind = normalize_library_kind(Some(body.kind.as_str()));
    if kind != "artists" && kind != "releases" {
        return Ok(json_error(StatusCode::BAD_REQUEST, "unsupported kind"));
    }
    let storage_dir = AppConfig::load_with_db(&db).await.0.agent_storage_dir;
    if storage_dir.trim().is_empty() {
        return Err(cot::Error::internal("agent_storage_dir is not configured"));
    }
    use base64::Engine;
    let image_data = base64::engine::general_purpose::STANDARD
        .decode(body.data.trim())
        .map_err(|e| cot::Error::internal(format!("invalid base64: {e}")))?;
    if image_data.is_empty() {
        return Ok(json_error(StatusCode::BAD_REQUEST, "image is empty"));
    }
    let title: Option<String> = if kind == "releases" {
        sqlx::query_scalar("SELECT title::text FROM furumusic__release WHERE id = $1")
    } else {
        sqlx::query_scalar("SELECT name::text FROM furumusic__artist WHERE id = $1")
    }
    .bind(body.id)
    .fetch_optional(pool)
    .await
    .map_err(|e| cot::Error::internal(e.to_string()))?;
    let Some(title) = title else {
        return Ok(json_error(StatusCode::NOT_FOUND, "library item not found"));
    };
    let cover = crate::agent::cover_art::CoverImage {
        data: image_data,
        mime_type: body.mime_type,
        source: crate::agent::cover_art::CoverSource::FolderFile(std::path::PathBuf::from(
            body.filename,
        )),
    };
    let media_file_id = crate::agent::cover_art::save_cover_to_storage(
        &db,
        pool,
        &storage_dir,
        &title,
        if kind == "artists" {
            "__artist_image__"
        } else {
            "__release_cover__"
        },
        &cover,
    )
    .await
    .map_err(|e| cot::Error::internal(format!("failed to save image: {e}")))?;
    set_library_item_image(
        session,
        db,
        pool,
        Json(SetLibraryImageRequest {
            kind,
            id: body.id,
            media_file_id: Some(media_file_id),
        }),
    )
    .await
}

pub async fn bulk_library(
    session: Session,
    db: Database,
    pool: &PgPool,
    Json(body): Json<BulkLibraryRequest>,
) -> cot::Result<cot::response::Response> {
    if let Err(response) = require_admin_json(&session, &db).await {
        return Ok(response);
    }

    let kind = normalize_library_kind(Some(body.kind.as_str()));
    let action = body.action.trim();
    if !matches!(action, "hide" | "show" | "delete") {
        return Ok(json_error(
            StatusCode::BAD_REQUEST,
            "unknown library action",
        ));
    }

    let mut ids = if body.mode.as_deref() == Some("filter") {
        library_ids_by_filter(pool, &kind, body.filter.unwrap_or_default()).await?
    } else {
        body.ids.unwrap_or_default()
    };
    ids.retain(|id| *id > 0);
    ids.sort_unstable();
    ids.dedup();
    if ids.is_empty() {
        return Json(MutationResponse {
            ok: true,
            affected: 0,
        })
        .into_response();
    }

    let storage_dir = if action == "delete" && matches!(kind.as_str(), "releases" | "tracks") {
        AppConfig::load_with_db(&db).await.0.agent_storage_dir
    } else {
        String::new()
    };
    let affected = apply_library_action(pool, &kind, action, &ids, &storage_dir).await?;
    Json(MutationResponse { ok: true, affected }).into_response()
}

pub async fn merge_releases(
    session: Session,
    db: Database,
    pool: &PgPool,
    Json(body): Json<MergeReleasesRequest>,
) -> cot::Result<cot::response::Response> {
    if let Err(response) = require_admin_json(&session, &db).await {
        return Ok(response);
    }

    let mut release_ids = body.release_ids;
    release_ids.retain(|id| *id > 0);
    release_ids.sort_unstable();
    release_ids.dedup();
    if release_ids.len() < 2 {
        return Ok(json_error(
            StatusCode::BAD_REQUEST,
            "select at least two releases to merge",
        ));
    }
    if release_ids.len() > 250 {
        return Ok(json_error(
            StatusCode::BAD_REQUEST,
            "at most 250 releases can be merged at once",
        ));
    }
    if !release_ids.contains(&body.target_release_id) {
        return Ok(json_error(
            StatusCode::BAD_REQUEST,
            "destination release must be part of the selection",
        ));
    }

    let title = body.title.trim();
    if title.is_empty() {
        return Ok(json_error(StatusCode::BAD_REQUEST, "title cannot be empty"));
    }
    if title.chars().count() > 255 {
        return Ok(json_error(
            StatusCode::BAD_REQUEST,
            "title cannot exceed 255 characters",
        ));
    }
    let release_type = body.release_type.trim().to_lowercase();
    if !matches!(
        release_type.as_str(),
        "album"
            | "single"
            | "ep"
            | "compilation"
            | "mixtape"
            | "live"
            | "soundtrack"
            | "remix"
            | "demo"
            | "unknown"
    ) {
        return Ok(json_error(StatusCode::BAD_REQUEST, "invalid release type"));
    }
    let year = match parse_merge_optional_i32(body.year.as_deref(), 0, 3000, "year") {
        Ok(year) => year,
        Err(message) => return Ok(json_error(StatusCode::BAD_REQUEST, &message)),
    };

    let mut tracks = Vec::with_capacity(body.tracks.len());
    for track in body.tracks {
        if track.id <= 0 {
            return Ok(json_error(StatusCode::BAD_REQUEST, "invalid track id"));
        }
        let track_number = match parse_merge_optional_i32(
            track.track_number.as_deref(),
            1,
            9999,
            "track number",
        ) {
            Ok(value) => value,
            Err(message) => return Ok(json_error(StatusCode::BAD_REQUEST, &message)),
        };
        let disc_number =
            match parse_merge_optional_i32(track.disc_number.as_deref(), 1, 999, "disc number") {
                Ok(value) => value,
                Err(message) => return Ok(json_error(StatusCode::BAD_REQUEST, &message)),
            };
        tracks.push(crate::library_cleanup::ReleaseMergeTrack {
            id: track.id,
            track_number,
            disc_number,
        });
    }

    let existing_release_ids: Vec<i64> =
        sqlx::query_scalar("SELECT id FROM furumusic__release WHERE id = ANY($1) ORDER BY id")
            .bind(&release_ids)
            .fetch_all(pool)
            .await
            .map_err(|error| cot::Error::internal(error.to_string()))?;
    if existing_release_ids != release_ids {
        return Ok(json_error(
            StatusCode::BAD_REQUEST,
            "one or more selected releases no longer exist",
        ));
    }
    let selected_track_ids: Vec<i64> = sqlx::query_scalar(
        "SELECT id FROM furumusic__track WHERE release_id = ANY($1) ORDER BY id",
    )
    .bind(&release_ids)
    .fetch_all(pool)
    .await
    .map_err(|error| cot::Error::internal(error.to_string()))?;
    let mut requested_track_ids = tracks.iter().map(|track| track.id).collect::<Vec<_>>();
    requested_track_ids.sort_unstable();
    if requested_track_ids.windows(2).any(|ids| ids[0] == ids[1])
        || requested_track_ids != selected_track_ids
    {
        return Ok(json_error(
            StatusCode::BAD_REQUEST,
            "track list does not match the selected releases; reopen the merge wizard",
        ));
    }

    let storage_dir = AppConfig::load_with_db(&db).await.0.agent_storage_dir;
    let result = match crate::library_cleanup::merge_releases(
        pool,
        crate::library_cleanup::ReleaseMergeSpec {
            release_ids,
            target_release_id: body.target_release_id,
            title: title.to_owned(),
            title_sort: normalize_name(title),
            release_type,
            year,
            hidden: body.hidden,
            cover_file_id: body.cover_file_id,
            artist_ids: body.artist_ids,
            tracks,
        },
        &storage_dir,
    )
    .await
    {
        Ok(result) => result,
        Err(error) => {
            tracing::error!(?error, "release merge failed");
            return Ok(json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "release merge failed; no changes were committed",
            ));
        }
    };

    let Some(item) = fetch_library_item(pool, "releases", body.target_release_id)
        .await
        .map_err(|error| cot::Error::internal(error.to_string()))?
    else {
        return Ok(json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "merged release could not be loaded",
        ));
    };
    Json(MergeReleasesResponse {
        ok: true,
        merged_releases: result.merged_releases,
        moved_tracks: result.moved_tracks,
        item,
    })
    .into_response()
}

async fn require_admin_json(
    session: &Session,
    db: &Database,
) -> Result<AuthenticatedUser, cot::response::Response> {
    let Some(user) = auth::get_session_user(session, db).await else {
        return Err(json_error(StatusCode::UNAUTHORIZED, "not authenticated"));
    };
    if user.role != Role::Admin {
        return Err(json_error(StatusCode::FORBIDDEN, "forbidden"));
    }
    Ok(user)
}

fn json_error(status: StatusCode, message: &str) -> cot::response::Response {
    let body = serde_json::json!({ "error": message });
    cot::http::Response::builder()
        .status(status)
        .header(CONTENT_TYPE, "application/json")
        .body(Body::fixed(body.to_string()))
        .expect("valid response")
}

fn build_dto() -> BuildDto {
    BuildDto {
        package: BUILD_INFO.pkg_name,
        version: BUILD_INFO.pkg_version,
        profile: BUILD_INFO.profile,
        target: BUILD_INFO.target,
        rustc_version: BUILD_INFO.rustc_version,
    }
}

async fn sync_registered_jobs(db: &Database, registry: &JobRegistry) {
    for job in registry.all_jobs() {
        if let Err(e) =
            ScheduledJob::upsert(db, job.name(), job.description(), job.default_cron()).await
        {
            tracing::error!("failed to upsert scheduled job {}: {e}", job.name());
        }
    }
    if let Ok(all) = ScheduledJob::list_all(db).await {
        for sched_job in all {
            if registry.get(sched_job.name_str()).is_none() {
                tracing::warn!("removing orphaned scheduled job '{}'", sched_job.name_str());
                let _ = ScheduledJob::delete_by_name(db, sched_job.name_str()).await;
            }
        }
    }
}

async fn load_overview_stats(pool: &PgPool) -> anyhow::Result<OverviewStatsDto> {
    let tracks: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM furumusic__track")
        .fetch_one(pool)
        .await?;
    let releases: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM furumusic__release")
        .fetch_one(pool)
        .await?;
    let artists: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM furumusic__artist")
        .fetch_one(pool)
        .await?;
    let playlists: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM furumusic__playlist")
        .fetch_one(pool)
        .await?;
    let hidden_tracks: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM furumusic__track WHERE is_hidden")
            .fetch_one(pool)
            .await?;
    let hidden_releases: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM furumusic__release WHERE is_hidden")
            .fetch_one(pool)
            .await?;
    let hidden_artists: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM furumusic__artist WHERE is_hidden")
            .fetch_one(pool)
            .await?;

    Ok(OverviewStatsDto {
        tracks,
        releases,
        artists,
        playlists,
        hidden_tracks,
        hidden_releases,
        hidden_artists,
    })
}

#[derive(Debug, sqlx::FromRow)]
struct AdminUserSqlRow {
    id: i64,
    username: String,
    display_name: Option<String>,
    email: Option<String>,
    role: String,
    is_active: bool,
}

async fn load_admin_users_page(
    pool: &PgPool,
    query: UsersQuery,
) -> anyhow::Result<AdminUsersPageDto> {
    let limit = query.limit.unwrap_or(40).clamp(10, 200);
    let offset = query.offset.unwrap_or(0).max(0);
    let search = query
        .search
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned);
    let pattern = search.as_ref().map(|value| format!("%{value}%"));

    let mut count_qb =
        QueryBuilder::<Postgres>::new("SELECT COUNT(*) FROM furumusic__user WHERE 1=1");
    if let Some(pattern) = pattern.as_ref() {
        count_qb.push(" AND (username ILIKE ");
        count_qb.push_bind(pattern);
        count_qb.push(" OR COALESCE(display_name, '') ILIKE ");
        count_qb.push_bind(pattern);
        count_qb.push(" OR COALESCE(email, '') ILIKE ");
        count_qb.push_bind(pattern);
        count_qb.push(")");
    }
    let total: i64 = count_qb.build_query_scalar().fetch_one(pool).await?;

    let mut qb = QueryBuilder::<Postgres>::new(
        "SELECT id, username::text, display_name, email, role::text, is_active FROM furumusic__user WHERE 1=1",
    );
    if let Some(pattern) = pattern.as_ref() {
        qb.push(" AND (username ILIKE ");
        qb.push_bind(pattern);
        qb.push(" OR COALESCE(display_name, '') ILIKE ");
        qb.push_bind(pattern);
        qb.push(" OR COALESCE(email, '') ILIKE ");
        qb.push_bind(pattern);
        qb.push(")");
    }
    qb.push(" ORDER BY username ASC LIMIT ");
    qb.push_bind(limit);
    qb.push(" OFFSET ");
    qb.push_bind(offset);
    let rows: Vec<AdminUserSqlRow> = qb.build_query_as().fetch_all(pool).await?;

    let active = crate::metrics::active_user_last_seen_ms();
    let online_cutoff_ms = 60_000;
    let items = rows
        .into_iter()
        .map(|row| admin_user_row(row, &active, online_cutoff_ms))
        .collect::<Vec<_>>();
    let online_count = active
        .values()
        .filter(|last_seen_ms| **last_seen_ms <= online_cutoff_ms)
        .count() as i64;

    Ok(AdminUsersPageDto {
        items,
        total,
        limit,
        offset,
        search,
        online_count,
    })
}

async fn load_admin_user_detail(
    pool: &PgPool,
    user_id: i64,
) -> anyhow::Result<Option<AdminUserDetailDto>> {
    let row = sqlx::query_as::<_, AdminUserSqlRow>(
        "SELECT id, username::text, display_name, email, role::text, is_active FROM furumusic__user WHERE id = $1",
    )
    .bind(user_id)
    .fetch_optional(pool)
    .await?;
    let Some(row) = row else {
        return Ok(None);
    };

    let active = crate::metrics::active_user_last_seen_ms();
    let user = admin_user_row(row, &active, 60_000);
    let (
        plays,
        completed_plays,
        listened_seconds,
        liked_tracks,
        followed_artists,
        own_playlists,
        saved_playlists,
        uploaded_tracks,
        torrent_sessions,
        lastfm_connected,
    ) = tokio::try_join!(
        sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM furumusic__play_history WHERE user_id = $1"
        )
        .bind(user_id)
        .fetch_one(pool),
        sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM furumusic__play_history WHERE user_id = $1 AND completed"
        )
        .bind(user_id)
        .fetch_one(pool),
        sqlx::query_scalar::<_, i64>(
            "SELECT COALESCE(SUM(duration_listened), 0)::bigint FROM furumusic__play_history WHERE user_id = $1"
        )
        .bind(user_id)
        .fetch_one(pool),
        sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM furumusic__user_liked_track WHERE user_id = $1"
        )
        .bind(user_id)
        .fetch_one(pool),
        sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM furumusic__user_followed_artist WHERE user_id = $1"
        )
        .bind(user_id)
        .fetch_one(pool),
        sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM furumusic__playlist WHERE owner_id = $1"
        )
        .bind(user_id)
        .fetch_one(pool),
        sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM furumusic__saved_playlist WHERE user_id = $1"
        )
        .bind(user_id)
        .fetch_one(pool),
        sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(DISTINCT t.id) FROM furumusic__track t JOIN furumusic__media_file mf ON mf.id = t.audio_file_id WHERE mf.uploaded_by_user_id = $1"
        )
        .bind(user_id)
        .fetch_one(pool),
        sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM furumusic__torrent_session WHERE user_id = $1"
        )
        .bind(user_id)
        .fetch_one(pool),
        sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS(SELECT 1 FROM furumusic__lastfm_account WHERE user_id = $1 AND session_key <> '')"
        )
        .bind(user_id)
        .fetch_one(pool),
    )?;
    let recent_plays = load_admin_user_recent_plays(pool, user_id, 30).await?;

    Ok(Some(AdminUserDetailDto {
        user,
        stats: AdminUserStatsDto {
            plays,
            completed_plays,
            listened_seconds,
            liked_tracks,
            followed_artists,
            own_playlists,
            saved_playlists,
            uploaded_tracks,
            torrent_sessions,
            lastfm_connected,
        },
        recent_plays,
    }))
}

async fn load_admin_user_recent_plays(
    pool: &PgPool,
    user_id: i64,
    limit: i64,
) -> anyhow::Result<Vec<AdminUserPlayDto>> {
    let rows = sqlx::query_as::<_, AdminUserPlaySqlRow>(
        r#"SELECT ph.id AS history_id,
                  ph.played_at::text AS played_at,
                  ph.duration_listened,
                  ph.completed,
                  t.id AS track_id,
                  t.title::text AS title,
                  COALESCE(NULLIF(STRING_AGG(a.name::text, ', ' ORDER BY ta.position), ''), 'Unknown artist') AS artists,
                  t.release_id,
                  COALESCE(r.title::text, '') AS release_title,
                  r.year AS release_year,
                  t.cover_file_id,
                  r.cover_file_id AS release_cover_file_id,
                  t.duration_seconds AS track_duration_seconds,
                  COALESCE(mf.uploader_name, 'UFO')::text AS uploader_name,
                  mf.audio_format,
                  mf.audio_bitrate
           FROM furumusic__play_history ph
           JOIN furumusic__track t ON t.id = ph.track_id
           LEFT JOIN furumusic__release r ON r.id = t.release_id
           LEFT JOIN furumusic__media_file mf ON mf.id = t.audio_file_id
           LEFT JOIN furumusic__track_artist ta ON ta.track_id = t.id AND ta.role = 'main'
           LEFT JOIN furumusic__artist a ON a.id = ta.artist_id
           WHERE ph.user_id = $1
           GROUP BY ph.id, ph.played_at, ph.duration_listened, ph.completed, t.id, t.title,
                    t.release_id, r.title, r.year, t.cover_file_id, r.cover_file_id,
                    t.duration_seconds, mf.uploader_name, mf.audio_format, mf.audio_bitrate
           ORDER BY ph.played_at DESC, ph.id DESC
           LIMIT $2"#,
    )
    .bind(user_id)
    .bind(limit)
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(|row| AdminUserPlayDto {
            history_id: row.history_id,
            played_at: row.played_at,
            duration_listened: row.duration_listened,
            completed: row.completed,
            track_id: row.track_id,
            title: row.title,
            artists: row.artists,
            release_id: row.release_id,
            release_title: row.release_title,
            release_year: row.release_year,
            cover_url: admin_track_cover_url(row.cover_file_id, row.release_cover_file_id),
            track_duration_seconds: row.track_duration_seconds,
            uploader_name: row.uploader_name,
            audio_format: row.audio_format,
            audio_bitrate: row.audio_bitrate,
        })
        .collect())
}

#[derive(Debug, sqlx::FromRow)]
struct AdminUserPlaySqlRow {
    history_id: i64,
    played_at: String,
    duration_listened: Option<i32>,
    completed: bool,
    track_id: i64,
    title: String,
    artists: String,
    release_id: i64,
    release_title: String,
    release_year: Option<i32>,
    cover_file_id: Option<i64>,
    release_cover_file_id: Option<i64>,
    track_duration_seconds: f64,
    uploader_name: String,
    audio_format: Option<String>,
    audio_bitrate: Option<i32>,
}

fn admin_track_cover_url(track_cover: Option<i64>, release_cover: Option<i64>) -> Option<String> {
    track_cover
        .or(release_cover)
        .map(|id| format!("/api/player/cover/{id}/medium"))
}

fn admin_user_row(
    row: AdminUserSqlRow,
    active: &HashMap<i64, i64>,
    online_cutoff_ms: i64,
) -> AdminUserRowDto {
    let last_seen_ms = active.get(&row.id).copied();
    AdminUserRowDto {
        id: row.id,
        username: row.username,
        display_name: row.display_name,
        email: row.email,
        role: row.role,
        is_active: row.is_active,
        is_online: last_seen_ms.is_some_and(|value| value <= online_cutoff_ms),
        last_seen_ms,
    }
}

fn load_runtime_overview(config: &AppConfig) -> RuntimeOverviewDto {
    let llm_configured = !config.agent_llm_url.trim().is_empty();
    let agent_status = if !config.agent_enabled {
        "disabled"
    } else if !llm_configured {
        "not_configured"
    } else {
        "enabled"
    };

    RuntimeOverviewDto {
        agent: AgentStatusDto {
            status: agent_status.to_owned(),
            enabled: config.agent_enabled,
            llm_configured,
            model: config.agent_llm_model.clone(),
            concurrency: config.agent_concurrency,
        },
        storage: vec![
            storage_path_dto("Inbox", &config.agent_inbox_dir),
            storage_path_dto("Library", &config.agent_storage_dir),
        ],
        node: NodeStatsDto {
            hostname: node_hostname(),
            os: std::env::consts::OS,
            arch: std::env::consts::ARCH,
            pid: std::process::id(),
            cpu_count: std::thread::available_parallelism()
                .map(|count| count.get())
                .unwrap_or(1),
        },
    }
}

fn storage_path_dto(label: &str, raw_path: &str) -> StoragePathDto {
    let path = raw_path.trim();
    let path_ref = Path::new(path);
    let usage = if path.is_empty() {
        None
    } else {
        disk_usage(path_ref).or_else(|| {
            path_ref
                .parent()
                .filter(|parent| !parent.as_os_str().is_empty())
                .and_then(disk_usage)
        })
    };

    StoragePathDto {
        label: label.to_owned(),
        path: path.to_owned(),
        exists: !path.is_empty() && path_ref.exists(),
        free_bytes: usage.map(|value| value.free_bytes),
        total_bytes: usage.map(|value| value.total_bytes),
    }
}

fn node_hostname() -> String {
    std::env::var("HOSTNAME")
        .or_else(|_| std::env::var("COMPUTERNAME"))
        .unwrap_or_else(|_| "unknown".to_owned())
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

async fn load_library_overview(pool: &PgPool) -> anyhow::Result<LibraryOverviewDto> {
    let stats = load_overview_stats(pool).await?;
    Ok(LibraryOverviewDto {
        artists: stats.artists,
        releases: stats.releases,
        tracks: stats.tracks,
        playlists: stats.playlists,
    })
}

async fn load_review_page(pool: &PgPool, query: ReviewsQuery) -> anyhow::Result<ReviewPageDto> {
    let limit = query.limit.unwrap_or(80).clamp(10, 250);
    let offset = query.offset.unwrap_or(0).max(0);
    let status = normalize_status(query.status.as_deref());
    let search = clean_search(query.search.as_deref());
    let search_pattern = search.as_ref().map(|s| format!("%{s}%"));

    let total = count_reviews(pool, status.clone(), search_pattern.clone()).await?;
    let total_all = count_reviews(pool, None, search_pattern.clone()).await?;
    let status_counts = load_review_status_counts(pool, None, search_pattern.clone()).await?;

    let mut qb = QueryBuilder::<Postgres>::new(
        "SELECT id, job_run_id, review_type::text AS review_type, input_path, context_json, \
                result_json, status::text AS status, created_at::text AS created_at, \
                updated_at::text AS updated_at, error_message \
         FROM furumusic__pending_review WHERE 1=1",
    );
    push_review_filters(&mut qb, status.clone(), search_pattern.clone());
    qb.push(" ORDER BY id DESC LIMIT ");
    qb.push_bind(limit);
    qb.push(" OFFSET ");
    qb.push_bind(offset);

    let rows = qb.build_query_as::<ReviewRow>().fetch_all(pool).await?;
    let stats = load_review_stats(pool, rows.iter().map(|row| row.id).collect()).await?;
    let media = load_review_media(pool, &rows).await?;
    let items = rows
        .into_iter()
        .map(|row| review_dto(row, &stats, &media))
        .collect();

    Ok(ReviewPageDto {
        items,
        total,
        total_all,
        limit,
        offset,
        status,
        search,
        status_counts,
    })
}

async fn count_reviews(
    pool: &PgPool,
    status: Option<String>,
    search_pattern: Option<String>,
) -> anyhow::Result<i64> {
    let mut qb = QueryBuilder::<Postgres>::new(
        "SELECT COUNT(*) AS count FROM furumusic__pending_review WHERE 1=1",
    );
    push_review_filters(&mut qb, status, search_pattern);
    Ok(qb.build_query_as::<CountRow>().fetch_one(pool).await?.count)
}

async fn load_review_status_counts(
    pool: &PgPool,
    status: Option<String>,
    search_pattern: Option<String>,
) -> anyhow::Result<Vec<StatusCountDto>> {
    let mut qb = QueryBuilder::<Postgres>::new(
        "SELECT status::text AS status, COUNT(*) AS count \
         FROM furumusic__pending_review WHERE 1=1",
    );
    push_review_filters(&mut qb, status, search_pattern);
    qb.push(" GROUP BY status ORDER BY status");

    let rows = qb
        .build_query_as::<StatusCountRow>()
        .fetch_all(pool)
        .await?;
    Ok(rows
        .into_iter()
        .map(|row| StatusCountDto {
            status: row.status,
            count: row.count,
        })
        .collect())
}

fn push_review_filters(
    qb: &mut QueryBuilder<'_, Postgres>,
    status: Option<String>,
    search_pattern: Option<String>,
) {
    if let Some(status) = status {
        qb.push(" AND status = ");
        qb.push_bind(status);
    }
    if let Some(pattern) = search_pattern {
        qb.push(" AND (input_path ILIKE ");
        qb.push_bind(pattern.clone());
        qb.push(" OR review_type::text ILIKE ");
        qb.push_bind(pattern.clone());
        qb.push(" OR COALESCE(error_message, '') ILIKE ");
        qb.push_bind(pattern);
        qb.push(")");
    }
}

async fn load_review_stats(
    pool: &PgPool,
    ids: Vec<i64>,
) -> anyhow::Result<HashMap<i64, ReviewStatsRow>> {
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    let rows = sqlx::query_as::<_, ReviewStatsRow>(
        "SELECT pending_review_id, model_name::text AS model_name, llm_duration_ms, \
                prompt_tokens, completion_tokens \
         FROM furumusic__processing_stats WHERE pending_review_id = ANY($1)",
    )
    .bind(&ids)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| (row.pending_review_id, row))
        .collect())
}

async fn load_review_media(
    pool: &PgPool,
    rows: &[ReviewRow],
) -> anyhow::Result<HashMap<String, ReviewMediaRow>> {
    let mut hashes = rows
        .iter()
        .filter_map(|row| context_sha256(row.context_json.as_deref().unwrap_or("")))
        .collect::<Vec<_>>();
    hashes.sort();
    hashes.dedup();
    if hashes.is_empty() {
        return Ok(HashMap::new());
    }

    let media_rows = sqlx::query_as::<_, ReviewMediaRow>(
        "SELECT sha256_hash::text AS sha256_hash, original_filename::text AS original_filename, \
                file_size_bytes, audio_format, audio_bitrate, audio_sample_rate, audio_bit_depth \
         FROM furumusic__media_file \
         WHERE file_type = 'audio' AND sha256_hash = ANY($1)",
    )
    .bind(&hashes)
    .fetch_all(pool)
    .await?;

    Ok(media_rows
        .into_iter()
        .map(|row| (row.sha256_hash.to_ascii_lowercase(), row))
        .collect())
}

fn review_dto(
    row: ReviewRow,
    stats: &HashMap<i64, ReviewStatsRow>,
    media: &HashMap<String, ReviewMediaRow>,
) -> ReviewDto {
    let input_path = row.input_path.unwrap_or_default();
    let filename = file_name(&input_path);
    let sha = context_sha256(row.context_json.as_deref().unwrap_or(""));
    let media_row = sha.as_ref().and_then(|hash| media.get(hash));
    let tags = media_row.map(media_tags).unwrap_or_default();
    let stat = stats.get(&row.id);
    let confidence = row
        .result_json
        .as_deref()
        .and_then(|json| serde_json::from_str::<serde_json::Value>(json).ok())
        .and_then(|value| value.get("confidence").and_then(|v| v.as_f64()));
    let normalized = row
        .result_json
        .as_deref()
        .map(review_edit_dto_from_json)
        .unwrap_or_default();

    ReviewDto {
        id: row.id,
        job_run_id: row.job_run_id,
        review_type: row.review_type,
        display_path: compact_path_tail(&input_path, 96),
        input_path,
        filename,
        status: row.status,
        confidence,
        model_name: stat.map(|s| s.model_name.clone()),
        llm_duration_ms: stat.map(|s| s.llm_duration_ms),
        token_count: stat.map(|s| s.prompt_tokens + s.completion_tokens),
        tags,
        error_message: row.error_message,
        normalized,
        created_at: row.created_at,
        updated_at: row.updated_at,
    }
}

fn review_edit_dto_from_json(result_json: &str) -> ReviewEditDto {
    let Ok(normalized) = serde_json::from_str::<crate::agent::dto::NormalizedFields>(result_json)
    else {
        return ReviewEditDto::default();
    };
    ReviewEditDto {
        title: normalized.title.unwrap_or_default(),
        artist: normalized.artist.unwrap_or_default(),
        album: normalized.album.unwrap_or_default(),
        year: normalized.year.map(|v| v.to_string()).unwrap_or_default(),
        track_number: normalized
            .track_number
            .map(|v| v.to_string())
            .unwrap_or_default(),
        genre: normalized.genre.unwrap_or_default(),
        featured_artists: normalized.featured_artists.join(", "),
        release_type: normalized
            .release_type
            .unwrap_or_else(|| "album".to_owned()),
        notes: normalized.notes.unwrap_or_default(),
    }
}

fn optional_trimmed(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_owned())
    }
}

fn parse_optional_i32(value: &str) -> Option<i32> {
    value.trim().parse::<i32>().ok()
}

fn parse_featured_artists(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .map(str::to_owned)
        .collect()
}

fn normalized_from_review_edit(edit: &ReviewEditDto) -> crate::agent::dto::NormalizedFields {
    crate::agent::dto::NormalizedFields {
        title: optional_trimmed(&edit.title),
        artist: optional_trimmed(&edit.artist),
        album: optional_trimmed(&edit.album),
        year: parse_optional_i32(&edit.year),
        track_number: parse_optional_i32(&edit.track_number),
        genre: optional_trimmed(&edit.genre),
        featured_artists: parse_featured_artists(&edit.featured_artists),
        release_type: optional_trimmed(&edit.release_type).or_else(|| Some("album".to_owned())),
        confidence: Some(1.0),
        notes: optional_trimmed(&edit.notes),
    }
}

fn media_tags(row: &ReviewMediaRow) -> Vec<TagDto> {
    let mut tags = Vec::new();
    if let Some(format) = row.audio_format.as_deref().filter(|s| !s.trim().is_empty()) {
        tags.push(tag(format.to_ascii_lowercase(), "format"));
    } else if let Some(ext) = file_extension(&row.original_filename) {
        tags.push(tag(ext, "format"));
    }
    if let Some(bitrate) = row.audio_bitrate {
        tags.push(tag(format!("{bitrate} kbps"), "bitrate"));
    }
    if let Some(sample_rate) = row.audio_sample_rate {
        if sample_rate % 1000 == 0 {
            tags.push(tag(format!("{} kHz", sample_rate / 1000), "sample"));
        } else {
            tags.push(tag(
                format!("{:.1} kHz", sample_rate as f64 / 1000.0),
                "sample",
            ));
        }
    }
    if let Some(bit_depth) = row.audio_bit_depth {
        tags.push(tag(format!("{bit_depth}-bit"), "depth"));
    }
    tags.push(tag(size_display(row.file_size_bytes), "size"));
    tags
}

async fn apply_review_filter_action(
    pool: &PgPool,
    action: &str,
    filter: ReviewFilter,
) -> cot::Result<u64> {
    let status = normalize_status(filter.status.as_deref());
    let search_pattern = clean_search(filter.search.as_deref()).map(|s| format!("%{s}%"));

    let result = if action == "delete" {
        let mut qb =
            QueryBuilder::<Postgres>::new("DELETE FROM furumusic__pending_review WHERE 1=1");
        push_review_filters(&mut qb, status, search_pattern);
        qb.build()
            .execute(pool)
            .await
            .map_err(|e| cot::Error::internal(e.to_string()))?
    } else {
        let now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
        let mut qb = QueryBuilder::<Postgres>::new(
            "UPDATE furumusic__pending_review \
             SET status = 'queued', error_message = NULL, updated_at = ",
        );
        qb.push_bind(now);
        qb.push(" WHERE 1=1");
        push_review_filters(&mut qb, status, search_pattern);
        qb.build()
            .execute(pool)
            .await
            .map_err(|e| cot::Error::internal(e.to_string()))?
    };

    Ok(result.rows_affected())
}

async fn load_jobs(
    db: &Database,
    pool: &PgPool,
    _registry: &JobRegistry,
) -> anyhow::Result<Vec<JobDto>> {
    let mut jobs = ScheduledJob::list_all(db).await?;
    jobs.sort_by(|a, b| a.name_str().cmp(b.name_str()));
    let recent_runs = load_recent_runs_per_job(pool, 8).await?;
    let mut runs_by_job: HashMap<String, Vec<JobRunDto>> = HashMap::new();
    for run in recent_runs {
        runs_by_job
            .entry(run.job_name.clone())
            .or_default()
            .push(run);
    }

    Ok(jobs
        .into_iter()
        .map(|job| {
            let runs = runs_by_job.remove(job.name_str()).unwrap_or_default();
            let is_running = runs.iter().any(|run| run.status == "running");
            let last_run = runs.first();
            let health = if !job.enabled {
                "disabled"
            } else if is_running {
                "running"
            } else if last_run.is_some_and(|run| run.status == "failed") {
                "failed"
            } else if last_run.is_some() {
                "ok"
            } else {
                "idle"
            };
            JobDto {
                name: job.name_str().to_owned(),
                description: job.description_str().to_owned(),
                cron_expression: job.cron_expression_str().to_owned(),
                enabled: job.enabled,
                health: health.to_owned(),
                is_running,
                last_run_at: optional_job_time(job.last_run_at_str()),
                next_run_at: optional_job_time(job.next_run_at_str()),
                recent_runs: runs,
            }
        })
        .collect())
}

async fn load_recent_runs(pool: &PgPool, limit: i64) -> anyhow::Result<Vec<JobRunDto>> {
    let rows = sqlx::query_as::<_, JobRunRow>(
        "SELECT id, job_name::text AS job_name, status::text AS status, started_at::text AS started_at, \
                finished_at, duration_ms, trigger::text AS trigger, error_message, \
                LEFT(COALESCE(log_output, ''), 1600) AS log_excerpt \
         FROM furumusic__job_run ORDER BY id DESC LIMIT $1",
    )
    .bind(limit)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(Into::into).collect())
}

async fn load_recent_runs_per_job(pool: &PgPool, per_job: i64) -> anyhow::Result<Vec<JobRunDto>> {
    let rows = sqlx::query_as::<_, JobRunRow>(
        "WITH ranked AS ( \
             SELECT id, job_name::text AS job_name, status::text AS status, started_at::text AS started_at, \
                    finished_at, duration_ms, trigger::text AS trigger, error_message, \
                    LEFT(COALESCE(log_output, ''), 1600) AS log_excerpt, \
                    ROW_NUMBER() OVER (PARTITION BY job_name ORDER BY id DESC) AS rn \
             FROM furumusic__job_run \
         ) \
         SELECT id, job_name, status, started_at, finished_at, duration_ms, trigger, error_message, log_excerpt \
         FROM ranked WHERE rn <= $1 ORDER BY id DESC",
    )
    .bind(per_job)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(Into::into).collect())
}

async fn load_runs_for_job(
    pool: &PgPool,
    job_name: &str,
    limit: i64,
) -> anyhow::Result<Vec<JobRunDto>> {
    let rows = sqlx::query_as::<_, JobRunRow>(
        "SELECT id, job_name::text AS job_name, status::text AS status, started_at::text AS started_at, \
                finished_at, duration_ms, trigger::text AS trigger, error_message, \
                LEFT(COALESCE(log_output, ''), 1600) AS log_excerpt \
         FROM furumusic__job_run WHERE job_name = $1 ORDER BY id DESC LIMIT $2",
    )
    .bind(job_name)
    .bind(limit)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(Into::into).collect())
}

async fn load_library_page(pool: &PgPool, query: LibraryQuery) -> anyhow::Result<LibraryPageDto> {
    let kind = normalize_library_kind(query.kind.as_deref());
    let limit = query.limit.unwrap_or(40).clamp(10, 120);
    let offset = query.offset.unwrap_or(0).max(0);
    let search = clean_search(query.search.as_deref());
    let search_pattern = search.as_ref().map(|s| format!("%{s}%"));

    let total = count_library(pool, &kind, search_pattern.clone()).await?;
    let rows = match kind.as_str() {
        "releases" => load_release_items(pool, search_pattern.clone(), limit, offset).await?,
        "tracks" => load_track_items(pool, search_pattern.clone(), limit, offset).await?,
        "playlists" => load_playlist_items(pool, search_pattern.clone(), limit, offset).await?,
        _ => load_artist_items(pool, search_pattern.clone(), limit, offset).await?,
    };
    let items = rows
        .into_iter()
        .map(|row| library_item_dto(&kind, row))
        .collect();

    Ok(LibraryPageDto {
        kind,
        items,
        total,
        limit,
        offset,
        search,
    })
}

async fn fetch_library_item(
    pool: &PgPool,
    kind: &str,
    id: i64,
) -> anyhow::Result<Option<LibraryItemDto>> {
    let row = match kind {
        "releases" => {
            sqlx::query_as::<_, LibraryItemRow>(
                "SELECT r.id, r.title::text AS title, \
                        (COALESCE(NULLIF(STRING_AGG(DISTINCT a.name::text, ', '), ''), 'Unknown artist') || COALESCE(' / ' || r.year::text, '')) AS subtitle, \
                        r.is_hidden, COUNT(DISTINCT t.id)::bigint AS primary_count, \
                        COUNT(DISTINCT ra.artist_id)::bigint AS secondary_count, \
                        COUNT(DISTINCT ph.id)::bigint AS tertiary_count, \
                        r.updated_at::text AS updated_at \
                 FROM furumusic__release r \
                 LEFT JOIN furumusic__track t ON t.release_id = r.id \
                 LEFT JOIN furumusic__release_artist ra ON ra.release_id = r.id \
                 LEFT JOIN furumusic__artist a ON a.id = ra.artist_id \
                 LEFT JOIN furumusic__play_history ph ON ph.track_id = t.id \
                 WHERE r.id = $1 \
                 GROUP BY r.id",
            )
            .bind(id)
            .fetch_optional(pool)
            .await?
        }
        "playlists" => {
            sqlx::query_as::<_, LibraryItemRow>(
                "SELECT p.id, p.title::text AS title, \
                        COALESCE(u.display_name, u.username, 'unknown') AS subtitle, \
                        (NOT p.is_public) AS is_hidden, COUNT(pt.id)::bigint AS primary_count, \
                        CASE WHEN p.is_public THEN 1 ELSE 0 END::bigint AS secondary_count, \
                        0::bigint AS tertiary_count, p.updated_at::text AS updated_at \
                 FROM furumusic__playlist p \
                 LEFT JOIN furumusic__playlist_track pt ON pt.playlist_id = p.id \
                 LEFT JOIN furumusic__user u ON u.id = p.owner_id \
                 WHERE p.id = $1 \
                 GROUP BY p.id, u.display_name, u.username",
            )
            .bind(id)
            .fetch_optional(pool)
            .await?
        }
        "tracks" => {
            sqlx::query_as::<_, LibraryItemRow>(
                "SELECT t.id, t.title::text AS title, \
                        CONCAT(COALESCE(r.title::text, 'No release'), COALESCE(' / #' || t.track_number::text, '')) AS subtitle, \
                        t.is_hidden, COUNT(DISTINCT ta.artist_id)::bigint AS primary_count, \
                        COUNT(DISTINCT ph.id)::bigint AS secondary_count, \
                        COUNT(DISTINCT pt.playlist_id)::bigint AS tertiary_count, \
                        t.updated_at::text AS updated_at \
                 FROM furumusic__track t \
                 LEFT JOIN furumusic__release r ON r.id = t.release_id \
                 LEFT JOIN furumusic__track_artist ta ON ta.track_id = t.id \
                 LEFT JOIN furumusic__play_history ph ON ph.track_id = t.id \
                 LEFT JOIN furumusic__playlist_track pt ON pt.track_id = t.id \
                 WHERE t.id = $1 \
                 GROUP BY t.id, r.title",
            )
            .bind(id)
            .fetch_optional(pool)
            .await?
        }
        _ => {
            sqlx::query_as::<_, LibraryItemRow>(
                "SELECT a.id, a.name::text AS title, NULL::text AS subtitle, a.is_hidden, \
                        COUNT(DISTINCT ra.release_id)::bigint AS primary_count, \
                        COUNT(DISTINCT ta.track_id)::bigint AS secondary_count, \
                        COUNT(DISTINCT ufa.user_id)::bigint AS tertiary_count, \
                        a.updated_at::text AS updated_at \
                 FROM furumusic__artist a \
                 LEFT JOIN furumusic__release_artist ra ON ra.artist_id = a.id \
                 LEFT JOIN furumusic__track_artist ta ON ta.artist_id = a.id \
                 LEFT JOIN furumusic__user_followed_artist ufa ON ufa.artist_id = a.id \
                 WHERE a.id = $1 \
                 GROUP BY a.id",
            )
            .bind(id)
            .fetch_optional(pool)
            .await?
        }
    };

    Ok(row.map(|row| library_item_dto(kind, row)))
}

async fn load_library_item_detail(
    pool: &PgPool,
    kind: &str,
    item: LibraryItemDto,
) -> anyhow::Result<LibraryItemDetailDto> {
    let mut detail = LibraryItemDetailDto {
        title: item.title.clone(),
        hidden: item.is_hidden.unwrap_or(false),
        release_type: None,
        year: None,
        release_id: None,
        track_number: None,
        disc_number: None,
        current_image_file_id: None,
        current_image_url: None,
        selected_artist_ids: Vec::new(),
        artists: Vec::new(),
        releases: Vec::new(),
        release_tracks: Vec::new(),
        available_covers: Vec::new(),
        metadata_tags: load_metadata_tags(pool, kind, item.id).await?,
        item,
    };

    match kind {
        "artists" => {
            let image_file_id: Option<i64> =
                sqlx::query_scalar("SELECT image_file_id FROM furumusic__artist WHERE id = $1")
                    .bind(detail.item.id)
                    .fetch_optional(pool)
                    .await?
                    .flatten();
            detail.current_image_url =
                image_file_id.map(|id| format!("/api/player/cover/{id}/large"));
            detail.current_image_file_id = image_file_id;
            detail.available_covers = artist_available_covers(pool, detail.item.id).await?;
        }
        "releases" => {
            let row: Option<(Option<String>, Option<i32>, Option<i64>)> = sqlx::query_as(
                "SELECT release_type::text, year, cover_file_id FROM furumusic__release WHERE id = $1",
            )
            .bind(detail.item.id)
            .fetch_optional(pool)
            .await?;
            if let Some((release_type, year, cover_file_id)) = row {
                detail.release_type = release_type;
                detail.year = year;
                detail.current_image_url =
                    cover_file_id.map(|id| format!("/api/player/cover/{id}/large"));
                detail.current_image_file_id = cover_file_id;
            }
            detail.selected_artist_ids = sqlx::query_as::<_, IdRow>(
                "SELECT artist_id AS id FROM furumusic__release_artist WHERE release_id = $1 ORDER BY position, artist_id",
            )
            .bind(detail.item.id)
            .fetch_all(pool)
            .await?
            .into_iter()
            .map(|row| row.id)
            .collect();
            detail.artists = load_artist_options(pool).await?;
            if detail.item.id > 0 {
                detail.release_tracks = load_release_tracks(pool, detail.item.id).await?;
            }
        }
        "tracks" => {
            let row: Option<(Option<i64>, Option<i32>, Option<i32>, Option<i32>)> = sqlx::query_as(
                "SELECT r.id AS release_id, t.track_number, t.disc_number, t.year \
                 FROM furumusic__track t \
                 LEFT JOIN furumusic__release r ON r.id = t.release_id \
                 WHERE t.id = $1",
            )
            .bind(detail.item.id)
            .fetch_optional(pool)
            .await?;
            if let Some((release_id, track_number, disc_number, year)) = row {
                detail.release_id = release_id;
                detail.track_number = track_number;
                detail.disc_number = disc_number;
                detail.year = year;
            }
            detail.selected_artist_ids = sqlx::query_as::<_, IdRow>(
                "SELECT artist_id AS id FROM furumusic__track_artist WHERE track_id = $1 AND role = 'main' ORDER BY position, artist_id",
            )
            .bind(detail.item.id)
            .fetch_all(pool)
            .await?
            .into_iter()
            .map(|row| row.id)
            .collect();
            detail.artists = load_artist_options(pool).await?;
            detail.releases = load_release_options(pool).await?;
        }
        _ => {}
    }

    Ok(detail)
}

async fn load_metadata_tags(
    pool: &PgPool,
    kind: &str,
    id: i64,
) -> anyhow::Result<Vec<MetadataTagDto>> {
    let entity_kind = match kind {
        "artists" => "artist",
        "releases" => "release",
        "tracks" => "track",
        _ => return Ok(Vec::new()),
    };
    let rows = sqlx::query_as::<_, (String, String, f64, String)>(
        r#"SELECT name, source, weight, updated_at
           FROM (
               SELECT g.name::text AS name,
                      egt.source::text AS source,
                      egt.weight,
                      egt.updated_at::text AS updated_at
                 FROM furumusic__entity_genre_tag egt
                 JOIN furumusic__genre g ON g.id = egt.genre_id
                WHERE egt.entity_kind = $1 AND egt.entity_id = $2
               UNION ALL
               SELECT g.name::text AS name,
                      'track_genre'::text AS source,
                      1.0::double precision AS weight,
                      ''::text AS updated_at
                 FROM furumusic__track_genre tg
                 JOIN furumusic__genre g ON g.id = tg.genre_id
                WHERE $1 = 'track'
                  AND tg.track_id = $2
                  AND NOT EXISTS (
                      SELECT 1
                        FROM furumusic__entity_genre_tag egt
                       WHERE egt.entity_kind = 'track'
                         AND egt.entity_id = tg.track_id
                         AND egt.genre_id = tg.genre_id
                  )
               UNION ALL
               SELECT g.name::text AS name,
                      ('release_' || egt.source)::text AS source,
                      egt.weight,
                      egt.updated_at::text AS updated_at
                 FROM furumusic__track t
                 JOIN furumusic__entity_genre_tag egt
                   ON egt.entity_kind = 'release'
                  AND egt.entity_id = t.release_id
                 JOIN furumusic__genre g ON g.id = egt.genre_id
                WHERE $1 = 'track'
                  AND t.id = $2
                  AND NOT EXISTS (
                      SELECT 1
                        FROM furumusic__entity_genre_tag direct_egt
                       WHERE direct_egt.entity_kind = 'track'
                         AND direct_egt.entity_id = t.id
                         AND direct_egt.genre_id = egt.genre_id
                  )
                  AND NOT EXISTS (
                      SELECT 1
                        FROM furumusic__track_genre tg
                       WHERE tg.track_id = t.id
                         AND tg.genre_id = egt.genre_id
                  )
           ) tags
           ORDER BY CASE source
                        WHEN 'lastfm' THEN 0
                        WHEN 'release_lastfm' THEN 1
                        WHEN 'review' THEN 2
                        WHEN 'release_review' THEN 3
                        WHEN 'file' THEN 4
                        WHEN 'release_file' THEN 5
                        WHEN 'track_genre' THEN 6
                        ELSE 7
                    END,
                    weight DESC,
                    name ASC"#,
    )
    .bind(entity_kind)
    .bind(id)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(name, source, weight, updated_at)| MetadataTagDto {
            name,
            source,
            weight,
            updated_at,
        })
        .collect())
}

async fn load_artist_options(pool: &PgPool) -> anyhow::Result<Vec<ArtistOptionDto>> {
    let rows = sqlx::query_as::<_, (i64, String)>(
        "SELECT id, name::text FROM furumusic__artist ORDER BY name ASC",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(id, name)| ArtistOptionDto { id, name })
        .collect())
}

async fn load_release_options(pool: &PgPool) -> anyhow::Result<Vec<ReleaseOptionDto>> {
    let rows = sqlx::query_as::<_, (i64, String, Option<String>)>(
        "SELECT r.id, r.title::text AS title, \
                (COALESCE(NULLIF(STRING_AGG(DISTINCT a.name::text, ', '), ''), 'Unknown artist') || COALESCE(' / ' || r.year::text, '')) AS subtitle \
         FROM furumusic__release r \
         LEFT JOIN furumusic__release_artist ra ON ra.release_id = r.id \
         LEFT JOIN furumusic__artist a ON a.id = ra.artist_id \
         GROUP BY r.id \
         ORDER BY r.title_sort ASC, r.year NULLS LAST, r.id ASC",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(id, title, subtitle)| ReleaseOptionDto {
            id,
            title,
            subtitle: subtitle.unwrap_or_default(),
        })
        .collect())
}

async fn create_release_library_item(
    pool: &PgPool,
    body: &UpdateLibraryItemRequest,
    title: &str,
    now: &str,
) -> anyhow::Result<i64> {
    let release_type = body
        .release_type
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("album");
    let year = body
        .year
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .and_then(|value| value.parse::<i32>().ok());
    let release_id: i64 = sqlx::query_scalar(
        "INSERT INTO furumusic__release \
           (title, title_sort, release_type, year, cover_file_id, total_tracks, total_discs, is_hidden, model_name, created_at, updated_at) \
         VALUES ($1, $2, $3, $4, NULL, NULL, NULL, $5, NULL, $6, $6) \
         RETURNING id",
    )
    .bind(title)
    .bind(normalize_name(title))
    .bind(release_type)
    .bind(year)
    .bind(body.hidden)
    .bind(now)
    .fetch_one(pool)
    .await?;

    if let Some(artist_ids) = body.artist_ids.as_deref() {
        set_release_artists(pool, release_id, artist_ids).await?;
    }
    if let Some(release_tracks) = body.release_tracks.as_deref() {
        update_release_tracks(pool, release_id, release_tracks, now).await?;
    }

    Ok(release_id)
}

async fn set_release_artists(
    pool: &PgPool,
    release_id: i64,
    artist_ids: &[i64],
) -> anyhow::Result<()> {
    sqlx::query("DELETE FROM furumusic__release_artist WHERE release_id = $1")
        .bind(release_id)
        .execute(pool)
        .await?;

    let mut seen_artist_ids = HashSet::new();
    let unique_artist_ids = artist_ids
        .iter()
        .copied()
        .filter(|id| *id > 0 && seen_artist_ids.insert(*id))
        .collect::<Vec<_>>();

    for (position, artist_id) in unique_artist_ids.iter().enumerate() {
        sqlx::query(
            "INSERT INTO furumusic__release_artist (release_id, artist_id, position) VALUES ($1, $2, $3)",
        )
        .bind(release_id)
        .bind(*artist_id)
        .bind(position as i32)
        .execute(pool)
        .await?;
    }

    Ok(())
}

async fn load_release_tracks(
    pool: &PgPool,
    release_id: i64,
) -> anyhow::Result<Vec<ReleaseTrackDto>> {
    let rows = sqlx::query_as::<_, ReleaseTrackRow>(
        "SELECT t.id, t.title::text AS title, \
                COALESCE(NULLIF(STRING_AGG(DISTINCT a.name::text, ', '), ''), 'Unknown artist') AS artists, \
                NULLIF(t.release_id, 0) AS release_id, r.title::text AS release_title, \
                t.track_number, t.disc_number, t.duration_seconds, t.is_hidden \
         FROM furumusic__track t \
         LEFT JOIN furumusic__release r ON r.id = t.release_id \
         LEFT JOIN furumusic__track_artist ta ON ta.track_id = t.id AND ta.role = 'main' \
         LEFT JOIN furumusic__artist a ON a.id = ta.artist_id \
         WHERE t.release_id = $1 \
         GROUP BY t.id, r.id, r.title \
         ORDER BY t.disc_number NULLS FIRST, t.track_number NULLS LAST, t.title ASC, t.id ASC",
    )
    .bind(release_id)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(release_track_dto).collect())
}

async fn search_tracks(
    pool: &PgPool,
    search: &str,
    limit: i64,
) -> anyhow::Result<Vec<ReleaseTrackDto>> {
    let pattern = format!("%{search}%");
    let starts_with = format!("{search}%");
    let rows = sqlx::query_as::<_, ReleaseTrackRow>(
        "SELECT t.id, t.title::text AS title, \
                COALESCE(NULLIF(STRING_AGG(DISTINCT a.name::text, ', '), ''), 'Unknown artist') AS artists, \
                NULLIF(t.release_id, 0) AS release_id, r.title::text AS release_title, \
                t.track_number, t.disc_number, t.duration_seconds, t.is_hidden \
         FROM furumusic__track t \
         LEFT JOIN furumusic__release r ON r.id = t.release_id \
         LEFT JOIN furumusic__track_artist ta ON ta.track_id = t.id AND ta.role = 'main' \
         LEFT JOIN furumusic__artist a ON a.id = ta.artist_id \
         WHERE t.title ILIKE $1 OR COALESCE(r.title::text, '') ILIKE $1 OR COALESCE(a.name::text, '') ILIKE $1 \
         GROUP BY t.id, r.id, r.title \
         ORDER BY CASE \
                    WHEN LOWER(t.title::text) = LOWER($2) THEN 0 \
                    WHEN t.title ILIKE $3 THEN 1 \
                    ELSE 2 \
                  END, \
                  t.title_sort ASC, t.id ASC \
         LIMIT $4",
    )
    .bind(pattern)
    .bind(search)
    .bind(starts_with)
    .bind(limit)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(release_track_dto).collect())
}

async fn update_release_tracks(
    pool: &PgPool,
    release_id: i64,
    tracks: &[ReleaseTrackUpdateRequest],
    now: &str,
) -> anyhow::Result<()> {
    let mut seen_ids = HashSet::new();
    let selected = tracks
        .iter()
        .filter(|track| track.id > 0 && seen_ids.insert(track.id))
        .collect::<Vec<_>>();
    let selected_ids = selected.iter().map(|track| track.id).collect::<Vec<_>>();

    let mut tx = pool.begin().await?;
    if selected_ids.is_empty() {
        sqlx::query(
            "UPDATE furumusic__track \
             SET release_id = 0, updated_at = $2 \
             WHERE release_id = $1",
        )
        .bind(release_id)
        .bind(now)
        .execute(&mut *tx)
        .await?;
    } else {
        sqlx::query(
            "UPDATE furumusic__track \
             SET release_id = 0, updated_at = $2 \
             WHERE release_id = $1 AND id <> ALL($3)",
        )
        .bind(release_id)
        .bind(now)
        .bind(&selected_ids)
        .execute(&mut *tx)
        .await?;
    }

    for track in selected {
        let track_number = parse_optional_admin_i32(track.track_number.as_deref(), 1, 9999);
        let disc_number = parse_optional_admin_i32(track.disc_number.as_deref(), 1, 999);
        sqlx::query(
            "UPDATE furumusic__track \
             SET release_id = $1, track_number = $2, disc_number = $3, updated_at = $4 \
             WHERE id = $5",
        )
        .bind(release_id)
        .bind(track_number)
        .bind(disc_number)
        .bind(now)
        .bind(track.id)
        .execute(&mut *tx)
        .await?;
    }

    tx.commit().await?;
    Ok(())
}

fn release_track_dto(row: ReleaseTrackRow) -> ReleaseTrackDto {
    ReleaseTrackDto {
        id: row.id,
        title: row.title,
        artists: row.artists,
        release_id: row.release_id,
        release_title: row.release_title,
        track_number: row.track_number,
        disc_number: row.disc_number,
        duration_seconds: row.duration_seconds,
        is_hidden: row.is_hidden,
    }
}

async fn artist_available_covers(
    pool: &PgPool,
    artist_id: i64,
) -> anyhow::Result<Vec<AvailableCoverDto>> {
    let rows = sqlx::query_as::<_, (i64, String)>(
        "SELECT DISTINCT r.cover_file_id AS media_file_id, r.title::text AS release_title \
         FROM furumusic__release r \
         LEFT JOIN furumusic__release_artist ra ON ra.release_id = r.id \
         LEFT JOIN furumusic__track t ON t.release_id = r.id \
         LEFT JOIN furumusic__track_artist ta ON ta.track_id = t.id \
         WHERE r.cover_file_id IS NOT NULL AND (ra.artist_id = $1 OR ta.artist_id = $1) \
         ORDER BY r.title::text ASC",
    )
    .bind(artist_id)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(media_file_id, release_title)| AvailableCoverDto {
            media_file_id,
            release_title,
            cover_url: format!("/api/player/cover/{media_file_id}/medium"),
        })
        .collect())
}

async fn library_ids_by_filter(
    pool: &PgPool,
    kind: &str,
    filter: LibraryFilter,
) -> cot::Result<Vec<i64>> {
    let search_pattern = clean_search(filter.search.as_deref()).map(|s| format!("%{s}%"));
    let mut qb = match kind {
        "releases" => QueryBuilder::<Postgres>::new(
            "SELECT DISTINCT r.id \
             FROM furumusic__release r \
             LEFT JOIN furumusic__release_artist ra ON ra.release_id = r.id \
             LEFT JOIN furumusic__artist a ON a.id = ra.artist_id WHERE 1=1",
        ),
        "tracks" => QueryBuilder::<Postgres>::new(
            "SELECT DISTINCT t.id \
             FROM furumusic__track t \
             LEFT JOIN furumusic__release r ON r.id = t.release_id \
             LEFT JOIN furumusic__track_artist ta ON ta.track_id = t.id \
             LEFT JOIN furumusic__artist a ON a.id = ta.artist_id WHERE 1=1",
        ),
        "playlists" => QueryBuilder::<Postgres>::new(
            "SELECT DISTINCT p.id \
             FROM furumusic__playlist p \
             LEFT JOIN furumusic__user u ON u.id = p.owner_id WHERE 1=1",
        ),
        _ => {
            QueryBuilder::<Postgres>::new("SELECT DISTINCT a.id FROM furumusic__artist a WHERE 1=1")
        }
    };
    push_library_search_filter(&mut qb, kind, search_pattern);
    let rows = qb
        .build_query_as::<IdRow>()
        .fetch_all(pool)
        .await
        .map_err(|e| cot::Error::internal(e.to_string()))?;
    Ok(rows.into_iter().map(|row| row.id).collect())
}

async fn apply_library_action(
    pool: &PgPool,
    kind: &str,
    action: &str,
    ids: &[i64],
    storage_dir: &str,
) -> cot::Result<u64> {
    match action {
        "hide" | "show" => set_library_visibility(pool, kind, ids, action == "hide").await,
        "delete" => delete_library_items(pool, kind, ids, storage_dir).await,
        _ => Ok(0),
    }
}

async fn set_library_visibility(
    pool: &PgPool,
    kind: &str,
    ids: &[i64],
    hidden: bool,
) -> cot::Result<u64> {
    let now = now_string();
    let result =
        match kind {
            "releases" => sqlx::query(
                "UPDATE furumusic__release SET is_hidden = $1, updated_at = $2 WHERE id = ANY($3)",
            )
            .bind(hidden)
            .bind(&now)
            .bind(ids)
            .execute(pool)
            .await,
            "playlists" => sqlx::query(
                "UPDATE furumusic__playlist SET is_public = $1, updated_at = $2 WHERE id = ANY($3)",
            )
            .bind(!hidden)
            .bind(&now)
            .bind(ids)
            .execute(pool)
            .await,
            "tracks" => sqlx::query(
                "UPDATE furumusic__track SET is_hidden = $1, updated_at = $2 WHERE id = ANY($3)",
            )
            .bind(hidden)
            .bind(&now)
            .bind(ids)
            .execute(pool)
            .await,
            _ => sqlx::query(
                "UPDATE furumusic__artist SET is_hidden = $1, updated_at = $2 WHERE id = ANY($3)",
            )
            .bind(hidden)
            .bind(&now)
            .bind(ids)
            .execute(pool)
            .await,
        }
        .map_err(|e| cot::Error::internal(e.to_string()))?;
    Ok(result.rows_affected())
}

async fn delete_library_items(
    pool: &PgPool,
    kind: &str,
    ids: &[i64],
    storage_dir: &str,
) -> cot::Result<u64> {
    match kind {
        "releases" => delete_releases(pool, ids, storage_dir).await,
        "tracks" => delete_tracks(pool, ids, storage_dir).await,
        "playlists" => delete_playlists(pool, ids).await,
        _ => delete_artists(pool, ids).await,
    }
}

async fn delete_artists(pool: &PgPool, ids: &[i64]) -> cot::Result<u64> {
    sqlx::query("DELETE FROM furumusic__user_followed_artist WHERE artist_id = ANY($1)")
        .bind(ids)
        .execute(pool)
        .await
        .map_err(|e| cot::Error::internal(e.to_string()))?;
    sqlx::query("DELETE FROM furumusic__track_artist WHERE artist_id = ANY($1)")
        .bind(ids)
        .execute(pool)
        .await
        .map_err(|e| cot::Error::internal(e.to_string()))?;
    sqlx::query("DELETE FROM furumusic__release_artist WHERE artist_id = ANY($1)")
        .bind(ids)
        .execute(pool)
        .await
        .map_err(|e| cot::Error::internal(e.to_string()))?;
    let result = sqlx::query("DELETE FROM furumusic__artist WHERE id = ANY($1)")
        .bind(ids)
        .execute(pool)
        .await
        .map_err(|e| cot::Error::internal(e.to_string()))?;
    Ok(result.rows_affected())
}

async fn delete_releases(pool: &PgPool, ids: &[i64], storage_dir: &str) -> cot::Result<u64> {
    crate::library_cleanup::delete_releases(pool, ids, storage_dir)
        .await
        .map_err(|error| cot::Error::internal(error.to_string()))
}

async fn delete_tracks(pool: &PgPool, ids: &[i64], storage_dir: &str) -> cot::Result<u64> {
    crate::library_cleanup::delete_tracks(pool, ids, storage_dir)
        .await
        .map_err(|error| cot::Error::internal(error.to_string()))
}

async fn delete_playlists(pool: &PgPool, ids: &[i64]) -> cot::Result<u64> {
    sqlx::query("DELETE FROM furumusic__playlist_track WHERE playlist_id = ANY($1)")
        .bind(ids)
        .execute(pool)
        .await
        .map_err(|e| cot::Error::internal(e.to_string()))?;
    sqlx::query("DELETE FROM furumusic__saved_playlist WHERE playlist_id = ANY($1)")
        .bind(ids)
        .execute(pool)
        .await
        .map_err(|e| cot::Error::internal(e.to_string()))?;
    let result = sqlx::query("DELETE FROM furumusic__playlist WHERE id = ANY($1)")
        .bind(ids)
        .execute(pool)
        .await
        .map_err(|e| cot::Error::internal(e.to_string()))?;
    Ok(result.rows_affected())
}

async fn count_library(
    pool: &PgPool,
    kind: &str,
    search_pattern: Option<String>,
) -> anyhow::Result<i64> {
    let mut qb = match kind {
        "releases" => QueryBuilder::<Postgres>::new(
            "SELECT COUNT(DISTINCT r.id) AS count \
             FROM furumusic__release r \
             LEFT JOIN furumusic__release_artist ra ON ra.release_id = r.id \
             LEFT JOIN furumusic__artist a ON a.id = ra.artist_id WHERE 1=1",
        ),
        "tracks" => QueryBuilder::<Postgres>::new(
            "SELECT COUNT(DISTINCT t.id) AS count \
             FROM furumusic__track t \
             LEFT JOIN furumusic__release r ON r.id = t.release_id \
             LEFT JOIN furumusic__track_artist ta ON ta.track_id = t.id \
             LEFT JOIN furumusic__artist a ON a.id = ta.artist_id WHERE 1=1",
        ),
        "playlists" => QueryBuilder::<Postgres>::new(
            "SELECT COUNT(DISTINCT p.id) AS count \
             FROM furumusic__playlist p \
             LEFT JOIN furumusic__user u ON u.id = p.owner_id WHERE 1=1",
        ),
        _ => QueryBuilder::<Postgres>::new(
            "SELECT COUNT(DISTINCT a.id) AS count FROM furumusic__artist a WHERE 1=1",
        ),
    };

    push_library_search_filter(&mut qb, kind, search_pattern);

    Ok(qb.build_query_as::<CountRow>().fetch_one(pool).await?.count)
}

fn push_library_search_filter(
    qb: &mut QueryBuilder<'_, Postgres>,
    kind: &str,
    search_pattern: Option<String>,
) {
    let Some(pattern) = search_pattern else {
        return;
    };
    match kind {
        "releases" => {
            qb.push(" AND (r.title ILIKE ");
            qb.push_bind(pattern.clone());
            qb.push(" OR a.name ILIKE ");
            qb.push_bind(pattern);
            qb.push(")");
        }
        "playlists" => {
            qb.push(" AND (p.title ILIKE ");
            qb.push_bind(pattern.clone());
            qb.push(" OR COALESCE(p.description, '') ILIKE ");
            qb.push_bind(pattern.clone());
            qb.push(" OR COALESCE(u.display_name, u.username, '') ILIKE ");
            qb.push_bind(pattern);
            qb.push(")");
        }
        "tracks" => {
            qb.push(" AND (t.title ILIKE ");
            qb.push_bind(pattern.clone());
            qb.push(" OR r.title ILIKE ");
            qb.push_bind(pattern.clone());
            qb.push(" OR a.name ILIKE ");
            qb.push_bind(pattern);
            qb.push(")");
        }
        _ => {
            qb.push(" AND a.name ILIKE ");
            qb.push_bind(pattern);
        }
    }
}

async fn load_artist_items(
    pool: &PgPool,
    search_pattern: Option<String>,
    limit: i64,
    offset: i64,
) -> anyhow::Result<Vec<LibraryItemRow>> {
    let mut qb = QueryBuilder::<Postgres>::new(
        "SELECT a.id, a.name::text AS title, NULL::text AS subtitle, a.is_hidden, \
                COUNT(DISTINCT ra.release_id)::bigint AS primary_count, \
                COUNT(DISTINCT ta.track_id)::bigint AS secondary_count, \
                COUNT(DISTINCT ufa.user_id)::bigint AS tertiary_count, \
                a.updated_at::text AS updated_at \
         FROM furumusic__artist a \
         LEFT JOIN furumusic__release_artist ra ON ra.artist_id = a.id \
         LEFT JOIN furumusic__track_artist ta ON ta.artist_id = a.id \
         LEFT JOIN furumusic__user_followed_artist ufa ON ufa.artist_id = a.id \
         WHERE 1=1",
    );
    if let Some(pattern) = search_pattern {
        qb.push(" AND a.name ILIKE ");
        qb.push_bind(pattern);
    }
    qb.push(" GROUP BY a.id ORDER BY secondary_count DESC, a.name ASC LIMIT ");
    qb.push_bind(limit);
    qb.push(" OFFSET ");
    qb.push_bind(offset);
    Ok(qb
        .build_query_as::<LibraryItemRow>()
        .fetch_all(pool)
        .await?)
}

async fn load_release_items(
    pool: &PgPool,
    search_pattern: Option<String>,
    limit: i64,
    offset: i64,
) -> anyhow::Result<Vec<LibraryItemRow>> {
    let mut qb = QueryBuilder::<Postgres>::new(
        "SELECT r.id, r.title::text AS title, \
                (COALESCE(NULLIF(STRING_AGG(DISTINCT a.name::text, ', '), ''), 'Unknown artist') || COALESCE(' / ' || r.year::text, '')) AS subtitle, \
                r.is_hidden, COUNT(DISTINCT t.id)::bigint AS primary_count, \
                COUNT(DISTINCT ra.artist_id)::bigint AS secondary_count, \
                COUNT(DISTINCT ph.id)::bigint AS tertiary_count, \
                r.updated_at::text AS updated_at \
         FROM furumusic__release r \
         LEFT JOIN furumusic__track t ON t.release_id = r.id \
         LEFT JOIN furumusic__release_artist ra ON ra.release_id = r.id \
         LEFT JOIN furumusic__artist a ON a.id = ra.artist_id \
         LEFT JOIN furumusic__play_history ph ON ph.track_id = t.id \
         WHERE 1=1",
    );
    if let Some(pattern) = search_pattern {
        qb.push(" AND (r.title ILIKE ");
        qb.push_bind(pattern.clone());
        qb.push(" OR a.name ILIKE ");
        qb.push_bind(pattern);
        qb.push(")");
    }
    qb.push(" GROUP BY r.id ORDER BY r.updated_at DESC, r.title ASC LIMIT ");
    qb.push_bind(limit);
    qb.push(" OFFSET ");
    qb.push_bind(offset);
    Ok(qb
        .build_query_as::<LibraryItemRow>()
        .fetch_all(pool)
        .await?)
}

async fn load_track_items(
    pool: &PgPool,
    search_pattern: Option<String>,
    limit: i64,
    offset: i64,
) -> anyhow::Result<Vec<LibraryItemRow>> {
    let mut qb = QueryBuilder::<Postgres>::new(
        "SELECT t.id, t.title::text AS title, \
                CONCAT(COALESCE(r.title::text, 'No release'), COALESCE(' / #' || t.track_number::text, '')) AS subtitle, \
                t.is_hidden, COUNT(DISTINCT ta.artist_id)::bigint AS primary_count, \
                COUNT(DISTINCT ph.id)::bigint AS secondary_count, \
                COUNT(DISTINCT pt.playlist_id)::bigint AS tertiary_count, \
                t.updated_at::text AS updated_at \
         FROM furumusic__track t \
         LEFT JOIN furumusic__release r ON r.id = t.release_id \
         LEFT JOIN furumusic__track_artist ta ON ta.track_id = t.id \
         LEFT JOIN furumusic__artist a ON a.id = ta.artist_id \
         LEFT JOIN furumusic__play_history ph ON ph.track_id = t.id \
         LEFT JOIN furumusic__playlist_track pt ON pt.track_id = t.id \
         WHERE 1=1",
    );
    if let Some(pattern) = search_pattern {
        qb.push(" AND (t.title ILIKE ");
        qb.push_bind(pattern.clone());
        qb.push(" OR r.title ILIKE ");
        qb.push_bind(pattern.clone());
        qb.push(" OR a.name ILIKE ");
        qb.push_bind(pattern);
        qb.push(")");
    }
    qb.push(" GROUP BY t.id, r.title ORDER BY COALESCE(r.title::text, '') ASC, t.disc_number NULLS FIRST, t.track_number NULLS FIRST, t.title ASC LIMIT ");
    qb.push_bind(limit);
    qb.push(" OFFSET ");
    qb.push_bind(offset);
    Ok(qb
        .build_query_as::<LibraryItemRow>()
        .fetch_all(pool)
        .await?)
}

async fn load_playlist_items(
    pool: &PgPool,
    search_pattern: Option<String>,
    limit: i64,
    offset: i64,
) -> anyhow::Result<Vec<LibraryItemRow>> {
    let mut qb = QueryBuilder::<Postgres>::new(
        "SELECT p.id, p.title::text AS title, \
                COALESCE(u.display_name, u.username, 'unknown') AS subtitle, \
                (NOT p.is_public) AS is_hidden, COUNT(pt.id)::bigint AS primary_count, \
                CASE WHEN p.is_public THEN 1 ELSE 0 END::bigint AS secondary_count, \
                0::bigint AS tertiary_count, p.updated_at::text AS updated_at \
         FROM furumusic__playlist p \
         LEFT JOIN furumusic__playlist_track pt ON pt.playlist_id = p.id \
         LEFT JOIN furumusic__user u ON u.id = p.owner_id \
         WHERE 1=1",
    );
    if let Some(pattern) = search_pattern {
        qb.push(" AND (p.title ILIKE ");
        qb.push_bind(pattern.clone());
        qb.push(" OR COALESCE(p.description, '') ILIKE ");
        qb.push_bind(pattern.clone());
        qb.push(" OR COALESCE(u.display_name, u.username, '') ILIKE ");
        qb.push_bind(pattern);
        qb.push(")");
    }
    qb.push(
        " GROUP BY p.id, u.display_name, u.username ORDER BY p.updated_at DESC, p.title ASC LIMIT ",
    );
    qb.push_bind(limit);
    qb.push(" OFFSET ");
    qb.push_bind(offset);
    Ok(qb
        .build_query_as::<LibraryItemRow>()
        .fetch_all(pool)
        .await?)
}

fn library_item_dto(kind: &str, row: LibraryItemRow) -> LibraryItemDto {
    let tags = match kind {
        "releases" => vec![
            tag(format!("{} tracks", row.primary_count), "count"),
            tag(format!("{} artists", row.secondary_count), "relation"),
            tag(format!("{} plays", row.tertiary_count), "plays"),
        ],
        "tracks" => vec![
            tag(format!("{} artists", row.primary_count), "relation"),
            tag(format!("{} plays", row.secondary_count), "plays"),
            tag(format!("{} playlists", row.tertiary_count), "count"),
        ],
        "playlists" => vec![
            tag(format!("{} tracks", row.primary_count), "count"),
            tag(
                if row.secondary_count > 0 {
                    "public"
                } else {
                    "private"
                },
                "visibility",
            ),
        ],
        _ => vec![
            tag(format!("{} tracks", row.secondary_count), "count"),
            tag(format!("{} releases", row.primary_count), "relation"),
            tag(format!("{} followers", row.tertiary_count), "followers"),
        ],
    };

    LibraryItemDto {
        id: row.id,
        kind: kind.to_owned(),
        title: row.title,
        subtitle: row.subtitle.unwrap_or_default(),
        is_hidden: row.is_hidden,
        tags,
        updated_at: row.updated_at,
    }
}

impl Default for ReviewFilter {
    fn default() -> Self {
        Self {
            status: None,
            search: None,
        }
    }
}

impl From<JobRunRow> for JobRunDto {
    fn from(row: JobRunRow) -> Self {
        Self {
            id: row.id,
            job_name: row.job_name,
            status: row.status,
            started_at: row.started_at,
            finished_at: row.finished_at,
            duration_ms: row.duration_ms,
            trigger: row.trigger,
            error_message: row.error_message,
            log_excerpt: row.log_excerpt,
        }
    }
}

impl From<JobRunDetailRow> for JobRunDto {
    fn from(row: JobRunDetailRow) -> Self {
        Self {
            id: row.id,
            job_name: row.job_name,
            status: row.status,
            started_at: row.started_at,
            finished_at: row.finished_at,
            duration_ms: row.duration_ms,
            trigger: row.trigger,
            error_message: row.error_message,
            log_excerpt: row.log_excerpt,
        }
    }
}

fn tag(label: impl Into<String>, kind: impl Into<String>) -> TagDto {
    TagDto {
        label: label.into(),
        kind: kind.into(),
    }
}

fn default_true() -> bool {
    true
}

fn optional_job_time(value: &str) -> Option<String> {
    if value.is_empty() {
        None
    } else {
        Some(value.to_owned())
    }
}

fn normalize_library_kind(kind: Option<&str>) -> String {
    let kind = kind.unwrap_or_default().trim().to_ascii_lowercase();
    match kind.as_str() {
        "release" | "releases" => "releases",
        "track" | "tracks" => "tracks",
        "playlist" | "playlists" => "playlists",
        "artist" | "artists" => "artists",
        _ => "artists",
    }
    .to_owned()
}

fn normalize_status(status: Option<&str>) -> Option<String> {
    let status = status?.trim();
    if status.is_empty() || status == "all" {
        return None;
    }
    Some(status.to_owned())
}

fn clean_search(search: Option<&str>) -> Option<String> {
    search
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.chars().take(120).collect())
}

fn normalize_name(value: &str) -> String {
    value.trim().to_lowercase()
}

fn parse_optional_admin_i32(value: Option<&str>, min: i32, max: i32) -> Option<i32> {
    let value = value?.trim();
    if value.is_empty() {
        return None;
    }
    value
        .parse::<i32>()
        .ok()
        .map(|parsed| parsed.clamp(min, max))
}

fn parse_merge_optional_i32(
    value: Option<&str>,
    min: i32,
    max: i32,
    field: &str,
) -> Result<Option<i32>, String> {
    let Some(value) = value.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(None);
    };
    let parsed = value
        .parse::<i32>()
        .map_err(|_| format!("{field} must be an integer from {min} to {max}"))?;
    if !(min..=max).contains(&parsed) {
        return Err(format!("{field} must be from {min} to {max}"));
    }
    Ok(Some(parsed))
}

fn deserialize_optional_stringish<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: Deserializer<'de>,
{
    let Some(value) = Option::<serde_json::Value>::deserialize(deserializer)? else {
        return Ok(None);
    };
    match value {
        serde_json::Value::Null => Ok(None),
        serde_json::Value::String(value) => Ok(Some(value)),
        serde_json::Value::Number(value) => Ok(Some(value.to_string())),
        other => Err(serde::de::Error::custom(format!(
            "expected string, number, or null, got {other}"
        ))),
    }
}

fn now_string() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

fn context_sha256(context_json: &str) -> Option<String> {
    let value = serde_json::from_str::<serde_json::Value>(context_json).ok()?;
    let sha = value.get("sha256")?.as_str()?.trim();
    let is_sha256 = sha.len() == 64 && sha.chars().all(|ch| ch.is_ascii_hexdigit());
    is_sha256.then(|| sha.to_ascii_lowercase())
}

fn file_name(path: &str) -> String {
    path.replace('\\', "/")
        .rsplit('/')
        .next()
        .unwrap_or(path)
        .to_owned()
}

fn compact_path_tail(path: &str, max_chars: usize) -> String {
    let normalized = path.replace('\\', "/");
    if normalized.chars().count() <= max_chars {
        return normalized;
    }
    let filename = file_name(&normalized);
    let filename_len = filename.chars().count();
    if filename_len + 4 <= max_chars {
        return format!(".../{filename}");
    }
    let suffix_len = max_chars.saturating_sub(3);
    let suffix = filename
        .chars()
        .skip(filename_len.saturating_sub(suffix_len))
        .collect::<String>();
    format!("...{suffix}")
}

fn file_extension(filename: &str) -> Option<String> {
    std::path::Path::new(filename)
        .extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| ext.trim().to_ascii_lowercase())
        .filter(|ext| !ext.is_empty())
}

fn size_display(bytes: i64) -> String {
    if bytes >= 1_073_741_824 {
        format!("{:.1} GB", bytes as f64 / 1_073_741_824.0)
    } else if bytes >= 1_048_576 {
        format!("{:.1} MB", bytes as f64 / 1_048_576.0)
    } else if bytes >= 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{bytes} B")
    }
}

#[cfg(test)]
mod tests {
    use super::parse_merge_optional_i32;

    #[test]
    fn merge_numbers_preserve_empty_values_and_reject_invalid_input() {
        assert_eq!(
            parse_merge_optional_i32(None, 1, 9999, "track number").unwrap(),
            None
        );
        assert_eq!(
            parse_merge_optional_i32(Some(" 38 "), 1, 9999, "track number").unwrap(),
            Some(38)
        );
        assert!(parse_merge_optional_i32(Some("0"), 1, 9999, "track number").is_err());
        assert!(parse_merge_optional_i32(Some("nope"), 1, 9999, "track number").is_err());
    }
}
