//! LRCLIB enrichment: durable positive/negative cache, bounded batches and retries.
use crate::scheduler::{Job, JobContext, JobLog};
use anyhow::Result;
use music_dht::lyrics::LyricsVersion;
use reqwest::Client;
use serde::Deserialize;
use sqlx::Row;

pub struct LyricsBackfillJob;

#[cfg(test)]
#[path = "lyrics_backfill_tests.rs"]
mod database_tests;
const BATCH_SIZE: i64 = 200;
const MAX_API_BYTES: usize = 4 * 1024 * 1024;

#[derive(Debug, sqlx::FromRow)]
struct Candidate {
    hash: String,
    title: String,
    artist: String,
    album: String,
    duration: f64,
    signature: String,
}

impl Candidate {
    fn log_context(&self) -> String {
        format!(
            "track {:?}, artist {:?}, audio {}",
            self.title.chars().take(120).collect::<String>(),
            self.artist.chars().take(120).collect::<String>(),
            self.hash
        )
    }
}

// Signature changes automatically invalidate a previous positive or negative lookup.
const CANDIDATES: &str = r#"WITH candidates AS (
    SELECT DISTINCT ON(m.sha256_hash) m.sha256_hash::text AS hash,t.title::text,
        COALESCE(a.name,'')::text AS artist,r.title::text AS album,t.duration_seconds AS duration,
        md5(concat_ws(E'\n',lower(trim(t.title)),lower(trim(COALESCE(a.name,''))),lower(trim(r.title)),round(t.duration_seconds)::text)) AS signature
    FROM furumusic__track t JOIN furumusic__release r ON r.id=t.release_id
    JOIN furumusic__media_file m ON m.id=t.audio_file_id
    LEFT JOIN LATERAL(SELECT string_agg(ar.name, ', ' ORDER BY ta.position,ar.id) AS name
        FROM furumusic__track_artist ta JOIN furumusic__artist ar ON ar.id=ta.artist_id
        WHERE ta.track_id=t.id AND ta.role='main') a ON true
    WHERE NOT t.is_hidden AND NOT r.is_hidden
    ORDER BY m.sha256_hash,t.id
) SELECT c.* FROM candidates c
LEFT JOIN furumusic__lyrics_binding b ON b.audio_sha256=c.hash
LEFT JOIN furumusic__lyrics_lookup l ON l.signature=c.signature
WHERE (b.signature IS DISTINCT FROM c.signature OR l.signature IS NULL OR l.status='retry')
  AND ($2 OR l.retry_at IS NULL OR l.retry_at<=now())
  AND ($3::text IS NULL OR c.hash > $3)
ORDER BY c.hash LIMIT $1"#;

#[async_trait::async_trait]
impl Job for LyricsBackfillJob {
    fn name(&self) -> &'static str {
        "lyrics_backfill"
    }
    fn description(&self) -> &'static str {
        "Enrich track lyrics from LRCLIB (cached, including missing lyrics)"
    }
    fn default_cron(&self) -> &'static str {
        "0 0 4 * * *"
    }
    async fn run(&self, ctx: &JobContext, log: &mut JobLog) -> Result<()> {
        if !ctx.config.lyrics_enrichment_enabled {
            log.info("Automatic LRCLIB enrichment is disabled; stored lyrics remain available.");
            return Ok(());
        }
        log.info("Starting LRCLIB lyrics enrichment; completed lookups will be skipped.");
        // Prevent overlapping manual/scheduled runs, including across server processes.
        let mut lock_connection = ctx.pool.begin().await?;
        let locked: bool = sqlx::query_scalar("SELECT pg_try_advisory_xact_lock(7834219601)")
            .fetch_one(&mut *lock_connection)
            .await?;
        if !locked {
            log.info("Another lyrics lookup is running.");
            return Ok(());
        }
        let result = enrich(ctx, log).await;
        if let Err(error) = &result {
            log.error(&format!("{error:#}"));
        }
        // Transaction locks are also released if the task is cancelled.
        lock_connection.rollback().await?;
        result
    }
}

async fn enrich(ctx: &JobContext, log: &mut JobLog) -> Result<()> {
    let manual = is_manual_run(&ctx.pool, ctx.run_id).await?;
    if cooldown_active(&ctx.pool, manual).await? {
        log.info("LRCLIB retry cooldown is active.");
        return Ok(());
    }
    if manual {
        log.info("Manual run: bypassing saved LRCLIB cooldown and retry delays.");
    }
    let client = Client::builder()
        .timeout(std::time::Duration::from_secs(25))
        .user_agent(concat!(
            "furumusic/",
            env!("CARGO_PKG_VERSION"),
            " (https://gt.hexor.cy/ab/furumusic)"
        ))
        .build()?;
    let mut processed = 0;
    let mut requests = 0;
    let mut found = 0;
    let mut deferred = 0;
    let mut cursor: Option<String> = None;
    'scan: loop {
        let batch = sqlx::query_as::<_, Candidate>(CANDIDATES)
            .bind(BATCH_SIZE)
            .bind(manual)
            .bind(cursor.as_deref())
            .fetch_all(&ctx.pool)
            .await?;
        if batch.is_empty() {
            break;
        }
        // Advance past deferred rows even when a manual run ignores retry_at.
        cursor = batch.last().map(|track| track.hash.clone());
        for track in batch {
            let (config, _) = crate::config::AppConfig::load_with_db(&ctx.db).await;
            if !config.lyrics_enrichment_enabled {
                log.info("LRCLIB enrichment was disabled.");
                return Ok(());
            }
            let cached = sqlx::query("SELECT status,versions FROM furumusic__lyrics_lookup WHERE signature=$1 AND status<>'retry'")
                .bind(&track.signature).fetch_optional(&ctx.pool).await?;
            let versions: Vec<LyricsVersion> = if let Some(row) = cached {
                serde_json::from_value(row.get("versions"))?
            } else {
                // Invalid/incomplete signatures are terminal until metadata changes.
                let outcome = if track.title.trim().is_empty()
                    || track.artist.trim().is_empty()
                    || !track.duration.is_finite()
                    || track.duration < 1.0
                {
                    Ok(Vec::new())
                } else {
                    requests += 1;
                    let result = lookup(&client, &track, log).await;
                    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
                    result
                };
                match outcome {
                    Ok(versions) => {
                        sqlx::query("INSERT INTO furumusic__lyrics_lookup(signature,status,versions) VALUES($1,$2,$3)
                            ON CONFLICT(signature) DO UPDATE SET status=EXCLUDED.status,versions=EXCLUDED.versions,retry_at=NULL,checked_at=now()")
                            .bind(&track.signature).bind(if versions.is_empty(){"not_found"}else{"found"}).bind(serde_json::to_value(&versions)?).execute(&ctx.pool).await?;
                        versions
                    }
                    Err(error) => {
                        let delay = error.retry_after.unwrap_or(300).clamp(30, 31_536_000);
                        save_retry(&ctx.pool, &track.signature, delay).await?;
                        let action = if error.service_unavailable {
                            save_retry(&ctx.pool, "_lrclib_cooldown", delay).await?;
                            "paused run and saved service cooldown"
                        } else {
                            "deferred track and continued"
                        };
                        log.warn(&format!(
                            "LRCLIB /api/search for {}: {}; {action} (retry delay at least {delay}s).",
                            track.log_context(),
                            error.message
                        ));
                        deferred += 1;
                        if error.service_unavailable {
                            break 'scan;
                        }
                        continue;
                    }
                }
            };
            attach(&ctx.pool, &track, &versions).await?;
            processed += 1;
            if !versions.is_empty() {
                found += 1
            }
            if processed == 1 || processed % 25 == 0 {
                log.info(&format!(
                    "Lyrics: {processed} content items, {found} found, {requests} API lookups."
                ));
            }
        }
    }
    log.info(&format!(
        "Lyrics run summary: {processed} content items completed, {found} found, {deferred} deferred, {requests} API lookups."
    ));
    Ok(())
}

async fn is_manual_run(pool: &sqlx::PgPool, run_id: i64) -> Result<bool> {
    Ok(sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM furumusic__job_run WHERE id=$1 AND job_name='lyrics_backfill' AND trigger='manual')")
        .bind(run_id).fetch_one(pool).await?)
}

async fn cooldown_active(pool: &sqlx::PgPool, manual: bool) -> Result<bool> {
    if manual {
        return Ok(false);
    }
    Ok(sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM furumusic__lyrics_lookup WHERE signature='_lrclib_cooldown' AND retry_at>now())").fetch_one(pool).await?)
}

async fn save_retry(pool: &sqlx::PgPool, signature: &str, delay: u64) -> Result<()> {
    let seconds = delay as f64
        * (1.0 + f64::from(chrono::Utc::now().timestamp_subsec_millis()) / 1000.0 * 0.1);
    sqlx::query("INSERT INTO furumusic__lyrics_lookup(signature,status,retry_at) VALUES($1,'retry',now()+make_interval(secs=>$2))
        ON CONFLICT(signature) DO UPDATE SET status='retry', attempts=furumusic__lyrics_lookup.attempts+1,
        retry_at=now()+make_interval(secs=>GREATEST($2,LEAST(86400,300*power(2,LEAST(furumusic__lyrics_lookup.attempts,8))))),checked_at=now()")
        .bind(signature).bind(seconds).execute(pool).await?;
    Ok(())
}

async fn attach(pool: &sqlx::PgPool, track: &Candidate, versions: &[LyricsVersion]) -> Result<()> {
    // Atomic attachment + progress: crash recovery never repeats a completed API lookup.
    let mut tx = pool.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1,0))")
        .bind(&track.hash)
        .execute(&mut *tx)
        .await?;
    let ids: Vec<&str> = versions.iter().map(|v| v.id.as_str()).collect();
    sqlx::query("DELETE FROM furumusic__lyrics WHERE audio_sha256=$1 AND source='lrclib' AND NOT(id=ANY($2))")
        .bind(&track.hash).bind(&ids).execute(&mut *tx).await?;
    for version in versions {
        sqlx::query("INSERT INTO furumusic__lyrics(audio_sha256,id,label,source,source_id,language,plain_text,synced_lrc,instrumental)
                    SELECT $1,$2,$3,$4,$5,$6,$7,$8,$9 WHERE (SELECT count(*) FROM furumusic__lyrics WHERE audio_sha256=$1)<16
                    ON CONFLICT(audio_sha256,id) DO UPDATE SET label=EXCLUDED.label,plain_text=EXCLUDED.plain_text,synced_lrc=EXCLUDED.synced_lrc,instrumental=EXCLUDED.instrumental,updated_at=now()
                    WHERE furumusic__lyrics.source<>'manual'")
                    .bind(&track.hash).bind(&version.id).bind(&version.label).bind(&version.source).bind(&version.source_id).bind(&version.language)
                    .bind(&version.plain_text).bind(&version.synced_lrc).bind(version.instrumental).execute(&mut *tx).await?;
    }
    sqlx::query("INSERT INTO furumusic__lyrics_binding(audio_sha256,signature) VALUES($1,$2) ON CONFLICT(audio_sha256) DO UPDATE SET signature=EXCLUDED.signature")
                .bind(&track.hash).bind(&track.signature).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(())
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Record {
    id: i64,
    track_name: String,
    artist_name: String,
    album_name: Option<String>,
    duration: Option<f64>,
    #[serde(default)]
    instrumental: bool,
    plain_lyrics: Option<String>,
    synced_lyrics: Option<String>,
}
struct LookupError {
    message: String,
    retry_after: Option<u64>,
    service_unavailable: bool,
}

impl LookupError {
    fn bad_response(message: String) -> Self {
        Self {
            message,
            retry_after: None,
            service_unavailable: false,
        }
    }
}

async fn get_json_url<T: serde::de::DeserializeOwned>(
    client: &Client,
    url: &str,
    query: &[(&str, String)],
) -> std::result::Result<T, LookupError> {
    let mut response = client
        .get(url)
        .query(query)
        .send()
        .await
        .map_err(|error| LookupError {
            service_unavailable: error.is_connect(),
            message: format!("request failed: {}", error.without_url()),
            retry_after: None,
        })?;
    if !response.status().is_success() {
        return Err(LookupError {
            message: format!("HTTP {}", response.status()),
            service_unavailable: matches!(response.status().as_u16(), 429 | 503),
            retry_after: response
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse().ok()),
        });
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|error| {
        LookupError::bad_response(format!(
            "reading response body failed: {}",
            error.without_url()
        ))
    })? {
        if bytes.len() + chunk.len() > MAX_API_BYTES {
            return Err(LookupError::bad_response(format!(
                "response exceeds {MAX_API_BYTES} bytes"
            )));
        }
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&bytes).map_err(|error| {
        LookupError::bad_response(format!(
            "invalid JSON search array ({} bytes, {:?} error at line {}, column {})",
            bytes.len(),
            error.classify(),
            error.line(),
            error.column()
        ))
    })
}
fn normalized(value: &str) -> String {
    value
        .chars()
        .flat_map(char::to_lowercase)
        .filter(|c| c.is_alphanumeric())
        .collect()
}
fn matching(record: &Record, track: &Candidate) -> bool {
    normalized(&record.track_name) == normalized(&track.title)
        && normalized(&record.artist_name) == normalized(&track.artist)
        && record.duration.is_some_and(|duration| {
            duration.is_finite() && (duration - track.duration).abs() <= 2.0
        })
}
fn version(mut record: Record) -> Option<LyricsVersion> {
    // JSON permits NUL, but PostgreSQL text/JSONB cannot represent it. Provider
    // texts occasionally contain these invisible bytes; remove them before
    // parsing LRC or writing either the lookup cache or the content binding.
    let album = without_nul(record.album_name.unwrap_or_default());
    record.plain_lyrics = record.plain_lyrics.map(without_nul);
    record.synced_lyrics = record.synced_lyrics.map(without_nul);
    let synced = record
        .synced_lyrics
        .filter(|v| !crate::lyrics::parse_lrc(v).is_empty());
    let value = LyricsVersion {
        id: format!("lrclib:{}", record.id),
        label: if album.is_empty() {
            "LRCLIB".into()
        } else {
            format!("LRCLIB · {album}")
        },
        source: "lrclib".into(),
        source_id: Some(record.id.to_string()),
        language: None,
        plain_text: record.plain_lyrics.filter(|v| !v.trim().is_empty()),
        synced_lrc: synced,
        instrumental: record.instrumental,
    };
    value.validate().ok().map(|()| value)
}

fn without_nul(mut text: String) -> String {
    text.retain(|c| c != '\0');
    text
}
async fn lookup(
    client: &Client,
    track: &Candidate,
    log: &mut JobLog,
) -> std::result::Result<Vec<LyricsVersion>, LookupError> {
    let query = vec![
        ("track_name", track.title.clone()),
        ("artist_name", track.artist.clone()),
    ];
    // Search returns all text representations in one request; no per-result fetches.
    let values: Vec<serde_json::Value> =
        get_json_url(client, "https://lrclib.net/api/search", &query).await?;
    let (records, rejected, first_rejected) = decode_records(values);
    let versions = select_versions(records, track);
    if let Some(index) = first_rejected {
        let detail =
            format!("skipped {rejected} malformed search records (first at index {index})");
        if versions.is_empty() {
            return Err(LookupError::bad_response(detail));
        }
        log.warn(&format!(
            "LRCLIB /api/search for {}: {detail}.",
            track.log_context()
        ));
    }
    Ok(versions)
}

fn decode_records(values: Vec<serde_json::Value>) -> (Vec<Record>, usize, Option<usize>) {
    let mut records = Vec::new();
    let mut rejected = 0;
    let mut first_rejected = None;
    for (index, value) in values.into_iter().enumerate() {
        match serde_json::from_value(value) {
            Ok(record) => records.push(record),
            Err(_) => {
                rejected += 1;
                first_rejected.get_or_insert(index);
            }
        }
    }
    (records, rejected, first_rejected)
}

fn select_versions(records: Vec<Record>, track: &Candidate) -> Vec<LyricsVersion> {
    // LRCLIB may return null/missing durations among otherwise valid search
    // results. These cannot be matched safely, but must not reject the batch.
    let mut records: Vec<_> = records.into_iter().filter(|r| matching(r, track)).collect();
    records.sort_by(|a, b| {
        let rank = |r: &Record| {
            (
                r.synced_lyrics
                    .as_ref()
                    .is_some_and(|v| !crate::lyrics::parse_lrc(v).is_empty()),
                r.album_name
                    .as_deref()
                    .is_some_and(|album| normalized(album) == normalized(&track.album)),
                !r.instrumental,
            )
        };
        rank(b)
            .cmp(&rank(a))
            .then_with(|| {
                let difference = |r: &Record| {
                    r.duration
                        .map(|duration| (duration - track.duration).abs())
                        .unwrap_or(f64::INFINITY)
                };
                difference(a).total_cmp(&difference(b))
            })
            .then(a.id.cmp(&b.id))
    });
    records.into_iter().find_map(version).into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn malformed_search_records_do_not_discard_valid_neighbors() {
        let (records, rejected, first_rejected) = decode_records(vec![
            serde_json::json!({"id": 1, "trackName": null, "artistName": "Artist"}),
            serde_json::json!({"id": 2, "trackName": "Song", "artistName": "Artist",
                               "albumName": null, "duration": 100, "plainLyrics": "Words"}),
            serde_json::json!({"id": 3, "trackName": "Song", "artistName": "Artist",
                               "syncedLyrics": ["wrong type"]}),
            serde_json::Value::Null,
        ]);
        assert_eq!(rejected, 3);
        assert_eq!(first_rejected, Some(0));
        let lyrics = version(records.into_iter().next().unwrap()).unwrap();
        assert_eq!(lyrics.id, "lrclib:2");
        assert_eq!(lyrics.label, "LRCLIB");
        assert_eq!(lyrics.plain_text.as_deref(), Some("Words"));
    }

    #[test]
    fn nullable_duration_does_not_discard_valid_search_results() {
        let track = Candidate {
            hash: "hash".into(),
            title: "Song".into(),
            artist: "Artist".into(),
            album: "Album".into(),
            duration: 100.0,
            signature: "signature".into(),
        };
        let records: Vec<Record> = serde_json::from_str(r#"[
            {"id":1,"trackName":"Song","artistName":"Artist","albumName":"Album","duration":null,"syncedLyrics":"[00:01]Unknown timing"},
            {"id":2,"trackName":"Song","artistName":"Artist","albumName":"Album","plainLyrics":"Missing duration"},
            {"id":3,"trackName":"Song","artistName":"Artist","albumName":"Album","duration":101,"syncedLyrics":"[00:01]Verified timing"}
        ]"#).unwrap();
        assert!(!matching(&records[0], &track));
        assert!(!matching(&records[1], &track));
        let result = select_versions(records, &track);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].source_id.as_deref(), Some("3"));
    }
    #[test]
    fn provider_nul_is_removed_before_cache_and_lrc_parsing() {
        let record:Record=serde_json::from_str(r#"{"id":7,"trackName":"Song","artistName":"Artist","albumName":"Al\u0000bum","duration":100,"plainLyrics":"\u0000Words 🎵","syncedLyrics":"\u0000[00:01]Wo\u0000rds 🎵"}"#).unwrap();
        let lyrics = version(record).unwrap();
        assert_eq!(lyrics.label, "LRCLIB · Album");
        assert_eq!(lyrics.plain_text.as_deref(), Some("Words 🎵"));
        assert_eq!(lyrics.synced_lrc.as_deref(), Some("[00:01]Words 🎵"));
        assert!(!serde_json::to_string(&lyrics).unwrap().contains("\\u0000"));
    }
    #[test]
    fn strict_matching_and_timed_validation() {
        let track = Candidate {
            hash: "hash".into(),
            title: "Song".into(),
            artist: "Artist".into(),
            album: "Album".into(),
            duration: 100.0,
            signature: "x".into(),
        };
        let mut record = Record {
            id: 1,
            track_name: "SONG".into(),
            artist_name: "Artist".into(),
            album_name: Some("Album".into()),
            duration: Some(102.0),
            instrumental: false,
            plain_lyrics: Some("words".into()),
            synced_lyrics: Some("[00:01]words".into()),
        };
        assert!(matching(&record, &track));
        record.duration = Some(102.01);
        assert!(!matching(&record, &track));
        record.synced_lyrics = Some("invalid".into());
        assert!(version(record).unwrap().synced_lrc.is_none());
    }
}
