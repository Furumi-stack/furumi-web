use super::*;
use cot::request::Request;
use schemars::JsonSchema;
use sqlx::{PgConnection, PgPool};

const CANDIDATES: &str = include_str!("playlist_cover_candidates.sql");

pub(super) fn cover_sql() -> String {
    format!(include_str!("playlist_cover.sql"), candidates = CANDIDATES)
}

pub(super) fn db_error(error: sqlx::Error) -> cot::Error {
    cot::Error::internal(error.to_string())
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
struct CoverOption {
    cover_id: i64,
    title: String,
    cover_url: String,
    #[serde(skip_serializing)]
    automatic: bool,
}

#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct Artwork {
    cover_mode: &'static str,
    cover_ids: Vec<i64>,
    cover_url: Option<String>,
    cover_urls: Vec<String>,
    auto_cover_urls: Vec<String>,
    cover_options: Vec<CoverOption>,
}

impl Artwork {
    pub(super) fn automatic(urls: impl Iterator<Item = String>) -> Self {
        let mut seen = HashSet::new();
        let cover_urls: Vec<_> = urls
            .filter(|url| seen.insert(url.clone()))
            .take(4)
            .collect();
        Self {
            cover_mode: "auto",
            cover_ids: vec![],
            cover_url: cover_urls.first().cloned(),
            auto_cover_urls: cover_urls.clone(),
            cover_urls,
            cover_options: vec![],
        }
    }
}

pub(super) async fn load(
    conn: &mut PgConnection,
    id: i64,
    editor: bool,
) -> Result<Artwork, sqlx::Error> {
    #[derive(sqlx::FromRow)]
    struct Stored {
        cover_hash: Option<String>,
        cover_file_id: Option<i64>,
        cover_ids: Vec<i64>,
        options: sqlx::types::Json<Vec<CoverOption>>,
    }
    let stored = sqlx::query_as::<_, Stored>(&format!(
        "SELECT p.cover_hash, p.cover_file_id, p.cover_ids, (
             SELECT COALESCE(jsonb_agg(jsonb_build_object(
                 'cover_id', c.art, 'title', c.title, 'automatic', c.automatic,
                 'cover_url', '/api/player/cover/' || c.art || '/small'
             ) ORDER BY c.position, c.item_id, c.automatic DESC, c.art), '[]')
             FROM ({CANDIDATES}) c
         ) AS options FROM furumusic__playlist p WHERE p.id=$1"
    ))
    .bind(id)
    .fetch_one(conn)
    .await?;
    let options = stored.options.0;
    let selected: Vec<_> = stored
        .cover_ids
        .iter()
        .filter_map(|id| options.iter().find(|option| option.cover_id == *id))
        .collect();
    let ids = selected.iter().map(|o| o.cover_id).collect();
    let auto_cover_urls: Vec<_> = options
        .iter()
        .filter(|o| o.automatic)
        .take(4)
        .map(|o| o.cover_url.clone())
        .collect();
    let upload = stored
        .cover_hash
        .map(|hash| format!("/api/player/playlists/{id}/cover?v={hash}"))
        .or_else(|| cover_variant_url(stored.cover_file_id, "small"));
    let (cover_mode, urls) = if let Some(url) = upload {
        ("upload", vec![url])
    } else if selected.is_empty() {
        ("auto", auto_cover_urls.clone())
    } else {
        (
            "releases",
            selected.iter().map(|o| o.cover_url.clone()).collect(),
        )
    };
    Ok(Artwork {
        cover_mode,
        cover_ids: ids,
        cover_url: urls.first().cloned(),
        auto_cover_urls,
        cover_urls: urls,
        cover_options: if editor { options } else { Vec::new() },
    })
}

enum Change {
    Keep,
    Auto,
    Releases(Vec<i64>),
    Upload(Vec<u8>, String),
    KeepUpload,
}

async fn cover_change(body: &mut UpdatePlaylistRequest) -> Result<Change, String> {
    match (
        body.cover_mode.as_deref(),
        body.cover_ids.take(),
        body.cover_image.take(),
    ) {
        (None, None, None) => Ok(Change::Keep),
        (Some("auto"), None, None) => Ok(Change::Auto),
        (Some("releases"), Some(ids), None)
            if (1..=4).contains(&ids.len())
                && ids.iter().all(|id| *id > 0)
                && ids.iter().collect::<HashSet<_>>().len() == ids.len() =>
        {
            Ok(Change::Releases(ids))
        }
        (Some("upload"), None, Some(encoded)) => {
            let (jpeg, hash) = super::playlist_image::decode(encoded).await?;
            Ok(Change::Upload(jpeg, hash))
        }
        (Some("upload"), None, None) => Ok(Change::KeepUpload),
        _ => Err("Invalid playlist artwork selection".into()),
    }
}

pub(super) async fn update(
    auth_ctx: auth::AuthContext,
    session: Session,
    db: Database,
    pool: &PgPool,
    path: Path<PathId>,
    request: Request,
) -> cot::Result<cot::response::Response> {
    let Some(user) = auth::get_request_user(&auth_ctx, &session, &db).await else {
        return Ok(json_error(StatusCode::UNAUTHORIZED, "not authenticated"));
    };
    let id = path.0.id;
    let owner: Option<i64> =
        sqlx::query_scalar("SELECT owner_id FROM furumusic__playlist WHERE id=$1")
            .bind(id)
            .fetch_optional(pool)
            .await
            .map_err(db_error)?;
    match owner {
        None => return Ok(json_error(StatusCode::NOT_FOUND, "playlist not found")),
        Some(owner) if owner != user.id => {
            return Ok(json_error(StatusCode::FORBIDDEN, "not your playlist"));
        }
        _ => {}
    }
    if !request
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.split(';').next())
        .is_some_and(|s| s.trim().eq_ignore_ascii_case("application/json"))
    {
        return Ok(json_error(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "expected application/json",
        ));
    }
    let Ok(bytes) = request
        .into_body()
        .into_bytes_limited(6 * 1024 * 1024)
        .await
    else {
        return Ok(json_error(
            StatusCode::PAYLOAD_TOO_LARGE,
            "playlist update exceeds 6 MiB",
        ));
    };
    let Ok(mut body) = serde_json::from_slice::<UpdatePlaylistRequest>(&bytes) else {
        return Ok(json_error(
            StatusCode::BAD_REQUEST,
            "invalid playlist update",
        ));
    };
    let change = match cover_change(&mut body).await {
        Ok(change) => change,
        Err(msg) => return Ok(json_error(StatusCode::BAD_REQUEST, &msg)),
    };
    save(pool, user.id, id, body, change).await
}

async fn save(
    pool: &PgPool,
    owner: i64,
    id: i64,
    body: UpdatePlaylistRequest,
    change: Change,
) -> cot::Result<cot::response::Response> {
    let mut tx = pool.begin().await.map_err(db_error)?;
    let current: Option<(String, bool)> = sqlx::query_as(
        "SELECT title::text, cover_hash IS NOT NULL OR cover_file_id IS NOT NULL
           FROM furumusic__playlist WHERE id=$1 AND owner_id=$2 FOR UPDATE",
    )
    .bind(id)
    .bind(owner)
    .fetch_optional(&mut *tx)
    .await
    .map_err(db_error)?;
    let Some((old_title, uploaded)) = current else {
        return Ok(json_error(StatusCode::NOT_FOUND, "playlist not found"));
    };
    let title = body.title.as_deref().map(str::trim).unwrap_or(&old_title);
    if title.is_empty() || title.chars().count() > 255 {
        return Ok(json_error(
            StatusCode::BAD_REQUEST,
            "playlist name must be 1–255 characters",
        ));
    }
    if matches!(change, Change::KeepUpload) && !uploaded {
        return Ok(json_error(StatusCode::BAD_REQUEST, "no uploaded cover"));
    }
    if let Change::Releases(ids) = &change {
        let valid: i64 = sqlx::query_scalar(&format!(
            "SELECT COUNT(*) FROM furumusic__playlist p CROSS JOIN LATERAL ({CANDIDATES}) c
             WHERE p.id=$1 AND c.art=ANY($2)"
        ))
        .bind(id)
        .bind(ids)
        .fetch_one(&mut *tx)
        .await
        .map_err(db_error)?;
        if valid != ids.len() as i64 {
            return Ok(json_error(
                StatusCode::BAD_REQUEST,
                "choose artwork from this playlist",
            ));
        }
    }
    let keep = matches!(change, Change::Keep | Change::KeepUpload);
    let (ids, jpeg, hash) = match change {
        Change::Releases(selection) => (selection, None, None),
        Change::Upload(bytes, digest) => (Vec::new(), Some(bytes), Some(digest)),
        _ => (Vec::new(), None, None),
    };
    let now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
    sqlx::query(
        "UPDATE furumusic__playlist SET title=$2, description=COALESCE($3,description), updated_at=$4,
             cover_file_id=CASE WHEN $5 THEN cover_file_id ELSE NULL END,
             cover_ids=CASE WHEN $5 THEN cover_ids ELSE $6 END,
             cover_hash=CASE WHEN $5 THEN cover_hash ELSE $7 END,
             cover_jpeg=CASE WHEN $5 THEN cover_jpeg ELSE $8 END WHERE id=$1"
    )
    .bind(id)
    .bind(title)
    .bind(body.description)
    .bind(now)
    .bind(keep)
    .bind(ids)
    .bind(hash)
    .bind(jpeg)
    .execute(&mut *tx)
    .await
    .map_err(db_error)?;
    let artwork = load(&mut tx, id, true).await.map_err(db_error)?;
    tx.commit().await.map_err(db_error)?;
    if title != old_title {
        record_fed_playlist_renamed(pool, owner, id, title).await;
    }
    let mut response = serde_json::json!(artwork);
    response["ok"] = true.into();
    response["title"] = title.into();
    Json(response).into_response()
}

#[derive(Deserialize)]
struct CoverVersion {
    v: String,
}

async fn image_for_listener(
    pool: &PgPool,
    id: i64,
    user: i64,
    hash: &str,
) -> Result<Option<Vec<u8>>, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT cover_jpeg FROM furumusic__playlist p WHERE p.id=$1 AND p.cover_hash=$3 AND
             (p.owner_id=$2 OR p.is_public OR EXISTS(
                 SELECT 1 FROM furumusic__saved_playlist s WHERE s.playlist_id=p.id AND s.user_id=$2))"
    ).bind(id).bind(user).bind(hash).fetch_optional(pool).await
}

pub(super) fn route(pool: Arc<tokio::sync::OnceCell<PgPool>>, config: Arc<AppConfig>) -> Route {
    Route::with_handler_and_name(
        "/playlists/{id}/cover",
        get(
            move |auth_ctx: auth::AuthContext,
                  session: Session,
                  db: Database,
                  path: Path<PathId>,
                  query: UrlQuery<CoverVersion>| {
                let pool = pool.clone();
                let config = config.clone();
                async move {
                    let Some(user) = auth::get_request_user(&auth_ctx, &session, &db).await else {
                        return Ok(json_error(StatusCode::UNAUTHORIZED, "not authenticated"));
                    };
                    let pool = pool
                        .get_or_try_init(|| async {
                            sqlx::postgres::PgPoolOptions::new()
                                .max_connections(5)
                                .connect(&config.database_url)
                                .await
                        })
                        .await
                        .map_err(db_error)?;
                    let bytes = image_for_listener(pool, path.0.id, user.id, &query.0.v)
                        .await
                        .map_err(db_error)?;
                    let Some(bytes) = bytes else {
                        return Ok(json_error(StatusCode::NOT_FOUND, "cover not found"));
                    };
                    Ok::<_, cot::Error>(
                        cot::http::Response::builder()
                            .header(CONTENT_TYPE, "image/jpeg")
                            .header("Cache-Control", "private, no-store")
                            .header("X-Content-Type-Options", "nosniff")
                            .body(Body::fixed(bytes))
                            .expect("valid cover response"),
                    )
                }
            },
        ),
        "player_playlist_cover",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(value: serde_json::Value) -> UpdatePlaylistRequest {
        serde_json::from_value(value).unwrap()
    }

    #[tokio::test]
    async fn rejects_malformed_selections() {
        for input in [
            serde_json::json!({"cover_mode":"releases","cover_ids":[]}),
            serde_json::json!({"cover_mode":"releases","cover_ids":[1,2,3,4,5]}),
            serde_json::json!({"cover_mode":"releases","cover_ids":[1,1]}),
            serde_json::json!({"cover_mode":"auto","cover_image":"ignored"}),
            serde_json::json!({"cover_ids":[5]}),
        ] {
            assert!(cover_change(&mut request(input)).await.is_err());
        }
    }

    #[tokio::test]
    #[ignore = "requires disposable PostgreSQL via FURU_STATISTICS_TEST_DATABASE_URL"]
    async fn playlist_artwork_tracks_membership_and_ownership() -> anyhow::Result<()> {
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .connect(&std::env::var("FURU_STATISTICS_TEST_DATABASE_URL")?)
            .await?;
        sqlx::raw_sql("
            CREATE TEMP TABLE furumusic__playlist(id bigint PRIMARY KEY,title text,description text,owner_id bigint,is_public boolean,updated_at text,cover_file_id bigint);
            CREATE TEMP TABLE furumusic__release(id bigint PRIMARY KEY,title text,is_hidden boolean DEFAULT false,cover_file_id bigint);
            CREATE TEMP TABLE furumusic__track(id bigint PRIMARY KEY,title text,release_id bigint,is_hidden boolean DEFAULT false,cover_file_id bigint);
            CREATE TEMP TABLE furumusic__playlist_track(id bigint,playlist_id bigint,track_id bigint,position int);
            CREATE TEMP TABLE furumusic__saved_playlist(user_id bigint,playlist_id bigint);
            INSERT INTO furumusic__playlist VALUES (1,'List',NULL,10,false,'',NULL),(2,'Old cover',NULL,10,false,'',60);
            INSERT INTO furumusic__release(id,title,cover_file_id) VALUES (1,'A',11),(2,'B',22),(3,'C',22),(4,'D',44);
            INSERT INTO furumusic__track(id,title,release_id,cover_file_id) VALUES (1,'One',1,51),(2,'Two',1,52),(3,'Three',2,NULL),(4,'Four',3,NULL),(5,'Five',4,NULL);
            INSERT INTO furumusic__playlist_track VALUES (1,1,1,1),(2,1,2,2),(3,1,3,3),(4,1,4,4),(5,2,5,1);
        ").execute(&pool).await?;
        sqlx::raw_sql(include_str!("playlist_artwork_schema.sql"))
            .execute(&pool)
            .await?;
        let read = async |id| {
            let mut conn = pool.acquire().await?;
            load(&mut conn, id, true).await
        };
        let auto = read(1).await?;
        assert_eq!(
            auto.cover_urls,
            ["/api/player/cover/11/small", "/api/player/cover/22/small"]
        );
        assert_eq!(
            auto.cover_options
                .iter()
                .map(|o| o.cover_id)
                .collect::<Vec<_>>(),
            [11, 51, 52, 22]
        );
        assert_eq!(read(2).await?.cover_mode, "upload");
        let rename = || request(serde_json::json!({"title":"Invalid rename"}));
        assert_eq!(
            save(&pool, 9, 1, rename(), Change::Auto).await?.status(),
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            save(&pool, 10, 1, rename(), Change::Releases(vec![44]))
                .await?
                .status(),
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            sqlx::query_scalar::<_, String>("SELECT title FROM furumusic__playlist WHERE id=1")
                .fetch_one(&pool)
                .await?,
            "List"
        );
        let unchanged = || request(serde_json::json!({}));
        assert_eq!(
            save(&pool, 10, 1, unchanged(), Change::Releases(vec![52, 51]))
                .await?
                .status(),
            StatusCode::OK
        );
        let chosen = read(1).await?;
        assert_eq!(chosen.cover_mode, "releases");
        assert_eq!(
            chosen.cover_urls,
            ["/api/player/cover/52/small", "/api/player/cover/51/small"]
        );
        assert_eq!(chosen.auto_cover_urls, auto.cover_urls);
        sqlx::query("DELETE FROM furumusic__playlist_track WHERE track_id=2")
            .execute(&pool)
            .await?;
        assert_eq!(read(1).await?.cover_ids, [51]);
        let bytes = vec![1, 2, 3];
        assert_eq!(
            save(
                &pool,
                10,
                1,
                unchanged(),
                Change::Upload(bytes.clone(), "hash".into())
            )
            .await?
            .status(),
            StatusCode::OK
        );
        assert_eq!(
            read(1).await?.cover_urls,
            ["/api/player/playlists/1/cover?v=hash"]
        );
        assert_eq!(
            image_for_listener(&pool, 1, 10, "hash").await?,
            Some(bytes.clone())
        );
        assert!(image_for_listener(&pool, 1, 9, "hash").await?.is_none());
        assert!(
            image_for_listener(&pool, 1, 10, "old-hash")
                .await?
                .is_none()
        );
        sqlx::query("UPDATE furumusic__playlist SET is_public=true WHERE id=1")
            .execute(&pool)
            .await?;
        assert_eq!(image_for_listener(&pool, 1, 9, "hash").await?, Some(bytes));
        assert_eq!(
            save(&pool, 10, 1, unchanged(), Change::Auto)
                .await?
                .status(),
            StatusCode::OK
        );
        assert_eq!(read(1).await?.cover_urls, auto.cover_urls);
        assert!(image_for_listener(&pool, 1, 10, "hash").await?.is_none());
        Ok(())
    }
}
