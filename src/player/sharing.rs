//! Public capabilities grant access only to a stored snapshot's visible tracks.
use super::*;

#[derive(Deserialize)]
pub(super) struct MediaPath {
    token: String,
    action: String,
    track_id: i64,
}

async fn load_share(pool: &sqlx::PgPool, token: &str) -> cot::Result<Option<ShareDetail>> {
    if !crate::is_share_token(token) {
        return Ok(None);
    }
    let row: Option<(String, String)> =
        sqlx::query_as("SELECT title, track_ids_json FROM furumusic__share_link WHERE token = $1")
            .bind(token)
            .fetch_optional(pool)
            .await
            .map_err(|e| cot::Error::internal(e.to_string()))?;
    let Some((title, json)) = row else {
        return Ok(None);
    };
    let ids: Vec<i64> =
        serde_json::from_str(&json).map_err(|e| cot::Error::internal(e.to_string()))?;
    let tracks = load_track_items_by_ids(pool, &ids).await?;
    Ok(Some(ShareDetail {
        token: token.to_owned(),
        title,
        tracks,
    }))
}

pub(super) async fn detail_handler(
    auth_ctx: auth::AuthContext,
    session: Session,
    db: Database,
    pool: &sqlx::PgPool,
    path: Path<PathStringId>,
) -> cot::Result<cot::response::Response> {
    let Some(mut share) = load_share(pool, &path.0.id).await? else {
        return Ok(json_error(StatusCode::NOT_FOUND, "shared music not found"));
    };
    let guest = auth::get_request_user(&auth_ctx, &session, &db)
        .await
        .is_none();
    for track in &mut share.tracks {
        track.uploader_name.clear();
        if guest {
            track.stream_url = format!("/api/player/shares/{}/stream/{}", share.token, track.id);
            if track.cover_url.is_some() {
                track.cover_url = Some(format!(
                    "/api/player/shares/{}/cover/{}",
                    share.token, track.id
                ));
            }
        }
    }
    let mut response = Json(share).into_response()?;
    response
        .headers_mut()
        .insert("Cache-Control", "private, no-store".parse().unwrap());
    Ok(response)
}

pub(super) async fn media_handler(
    pool: &sqlx::PgPool,
    config: &AppConfig,
    request: &cot::http::Request<Body>,
    path: Path<MediaPath>,
) -> cot::Result<cot::response::Response> {
    let p = path.0;
    if !crate::is_share_token(&p.token) {
        return Ok(json_error(StatusCode::NOT_FOUND, "shared music not found"));
    }
    // Media range requests need only a membership check, not the metadata and
    // artist lists of every track in a potentially large shared queue.
    let allowed: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM furumusic__share_link s
         JOIN furumusic__track t ON t.id = $2
         JOIN furumusic__release r ON r.id = t.release_id
         WHERE s.token = $1 AND s.track_ids_json::jsonb @> jsonb_build_array($2::bigint)
           AND t.is_hidden = false AND r.is_hidden = false)",
    )
    .bind(&p.token)
    .bind(p.track_id)
    .fetch_one(pool)
    .await
    .map_err(|e| cot::Error::internal(e.to_string()))?;
    if !allowed {
        return Ok(json_error(StatusCode::NOT_FOUND, "shared track not found"));
    }
    match p.action.as_str() {
        "lyrics" => {
            let value = crate::lyrics::for_track(pool, p.track_id, true)
                .await
                .map_err(|e| cot::Error::internal(e.to_string()))?;
            Json(value).into_response()
        }
        "stream" | "download" => {
            audio_response(pool, config, request, p.track_id, p.action == "download").await
        }
        "cover" => {
            let cover: Option<i64> = sqlx::query_scalar(
                "SELECT COALESCE(t.cover_file_id, r.cover_file_id) FROM furumusic__track t JOIN furumusic__release r ON r.id = t.release_id WHERE t.id = $1")
                .bind(p.track_id).fetch_one(pool).await.map_err(|e| cot::Error::internal(e.to_string()))?;
            match cover {
                Some(id) => cover_file_response(pool, config, id, Some("card")).await,
                None => Ok(json_error(StatusCode::NOT_FOUND, "cover not found")),
            }
        }
        _ => Ok(json_error(StatusCode::NOT_FOUND, "not found")),
    }
}

pub(super) async fn download_handler(
    auth_ctx: auth::AuthContext,
    session: Session,
    db: Database,
    pool: &sqlx::PgPool,
    config: &AppConfig,
    request: &cot::http::Request<Body>,
    path: Path<PathTrackId>,
) -> cot::Result<cot::response::Response> {
    if auth::get_request_user(&auth_ctx, &session, &db)
        .await
        .is_none()
    {
        return Ok(json_error(StatusCode::UNAUTHORIZED, "not authenticated"));
    }
    audio_response(pool, config, request, path.0.track_id, true).await
}
