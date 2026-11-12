use chrono::{NaiveDate, Utc};
use cot::{db::Database, http::StatusCode, json::Json, response::IntoResponse, session::Session};
use serde::Deserialize;
use serde_json::Value;
use sqlx::PgPool;

use super::v2::{json_error, require_admin_json};

const LISTENS: &str = include_str!("sql/listens.sql");
const PAGE_SIZE: i64 = 25;

#[derive(Debug, Default, Deserialize)]
pub(super) struct StatisticsQuery {
    pub from: Option<String>,
    pub to: Option<String>,
    pub user_id: Option<i64>,
    pub content_id: Option<String>,
    pub artist: Option<String>,
    pub source: Option<String>,
    pub kind: Option<String>,
    pub search: Option<String>,
    pub rank_page: Option<i64>,
    pub history_page: Option<i64>,
    pub history_kind: Option<String>,
}

struct Filter {
    from: i64,
    to: i64,
    user_id: Option<i64>,
    content_id: Option<String>,
    artist: Option<String>,
    source: String,
}

impl Filter {
    fn parse(query: &StatisticsQuery) -> Result<Self, String> {
        fn date(value: &str) -> Result<i64, String> {
            NaiveDate::parse_from_str(value, "%Y-%m-%d")
                .ok()
                .and_then(|d| d.and_hms_opt(0, 0, 0))
                .map(|d| d.and_utc().timestamp_millis())
                .filter(|d| *d >= 0)
                .ok_or_else(|| "Use dates in YYYY-MM-DD format (1970 or later)".to_owned())
        }
        let to = match query.to.as_deref().filter(|s| !s.is_empty()) {
            Some(value) => date(value)?
                .checked_add(86_400_000)
                .ok_or("Invalid end date")?,
            None => Utc::now().timestamp_millis(),
        };
        let from = match query.from.as_deref().filter(|s| !s.is_empty()) {
            Some("all") => 0,
            Some(value) => date(value)?,
            None => to.saturating_sub(30 * 86_400_000),
        };
        if from >= to {
            return Err("Start date must not be after end date".into());
        }
        let source = query.source.as_deref().unwrap_or("all");
        if !["all", "instance", "synced"].contains(&source) {
            return Err("Unknown playback source".into());
        }
        if query.user_id.is_some_and(|id| id <= 0) {
            return Err("Invalid user".into());
        }
        if query.artist.as_ref().is_some_and(|s| s.len() > 1024)
            || query.content_id.as_ref().is_some_and(|s| s.len() > 256)
            || query.search.as_ref().is_some_and(|s| s.len() > 1024)
        {
            return Err("Filter is too long".into());
        }
        Ok(Self {
            from,
            to,
            user_id: query.user_id,
            content_id: query.content_id.clone(),
            artist: query.artist.clone(),
            source: source.into(),
        })
    }

    async fn query(&self, pool: &PgPool, suffix: &str) -> anyhow::Result<Value> {
        let sql = format!("{LISTENS}{suffix}");
        Ok(sqlx::query_scalar(&sql)
            .bind(self.from)
            .bind(self.to)
            .bind(self.user_id)
            .bind(&self.content_id)
            .bind(&self.artist)
            .bind(&self.source)
            .fetch_one(pool)
            .await?)
    }
}

pub(super) async fn statistics(
    session: Session,
    db: Database,
    pool: &PgPool,
    query: StatisticsQuery,
) -> cot::Result<cot::response::Response> {
    if let Err(response) = require_admin_json(&session, &db).await {
        return Ok(response);
    }
    let filter = match Filter::parse(&query) {
        Ok(filter) => filter,
        Err(message) => return Ok(json_error(StatusCode::BAD_REQUEST, &message)),
    };
    let kind = query.kind.as_deref().unwrap_or("artists");
    if !["artists", "tracks", "releases", "users"].contains(&kind) {
        return Ok(json_error(StatusCode::BAD_REQUEST, "Unknown ranking"));
    }
    let history_kind = query.history_kind.as_deref().unwrap_or("all");
    if !["all", "plays", "short"].contains(&history_kind) {
        return Ok(json_error(
            StatusCode::BAD_REQUEST,
            "Unknown history filter",
        ));
    }
    let result = load(pool, &filter, &query, kind, history_kind)
        .await
        .map_err(|e| cot::Error::internal(e.to_string()))?;
    Json(result).into_response()
}

async fn load(
    pool: &PgPool,
    filter: &Filter,
    query: &StatisticsQuery,
    kind: &str,
    history_kind: &str,
) -> anyhow::Result<Value> {
    let summary_sql = r#"
        SELECT to_jsonb(s) FROM (
            SELECT COUNT(*) AS events, COUNT(*) FILTER (WHERE qualified) AS plays,
                   COUNT(*) FILTER (WHERE NOT qualified) AS short_listens,
                   COUNT(*) FILTER (WHERE trim(both '"' from ended_reason) = 'finished') AS completed,
                   COALESCE(SUM(listened_ms), 0)::bigint / 1000 AS listened_seconds,
                   COUNT(DISTINCT user_id) FILTER (WHERE qualified) AS listeners,
                   COUNT(DISTINCT content_id) FILTER (WHERE qualified) AS tracks,
                   COUNT(*) FILTER (WHERE qualified AND source = 'instance') AS instance_plays,
                   COUNT(*) FILTER (WHERE qualified AND source = 'synced') AS synced_plays
            FROM filtered
        ) s"#;
    let daily_sql = r#",
        bounds AS (
            SELECT COALESCE(MIN(started_at_ms), $2) AS first_ms FROM filtered
        ), granularity AS (
            SELECT CASE WHEN $2 - GREATEST($1, first_ms) > 120::bigint * 86400000
                        THEN 'month' ELSE 'day' END AS unit FROM bounds
        ), daily AS (
            SELECT date_trunc(g.unit, to_timestamp(started_at_ms / 1000.0) AT TIME ZONE 'UTC') AS bucket,
                   COUNT(*) FILTER (WHERE qualified) AS plays,
                   COUNT(DISTINCT user_id) FILTER (WHERE qualified) AS listeners,
                   COALESCE(SUM(listened_ms), 0)::bigint / 1000 AS listened_seconds
            FROM filtered CROSS JOIN granularity g GROUP BY bucket
        ), series AS (
            SELECT generate_series((SELECT MIN(bucket) FROM daily), (SELECT MAX(bucket) FROM daily),
                CASE WHEN unit = 'month' THEN interval '1 month' ELSE interval '1 day' END) AS bucket
            FROM granularity
        ), filled AS (
            SELECT s.bucket, COALESCE(d.plays, 0) AS plays, COALESCE(d.listeners, 0) AS listeners,
                   COALESCE(d.listened_seconds, 0) AS listened_seconds
            FROM series s LEFT JOIN daily d USING (bucket)
        )
        SELECT jsonb_build_object('unit', (SELECT unit FROM granularity), 'items',
            COALESCE((SELECT jsonb_agg(to_jsonb(d) ORDER BY bucket) FROM filled d), '[]'::jsonb))"#;
    let (summary, daily, ranking, history, instance) = tokio::try_join!(
        filter.query(pool, summary_sql),
        filter.query(pool, daily_sql),
        ranking(pool, filter, query, kind),
        history(pool, filter, query, history_kind),
        instance(pool),
    )?;
    Ok(
        serde_json::json!({"summary": summary, "daily": daily, "ranking": ranking,
        "history": history, "instance": instance, "from_ms": filter.from, "to_ms": filter.to}),
    )
}

async fn ranking(
    pool: &PgPool,
    filter: &Filter,
    query: &StatisticsQuery,
    kind: &str,
) -> anyhow::Result<Value> {
    // Only fixed SQL fragments are interpolated; every user filter is bound.
    let (key, label, extra, join) = match kind {
        "users" => ("user_id::text", "MIN(user_name)", "''::text", ""),
        "tracks" => (
            "content_id",
            "MIN(title)",
            "MIN(array_to_string(artist_names, ', '))",
            "",
        ),
        "releases" => (
            "jsonb_build_array(release_title, artist_names)::text",
            "MIN(NULLIF(release_title, ''))",
            "MIN(array_to_string(artist_names, ', '))",
            "",
        ),
        _ => (
            "lower(artist.name)",
            "MIN(artist.name)",
            "''::text",
            "CROSS JOIN LATERAL (SELECT DISTINCT ON (lower(n)) n AS name FROM unnest(artist_names) n ORDER BY lower(n), n) artist",
        ),
    };
    let sql = format!(
        r#"{LISTENS}, ranked AS (
        SELECT {key} AS key, {label} AS label, {extra} AS detail, COUNT(*) AS plays,
               COUNT(DISTINCT user_id) AS listeners,
               SUM(listened_ms)::bigint / 1000 AS listened_seconds, MAX(started_at_ms) AS last_play_ms
        FROM filtered {join} WHERE qualified GROUP BY {key}
    ), searched AS (
        SELECT * FROM ranked WHERE label IS NOT NULL
          AND ($7::text IS NULL OR strpos(lower(label || ' ' || detail), lower($7)) > 0)
    ), page AS (SELECT * FROM searched ORDER BY plays DESC, key LIMIT $8 OFFSET $9)
    SELECT jsonb_build_object('total', (SELECT COUNT(*) FROM searched),
        'items', COALESCE((SELECT jsonb_agg(to_jsonb(p) ORDER BY plays DESC, key) FROM page p), '[]'::jsonb))"#
    );
    let page = query.rank_page.unwrap_or(1).clamp(1, 1_000_000);
    let mut result: Value = sqlx::query_scalar(&sql)
        .bind(filter.from)
        .bind(filter.to)
        .bind(filter.user_id)
        .bind(&filter.content_id)
        .bind(&filter.artist)
        .bind(&filter.source)
        .bind(query.search.as_deref().filter(|s| !s.is_empty()))
        .bind(PAGE_SIZE)
        .bind((page - 1) * PAGE_SIZE)
        .fetch_one(pool)
        .await?;
    result["page"] = page.into();
    result["limit"] = PAGE_SIZE.into();
    result["kind"] = kind.into();
    Ok(result)
}

async fn history(
    pool: &PgPool,
    filter: &Filter,
    query: &StatisticsQuery,
    kind: &str,
) -> anyhow::Result<Value> {
    let sql = format!(
        r#"{LISTENS}, selected AS (
        SELECT user_id, user_name, listen_id, content_id, title, artist_names, release_title,
               started_at_ms, listened_ms, track_duration_ms, qualified,
               trim(both '"' from ended_reason) AS ended_reason, device_name, source
        FROM filtered WHERE $7 = 'all' OR ($7 = 'plays' AND qualified) OR ($7 = 'short' AND NOT qualified)
    ), page AS (SELECT * FROM selected ORDER BY started_at_ms DESC, user_id, listen_id LIMIT $8 OFFSET $9)
    SELECT jsonb_build_object('total', (SELECT COUNT(*) FROM selected),
        'items', COALESCE((SELECT jsonb_agg(to_jsonb(p) ORDER BY started_at_ms DESC, user_id, listen_id) FROM page p), '[]'::jsonb))"#
    );
    let page = query.history_page.unwrap_or(1).clamp(1, 1_000_000);
    let mut result: Value = sqlx::query_scalar(&sql)
        .bind(filter.from)
        .bind(filter.to)
        .bind(filter.user_id)
        .bind(&filter.content_id)
        .bind(&filter.artist)
        .bind(&filter.source)
        .bind(kind)
        .bind(PAGE_SIZE)
        .bind((page - 1) * PAGE_SIZE)
        .fetch_one(pool)
        .await?;
    result["page"] = page.into();
    result["limit"] = PAGE_SIZE.into();
    Ok(result)
}

async fn instance(pool: &PgPool) -> anyhow::Result<Value> {
    Ok(sqlx::query_scalar(r#"SELECT jsonb_build_object(
        'users', (SELECT COUNT(*) FROM furumusic__user),
        'tracks', (SELECT COUNT(*) FROM furumusic__track),
        'releases', (SELECT COUNT(*) FROM furumusic__release),
        'artists', (SELECT COUNT(*) FROM furumusic__artist),
        'playlists', (SELECT COUNT(*) FROM furumusic__playlist),
        'media_bytes', (SELECT COALESCE(SUM(file_size_bytes), 0)::bigint FROM furumusic__media_file),
        'formats', (SELECT COALESCE(jsonb_agg(to_jsonb(f)), '[]'::jsonb) FROM (
            SELECT COALESCE(audio_format, 'unknown') AS label, COUNT(*) AS count
            FROM furumusic__media_file WHERE file_type = 'audio' GROUP BY audio_format ORDER BY count DESC
        ) f),
        'reviews', (SELECT COALESCE(jsonb_agg(to_jsonb(r)), '[]'::jsonb) FROM (
            SELECT status::text AS label, COUNT(*) AS count FROM furumusic__pending_review GROUP BY status ORDER BY status
        ) r),
        'jobs', (SELECT COALESCE(jsonb_agg(to_jsonb(j)), '[]'::jsonb) FROM (
            SELECT status::text AS label, COUNT(*) AS count FROM furumusic__job_run GROUP BY status ORDER BY status
        ) j),
        'scrobbles', (SELECT COALESCE(jsonb_agg(to_jsonb(s)), '[]'::jsonb) FROM (
            SELECT status::text AS label, COUNT(*) AS count FROM furumusic__lastfm_scrobble_outbox GROUP BY status ORDER BY status
        ) s)
    )"#).fetch_one(pool).await?)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn dates_are_inclusive_utc_and_filters_are_validated() {
        let q = StatisticsQuery {
            from: Some("2026-11-01".into()),
            to: Some("2026-11-01".into()),
            ..Default::default()
        };
        let f = Filter::parse(&q).unwrap();
        assert_eq!(f.to - f.from, 86_400_000);
        assert!(
            Filter::parse(&StatisticsQuery {
                from: Some("broken".into()),
                ..q
            })
            .is_err()
        );
        assert!(
            Filter::parse(&StatisticsQuery {
                source: Some("invalid".into()),
                ..Default::default()
            })
            .is_err()
        );
    }
    #[tokio::test]
    #[ignore = "requires FURU_STATISTICS_TEST_DATABASE_URL pointing at a disposable PostgreSQL"]
    async fn postgres_statistics_drilldown_and_pagination() {
        let url = std::env::var("FURU_STATISTICS_TEST_DATABASE_URL").expect("test database URL");
        // Temporary tables are isolated to this one connection and disappear on close.
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .connect(&url)
            .await
            .unwrap();
        sqlx::raw_sql(include_str!("../../tests/statistics_fixture.sql"))
            .execute(&pool)
            .await
            .unwrap();
        let mut q = StatisticsQuery {
            from: Some("2026-11-01".into()),
            to: Some("2026-11-03".into()),
            ..Default::default()
        };
        let result = load(&pool, &Filter::parse(&q).unwrap(), &q, "artists", "all")
            .await
            .unwrap();
        assert_eq!(result["summary"]["plays"], 32);
        assert_eq!(result["summary"]["short_listens"], 1);
        assert_eq!(result["summary"]["listeners"], 2);
        assert_eq!(result["summary"]["listened_seconds"], 5618);
        assert_eq!(result["summary"]["synced_plays"], 1);
        assert_eq!(result["ranking"]["items"][0]["label"], "Artist A");
        assert_eq!(result["ranking"]["items"][0]["plays"], 31); // duplicate artist credits never multiply plays
        assert_eq!(result["ranking"]["items"][0]["listeners"], 2);
        assert_eq!(result["history"]["total"], 33);
        assert_eq!(result["history"]["items"].as_array().unwrap().len(), 25);
        assert_eq!(result["daily"]["items"].as_array().unwrap().len(), 3);
        assert_eq!(result["daily"]["items"][1]["plays"], 0); // fill the silent day
        q.artist = Some("artist a".into());
        let result = load(&pool, &Filter::parse(&q).unwrap(), &q, "users", "all")
            .await
            .unwrap();
        assert_eq!(result["ranking"]["total"], 2);
        assert_eq!(result["ranking"]["items"][0]["label"], "Alice");
        assert_eq!(result["ranking"]["items"][0]["plays"], 30);
        q.user_id = Some(2);
        let result = load(&pool, &Filter::parse(&q).unwrap(), &q, "tracks", "short")
            .await
            .unwrap();
        assert_eq!(result["summary"]["plays"], 1);
        assert_eq!(result["history"]["total"], 1);
        assert_eq!(result["history"]["items"][0]["ended_reason"], "skipped");
        assert_eq!(result["ranking"]["items"][0]["label"], "First song"); // no local track exists
        q.user_id = None;
        q.artist = None;
        q.history_page = Some(2);
        let result = load(&pool, &Filter::parse(&q).unwrap(), &q, "releases", "all")
            .await
            .unwrap();
        assert_eq!(result["history"]["items"].as_array().unwrap().len(), 8);
        q.source = Some("synced".into());
        let result = load(&pool, &Filter::parse(&q).unwrap(), &q, "users", "all")
            .await
            .unwrap();
        assert_eq!(result["summary"]["plays"], 1);
        q.source = Some("instance".into());
        q.search = Some("' OR 1=1 --".into());
        let result = load(&pool, &Filter::parse(&q).unwrap(), &q, "artists", "all")
            .await
            .unwrap();
        assert_eq!(result["summary"]["plays"], 31);
        assert_eq!(result["ranking"]["total"], 0);
        q.user_id = Some(3);
        let result = load(&pool, &Filter::parse(&q).unwrap(), &q, "users", "all")
            .await
            .unwrap();
        assert_eq!(result["summary"]["plays"], 0);
        assert_eq!(result["history"]["total"], 0);
        pool.close().await;
    }
}
