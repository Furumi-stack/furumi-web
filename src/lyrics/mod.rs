//! Content-bound lyrics persistence. External API policy lives in the job.
use anyhow::Result;
use music_dht::lyrics::{LyricsVersion, MAX_LYRICS_VERSIONS};
use serde::{Deserialize, Serialize};
use sqlx::{PgPool, Row};

#[derive(Debug, Serialize)]
pub struct TrackLyrics {
    pub content_id: Option<String>,
    pub versions: Vec<LyricsVersion>,
}

pub async fn track_hash(
    pool: &PgPool,
    track_id: i64,
    visible_only: bool,
) -> Result<Option<String>> {
    Ok(sqlx::query_scalar(
        "SELECT m.sha256_hash::text FROM furumusic__track t
         JOIN furumusic__release r ON r.id=t.release_id
         JOIN furumusic__media_file m ON m.id=t.audio_file_id
         WHERE t.id=$1 AND (NOT $2 OR (NOT t.is_hidden AND NOT r.is_hidden))",
    )
    .bind(track_id)
    .bind(visible_only)
    .fetch_optional(pool)
    .await?)
}

pub async fn versions(pool: &PgPool, hash: &str) -> Result<Vec<LyricsVersion>> {
    let rows = sqlx::query(
        "SELECT * FROM furumusic__lyrics WHERE audio_sha256=$1
        ORDER BY (NULLIF(trim(synced_lrc),'') IS NOT NULL) DESC,
                 (source='manual') DESC, instrumental, id LIMIT $2",
    )
    .bind(hash)
    .bind(MAX_LYRICS_VERSIONS as i64)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| LyricsVersion {
            id: r.get("id"),
            label: r.get("label"),
            source: r.get("source"),
            source_id: r.get("source_id"),
            language: r.get("language"),
            plain_text: r.get("plain_text"),
            synced_lrc: r.get("synced_lrc"),
            instrumental: r.get("instrumental"),
        })
        .collect())
}

pub async fn for_track(
    pool: &PgPool,
    track_id: i64,
    visible_only: bool,
) -> Result<Option<TrackLyrics>> {
    let Some(hash) = track_hash(pool, track_id, visible_only).await? else {
        return Ok(None);
    };
    let content_id = sqlx::query_scalar("SELECT c.content_id FROM furumusic__track t
        JOIN furumusic__media_file m ON m.id=t.audio_file_id
        JOIN furumusic__federation_content_id_cache c ON c.media_file_id=m.id AND c.sha256_hash=m.sha256_hash
        WHERE t.id=$1").bind(track_id).fetch_optional(pool).await?;
    Ok(Some(TrackLyrics {
        content_id,
        versions: versions(pool, &hash).await?,
    }))
}

pub async fn available_track_ids(
    pool: &PgPool,
    ids: &[i64],
) -> Result<std::collections::HashSet<i64>> {
    Ok(sqlx::query_scalar("SELECT t.id FROM furumusic__track t JOIN furumusic__media_file m ON m.id=t.audio_file_id
        WHERE t.id=ANY($1) AND EXISTS(SELECT 1 FROM furumusic__lyrics l WHERE l.audio_sha256=m.sha256_hash)")
        .bind(ids).fetch_all(pool).await?.into_iter().collect())
}

#[derive(Debug, Deserialize)]
pub struct LyricsEdit {
    pub versions: Vec<LyricsVersion>,
    #[serde(default)]
    pub retry_lookup: bool,
}

pub fn validate_edit(body: &LyricsEdit) -> Result<()> {
    anyhow::ensure!(
        body.versions.len() <= MAX_LYRICS_VERSIONS,
        "at most {MAX_LYRICS_VERSIONS} versions"
    );
    let mut ids = std::collections::HashSet::new();
    for version in &body.versions {
        version.validate()?;
        anyhow::ensure!(ids.insert(&version.id), "duplicate version id");
    }
    anyhow::ensure!(
        serde_json::to_vec(&body.versions)?.len()
            <= music_dht::lyrics::MAX_LYRICS_RESPONSE_BYTES - 1024,
        "combined lyrics exceed the protocol response limit"
    );
    Ok(())
}

/// Atomic replacement; admin edits are local and are never published to LRCLIB.
pub async fn edit(pool: &PgPool, track_id: i64, body: &LyricsEdit) -> Result<()> {
    validate_edit(body)?;
    let hash = track_hash(pool, track_id, false)
        .await?
        .ok_or_else(|| anyhow::anyhow!("track audio not found"))?;
    let mut tx = pool.begin().await?;
    // Serialize manual and background changes to the same content.
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1,0))")
        .bind(&hash)
        .execute(&mut *tx)
        .await?;
    let existing: Vec<serde_json::Value> =
        sqlx::query_scalar("SELECT to_jsonb(l) FROM furumusic__lyrics l WHERE audio_sha256=$1")
            .bind(&hash)
            .fetch_all(&mut *tx)
            .await?;
    let existing: Vec<LyricsVersion> = existing
        .into_iter()
        .map(serde_json::from_value)
        .collect::<std::result::Result<_, _>>()?;
    sqlx::query("DELETE FROM furumusic__lyrics WHERE audio_sha256=$1")
        .bind(&hash)
        .execute(&mut *tx)
        .await?;
    for version in &body.versions {
        let unchanged = existing.iter().any(|old| old == version);
        sqlx::query("INSERT INTO furumusic__lyrics(audio_sha256,id,label,source,source_id,language,plain_text,synced_lrc,instrumental)
            VALUES($1,$2,$3,$9,$4,$5,$6,$7,$8)")
            .bind(&hash).bind(&version.id).bind(&version.label).bind(&version.source_id).bind(&version.language)
            .bind(&version.plain_text).bind(&version.synced_lrc).bind(version.instrumental)
            .bind(if unchanged {version.source.as_str()} else {"manual"}).execute(&mut *tx).await?;
    }
    if body.retry_lookup {
        sqlx::query("DELETE FROM furumusic__lyrics_lookup WHERE signature=(SELECT signature FROM furumusic__lyrics_binding WHERE audio_sha256=$1)").bind(&hash).execute(&mut *tx).await?;
        sqlx::query("DELETE FROM furumusic__lyrics_binding WHERE audio_sha256=$1")
            .bind(&hash)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(())
}

/// Parse standard LRC, repeated timestamps and file-wide offsets.
pub fn parse_lrc(value: &str) -> Vec<(i64, String)> {
    let offset = value
        .lines()
        .find_map(|line| {
            line.trim()
                .strip_prefix("[offset:")?
                .strip_suffix(']')?
                .trim()
                .parse::<i64>()
                .ok()
        })
        .unwrap_or(0)
        .clamp(-3_600_000, 3_600_000);
    let mut lines = Vec::new();
    for line in value.lines() {
        let mut rest = line.trim();
        let mut times = Vec::new();
        while let Some(after) = rest.strip_prefix('[') {
            let Some((tag, tail)) = after.split_once(']') else {
                break;
            };
            let Some((minutes, seconds)) = tag.split_once(':') else {
                break;
            };
            let (Ok(minutes), Ok(seconds)) = (minutes.parse::<u32>(), seconds.parse::<f64>())
            else {
                break;
            };
            if !seconds.is_finite() || !(0.0..60.0).contains(&seconds) || minutes > 1440 {
                break;
            }
            times.push(((f64::from(minutes) * 60.0 + seconds) * 1000.0).round() as i64 + offset);
            rest = tail;
        }
        for time in times {
            lines.push((time.max(0), rest.trim().to_string()));
        }
    }
    lines.sort_by_key(|v| v.0);
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn lrc_timestamps_offsets_and_malformed_metadata() {
        assert_eq!(
            parse_lrc(
                "[ar:Artist]\n[offset:-500]\n[00:02.50][01:03] words\n[00:00.10]start\n[99:no]bad"
            ),
            vec![
                (0, "start".into()),
                (2000, "words".into()),
                (62500, "words".into())
            ]
        );
        assert!(parse_lrc("plain text\n[00:NaN]bad\n[00:60]bad").is_empty());
    }
}
