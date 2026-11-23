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
  AND (l.retry_at IS NULL OR l.retry_at<=now())
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
    let cooldown: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM furumusic__lyrics_lookup WHERE signature='_lrclib_cooldown' AND retry_at>now())").fetch_one(&ctx.pool).await?;
    if cooldown {
        log.info("LRCLIB retry cooldown is active.");
        return Ok(());
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
    loop {
        let batch = sqlx::query_as::<_, Candidate>(CANDIDATES)
            .bind(BATCH_SIZE)
            .fetch_all(&ctx.pool)
            .await?;
        if batch.is_empty() {
            break;
        }
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
                    let result = lookup(&client, &track).await;
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
                        let delay = error.retry_after.unwrap_or(300).max(30);
                        // Persist service-wide cooldown, including Retry-After, across runs.
                        for signature in [track.signature.as_str(), "_lrclib_cooldown"] {
                            save_retry(&ctx.pool, signature, delay).await?;
                        }
                        log.info(&format!("LRCLIB temporarily unavailable ({}); saved retry state and stopped requests.",error.message));
                        return Ok(());
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
        "Lyrics complete: {processed} content items, {found} found, {requests} API lookups."
    ));
    Ok(())
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
    album_name: String,
    duration: Option<f64>,
    #[serde(default)]
    instrumental: bool,
    plain_lyrics: Option<String>,
    synced_lyrics: Option<String>,
}
struct LookupError {
    message: String,
    retry_after: Option<u64>,
}
impl From<anyhow::Error> for LookupError {
    fn from(e: anyhow::Error) -> Self {
        Self {
            message: e.to_string(),
            retry_after: None,
        }
    }
}

async fn get_json<T: serde::de::DeserializeOwned>(
    client: &Client,
    endpoint: &str,
    query: &[(&str, String)],
) -> std::result::Result<Option<T>, LookupError> {
    get_json_url(client, &format!("https://lrclib.net/api/{endpoint}"), query).await
}

async fn get_json_url<T: serde::de::DeserializeOwned>(
    client: &Client,
    url: &str,
    query: &[(&str, String)],
) -> std::result::Result<Option<T>, LookupError> {
    let mut response = client
        .get(url)
        .query(query)
        .send()
        .await
        .map_err(|e| LookupError::from(anyhow::Error::from(e)))?;
    if !response.status().is_success() {
        return Err(LookupError {
            message: format!("HTTP {}", response.status()),
            retry_after: response
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse().ok()),
        });
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|e| LookupError::from(anyhow::Error::from(e)))?
    {
        if bytes.len() + chunk.len() > MAX_API_BYTES {
            return Err(anyhow::anyhow!("oversized LRCLIB response").into());
        }
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|e| LookupError::from(anyhow::Error::from(e)))
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
    record.album_name.retain(|c| c != '\0');
    record.plain_lyrics = record.plain_lyrics.map(without_nul);
    record.synced_lyrics = record.synced_lyrics.map(without_nul);
    let synced = record
        .synced_lyrics
        .filter(|v| !crate::lyrics::parse_lrc(v).is_empty());
    let value = LyricsVersion {
        id: format!("lrclib:{}", record.id),
        label: if record.album_name.is_empty() {
            "LRCLIB".into()
        } else {
            format!("LRCLIB · {}", record.album_name)
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
) -> std::result::Result<Vec<LyricsVersion>, LookupError> {
    let query = vec![
        ("track_name", track.title.clone()),
        ("artist_name", track.artist.clone()),
    ];
    // Search returns all text representations in one request; no per-result fetches.
    let records: Vec<Record> = get_json(client, "search", &query)
        .await?
        .unwrap_or_default();
    Ok(select_versions(records, track))
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
                normalized(&r.album_name) == normalized(&track.album),
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
            album_name: "Album".into(),
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
