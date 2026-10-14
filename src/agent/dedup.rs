//! Conservative recording identity checks and in-place library audio upgrades.
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use sqlx::{FromRow, PgPool};

use super::dto::NormalizedFields;

#[derive(Debug)]
pub struct DuplicateReview(pub String);

impl std::fmt::Display for DuplicateReview {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Duplicate check needs review: {}", self.0)
    }
}

impl std::error::Error for DuplicateReview {}

fn review(message: impl Into<String>) -> anyhow::Error {
    DuplicateReview(message.into()).into()
}

#[derive(FromRow)]
struct Candidate {
    track_id: i64,
    media_id: i64,
    file_path: String,
    sha256_hash: String,
    album: String,
    year: Option<i32>,
    artist_names: Vec<String>,
}

#[derive(Debug)]
struct AudioInfo {
    codec: String,
    duration: f64,
    channels: u32,
    sample_rate: u32,
    bit_depth: u32,
    bitrate: u64,
}

impl AudioInfo {
    fn lossless(&self) -> bool {
        matches!(self.codec.as_str(), "flac" | "alac" | "ape" | "tta")
            || self.codec.starts_with("pcm_")
    }

    fn lossy(&self) -> bool {
        matches!(
            self.codec.as_str(),
            "mp3"
                | "mp2"
                | "aac"
                | "opus"
                | "vorbis"
                | "wmav1"
                | "wmav2"
                | "ac3"
                | "eac3"
                | "amr_nb"
                | "amr_wb"
        )
    }
}

#[derive(Debug, PartialEq)]
enum Quality {
    Better,
    Keep,
    Incomparable,
}

fn compare_quality(new: &AudioInfo, old: &AudioInfo) -> Quality {
    if new.channels != old.channels
        || new.channels == 0
        || new.sample_rate == 0
        || old.sample_rate == 0
        || (!new.lossless() && !new.lossy())
        || (!old.lossless() && !old.lossy())
    {
        return Quality::Incomparable;
    }
    if old.lossless() && !new.lossless() {
        return Quality::Keep;
    }
    if new.lossless() && !old.lossless() {
        return if new.sample_rate >= old.sample_rate && new.bit_depth >= 16 {
            Quality::Better
        } else {
            Quality::Incomparable
        };
    }
    if new.lossless() && old.lossless() {
        if new.bit_depth == 0 || old.bit_depth == 0 {
            return Quality::Incomparable;
        }
        if new.sample_rate >= old.sample_rate && new.bit_depth >= old.bit_depth {
            return if new.sample_rate > old.sample_rate || new.bit_depth > old.bit_depth {
                Quality::Better
            } else {
                Quality::Keep
            };
        }
        if new.sample_rate <= old.sample_rate && new.bit_depth <= old.bit_depth {
            return Quality::Keep;
        }
        return Quality::Incomparable;
    }
    // Bitrate alone is comparable only within the same codec and sampling rate.
    if new.codec != old.codec
        || new.sample_rate != old.sample_rate
        || new.bitrate == 0
        || old.bitrate == 0
    {
        return Quality::Incomparable;
    }
    if new.bitrate > old.bitrate + old.bitrate / 10 {
        Quality::Better
    } else {
        Quality::Keep
    }
}

fn durations_match(a: f64, b: f64) -> bool {
    a.is_finite()
        && b.is_finite()
        && a > 0.0
        && b > 0.0
        && (a - b).abs() <= 2.0_f64.min(a.min(b) * 0.01)
}

/// Compare the complete fingerprints, tolerating a small encoder-delay shift.
/// Silence and very short/constant fingerprints cannot establish identity.
fn fingerprints_match(a: &[u32], b: &[u32]) -> bool {
    if a.len().min(b.len()) < 64 {
        return false;
    }
    for fp in [a, b] {
        if fp
            .iter()
            .copied()
            .collect::<std::collections::HashSet<_>>()
            .len()
            < 8
        {
            return false;
        }
    }
    for offset in -8_i32..=8 {
        let left = &a[offset.max(0) as usize..];
        let right = &b[(-offset).max(0) as usize..];
        let length = left.len().min(right.len());
        if length * 100 < a.len().max(b.len()) * 95 {
            continue;
        }
        // Every ~8-second section must agree, not just an identical intro.
        let mut total_errors = 0_u64;
        let mut all_sections_match = true;
        for start in (0..length).step_by(64) {
            let end = (start + 64).min(length);
            let errors: u64 = left[start..end]
                .iter()
                .zip(&right[start..end])
                .map(|(x, y)| u64::from((x ^ y).count_ones()))
                .sum();
            if errors * 100 > (end - start) as u64 * 32 * 10 {
                all_sections_match = false;
                break;
            }
            total_errors += errors;
        }
        if all_sections_match && total_errors * 100 <= length as u64 * 32 * 5 {
            return true;
        }
    }
    false
}

async fn command_output(program: &str, args: &[&str], path: &Path) -> Result<Vec<u8>> {
    let mut command = tokio::process::Command::new(program);
    command.args(args).arg(path).kill_on_drop(true);
    let output = tokio::time::timeout(Duration::from_secs(120), command.output())
        .await
        .context("audio inspection timed out")?
        .with_context(|| format!("cannot run {program}"))?;
    // Debian's fpcalc 1.5.1 can report EOF after producing complete results.
    let legacy_eof = program == "fpcalc"
        && output.status.code() == Some(3)
        && String::from_utf8_lossy(&output.stderr).trim()
            == "ERROR: Error decoding audio frame (End of file)";
    ensure!(
        output.status.success() || legacy_eof,
        "{program} failed: {}",
        String::from_utf8_lossy(&output.stderr)
            .chars()
            .take(300)
            .collect::<String>()
    );
    Ok(output.stdout)
}

async fn probe(path: &Path) -> Result<AudioInfo> {
    let output = command_output("ffprobe", &[
        "-v", "error", "-select_streams", "a:0", "-show_entries",
        "stream=codec_name,sample_rate,bits_per_sample,bits_per_raw_sample,bit_rate,channels,duration:format=duration,bit_rate",
        "-of", "json",
    ], path).await?;
    let info: serde_json::Value = serde_json::from_slice(&output)?;
    let stream = info["streams"]
        .as_array()
        .and_then(|items| items.first())
        .context("file has no audio stream")?;
    let number = |value: &serde_json::Value| {
        value
            .as_f64()
            .or_else(|| value.as_str().and_then(|s| s.parse().ok()))
            .unwrap_or(0.0)
    };
    let duration = number(&stream["duration"]);
    Ok(AudioInfo {
        codec: stream["codec_name"]
            .as_str()
            .context("unknown audio codec")?
            .to_owned(),
        duration: if duration > 0.0 {
            duration
        } else {
            number(&info["format"]["duration"])
        },
        channels: number(&stream["channels"]) as u32,
        sample_rate: number(&stream["sample_rate"]) as u32,
        bit_depth: number(&stream["bits_per_raw_sample"]).max(number(&stream["bits_per_sample"]))
            as u32,
        // Stream bitrate excludes embedded artwork; container bitrate is not
        // evidence of a better lossy encoding.
        bitrate: number(&stream["bit_rate"]) as u64,
    })
}

#[derive(Deserialize)]
struct AudioFingerprint {
    duration: f64,
    fingerprint: Vec<u32>,
}

async fn fingerprint(path: &Path, expected_duration: f64) -> Result<AudioFingerprint> {
    // One chunk covering the whole recording reports actual decoded duration,
    // rather than copying a possibly stale duration from the container header.
    let output = command_output(
        "fpcalc",
        &["-json", "-raw", "-length", "0", "-chunk", "86400"],
        path,
    )
    .await?;
    let result: AudioFingerprint = serde_json::from_slice(&output)?;
    ensure!(
        result.duration.is_finite()
            && result.duration > 0.0
            && (result.duration - expected_duration).abs() <= 0.5,
        "decoded audio duration disagrees with its header; possible truncated source"
    );
    Ok(result)
}

/// Returns true when the upload was merged into an existing track.
/// Ambiguous matches remain in the inbox for review; no duplicate is created.
pub async fn merge_existing(
    pool: &PgPool,
    storage_dir: &str,
    source: &Path,
    fields: &NormalizedFields,
    context: &serde_json::Value,
) -> Result<bool> {
    let Some(artist) = fields.artist.as_deref().filter(|s| !s.trim().is_empty()) else {
        return Ok(false);
    };
    let Some(title) = fields.title.as_deref().filter(|s| !s.trim().is_empty()) else {
        return Ok(false);
    };
    let candidates = sqlx::query_as::<_, Candidate>(
        r#"SELECT t.id AS track_id, m.id AS media_id, m.file_path,
                  m.sha256_hash::text, r.title_sort::text AS album, r.year,
                  ARRAY(SELECT DISTINCT a.name_sort::text FROM furumusic__track_artist ta
                        JOIN furumusic__artist a ON a.id = ta.artist_id
                        WHERE ta.track_id = t.id) AS artist_names
           FROM furumusic__track t
           JOIN furumusic__release r ON r.id = t.release_id
           JOIN furumusic__media_file m ON m.id = t.audio_file_id
           WHERE t.title_sort = $1 AND m.file_type = 'audio'
             AND EXISTS (SELECT 1 FROM furumusic__track_artist ta
                         JOIN furumusic__artist a ON a.id = ta.artist_id
                         WHERE ta.track_id = t.id AND a.name_sort = $2)
           ORDER BY t.id"#,
    )
    .bind(title.trim().to_lowercase())
    .bind(artist.trim().to_lowercase())
    .fetch_all(pool)
    .await?;
    let mut artists: Vec<_> = fields
        .featured_artists
        .iter()
        .map(|s| s.trim().to_lowercase())
        .collect();
    artists.push(artist.trim().to_lowercase());
    artists.sort();
    artists.dedup();
    let candidates: Vec<_> = candidates
        .into_iter()
        .map(|mut candidate| {
            candidate.artist_names.sort();
            candidate.artist_names.dedup();
            candidate
        })
        .filter(|c| {
            fields
                .album
                .as_deref()
                .is_none_or(|album| c.album == album.trim().to_lowercase())
        })
        .collect();
    if candidates.is_empty() {
        return Ok(false);
    }
    let inspected_sha = super::mover::sha256_file(source)
        .await
        .map_err(|e| review(format!("cannot read incoming audio: {e:#}")))?;
    let new_info = probe(source)
        .await
        .map_err(|e| review(format!("cannot inspect incoming audio: {e:#}")))?;
    let mut new_fingerprint = None;
    let mut confirmed = Vec::new();
    let mut ambiguous = Vec::new();
    for candidate in candidates {
        let existing =
            crate::media_paths::resolve_path_from_root(storage_dir, &candidate.file_path);
        let old_info = probe(&existing).await.map_err(|e| {
            review(format!(
                "cannot inspect existing track {}: {e:#}",
                candidate.track_id
            ))
        })?;
        if !durations_match(new_info.duration, old_info.duration) {
            ambiguous.push(format!(
                "track {}: duration differs ({:.3}s vs {:.3}s)",
                candidate.track_id, new_info.duration, old_info.duration
            ));
            continue;
        }
        if candidate.artist_names != artists {
            ambiguous.push(format!(
                "track {}: artist credits differ",
                candidate.track_id
            ));
            continue;
        }
        if fields.album.is_none() || fields.year.zip(candidate.year).is_some_and(|(a, b)| a != b) {
            ambiguous.push(format!(
                "track {}: album or edition year needs confirmation",
                candidate.track_id
            ));
            continue;
        }
        if new_fingerprint.is_none() {
            new_fingerprint = Some(
                fingerprint(source, new_info.duration)
                    .await
                    .map_err(|e| review(format!("cannot fingerprint incoming audio: {e:#}")))?,
            );
        }
        let old_fp = fingerprint(&existing, old_info.duration)
            .await
            .map_err(|e| review(format!("cannot fingerprint existing track: {e:#}")))?;
        let new_fp = new_fingerprint.as_ref().unwrap();
        if (new_fp.duration - old_fp.duration).abs() <= 0.25
            && fingerprints_match(&new_fp.fingerprint, &old_fp.fingerprint)
        {
            confirmed.push((candidate, existing, old_info));
        } else {
            // Same metadata/duration with different sound is an edition or a
            // metadata conflict, not permission to discard either recording.
            ambiguous.push(format!(
                "track {}: decoded duration or audio fingerprint differs",
                candidate.track_id
            ));
        }
    }
    if confirmed.len() > 1 {
        return Err(review(
            "multiple existing tracks match this recording; catalog deduplication needs review",
        ));
    }
    if !ambiguous.is_empty() {
        return Err(review(ambiguous.join("; ")));
    }
    let Some((candidate, old_path, old_info)) = confirmed.pop() else {
        return Ok(false);
    };
    match compare_quality(&new_info, &old_info) {
        Quality::Keep => {
            if super::mover::sha256_file(source).await? != inspected_sha {
                return Err(review("incoming audio changed during duplicate inspection"));
            }
            tokio::fs::remove_file(source).await?;
            tracing::info!(
                track_id = candidate.track_id,
                "duplicate upload merged; existing audio retained"
            );
        }
        Quality::Incomparable => {
            return Err(review(format!(
                "track {} matches; cannot automatically rank incoming {} {}kbps/{}Hz/{}bit/{}ch against existing {} {}kbps/{}Hz/{}bit/{}ch",
                candidate.track_id,
                new_info.codec,
                new_info.bitrate / 1000,
                new_info.sample_rate,
                new_info.bit_depth,
                new_info.channels,
                old_info.codec,
                old_info.bitrate / 1000,
                old_info.sample_rate,
                old_info.bit_depth,
                old_info.channels
            )));
        }
        Quality::Better => {
            let mut checked_context = context.clone();
            checked_context["inspected_sha256"] = serde_json::json!(inspected_sha);
            upgrade(
                pool,
                storage_dir,
                source,
                (&candidate, &old_path),
                &new_info,
                &checked_context,
            )
            .await?
        }
    }
    // A repeated YouTube import must still resolve to this existing media ID.
    if let Some(sha) = context["sha256"].as_str() {
        if let Err(error) = sqlx::query(
            r#"INSERT INTO furumusic__youtube_import_media (item_id, media_file_id)
               SELECT DISTINCT item.id, $1
               FROM furumusic__youtube_download_item item
               JOIN furumusic__pending_review review
                 ON review.input_path = item.inbox_path
                 OR left(review.input_path, length(item.inbox_path) + 1) = item.inbox_path || '/'
               WHERE substring(review.context_json from '"sha256"[[:space:]]*:[[:space:]]*"([0-9a-fA-F]{64})"') = $2
               ON CONFLICT (item_id, media_file_id) DO NOTHING"#,
        ).bind(candidate.media_id).bind(sha).execute(pool).await {
            tracing::warn!(%error, "could not link merged upload to YouTube history");
        }
        if let Err(error) =
            crate::youtube::sync_target_playlists_for_imported_media(pool, candidate.media_id).await
        {
            tracing::warn!(%error, "could not sync merged YouTube import to playlists");
        }
    }
    Ok(true)
}

async fn upgrade(
    pool: &PgPool,
    storage_dir: &str,
    source: &Path,
    candidate_and_path: (&Candidate, &Path),
    info: &AudioInfo,
    context: &serde_json::Value,
) -> Result<()> {
    let (candidate, old_path) = candidate_and_path;
    let ext = source
        .extension()
        .and_then(|s| s.to_str())
        .context("incoming audio has no extension")?;
    let base_stem = old_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("audio")
        .split(" [upgrade-")
        .next()
        .unwrap_or("audio");
    let mut stem_end = base_stem.len().min(160);
    while !base_stem.is_char_boundary(stem_end) {
        stem_end -= 1
    }
    let destination = old_path.with_file_name(format!(
        "{} [upgrade-{}].{}",
        &base_stem[..stem_end],
        uuid::Uuid::new_v4(),
        ext
    ));
    // Keep both original files until the database transaction commits.
    super::mover::publish_copy(source, &destination).await?;
    let mut provenance = context.clone();
    provenance["original_filename"] =
        serde_json::json!(source.file_name().and_then(|s| s.to_str()));
    let result = update_media(
        pool,
        storage_dir,
        &destination,
        candidate,
        info,
        &provenance,
    )
    .await;
    if let Err(error) = result {
        let _ = tokio::fs::remove_file(&destination).await;
        return Err(error);
    }
    // Other catalog media records may still refer to the old physical path.
    let mut old_aliases = vec![
        candidate.file_path.clone(),
        old_path.to_string_lossy().into_owned(),
    ];
    if let Some(relative) = crate::media_paths::media_file_path_for_storage(storage_dir, old_path) {
        old_aliases.push(relative);
    }
    let referenced: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM furumusic__media_file WHERE file_path = ANY($1))",
    )
    .bind(old_aliases)
    .fetch_one(pool)
    .await
    .unwrap_or(true);
    if !referenced && let Err(error) = tokio::fs::remove_file(old_path).await {
        tracing::warn!(?old_path, %error, "upgraded audio; could not remove superseded file");
    }
    tokio::fs::remove_file(source).await?;
    tracing::info!(
        track_id = candidate.track_id,
        media_id = candidate.media_id,
        codec = info.codec,
        bitrate = info.bitrate,
        "existing track audio upgraded without creating a duplicate"
    );
    Ok(())
}

async fn update_media(
    pool: &PgPool,
    storage_dir: &str,
    destination: &Path,
    candidate: &Candidate,
    info: &AudioInfo,
    context: &serde_json::Value,
) -> Result<()> {
    let path = crate::media_paths::media_file_path_for_storage(storage_dir, destination)
        .context("upgraded file is outside storage root")?;
    let sha = super::mover::sha256_file(destination).await?;
    if context["inspected_sha256"].as_str() != Some(sha.as_str()) {
        return Err(review("incoming audio changed while staging the upgrade"));
    }
    let size = tokio::fs::metadata(destination).await?.len() as i64;
    let ext = destination
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_lowercase();
    let mime = match ext.as_str() {
        "mp3" => "audio/mpeg",
        "flac" => "audio/flac",
        "ogg" | "opus" => "audio/ogg",
        "aac" | "m4a" => "audio/mp4",
        "wav" => "audio/wav",
        "aiff" | "aif" => "audio/aiff",
        _ => "application/octet-stream",
    };
    let mut tx = pool.begin().await?;
    let updated = sqlx::query(
        r#"UPDATE furumusic__media_file SET file_path=$1, sha256_hash=$2,
           file_size_bytes=$3, mime_type=$4, audio_format=$5, audio_bitrate=$6,
           audio_sample_rate=$7, audio_bit_depth=$8, created_at=$9,
           original_filename=COALESCE($12, original_filename),
           uploaded_by_user_id=COALESCE($13, uploaded_by_user_id),
           uploader_name=COALESCE($14, uploader_name)
           WHERE id=$10 AND sha256_hash=$11"#,
    )
    .bind(path)
    .bind(&sha)
    .bind(size)
    .bind(mime)
    .bind(ext)
    .bind((info.bitrate > 0).then_some((info.bitrate / 1000) as i32))
    .bind(info.sample_rate as i32)
    .bind((info.bit_depth > 0).then_some(info.bit_depth as i32))
    .bind(chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
    .bind(candidate.media_id)
    .bind(&candidate.sha256_hash)
    .bind(context["original_filename"].as_str())
    .bind(context["uploaded_by_user_id"].as_i64())
    .bind(context["uploader_name"].as_str())
    .execute(&mut *tx)
    .await?;
    ensure!(
        updated.rows_affected() == 1,
        "existing audio changed during the upgrade; retry required"
    );
    sqlx::query(
        "UPDATE furumusic__track SET duration_seconds=$1, updated_at=$2 WHERE audio_file_id=$3",
    )
    .bind(info.duration)
    .bind(chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
    .bind(candidate.media_id)
    .execute(&mut *tx)
    .await?;
    sqlx::query("DELETE FROM furumusic__federation_content_id_cache WHERE media_file_id=$1")
        .bind(candidate.media_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM furumusic__track_embedding WHERE track_id IN (SELECT id FROM furumusic__track WHERE audio_file_id=$1)")
        .bind(candidate.media_id).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn audio(codec: &str, bitrate: u64, rate: u32, depth: u32) -> AudioInfo {
        AudioInfo {
            codec: codec.into(),
            duration: 180.0,
            channels: 2,
            sample_rate: rate,
            bit_depth: depth,
            bitrate,
        }
    }

    #[test]
    fn quality_comparison_preserves_lossless_and_compares_codec_like_for_like() {
        let mp3 = audio("mp3", 128000, 44100, 0);
        let better_mp3 = audio("mp3", 320000, 44100, 0);
        let flac = audio("flac", 700000, 44100, 16);
        assert_eq!(compare_quality(&better_mp3, &mp3), Quality::Better);
        assert_eq!(compare_quality(&mp3, &better_mp3), Quality::Keep);
        assert_eq!(compare_quality(&flac, &better_mp3), Quality::Better);
        assert_eq!(compare_quality(&better_mp3, &flac), Quality::Keep);
        assert_eq!(
            compare_quality(&audio("alac", 1400000, 44100, 16), &flac),
            Quality::Keep
        );
        assert_eq!(
            compare_quality(&audio("opus", 192000, 48000, 0), &mp3),
            Quality::Incomparable
        );
        assert_eq!(
            compare_quality(&audio("flac", 1500000, 96000, 24), &flac),
            Quality::Better
        );
        let mut mono = better_mp3;
        mono.channels = 1;
        assert_eq!(compare_quality(&mono, &mp3), Quality::Incomparable);
        assert_eq!(
            compare_quality(&audio("wavpack", 2000000, 44100, 24), &mp3),
            Quality::Incomparable
        );
    }

    #[test]
    fn durations_reject_unknown_values_and_different_versions() {
        assert!(durations_match(180.0, 180.2));
        assert!(!durations_match(180.0, 184.0));
        assert!(!durations_match(10.0, 11.0));
        assert!(!durations_match(0.0, 0.0));
        assert!(!durations_match(f64::NAN, 180.0));
    }

    #[test]
    fn fingerprints_allow_encoder_shift_but_reject_silence_and_altered_endings() {
        let fp: Vec<u32> = (0..512_u32)
            .map(|n| n.wrapping_mul(2654435761).rotate_left(7))
            .collect();
        let mut shifted = vec![0xdeadbeef; 2];
        shifted.extend(&fp);
        assert!(fingerprints_match(&fp, &shifted));
        let slight_noise: Vec<_> = fp.iter().map(|n| n ^ 1).collect();
        assert!(fingerprints_match(&fp, &slight_noise));
        let other: Vec<_> = fp.iter().map(|n| !n).collect();
        assert!(!fingerprints_match(&fp, &other));
        assert!(!fingerprints_match(&[0; 512], &[0; 512]));
        assert!(!fingerprints_match(&fp[..32], &fp[..32]));
        let mut altered = fp.clone();
        for n in &mut altered[448..] {
            *n = !*n
        }
        assert!(!fingerprints_match(&fp, &altered));
    }

    async fn encode_test_audio(destination: &Path, input: Option<&Path>, bitrate: &str) {
        let mut command = tokio::process::Command::new("ffmpeg");
        command.args(["-hide_banner", "-loglevel", "error", "-y"]);
        if let Some(input) = input {
            command.arg("-i").arg(input);
        } else {
            command.args([
                "-f",
                "lavfi",
                "-i",
                "aevalsrc=0.3*sin(2*PI*(220+10*t)*t)+0.2*sin(2*PI*(440+17*t)*t):s=44100:d=30",
            ]);
        }
        command.args(["-ac", "2", "-b:a", bitrate]).arg(destination);
        let output = command.output().await.unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[tokio::test]
    async fn real_transcodes_match_and_keep_distinct_bitrates() {
        let root =
            std::env::temp_dir().join(format!("furumi-fingerprint-{}", uuid::Uuid::new_v4()));
        tokio::fs::create_dir_all(&root).await.unwrap();
        let source = root.join("source.wav");
        let low = root.join("low.mp3");
        let high = root.join("high.mp3");
        encode_test_audio(&source, None, "128k").await;
        encode_test_audio(&low, Some(&source), "128k").await;
        encode_test_audio(&high, Some(&source), "320k").await;
        let low_info = probe(&low).await.unwrap();
        let high_info = probe(&high).await.unwrap();
        assert!(durations_match(low_info.duration, high_info.duration));
        assert_eq!(compare_quality(&high_info, &low_info), Quality::Better);
        let low_fp = fingerprint(&low, low_info.duration).await.unwrap();
        let high_fp = fingerprint(&high, high_info.duration).await.unwrap();
        assert!(
            fingerprints_match(&low_fp.fingerprint, &high_fp.fingerprint),
            "transcodes did not match"
        );
        let broken = root.join("truncated.mp3");
        let bytes = tokio::fs::read(&high).await.unwrap();
        tokio::fs::write(&broken, &bytes[..bytes.len() / 2])
            .await
            .unwrap();
        let broken_info = probe(&broken).await.unwrap();
        if let Ok(broken_fp) = fingerprint(&broken, broken_info.duration).await {
            assert!((broken_fp.duration - high_fp.duration).abs() > 0.25);
            assert!(!fingerprints_match(
                &broken_fp.fingerprint,
                &high_fp.fingerprint
            ));
        }
        tokio::fs::remove_dir_all(root).await.unwrap();
    }

    #[tokio::test]
    #[ignore = "requires an isolated PostgreSQL socket in FURUMI_DEDUP_TEST_SOCKET"]
    async fn upgrades_preserve_ids_and_database_errors_preserve_both_sources() {
        let socket =
            std::env::var("FURUMI_DEDUP_TEST_SOCKET").expect("isolated test PostgreSQL socket");
        let schema = format!("dedup_test_{}", uuid::Uuid::new_v4().simple());
        let search_path = format!("SET search_path TO {schema}");
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(5)
            .after_connect(move |conn, _| {
                let statement = search_path.clone();
                Box::pin(async move {
                    sqlx::query(&statement).execute(conn).await?;
                    Ok(())
                })
            })
            .connect_with(
                sqlx::postgres::PgConnectOptions::new()
                    .host(&socket)
                    .username("furumi_dedup_test")
                    .database("postgres"),
            )
            .await
            .unwrap();
        sqlx::query(&format!("CREATE SCHEMA {schema}"))
            .execute(&pool)
            .await
            .unwrap();
        sqlx::raw_sql(r#"
            CREATE TABLE furumusic__artist (id bigint PRIMARY KEY, name text, name_sort text);
            CREATE TABLE furumusic__release (id bigint PRIMARY KEY, title_sort text, year int);
            CREATE TABLE furumusic__media_file (id bigint PRIMARY KEY, file_path text, file_type text,
                sha256_hash text, file_size_bytes bigint, mime_type text, audio_format text,
                audio_bitrate int, audio_sample_rate int, audio_bit_depth int, created_at text,
                original_filename text, uploaded_by_user_id bigint, uploader_name text);
            CREATE TABLE furumusic__track (id bigint PRIMARY KEY, release_id bigint,
                audio_file_id bigint, title_sort text, duration_seconds double precision, updated_at text);
            CREATE TABLE furumusic__track_artist (track_id bigint, artist_id bigint, role text);
            CREATE TABLE furumusic__federation_content_id_cache (media_file_id bigint);
            CREATE TABLE furumusic__track_embedding (track_id bigint);
            CREATE TABLE test_likes (track_id bigint REFERENCES furumusic__track(id));
            CREATE TABLE test_playlist_tracks (track_id bigint REFERENCES furumusic__track(id));
            CREATE TABLE furumusic__local_upload (id text PRIMARY KEY, user_id bigint,
                filename text, size_bytes bigint, status text, inbox_path text, error text,
                created_at text, updated_at text, completed_at text);
            INSERT INTO furumusic__artist VALUES (1, 'Artist', 'artist');
            INSERT INTO furumusic__release VALUES (2, 'album', 2002);
            INSERT INTO furumusic__track VALUES (42, 2, 7, 'song', 30, 'before');
            INSERT INTO furumusic__track_artist VALUES (42, 1, 'main');
            INSERT INTO test_likes VALUES (42);
            INSERT INTO test_playlist_tracks VALUES (42);
            INSERT INTO furumusic__federation_content_id_cache VALUES (7);
            INSERT INTO furumusic__track_embedding VALUES (42);
        "#).execute(&pool).await.unwrap();
        let root = std::env::temp_dir().join(format!("furumi-dedup-db-{}", uuid::Uuid::new_v4()));
        let inbox = root.join("incoming");
        let storage = root.join("storage");
        tokio::fs::create_dir_all(&inbox).await.unwrap();
        tokio::fs::create_dir_all(&storage).await.unwrap();
        let master = root.join("master.wav");
        let low = storage.join("song.mp3");
        let high = inbox.join("song.mp3");
        encode_test_audio(&master, None, "128k").await;
        encode_test_audio(&low, Some(&master), "128k").await;
        encode_test_audio(&high, Some(&master), "320k").await;
        let low_hash = super::super::mover::sha256_file(&low).await.unwrap();
        let high_hash = super::super::mover::sha256_file(&high).await.unwrap();
        sqlx::query("INSERT INTO furumusic__media_file (id,file_path,file_type,sha256_hash) VALUES (7,'song.mp3','audio',$1)")
            .bind(&low_hash).execute(&pool).await.unwrap();
        let fields = NormalizedFields {
            artist: Some("Artist".into()),
            album: Some("Album".into()),
            title: Some("Song".into()),
            year: Some(2002),
            ..Default::default()
        };
        let context = serde_json::json!({});
        assert!(
            merge_existing(&pool, storage.to_str().unwrap(), &high, &fields, &context)
                .await
                .unwrap()
        );
        let (track_id, media_id, path, sha): (i64, i64, String, String) = sqlx::query_as(
            "SELECT t.id, m.id, m.file_path, m.sha256_hash FROM furumusic__track t JOIN furumusic__media_file m ON m.id=t.audio_file_id"
        ).fetch_one(&pool).await.unwrap();
        assert_eq!((track_id, media_id), (42, 7));
        assert_eq!(sha, high_hash);
        assert!(!high.exists());
        assert!(!low.exists());
        assert!(storage.join(&path).exists());
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM test_likes WHERE track_id=42")
                .fetch_one(&pool)
                .await
                .unwrap(),
            1
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>(
                "SELECT COUNT(*) FROM test_playlist_tracks WHERE track_id=42"
            )
            .fetch_one(&pool)
            .await
            .unwrap(),
            1
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM furumusic__track_embedding")
                .fetch_one(&pool)
                .await
                .unwrap(),
            0
        );
        // Lower-quality reuploads disappear without changing the catalog file.
        encode_test_audio(&high, Some(&master), "128k").await;
        assert!(
            merge_existing(&pool, storage.to_str().unwrap(), &high, &fields, &context)
                .await
                .unwrap()
        );
        assert!(!high.exists());
        let stored_hash: String =
            sqlx::query_scalar("SELECT sha256_hash FROM furumusic__media_file WHERE id=7")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(stored_hash, high_hash);
        // A codec upgrade updates MIME/path while preserving the same IDs.
        let lossless = inbox.join("song.flac");
        encode_test_audio(&lossless, Some(&master), "128k").await;
        assert!(
            merge_existing(
                &pool,
                storage.to_str().unwrap(),
                &lossless,
                &fields,
                &context
            )
            .await
            .unwrap()
        );
        let mime: String =
            sqlx::query_scalar("SELECT mime_type FROM furumusic__media_file WHERE id=7")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(mime, "audio/flac");
        assert!(!lossless.exists());
        assert!(!storage.join(&path).exists());
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM furumusic__track")
                .fetch_one(&pool)
                .await
                .unwrap(),
            1
        );
        // Guest credit changes must not silently create another track. A
        // primary/guest ordering change with identical credits is a duplicate.
        encode_test_audio(&high, Some(&master), "128k").await;
        let with_guest = NormalizedFields {
            artist: Some("Artist".into()),
            featured_artists: vec!["Guest".into()],
            album: Some("Album".into()),
            title: Some("Song".into()),
            year: Some(2002),
            ..Default::default()
        };
        let error = merge_existing(
            &pool,
            storage.to_str().unwrap(),
            &high,
            &with_guest,
            &context,
        )
        .await
        .unwrap_err();
        assert!(error.downcast_ref::<DuplicateReview>().is_some());
        assert!(high.exists());
        sqlx::raw_sql("INSERT INTO furumusic__artist VALUES (9,'Guest','guest'); INSERT INTO furumusic__track_artist VALUES (42,9,'featuring');")
            .execute(&pool).await.unwrap();
        let flipped = NormalizedFields {
            artist: Some("Guest".into()),
            featured_artists: vec!["Artist".into()],
            album: Some("Album".into()),
            title: Some("Song".into()),
            year: Some(2002),
            ..Default::default()
        };
        assert!(
            merge_existing(&pool, storage.to_str().unwrap(), &high, &flipped, &context)
                .await
                .unwrap()
        );
        assert!(!high.exists());
        sqlx::query("DELETE FROM furumusic__track_artist WHERE artist_id=9")
            .execute(&pool)
            .await
            .unwrap();
        // Reset to the low-quality file, then force a late SQL failure after the
        // media UPDATE. Neither the old file nor the inbox may be deleted.
        encode_test_audio(&low, Some(&master), "128k").await;
        encode_test_audio(&high, Some(&master), "320k").await;
        sqlx::query(
            "UPDATE furumusic__media_file SET file_path='song.mp3', sha256_hash=$1 WHERE id=7",
        )
        .bind(&low_hash)
        .execute(&pool)
        .await
        .unwrap();
        let receipt_id = "87654321-e89b-12d3-a456-426614174000";
        crate::local_uploads::create(
            &pool,
            receipt_id,
            6,
            "song.mp3",
            3,
            "user_uploads/6/batch/song.mp3",
        )
        .await
        .unwrap();
        crate::local_uploads::create(
            &pool,
            receipt_id,
            6,
            "song.mp3",
            3,
            "user_uploads/6/batch/song.mp3",
        )
        .await
        .unwrap();
        crate::local_uploads::mark_queued(&pool, receipt_id, 6)
            .await
            .unwrap();
        assert!(
            crate::local_uploads::successful_receipt(
                &pool,
                6,
                receipt_id,
                "user_uploads/6/batch/song.mp3"
            )
            .await
            .unwrap()
            .is_some()
        );
        assert!(
            crate::local_uploads::successful_receipt(
                &pool,
                7,
                receipt_id,
                "user_uploads/6/batch/song.mp3"
            )
            .await
            .unwrap()
            .is_none()
        );
        assert!(
            crate::local_uploads::create(
                &pool,
                receipt_id,
                7,
                "song.mp3",
                3,
                "user_uploads/7/song.mp3"
            )
            .await
            .is_err()
        );
        sqlx::query("DROP TABLE furumusic__track_embedding")
            .execute(&pool)
            .await
            .unwrap();
        assert!(
            merge_existing(&pool, storage.to_str().unwrap(), &high, &fields, &context)
                .await
                .is_err()
        );
        let hash: String =
            sqlx::query_scalar("SELECT sha256_hash FROM furumusic__media_file WHERE id=7")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(hash, low_hash);
        assert!(low.exists());
        assert!(high.exists());
        sqlx::query(&format!("DROP SCHEMA {schema} CASCADE"))
            .execute(&pool)
            .await
            .unwrap();
        pool.close().await;
        tokio::fs::remove_dir_all(root).await.unwrap();
    }
}
