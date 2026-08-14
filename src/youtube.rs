use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, bail};
use image::codecs::jpeg::JpegEncoder;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::{FromRow, PgPool};
use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};
use tokio::process::Command;
use tokio::sync::{Mutex, OnceCell, Semaphore};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::scheduler::SchedulerHandle;

const YOUTUBE_LIST_LIMIT: i64 = 100;
const RESOLVE_TIMEOUT: Duration = Duration::from_secs(180);
const HTTP_403_RETRY_DELAY: Duration = Duration::from_secs(30);
const HTTP_403_MAX_RETRIES: usize = 3;
const MAX_ERROR_LEN: usize = 4_000;
const AUDIO_EXTENSIONS: &[&str] = &[
    "mp3", "flac", "ogg", "opus", "aac", "m4a", "wav", "ape", "wv", "wma", "tta", "aiff", "aif",
];
const IMAGE_EXTENSIONS: &[&str] = &["jpg", "jpeg", "png", "webp", "bmp", "gif"];

#[derive(Debug, Deserialize)]
pub struct YouTubePreviewRequest {
    pub url: String,
}

#[derive(Debug, Deserialize)]
pub struct YouTubeStartRequest {
    pub url: String,
    pub selected_source_ids: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct YouTubePreviewItemDto {
    pub source_id: String,
    pub title: String,
    pub playlist_index: i32,
    pub selected_by_default: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct YouTubePreviewDto {
    pub source_url: String,
    pub title: String,
    pub source_kind: String,
    pub items: Vec<YouTubePreviewItemDto>,
}

#[derive(Debug, Clone, Serialize)]
pub struct YouTubeItemDto {
    pub id: String,
    pub source_id: String,
    pub source_url: String,
    pub title: String,
    pub playlist_index: i32,
    pub status: String,
    pub progress_percent: f64,
    pub downloaded_bytes: u64,
    pub total_bytes: Option<u64>,
    pub speed_bytes_per_sec: Option<u64>,
    pub eta_seconds: Option<u64>,
    pub chapter_count: i32,
    pub audio_file_count: i32,
    pub error: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub completed_at: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct YouTubeJobDto {
    pub id: String,
    pub source_url: String,
    pub title: String,
    pub source_kind: String,
    pub status: String,
    pub total_items: i32,
    pub completed_items: i32,
    pub failed_items: i32,
    pub review_items: i32,
    pub error: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub completed_at: Option<String>,
    pub items: Vec<YouTubeItemDto>,
}

#[derive(Debug, Clone, FromRow)]
struct YouTubeJobRow {
    id: String,
    user_id: i64,
    source_url: String,
    title: String,
    source_kind: String,
    status: String,
    total_items: i32,
    completed_items: i32,
    failed_items: i32,
    review_items: i32,
    error: Option<String>,
    created_at: String,
    updated_at: String,
    completed_at: Option<String>,
}

#[derive(Debug, Clone, FromRow)]
struct YouTubeItemRow {
    id: String,
    job_id: String,
    source_id: String,
    source_url: String,
    title: String,
    playlist_index: i32,
    status: String,
    progress_percent: f64,
    downloaded_bytes: i64,
    total_bytes: Option<i64>,
    speed_bytes_per_sec: Option<i64>,
    eta_seconds: Option<i64>,
    chapter_count: i32,
    audio_file_count: i32,
    inbox_path: Option<String>,
    error: Option<String>,
    created_at: String,
    updated_at: String,
    completed_at: Option<String>,
}

impl YouTubeItemRow {
    fn dto(&self) -> YouTubeItemDto {
        YouTubeItemDto {
            id: self.id.clone(),
            source_id: self.source_id.clone(),
            source_url: self.source_url.clone(),
            title: self.title.clone(),
            playlist_index: self.playlist_index,
            status: self.status.clone(),
            progress_percent: self.progress_percent.clamp(0.0, 100.0),
            downloaded_bytes: non_negative(self.downloaded_bytes),
            total_bytes: self.total_bytes.map(non_negative),
            speed_bytes_per_sec: self.speed_bytes_per_sec.map(non_negative),
            eta_seconds: self.eta_seconds.map(non_negative),
            chapter_count: self.chapter_count,
            audio_file_count: self.audio_file_count,
            error: self.error.clone(),
            created_at: self.created_at.clone(),
            updated_at: self.updated_at.clone(),
            completed_at: self.completed_at.clone(),
        }
    }
}

impl YouTubeJobRow {
    fn dto(&self, items: Vec<YouTubeItemDto>) -> YouTubeJobDto {
        YouTubeJobDto {
            id: self.id.clone(),
            source_url: self.source_url.clone(),
            title: self.title.clone(),
            source_kind: self.source_kind.clone(),
            status: self.status.clone(),
            total_items: self.total_items,
            completed_items: self.completed_items,
            failed_items: self.failed_items,
            review_items: self.review_items,
            error: self.error.clone(),
            created_at: self.created_at.clone(),
            updated_at: self.updated_at.clone(),
            completed_at: self.completed_at.clone(),
            items,
        }
    }
}

#[derive(Debug)]
struct ResolvedSource {
    title: String,
    kind: String,
    items: Vec<ResolvedItem>,
}

#[derive(Debug)]
struct ResolvedItem {
    source_id: String,
    source_url: String,
    title: String,
    playlist_index: i32,
}

#[derive(Debug)]
struct PreparedFolder {
    inbox_path: Option<String>,
    chapter_count: i32,
    audio_file_count: i32,
    all_files_known: bool,
}

#[derive(Debug)]
struct YtDlpDownloadFailure {
    message: String,
    http_forbidden: bool,
}

impl std::fmt::Display for YtDlpDownloadFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for YtDlpDownloadFailure {}

pub struct YouTubeService {
    running_jobs: Mutex<HashSet<String>>,
    cancellations: Mutex<HashMap<String, CancellationToken>>,
    concurrency: Arc<Semaphore>,
    scheduler_handle: Arc<OnceCell<Arc<SchedulerHandle>>>,
}

impl YouTubeService {
    pub fn new(scheduler_handle: Arc<OnceCell<Arc<SchedulerHandle>>>) -> Self {
        Self {
            running_jobs: Mutex::new(HashSet::new()),
            cancellations: Mutex::new(HashMap::new()),
            concurrency: Arc::new(Semaphore::new(2)),
            scheduler_handle,
        }
    }

    pub async fn preview(
        &self,
        request: YouTubePreviewRequest,
        proxy_url: Option<&str>,
    ) -> anyhow::Result<YouTubePreviewDto> {
        let url = validate_youtube_url(&request.url)?;
        let resolved = resolve_source(&url, proxy_url).await?;
        let requested_video_id = requested_video_id(&url);
        let select_requested_only = resolved.kind == "playlist" && requested_video_id.is_some();
        Ok(YouTubePreviewDto {
            source_url: url,
            title: resolved.title,
            source_kind: resolved.kind,
            items: resolved
                .items
                .into_iter()
                .map(|item| {
                    let selected_by_default = !select_requested_only
                        || requested_video_id.as_deref() == Some(item.source_id.as_str());
                    YouTubePreviewItemDto {
                        source_id: item.source_id,
                        title: item.title,
                        playlist_index: item.playlist_index,
                        selected_by_default,
                    }
                })
                .collect(),
        })
    }

    pub async fn start(
        self: &Arc<Self>,
        pool: &PgPool,
        user_id: i64,
        request: YouTubeStartRequest,
        inbox_dir: &str,
        proxy_url: Option<String>,
    ) -> anyhow::Result<YouTubeJobDto> {
        let url = validate_youtube_url(&request.url)?;
        validate_inbox_dir(inbox_dir)?;
        let selected: HashSet<String> = request
            .selected_source_ids
            .into_iter()
            .map(|id| id.trim().to_string())
            .filter(|id| !id.is_empty())
            .collect();
        if selected.is_empty() {
            bail!("select at least one YouTube video to import");
        }
        if selected.iter().any(|id| !valid_source_id(id)) {
            bail!("YouTube selection contains an invalid video ID");
        }

        let resolved = resolve_source(&url, proxy_url.as_deref()).await?;
        let selected_items: Vec<ResolvedItem> = resolved
            .items
            .into_iter()
            .filter(|item| selected.contains(&item.source_id))
            .collect();
        if selected_items.len() != selected.len() {
            bail!("YouTube selection no longer matches the parsed link; preview it again");
        }

        let id = Uuid::new_v4().to_string();
        let now = now_string();
        let source_ids: Vec<String> = selected_items
            .iter()
            .map(|item| item.source_id.clone())
            .collect();
        let already_imported = already_imported_source_ids(pool, user_id, &source_ids).await?;
        let mut transaction = pool.begin().await?;
        sqlx::query(
            r#"INSERT INTO furumusic__youtube_download
                  (id, user_id, source_url, title, source_kind, status,
                   total_items, completed_items, failed_items, review_items,
                   error, created_at, updated_at, completed_at)
               VALUES ($1, $2, $3, $4, $5, 'queued', $6, 0, 0, 0,
                       NULL, $7, $7, NULL)"#,
        )
        .bind(&id)
        .bind(user_id)
        .bind(&url)
        .bind(&resolved.title)
        .bind(&resolved.kind)
        .bind(i32::try_from(selected_items.len()).unwrap_or(i32::MAX))
        .bind(&now)
        .execute(&mut *transaction)
        .await?;

        for item in selected_items {
            let is_already_imported = already_imported.contains(&item.source_id);
            let item_id = Uuid::new_v4().to_string();
            let status = if is_already_imported {
                "skipped"
            } else {
                "queued"
            };
            let progress = if is_already_imported { 100.0 } else { 0.0 };
            let completed_at = is_already_imported.then(|| now.clone());
            sqlx::query(
                r#"INSERT INTO furumusic__youtube_download_item
                      (id, job_id, source_id, source_url, title, playlist_index,
                       status, progress_percent, downloaded_bytes, total_bytes,
                       speed_bytes_per_sec, eta_seconds, chapter_count,
                       audio_file_count, inbox_path, error, created_at, updated_at,
                       completed_at)
                   VALUES ($1, $2, $3, $4, $5, $6, $7, $8, 0, NULL, NULL,
                           NULL, 0, 0, NULL, NULL, $9, $9, $10)"#,
            )
            .bind(&item_id)
            .bind(&id)
            .bind(&item.source_id)
            .bind(&item.source_url)
            .bind(&item.title)
            .bind(item.playlist_index)
            .bind(status)
            .bind(progress)
            .bind(&now)
            .bind(completed_at)
            .execute(&mut *transaction)
            .await?;
        }
        transaction.commit().await?;

        self.spawn_job(pool.clone(), id.clone(), inbox_dir.to_string(), proxy_url)
            .await;
        load_job_dto(pool, user_id, &id).await
    }

    pub async fn list(
        self: &Arc<Self>,
        pool: &PgPool,
        user_id: i64,
        inbox_dir: &str,
        proxy_url: Option<String>,
    ) -> anyhow::Result<Vec<YouTubeJobDto>> {
        validate_inbox_dir(inbox_dir)?;
        sync_ai_statuses(pool, user_id).await?;

        let resumable: Vec<(String, String)> = sqlx::query_as(
            r#"SELECT id, status::text
               FROM furumusic__youtube_download
               WHERE user_id = $1
                 AND status IN ('queued', 'resolving', 'downloading', 'postprocessing')
               ORDER BY created_at"#,
        )
        .bind(user_id)
        .fetch_all(pool)
        .await?;
        for (id, _) in resumable {
            self.spawn_job(pool.clone(), id, inbox_dir.to_string(), proxy_url.clone())
                .await;
        }

        let ids: Vec<String> = sqlx::query_scalar(
            r#"SELECT id FROM furumusic__youtube_download
               WHERE user_id = $1 ORDER BY created_at DESC, id DESC LIMIT $2"#,
        )
        .bind(user_id)
        .bind(YOUTUBE_LIST_LIMIT)
        .fetch_all(pool)
        .await?;
        for id in &ids {
            refresh_parent(pool, id).await?;
        }

        let mut jobs = Vec::with_capacity(ids.len());
        for id in ids {
            jobs.push(load_job_dto(pool, user_id, &id).await?);
        }
        Ok(jobs)
    }

    pub async fn retry(
        self: &Arc<Self>,
        pool: &PgPool,
        user_id: i64,
        id: &str,
        inbox_dir: &str,
        proxy_url: Option<String>,
    ) -> anyhow::Result<YouTubeJobDto> {
        validate_inbox_dir(inbox_dir)?;
        let job = load_job_row(pool, user_id, id).await?;
        if is_active_job_status(&job.status) {
            bail!("YouTube download is already active");
        }

        let now = now_string();
        sqlx::query(
            r#"UPDATE furumusic__youtube_download_item
               SET status = 'queued', progress_percent = 0, downloaded_bytes = 0,
                   total_bytes = NULL, speed_bytes_per_sec = NULL, eta_seconds = NULL,
                   error = NULL, completed_at = NULL, updated_at = $2
               WHERE job_id = $1 AND status = 'failed'"#,
        )
        .bind(id)
        .bind(&now)
        .execute(pool)
        .await?;
        sqlx::query(
            r#"UPDATE furumusic__youtube_download
               SET status = 'queued', error = NULL, completed_at = NULL, updated_at = $2
               WHERE id = $1 AND user_id = $3"#,
        )
        .bind(id)
        .bind(&now)
        .bind(user_id)
        .execute(pool)
        .await?;

        self.spawn_job(
            pool.clone(),
            id.to_string(),
            inbox_dir.to_string(),
            proxy_url,
        )
        .await;
        load_job_dto(pool, user_id, id).await
    }

    pub async fn cancel(
        &self,
        pool: &PgPool,
        user_id: i64,
        id: &str,
    ) -> anyhow::Result<YouTubeJobDto> {
        let job = load_job_row(pool, user_id, id).await?;
        if job.status == "cancelled" {
            return load_job_dto(pool, user_id, id).await;
        }
        if !is_cancellable_job_status(&job.status) {
            bail!("this YouTube import can no longer be stopped");
        }

        let now = now_string();
        let mut transaction = pool.begin().await?;
        sqlx::query(
            r#"UPDATE furumusic__youtube_download_item
               SET status = 'cancelled', speed_bytes_per_sec = NULL,
                   eta_seconds = NULL, error = NULL, completed_at = $2,
                   updated_at = $2
               WHERE job_id = $1
                 AND status IN ('queued', 'downloading', 'postprocessing')"#,
        )
        .bind(id)
        .bind(&now)
        .execute(&mut *transaction)
        .await?;
        sqlx::query(
            r#"UPDATE furumusic__youtube_download
               SET status = 'cancelled', error = NULL, completed_at = $2,
                   updated_at = $2
               WHERE id = $1 AND user_id = $3"#,
        )
        .bind(id)
        .bind(&now)
        .bind(user_id)
        .execute(&mut *transaction)
        .await?;
        transaction.commit().await?;

        if let Some(token) = self.cancellations.lock().await.get(id).cloned() {
            token.cancel();
        }
        refresh_parent(pool, id).await?;
        load_job_dto(pool, user_id, id).await
    }

    pub async fn remove(
        &self,
        pool: &PgPool,
        user_id: i64,
        id: &str,
        inbox_dir: &str,
    ) -> anyhow::Result<()> {
        let job = load_job_row(pool, user_id, id).await?;
        if is_active_job_status(&job.status) || self.running_jobs.lock().await.contains(id) {
            bail!("active YouTube downloads cannot be removed");
        }

        let result =
            sqlx::query("DELETE FROM furumusic__youtube_download WHERE id = $1 AND user_id = $2")
                .bind(id)
                .bind(user_id)
                .execute(pool)
                .await?;
        if result.rows_affected() == 0 {
            bail!("YouTube download not found");
        }

        if let Ok(inbox_root) = validate_inbox_dir(inbox_dir) {
            let staging = staging_job_root(&inbox_root, id);
            if tokio::fs::try_exists(&staging).await.unwrap_or(false) {
                let _ = tokio::fs::remove_dir_all(staging).await;
            }
        }
        Ok(())
    }

    async fn spawn_job(
        self: &Arc<Self>,
        pool: PgPool,
        id: String,
        inbox_dir: String,
        proxy_url: Option<String>,
    ) {
        {
            let mut running = self.running_jobs.lock().await;
            if !running.insert(id.clone()) {
                return;
            }
        }
        let cancel = CancellationToken::new();
        self.cancellations
            .lock()
            .await
            .insert(id.clone(), cancel.clone());

        let service = Arc::clone(self);
        tokio::spawn(async move {
            let permit = tokio::select! {
                permit = Arc::clone(&service.concurrency).acquire_owned() => permit.ok(),
                _ = cancel.cancelled() => None,
            };
            let result = if let Some(permit) = permit {
                let result = service
                    .run_job(&pool, &id, &inbox_dir, proxy_url.as_deref(), &cancel)
                    .await;
                drop(permit);
                result
            } else {
                Ok(())
            };
            if let Err(err) = result
                && !cancel.is_cancelled()
            {
                tracing::error!(job_id = %id, error = %err, "YouTube download job failed");
                let _ = fail_parent(&pool, &id, &err.to_string()).await;
            }
            if cancel.is_cancelled()
                && let Ok(inbox_root) = validate_inbox_dir(&inbox_dir)
            {
                let _ = tokio::fs::remove_dir_all(staging_job_root(&inbox_root, &id)).await;
            }
            service.running_jobs.lock().await.remove(&id);
            service.cancellations.lock().await.remove(&id);
        });
    }

    async fn run_job(
        &self,
        pool: &PgPool,
        id: &str,
        inbox_dir: &str,
        proxy_url: Option<&str>,
        cancel: &CancellationToken,
    ) -> anyhow::Result<()> {
        if cancel.is_cancelled() {
            return Ok(());
        }
        let inbox_root = validate_inbox_dir(inbox_dir)?;
        let job: YouTubeJobRow = sqlx::query_as(
            r#"SELECT id, user_id, source_url, title, source_kind, status,
                      total_items, completed_items, failed_items, review_items,
                      error, created_at, updated_at, completed_at
               FROM furumusic__youtube_download WHERE id = $1"#,
        )
        .bind(id)
        .fetch_one(pool)
        .await?;

        let mut items = load_items(pool, id).await?;
        if items.is_empty() {
            if cancel.is_cancelled() {
                return Ok(());
            }
            set_parent_status(pool, id, "resolving", None).await?;
            let resolved = resolve_source(&job.source_url, proxy_url).await?;
            if cancel.is_cancelled() {
                return Ok(());
            }
            let now = now_string();
            sqlx::query(
                r#"UPDATE furumusic__youtube_download
                   SET title = $2, source_kind = $3, total_items = $4,
                       status = 'queued', error = NULL, updated_at = $5
                   WHERE id = $1"#,
            )
            .bind(id)
            .bind(&resolved.title)
            .bind(&resolved.kind)
            .bind(i32::try_from(resolved.items.len()).unwrap_or(i32::MAX))
            .bind(&now)
            .execute(pool)
            .await?;

            for item in resolved.items {
                let already_imported =
                    source_already_imported(pool, job.user_id, id, &item.source_id).await?;
                let item_id = Uuid::new_v4().to_string();
                let status = if already_imported {
                    "skipped"
                } else {
                    "queued"
                };
                let progress = if already_imported { 100.0 } else { 0.0 };
                let completed_at = already_imported.then(|| now.clone());
                sqlx::query(
                    r#"INSERT INTO furumusic__youtube_download_item
                          (id, job_id, source_id, source_url, title, playlist_index,
                           status, progress_percent, downloaded_bytes, total_bytes,
                           speed_bytes_per_sec, eta_seconds, chapter_count,
                           audio_file_count, inbox_path, error, created_at, updated_at,
                           completed_at)
                       VALUES ($1, $2, $3, $4, $5, $6, $7, $8, 0, NULL, NULL,
                               NULL, 0, 0, NULL, NULL, $9, $9, $10)"#,
                )
                .bind(&item_id)
                .bind(id)
                .bind(&item.source_id)
                .bind(&item.source_url)
                .bind(&item.title)
                .bind(item.playlist_index)
                .bind(status)
                .bind(progress)
                .bind(&now)
                .bind(completed_at)
                .execute(pool)
                .await?;
            }
            items = load_items(pool, id).await?;
        }

        for item in items {
            if cancel.is_cancelled() {
                break;
            }
            if !matches!(
                item.status.as_str(),
                "queued" | "downloading" | "postprocessing"
            ) {
                continue;
            }
            if let Err(err) = self
                .process_item(pool, &job, &item, &inbox_root, proxy_url, cancel)
                .await
            {
                if cancel.is_cancelled() {
                    break;
                }
                tracing::warn!(
                    job_id = %id,
                    item_id = %item.id,
                    source_id = %item.source_id,
                    error = %err,
                    "YouTube playlist item failed"
                );
                fail_item(pool, &item.id, &err.to_string()).await?;
            }
            refresh_parent(pool, id).await?;
        }

        self.trigger_discover().await;
        let _ = tokio::fs::remove_dir(staging_job_root(&inbox_root, id)).await;
        refresh_parent(pool, id).await?;
        Ok(())
    }

    async fn process_item(
        &self,
        pool: &PgPool,
        job: &YouTubeJobRow,
        item: &YouTubeItemRow,
        inbox_root: &Path,
        proxy_url: Option<&str>,
        cancel: &CancellationToken,
    ) -> anyhow::Result<()> {
        if cancel.is_cancelled() {
            bail!("YouTube import cancelled");
        }
        set_item_status(pool, &item.id, "downloading", None).await?;
        set_parent_status(pool, &job.id, "downloading", None).await?;

        let stage = staging_item_root(inbox_root, &job.id, &item.id);
        tokio::fs::create_dir_all(&stage).await?;
        let mut forbidden_retries = 0;
        loop {
            match run_ytdlp_download(pool, &item.id, &item.source_url, &stage, proxy_url, cancel)
                .await
            {
                Ok(()) => break,
                Err(error)
                    if !cancel.is_cancelled()
                        && forbidden_retries < HTTP_403_MAX_RETRIES
                        && is_http_403_download_failure(&error) =>
                {
                    forbidden_retries += 1;
                    tracing::warn!(
                        job_id = %job.id,
                        item_id = %item.id,
                        source_id = %item.source_id,
                        delay_seconds = HTTP_403_RETRY_DELAY.as_secs(),
                        "yt-dlp received HTTP 403; restarting the download after a delay"
                    );
                    set_item_status(
                        pool,
                        &item.id,
                        "downloading",
                        Some("HTTP 403 received; retrying automatically in 30 seconds"),
                    )
                    .await?;
                    tokio::select! {
                        _ = tokio::time::sleep(HTTP_403_RETRY_DELAY) => {}
                        _ = cancel.cancelled() => bail!("YouTube import cancelled"),
                    }
                    set_item_status(pool, &item.id, "downloading", None).await?;
                }
                Err(error) => return Err(error),
            }
        }

        if cancel.is_cancelled() {
            bail!("YouTube import cancelled");
        }

        set_item_status(pool, &item.id, "postprocessing", None).await?;
        set_parent_status(pool, &job.id, "postprocessing", None).await?;
        let prepared =
            prepare_downloaded_folder(pool, inbox_root, job.user_id, item, &stage, cancel).await?;

        let now = now_string();
        if prepared.all_files_known {
            sqlx::query(
                r#"UPDATE furumusic__youtube_download_item
                   SET status = 'skipped', progress_percent = 100,
                       chapter_count = $2, audio_file_count = 0,
                       inbox_path = NULL, error = NULL, completed_at = $3,
                       updated_at = $3 WHERE id = $1"#,
            )
            .bind(&item.id)
            .bind(prepared.chapter_count)
            .bind(&now)
            .execute(pool)
            .await?;
            return Ok(());
        }

        let inbox_path = prepared
            .inbox_path
            .context("prepared YouTube folder has no inbox path")?;
        sqlx::query(
            r#"UPDATE furumusic__youtube_download_item
               SET status = 'awaiting_ai', progress_percent = 100,
                   chapter_count = $2, audio_file_count = $3, inbox_path = $4,
                   error = NULL, completed_at = NULL, updated_at = $5
               WHERE id = $1"#,
        )
        .bind(&item.id)
        .bind(prepared.chapter_count)
        .bind(prepared.audio_file_count)
        .bind(&inbox_path)
        .bind(&now)
        .execute(pool)
        .await?;
        self.trigger_discover().await;
        Ok(())
    }

    async fn trigger_discover(&self) {
        if let Some(handle) = self.scheduler_handle.get() {
            let handle = Arc::clone(handle);
            tokio::spawn(async move {
                if let Err(err) = handle.trigger_job_now("inbox_discover").await {
                    tracing::warn!(
                        "failed to trigger inbox_discover after YouTube download: {err}"
                    );
                }
            });
        }
    }
}

async fn resolve_source(url: &str, proxy_url: Option<&str>) -> anyhow::Result<ResolvedSource> {
    let mut command = base_ytdlp_command(proxy_url);
    command
        .arg("--flat-playlist")
        .arg("--dump-single-json")
        .arg("--skip-download")
        .arg("--ignore-errors")
        .arg("--no-warnings")
        .arg("--")
        .arg(url)
        .kill_on_drop(true);

    let output = tokio::time::timeout(RESOLVE_TIMEOUT, command.output())
        .await
        .context("yt-dlp metadata resolution timed out")??;
    let stdout = String::from_utf8_lossy(&output.stdout);
    if stdout.trim().is_empty() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("yt-dlp could not resolve URL: {}", useful_error(&stderr));
    }
    let value: serde_json::Value =
        serde_json::from_str(stdout.trim()).context("yt-dlp returned invalid metadata JSON")?;
    if value.is_null() {
        bail!("yt-dlp could not resolve a public YouTube video or playlist");
    }

    let parent_title = json_string(&value, "title").unwrap_or_else(|| "YouTube download".into());
    if let Some(entries) = value.get("entries").and_then(|v| v.as_array()) {
        let mut items = Vec::new();
        let mut seen = HashSet::new();
        for (index, entry) in entries.iter().enumerate() {
            let Some(source_id) = json_string(entry, "id").filter(|id| valid_source_id(id)) else {
                continue;
            };
            if !seen.insert(source_id.clone()) {
                continue;
            }
            let title = json_string(entry, "title").unwrap_or_else(|| source_id.clone());
            let playlist_index = json_positive_i32(entry, "playlist_index")
                .unwrap_or_else(|| i32::try_from(index + 1).unwrap_or(i32::MAX));
            items.push(ResolvedItem {
                source_url: format!("https://www.youtube.com/watch?v={source_id}"),
                source_id,
                title,
                playlist_index,
            });
        }
        if items.is_empty() {
            bail!("the playlist contains no available public YouTube videos");
        }
        return Ok(ResolvedSource {
            title: parent_title,
            kind: "playlist".into(),
            items,
        });
    }

    let source_id = json_string(&value, "id")
        .filter(|id| valid_source_id(id))
        .context("yt-dlp metadata has no valid YouTube video ID")?;
    Ok(ResolvedSource {
        title: parent_title.clone(),
        kind: "video".into(),
        items: vec![ResolvedItem {
            source_url: format!("https://www.youtube.com/watch?v={source_id}"),
            source_id,
            title: parent_title,
            playlist_index: 1,
        }],
    })
}

fn base_ytdlp_command(proxy_url: Option<&str>) -> Command {
    let mut command = Command::new("yt-dlp");
    command
        .arg("--no-config")
        .arg("--no-cookies")
        .arg("--no-cookies-from-browser")
        .arg("--js-runtimes")
        .arg("deno")
        .stdin(Stdio::null());
    if let Some(proxy_url) = proxy_url {
        command.arg("--proxy").arg(proxy_url);
    }
    command
}

async fn run_ytdlp_download(
    pool: &PgPool,
    item_id: &str,
    url: &str,
    stage: &Path,
    proxy_url: Option<&str>,
    cancel: &CancellationToken,
) -> anyhow::Result<()> {
    let mut command = base_ytdlp_command(proxy_url);
    command
        .arg("--no-playlist")
        .arg("--continue")
        .arg("--newline")
        .arg("--no-colors")
        .arg("--progress-delta")
        .arg("1")
        .arg("--progress-template")
        .arg("download:YT_PROGRESS|%(progress.downloaded_bytes)s|%(progress.total_bytes)s|%(progress.total_bytes_estimate)s|%(progress.speed)s|%(progress.eta)s")
        .arg("--retries")
        .arg("10")
        .arg("--fragment-retries")
        .arg("10")
        .arg("--retry-sleep")
        .arg("exp=1:20")
        .arg("--socket-timeout")
        .arg("30")
        .arg("--sleep-requests")
        .arg("1")
        .arg("-f")
        .arg("bestaudio/best")
        .arg("--extract-audio")
        .arg("--audio-format")
        .arg("best")
        .arg("--embed-metadata")
        .arg("--no-embed-info-json")
        .arg("--split-chapters")
        .arg("--write-thumbnail")
        .arg("--convert-thumbnails")
        .arg("jpg")
        .arg("--write-info-json")
        .arg("--no-write-comments")
        .arg("--windows-filenames")
        .arg("-P")
        .arg(stage)
        .arg("-o")
        .arg("__source__.%(ext)s")
        .arg("-o")
        .arg("chapter:%(section_number)03d - %(section_title).180B.%(ext)s")
        .arg("-o")
        .arg("thumbnail:cover.%(ext)s")
        .arg("-o")
        .arg("infojson:metadata.%(ext)s")
        .arg("--")
        .arg(url)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    configure_process_group(&mut command);

    let mut child = command
        .spawn()
        .context("could not start yt-dlp; install yt-dlp, FFmpeg/FFprobe, yt-dlp-ejs and Deno")?;
    let stdout = child
        .stdout
        .take()
        .context("yt-dlp stdout is unavailable")?;
    let stderr = child
        .stderr
        .take()
        .context("yt-dlp stderr is unavailable")?;
    let stdout_task = tokio::spawn(read_ytdlp_output(stdout, pool.clone(), item_id.to_string()));
    let stderr_task = tokio::spawn(read_ytdlp_output(stderr, pool.clone(), item_id.to_string()));

    let exit = tokio::select! {
        exit = child.wait() => exit?,
        _ = cancel.cancelled() => {
            terminate_process_tree(&mut child).await;
            let _ = stdout_task.await;
            let _ = stderr_task.await;
            bail!("YouTube import cancelled");
        }
    };
    let stdout_lines = stdout_task.await??;
    let stderr_lines = stderr_task.await??;
    if !exit.success() {
        let details = stdout_lines
            .into_iter()
            .chain(stderr_lines)
            .collect::<Vec<_>>()
            .join("\n");
        return Err(YtDlpDownloadFailure {
            message: format!("yt-dlp failed: {}", useful_error(&details)),
            http_forbidden: output_reports_http_403(&details),
        }
        .into());
    }
    Ok(())
}

fn is_http_403_download_failure(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<YtDlpDownloadFailure>()
        .is_some_and(|failure| failure.http_forbidden)
}

fn output_reports_http_403(output: &str) -> bool {
    let normalized = output.to_ascii_lowercase();
    [
        "http error 403",
        "http status 403",
        "status code 403",
        "403: forbidden",
        "403 forbidden",
    ]
    .iter()
    .any(|marker| normalized.contains(marker))
}

#[cfg(unix)]
fn configure_process_group(command: &mut Command) {
    use std::os::unix::process::CommandExt as _;
    command.as_std_mut().process_group(0);
}

#[cfg(not(unix))]
fn configure_process_group(_command: &mut Command) {}

async fn terminate_process_tree(child: &mut tokio::process::Child) {
    #[cfg(unix)]
    if let Some(process_group) = child.id().and_then(|id| i32::try_from(id).ok()) {
        // yt-dlp launches FFmpeg as a child. Both processes are placed in their
        // own group so stopping an import does not leave FFmpeg running.
        // SAFETY: `kill` receives a checked positive process-group ID negated
        // according to POSIX; it does not dereference application memory.
        unsafe {
            libc::kill(-process_group, libc::SIGTERM);
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
        // SAFETY: same process-group ID and POSIX contract as above.
        unsafe {
            libc::kill(-process_group, libc::SIGKILL);
        }
    }
    let _ = child.kill().await;
    let _ = child.wait().await;
}

async fn read_ytdlp_output<R: AsyncRead + Unpin>(
    reader: R,
    pool: PgPool,
    item_id: String,
) -> anyhow::Result<Vec<String>> {
    let mut lines = BufReader::new(reader).lines();
    let mut tail = VecDeque::with_capacity(30);
    while let Some(line) = lines.next_line().await? {
        if let Some(progress) = parse_progress_line(&line) {
            let _ = persist_item_progress(&pool, &item_id, progress).await;
        } else if is_postprocessing_line(&line) {
            let _ = set_item_status(&pool, &item_id, "postprocessing", None).await;
        }
        if !line.trim().is_empty() {
            if tail.len() == 30 {
                tail.pop_front();
            }
            tail.push_back(line);
        }
    }
    Ok(tail.into_iter().collect())
}

#[derive(Debug, PartialEq)]
struct DownloadProgress {
    downloaded: i64,
    total: Option<i64>,
    speed: Option<i64>,
    eta: Option<i64>,
    percent: f64,
}

fn parse_progress_line(line: &str) -> Option<DownloadProgress> {
    let payload = line.trim().strip_prefix("YT_PROGRESS|")?;
    let mut fields = payload.split('|');
    let downloaded = parse_number(fields.next()?)?;
    let exact_total = parse_number(fields.next().unwrap_or(""));
    let estimated_total = parse_number(fields.next().unwrap_or(""));
    let total = exact_total.or(estimated_total).filter(|v| *v > 0);
    let speed = parse_number(fields.next().unwrap_or(""));
    let eta = parse_number(fields.next().unwrap_or(""));
    let percent = total
        .map(|total| downloaded as f64 / total as f64 * 100.0)
        .unwrap_or(0.0)
        .clamp(0.0, 100.0);
    Some(DownloadProgress {
        downloaded,
        total,
        speed,
        eta,
        percent,
    })
}

fn parse_number(value: &str) -> Option<i64> {
    let value = value.trim();
    if value.is_empty() || value.eq_ignore_ascii_case("NA") || value.eq_ignore_ascii_case("none") {
        return None;
    }
    value
        .parse::<f64>()
        .ok()
        .filter(|v| v.is_finite() && *v >= 0.0)
        .map(|v| v.round() as i64)
}

fn is_postprocessing_line(line: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    [
        "[extractaudio]",
        "[splitchapters]",
        "[thumbnailconvertor]",
        "[metadata]",
        "splitting video by chapters",
    ]
    .iter()
    .any(|marker| lower.contains(marker))
}

async fn persist_item_progress(
    pool: &PgPool,
    item_id: &str,
    progress: DownloadProgress,
) -> anyhow::Result<()> {
    sqlx::query(
        r#"UPDATE furumusic__youtube_download_item
           SET progress_percent = $2, downloaded_bytes = $3, total_bytes = $4,
               speed_bytes_per_sec = $5, eta_seconds = $6, updated_at = $7
           WHERE id = $1"#,
    )
    .bind(item_id)
    .bind(progress.percent)
    .bind(progress.downloaded)
    .bind(progress.total)
    .bind(progress.speed)
    .bind(progress.eta)
    .bind(now_string())
    .execute(pool)
    .await?;
    Ok(())
}

async fn prepare_downloaded_folder(
    pool: &PgPool,
    inbox_root: &Path,
    user_id: i64,
    item: &YouTubeItemRow,
    stage: &Path,
    cancel: &CancellationToken,
) -> anyhow::Result<PreparedFolder> {
    if cancel.is_cancelled() {
        bail!("YouTube import cancelled");
    }
    let metadata = read_info_json(stage).await?;
    let chapter_count = metadata
        .as_ref()
        .and_then(|value| value.get("chapters"))
        .and_then(|value| value.as_array())
        .map(|chapters| i32::try_from(chapters.len()).unwrap_or(i32::MAX))
        .unwrap_or(0);

    let mut audio_files = find_audio_files(stage).await?;
    if chapter_count > 0 {
        for source in audio_files
            .iter()
            .filter(|path| file_name(path).starts_with("__source__."))
        {
            tokio::fs::remove_file(source).await?;
        }
    } else if let Some(source) = audio_files
        .iter()
        .find(|path| file_name(path).starts_with("__source__."))
        .cloned()
    {
        let extension = source
            .extension()
            .and_then(|v| v.to_str())
            .unwrap_or("opus");
        let destination = stage.join(format!(
            "{:03} - {} [{}].{}",
            item.playlist_index.max(1),
            sanitize_component(&item.title),
            item.source_id,
            extension
        ));
        if source != destination {
            tokio::fs::rename(&source, &destination).await?;
        }
    }

    normalize_cover(stage).await;
    cleanup_sidecars(stage).await?;
    audio_files = find_audio_files(stage).await?;
    if audio_files.is_empty() {
        bail!("yt-dlp produced no supported audio files");
    }

    preserve_track_numbers(&mut audio_files, chapter_count, item.playlist_index, cancel).await?;

    for audio in &audio_files {
        let data = tokio::select! {
            data = tokio::fs::read(audio) => data?,
            _ = cancel.cancelled() => bail!("YouTube import cancelled"),
        };
        let hash = format!("{:x}", Sha256::digest(&data));
        if crate::agent::rag::file_hash_exists(pool, &hash)
            .await
            .unwrap_or(false)
        {
            tokio::fs::remove_file(audio).await?;
        }
    }
    audio_files = find_audio_files(stage).await?;
    if audio_files.is_empty() {
        tokio::fs::remove_dir_all(stage).await?;
        return Ok(PreparedFolder {
            inbox_path: None,
            chapter_count,
            audio_file_count: 0,
            all_files_known: true,
        });
    }

    if cancel.is_cancelled() {
        bail!("YouTube import cancelled");
    }
    let folder_name = format!("{} [{}]", sanitize_component(&item.title), item.source_id);
    let destination = inbox_root
        .join("user_uploads")
        .join(user_id.to_string())
        .join(folder_name);
    if let Some(parent) = destination.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    if tokio::fs::try_exists(&destination).await? {
        let existing = find_audio_files(&destination).await?;
        if existing.is_empty() {
            bail!("YouTube inbox destination already exists without audio");
        }
        tokio::fs::remove_dir_all(stage).await?;
        audio_files = existing;
    } else {
        tokio::fs::rename(stage, &destination).await?;
    }

    let inbox_root_text = inbox_root.to_string_lossy();
    let inbox_path = crate::media_paths::path_for_root(&inbox_root_text, &destination)
        .context("YouTube destination escaped agent_inbox_dir")?;
    Ok(PreparedFolder {
        inbox_path: Some(inbox_path),
        chapter_count,
        audio_file_count: i32::try_from(audio_files.len()).unwrap_or(i32::MAX),
        all_files_known: false,
    })
}

async fn preserve_track_numbers(
    audio_files: &mut [PathBuf],
    chapter_count: i32,
    playlist_index: i32,
    cancel: &CancellationToken,
) -> anyhow::Result<()> {
    for (position, audio) in audio_files.iter_mut().enumerate() {
        if cancel.is_cancelled() {
            bail!("YouTube import cancelled");
        }
        let track_number = if chapter_count > 0 {
            track_number_from_file_name(audio)
                .unwrap_or_else(|| i32::try_from(position + 1).unwrap_or(i32::MAX))
        } else {
            playlist_index.max(1)
        };

        if track_number_from_file_name(audio) != Some(track_number) {
            let numbered =
                audio.with_file_name(format!("{track_number:03} - {}", file_name(audio)));
            tokio::fs::rename(&*audio, &numbered).await?;
            *audio = numbered;
        }
        embed_track_number(audio, track_number, cancel).await?;
    }
    Ok(())
}

fn track_number_from_file_name(path: &Path) -> Option<i32> {
    let stem = path.file_stem()?.to_str()?.trim_start();
    let digit_count = stem.bytes().take_while(u8::is_ascii_digit).count();
    if digit_count == 0 {
        return None;
    }
    let separator = stem[digit_count..].trim_start();
    if !separator.starts_with('-') && !separator.starts_with('.') {
        return None;
    }
    stem[..digit_count]
        .parse::<i32>()
        .ok()
        .filter(|number| *number > 0)
}

async fn embed_track_number(
    audio: &Path,
    track_number: i32,
    cancel: &CancellationToken,
) -> anyhow::Result<()> {
    let extension = audio
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or("audio");
    let temporary =
        audio.with_file_name(format!(".furumusic-track-{}.{}", Uuid::new_v4(), extension));
    let mut command = Command::new("ffmpeg");
    command
        .arg("-hide_banner")
        .arg("-loglevel")
        .arg("error")
        .arg("-y")
        .arg("-i")
        .arg(audio)
        .arg("-map")
        .arg("0")
        .arg("-c")
        .arg("copy")
        .arg("-metadata")
        .arg(format!("track={track_number}"))
        .arg(&temporary)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    configure_process_group(&mut command);

    let output = tokio::select! {
        output = command.output() => output?,
        _ = cancel.cancelled() => {
            let _ = tokio::fs::remove_file(&temporary).await;
            bail!("YouTube import cancelled");
        }
    };
    if !output.status.success() {
        let _ = tokio::fs::remove_file(&temporary).await;
        bail!(
            "ffmpeg could not preserve track number {}: {}",
            track_number,
            useful_error(&String::from_utf8_lossy(&output.stderr))
        );
    }

    let backup = audio.with_file_name(format!(
        ".furumusic-track-backup-{}.{}",
        Uuid::new_v4(),
        extension
    ));
    tokio::fs::rename(audio, &backup).await?;
    if let Err(error) = tokio::fs::rename(&temporary, audio).await {
        let _ = tokio::fs::rename(&backup, audio).await;
        let _ = tokio::fs::remove_file(&temporary).await;
        return Err(error.into());
    }
    tokio::fs::remove_file(backup).await?;
    Ok(())
}

async fn read_info_json(stage: &Path) -> anyhow::Result<Option<serde_json::Value>> {
    let mut entries = tokio::fs::read_dir(stage).await?;
    while let Some(entry) = entries.next_entry().await? {
        if !entry.file_type().await?.is_file() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        if name.ends_with(".info.json") || name == "metadata.json" {
            let data = tokio::fs::read(entry.path()).await?;
            return Ok(Some(
                serde_json::from_slice(&data).context("invalid yt-dlp info JSON")?,
            ));
        }
    }
    Ok(None)
}

async fn find_audio_files(dir: &Path) -> anyhow::Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    let mut entries = tokio::fs::read_dir(dir).await?;
    while let Some(entry) = entries.next_entry().await? {
        if entry.file_type().await?.is_file() && is_audio_path(&entry.path()) {
            files.push(entry.path());
        }
    }
    files.sort();
    Ok(files)
}

async fn normalize_cover(stage: &Path) {
    let cover_path = stage.join("cover.jpg");
    if tokio::fs::try_exists(&cover_path).await.unwrap_or(false) {
        return;
    }
    let Ok(mut entries) = tokio::fs::read_dir(stage).await else {
        return;
    };
    let mut candidate = None;
    while let Ok(Some(entry)) = entries.next_entry().await {
        let path = entry.path();
        let is_image = path
            .extension()
            .and_then(|v| v.to_str())
            .map(|ext| IMAGE_EXTENSIONS.contains(&ext.to_ascii_lowercase().as_str()))
            .unwrap_or(false);
        if is_image {
            candidate = Some(path);
            break;
        }
    }
    let Some(candidate) = candidate else {
        return;
    };
    let Ok(data) = tokio::fs::read(&candidate).await else {
        return;
    };
    let encoded = tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<u8>> {
        let image = image::load_from_memory(&data)?.to_rgb8();
        let mut output = Vec::new();
        JpegEncoder::new_with_quality(&mut output, 90).encode(
            &image,
            image.width(),
            image.height(),
            image::ExtendedColorType::Rgb8,
        )?;
        Ok(output)
    })
    .await;
    if let Ok(Ok(encoded)) = encoded
        && tokio::fs::write(&cover_path, encoded).await.is_ok()
        && candidate != cover_path
    {
        let _ = tokio::fs::remove_file(candidate).await;
    }
}

async fn cleanup_sidecars(stage: &Path) -> anyhow::Result<()> {
    let mut entries = tokio::fs::read_dir(stage).await?;
    while let Some(entry) = entries.next_entry().await? {
        if !entry.file_type().await?.is_file() {
            continue;
        }
        let path = entry.path();
        if is_audio_path(&path) || file_name(&path).eq_ignore_ascii_case("cover.jpg") {
            continue;
        }
        tokio::fs::remove_file(path).await?;
    }
    Ok(())
}

fn is_audio_path(path: &Path) -> bool {
    path.extension()
        .and_then(|value| value.to_str())
        .map(|value| AUDIO_EXTENSIONS.contains(&value.to_ascii_lowercase().as_str()))
        .unwrap_or(false)
}

async fn sync_ai_statuses(pool: &PgPool, user_id: i64) -> anyhow::Result<()> {
    let items: Vec<YouTubeItemRow> = sqlx::query_as(
        r#"SELECT i.id, i.job_id, i.source_id, i.source_url, i.title,
                  i.playlist_index, i.status, i.progress_percent,
                  i.downloaded_bytes, i.total_bytes, i.speed_bytes_per_sec,
                  i.eta_seconds, i.chapter_count, i.audio_file_count,
                  i.inbox_path, i.error, i.created_at, i.updated_at, i.completed_at
           FROM furumusic__youtube_download_item i
           JOIN furumusic__youtube_download j ON j.id = i.job_id
           WHERE j.user_id = $1
             AND i.status IN ('awaiting_ai', 'ai_processing', 'needs_review', 'ai_failed')
             AND i.inbox_path IS NOT NULL"#,
    )
    .bind(user_id)
    .fetch_all(pool)
    .await?;

    let mut touched_jobs = HashSet::new();
    for item in items {
        let Some(prefix) = item.inbox_path.as_deref() else {
            continue;
        };
        let states: Vec<(String, i64)> = sqlx::query_as(
            r#"SELECT status::text, COUNT(*)
               FROM furumusic__pending_review
               WHERE input_path = $1
                  OR left(input_path, length($1) + 1) = $1 || '/'
               GROUP BY status"#,
        )
        .bind(prefix)
        .fetch_all(pool)
        .await?;
        let counts: HashMap<String, i64> = states.into_iter().collect();
        let total: i64 = counts.values().sum();
        let expected = i64::from(item.audio_file_count.max(0));
        let active_processing = counts.get("processing").copied().unwrap_or(0) > 0;
        let queued = counts.get("queued").copied().unwrap_or(0) > 0;

        let (next, error) = if total < expected || total == 0 {
            (
                if active_processing {
                    "ai_processing"
                } else {
                    "awaiting_ai"
                },
                None,
            )
        } else if active_processing {
            ("ai_processing", None)
        } else if queued {
            ("awaiting_ai", None)
        } else if counts.get("failed").copied().unwrap_or(0) > 0 {
            let error: Option<String> = sqlx::query_scalar(
                r#"SELECT error_message FROM furumusic__pending_review
                   WHERE (input_path = $1 OR left(input_path, length($1) + 1) = $1 || '/')
                     AND status = 'failed' AND error_message IS NOT NULL
                   ORDER BY id DESC LIMIT 1"#,
            )
            .bind(prefix)
            .fetch_optional(pool)
            .await?
            .flatten();
            ("ai_failed", error)
        } else if counts.get("pending").copied().unwrap_or(0) > 0
            || counts.get("rejected").copied().unwrap_or(0) > 0
        {
            ("needs_review", None)
        } else {
            ("complete", None)
        };

        if item.status != next || item.error != error {
            let now = now_string();
            let completed_at =
                matches!(next, "complete" | "needs_review" | "ai_failed").then(|| now.clone());
            sqlx::query(
                r#"UPDATE furumusic__youtube_download_item
                   SET status = $2, error = $3, completed_at = $4, updated_at = $5
                   WHERE id = $1"#,
            )
            .bind(&item.id)
            .bind(next)
            .bind(error)
            .bind(completed_at)
            .bind(&now)
            .execute(pool)
            .await?;
            touched_jobs.insert(item.job_id);
        }
    }
    for job_id in touched_jobs {
        refresh_parent(pool, &job_id).await?;
    }
    Ok(())
}

async fn refresh_parent(pool: &PgPool, job_id: &str) -> anyhow::Result<()> {
    let parent_status: Option<String> =
        sqlx::query_scalar("SELECT status::text FROM furumusic__youtube_download WHERE id = $1")
            .bind(job_id)
            .fetch_optional(pool)
            .await?;
    let states: Vec<(String, i64)> = sqlx::query_as(
        r#"SELECT status::text, COUNT(*)
           FROM furumusic__youtube_download_item WHERE job_id = $1 GROUP BY status"#,
    )
    .bind(job_id)
    .fetch_all(pool)
    .await?;
    if states.is_empty() {
        return Ok(());
    }
    let counts: HashMap<String, i64> = states.into_iter().collect();
    let count = |status: &str| counts.get(status).copied().unwrap_or(0);
    let total: i64 = counts.values().sum();
    let failed = count("failed") + count("ai_failed");
    let review = count("needs_review");
    let completed = count("complete") + count("skipped") + review;

    let status = if parent_status.as_deref() == Some("cancelled") {
        "cancelled"
    } else if count("downloading") > 0 || count("queued") > 0 {
        "downloading"
    } else if count("postprocessing") > 0 {
        "postprocessing"
    } else if count("ai_processing") > 0 {
        "ai_processing"
    } else if count("awaiting_ai") > 0 {
        "awaiting_ai"
    } else if failed == total {
        "failed"
    } else if failed > 0 {
        "complete_with_errors"
    } else if review > 0 {
        "needs_review"
    } else {
        "complete"
    };
    let now = now_string();
    let completed_at = matches!(
        status,
        "complete" | "complete_with_errors" | "failed" | "needs_review" | "cancelled"
    )
    .then(|| now.clone());
    sqlx::query(
        r#"UPDATE furumusic__youtube_download
           SET status = $2, total_items = $3, completed_items = $4,
               failed_items = $5, review_items = $6, completed_at = $7,
               updated_at = $8 WHERE id = $1"#,
    )
    .bind(job_id)
    .bind(status)
    .bind(i32::try_from(total).unwrap_or(i32::MAX))
    .bind(i32::try_from(completed).unwrap_or(i32::MAX))
    .bind(i32::try_from(failed).unwrap_or(i32::MAX))
    .bind(i32::try_from(review).unwrap_or(i32::MAX))
    .bind(completed_at)
    .bind(now)
    .execute(pool)
    .await?;
    Ok(())
}

async fn source_already_imported(
    pool: &PgPool,
    user_id: i64,
    current_job_id: &str,
    source_id: &str,
) -> anyhow::Result<bool> {
    let exists: bool = sqlx::query_scalar(
        r#"SELECT EXISTS (
               SELECT 1
               FROM furumusic__youtube_download_item i
               JOIN furumusic__youtube_download j ON j.id = i.job_id
               WHERE j.user_id = $1 AND i.job_id <> $2 AND i.source_id = $3
                 AND i.status IN (
                     'queued', 'downloading', 'postprocessing', 'awaiting_ai',
                     'ai_processing', 'complete', 'needs_review', 'skipped'
                 )
           )"#,
    )
    .bind(user_id)
    .bind(current_job_id)
    .bind(source_id)
    .fetch_one(pool)
    .await?;
    Ok(exists)
}

async fn already_imported_source_ids(
    pool: &PgPool,
    user_id: i64,
    source_ids: &[String],
) -> anyhow::Result<HashSet<String>> {
    let ids: Vec<String> = sqlx::query_scalar(
        r#"SELECT DISTINCT i.source_id
           FROM furumusic__youtube_download_item i
           JOIN furumusic__youtube_download j ON j.id = i.job_id
           WHERE j.user_id = $1 AND i.source_id = ANY($2)
             AND i.status IN (
                 'queued', 'downloading', 'postprocessing', 'awaiting_ai',
                 'ai_processing', 'complete', 'needs_review', 'skipped'
             )"#,
    )
    .bind(user_id)
    .bind(source_ids)
    .fetch_all(pool)
    .await?;
    Ok(ids.into_iter().collect())
}

async fn load_job_dto(pool: &PgPool, user_id: i64, id: &str) -> anyhow::Result<YouTubeJobDto> {
    let row = load_job_row(pool, user_id, id).await?;
    let items = load_items(pool, id)
        .await?
        .iter()
        .map(YouTubeItemRow::dto)
        .collect();
    Ok(row.dto(items))
}

async fn load_job_row(pool: &PgPool, user_id: i64, id: &str) -> anyhow::Result<YouTubeJobRow> {
    sqlx::query_as(
        r#"SELECT id, user_id, source_url, title, source_kind, status,
                  total_items, completed_items, failed_items, review_items,
                  error, created_at, updated_at, completed_at
           FROM furumusic__youtube_download WHERE id = $1 AND user_id = $2"#,
    )
    .bind(id)
    .bind(user_id)
    .fetch_optional(pool)
    .await?
    .context("YouTube download not found")
}

async fn load_items(pool: &PgPool, job_id: &str) -> anyhow::Result<Vec<YouTubeItemRow>> {
    Ok(sqlx::query_as(
        r#"SELECT id, job_id, source_id, source_url, title, playlist_index,
                  status, progress_percent, downloaded_bytes, total_bytes,
                  speed_bytes_per_sec, eta_seconds, chapter_count,
                  audio_file_count, inbox_path, error, created_at, updated_at,
                  completed_at
           FROM furumusic__youtube_download_item WHERE job_id = $1
           ORDER BY playlist_index, id"#,
    )
    .bind(job_id)
    .fetch_all(pool)
    .await?)
}

async fn set_parent_status(
    pool: &PgPool,
    id: &str,
    status: &str,
    error: Option<&str>,
) -> anyhow::Result<()> {
    sqlx::query(
        r#"UPDATE furumusic__youtube_download
           SET status = $2, error = $3, updated_at = $4
           WHERE id = $1 AND status <> 'cancelled'"#,
    )
    .bind(id)
    .bind(status)
    .bind(error.map(trim_error))
    .bind(now_string())
    .execute(pool)
    .await?;
    Ok(())
}

async fn set_item_status(
    pool: &PgPool,
    id: &str,
    status: &str,
    error: Option<&str>,
) -> anyhow::Result<()> {
    sqlx::query(
        r#"UPDATE furumusic__youtube_download_item
           SET status = $2, error = $3, updated_at = $4
           WHERE id = $1 AND status <> 'cancelled'"#,
    )
    .bind(id)
    .bind(status)
    .bind(error.map(trim_error))
    .bind(now_string())
    .execute(pool)
    .await?;
    Ok(())
}

async fn fail_item(pool: &PgPool, id: &str, error: &str) -> anyhow::Result<()> {
    let now = now_string();
    sqlx::query(
        r#"UPDATE furumusic__youtube_download_item
           SET status = 'failed', error = $2, speed_bytes_per_sec = NULL,
               eta_seconds = NULL, completed_at = $3, updated_at = $3
           WHERE id = $1"#,
    )
    .bind(id)
    .bind(trim_error(error))
    .bind(now)
    .execute(pool)
    .await?;
    Ok(())
}

async fn fail_parent(pool: &PgPool, id: &str, error: &str) -> anyhow::Result<()> {
    let now = now_string();
    sqlx::query(
        r#"UPDATE furumusic__youtube_download
           SET status = 'failed', error = $2, completed_at = $3, updated_at = $3
           WHERE id = $1"#,
    )
    .bind(id)
    .bind(trim_error(error))
    .bind(now)
    .execute(pool)
    .await?;
    Ok(())
}

fn validate_youtube_url(value: &str) -> anyhow::Result<String> {
    let value = value.trim();
    if value.is_empty() {
        bail!("YouTube URL is empty");
    }
    let url = reqwest::Url::parse(value).context("invalid YouTube URL")?;
    if !matches!(url.scheme(), "http" | "https") {
        bail!("only HTTP and HTTPS YouTube URLs are supported");
    }
    if !url.username().is_empty() || url.password().is_some() {
        bail!("YouTube URL must not contain credentials");
    }
    let host = url
        .host_str()
        .map(|host| host.trim_end_matches('.').to_ascii_lowercase())
        .context("YouTube URL has no host")?;
    let allowed = host == "youtu.be" || host == "youtube.com" || host.ends_with(".youtube.com");
    if !allowed {
        bail!("only youtube.com, music.youtube.com and youtu.be URLs are supported");
    }
    Ok(url.to_string())
}

fn validate_inbox_dir(value: &str) -> anyhow::Result<PathBuf> {
    let value = value.trim();
    if value.is_empty() {
        bail!("agent_inbox_dir is not configured");
    }
    let path = crate::media_paths::resolve_config_path_buf(value);
    if !path.is_absolute() {
        bail!("agent_inbox_dir must be an absolute path");
    }
    Ok(path)
}

fn staging_job_root(inbox_root: &Path, job_id: &str) -> PathBuf {
    inbox_root.join(".downloads").join("youtube").join(job_id)
}

fn staging_item_root(inbox_root: &Path, job_id: &str, item_id: &str) -> PathBuf {
    staging_job_root(inbox_root, job_id).join(item_id)
}

fn sanitize_component(value: &str) -> String {
    let value: String = value
        .chars()
        .take(160)
        .map(|character| match character {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            character if character.is_control() => '_',
            character => character,
        })
        .collect();
    let value = value.trim().trim_matches('.').trim();
    if value.is_empty() {
        "YouTube audio".to_string()
    } else {
        value.to_string()
    }
}

fn valid_source_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

fn requested_video_id(value: &str) -> Option<String> {
    let url = reqwest::Url::parse(value).ok()?;
    let host = url.host_str()?.trim_end_matches('.').to_ascii_lowercase();
    let candidate = if host == "youtu.be" {
        url.path_segments()?
            .find(|segment| !segment.is_empty())
            .map(str::to_string)
    } else if url.path() == "/watch" {
        url.query_pairs()
            .find_map(|(key, value)| (key == "v").then_some(value.into_owned()))
    } else {
        let mut segments = url.path_segments()?;
        match (segments.next(), segments.next()) {
            (Some("embed" | "live" | "shorts"), Some(id)) => Some(id.to_string()),
            _ => None,
        }
    }?;
    valid_source_id(&candidate).then_some(candidate)
}

fn json_string(value: &serde_json::Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn json_positive_i32(value: &serde_json::Value, key: &str) -> Option<i32> {
    let value = value.get(key)?;
    let parsed = value
        .as_i64()
        .or_else(|| value.as_str()?.trim().parse::<i64>().ok())?;
    i32::try_from(parsed).ok().filter(|value| *value > 0)
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_string()
}

fn is_active_job_status(status: &str) -> bool {
    matches!(
        status,
        "queued" | "resolving" | "downloading" | "postprocessing" | "awaiting_ai" | "ai_processing"
    )
}

fn is_cancellable_job_status(status: &str) -> bool {
    matches!(
        status,
        "queued" | "resolving" | "downloading" | "postprocessing"
    )
}

fn useful_error(value: &str) -> String {
    let lines: Vec<&str> = value
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    trim_error(lines.last().copied().unwrap_or("unknown error"))
}

fn trim_error(value: &str) -> String {
    value.chars().take(MAX_ERROR_LEN).collect()
}

fn non_negative(value: i64) -> u64 {
    u64::try_from(value).unwrap_or(0)
}

fn now_string() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_supported_youtube_hosts() {
        assert!(validate_youtube_url("https://youtube.com/watch?v=abc").is_ok());
        assert!(validate_youtube_url("https://www.youtube.com/playlist?list=abc").is_ok());
        assert!(validate_youtube_url("https://music.youtube.com/watch?v=abc").is_ok());
        assert!(validate_youtube_url("https://youtu.be/abc").is_ok());
    }

    #[test]
    fn rejects_non_youtube_and_lookalike_hosts() {
        assert!(validate_youtube_url("https://example.com/video").is_err());
        assert!(validate_youtube_url("https://youtube.com.example.com/video").is_err());
        assert!(validate_youtube_url("file:///etc/passwd").is_err());
        assert!(validate_youtube_url("https://user@youtube.com/video").is_err());
    }

    #[test]
    fn parses_machine_readable_progress() {
        let progress = parse_progress_line("YT_PROGRESS|500|1000|NA|250|2").unwrap();
        assert_eq!(
            progress,
            DownloadProgress {
                downloaded: 500,
                total: Some(1000),
                speed: Some(250),
                eta: Some(2),
                percent: 50.0,
            }
        );
    }

    #[test]
    fn sanitizes_download_folder_names() {
        assert_eq!(
            sanitize_component("  Album: Live?/Test  "),
            "Album_ Live__Test"
        );
        assert_eq!(sanitize_component("..."), "YouTube audio");
    }

    #[test]
    fn recognizes_http_403_download_failures() {
        assert!(output_reports_http_403(
            "ERROR: unable to download video data: HTTP Error 403: Forbidden"
        ));
        assert!(output_reports_http_403(
            "server returned status code 403 while downloading a fragment"
        ));
        assert!(!output_reports_http_403(
            "ERROR: unable to download video data: HTTP Error 404: Not Found"
        ));

        let error = anyhow::Error::new(YtDlpDownloadFailure {
            message: "yt-dlp failed".to_string(),
            http_forbidden: true,
        });
        assert!(is_http_403_download_failure(&error));
    }

    #[test]
    fn preserves_playlist_and_chapter_numbers_from_source_metadata() {
        let entry = serde_json::json!({"playlist_index": 7});
        let string_entry = serde_json::json!({"playlist_index": "12"});
        assert_eq!(json_positive_i32(&entry, "playlist_index"), Some(7));
        assert_eq!(json_positive_i32(&string_entry, "playlist_index"), Some(12));
        assert_eq!(
            track_number_from_file_name(Path::new("007 - Playlist track.opus")),
            Some(7)
        );
        assert_eq!(
            track_number_from_file_name(Path::new("003 - Chapter title.m4a")),
            Some(3)
        );
        assert_eq!(
            track_number_from_file_name(Path::new("1984 remix.webm")),
            None
        );
    }

    #[test]
    fn ytdlp_command_receives_selected_proxy() {
        let command = base_ytdlp_command(Some("socks5://user:pass@127.0.0.1:1080/"));
        let args: Vec<String> = command
            .as_std()
            .get_args()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect();
        assert!(args.windows(2).any(|pair| {
            pair == [
                "--proxy".to_string(),
                "socks5://user:pass@127.0.0.1:1080/".to_string(),
            ]
        }));

        let direct = base_ytdlp_command(None);
        assert!(
            direct
                .as_std()
                .get_args()
                .all(|argument| argument != "--proxy")
        );
    }

    #[test]
    fn extracts_explicit_video_from_playlist_links() {
        assert_eq!(
            requested_video_id("https://www.youtube.com/watch?v=qZ4PNyZGSJ8&list=RDqZ4PNyZGSJ8")
                .as_deref(),
            Some("qZ4PNyZGSJ8")
        );
        assert_eq!(
            requested_video_id("https://youtu.be/qZ4PNyZGSJ8?list=RDqZ4PNyZGSJ8").as_deref(),
            Some("qZ4PNyZGSJ8")
        );
        assert_eq!(
            requested_video_id("https://youtube.com/playlist?list=PL123"),
            None
        );
    }
}
