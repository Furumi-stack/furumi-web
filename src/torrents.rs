use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, bail};
use base64::Engine;
use librqbit::{
    AddTorrent, AddTorrentOptions, AddTorrentResponse, ManagedTorrent, Session, SessionOptions,
};
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, PgPool};
use tokio::sync::{Mutex, OnceCell};
use uuid::Uuid;

use crate::scheduler::SchedulerHandle;

const METADATA_TIMEOUT: Duration = Duration::from_secs(90);
const TORRENT_LIST_LIMIT: i64 = 100;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TorrentFileDto {
    pub index: usize,
    pub name: String,
    pub components: Vec<String>,
    pub length: u64,
    pub selected: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct TorrentPreviewDto {
    pub id: String,
    pub name: String,
    pub info_hash: String,
    pub total_size: u64,
    pub files: Vec<TorrentFileDto>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TorrentJobDto {
    pub id: String,
    pub name: String,
    pub info_hash: String,
    pub status: String,
    pub client_state: Option<String>,
    pub total_size: u64,
    pub selected_size: u64,
    pub downloaded_bytes: u64,
    pub uploaded_bytes: u64,
    pub progress_percent: f64,
    pub download_speed_mbps: Option<f64>,
    pub upload_speed_mbps: Option<f64>,
    pub peers_live: Option<usize>,
    pub peers_seen: Option<usize>,
    pub eta: Option<String>,
    pub active: bool,
    pub error: Option<String>,
    pub created_at: Option<String>,
    pub updated_at: Option<String>,
    pub completed_at: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TorrentSessionDto {
    pub job: TorrentJobDto,
    pub preview: TorrentPreviewDto,
    pub selected_files: Vec<usize>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TorrentPreviewKind {
    Magnet,
    TorrentFile,
}

impl TorrentPreviewKind {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Magnet => "magnet",
            Self::TorrentFile => "torrent_file",
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct TorrentPreviewRequest {
    pub kind: TorrentPreviewKind,
    pub magnet: Option<String>,
    pub torrent_base64: Option<String>,
    pub source_label: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct TorrentStartRequest {
    pub selected_files: Vec<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TorrentJobStatus {
    Resolving,
    Preview,
    Downloading,
    Moving,
    Complete,
    Failed,
    Paused,
}

impl TorrentJobStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Preview => "preview",
            Self::Resolving => "resolving",
            Self::Downloading => "downloading",
            Self::Moving => "moving",
            Self::Complete => "complete",
            Self::Failed => "failed",
            Self::Paused => "paused",
        }
    }

    fn from_str(value: &str) -> Self {
        match value {
            "downloading" => Self::Downloading,
            "resolving" => Self::Resolving,
            "moving" => Self::Moving,
            "complete" => Self::Complete,
            "failed" => Self::Failed,
            "paused" => Self::Paused,
            _ => Self::Preview,
        }
    }
}

struct TorrentJob {
    id: String,
    user_id: i64,
    name: String,
    info_hash: String,
    source_kind: String,
    source_label: Option<String>,
    torrent_bytes: Vec<u8>,
    files: Vec<TorrentFileDto>,
    status: TorrentJobStatus,
    output_dir: PathBuf,
    selected_files: Vec<usize>,
    handle: Option<Arc<ManagedTorrent>>,
    downloaded_bytes: u64,
    uploaded_bytes: u64,
    progress_percent: f64,
    error: Option<String>,
    created_at: String,
    updated_at: String,
    completed_at: Option<String>,
}

#[derive(Debug, FromRow)]
struct TorrentSessionRow {
    id: String,
    user_id: i64,
    name: String,
    info_hash: String,
    source_kind: String,
    source_label: Option<String>,
    torrent_bytes: Vec<u8>,
    files_json: String,
    selected_files_json: String,
    status: String,
    total_size: i64,
    selected_size: i64,
    downloaded_bytes: i64,
    uploaded_bytes: i64,
    progress_percent: f64,
    error: Option<String>,
    created_at: String,
    updated_at: String,
    completed_at: Option<String>,
}

impl TorrentSessionRow {
    fn files(&self) -> anyhow::Result<Vec<TorrentFileDto>> {
        serde_json::from_str(&self.files_json).context("invalid torrent file list")
    }

    fn selected_files(&self) -> Vec<usize> {
        serde_json::from_str(&self.selected_files_json).unwrap_or_default()
    }

    fn dto(&self, handle: Option<&Arc<ManagedTorrent>>) -> TorrentJobDto {
        let active = handle.is_some();
        let status = if active {
            self.status.as_str()
        } else if self.status == "downloading" || self.status == "moving" {
            "paused"
        } else {
            self.status.as_str()
        };
        let stats = handle.map(|h| h.stats());
        let mut downloaded_bytes = stats
            .as_ref()
            .map(|s| s.progress_bytes)
            .unwrap_or_else(|| i64_to_u64(self.downloaded_bytes));
        let selected_size = i64_to_u64(self.selected_size);
        if status == "complete" {
            downloaded_bytes = selected_size;
        }
        let uploaded_bytes = stats
            .as_ref()
            .map(|s| s.uploaded_bytes)
            .unwrap_or_else(|| i64_to_u64(self.uploaded_bytes));
        let total_bytes = stats
            .as_ref()
            .map(|s| s.total_bytes)
            .filter(|v| *v > 0)
            .unwrap_or_else(|| i64_to_u64(self.selected_size));
        let progress_percent = progress_percent(downloaded_bytes, total_bytes)
            .unwrap_or(self.progress_percent)
            .clamp(0.0, 100.0);
        let progress_percent = if status == "complete" {
            100.0
        } else {
            progress_percent
        };
        let live = stats.as_ref().and_then(|s| s.live.as_ref());
        let peer_stats = live.map(|l| &l.snapshot.peer_stats);

        TorrentJobDto {
            id: self.id.clone(),
            name: self.name.clone(),
            info_hash: self.info_hash.clone(),
            status: status.to_string(),
            client_state: stats.as_ref().map(|s| s.state.to_string()),
            total_size: i64_to_u64(self.total_size),
            selected_size,
            downloaded_bytes,
            uploaded_bytes,
            progress_percent,
            download_speed_mbps: live.map(|l| l.download_speed.mbps),
            upload_speed_mbps: live.map(|l| l.upload_speed.mbps),
            peers_live: peer_stats.map(|p| p.live),
            peers_seen: peer_stats.map(|p| p.seen),
            eta: live.and_then(|l| l.time_remaining.as_ref().map(|eta| eta.to_string())),
            active,
            error: self.error.clone(),
            created_at: Some(self.created_at.clone()),
            updated_at: Some(self.updated_at.clone()),
            completed_at: self.completed_at.clone(),
        }
    }

    fn preview(&self) -> anyhow::Result<TorrentPreviewDto> {
        Ok(TorrentPreviewDto {
            id: self.id.clone(),
            name: self.name.clone(),
            info_hash: self.info_hash.clone(),
            total_size: i64_to_u64(self.total_size),
            files: self.files()?,
        })
    }

    fn into_job(self, temp_root: &Path) -> anyhow::Result<TorrentJob> {
        let id = self.id.clone();
        let files = self.files()?;
        let selected_files = self.selected_files();
        Ok(TorrentJob {
            id: id.clone(),
            user_id: self.user_id,
            name: self.name,
            info_hash: self.info_hash,
            source_kind: self.source_kind,
            source_label: self.source_label,
            torrent_bytes: self.torrent_bytes,
            files,
            status: TorrentJobStatus::from_str(&self.status),
            output_dir: temp_root.join(&id).join("download"),
            selected_files,
            handle: None,
            downloaded_bytes: i64_to_u64(self.downloaded_bytes),
            uploaded_bytes: i64_to_u64(self.uploaded_bytes),
            progress_percent: self.progress_percent,
            error: self.error,
            created_at: self.created_at,
            updated_at: self.updated_at,
            completed_at: self.completed_at,
        })
    }
}

impl TorrentJob {
    fn total_size(&self) -> u64 {
        self.files.iter().map(|f| f.length).sum()
    }

    fn selected_size(&self) -> u64 {
        selected_size(&self.files, &self.selected_files)
    }

    fn preview(&self) -> TorrentPreviewDto {
        TorrentPreviewDto {
            id: self.id.clone(),
            name: self.name.clone(),
            info_hash: self.info_hash.clone(),
            total_size: self.total_size(),
            files: self.files.clone(),
        }
    }

    fn refresh_progress(&mut self) {
        let Some(handle) = &self.handle else {
            return;
        };
        let stats = handle.stats();
        self.downloaded_bytes = stats.progress_bytes;
        self.uploaded_bytes = stats.uploaded_bytes;
        self.progress_percent = progress_percent(stats.progress_bytes, stats.total_bytes)
            .unwrap_or(self.progress_percent)
            .clamp(0.0, 100.0);
    }

    fn dto(&self) -> TorrentJobDto {
        let stats = self.handle.as_ref().map(|h| h.stats());
        let mut downloaded_bytes = stats
            .as_ref()
            .map(|s| s.progress_bytes)
            .unwrap_or(self.downloaded_bytes);
        let selected_size = self.selected_size();
        if self.status == TorrentJobStatus::Complete {
            downloaded_bytes = selected_size;
        }
        let uploaded_bytes = stats
            .as_ref()
            .map(|s| s.uploaded_bytes)
            .unwrap_or(self.uploaded_bytes);
        let total_bytes = stats
            .as_ref()
            .map(|s| s.total_bytes)
            .filter(|v| *v > 0)
            .unwrap_or_else(|| self.selected_size());
        let live = stats.as_ref().and_then(|s| s.live.as_ref());
        let peer_stats = live.map(|l| &l.snapshot.peer_stats);

        TorrentJobDto {
            id: self.id.clone(),
            name: self.name.clone(),
            info_hash: self.info_hash.clone(),
            status: self.status.as_str().to_string(),
            client_state: stats.as_ref().map(|s| s.state.to_string()),
            total_size: self.total_size(),
            selected_size,
            downloaded_bytes,
            uploaded_bytes,
            progress_percent: if self.status == TorrentJobStatus::Complete {
                100.0
            } else {
                progress_percent(downloaded_bytes, total_bytes)
                    .unwrap_or(self.progress_percent)
                    .clamp(0.0, 100.0)
            },
            download_speed_mbps: live.map(|l| l.download_speed.mbps),
            upload_speed_mbps: live.map(|l| l.upload_speed.mbps),
            peers_live: peer_stats.map(|p| p.live),
            peers_seen: peer_stats.map(|p| p.seen),
            eta: live.and_then(|l| l.time_remaining.as_ref().map(|eta| eta.to_string())),
            active: self.handle.is_some(),
            error: self.error.clone(),
            created_at: Some(self.created_at.clone()),
            updated_at: Some(self.updated_at.clone()),
            completed_at: self.completed_at.clone(),
        }
    }
}

pub struct TorrentService {
    temp_root: PathBuf,
    sessions: Mutex<HashMap<String, Arc<Session>>>,
    job_sessions: Mutex<HashMap<String, Arc<Session>>>,
    jobs: Mutex<HashMap<String, TorrentJob>>,
    resolving_jobs: Mutex<HashSet<String>>,
    scheduler_handle: Arc<OnceCell<Arc<SchedulerHandle>>>,
}

impl TorrentService {
    pub fn new(scheduler_handle: Arc<OnceCell<Arc<SchedulerHandle>>>) -> Self {
        Self {
            temp_root: std::env::temp_dir().join("furumusic").join("torrents"),
            sessions: Mutex::new(HashMap::new()),
            job_sessions: Mutex::new(HashMap::new()),
            jobs: Mutex::new(HashMap::new()),
            resolving_jobs: Mutex::new(HashSet::new()),
            scheduler_handle,
        }
    }

    async fn session(&self, proxy_url: Option<&str>) -> anyhow::Result<Arc<Session>> {
        let key = proxy_url.unwrap_or_default().to_string();
        let mut sessions = self.sessions.lock().await;
        if let Some(session) = sessions.get(&key) {
            return Ok(Arc::clone(session));
        }

        tokio::fs::create_dir_all(&self.temp_root).await?;
        let session = Session::new_with_opts(
            self.temp_root.clone(),
            SessionOptions {
                // SOCKS is intentionally limited to peer TCP and HTTP(S)
                // tracker traffic. DHT and other UDP discovery stay direct.
                disable_dht: false,
                // Sessions are keyed by proxy and can coexist, so they cannot
                // safely share one persisted DHT socket configuration.
                disable_dht_persistence: true,
                disable_upload: true,
                enable_upnp_port_forwarding: false,
                socks_proxy_url: proxy_url.map(str::to_owned),
                ..Default::default()
            },
        )
        .await?;
        sessions.insert(key, Arc::clone(&session));
        Ok(session)
    }

    pub async fn list(
        self: &Arc<Self>,
        pool: &PgPool,
        user_id: i64,
        proxy_url: Option<String>,
    ) -> anyhow::Result<Vec<TorrentJobDto>> {
        let rows = sqlx::query_as::<_, TorrentSessionRow>(
            r#"SELECT id, user_id, name, info_hash, source_kind, source_label, torrent_bytes,
                      files_json, selected_files_json, status, total_size, selected_size,
                      downloaded_bytes, uploaded_bytes, progress_percent, error,
                      created_at, updated_at, completed_at
                 FROM furumusic__torrent_session
                WHERE user_id = $1
                ORDER BY created_at DESC, id DESC
                LIMIT $2"#,
        )
        .bind(user_id)
        .bind(TORRENT_LIST_LIMIT)
        .fetch_all(pool)
        .await?;

        let handles = {
            let jobs = self.jobs.lock().await;
            jobs.iter()
                .filter_map(|(id, job)| job.handle.as_ref().map(|h| (id.clone(), Arc::clone(h))))
                .collect::<HashMap<_, _>>()
        };

        for row in rows.iter().filter(|row| row.status == "resolving") {
            if row.source_kind == "magnet" {
                if let Some(magnet) = row.source_label.clone() {
                    self.spawn_resolve_pending_magnet(
                        pool.clone(),
                        user_id,
                        row.id.clone(),
                        magnet,
                        row.created_at.clone(),
                        proxy_url.clone(),
                    )
                    .await;
                }
            }
        }

        Ok(rows
            .iter()
            .map(|row| row.dto(handles.get(&row.id)))
            .collect())
    }

    pub async fn details(
        &self,
        pool: &PgPool,
        user_id: i64,
        id: &str,
    ) -> anyhow::Result<TorrentSessionDto> {
        if let Some(session) = self.memory_details(user_id, id).await {
            return Ok(session);
        }

        let row = load_row(pool, user_id, id).await?;
        let selected_files = row.selected_files();
        let job = row.dto(None);
        let preview = row.preview()?;
        Ok(TorrentSessionDto {
            job,
            preview,
            selected_files,
        })
    }

    pub async fn preview(
        self: &Arc<Self>,
        pool: &PgPool,
        user_id: i64,
        request: TorrentPreviewRequest,
        proxy_url: Option<&str>,
    ) -> anyhow::Result<TorrentSessionDto> {
        let session = self.session(proxy_url).await?;
        let id = Uuid::new_v4().to_string();
        let output_dir = self.temp_root.join(&id).join("download");
        tokio::fs::create_dir_all(&output_dir).await?;

        let source_kind = request.kind.as_str().to_string();
        let source_label = request
            .source_label
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned);

        if matches!(request.kind, TorrentPreviewKind::Magnet) {
            let magnet = request
                .magnet
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .context("magnet link is empty")?
                .to_string();
            let info_hash = extract_magnet_info_hash(&magnet).context("invalid magnet link")?;
            let name = magnet_display_name(&magnet)
                .or(source_label)
                .unwrap_or_else(|| info_hash.clone());
            let now = now_string();
            insert_pending_magnet(pool, &id, user_id, &name, &info_hash, &magnet, &now).await?;
            self.spawn_resolve_pending_magnet(
                pool.clone(),
                user_id,
                id.clone(),
                magnet,
                now,
                proxy_url.map(str::to_owned),
            )
            .await;

            let row = load_row(pool, user_id, &id).await?;
            return Ok(TorrentSessionDto {
                job: row.dto(None),
                preview: row.preview()?,
                selected_files: row.selected_files(),
            });
        }

        let encoded = request
            .torrent_base64
            .as_deref()
            .filter(|s| !s.is_empty())
            .context("torrent file is empty")?;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .context("invalid torrent file encoding")?;

        let response = session
            .add_torrent(
                AddTorrent::from_bytes(bytes),
                Some(AddTorrentOptions {
                    list_only: true,
                    output_folder: Some(output_dir.to_string_lossy().to_string()),
                    ..Default::default()
                }),
            )
            .await?;

        let AddTorrentResponse::ListOnly(list) = response else {
            bail!("torrent was unexpectedly added instead of previewed");
        };

        let name = list
            .info
            .name
            .as_ref()
            .map(|b| String::from_utf8_lossy(b.as_ref()).to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| list.info_hash.as_string());

        let mut files = Vec::new();
        for (index, details) in list.info.iter_file_details()?.enumerate() {
            let name = details
                .filename
                .to_string()
                .unwrap_or_else(|_| "<invalid filename>".to_string());
            files.push(TorrentFileDto {
                index,
                name,
                components: details.filename.to_vec().unwrap_or_default(),
                length: details.len,
                selected: true,
            });
        }

        let selected_files = files.iter().map(|f| f.index).collect::<Vec<_>>();
        let now = now_string();
        let job = TorrentJob {
            id: id.clone(),
            user_id,
            name,
            info_hash: list.info_hash.as_string(),
            source_kind,
            source_label,
            torrent_bytes: list.torrent_bytes.to_vec(),
            files,
            status: TorrentJobStatus::Preview,
            output_dir,
            selected_files,
            handle: None,
            downloaded_bytes: 0,
            uploaded_bytes: 0,
            progress_percent: 0.0,
            error: None,
            created_at: now.clone(),
            updated_at: now,
            completed_at: None,
        };
        insert_job(pool, &job).await?;

        let dto = TorrentSessionDto {
            job: job.dto(),
            preview: job.preview(),
            selected_files: job.selected_files.clone(),
        };
        self.jobs.lock().await.insert(id, job);

        Ok(dto)
    }

    async fn spawn_resolve_pending_magnet(
        self: &Arc<Self>,
        pool: PgPool,
        user_id: i64,
        id: String,
        magnet: String,
        created_at: String,
        proxy_url: Option<String>,
    ) {
        {
            let mut resolving = self.resolving_jobs.lock().await;
            if !resolving.insert(id.clone()) {
                return;
            }
        }

        let service = Arc::clone(self);
        tokio::spawn(async move {
            let result = service
                .resolve_pending_magnet(
                    &pool,
                    user_id,
                    &id,
                    &magnet,
                    &created_at,
                    proxy_url.as_deref(),
                )
                .await;
            if let Err(err) = result {
                update_resolving_error(&pool, &id, &err.to_string()).await;
            }
            service.resolving_jobs.lock().await.remove(&id);
        });
    }

    async fn resolve_pending_magnet(
        &self,
        pool: &PgPool,
        user_id: i64,
        id: &str,
        magnet: &str,
        created_at: &str,
        proxy_url: Option<&str>,
    ) -> anyhow::Result<()> {
        let session = self.session(proxy_url).await?;
        let output_dir = self.temp_root.join(id).join("download");
        tokio::fs::create_dir_all(&output_dir).await?;
        let response = tokio::time::timeout(
            METADATA_TIMEOUT,
            session.add_torrent(
                AddTorrent::from_url(magnet.to_string()),
                Some(AddTorrentOptions {
                    list_only: true,
                    output_folder: Some(output_dir.to_string_lossy().to_string()),
                    ..Default::default()
                }),
            ),
        )
        .await
        .context("timed out while resolving torrent metadata")??;

        let AddTorrentResponse::ListOnly(list) = response else {
            bail!("torrent was unexpectedly added instead of previewed");
        };

        let name = list
            .info
            .name
            .as_ref()
            .map(|b| String::from_utf8_lossy(b.as_ref()).to_string())
            .filter(|s| !s.is_empty())
            .or_else(|| magnet_display_name(magnet))
            .unwrap_or_else(|| list.info_hash.as_string());

        let mut files = Vec::new();
        for (index, details) in list.info.iter_file_details()?.enumerate() {
            let name = details
                .filename
                .to_string()
                .unwrap_or_else(|_| "<invalid filename>".to_string());
            files.push(TorrentFileDto {
                index,
                name,
                components: details.filename.to_vec().unwrap_or_default(),
                length: details.len,
                selected: true,
            });
        }

        let selected_files = files.iter().map(|f| f.index).collect::<Vec<_>>();
        let job = TorrentJob {
            id: id.to_string(),
            user_id,
            name,
            info_hash: list.info_hash.as_string(),
            source_kind: "magnet".to_string(),
            source_label: Some(magnet.to_string()),
            torrent_bytes: list.torrent_bytes.to_vec(),
            files,
            status: TorrentJobStatus::Preview,
            output_dir,
            selected_files,
            handle: None,
            downloaded_bytes: 0,
            uploaded_bytes: 0,
            progress_percent: 0.0,
            error: None,
            created_at: created_at.to_string(),
            updated_at: now_string(),
            completed_at: None,
        };

        update_resolved_job(pool, &job).await?;
        self.jobs.lock().await.insert(id.to_string(), job);
        Ok(())
    }

    pub async fn status(
        &self,
        pool: &PgPool,
        user_id: i64,
        id: &str,
    ) -> anyhow::Result<TorrentJobDto> {
        let dto = {
            let mut jobs = self.jobs.lock().await;
            jobs.get_mut(id)
                .filter(|job| job.user_id == user_id)
                .map(|job| {
                    job.refresh_progress();
                    job.dto()
                })
        };

        if let Some(dto) = dto {
            persist_progress(pool, &dto).await?;
            return Ok(dto);
        }

        let row = load_row(pool, user_id, id).await?;
        Ok(row.dto(None))
    }

    pub async fn remove(&self, pool: &PgPool, user_id: i64, id: &str) -> anyhow::Result<()> {
        let removed = {
            let mut jobs = self.jobs.lock().await;
            jobs.remove(id).and_then(|job| job.handle)
        };
        if let Some(handle) = removed {
            self.stop_torrent(id, &handle).await;
        }

        let result =
            sqlx::query("DELETE FROM furumusic__torrent_session WHERE id = $1 AND user_id = $2")
                .bind(id)
                .bind(user_id)
                .execute(pool)
                .await?;

        if result.rows_affected() == 0 {
            bail!("torrent session not found");
        }
        Ok(())
    }

    pub async fn start(
        self: &Arc<Self>,
        pool: &PgPool,
        id: &str,
        selected_files: Vec<usize>,
        inbox_dir: String,
        uploader_user_id: i64,
        proxy_url: Option<&str>,
    ) -> anyhow::Result<TorrentJobDto> {
        if selected_files.is_empty() {
            bail!("select at least one file");
        }
        if inbox_dir.trim().is_empty() {
            bail!("agent_inbox_dir is not configured");
        }
        let inbox_dir = validate_inbox_dir(&inbox_dir)?;

        self.ensure_memory_job(pool, uploader_user_id, id).await?;

        let (torrent_bytes, output_dir, selected_size) = {
            let mut jobs = self.jobs.lock().await;
            let job = jobs.get_mut(id).context("torrent job not found")?;
            if job.user_id != uploader_user_id {
                bail!("torrent job not found");
            }
            if job.handle.is_some()
                && matches!(
                    job.status,
                    TorrentJobStatus::Downloading | TorrentJobStatus::Moving
                )
            {
                bail!("torrent job is already running");
            }
            validate_selection(&job.files, &selected_files)?;
            job.status = TorrentJobStatus::Downloading;
            job.selected_files = selected_files.clone();
            job.downloaded_bytes = 0;
            job.uploaded_bytes = 0;
            job.progress_percent = 0.0;
            job.error = None;
            job.completed_at = None;
            job.updated_at = now_string();
            (
                job.torrent_bytes.clone(),
                job.output_dir.clone(),
                job.selected_size(),
            )
        };

        tokio::fs::create_dir_all(&output_dir).await?;
        mark_job_started(pool, id, &selected_files, &self.memory_job_dto(id).await?).await?;

        let session = self.session(proxy_url).await?;
        let response = match session
            .add_torrent(
                AddTorrent::from_bytes(torrent_bytes),
                Some(AddTorrentOptions {
                    only_files: Some(selected_files),
                    output_folder: Some(output_dir.to_string_lossy().to_string()),
                    overwrite: true,
                    ..Default::default()
                }),
            )
            .await
        {
            Ok(response) => response,
            Err(err) => {
                self.fail_job(pool, id, err.to_string()).await;
                return Err(err.into());
            }
        };

        let handle = match response.into_handle() {
            Some(handle) => handle,
            None => {
                let err = anyhow::anyhow!("torrent did not return a download handle");
                self.fail_job(pool, id, err.to_string()).await;
                return Err(err);
            }
        };
        self.job_sessions
            .lock()
            .await
            .insert(id.to_string(), Arc::clone(&session));

        let dto = {
            let mut jobs = self.jobs.lock().await;
            let job = jobs.get_mut(id).context("torrent job not found")?;
            job.handle = Some(handle.clone());
            job.dto()
        };
        persist_progress(pool, &dto).await?;

        let service = Arc::clone(self);
        let pool = pool.clone();
        let id = id.to_string();
        let download_start = std::time::Instant::now();
        tokio::spawn(async move {
            if let Err(err) = handle.wait_until_completed().await {
                if service.is_paused(&id).await {
                    return;
                }
                service.stop_torrent(&id, &handle).await;
                service.fail_job(&pool, &id, err.to_string()).await;
                crate::metrics::record_torrent_download(
                    "failed",
                    selected_size,
                    download_start.elapsed(),
                );
                return;
            }
            service.stop_torrent(&id, &handle).await;
            if let Err(err) = service
                .finalize_completed(&pool, &id, &inbox_dir, uploader_user_id)
                .await
            {
                service.fail_job(&pool, &id, err.to_string()).await;
                crate::metrics::record_torrent_download(
                    "failed",
                    selected_size,
                    download_start.elapsed(),
                );
            } else {
                crate::metrics::record_torrent_download(
                    "completed",
                    selected_size,
                    download_start.elapsed(),
                );
            }
        });

        Ok(dto)
    }

    pub async fn pause(
        &self,
        pool: &PgPool,
        user_id: i64,
        id: &str,
    ) -> anyhow::Result<TorrentJobDto> {
        self.ensure_memory_job(pool, user_id, id).await?;

        let (dto, handle) = {
            let mut jobs = self.jobs.lock().await;
            let job = jobs.get_mut(id).context("torrent job not found")?;
            if job.user_id != user_id {
                bail!("torrent job not found");
            }
            job.refresh_progress();
            job.status = TorrentJobStatus::Paused;
            job.updated_at = now_string();
            let handle = job.handle.take();
            (job.dto(), handle)
        };

        persist_progress(pool, &dto).await?;
        if let Some(handle) = handle {
            self.stop_torrent(id, &handle).await;
        }
        Ok(dto)
    }

    async fn memory_details(&self, user_id: i64, id: &str) -> Option<TorrentSessionDto> {
        let jobs = self.jobs.lock().await;
        let job = jobs.get(id)?;
        if job.user_id != user_id {
            return None;
        }
        Some(TorrentSessionDto {
            job: job.dto(),
            preview: job.preview(),
            selected_files: job.selected_files.clone(),
        })
    }

    async fn ensure_memory_job(&self, pool: &PgPool, user_id: i64, id: &str) -> anyhow::Result<()> {
        if self.jobs.lock().await.contains_key(id) {
            return Ok(());
        }

        let row = load_row(pool, user_id, id).await?;
        let job = row.into_job(&self.temp_root)?;
        self.jobs.lock().await.insert(id.to_string(), job);
        Ok(())
    }

    async fn memory_job_dto(&self, id: &str) -> anyhow::Result<TorrentJobDto> {
        let jobs = self.jobs.lock().await;
        let job = jobs.get(id).context("torrent job not found")?;
        Ok(job.dto())
    }

    async fn is_paused(&self, id: &str) -> bool {
        let jobs = self.jobs.lock().await;
        jobs.get(id)
            .map(|job| job.status == TorrentJobStatus::Paused)
            .unwrap_or(false)
    }

    async fn fail_job(&self, pool: &PgPool, id: &str, error: String) {
        let dto = {
            let mut jobs = self.jobs.lock().await;
            jobs.get_mut(id).map(|job| {
                job.refresh_progress();
                job.status = TorrentJobStatus::Failed;
                job.error = Some(error.clone());
                job.handle = None;
                job.updated_at = now_string();
                job.dto()
            })
        };
        if let Some(dto) = dto {
            let _ = persist_progress(pool, &dto).await;
        } else {
            let _ = sqlx::query(
                "UPDATE furumusic__torrent_session
                    SET status = 'failed', error = $2, updated_at = $3
                  WHERE id = $1",
            )
            .bind(id)
            .bind(error)
            .bind(now_string())
            .execute(pool)
            .await;
        }
    }

    async fn stop_torrent(&self, id: &str, handle: &Arc<ManagedTorrent>) {
        let session = self.job_sessions.lock().await.remove(id);
        if let Some(session) = session
            && let Err(err) = session.delete(handle.id().into(), false).await
        {
            tracing::warn!("failed to stop completed torrent: {err}");
        }
    }

    async fn finalize_completed(
        &self,
        pool: &PgPool,
        id: &str,
        inbox_dir: &Path,
        uploader_user_id: i64,
    ) -> anyhow::Result<()> {
        let (name, files, selected_files, output_dir) = {
            let mut jobs = self.jobs.lock().await;
            let job = jobs.get_mut(id).context("torrent job not found")?;
            job.refresh_progress();
            job.status = TorrentJobStatus::Moving;
            job.updated_at = now_string();
            (
                job.name.clone(),
                job.files.clone(),
                job.selected_files.clone(),
                job.output_dir.clone(),
            )
        };

        let moving_dto = self.memory_job_dto(id).await?;
        persist_progress(pool, &moving_dto).await?;

        let destination_root = inbox_dir
            .join("user_uploads")
            .join(uploader_user_id.to_string())
            .join(sanitize_path_component(&name));
        tokio::fs::create_dir_all(&destination_root).await?;

        for file in files.iter().filter(|f| selected_files.contains(&f.index)) {
            let source = safe_join(&output_dir, &file.components)?;
            if !tokio::fs::try_exists(&source).await? {
                continue;
            }
            let destination = safe_join(&destination_root, &file.components)?;
            if let Some(parent) = destination.parent() {
                tokio::fs::create_dir_all(parent).await?;
            }
            move_file(&source, &destination).await?;
        }

        let job_root = self.temp_root.join(id);
        let _ = tokio::fs::remove_dir_all(job_root).await;

        let completed_dto = {
            let mut jobs = self.jobs.lock().await;
            let job = jobs.get_mut(id).context("torrent job not found")?;
            job.refresh_progress();
            job.status = TorrentJobStatus::Complete;
            job.downloaded_bytes = job.selected_size();
            job.progress_percent = 100.0;
            job.completed_at = Some(now_string());
            job.updated_at = now_string();
            let dto = job.dto();
            job.handle = None;
            dto
        };
        persist_progress(pool, &completed_dto).await?;

        if let Some(handle) = self.scheduler_handle.get() {
            let handle = Arc::clone(handle);
            tokio::spawn(async move {
                if let Err(err) = handle.trigger_job_now("inbox_discover").await {
                    tracing::warn!("failed to trigger inbox_discover after torrent: {err}");
                }
            });
        }

        Ok(())
    }
}

async fn load_row(pool: &PgPool, user_id: i64, id: &str) -> anyhow::Result<TorrentSessionRow> {
    sqlx::query_as::<_, TorrentSessionRow>(
        r#"SELECT id, user_id, name, info_hash, source_kind, source_label, torrent_bytes,
                  files_json, selected_files_json, status, total_size, selected_size,
                  downloaded_bytes, uploaded_bytes, progress_percent, error,
                  created_at, updated_at, completed_at
             FROM furumusic__torrent_session
            WHERE id = $1 AND user_id = $2"#,
    )
    .bind(id)
    .bind(user_id)
    .fetch_optional(pool)
    .await?
    .context("torrent session not found")
}

async fn insert_job(pool: &PgPool, job: &TorrentJob) -> anyhow::Result<()> {
    sqlx::query(
        r#"INSERT INTO furumusic__torrent_session
              (id, user_id, name, info_hash, source_kind, source_label, torrent_bytes,
               files_json, selected_files_json, status, total_size, selected_size,
               downloaded_bytes, uploaded_bytes, progress_percent, error,
               created_at, updated_at, completed_at)
           VALUES ($1, $2, $3, $4, $5, $6, $7,
                   $8, $9, $10, $11, $12,
                   0, 0, 0, NULL,
                   $13, $14, NULL)"#,
    )
    .bind(&job.id)
    .bind(job.user_id)
    .bind(&job.name)
    .bind(&job.info_hash)
    .bind(&job.source_kind)
    .bind(&job.source_label)
    .bind(&job.torrent_bytes)
    .bind(serde_json::to_string(&job.files)?)
    .bind(serde_json::to_string(&job.selected_files)?)
    .bind(job.status.as_str())
    .bind(u64_to_i64(job.total_size()))
    .bind(u64_to_i64(job.selected_size()))
    .bind(&job.created_at)
    .bind(&job.updated_at)
    .execute(pool)
    .await?;
    Ok(())
}

async fn insert_pending_magnet(
    pool: &PgPool,
    id: &str,
    user_id: i64,
    name: &str,
    info_hash: &str,
    magnet: &str,
    now: &str,
) -> anyhow::Result<()> {
    sqlx::query(
        r#"INSERT INTO furumusic__torrent_session
              (id, user_id, name, info_hash, source_kind, source_label, torrent_bytes,
               files_json, selected_files_json, status, total_size, selected_size,
               downloaded_bytes, uploaded_bytes, progress_percent, error,
               created_at, updated_at, completed_at)
           VALUES ($1, $2, $3, $4, 'magnet', $5, $6,
                   '[]', '[]', 'resolving', 0, 0,
                   0, 0, 0, NULL,
                   $7, $8, NULL)"#,
    )
    .bind(id)
    .bind(user_id)
    .bind(name)
    .bind(info_hash)
    .bind(magnet)
    .bind(Vec::<u8>::new())
    .bind(now)
    .bind(now)
    .execute(pool)
    .await?;
    Ok(())
}

async fn update_resolved_job(pool: &PgPool, job: &TorrentJob) -> anyhow::Result<()> {
    sqlx::query(
        r#"UPDATE furumusic__torrent_session
              SET name = $2,
                  info_hash = $3,
                  torrent_bytes = $4,
                  files_json = $5,
                  selected_files_json = $6,
                  status = 'preview',
                  total_size = $7,
                  selected_size = $8,
                  downloaded_bytes = 0,
                  uploaded_bytes = 0,
                  progress_percent = 0,
                  error = NULL,
                  updated_at = $9,
                  completed_at = NULL
            WHERE id = $1"#,
    )
    .bind(&job.id)
    .bind(&job.name)
    .bind(&job.info_hash)
    .bind(&job.torrent_bytes)
    .bind(serde_json::to_string(&job.files)?)
    .bind(serde_json::to_string(&job.selected_files)?)
    .bind(u64_to_i64(job.total_size()))
    .bind(u64_to_i64(job.selected_size()))
    .bind(&job.updated_at)
    .execute(pool)
    .await?;
    Ok(())
}

async fn update_resolving_error(pool: &PgPool, id: &str, error: &str) {
    if let Err(err) = sqlx::query(
        r#"UPDATE furumusic__torrent_session
              SET error = $2,
                  updated_at = $3
            WHERE id = $1 AND status = 'resolving'"#,
    )
    .bind(id)
    .bind(error)
    .bind(now_string())
    .execute(pool)
    .await
    {
        tracing::warn!("failed to persist torrent metadata resolving error: {err}");
    }
}

async fn mark_job_started(
    pool: &PgPool,
    id: &str,
    selected_files: &[usize],
    dto: &TorrentJobDto,
) -> anyhow::Result<()> {
    sqlx::query(
        r#"UPDATE furumusic__torrent_session
              SET selected_files_json = $2,
                  status = 'downloading',
                  selected_size = $3,
                  downloaded_bytes = 0,
                  uploaded_bytes = 0,
                  progress_percent = 0,
                  error = NULL,
                  updated_at = $4,
                  completed_at = NULL
            WHERE id = $1"#,
    )
    .bind(id)
    .bind(serde_json::to_string(selected_files)?)
    .bind(u64_to_i64(dto.selected_size))
    .bind(now_string())
    .execute(pool)
    .await?;
    Ok(())
}

async fn persist_progress(pool: &PgPool, dto: &TorrentJobDto) -> anyhow::Result<()> {
    sqlx::query(
        r#"UPDATE furumusic__torrent_session
              SET status = $2,
                  selected_size = $3,
                  downloaded_bytes = $4,
                  uploaded_bytes = $5,
                  progress_percent = $6,
                  error = $7,
                  updated_at = $8,
                  completed_at = $9
            WHERE id = $1"#,
    )
    .bind(&dto.id)
    .bind(&dto.status)
    .bind(u64_to_i64(dto.selected_size))
    .bind(u64_to_i64(dto.downloaded_bytes))
    .bind(u64_to_i64(dto.uploaded_bytes))
    .bind(dto.progress_percent)
    .bind(&dto.error)
    .bind(now_string())
    .bind(&dto.completed_at)
    .execute(pool)
    .await?;
    Ok(())
}

fn validate_selection(files: &[TorrentFileDto], selected_files: &[usize]) -> anyhow::Result<()> {
    for index in selected_files {
        if !files.iter().any(|file| file.index == *index) {
            bail!("selected file index {index} is not in this torrent");
        }
    }
    Ok(())
}

fn validate_inbox_dir(inbox_dir: &str) -> anyhow::Result<PathBuf> {
    let trimmed = inbox_dir.trim();
    let path = crate::media_paths::resolve_config_path_buf(trimmed);
    if !path.is_absolute() {
        bail!(
            "agent_inbox_dir must be an absolute path for this host, got `{}`",
            trimmed
        );
    }
    Ok(path)
}

fn selected_size(files: &[TorrentFileDto], selected_files: &[usize]) -> u64 {
    if selected_files.is_empty() {
        return 0;
    }
    files
        .iter()
        .filter(|f| selected_files.contains(&f.index))
        .map(|f| f.length)
        .sum()
}

fn progress_percent(downloaded: u64, total: u64) -> Option<f64> {
    if total == 0 {
        None
    } else {
        Some(downloaded as f64 / total as f64 * 100.0)
    }
}

fn now_string() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

fn u64_to_i64(value: u64) -> i64 {
    value.min(i64::MAX as u64) as i64
}

fn i64_to_u64(value: i64) -> u64 {
    value.max(0) as u64
}

fn extract_magnet_info_hash(magnet: &str) -> Option<String> {
    if !magnet.starts_with("magnet:?") {
        return None;
    }
    magnet
        .split(['?', '&'])
        .find_map(|part| part.strip_prefix("xt=urn:btih:"))
        .map(|hash| percent_decode(hash).to_ascii_lowercase())
        .filter(|hash| !hash.is_empty())
}

fn magnet_display_name(magnet: &str) -> Option<String> {
    magnet
        .split(['?', '&'])
        .find_map(|part| part.strip_prefix("dn="))
        .map(percent_decode)
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'+' => {
                out.push(b' ');
                index += 1;
            }
            b'%' if index + 2 < bytes.len() => {
                let hi = hex_value(bytes[index + 1]);
                let lo = hex_value(bytes[index + 2]);
                if let (Some(hi), Some(lo)) = (hi, lo) {
                    out.push((hi << 4) | lo);
                    index += 3;
                } else {
                    out.push(bytes[index]);
                    index += 1;
                }
            }
            byte => {
                out.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).to_string()
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn sanitize_path_component(value: &str) -> String {
    let sanitized: String = value
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect();
    let trimmed = sanitized.trim().trim_matches('.').trim();
    if trimmed.is_empty() {
        "torrent".to_string()
    } else {
        trimmed.to_string()
    }
}

fn safe_join(root: &Path, components: &[String]) -> anyhow::Result<PathBuf> {
    let mut path = root.to_path_buf();
    for component in components {
        let sanitized = sanitize_path_component(component);
        if sanitized == "." || sanitized == ".." {
            bail!("unsafe torrent path component");
        }
        path.push(sanitized);
    }
    Ok(path)
}

async fn move_file(source: &Path, destination: &Path) -> anyhow::Result<()> {
    match tokio::fs::rename(source, destination).await {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::CrossesDevices => {
            tokio::fs::copy(source, destination).await?;
            tokio::fs::remove_file(source).await?;
            Ok(())
        }
        Err(err) => Err(err.into()),
    }
}
