use std::collections::HashSet;
use std::io::ErrorKind;
use std::path::PathBuf;

use anyhow::{Context, bail};
use sqlx::{FromRow, PgPool, Postgres, Transaction};
use uuid::Uuid;

#[derive(Debug, FromRow)]
struct MediaFileRow {
    id: i64,
    file_type: String,
    file_path: String,
    sha256_hash: String,
}

#[derive(Debug, FromRow)]
struct PlaybackStateRow {
    id: i64,
    current_track_id: Option<i64>,
    position_ms: i32,
    queue_json: String,
    queue_position: i32,
}

#[derive(Debug)]
struct QuarantinedFile {
    original: PathBuf,
    quarantined: PathBuf,
}

#[derive(Debug, Default)]
struct Quarantine {
    root: Option<PathBuf>,
    files: Vec<QuarantinedFile>,
}

pub async fn delete_tracks(
    pool: &PgPool,
    requested_track_ids: &[i64],
    storage_dir: &str,
) -> anyhow::Result<u64> {
    delete_scope(pool, requested_track_ids, &[], storage_dir).await
}

pub async fn delete_releases(
    pool: &PgPool,
    requested_release_ids: &[i64],
    storage_dir: &str,
) -> anyhow::Result<u64> {
    let mut transaction = pool.begin().await?;
    let release_ids: Vec<i64> = sqlx::query_scalar(
        "SELECT id FROM furumusic__release WHERE id = ANY($1) ORDER BY id FOR UPDATE",
    )
    .bind(requested_release_ids)
    .fetch_all(&mut *transaction)
    .await?;
    if release_ids.is_empty() {
        transaction.rollback().await?;
        return Ok(0);
    }
    let track_ids: Vec<i64> = sqlx::query_scalar(
        "SELECT id FROM furumusic__track WHERE release_id = ANY($1) ORDER BY id FOR UPDATE",
    )
    .bind(&release_ids)
    .fetch_all(&mut *transaction)
    .await?;
    delete_locked_scope(transaction, track_ids, release_ids, storage_dir, true).await
}

#[derive(Debug)]
pub struct ReleaseMergeTrack {
    pub id: i64,
    pub track_number: Option<i32>,
    pub disc_number: Option<i32>,
}

#[derive(Debug)]
pub struct ReleaseMergeSpec {
    pub release_ids: Vec<i64>,
    pub target_release_id: i64,
    pub title: String,
    pub title_sort: String,
    pub release_type: String,
    pub year: Option<i32>,
    pub hidden: bool,
    pub cover_file_id: Option<i64>,
    pub artist_ids: Vec<i64>,
    pub tracks: Vec<ReleaseMergeTrack>,
}

#[derive(Debug)]
pub struct ReleaseMergeResult {
    pub merged_releases: u64,
    pub moved_tracks: u64,
}

/// Merge several releases into one while preserving their tracks and media.
///
/// Source cover files are quarantined before the database transaction commits,
/// just like normal library deletion. The cover selected for the destination
/// and any media still referenced elsewhere are retained.
pub async fn merge_releases(
    pool: &PgPool,
    mut spec: ReleaseMergeSpec,
    storage_dir: &str,
) -> anyhow::Result<ReleaseMergeResult> {
    spec.release_ids.retain(|id| *id > 0);
    spec.release_ids.sort_unstable();
    spec.release_ids.dedup();
    if spec.release_ids.len() < 2 {
        bail!("select at least two releases to merge");
    }
    if !spec.release_ids.contains(&spec.target_release_id) {
        bail!("destination release must be part of the selection");
    }

    let mut transaction = pool.begin().await?;
    let locked_release_ids: Vec<i64> = sqlx::query_scalar(
        "SELECT id FROM furumusic__release WHERE id = ANY($1) ORDER BY id FOR UPDATE",
    )
    .bind(&spec.release_ids)
    .fetch_all(&mut *transaction)
    .await?;
    if locked_release_ids != spec.release_ids {
        bail!("one or more selected releases no longer exist; reopen the merge wizard");
    }
    let original_target_cover: Option<i64> =
        sqlx::query_scalar("SELECT cover_file_id FROM furumusic__release WHERE id = $1")
            .bind(spec.target_release_id)
            .fetch_one(&mut *transaction)
            .await?;

    let source_release_ids = spec
        .release_ids
        .iter()
        .copied()
        .filter(|id| *id != spec.target_release_id)
        .collect::<Vec<_>>();
    let locked_track_ids: Vec<i64> = sqlx::query_scalar(
        "SELECT id FROM furumusic__track WHERE release_id = ANY($1) ORDER BY id FOR UPDATE",
    )
    .bind(&spec.release_ids)
    .fetch_all(&mut *transaction)
    .await?;
    let mut requested_track_ids = spec.tracks.iter().map(|track| track.id).collect::<Vec<_>>();
    requested_track_ids.sort_unstable();
    if requested_track_ids.windows(2).any(|ids| ids[0] == ids[1]) {
        bail!("the merge track list contains duplicates");
    }
    if requested_track_ids != locked_track_ids {
        bail!("the selected releases changed; reopen the merge wizard before merging");
    }

    if let Some(cover_file_id) = spec.cover_file_id {
        let valid_cover: Option<i64> = sqlx::query_scalar(
            r#"SELECT r.cover_file_id
                 FROM furumusic__release r
                 JOIN furumusic__media_file mf ON mf.id = r.cover_file_id
                WHERE r.id = ANY($1)
                  AND r.cover_file_id = $2
                  AND mf.file_type = 'cover_art'
                LIMIT 1"#,
        )
        .bind(&spec.release_ids)
        .bind(cover_file_id)
        .fetch_optional(&mut *transaction)
        .await?;
        if valid_cover.is_none() {
            bail!("selected cover does not belong to one of the merged releases");
        }
    }

    let mut seen_artist_ids = HashSet::new();
    spec.artist_ids
        .retain(|id| *id > 0 && seen_artist_ids.insert(*id));
    if !spec.artist_ids.is_empty() {
        let existing_artist_ids: Vec<i64> =
            sqlx::query_scalar("SELECT id FROM furumusic__artist WHERE id = ANY($1) ORDER BY id")
                .bind(&spec.artist_ids)
                .fetch_all(&mut *transaction)
                .await?;
        let mut requested_artist_ids = spec.artist_ids.clone();
        requested_artist_ids.sort_unstable();
        if existing_artist_ids != requested_artist_ids {
            bail!("one or more selected artists no longer exist");
        }
    }

    let now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
    let total_discs = spec
        .tracks
        .iter()
        .filter_map(|track| track.disc_number)
        .max();
    sqlx::query(
        r#"UPDATE furumusic__release
              SET title = $2, title_sort = $3, release_type = $4, year = $5,
                  cover_file_id = $6, total_tracks = $7, total_discs = $8,
                  is_hidden = $9, model_name = NULL, updated_at = $10
            WHERE id = $1"#,
    )
    .bind(spec.target_release_id)
    .bind(&spec.title)
    .bind(&spec.title_sort)
    .bind(&spec.release_type)
    .bind(spec.year)
    .bind(spec.cover_file_id)
    .bind(i32::try_from(spec.tracks.len()).unwrap_or(i32::MAX))
    .bind(total_discs)
    .bind(spec.hidden)
    .bind(&now)
    .execute(&mut *transaction)
    .await?;

    sqlx::query("DELETE FROM furumusic__release_artist WHERE release_id = $1")
        .bind(spec.target_release_id)
        .execute(&mut *transaction)
        .await?;
    for (position, artist_id) in spec.artist_ids.iter().enumerate() {
        sqlx::query(
            "INSERT INTO furumusic__release_artist (release_id, artist_id, position) VALUES ($1, $2, $3)",
        )
        .bind(spec.target_release_id)
        .bind(*artist_id)
        .bind(i32::try_from(position).unwrap_or(i32::MAX))
        .execute(&mut *transaction)
        .await?;
    }

    for track in &spec.tracks {
        sqlx::query(
            r#"UPDATE furumusic__track
                  SET release_id = $1, track_number = $2, disc_number = $3,
                      updated_at = $4
                WHERE id = $5"#,
        )
        .bind(spec.target_release_id)
        .bind(track.track_number)
        .bind(track.disc_number)
        .bind(&now)
        .bind(track.id)
        .execute(&mut *transaction)
        .await?;
    }

    sqlx::query(
        r#"INSERT INTO furumusic__entity_genre_tag
                  (entity_kind, entity_id, genre_id, source, weight, updated_at)
            SELECT 'release', $1, genre_id, source, weight, $3
              FROM furumusic__entity_genre_tag
             WHERE entity_kind = 'release' AND entity_id = ANY($2)
            ON CONFLICT (entity_kind, entity_id, genre_id, source) DO UPDATE
                SET weight = GREATEST(furumusic__entity_genre_tag.weight, EXCLUDED.weight),
                    updated_at = EXCLUDED.updated_at"#,
    )
    .bind(spec.target_release_id)
    .bind(&spec.release_ids)
    .bind(&now)
    .execute(&mut *transaction)
    .await?;

    let extra_media_ids = original_target_cover.into_iter().collect::<Vec<_>>();
    let media_files = deletable_media_files_with_extra(
        &mut transaction,
        &[],
        &source_release_ids,
        &extra_media_ids,
    )
    .await?;
    let quarantine = match quarantine_media_files(storage_dir, &media_files).await {
        Ok(quarantine) => quarantine,
        Err(error) => {
            transaction.rollback().await?;
            return Err(error);
        }
    };
    let deletion = delete_database_rows(
        &mut transaction,
        &[],
        &source_release_ids,
        &media_files,
        true,
    )
    .await;
    if let Err(error) = deletion {
        transaction.rollback().await?;
        restore_quarantine(&quarantine).await;
        return Err(error);
    }
    if let Err(error) = transaction.commit().await {
        restore_quarantine(&quarantine).await;
        return Err(error.into());
    }
    purge_quarantine(&quarantine).await;
    remove_empty_storage_parents(storage_dir, &quarantine.files).await;

    Ok(ReleaseMergeResult {
        merged_releases: u64::try_from(spec.release_ids.len()).unwrap_or(u64::MAX),
        moved_tracks: u64::try_from(spec.tracks.len()).unwrap_or(u64::MAX),
    })
}

async fn delete_scope(
    pool: &PgPool,
    requested_track_ids: &[i64],
    release_ids: &[i64],
    storage_dir: &str,
) -> anyhow::Result<u64> {
    let mut transaction = pool.begin().await?;
    let track_ids: Vec<i64> = sqlx::query_scalar(
        "SELECT id FROM furumusic__track WHERE id = ANY($1) ORDER BY id FOR UPDATE",
    )
    .bind(requested_track_ids)
    .fetch_all(&mut *transaction)
    .await?;
    if track_ids.is_empty() {
        transaction.rollback().await?;
        return Ok(0);
    }
    delete_locked_scope(
        transaction,
        track_ids,
        release_ids.to_vec(),
        storage_dir,
        false,
    )
    .await
}

async fn delete_locked_scope(
    mut transaction: Transaction<'_, Postgres>,
    track_ids: Vec<i64>,
    release_ids: Vec<i64>,
    storage_dir: &str,
    delete_release_rows: bool,
) -> anyhow::Result<u64> {
    let media_files = deletable_media_files(&mut transaction, &track_ids, &release_ids).await?;
    let quarantine = match quarantine_media_files(storage_dir, &media_files).await {
        Ok(quarantine) => quarantine,
        Err(error) => {
            transaction.rollback().await?;
            return Err(error);
        }
    };

    let deletion = delete_database_rows(
        &mut transaction,
        &track_ids,
        &release_ids,
        &media_files,
        delete_release_rows,
    )
    .await;
    let affected = match deletion {
        Ok(affected) => affected,
        Err(error) => {
            transaction.rollback().await?;
            restore_quarantine(&quarantine).await;
            return Err(error);
        }
    };

    if let Err(error) = transaction.commit().await {
        restore_quarantine(&quarantine).await;
        return Err(error.into());
    }
    purge_quarantine(&quarantine).await;
    remove_empty_storage_parents(storage_dir, &quarantine.files).await;
    Ok(affected)
}

async fn deletable_media_files(
    transaction: &mut Transaction<'_, Postgres>,
    track_ids: &[i64],
    release_ids: &[i64],
) -> anyhow::Result<Vec<MediaFileRow>> {
    deletable_media_files_with_extra(transaction, track_ids, release_ids, &[]).await
}

async fn deletable_media_files_with_extra(
    transaction: &mut Transaction<'_, Postgres>,
    track_ids: &[i64],
    release_ids: &[i64],
    extra_media_ids: &[i64],
) -> anyhow::Result<Vec<MediaFileRow>> {
    Ok(sqlx::query_as(
        r#"WITH seed_media(id) AS (
               SELECT audio_file_id FROM furumusic__track WHERE id = ANY($1)
               UNION
               SELECT cover_file_id FROM furumusic__track
                WHERE id = ANY($1) AND cover_file_id IS NOT NULL
               UNION
               SELECT cover_file_id FROM furumusic__release
                WHERE id = ANY($2) AND cover_file_id IS NOT NULL
               UNION
               SELECT UNNEST($3::bigint[])
           ), candidate_media(id) AS (
               SELECT id FROM seed_media
               UNION
               SELECT duplicate.id
                 FROM furumusic__media_file duplicate
                 JOIN furumusic__media_file seed
                   ON duplicate.file_path = seed.file_path
                  AND duplicate.sha256_hash = seed.sha256_hash
                 JOIN seed_media ON seed_media.id = seed.id
           )
           SELECT mf.id, mf.file_type::text AS file_type, mf.file_path,
                  mf.sha256_hash::text AS sha256_hash
             FROM furumusic__media_file mf
             JOIN candidate_media candidate ON candidate.id = mf.id
            WHERE NOT EXISTS (
                      SELECT 1
                        FROM furumusic__track track
                        JOIN furumusic__media_file linked
                          ON linked.id = track.audio_file_id
                          OR linked.id = track.cover_file_id
                       WHERE linked.file_path = mf.file_path
                         AND linked.sha256_hash = mf.sha256_hash
                         AND NOT (track.id = ANY($1))
                  )
              AND NOT EXISTS (
                      SELECT 1
                        FROM furumusic__release release
                        JOIN furumusic__media_file linked
                          ON linked.id = release.cover_file_id
                       WHERE linked.file_path = mf.file_path
                         AND linked.sha256_hash = mf.sha256_hash
                         AND NOT (release.id = ANY($2))
                  )
              AND NOT EXISTS (
                      SELECT 1
                        FROM furumusic__artist artist
                        JOIN furumusic__media_file linked
                          ON linked.id = artist.image_file_id
                       WHERE linked.file_path = mf.file_path
                         AND linked.sha256_hash = mf.sha256_hash
                  )
              AND NOT EXISTS (
                      SELECT 1
                        FROM furumusic__playlist playlist
                        JOIN furumusic__media_file linked
                          ON linked.id = playlist.cover_file_id
                       WHERE linked.file_path = mf.file_path
                         AND linked.sha256_hash = mf.sha256_hash
                  )
            ORDER BY mf.id
            FOR UPDATE OF mf"#,
    )
    .bind(track_ids)
    .bind(release_ids)
    .bind(extra_media_ids)
    .fetch_all(&mut **transaction)
    .await?)
}

async fn quarantine_media_files(
    storage_dir: &str,
    media_files: &[MediaFileRow],
) -> anyhow::Result<Quarantine> {
    if media_files.is_empty() {
        return Ok(Quarantine::default());
    }
    if storage_dir.trim().is_empty() {
        bail!("agent_storage_dir is not configured; refusing to leave deleted tracks on disk");
    }

    let storage_root = crate::media_paths::resolve_config_path_buf(storage_dir);
    if storage_root.parent().is_none() {
        bail!("agent_storage_dir must not be a filesystem root");
    }
    let quarantine_root = storage_root
        .join(".furumusic-trash")
        .join(Uuid::new_v4().to_string());
    let mut quarantine = Quarantine {
        root: Some(quarantine_root.clone()),
        files: Vec::new(),
    };
    let mut seen = HashSet::new();
    let result: anyhow::Result<()> =
        async {
            for media in media_files {
                let original = checked_storage_path(storage_dir, &media.file_path)?;
                let mut paths = vec![original.clone()];
                if media.file_type == "cover_art" {
                    paths.extend(crate::agent::cover_variants::COVER_VARIANTS.iter().map(
                        |variant| crate::agent::cover_variants::variant_path(&original, *variant),
                    ));
                }

                for (index, path) in paths.into_iter().enumerate() {
                    if !seen.insert(path.clone()) {
                        continue;
                    }
                    match tokio::fs::symlink_metadata(&path).await {
                        Ok(metadata) if metadata.is_file() || metadata.file_type().is_symlink() => {
                        }
                        Ok(_) => bail!("media path is not a regular file: {}", path.display()),
                        Err(error) if error.kind() == ErrorKind::NotFound => continue,
                        Err(error) => return Err(error.into()),
                    }
                    tokio::fs::create_dir_all(&quarantine_root).await?;
                    let extension = path
                        .extension()
                        .and_then(|value| value.to_str())
                        .unwrap_or("bin");
                    let quarantined = quarantine_root.join(format!(
                        "{}-{index}-{}.{}",
                        media.id,
                        Uuid::new_v4(),
                        extension
                    ));
                    tokio::fs::rename(&path, &quarantined)
                        .await
                        .with_context(|| format!("failed to remove {}", path.display()))?;
                    quarantine.files.push(QuarantinedFile {
                        original: path,
                        quarantined,
                    });
                }
            }
            Ok(())
        }
        .await;
    match result {
        Ok(()) => Ok(quarantine),
        Err(error) => {
            restore_quarantine(&quarantine).await;
            Err(error)
        }
    }
}

fn checked_storage_path(storage_dir: &str, stored_path: &str) -> anyhow::Result<PathBuf> {
    let resolved = crate::media_paths::resolve_media_file_path(storage_dir, stored_path);
    crate::media_paths::path_for_root(storage_dir, &resolved).with_context(|| {
        format!(
            "refusing to delete media outside agent_storage_dir: {}",
            resolved.display()
        )
    })?;
    Ok(resolved)
}

async fn restore_quarantine(quarantine: &Quarantine) {
    for file in quarantine.files.iter().rev() {
        if let Some(parent) = file.original.parent() {
            let _ = tokio::fs::create_dir_all(parent).await;
        }
        let _ = tokio::fs::rename(&file.quarantined, &file.original).await;
    }
    if let Some(root) = &quarantine.root {
        let _ = tokio::fs::remove_dir_all(root).await;
    }
}

async fn purge_quarantine(quarantine: &Quarantine) {
    if let Some(root) = &quarantine.root
        && let Err(error) = tokio::fs::remove_dir_all(root).await
        && error.kind() != ErrorKind::NotFound
    {
        tracing::warn!(path = %root.display(), error = %error, "failed to purge deleted media quarantine");
    }
    if let Some(parent) = quarantine.root.as_deref().and_then(|root| root.parent()) {
        let _ = tokio::fs::remove_dir(parent).await;
    }
}

async fn remove_empty_storage_parents(storage_dir: &str, files: &[QuarantinedFile]) {
    let storage_root = crate::media_paths::resolve_config_path_buf(storage_dir);
    let mut seen = HashSet::new();
    for file in files {
        let mut current = file.original.parent();
        while let Some(directory) = current {
            if directory == storage_root || !directory.starts_with(&storage_root) {
                break;
            }
            if !seen.insert(directory.to_path_buf()) {
                break;
            }
            match tokio::fs::remove_dir(directory).await {
                Ok(()) => current = directory.parent(),
                Err(_) => break,
            }
        }
    }
}

async fn delete_database_rows(
    transaction: &mut Transaction<'_, Postgres>,
    track_ids: &[i64],
    release_ids: &[i64],
    media_files: &[MediaFileRow],
    delete_release_rows: bool,
) -> anyhow::Result<u64> {
    cleanup_playback_states(transaction, track_ids).await?;

    for table in [
        "furumusic__playlist_track",
        "furumusic__user_liked_track",
        "furumusic__play_history",
        "furumusic__track_popularity_history",
        "furumusic__lastfm_scrobble_outbox",
        "furumusic__track_genre",
        "furumusic__track_artist",
        "furumusic__track_embedding",
    ] {
        let query = format!("DELETE FROM {table} WHERE track_id = ANY($1)");
        sqlx::query(&query)
            .bind(track_ids)
            .execute(&mut **transaction)
            .await?;
    }
    for table in [
        "furumusic__entity_genre_tag",
        "furumusic__external_metadata_id",
        "furumusic__artwork_lookup_state",
    ] {
        let query =
            format!("DELETE FROM {table} WHERE entity_kind = 'track' AND entity_id = ANY($1)");
        sqlx::query(&query)
            .bind(track_ids)
            .execute(&mut **transaction)
            .await?;
    }
    for table in [
        "furumusic__fed_state_like",
        "furumusic__fed_state_playlist_item",
        "furumusic__track_ref",
        "furumusic__listen_event",
    ] {
        let query =
            format!("UPDATE {table} SET local_track_id = NULL WHERE local_track_id = ANY($1)");
        sqlx::query(&query)
            .bind(track_ids)
            .execute(&mut **transaction)
            .await?;
    }

    let tracks_deleted = sqlx::query("DELETE FROM furumusic__track WHERE id = ANY($1)")
        .bind(track_ids)
        .execute(&mut **transaction)
        .await?
        .rows_affected();

    if delete_release_rows {
        for table in [
            "furumusic__entity_genre_tag",
            "furumusic__external_metadata_id",
            "furumusic__artwork_lookup_state",
        ] {
            let query = format!(
                "DELETE FROM {table} WHERE entity_kind = 'release' AND entity_id = ANY($1)"
            );
            sqlx::query(&query)
                .bind(release_ids)
                .execute(&mut **transaction)
                .await?;
        }
        sqlx::query("DELETE FROM furumusic__release_artist WHERE release_id = ANY($1)")
            .bind(release_ids)
            .execute(&mut **transaction)
            .await?;
        sqlx::query("DELETE FROM furumusic__release WHERE id = ANY($1)")
            .bind(release_ids)
            .execute(&mut **transaction)
            .await?;
    }

    let media_ids: Vec<i64> = media_files.iter().map(|media| media.id).collect();
    if !media_ids.is_empty() {
        let media_hashes: Vec<String> = media_files
            .iter()
            .map(|media| media.sha256_hash.clone())
            .collect();
        sqlx::query(
            r#"UPDATE furumusic__youtube_download_item
                  SET status = 'failed', progress_percent = 0,
                      downloaded_bytes = 0, total_bytes = NULL,
                      speed_bytes_per_sec = NULL, eta_seconds = NULL,
                      error = 'Imported library files were deleted; this source can be imported again',
                      completed_at = NULL, updated_at = $2
                WHERE id IN (
                    SELECT item_id
                      FROM furumusic__youtube_import_media
                     WHERE media_file_id = ANY($1)
                )"#,
        )
        .bind(&media_ids)
        .bind(chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string())
        .execute(&mut **transaction)
        .await?;

        let review_ids: Vec<i64> = sqlx::query_scalar(
            r#"SELECT id FROM furumusic__pending_review
                WHERE context_json IS NOT NULL
                  AND substring(
                          context_json
                          from '"sha256"[[:space:]]*:[[:space:]]*"([0-9a-fA-F]{64})"'
                      ) = ANY($1)"#,
        )
        .bind(&media_hashes)
        .fetch_all(&mut **transaction)
        .await?;
        if !review_ids.is_empty() {
            sqlx::query(
                "DELETE FROM furumusic__processing_stats WHERE pending_review_id = ANY($1)",
            )
            .bind(&review_ids)
            .execute(&mut **transaction)
            .await?;
            sqlx::query("DELETE FROM furumusic__pending_review WHERE id = ANY($1)")
                .bind(&review_ids)
                .execute(&mut **transaction)
                .await?;
        }
        sqlx::query(
            "DELETE FROM furumusic__federation_content_id_cache WHERE media_file_id = ANY($1)",
        )
        .bind(&media_ids)
        .execute(&mut **transaction)
        .await?;
        sqlx::query("DELETE FROM furumusic__media_file WHERE id = ANY($1)")
            .bind(&media_ids)
            .execute(&mut **transaction)
            .await?;
    }
    Ok(if delete_release_rows {
        u64::try_from(release_ids.len()).unwrap_or(u64::MAX)
    } else {
        tracks_deleted
    })
}

async fn cleanup_playback_states(
    transaction: &mut Transaction<'_, Postgres>,
    track_ids: &[i64],
) -> anyhow::Result<()> {
    let deleted: HashSet<i64> = track_ids.iter().copied().collect();
    let states: Vec<PlaybackStateRow> = sqlx::query_as(
        "SELECT id, current_track_id, position_ms, queue_json, queue_position FROM furumusic__playback_state FOR UPDATE",
    )
    .fetch_all(&mut **transaction)
    .await?;

    for state in states {
        let mut queue: Vec<i64> = serde_json::from_str(&state.queue_json).unwrap_or_default();
        let original_queue = queue.clone();
        queue.retain(|track_id| !deleted.contains(track_id));
        let current_track_id = state.current_track_id.filter(|id| !deleted.contains(id));
        if queue == original_queue && current_track_id == state.current_track_id {
            continue;
        }
        let queue_position = current_track_id
            .and_then(|current| queue.iter().position(|id| *id == current))
            .map(|position| i32::try_from(position).unwrap_or(i32::MAX))
            .unwrap_or_else(|| {
                if queue.is_empty() {
                    0
                } else {
                    state
                        .queue_position
                        .clamp(0, i32::try_from(queue.len() - 1).unwrap_or(i32::MAX))
                }
            });
        let position_ms = if current_track_id.is_some() {
            state.position_ms
        } else {
            0
        };
        let queue_json = serde_json::to_string(&queue)?;
        sqlx::query(
            r#"UPDATE furumusic__playback_state
                  SET current_track_id = $2, position_ms = $3,
                      queue_json = $4, queue_position = $5
                WHERE id = $1"#,
        )
        .bind(state.id)
        .bind(current_track_id)
        .bind(position_ms)
        .bind(queue_json)
        .bind(queue_position)
        .execute(&mut **transaction)
        .await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_media_paths_outside_storage() {
        assert!(checked_storage_path("/srv/music", "/etc/passwd").is_err());
        assert!(checked_storage_path("/srv/music", "../outside.flac").is_err());
        assert_eq!(
            checked_storage_path("/srv/music", "Artist/Album/01.flac").unwrap(),
            PathBuf::from("/srv/music/Artist/Album/01.flac")
        );
    }

    #[test]
    fn deleted_track_ids_are_removed_from_saved_queue() {
        let deleted = HashSet::from([2_i64, 4]);
        let mut queue = vec![1_i64, 2, 3, 4, 5];
        queue.retain(|track_id| !deleted.contains(track_id));
        assert_eq!(queue, vec![1, 3, 5]);
        let serialized: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&queue).unwrap()).unwrap();
        assert_eq!(serialized, serde_json::json!([1, 3, 5]));
    }
}
