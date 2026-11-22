use super::*;
use crate::{auth, auth::Role};
use cot::http::{Request, StatusCode};
use cot::{
    App, Body,
    db::Database,
    json::Json,
    request::extractors::Path,
    response::IntoResponse,
    router::{
        Route, Router,
        method::{get, post},
    },
    session::Session,
};
use futures_util::TryStreamExt;
use serde::Deserialize;
use std::sync::Arc;

#[cfg(test)]
#[path = "tests.rs"]
mod tests;

#[derive(Deserialize)]
struct Action {
    action: String,
    #[serde(default)]
    id: String,
    #[serde(default)]
    url: String,
    #[serde(default)]
    query: String,
    #[serde(default)]
    shared: bool,
    #[serde(default)]
    confirmed: bool,
    #[serde(default)]
    position_ms: i64,
    #[serde(default)]
    completed: bool,
    #[serde(default)]
    explicit: bool,
    #[serde(default)]
    generation: i64,
    #[serde(default)]
    sequence: i64,
    #[serde(default)]
    offset: i64,
    retention_days: Option<i32>,
}
#[derive(Deserialize)]
struct EpisodePath {
    id: String,
}

fn episode_path_id(value: &str) -> Result<music_dht::podcast::EpisodeId> {
    // Cot's path extractor preserves percent escapes. The contract's only
    // escaped path character is the colon separating the id prefix.
    let decoded = value.replace("%3A", ":").replace("%3a", ":");
    Ok(serde_json::from_value(json!(decoded))?)
}

fn error(status: StatusCode, message: &str) -> cot::Result<cot::response::Response> {
    Ok(cot::http::Response::builder()
        .status(status)
        .header("Content-Type", "application/json")
        .body(Body::fixed(json!({"error":message}).to_string()))
        .expect("valid response"))
}

async fn list(pool: &PgPool, user: i64, admin: bool) -> Result<Value> {
    let mine: Vec<Value> = sqlx::query_scalar("SELECT jsonb_build_object('id',p.id,'metadata',p.metadata,'shared',s.shared,'checked_at',p.checked_at,'error',p.last_error,'episodes',(SELECT count(*) FROM furumusic__podcast_episode e WHERE e.podcast_id=p.id),'new_episodes',(SELECT count(*) FROM furumusic__podcast_episode e WHERE e.podcast_id=p.id AND NOT EXISTS(SELECT 1 FROM furumusic__podcast_progress g WHERE g.user_id=$1 AND g.episode_id=e.id AND g.completed))) FROM furumusic__podcast p JOIN furumusic__podcast_subscription s ON s.podcast_id=p.id WHERE s.user_id=$1 ORDER BY p.metadata->>'title'").bind(user).fetch_all(pool).await?;
    let public: Vec<Value> = sqlx::query_scalar("SELECT jsonb_build_object('id',p.id,'url',p.origin_url,'metadata',p.metadata,'subscribed',EXISTS(SELECT 1 FROM furumusic__podcast_subscription m WHERE m.user_id=$1 AND m.podcast_id=p.id)) FROM furumusic__podcast p WHERE EXISTS(SELECT 1 FROM furumusic__podcast_subscription s WHERE s.podcast_id=p.id AND s.shared AND s.user_id<>$1) ORDER BY p.metadata->>'title' LIMIT 200").bind(user).fetch_all(pool).await?;
    let mut result = json!({"mine":mine,"public":public});
    if admin {
        let all: Vec<Value> = sqlx::query_scalar("SELECT jsonb_build_object('id',p.id,'metadata',p.metadata,'error',p.last_error,'checked_at',p.checked_at,'subscribers',(SELECT count(*) FROM furumusic__podcast_subscription s WHERE s.podcast_id=p.id),'public',(SELECT count(*) FROM furumusic__podcast_subscription s WHERE s.podcast_id=p.id AND s.shared),'cached',(SELECT count(*) FROM furumusic__podcast_episode e WHERE e.podcast_id=p.id AND cached_at IS NOT NULL)) FROM furumusic__podcast p ORDER BY p.metadata->>'title'").fetch_all(pool).await?;
        let days: Option<i32> =
            sqlx::query_scalar("SELECT retention_days FROM furumusic__podcast_settings WHERE id=1")
                .fetch_one(pool)
                .await?;
        result["all"] = json!(all);
        result["retention_days"] = json!(days);
    }
    Ok(result)
}

async fn require_subscription(pool: &PgPool, user: i64, id: &str) -> Result<()> {
    let yes: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM furumusic__podcast_subscription WHERE user_id=$1 AND podcast_id=$2)").bind(user).bind(id).fetch_one(pool).await?;
    if !yes {
        bail!("podcast subscription not found");
    }
    Ok(())
}

async fn action(
    pool: PgPool,
    config: AppConfig,
    user: i64,
    admin: bool,
    body: Action,
) -> Result<Value> {
    if body.action.starts_with("admin_") && !admin {
        bail!("administrator access required");
    }
    match body.action.as_str() {
        "list" => return list(&pool, user, false).await,
        "detail" => {
            let detail:Option<Value>=sqlx::query_scalar("SELECT jsonb_build_object('id',p.id,'url',p.origin_url,'metadata',p.metadata,'subscribed',EXISTS(SELECT 1 FROM furumusic__podcast_subscription s WHERE s.user_id=$1 AND s.podcast_id=p.id),'episodes',(SELECT count(*) FROM furumusic__podcast_episode e WHERE e.podcast_id=p.id)) FROM furumusic__podcast p WHERE p.id=$2 AND EXISTS(SELECT 1 FROM furumusic__podcast_subscription s WHERE s.podcast_id=p.id AND (s.user_id=$1 OR s.shared))").bind(user).bind(&body.id).fetch_optional(&pool).await?;
            return detail.context("podcast not found");
        }
        "admin_list" => return list(&pool, user, true).await,
        "add" => {
            let id = subscribe(&pool, user, &body.url).await?;
            sync::ensure_subscribed(&pool, user, &id).await?;
            sync::seed_user(&pool, user).await?;
            let checked = refresh(&pool, &id).await;
            let download_id = id.clone();
            tokio::spawn(async move {
                let _ = download_pending(&pool, &config, Some(&download_id)).await;
            });
            return Ok(
                json!({"ok":true,"id":id,"warning":checked.err().map(|_| "Feed check failed; the subscription was saved") }),
            );
        }
        "remove" => {
            require_subscription(&pool, user, &body.id).await?;
            sync::local_subscription(&pool, user, &body.id, false).await?;
            sqlx::query(
                "DELETE FROM furumusic__podcast_subscription WHERE user_id=$1 AND podcast_id=$2",
            )
            .bind(user)
            .bind(&body.id)
            .execute(&pool)
            .await?;
        }
        "share" => {
            require_subscription(&pool, user, &body.id).await?;
            sync::seed_user(&pool, user).await?;
            let row = sqlx::query("SELECT origin_url,metadata FROM furumusic__podcast WHERE id=$1")
                .bind(&body.id)
                .fetch_one(&pool)
                .await?;
            let meta: PodcastMeta = serde_json::from_value(row.get("metadata"))?;
            let origin = FeedUrl::parse(row.get::<&str, _>("origin_url"))?;
            let sub = SubscriptionRecord {
                podcast_id: serde_json::from_value(json!(body.id))?,
                subscribed: true,
                podcast: meta,
                feed_aliases: vec![origin],
                generation: 1,
                hlc_ms: 1,
                op_id: "web".into(),
            };
            let card = PodcastCardSpec::from_subscription(&sub)?;
            let urls = json!([card.feed_url, card.id_feed_url]);
            if body.shared && !body.confirmed && !card.privacy_warnings().is_empty() {
                return Ok(json!({"confirmation_required":true,"urls":urls}));
            }
            sqlx::query("UPDATE furumusic__podcast_subscription SET shared=$3,confirmed_urls=$4 WHERE user_id=$1 AND podcast_id=$2").bind(user).bind(&body.id).bind(body.shared).bind(if body.shared {Some(urls)} else {None}).execute(&pool).await?;
            sync::local_share(&pool, user, &body.id, body.shared).await?;
        }
        "episodes" => {
            require_subscription(&pool, user, &body.id).await?;
            let mut episodes: Vec<Value> = sqlx::query_scalar("SELECT jsonb_build_object('id',e.id,'metadata',e.metadata,'cached',e.cached_at IS NOT NULL,'downloading',COALESCE(e.download_until>now(),false),'error',e.download_error,'position_ms',COALESCE(g.position_ms,0),'completed',COALESCE(g.completed,false)) FROM furumusic__podcast_episode e LEFT JOIN furumusic__podcast_progress g ON g.episode_id=e.id AND g.user_id=$1 WHERE e.podcast_id=$2 ORDER BY (e.metadata->>'published_at_ms')::bigint DESC NULLS LAST,e.discovered_at DESC,e.id LIMIT 201 OFFSET $3").bind(user).bind(&body.id).bind(body.offset.max(0)).fetch_all(&pool).await?;
            let more = episodes.len() > 200;
            episodes.truncate(200);
            return Ok(json!({"episodes":episodes,"has_more":more}));
        }
        "refresh" | "admin_refresh" => {
            if body.action == "refresh" {
                require_subscription(&pool, user, &body.id).await?;
            }
            let added = refresh(&pool, &body.id).await?;
            tokio::spawn(async move {
                let _ = download_pending(&pool, &config, Some(&body.id)).await;
            });
            return Ok(json!({"added":added}));
        }
        "download" | "progress" => {
            let podcast: String =
                sqlx::query_scalar("SELECT podcast_id FROM furumusic__podcast_episode WHERE id=$1")
                    .bind(&body.id)
                    .fetch_one(&pool)
                    .await?;
            require_subscription(&pool, user, &podcast).await?;
            if body.action == "download" {
                tokio::spawn(async move {
                    let _ = download(&pool, &config, &body.id, true).await;
                });
                return Ok(json!({"ok":true}));
            }
            let duration: Option<i64> = sqlx::query_scalar("SELECT (metadata->>'duration_ms')::bigint FROM furumusic__podcast_episode WHERE id=$1").bind(&body.id).fetch_one(&pool).await?;
            let completed = body.completed
                || (!body.explicit
                    && music_dht::podcast::reaches_completion(body.position_ms, duration));
            let generation: Option<i64> = if body.explicit {
                Some(sqlx::query_scalar("INSERT INTO furumusic__podcast_progress(user_id,episode_id,position_ms,completed,generation,checkpoint) VALUES($1,$2,$3,$4,1,$5) ON CONFLICT(user_id,episode_id) DO UPDATE SET position_ms=EXCLUDED.position_ms,completed=EXCLUDED.completed,generation=furumusic__podcast_progress.generation+1,checkpoint=EXCLUDED.checkpoint,updated_at=now() RETURNING generation").bind(user).bind(&body.id).bind(body.position_ms.max(0)).bind(completed).bind(body.sequence).fetch_one(&pool).await?)
            } else {
                sqlx::query_scalar("UPDATE furumusic__podcast_progress SET position_ms=$3,completed=$4,checkpoint=$6,updated_at=now() WHERE user_id=$1 AND episode_id=$2 AND generation=$5 AND checkpoint<=$6 RETURNING generation").bind(user).bind(&body.id).bind(body.position_ms.max(0)).bind(completed).bind(body.generation).bind(body.sequence).fetch_optional(&pool).await?
            };
            if let Some(generation) = generation {
                sync::local_progress(
                    &pool,
                    user,
                    &body.id,
                    generation.max(0) as u64,
                    body.position_ms.max(0),
                    completed,
                )
                .await?;
            }
            return Ok(json!({"ok":true,"generation":generation}));
        }
        "search" => {
            if body.query.chars().count() < 2 || body.query.len() > 200 {
                bail!("enter 2–200 characters");
            }
            let mut hits = super::search(&pool, user, &body.query, 100).await?;
            if let Ok(Ok(Value::Array(remote))) = tokio::time::timeout(
                Duration::from_secs(10),
                crate::federation::handle().search_podcasts(&body.query),
            )
            .await
            {
                for hit in remote {
                    if !hits.iter().any(|local| local["id"] == hit["id"]) {
                        hits.push(hit);
                    }
                }
            }
            return Ok(json!({"hits":hits}));
        }
        "admin_settings" => {
            if body
                .retention_days
                .is_some_and(|days| !(1..=36500).contains(&days))
            {
                bail!("retention must be between 1 and 36500 days");
            }
            sqlx::query("UPDATE furumusic__podcast_settings SET retention_days=$1 WHERE id=1")
                .bind(body.retention_days)
                .execute(&pool)
                .await?;
        }
        "admin_unpublish" => {
            let owners:Vec<i64>=sqlx::query_scalar("SELECT user_id FROM furumusic__podcast_subscription WHERE podcast_id=$1 AND shared").bind(&body.id).fetch_all(&pool).await?;
            for owner in owners {
                sync::seed_user(&pool, owner).await?;
                sync::local_share(&pool, owner, &body.id, false).await?;
            }
            sqlx::query("UPDATE furumusic__podcast_subscription SET shared=false,confirmed_urls=NULL WHERE podcast_id=$1").bind(&body.id).execute(&pool).await?;
        }
        "admin_clear" | "admin_delete" => {
            if body.action == "admin_delete" {
                let owners: Vec<i64> = sqlx::query_scalar(
                    "SELECT user_id FROM furumusic__podcast_subscription WHERE podcast_id=$1",
                )
                .bind(&body.id)
                .fetch_all(&pool)
                .await?;
                for owner in owners {
                    sync::local_subscription(&pool, owner, &body.id, false).await?;
                }
            }
            let mut tx = pool.begin().await?;
            let locked: bool =
                sqlx::query_scalar("SELECT pg_try_advisory_xact_lock(hashtextextended($1,7421))")
                    .bind(&body.id)
                    .fetch_one(&mut *tx)
                    .await?;
            if !locked {
                bail!("downloads are running; try again when they finish");
            }
            sqlx::query("SELECT id FROM furumusic__podcast WHERE id=$1 FOR UPDATE")
                .bind(&body.id)
                .fetch_optional(&mut *tx)
                .await?;
            let ids: Vec<String> =
                sqlx::query_scalar("SELECT id FROM furumusic__podcast_episode WHERE podcast_id=$1")
                    .bind(&body.id)
                    .fetch_all(&pool)
                    .await?;
            // A leased download must finish before clearing or deleting its catalogue.
            let busy: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM furumusic__podcast_episode WHERE podcast_id=$1 AND download_until>now())").bind(&body.id).fetch_one(&pool).await?;
            if busy {
                bail!("downloads are running; try again when they finish");
            }
            for id in ids {
                match tokio::fs::remove_file(cache_path(&config, &id)).await {
                    Ok(()) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => return Err(e.into()),
                }
            }
            sqlx::query("UPDATE furumusic__podcast_episode SET cached_at=NULL,expired=true WHERE podcast_id=$1").bind(&body.id).execute(&mut *tx).await?;
            if body.action == "admin_delete" {
                sqlx::query("DELETE FROM furumusic__podcast WHERE id=$1")
                    .bind(&body.id)
                    .execute(&mut *tx)
                    .await?;
            }
            tx.commit().await?;
        }
        _ => bail!("unknown podcast action"),
    }
    let federation = crate::federation::handle();
    let pending = federation.sync_podcasts().await.is_err();
    Ok(json!({"ok":true,"publication_pending":pending}))
}

async fn media(
    pool: &PgPool,
    config: &AppConfig,
    user: i64,
    id: &str,
    request: &Request<Body>,
) -> Result<cot::response::Response> {
    let row = sqlx::query("SELECT e.metadata,e.cached_at IS NOT NULL AS cached FROM furumusic__podcast_episode e JOIN furumusic__podcast_subscription s ON s.podcast_id=e.podcast_id WHERE e.id=$1 AND s.user_id=$2").bind(id).bind(user).fetch_one(pool).await?;
    let meta: EpisodeMeta = serde_json::from_value(row.get("metadata"))?;
    let range = request.headers().get("range").and_then(|v| v.to_str().ok());
    let mut builder = cot::http::Response::builder().header("Cache-Control", "private, no-store");
    let path = cache_path(config, id);
    if row.get::<bool, _>("cached") && tokio::fs::try_exists(&path).await? {
        use tokio::io::{AsyncReadExt, AsyncSeekExt};
        let mut file = tokio::fs::File::open(&path).await?;
        let size = file.metadata().await?.len();
        let (start, end) = if let Some(range) = range {
            match crate::player::parse_range(range, size) {
                Some((start, end)) => {
                    builder = builder
                        .status(206)
                        .header("Content-Range", format!("bytes {start}-{end}/{size}"));
                    (start, end)
                }
                None => {
                    return Ok(builder
                        .status(416)
                        .header("Content-Range", format!("bytes */{size}"))
                        .body(Body::empty())?);
                }
            }
        } else {
            (0, size.saturating_sub(1))
        };
        file.seek(std::io::SeekFrom::Start(start)).await?;
        let stream = tokio_util::io::ReaderStream::new(file.take(end - start + 1))
            .map_err(|e| cot::Error::internal(e.to_string()));
        return Ok(builder
            .header("Content-Type", meta.mime.as_deref().unwrap_or("audio/mpeg"))
            .header("Accept-Ranges", "bytes")
            .header("Content-Length", (end - start + 1).to_string())
            .body(Body::streaming(stream))?);
    }
    sqlx::query("UPDATE furumusic__podcast_episode SET cached_at=NULL WHERE id=$1")
        .bind(id)
        .execute(pool)
        .await?;
    let pool_clone = pool.clone();
    let config_clone = config.clone();
    let id_clone = id.to_owned();
    tokio::spawn(async move {
        let _ = download(&pool_clone, &config_clone, &id_clone, true).await;
    });
    let response = fetch(&meta.audio_url, range, None).await?;
    builder = builder.status(response.status());
    for key in [
        "content-type",
        "content-length",
        "content-range",
        "accept-ranges",
    ] {
        if let Some(value) = response.headers().get(key) {
            builder = builder.header(key, value);
        }
    }
    let stream = response
        .bytes_stream()
        .map_err(|_| cot::Error::internal("podcast stream interrupted"));
    Ok(builder.body(Body::streaming(stream))?)
}

pub struct PodcastApp {
    config: Arc<AppConfig>,
}
impl PodcastApp {
    pub fn new(config: Arc<AppConfig>) -> Self {
        Self { config }
    }
}
impl App for PodcastApp {
    fn name(&self) -> &'static str {
        "podcasts"
    }
    fn router(&self) -> Router {
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(8)
            .connect_lazy(&self.config.database_url)
            .expect("podcast pool configuration");
        let action_pool = pool.clone();
        Router::with_urls([
            Route::with_handler_and_name(
                "/action",
                {
                    post(
                        move |auth_ctx: auth::AuthContext,
                              session: Session,
                              db: Database,
                              Json(body): Json<Action>| {
                            let pool = action_pool.clone();
                            async move {
                                let Some(user) =
                                    auth::get_request_user(&auth_ctx, &session, &db).await
                                else {
                                    return error(StatusCode::UNAUTHORIZED, "not authenticated");
                                };
                                if body.action.starts_with("admin_") && user.role != Role::Admin {
                                    return error(
                                        StatusCode::FORBIDDEN,
                                        "administrator access required",
                                    );
                                }
                                let (config, _) = AppConfig::load_with_db(&db).await;
                                match action(pool, config, user.id, user.role == Role::Admin, body)
                                    .await
                                {
                                    Ok(value) => Json(value).into_response(),
                                    Err(err) => {
                                        let message = if err.downcast_ref::<sqlx::Error>().is_some()
                                        {
                                            "Podcast storage operation failed; try again".to_owned()
                                        } else {
                                            err.to_string()
                                        };
                                        error(StatusCode::BAD_REQUEST, &message)
                                    }
                                }
                            }
                        },
                    )
                },
                "podcast_action",
            ),
            Route::with_handler_and_name(
                "/media/{id}",
                {
                    get(
                        move |auth_ctx: auth::AuthContext,
                              session: Session,
                              db: Database,
                              path: Path<EpisodePath>,
                              request: Request<Body>| {
                            let pool = pool.clone();
                            async move {
                                let Some(user) =
                                    auth::get_request_user(&auth_ctx, &session, &db).await
                                else {
                                    return error(StatusCode::UNAUTHORIZED, "not authenticated");
                                };
                                let (config, _) = AppConfig::load_with_db(&db).await;
                                let Ok(id) = episode_path_id(&path.0.id) else {
                                    return error(StatusCode::NOT_FOUND, "episode unavailable");
                                };
                                match media(&pool, &config, user.id, id.as_str(), &request).await {
                                    Ok(response) => Ok(response),
                                    Err(_) => error(StatusCode::NOT_FOUND, "episode unavailable"),
                                }
                            }
                        },
                    )
                },
                "podcast_media",
            ),
        ])
    }
}
