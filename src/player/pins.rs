use super::*;
use sqlx::{PgConnection, PgPool};

#[derive(Deserialize)]
struct PinChange {
    href: String,
    pinned: bool,
    title: String,
    cover_url: Option<String>,
    before_href: Option<String>,
    after_href: Option<String>,
}

#[derive(Deserialize)]
struct PinMove {
    href: String,
    before_href: Option<String>,
}

enum Mutation {
    Set(PinChange),
    Move(PinMove),
}

enum MissingAnchor<'a> {
    Reject,
    RestoreAfter(Option<&'a str>),
}

#[derive(Debug, PartialEq)]
enum Outcome {
    Applied,
    Invalid,
    Missing,
    Stale,
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

const PIN_TARGETS: &str = "FROM furumusic__sidebar_pin pin
             LEFT JOIN furumusic__artist a ON a.id = pin.artist_id AND NOT a.is_hidden
             LEFT JOIN furumusic__release r ON r.id = pin.release_id AND NOT r.is_hidden
             LEFT JOIN furumusic__playlist p ON p.id = pin.playlist_id AND (
                 p.owner_id = pin.user_id OR p.is_public OR EXISTS (
                     SELECT 1 FROM furumusic__saved_playlist s WHERE s.user_id = pin.user_id AND s.playlist_id = p.id))";
const VISIBLE: &str = "(COALESCE(a.id, r.id, p.id) IS NOT NULL OR pin.peer_title IS NOT NULL)";
const PIN_ORDER: &str = "pin.position NULLS LAST, pin.created_at, pin.href";

async fn list(pool: &PgPool, user: i64) -> Result<Vec<serde_json::Value>, sqlx::Error> {
    sqlx::query_scalar(&format!(
        r#"SELECT jsonb_build_object(
                 'href', pin.href, 'kind', CASE WHEN pin.href LIKE '#artist%' THEN 'artist'
                                                WHEN pin.href LIKE '#release%' THEN 'release' ELSE 'playlist' END,
                 'id', COALESCE(a.id, r.id, p.id), 'title', COALESCE(a.name, r.title, p.title, pin.peer_title),
                 'cover_url', COALESCE('/api/player/cover/' || COALESCE(a.image_file_id, r.cover_file_id) || '/small', art.urls[1], pin.peer_cover),
                 'cover_urls', COALESCE(art.urls, ARRAY[]::text[]))
              {PIN_TARGETS} LEFT JOIN LATERAL ({}) art ON p.id IS NOT NULL
              WHERE pin.user_id = $1 AND {VISIBLE} ORDER BY {PIN_ORDER}"#, super::playlist_artwork::cover_sql()))
        .bind(user).fetch_all(pool).await
}

async fn set_pin(
    conn: &mut PgConnection,
    user: i64,
    target: Target,
    change: &PinChange,
) -> Result<Outcome, sqlx::Error> {
    if !change.pinned {
        sqlx::query("DELETE FROM furumusic__sidebar_pin WHERE user_id = $1 AND href = $2")
            .bind(user)
            .bind(&change.href)
            .execute(conn)
            .await?;
        return Ok(Outcome::Applied);
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
    let inserted: Option<bool> = sqlx::query_scalar(r#"WITH written AS (
                INSERT INTO furumusic__sidebar_pin (user_id, href, artist_id, release_id, playlist_id, peer_title, peer_cover, created_at)
                SELECT $1, $2, $3, $4, $5, $6, $7, clock_timestamp() WHERE $6::text IS NOT NULL
                OR EXISTS (SELECT 1 FROM furumusic__artist WHERE id = $3 AND NOT is_hidden)
                OR EXISTS (SELECT 1 FROM furumusic__release WHERE id = $4 AND NOT is_hidden)
                OR EXISTS (SELECT 1 FROM furumusic__playlist p WHERE p.id = $5 AND (p.owner_id = $1 OR p.is_public OR
                    EXISTS (SELECT 1 FROM furumusic__saved_playlist s WHERE s.user_id = $1 AND s.playlist_id = p.id)))
                ON CONFLICT (user_id, href) DO UPDATE SET peer_title = EXCLUDED.peer_title, peer_cover = EXCLUDED.peer_cover
                RETURNING href)
                SELECT NOT EXISTS (SELECT 1 FROM furumusic__sidebar_pin WHERE user_id = $1 AND href = $2) FROM written"#)
        .bind(user).bind(&change.href).bind(artist).bind(release).bind(playlist).bind(title).bind(cover)
        .fetch_optional(&mut *conn).await?;
    match inserted {
        None => Ok(Outcome::Missing),
        Some(true) if change.before_href.is_some() || change.after_href.is_some() => {
            place_pin(
                conn,
                user,
                &change.href,
                change.before_href.as_deref(),
                MissingAnchor::RestoreAfter(change.after_href.as_deref()),
            )
            .await
        }
        Some(_) => Ok(Outcome::Applied),
    }
}

// A move requires its anchor to still exist. Undo may fall back to the old
// predecessor when the next pin was removed in another tab.
async fn place_pin(
    conn: &mut PgConnection,
    user: i64,
    href: &str,
    before: Option<&str>,
    missing: MissingAnchor<'_>,
) -> Result<Outcome, sqlx::Error> {
    let mut pins: Vec<(String, bool)> = sqlx::query_as(&format!(
        "SELECT pin.href, {VISIBLE} {PIN_TARGETS} WHERE pin.user_id = $1 ORDER BY {PIN_ORDER}"
    ))
    .bind(user)
    .fetch_all(&mut *conn)
    .await?;
    let Some(source) = pins
        .iter()
        .position(|(key, visible)| *visible && key == href)
    else {
        return Ok(Outcome::Stale);
    };
    if before == Some(href) {
        return Ok(Outcome::Applied);
    }
    let moved = pins.remove(source);
    let visible_index = |href: &str| {
        pins.iter()
            .position(|(key, visible)| *visible && key == href)
    };
    let at = if let Some(i) = before.and_then(visible_index) {
        i
    } else if let MissingAnchor::RestoreAfter(previous) = missing {
        previous
            .and_then(visible_index)
            .map_or(pins.len(), |i| i + 1)
    } else if before.is_none() {
        pins.len()
    } else {
        return Ok(Outcome::Stale);
    };
    if at != source {
        pins.insert(at, moved);
        let order: Vec<_> = pins.into_iter().map(|(key, _)| key).collect();
        sqlx::query("UPDATE furumusic__sidebar_pin p SET position = u.pos
                       FROM unnest($2::text[]) WITH ORDINALITY AS u(href, pos)
                      WHERE p.user_id = $1 AND p.href = u.href AND p.position IS DISTINCT FROM u.pos")
            .bind(user).bind(order).execute(conn).await?;
    }
    Ok(Outcome::Applied)
}

async fn mutate(pool: &PgPool, user: i64, action: Mutation) -> Result<Outcome, sqlx::Error> {
    let mut tx = pool.begin().await?;
    // Serialize all pin writers for this account, including when it has no pins.
    sqlx::query("SELECT id FROM furumusic__user WHERE id = $1 FOR NO KEY UPDATE")
        .bind(user)
        .fetch_one(&mut *tx)
        .await?;
    let outcome = match action {
        Mutation::Set(change) => {
            let Some(target) = Target::parse(&change.href) else {
                return Ok(Outcome::Invalid);
            };
            if change.pinned
                && matches!(target, Target::Peer)
                && (change.title.trim().is_empty() || change.title.len() > 2048)
            {
                return Ok(Outcome::Invalid);
            }
            set_pin(&mut tx, user, target, &change).await?
        }
        Mutation::Move(change) => {
            place_pin(
                &mut tx,
                user,
                &change.href,
                change.before_href.as_deref(),
                MissingAnchor::Reject,
            )
            .await?
        }
    };
    if outcome == Outcome::Applied {
        tx.commit().await?;
    }
    Ok(outcome)
}

async fn handle(
    auth_ctx: auth::AuthContext,
    session: Session,
    db: Database,
    pool: PgPool,
    change: Option<Mutation>,
) -> cot::Result<cot::response::Response> {
    let Some(user) = auth::get_request_user(&auth_ctx, &session, &db).await else {
        return Ok(json_error(StatusCode::UNAUTHORIZED, "not authenticated"));
    };
    if let Some(action) = change {
        match mutate(&pool, user.id, action)
            .await
            .map_err(|e| cot::Error::internal(e.to_string()))?
        {
            Outcome::Applied => {}
            Outcome::Invalid => {
                return Ok(json_error(
                    StatusCode::BAD_REQUEST,
                    "invalid sidebar target",
                ));
            }
            Outcome::Missing => {
                return Ok(json_error(
                    StatusCode::NOT_FOUND,
                    "sidebar target not found",
                ));
            }
            Outcome::Stale => return Ok(json_error(StatusCode::CONFLICT, "sidebar pins changed")),
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
    let write = connect.clone();
    Route::with_handler_and_name(
        "/pins",
        get(move |auth_ctx, session, db| {
            let pool = read();
            async move { handle(auth_ctx, session, db, pool.await?, None).await }
        })
        .post(move |auth_ctx, session, db, Json(body): Json<PinChange>| {
            let pool = write();
            async move {
                handle(
                    auth_ctx,
                    session,
                    db,
                    pool.await?,
                    Some(Mutation::Set(body)),
                )
                .await
            }
        })
        .put(move |auth_ctx, session, db, Json(body): Json<PinMove>| {
            let pool = connect();
            async move {
                handle(
                    auth_ctx,
                    session,
                    db,
                    pool.await?,
                    Some(Mutation::Move(body)),
                )
                .await
            }
        }),
        "player_sidebar_pins",
    )
}
