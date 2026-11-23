use chrono::NaiveDate;
use cot::{db::Database, http::StatusCode, json::Json, response::IntoResponse, session::Session};
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::PgPool;

use super::json_error;
use crate::auth;

#[derive(Default, Deserialize)]
#[serde(rename_all = "lowercase")]
enum View {
    #[default]
    Overview,
    History,
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Ranking {
    #[default]
    Artists,
    Tracks,
    Releases,
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(super) struct Query {
    view: View,
    kind: Ranking,
    from: Option<String>,
    to: Option<String>,
    tz: Option<String>,
    page: Option<u32>,
}

impl Query {
    fn valid_dates(&self) -> bool {
        let valid = |s: &str| {
            s.len() == 10 && s >= "1970-01-01" && NaiveDate::parse_from_str(s, "%Y-%m-%d").is_ok()
        };
        self.from.as_deref().is_none_or(|s| s == "all" || valid(s))
            && self.to.as_deref().is_none_or(valid)
    }
}

pub(super) async fn handler(
    auth_ctx: auth::AuthContext,
    session: Session,
    db: Database,
    pool: &PgPool,
    query: Query,
) -> cot::Result<cot::response::Response> {
    let Some(user) = auth::get_request_user(&auth_ctx, &session, &db).await else {
        return Ok(json_error(StatusCode::UNAUTHORIZED, "not authenticated"));
    };
    if !query.valid_dates() {
        return Ok(json_error(
            StatusCode::BAD_REQUEST,
            "Use dates in YYYY-MM-DD format",
        ));
    }
    let tz = query.tz.as_deref().unwrap_or("UTC");
    if tz.is_empty() || tz.len() > 128 {
        return Ok(json_error(StatusCode::BAD_REQUEST, "Invalid time zone"));
    }
    let bounds = match bounds(pool, &query, tz).await {
        Ok((start, end)) if start < end => (start, end),
        Ok(_) => return Ok(json_error(StatusCode::BAD_REQUEST, "Reversed date range")),
        Err(sqlx::Error::Database(e)) if e.code().as_deref() == Some("22023") => {
            return Ok(json_error(StatusCode::BAD_REQUEST, "Unknown time zone"));
        }
        Err(e) => return Err(cot::Error::internal(e.to_string())),
    };
    let data = match query.view {
        View::Overview => overview(pool, user.id, bounds, tz, &query.kind).await,
        View::History => history(pool, user.id, bounds, query.page.unwrap_or(1).max(1)).await,
    }
    .map_err(|e| cot::Error::internal(e.to_string()))?;
    let mut response = Json(data).into_response()?;
    response
        .headers_mut()
        .insert("Cache-Control", "private, no-store".parse().unwrap());
    Ok(response)
}

// Calendar midnights are converted independently so DST days work on the
// chart and when drilling into a single day's history.
async fn bounds(pool: &PgPool, query: &Query, tz: &str) -> Result<(i64, i64), sqlx::Error> {
    sqlx::query_as(
        "WITH bounds AS (SELECT COALESCE($2::text::date, (now() AT TIME ZONE $3)::date) AS last_day)
         SELECT CASE WHEN $1::text = 'all' THEN 0 ELSE (EXTRACT(EPOCH FROM
             (COALESCE(NULLIF($1, 'all')::date, last_day - 29)::timestamp AT TIME ZONE $3)) * 1000)::bigint END,
             (EXTRACT(EPOCH FROM ((last_day + 1)::timestamp AT TIME ZONE $3)) * 1000)::bigint
         FROM bounds",
    )
    .bind(&query.from).bind(&query.to).bind(tz).fetch_one(pool).await
}

// Every projection starts from the authenticated listener's qualified events.
// NOT MATERIALIZED lets PostgreSQL use the per-user time index for a history
// page and count its rows without reading every event's metadata.
const EVENTS: &str = "
    WITH events AS NOT MATERIALIZED (
        SELECT * FROM furumusic__listen_event
         WHERE user_id = $1 AND qualified AND started_at_ms >= $2 AND started_at_ms < $3
    )";

const RECORDINGS: &str = "
    , recordings AS (
        SELECT DISTINCT ON (content_id) content_id, metadata_json, local_track_id,
               COUNT(*) OVER (PARTITION BY content_id) AS plays
          FROM events ORDER BY content_id, started_at_ms DESC, listen_id DESC
    )";

// Materialized tracks keep the same content ID. Resolve against today's audio
// hash, including tracks that arrived after the listen was synced.
const LOCAL_TRACKS: &str = "
    , local AS (
        SELECT DISTINCT ON (w.content_id) w.content_id, t.id AS track_id, t.title,
               r.id AS release_id, r.title AS release_title, r.cover_file_id AS album_art,
               COALESCE(t.cover_file_id, r.cover_file_id) AS track_art
          FROM wanted w JOIN furumusic__federation_content_id_cache c USING (content_id)
          JOIN furumusic__media_file m ON m.id = c.media_file_id AND m.sha256_hash = c.sha256_hash
          JOIN furumusic__track t ON t.audio_file_id = m.id AND NOT t.is_hidden
          JOIN furumusic__release r ON r.id = t.release_id AND NOT r.is_hidden
         ORDER BY w.content_id, (t.id = w.local_track_id) DESC NULLS LAST, t.id
    )";

const SUMMARY: &str = "
    , daily AS (
        SELECT (to_timestamp(started_at_ms / 1000.0) AT TIME ZONE $4)::date AS day,
               COUNT(*) AS plays, SUM(listened_ms)::bigint / 1000 AS listened_seconds
          FROM events GROUP BY day
    )
    SELECT jsonb_build_object('plays', COUNT(*), 'tracks', COUNT(DISTINCT content_id),
           'listened_seconds', COALESCE(SUM(listened_ms), 0)::bigint / 1000,
           'daily', COALESCE((SELECT jsonb_agg(to_jsonb(daily) ORDER BY day) FROM daily), '[]'))
      FROM events";

const TOP_ARTISTS: &str = "
    , top AS (
        SELECT lower(name) AS key, MIN(name) AS title, COUNT(*) AS plays
          FROM events CROSS JOIN LATERAL (
              SELECT DISTINCT ON (lower(trim(n))) trim(n) AS name
                FROM jsonb_array_elements_text(COALESCE(metadata_json->'artist_names', '[]') ||
                     COALESCE(metadata_json->'featured_artist_names', '[]')) n
               WHERE trim(n) <> '' ORDER BY lower(trim(n)), trim(n)
          ) credit
         GROUP BY lower(name) ORDER BY plays DESC, key LIMIT 5
    )
    SELECT COALESCE(jsonb_agg(to_jsonb(item) ORDER BY plays DESC, title), '[]') FROM (
        SELECT COALESCE(a.name, top.title) AS title, top.plays, a.id AS artist_id,
               CASE WHEN a.image_file_id IS NOT NULL THEN '/api/player/cover/' || a.image_file_id || '/small' END AS cover_url
          FROM top LEFT JOIN LATERAL (
              SELECT id, name, image_file_id FROM furumusic__artist WHERE name_sort = top.key
                 AND NOT is_hidden ORDER BY id LIMIT 1
          ) a ON true
    ) item";

const TRACK_ROWS: &str = "
    SELECT COALESCE(jsonb_agg(to_jsonb(item) ORDER BY plays DESC, content_id), '[]') FROM (
        SELECT w.content_id, w.plays, l.track_id, l.release_id,
               COALESCE(l.title, w.metadata_json->>'title', '') AS title,
               COALESCE(w.metadata_json->'artist_names', '[]') AS artist_names,
               array_to_string(ARRAY(SELECT jsonb_array_elements_text(w.metadata_json->'artist_names')), ', ') AS detail,
               CASE WHEN l.track_art IS NOT NULL THEN '/api/player/cover/' || l.track_art || '/small' END AS cover_url
          FROM wanted w LEFT JOIN local l USING (content_id)
    ) item";

const RELEASE_ROWS: &str = "
    , editions AS (
        SELECT w.plays, l.release_id, l.album_art,
               COALESCE(l.release_title, w.metadata_json->>'release_title') AS title,
               COALESCE(w.metadata_json->'artist_names', '[]') AS artists,
               w.metadata_json->>'year' AS year
          FROM wanted w LEFT JOIN local l USING (content_id)
    ), grouped AS (
        SELECT release_id, MIN(title) AS title, MIN(album_art) AS art,
               (array_agg(artists ORDER BY artists))[1] AS snapshot_artists,
               SUM(plays)::bigint AS plays
          FROM editions WHERE NULLIF(trim(title), '') IS NOT NULL
         GROUP BY release_id, CASE WHEN release_id IS NULL THEN jsonb_build_array(
             lower(title), ARRAY(SELECT DISTINCT lower(trim(name)) FROM jsonb_array_elements_text(artists) name
                                  ORDER BY lower(trim(name))), year) END
         ORDER BY plays DESC, title, release_id NULLS LAST, snapshot_artists LIMIT 5
    ), ranked AS (
        SELECT g.*, COALESCE((
            SELECT jsonb_agg(a.name ORDER BY ra.position, a.id)
              FROM furumusic__release_artist ra JOIN furumusic__artist a ON a.id = ra.artist_id
             WHERE ra.release_id = g.release_id AND NOT a.is_hidden
        ), snapshot_artists) AS artist_names FROM grouped g
    )
    SELECT COALESCE(jsonb_agg(to_jsonb(item) ORDER BY plays DESC, title, release_id NULLS LAST, artist_names), '[]') FROM (
        SELECT release_id, title, artist_names, plays,
               array_to_string(ARRAY(SELECT jsonb_array_elements_text(artist_names)), ', ') AS detail,
               CASE WHEN art IS NOT NULL THEN '/api/player/cover/' || art || '/small' END AS cover_url
          FROM ranked
    ) item";

async fn overview(
    pool: &PgPool,
    user: i64,
    (start, end): (i64, i64),
    tz: &str,
    kind: &Ranking,
) -> Result<Value, sqlx::Error> {
    let summary_sql = format!("{EVENTS}{SUMMARY}");
    let ranking_sql = match kind {
        Ranking::Artists => format!("{EVENTS}{TOP_ARTISTS}"),
        Ranking::Tracks => format!(
            "{EVENTS}{RECORDINGS}, wanted AS (SELECT * FROM recordings ORDER BY plays DESC, content_id LIMIT 5){LOCAL_TRACKS}{TRACK_ROWS}"
        ),
        Ranking::Releases => format!(
            "{EVENTS}{RECORDINGS}, wanted AS (SELECT * FROM recordings){LOCAL_TRACKS}{RELEASE_ROWS}"
        ),
    };
    let (mut summary, ranking): (Value, Value) = tokio::try_join!(
        sqlx::query_scalar(&summary_sql)
            .bind(user)
            .bind(start)
            .bind(end)
            .bind(tz)
            .fetch_one(pool),
        sqlx::query_scalar(&ranking_sql)
            .bind(user)
            .bind(start)
            .bind(end)
            .fetch_one(pool),
    )?;
    summary["ranking"] = ranking;
    Ok(summary)
}

async fn history(
    pool: &PgPool,
    user: i64,
    (start, end): (i64, i64),
    page: u32,
) -> Result<Value, sqlx::Error> {
    const LIMIT: i64 = 25;
    let sql = format!("{EVENTS}, wanted AS (
        SELECT * FROM events ORDER BY started_at_ms DESC, listen_id DESC LIMIT $4 OFFSET $5
    ){LOCAL_TRACKS}, items AS (
        SELECT w.listen_id AS id, w.content_id, w.started_at_ms, w.listened_ms, l.track_id, l.release_id,
               COALESCE(l.title, w.metadata_json->>'title', '') AS title,
               COALESCE(l.release_title, w.metadata_json->>'release_title', '') AS release_title,
               COALESCE(w.metadata_json->'artist_names', '[]') ||
                 COALESCE(w.metadata_json->'featured_artist_names', '[]') AS artist_names,
               CASE WHEN l.track_art IS NOT NULL THEN '/api/player/cover/' || l.track_art || '/small' END AS cover_url,
               COALESCE(NULLIF(d.name, ''), w.origin_device_id) AS device_name,
               trim(both '\"' from w.ended_reason) = 'finished' AS completed
          FROM wanted w LEFT JOIN local l USING (content_id)
          LEFT JOIN furumusic__fed_device d ON d.user_id = w.user_id AND d.device_id = w.origin_device_id
    ) SELECT jsonb_build_object('total', (SELECT COUNT(*) FROM events), 'items',
        COALESCE((SELECT jsonb_agg(to_jsonb(items) ORDER BY started_at_ms DESC, id DESC) FROM items), '[]'))");
    let mut data: Value = sqlx::query_scalar(&sql)
        .bind(user)
        .bind(start)
        .bind(end)
        .bind(LIMIT)
        .bind((i64::from(page) - 1) * LIMIT)
        .fetch_one(pool)
        .await?;
    data["page"] = json!(page);
    data["limit"] = json!(LIMIT);
    Ok(data)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn personal_query_accepts_calendar_dates_but_no_user_selector() {
        assert!(serde_json::from_value::<Query>(json!({"user_id": 3})).is_err());
        for (date, valid) in [("all", true), ("2024-02-29", true), ("2026-02-29", false)] {
            let query = Query {
                from: Some(date.into()),
                ..Default::default()
            };
            assert_eq!(query.valid_dates(), valid);
        }
    }

    #[tokio::test]
    #[ignore = "requires disposable PostgreSQL via FURU_STATISTICS_TEST_DATABASE_URL"]
    async fn personal_listening_filters_events_and_resolves_playable_editions() -> anyhow::Result<()>
    {
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .connect(&std::env::var("FURU_STATISTICS_TEST_DATABASE_URL")?)
            .await?;
        sqlx::raw_sql(include_str!("../../tests/statistics_fixture.sql"))
            .execute(&pool)
            .await?;
        sqlx::raw_sql("
            ALTER TABLE furumusic__track ADD title text, ADD release_id bigint, ADD audio_file_id bigint, ADD is_hidden bool DEFAULT false, ADD cover_file_id bigint;
            ALTER TABLE furumusic__release ADD title text, ADD is_hidden bool DEFAULT false, ADD cover_file_id bigint;
            ALTER TABLE furumusic__media_file ADD id bigint, ADD sha256_hash text;
            ALTER TABLE furumusic__artist ADD name text, ADD name_sort text, ADD image_file_id bigint, ADD is_hidden bool DEFAULT false;
            CREATE TEMP TABLE furumusic__federation_content_id_cache(media_file_id bigint, sha256_hash text, content_id text);
            CREATE TEMP TABLE furumusic__release_artist(release_id bigint, artist_id bigint, position int);
            INSERT INTO furumusic__release (id,title,cover_file_id) VALUES (14,'Shared release',91);
            INSERT INTO furumusic__artist (id,name,name_sort,image_file_id) VALUES (15,'Artist A','artist a',92);
            INSERT INTO furumusic__release_artist VALUES (14,15,0);
            INSERT INTO furumusic__track (id,title,release_id,audio_file_id) VALUES (50,'Local title',14,51),(60,'Imported track',14,61);
            INSERT INTO furumusic__media_file (id,sha256_hash) VALUES (51,'sha'),(61,'new-sha');
            INSERT INTO furumusic__federation_content_id_cache VALUES (51,'sha','content-one'),(61,'old-sha','content-two');
        ").execute(&pool).await?;
        let all = (0, i64::MAX);
        let data = overview(&pool, 1, all, "UTC", &Ranking::Artists).await?;
        assert_eq!(data["plays"], 31);
        assert_eq!(data["tracks"], 2);
        assert_eq!(data["listened_seconds"], 5520);
        assert_eq!(data["ranking"][0]["plays"], 30);
        assert_eq!(data["ranking"][0]["artist_id"], 15);
        assert_eq!(data["ranking"][1]["title"], "Guest");
        let bob = overview(&pool, 2, all, "UTC", &Ranking::Tracks).await?;
        assert_eq!(bob["plays"], 1);
        assert_eq!(bob["listened_seconds"], 90);
        assert_eq!(bob["ranking"][0]["title"], "Local title");
        assert_eq!(bob["ranking"][0]["track_id"], 50);
        assert_eq!(bob["ranking"][0]["cover_url"], "/api/player/cover/91/small");
        let before = history(&pool, 1, all, 1).await?;
        assert_eq!(before["total"], 31);
        assert_eq!(before["items"].as_array().unwrap().len(), 25);
        assert!(before["items"][0]["track_id"].is_null());
        assert_eq!(before["items"][0]["title"], "Remote song");
        assert_eq!(before["items"][0]["device_name"], "Alice phone");
        assert_eq!(before["items"][0]["completed"], false);
        let page_two = history(&pool, 1, all, 2).await?;
        assert_eq!(page_two["items"].as_array().unwrap().len(), 6);
        sqlx::query("UPDATE furumusic__federation_content_id_cache SET sha256_hash = 'new-sha' WHERE content_id = 'content-two'")
            .execute(&pool).await?;
        let albums = overview(&pool, 1, all, "UTC", &Ranking::Releases).await?;
        assert_eq!(albums["ranking"].as_array().unwrap().len(), 1);
        assert_eq!(albums["ranking"][0]["plays"], 31);
        assert_eq!(albums["ranking"][0]["release_id"], 14);
        assert_eq!(albums["ranking"][0]["artist_names"], json!(["Artist A"]));
        let dst = Query {
            from: Some("2026-11-01".into()),
            to: Some("2026-11-01".into()),
            ..Default::default()
        };
        let range = bounds(&pool, &dst, "America/New_York").await?;
        assert_eq!(range.1 - range.0, 25 * 3_600_000);
        assert_eq!(history(&pool, 1, range, 1).await?["total"], 0);
        let local_days = overview(&pool, 1, all, "America/New_York", &Ranking::Artists).await?;
        assert_eq!(local_days["daily"][0]["day"], "2026-10-31");
        let empty = overview(&pool, 3, all, "UTC", &Ranking::Releases).await?;
        assert_eq!(empty["plays"], 0);
        assert_eq!(empty["ranking"], json!([]));
        Ok(())
    }
}
