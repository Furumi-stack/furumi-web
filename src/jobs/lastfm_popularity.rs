use std::time::Duration;

use anyhow::Context;
use reqwest::{Client, StatusCode};
use serde::Deserialize;
use sqlx::PgConnection;

use crate::scheduler::{Job, JobContext, JobLog};

pub struct LastfmPopularityJob;

const LASTFM_API_URL: &str = "https://ws.audioscrobbler.com/2.0/";
const LASTFM_REQUEST_DELAY: Duration = Duration::from_millis(1200);
const TRACK_PAGE_SIZE: i64 = 500;
const ADVISORY_LOCK_ID: i64 = 0x4c41_5354_464d_504f; // "LASTFMPO"

/// Scheduled and ordinary manual runs use light mode. Full mode is opt-in.
#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LastfmPopularityOptions {
    pub refresh_existing: bool,
}

#[derive(Debug, sqlx::FromRow)]
struct TrackLookupRow {
    id: i64,
    title: String,
    artist_name: Option<String>,
}

// Store the lookup identity alongside the result so every metadata editing path
// (including artist renames and link changes) automatically invalidates it.
const TRACK_CANDIDATES_SQL: &str = r#"
    SELECT t.id, t.title::text AS title, primary_artist.name AS artist_name
      FROM furumusic__track t
      LEFT JOIN LATERAL (
          SELECT a.name::text AS name
            FROM furumusic__track_artist ta
            JOIN furumusic__artist a ON a.id = ta.artist_id
           WHERE ta.track_id = t.id AND ta.role <> 'featuring'
           ORDER BY ta.position, ta.artist_id
           LIMIT 1
      ) primary_artist ON true
     WHERE t.is_hidden = false AND t.id > $1 AND t.id <= $2
       AND ($3 OR t.lastfm_checked_at IS NULL
               OR t.lastfm_lookup_title IS DISTINCT FROM t.title::text
               OR t.lastfm_lookup_artist IS DISTINCT FROM primary_artist.name)
     ORDER BY t.id
     LIMIT $4
"#;

#[derive(Debug, Deserialize)]
struct LastfmTrackInfoResponse {
    track: Option<LastfmTrack>,
    error: Option<i32>,
    message: Option<String>,
}

#[derive(Debug, Deserialize)]
struct LastfmTrack {
    listeners: String,
    playcount: String,
}

#[derive(Debug, PartialEq)]
enum LookupOutcome {
    Found { listeners: i64, playcount: i64 },
    NotFound,
}

#[derive(Debug)]
enum LookupError {
    Http(StatusCode),
    Api { code: i32, message: String },
    Transport(reqwest::Error),
    InvalidResponse(String),
}

impl LookupError {
    fn should_stop_run(&self) -> bool {
        match self {
            Self::Http(status) => matches!(status.as_u16(), 401 | 403 | 429),
            Self::Api { code, .. } => matches!(code, 2 | 3 | 4 | 5 | 9 | 10 | 11 | 13 | 26 | 29),
            _ => false,
        }
    }
}

impl std::fmt::Display for LookupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Http(status) => write!(f, "Last.fm HTTP {status}"),
            Self::Api { code, message } => write!(f, "Last.fm API error {code}: {message}"),
            Self::Transport(err) => write!(f, "Last.fm request failed: {err}"),
            Self::InvalidResponse(message) => write!(f, "Invalid Last.fm response: {message}"),
        }
    }
}

impl std::error::Error for LookupError {}

#[derive(Default)]
struct PopularityStats {
    considered: u64,
    requested: u64,
    updated: u64,
    not_found: u64,
    skipped: u64,
    failed: u64,
}

impl PopularityStats {
    fn summary(&self) -> String {
        format!(
            "{} considered, {} requested, {} updated, {} not found, {} skipped, {} failed",
            self.considered,
            self.requested,
            self.updated,
            self.not_found,
            self.skipped,
            self.failed
        )
    }
}

#[async_trait::async_trait]
impl Job for LastfmPopularityJob {
    fn name(&self) -> &'static str {
        "lastfm_popularity"
    }

    fn description(&self) -> &'static str {
        "Fetch Last.fm popularity for unchecked tracks; optionally refresh all tracks"
    }

    fn default_cron(&self) -> &'static str {
        // Sundays at 04:15
        "0 15 4 * * Sun"
    }

    async fn run(&self, ctx: &JobContext, log: &mut JobLog) -> anyhow::Result<()> {
        run_with_options(ctx, log, LastfmPopularityOptions::default()).await
    }
}

pub async fn run_with_options(
    ctx: &JobContext,
    log: &mut JobLog,
    options: LastfmPopularityOptions,
) -> anyhow::Result<()> {
    let api_key = ctx.config.lastfm_api_key.trim();
    if api_key.is_empty() {
        log.warn("lastfm_api_key is not configured, skipping Last.fm popularity update");
        return Ok(());
    }

    let mut conn = ctx.pool.acquire().await?;
    // A session lock must never return to the pool, including on cancellation.
    // All database work below uses this connection; no transaction spans HTTP.
    conn.close_on_drop();
    if !try_lock(&mut conn).await? {
        log.info("Last.fm popularity update is already running; skipping this run");
        return Ok(());
    }

    let client = Client::builder()
        .user_agent(format!(
            "furumusic-lastfm-popularity/{}",
            env!("CARGO_PKG_VERSION")
        ))
        .timeout(Duration::from_secs(15))
        .build()?;
    log.info(&format!(
        "Last.fm popularity options: mode={}; request delay={} ms",
        if options.refresh_existing {
            "full"
        } else {
            "light"
        },
        LASTFM_REQUEST_DELAY.as_millis()
    ));
    let result = process_tracks(&mut conn, &client, api_key, options, log).await;
    // Release promptly on normal completion; close_on_drop also covers errors
    // and task cancellation without leaking the lock into another run.
    conn.close().await?;
    result
}

async fn try_lock(conn: &mut PgConnection) -> anyhow::Result<bool> {
    Ok(sqlx::query_scalar("SELECT pg_try_advisory_lock($1)")
        .bind(ADVISORY_LOCK_ID)
        .fetch_one(conn)
        .await?)
}

async fn fetch_candidates(
    conn: &mut PgConnection,
    after_id: i64,
    max_id: i64,
    options: LastfmPopularityOptions,
) -> anyhow::Result<Vec<TrackLookupRow>> {
    Ok(sqlx::query_as(TRACK_CANDIDATES_SQL)
        .bind(after_id)
        .bind(max_id)
        .bind(options.refresh_existing)
        .bind(TRACK_PAGE_SIZE)
        .fetch_all(conn)
        .await?)
}

async fn process_tracks(
    conn: &mut PgConnection,
    client: &Client,
    api_key: &str,
    options: LastfmPopularityOptions,
    log: &mut JobLog,
) -> anyhow::Result<()> {
    // Bound the snapshot by id, not by count: every eligible track at the start
    // can be processed, while concurrent imports wait for the next run.
    let max_id: i64 = sqlx::query_scalar("SELECT COALESCE(MAX(id), 0) FROM furumusic__track")
        .fetch_one(&mut *conn)
        .await?;
    let mut after_id = 0;
    let mut stats = PopularityStats::default();
    let mut next_request_at = tokio::time::Instant::now();
    let mut last_progress = std::time::Instant::now();
    loop {
        let tracks = fetch_candidates(conn, after_id, max_id, options).await?;
        if tracks.is_empty() {
            break;
        }
        // Invalidate the page before HTTP, preserving its statistics. A failed
        // write or cancellation during a full refresh then also leaves work for
        // the next light run. This costs one update per page, not per request.
        let ids: Vec<i64> = tracks.iter().map(|track| track.id).collect();
        mark_pending(conn, &ids).await?;
        for track in tracks {
            // Advance even after an error so a failing track is tried only once
            // per run and cannot starve the remaining candidates.
            after_id = track.id;
            stats.considered += 1;
            let artist = track.artist_name.as_deref().unwrap_or_default();
            if artist.trim().is_empty() || track.title.trim().is_empty() {
                stats.skipped += 1;
                // Missing local metadata says nothing about the Last.fm catalog.
                continue;
            }
            tokio::time::sleep_until(next_request_at).await;
            stats.requested += 1;
            let outcome = fetch_track_info(client, api_key, artist, &track.title).await;
            next_request_at = tokio::time::Instant::now() + LASTFM_REQUEST_DELAY;
            match outcome {
                Ok(outcome) => {
                    if save_outcome(conn, &track, &outcome).await? {
                        match outcome {
                            LookupOutcome::Found { .. } => stats.updated += 1,
                            LookupOutcome::NotFound => stats.not_found += 1,
                        }
                    } else {
                        // Metadata changed or the track was removed during HTTP.
                        stats.skipped += 1;
                    }
                }
                Err(err) => {
                    stats.failed += 1;
                    tracing::warn!(track_id = track.id, error = %err, "Last.fm popularity lookup failed");
                    if stats.failed <= 5 || err.should_stop_run() {
                        log.warn(&format!(
                            "Last.fm lookup failed for track {}: {err}",
                            track.id
                        ));
                    }
                    if err.should_stop_run() {
                        log.error(&format!("Last.fm popularity stopped: {}", stats.summary()));
                        return Err(err.into());
                    }
                }
            }
            // Keep live logs bounded: the shared logger rewrites its entire
            // accumulated output whenever a new line is added.
            if last_progress.elapsed() >= Duration::from_secs(30) {
                log.info(&format!("Last.fm progress: {}", stats.summary()));
                last_progress = std::time::Instant::now();
            }
        }
    }
    log.info(&format!("Last.fm popularity finished: {}", stats.summary()));
    anyhow::ensure!(
        stats.failed == 0,
        "{} Last.fm lookups failed; they will be retried in light mode",
        stats.failed
    );
    Ok(())
}

async fn mark_pending(conn: &mut PgConnection, track_ids: &[i64]) -> anyhow::Result<()> {
    sqlx::query(
        "UPDATE furumusic__track SET lastfm_checked_at = NULL \
         WHERE id = ANY($1) AND lastfm_checked_at IS NOT NULL",
    )
    .bind(track_ids)
    .execute(conn)
    .await?;
    Ok(())
}

async fn save_outcome(
    conn: &mut PgConnection,
    track: &TrackLookupRow,
    outcome: &LookupOutcome,
) -> anyhow::Result<bool> {
    let (listeners, playcount, rating, updated_at) = match outcome {
        LookupOutcome::Found {
            listeners,
            playcount,
        } => (
            Some(*listeners),
            Some(*playcount),
            Some(popularity_rating(*listeners, *playcount)),
            Some(chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()),
        ),
        // An authoritative miss replaces the current result, but preserves
        // historical observations. Unknown popularity is not a zero rating.
        LookupOutcome::NotFound => (None, None, None, None),
    };
    // The data-modifying CTE atomically saves the result and its history in one
    // round trip. Ignore responses for a lookup identity that is already stale.
    let saved: bool = sqlx::query_scalar(
        r#"
        WITH updated AS (
            UPDATE furumusic__track t
               SET lastfm_listeners = $2, lastfm_playcount = $3,
                   lastfm_rating = $4, lastfm_updated_at = $5,
                   lastfm_checked_at = CURRENT_TIMESTAMP,
                   lastfm_lookup_title = $6, lastfm_lookup_artist = $7
             WHERE t.id = $1 AND t.is_hidden = false AND t.title::text = $6
               AND (SELECT a.name::text
                      FROM furumusic__track_artist ta
                      JOIN furumusic__artist a ON a.id = ta.artist_id
                     WHERE ta.track_id = t.id AND ta.role <> 'featuring'
                     ORDER BY ta.position, ta.artist_id LIMIT 1) IS NOT DISTINCT FROM $7
            RETURNING id
        ), history AS (
            INSERT INTO furumusic__track_popularity_history
                (track_id, source, listeners, playcount, rating, fetched_at)
            SELECT id, 'lastfm', $2::bigint, $3::bigint, $4::double precision, $5::varchar
              FROM updated WHERE $2::bigint IS NOT NULL
        )
        SELECT EXISTS (SELECT 1 FROM updated)
    "#,
    )
    .bind(track.id)
    .bind(listeners)
    .bind(playcount)
    .bind(rating)
    .bind(updated_at)
    .bind(&track.title)
    .bind(&track.artist_name)
    .fetch_one(conn)
    .await?;
    Ok(saved)
}

async fn fetch_track_info(
    client: &Client,
    api_key: &str,
    artist: &str,
    track: &str,
) -> Result<LookupOutcome, LookupError> {
    let response = client
        .get(LASTFM_API_URL)
        .query(&[
            ("method", "track.getInfo"),
            ("api_key", api_key),
            ("artist", artist),
            ("track", track),
            ("autocorrect", "1"),
            ("format", "json"),
        ])
        .send()
        .await
        .map_err(|err| LookupError::Transport(err.without_url()))?;
    let status = response.status();
    if !status.is_success() {
        return Err(LookupError::Http(status));
    }
    let body = response
        .bytes()
        .await
        .map_err(|err| LookupError::Transport(err.without_url()))?;
    interpret_response(status, &body)
}

fn interpret_response(status: StatusCode, body: &[u8]) -> Result<LookupOutcome, LookupError> {
    // A bare HTTP 404 (or any other HTTP failure) never proves catalog absence.
    if !status.is_success() {
        return Err(LookupError::Http(status));
    }
    let body: LastfmTrackInfoResponse = serde_json::from_slice(body)
        .map_err(|err| LookupError::InvalidResponse(err.to_string()))?;
    if let Some(code) = body.error {
        let message = body.message.unwrap_or_else(|| "unknown error".to_owned());
        // Last.fm uses error 6 for both missing tracks and invalid parameters.
        // Only an explicit track-not-found message is a cacheable miss.
        if matches!(code, 6 | 7)
            && message
                .trim()
                .trim_end_matches('.')
                .eq_ignore_ascii_case("track not found")
        {
            return Ok(LookupOutcome::NotFound);
        }
        return Err(LookupError::Api { code, message });
    }
    let info = body
        .track
        .ok_or_else(|| LookupError::InvalidResponse("missing track".to_owned()))?;
    let parse_count = |value: &str, field: &str| {
        value
            .parse::<i64>()
            .ok()
            .filter(|value| *value >= 0)
            .with_context(|| format!("invalid {field}"))
            .map_err(|err| LookupError::InvalidResponse(err.to_string()))
    };
    Ok(LookupOutcome::Found {
        listeners: parse_count(&info.listeners, "listeners")?,
        playcount: parse_count(&info.playcount, "playcount")?,
    })
}

fn popularity_rating(listeners: i64, playcount: i64) -> f64 {
    (playcount as f64).ln_1p() * (listeners as f64).ln_1p()
}

#[cfg(test)]
#[path = "lastfm_popularity_tests.rs"]
mod tests;
