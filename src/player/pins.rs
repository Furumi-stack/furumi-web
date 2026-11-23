use super::*;
use sqlx::PgPool;

#[derive(Deserialize)]
struct PinChange {
    href: String,
    pinned: bool,
    title: String,
    cover_url: Option<String>,
}

enum Target {
    Artist(i64),
    Release(i64),
    Playlist(i64),
    Peer,
}

impl Target {
    fn parse(href: &str) -> Option<Self> {
        if href.len() > 2048 || href.chars().any(char::is_control) {
            return None;
        }
        if let Some(value) = href
            .strip_prefix("#artistfed?name=")
            .or_else(|| href.strip_prefix("#releasefed?key="))
        {
            return (!value.is_empty()).then_some(Self::Peer);
        }
        let (kind, value) = href.split_once('/')?;
        let id = value.parse::<i64>().ok()?;
        if id <= 0 || value != id.to_string() {
            return None;
        }
        match kind {
            "#artist" => Some(Self::Artist(id)),
            "#release" => Some(Self::Release(id)),
            "#playlist" => Some(Self::Playlist(id)),
            _ => None,
        }
    }
}

async fn list(pool: &PgPool, user: i64) -> Result<Vec<serde_json::Value>, sqlx::Error> {
    sqlx::query_scalar(&format!(
        r#"SELECT jsonb_build_object(
                 'href', pin.href, 'kind', CASE WHEN pin.href LIKE '#artist%' THEN 'artist'
                                                WHEN pin.href LIKE '#release%' THEN 'release' ELSE 'playlist' END,
                 'id', COALESCE(a.id, r.id, p.id), 'title', COALESCE(a.name, r.title, p.title, pin.peer_title),
                 'cover_url', COALESCE('/api/player/cover/' || COALESCE(a.image_file_id, r.cover_file_id, p.cover_file_id, ({})) || '/small', pin.peer_cover))
             FROM furumusic__sidebar_pin pin
             LEFT JOIN furumusic__artist a ON a.id = pin.artist_id AND NOT a.is_hidden
             LEFT JOIN furumusic__release r ON r.id = pin.release_id AND NOT r.is_hidden
             LEFT JOIN furumusic__playlist p ON p.id = pin.playlist_id AND (
                 p.owner_id = pin.user_id OR p.is_public OR EXISTS (
                     SELECT 1 FROM furumusic__saved_playlist s WHERE s.user_id = pin.user_id AND s.playlist_id = p.id))
             WHERE pin.user_id = $1 AND (COALESCE(a.id, r.id, p.id) IS NOT NULL OR pin.peer_title IS NOT NULL)
             ORDER BY pin.created_at, pin.href"#, include_str!("playlist_cover.sql")))
        .bind(user).fetch_all(pool).await
}

async fn update_pin(
    pool: &PgPool,
    user: i64,
    target: Target,
    change: &PinChange,
) -> Result<bool, sqlx::Error> {
    if !change.pinned {
        sqlx::query("DELETE FROM furumusic__sidebar_pin WHERE user_id = $1 AND href = $2")
            .bind(user)
            .bind(&change.href)
            .execute(pool)
            .await?;
        return Ok(true);
    }
    let (artist, release, playlist, title) = match target {
        Target::Artist(id) => (Some(id), None, None, None),
        Target::Release(id) => (None, Some(id), None, None),
        Target::Playlist(id) => (None, None, Some(id), None),
        Target::Peer => (None, None, None, Some(change.title.trim())),
    };
    let cover = change.cover_url.as_deref().filter(|url| {
        title.is_some()
            && url.len() <= 4096
            && url.starts_with("/api/player/federation/catalog/artwork/")
    });
    let result = sqlx::query(r#"INSERT INTO furumusic__sidebar_pin (user_id, href, artist_id, release_id, playlist_id, peer_title, peer_cover)
                SELECT $1, $2, $3, $4, $5, $6, $7 WHERE $6::text IS NOT NULL
                OR EXISTS (SELECT 1 FROM furumusic__artist WHERE id = $3 AND NOT is_hidden)
                OR EXISTS (SELECT 1 FROM furumusic__release WHERE id = $4 AND NOT is_hidden)
                OR EXISTS (SELECT 1 FROM furumusic__playlist p WHERE p.id = $5 AND (p.owner_id = $1 OR p.is_public OR
                    EXISTS (SELECT 1 FROM furumusic__saved_playlist s WHERE s.user_id = $1 AND s.playlist_id = p.id)))
                ON CONFLICT (user_id, href) DO UPDATE SET peer_title = EXCLUDED.peer_title, peer_cover = EXCLUDED.peer_cover"#)
        .bind(user).bind(&change.href).bind(artist).bind(release).bind(playlist).bind(title).bind(cover)
        .execute(pool).await?;
    Ok(result.rows_affected() > 0)
}

async fn handle(
    auth_ctx: auth::AuthContext,
    session: Session,
    db: Database,
    pool: PgPool,
    change: Option<PinChange>,
) -> cot::Result<cot::response::Response> {
    let Some(user) = auth::get_request_user(&auth_ctx, &session, &db).await else {
        return Ok(json_error(StatusCode::UNAUTHORIZED, "not authenticated"));
    };
    if let Some(change) = change {
        let Some(target) = Target::parse(&change.href) else {
            return Ok(json_error(
                StatusCode::BAD_REQUEST,
                "invalid sidebar target",
            ));
        };
        if change.pinned
            && matches!(target, Target::Peer)
            && (change.title.trim().is_empty() || change.title.len() > 2048)
        {
            return Ok(json_error(StatusCode::BAD_REQUEST, "invalid title"));
        }
        if !update_pin(&pool, user.id, target, &change)
            .await
            .map_err(|e| cot::Error::internal(e.to_string()))?
        {
            return Ok(json_error(
                StatusCode::NOT_FOUND,
                "sidebar target not found",
            ));
        }
    }
    let items = list(&pool, user.id)
        .await
        .map_err(|err| cot::Error::internal(err.to_string()))?;
    let mut response = Json(items).into_response()?;
    response
        .headers_mut()
        .insert("Cache-Control", "private, no-store".parse().unwrap());
    Ok(response)
}

#[cfg(test)]
#[path = "pins_tests.rs"]
mod tests;

pub(super) fn route(pool: Arc<tokio::sync::OnceCell<PgPool>>, config: Arc<AppConfig>) -> Route {
    let connect = move || {
        let pool = pool.clone();
        let config = config.clone();
        async move {
            pool.get_or_try_init(|| async {
                sqlx::postgres::PgPoolOptions::new()
                    .max_connections(5)
                    .connect(&config.database_url)
                    .await
            })
            .await
            .cloned()
            .map_err(|err| cot::Error::internal(err.to_string()))
        }
    };
    let read = connect.clone();
    Route::with_handler_and_name(
        "/pins",
        get(move |auth_ctx, session, db| {
            let pool = read();
            async move { handle(auth_ctx, session, db, pool.await?, None).await }
        })
        .post(move |auth_ctx, session, db, Json(body): Json<PinChange>| {
            let pool = connect();
            async move { handle(auth_ctx, session, db, pool.await?, Some(body)).await }
        }),
        "player_sidebar_pins",
    )
}
