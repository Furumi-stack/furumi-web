use super::*;
use sqlx::{FromRow, PgPool};
use tokio::sync::OnceCell;

#[cfg(test)]
#[path = "download_history_tests.rs"]
mod tests;

#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
enum Source {
    Files,
    Torrents,
    Youtube,
}

impl Source {
    fn name(&self) -> &'static str {
        match self {
            Self::Files => "files",
            Self::Torrents => "torrents",
            Self::Youtube => "youtube",
        }
    }
}

#[derive(Deserialize)]
struct Key {
    source: Source,
    id: String,
}
#[derive(Deserialize)]
struct Hide {
    items: Vec<Key>,
}
#[derive(Deserialize)]
struct Query {
    limit: Option<i64>,
    offset: Option<i64>,
    source: Option<Source>,
    id: Option<String>,
}
#[derive(Serialize, FromRow)]
struct Entry {
    source: String,
    id: String,
    title: Option<String>,
    status: String,
    created_at: String,
    updated_at: String,
    item_count: i64,
    progress_percent: Option<f64>,
    error: Option<String>,
    #[serde(skip)]
    processed: bool,
}
#[derive(Serialize, FromRow)]
struct Stats {
    tracks: i64,
    releases: i64,
}
#[derive(Serialize)]
struct Page {
    items: Vec<Entry>,
    stats: Stats,
    has_more: bool,
}

async fn keys(
    pool: &PgPool,
    user: i64,
    offset: i64,
    limit: i64,
    entry: Option<(&Source, &str)>,
) -> sqlx::Result<Vec<(String, String)>> {
    sqlx::query_as(
        "SELECT source,id FROM (
            SELECT 'files' AS source, batch_id AS id, min(created_at) AS added
            FROM furumusic__local_upload WHERE user_id=$1 GROUP BY batch_id
            UNION ALL SELECT 'torrents',id,created_at FROM furumusic__torrent_session WHERE user_id=$1
            UNION ALL SELECT 'youtube',id,created_at FROM furumusic__youtube_download WHERE user_id=$1
         ) jobs WHERE ($4::text IS NULL OR (source=$4 AND id=$5))
         AND NOT EXISTS (
            SELECT 1 FROM furumusic__hidden_download h
            WHERE h.user_id=$1 AND h.source=jobs.source AND h.download_id=jobs.id
         ) ORDER BY added DESC,source,id LIMIT $2 OFFSET $3"
    )
    .bind(user)
    .bind(limit)
    .bind(offset)
    .bind(entry.map(|(source,_)| source.name()))
    .bind(entry.map(|(_,id)| id))
    .fetch_all(pool)
    .await
}

async fn stats(pool: &PgPool, user: i64) -> sqlx::Result<Stats> {
    sqlx::query_as(
        "SELECT count(*) AS tracks,count(DISTINCT t.release_id) AS releases
        FROM furumusic__track t JOIN furumusic__media_file f ON f.id=t.audio_file_id
        WHERE f.uploaded_by_user_id=$1",
    )
    .bind(user)
    .fetch_one(pool)
    .await
}

async fn summaries(
    pool: &PgPool,
    user: i64,
    sources: &[String],
    ids: &[String],
) -> sqlx::Result<Vec<Entry>> {
    sqlx::query_as(include_str!("download_history_summary.sql"))
        .bind(user)
        .bind(sources)
        .bind(ids)
        .fetch_all(pool)
        .await
}

async fn hide(pool: &PgPool, user: i64, items: &[Key]) -> sqlx::Result<()> {
    let sources: Vec<_> = items.iter().map(|item| item.source.name()).collect();
    let ids: Vec<_> = items.iter().map(|item| item.id.as_str()).collect();
    sqlx::query(
        "INSERT INTO furumusic__hidden_download(user_id, source, download_id)
        SELECT DISTINCT $1,source,id FROM unnest($2::text[],$3::text[]) wanted(source,id)
        WHERE CASE source
          WHEN 'files' THEN EXISTS (SELECT 1 FROM furumusic__local_upload f WHERE f.user_id=$1 AND f.batch_id=wanted.id)
          WHEN 'torrents' THEN EXISTS (SELECT 1 FROM furumusic__torrent_session t WHERE t.user_id=$1 AND t.id=wanted.id)
          WHEN 'youtube' THEN EXISTS (SELECT 1 FROM furumusic__youtube_download y WHERE y.user_id=$1 AND y.id=wanted.id)
        END ON CONFLICT DO NOTHING"
    )
    .bind(user)
    .bind(sources)
    .bind(ids)
    .execute(pool)
    .await?;
    Ok(())
}

// Discovery discards exact duplicates without creating a review record. The
// inbox is authoritative until all audio has been moved or deduplicated.
async fn has_inbox_audio(dir: &std::path::Path) -> std::io::Result<bool> {
    let mut pending = vec![dir.to_owned()];
    while let Some(dir) = pending.pop() {
        let mut entries = match tokio::fs::read_dir(dir).await {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e),
        };
        while let Some(entry) = entries.next_entry().await? {
            let filename = entry.file_name();
            let name = filename.to_string_lossy();
            if name.starts_with('.') {
                continue;
            }
            let kind = entry.file_type().await?;
            if kind.is_dir() {
                pending.push(entry.path());
            } else if kind.is_file() && crate::jobs::inbox_discover::is_audio_file(&name) {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

async fn torrent_status(
    pool: &PgPool,
    user: i64,
    inbox: Option<&std::path::Path>,
    service: &TorrentService,
    item: &mut Entry,
) -> anyhow::Result<()> {
    if matches!(item.status.as_str(), "downloading" | "moving") {
        if let Some(progress) = service.live_progress(user, &item.id).await {
            item.progress_percent = Some(progress);
        } else {
            item.status = "paused".into();
        }
    }
    if item.status == "complete" && !item.processed {
        let Some(inbox) = inbox else {
            item.status = "awaiting_ai".into();
            return Ok(());
        };
        let dir = inbox
            .join("user_uploads")
            .join(user.to_string())
            .join(format!("torrent-{}", item.id));
        if has_inbox_audio(&dir).await? {
            item.status = "awaiting_ai".into();
        } else {
            sqlx::query("UPDATE furumusic__torrent_session SET history_processed=true
                WHERE user_id=$1 AND id=$2 AND status='complete' AND updated_at=$3 AND NOT history_processed")
                .bind(user).bind(&item.id).bind(&item.updated_at).execute(pool).await?;
        }
    }
    Ok(())
}

#[derive(Clone)]
pub(super) struct Services {
    pub pool: Arc<OnceCell<PgPool>>,
    pub config: Arc<AppConfig>,
    pub torrents: Arc<OnceCell<Arc<TorrentService>>>,
    pub youtube: Arc<OnceCell<Arc<YouTubeService>>>,
    pub scheduler: Arc<OnceCell<Arc<SchedulerHandle>>>,
}

impl Services {
    async fn pool(&self) -> cot::Result<&PgPool> {
        self.pool
            .get_or_try_init(|| {
                sqlx::postgres::PgPoolOptions::new()
                    .max_connections(5)
                    .connect(&self.config.database_url)
            })
            .await
            .map_err(|e| cot::Error::internal(e.to_string()))
    }

    async fn page(
        &self,
        db: &Database,
        user: i64,
        config: &AppConfig,
        query: Query,
    ) -> anyhow::Result<Page> {
        let pool = self.pool().await?;
        let entry = query.source.as_ref().zip(query.id.as_deref());
        let limit = if entry.is_some() {
            1
        } else {
            query.limit.unwrap_or(30).clamp(1, 200)
        };
        let offset = if entry.is_some() {
            0
        } else {
            query.offset.unwrap_or(0).max(0)
        };
        let (mut page, stats) = tokio::try_join!(
            keys(pool, user, offset, limit + 1, entry),
            stats(pool, user)
        )?;
        let has_more = page.len() as i64 > limit;
        page.truncate(limit as usize);
        let (sources, ids): (Vec<_>, Vec<_>) = page.into_iter().unzip();
        let ids_for = |kind: &str| {
            sources
                .iter()
                .zip(&ids)
                .filter(|(s, _)| s.as_str() == kind)
                .map(|(_, id)| id.clone())
                .collect::<Vec<_>>()
        };
        let local = ids_for("files");
        let youtube = ids_for("youtube");
        let inbox = config.agent_inbox_dir.trim();
        let root = crate::media_paths::resolve_config_path_buf(inbox);
        let inbox_available =
            !inbox.is_empty() && tokio::fs::metadata(&root).await.is_ok_and(|m| m.is_dir());
        if !local.is_empty()
            && inbox_available
            && let Err(error) =
                crate::local_uploads::sync_statuses(pool, user, inbox, Some(&local)).await
        {
            tracing::warn!(user, %error, "could not refresh local upload status");
        }
        if !youtube.is_empty()
            && let Err(error) = crate::youtube::sync_ai_statuses(pool, user, Some(&youtube)).await
        {
            tracing::warn!(user, %error, "could not refresh YouTube import status");
        }
        let torrents = self
            .torrents
            .get_or_init(|| async { Arc::new(TorrentService::new(self.scheduler.clone())) })
            .await;
        if config.torrent_downloads_enabled
            && let Ok(proxy) = config.torrent_proxy_url()
            && let Err(error) = torrents.resume_pending(pool, user, proxy).await
        {
            tracing::warn!(user, %error, "could not resume pending magnets");
        }
        if config.youtube_downloads_enabled
            && inbox_available
            && let Err(error) = self.resume_youtube(pool, db, config, user).await
        {
            tracing::warn!(user, %error, "could not resume pending YouTube downloads");
        }
        let mut items = summaries(pool, user, &sources, &ids).await?;
        for item in items.iter_mut().filter(|item| item.source == "torrents") {
            if let Err(error) = torrent_status(
                pool,
                user,
                inbox_available.then_some(root.as_path()),
                torrents,
                item,
            )
            .await
            {
                tracing::warn!(user, id=item.id, %error, "could not verify torrent import status");
                item.status = "awaiting_ai".into();
            }
        }
        Ok(Page {
            items,
            stats,
            has_more,
        })
    }

    async fn resume_youtube(
        &self,
        pool: &PgPool,
        db: &Database,
        config: &AppConfig,
        user: i64,
    ) -> anyhow::Result<()> {
        let service = self
            .youtube
            .get_or_init(|| async { Arc::new(YouTubeService::new(self.scheduler.clone())) })
            .await;
        let jobs = service.pending_jobs(pool, user).await?;
        if !jobs.is_empty() {
            let cookies =
                crate::youtube::selected_cookie_contents(db, &config.youtube_cookie_id).await?;
            service
                .resume_jobs(
                    pool,
                    jobs,
                    &config.agent_inbox_dir,
                    config.youtube_proxy_url()?,
                    cookies,
                )
                .await;
        }
        Ok(())
    }

    pub(super) fn routes(self) -> [Route; 2] {
        let read = self.clone();
        let list = get(
            move |auth_ctx, session, db, UrlQuery(query): UrlQuery<Query>| {
                let read = read.clone();
                async move {
                    let Some(user) = auth::get_request_user(&auth_ctx, &session, &db).await else {
                        return Ok(json_error(StatusCode::UNAUTHORIZED, "not authenticated"));
                    };
                    let (config, _) = AppConfig::load_with_db(&db).await;
                    if let Err(error) = require_download_method(&config, DownloadMethod::LocalFile)
                    {
                        return Ok(error);
                    }
                    if query.source.is_some() != query.id.is_some()
                        || query
                            .id
                            .as_ref()
                            .is_some_and(|id| id.trim().is_empty() || id.len() > 36)
                    {
                        return Ok(json_error(
                            StatusCode::BAD_REQUEST,
                            "expected a download source and ID together",
                        ));
                    }
                    let page = read
                        .page(&db, user.id, &config, query)
                        .await
                        .map_err(|e| cot::Error::internal(e.to_string()))?;
                    Json(page).into_response()
                }
            },
        );
        let remove = post(move |auth_ctx, session, db, Json(body): Json<Hide>| {
            let services = self.clone();
            async move {
                let Some(user) = auth::get_request_user(&auth_ctx, &session, &db).await else {
                    return Ok(json_error(StatusCode::UNAUTHORIZED, "not authenticated"));
                };
                let (config, _) = AppConfig::load_with_db(&db).await;
                if let Err(error) = require_download_method(&config, DownloadMethod::LocalFile) {
                    return Ok(error);
                }
                hide(services.pool().await?, user.id, &body.items)
                    .await
                    .map_err(|e| cot::Error::internal(e.to_string()))?;
                Json(serde_json::json!({"ok":true})).into_response()
            }
        });
        [
            Route::with_handler_and_name("/downloads/history", list, "player_download_history"),
            Route::with_handler_and_name(
                "/downloads/history/hide",
                remove,
                "player_download_history_hide",
            ),
        ]
    }
}
