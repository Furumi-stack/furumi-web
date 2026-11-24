use std::path::Path;

use sqlx::PgPool;

use crate::agent::{mover, rag};
use crate::config::AppConfig;
use crate::scheduler::JobLog;

/// Record the existing audio as the outcome of this upload before closing its review.
pub async fn link_exact_import(
    pool: &PgPool,
    input_path: &str,
    sha256: &str,
    media_id: i64,
) -> anyhow::Result<()> {
    crate::import_playlists::link_media(pool, sha256, media_id).await?;
    sqlx::query(
        "INSERT INTO furumusic__youtube_import_media (item_id, media_file_id)
         SELECT id, $2 FROM furumusic__youtube_download_item
         WHERE inbox_path = $1 OR left($1, length(inbox_path) + 1) = inbox_path || '/'
         ON CONFLICT (item_id, media_file_id) DO NOTHING",
    )
    .bind(input_path)
    .bind(media_id)
    .execute(pool)
    .await?;
    crate::youtube::sync_target_playlists_for_imported_media(pool, media_id).await
}

/// A legacy review can point outside the current inbox. Only consume an inbox
/// copy that still contains the bytes recorded when the review was created.
pub async fn remove_exact_inbox_copy(
    config: &AppConfig,
    source: &Path,
    sha256: &str,
) -> anyhow::Result<bool> {
    if sha256.is_empty() || config.agent_inbox_dir.is_empty() || config.agent_storage_dir.is_empty()
    {
        return Ok(false);
    }
    let canonical = match tokio::fs::canonicalize(source).await {
        Ok(path) => path,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    let inbox = tokio::fs::canonicalize(&config.agent_inbox_dir).await?;
    let library = tokio::fs::canonicalize(&config.agent_storage_dir).await?;
    if !canonical.starts_with(inbox) || canonical.starts_with(library) {
        return Ok(false);
    }
    let before = tokio::fs::symlink_metadata(source).await?;
    if !before.is_file() || mover::sha256_file(source).await? != sha256 {
        return Ok(false);
    }
    let after = tokio::fs::symlink_metadata(source).await?;
    if !same_file_version(&before, &after) {
        return Ok(false);
    }
    tokio::fs::remove_file(source).await?;
    Ok(true)
}

pub(super) fn same_file_version(left: &std::fs::Metadata, right: &std::fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if (left.dev(), left.ino(), left.ctime(), left.ctime_nsec())
            != (right.dev(), right.ino(), right.ctime(), right.ctime_nsec())
        {
            return false;
        }
    }
    left.len() == right.len()
        && left
            .modified()
            .is_ok_and(|t| Some(t) == right.modified().ok())
}

#[derive(sqlx::FromRow)]
pub(super) struct ReviewFile {
    pub id: i64,
    pub input_path: String,
    pub sha256: Option<String>,
    pub status: String,
    pub updated_at: String,
}

pub(super) async fn tracked_files(
    pool: &PgPool,
    paths: &[String],
) -> anyhow::Result<Vec<ReviewFile>> {
    Ok(sqlx::query_as(
        r#"SELECT DISTINCT ON (input_path) id, input_path, status::text, updated_at::text,
               substring(context_json from '"sha256"[[:space:]]*:[[:space:]]*"([0-9a-fA-F]{64})"') AS sha256
           FROM furumusic__pending_review WHERE input_path = ANY($1) ORDER BY input_path, id DESC"#,
    ).bind(paths).fetch_all(pool).await?)
}

pub(super) async fn finish_exact_review(
    pool: &PgPool,
    config: &AppConfig,
    review: &ReviewFile,
    sha256: &str,
    media_id: i64,
) -> anyhow::Result<bool> {
    let mut tx = pool.begin().await?;
    // Serialize against approval, dismissal, or the discovery of different bytes at this path.
    let locked: Option<i64> = sqlx::query_scalar(
        r#"SELECT r.id FROM furumusic__pending_review r
           JOIN furumusic__media_file m ON m.id=$5 AND m.sha256_hash=$4 AND m.file_type='audio'
           JOIN furumusic__track t ON t.audio_file_id=m.id
           WHERE r.id=$1 AND r.status=$2 AND r.input_path=$3
             AND substring(r.context_json from '"sha256"[[:space:]]*:[[:space:]]*"([0-9a-fA-F]{64})"')=$4
             AND NOT EXISTS (SELECT 1 FROM furumusic__dismissed_review d
                             WHERE d.input_path=r.input_path AND d.sha256=lower($4))
           ORDER BY t.id LIMIT 1 FOR UPDATE OF r FOR SHARE OF t,m"#,
    ).bind(review.id).bind(&review.status).bind(&review.input_path).bind(sha256).bind(media_id)
        .fetch_optional(&mut *tx).await?;
    if locked.is_none() {
        return Ok(false);
    }
    link_exact_import(pool, &review.input_path, sha256, media_id).await?;
    if !matches!(review.status.as_str(), "approved" | "auto_approved") {
        sqlx::query(
            "UPDATE furumusic__pending_review SET status='auto_approved', error_message=NULL,
                     updated_at=$2 WHERE id=$1",
        )
        .bind(review.id)
        .bind(chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
        .execute(&mut *tx)
        .await?;
    }
    let source =
        crate::media_paths::resolve_path_from_root(&config.agent_inbox_dir, &review.input_path);
    if let Err(error) = remove_exact_inbox_copy(config, &source, sha256).await {
        tracing::warn!(review_id=review.id, %error, "could not clean up a verified library duplicate");
    }
    tx.commit().await?;
    Ok(true)
}

pub(super) async fn reconcile_open_reviews(
    pool: &PgPool,
    config: &AppConfig,
    log: &mut JobLog,
) -> anyhow::Result<()> {
    // Older manual approvals recorded duplicate decisions as retryable failures.
    sqlx::query("UPDATE furumusic__pending_review
                 SET status='pending', updated_at=$1,
                     error_message=substring(error_message FROM strpos(error_message, 'Duplicate check needs review:'))
                 WHERE (status='failed' OR (status='pending' AND error_message NOT LIKE 'Duplicate check needs review:%'))
                   AND strpos(error_message, 'Duplicate check needs review:') > 0")
        .bind(chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
        .execute(pool).await?;
    // Narrow the queue in PostgreSQL. Unrelated archive media never needs filesystem I/O.
    let matches: Vec<ReviewFile> = sqlx::query_as(
        r#"SELECT id, input_path, sha256, status::text, updated_at::text FROM (
               SELECT r.*, substring(context_json from '"sha256"[[:space:]]*:[[:space:]]*"([0-9a-fA-F]{64})"') AS sha256
               FROM furumusic__pending_review r
               WHERE status IN ('queued','failed','pending') AND input_path IS NOT NULL
           ) review WHERE EXISTS (
               SELECT 1 FROM furumusic__media_file m JOIN furumusic__track t ON t.audio_file_id=m.id
               WHERE m.file_type='audio' AND m.sha256_hash=review.sha256
           ) AND NOT EXISTS (SELECT 1 FROM furumusic__dismissed_review d
                             WHERE d.input_path=review.input_path AND d.sha256=lower(review.sha256))
           ORDER BY id"#,
    ).fetch_all(pool).await?;
    let mut completed = 0;
    for review in matches {
        let Some(sha256) = review.sha256.as_deref() else {
            continue;
        };
        let reconciliation = async {
            let Some(media) =
                rag::verified_audio_media(pool, sha256, &config.agent_storage_dir).await?
            else {
                return Ok(false);
            };
            finish_exact_review(pool, config, &review, sha256, media).await
        }
        .await;
        match reconciliation {
            Ok(true) => completed += 1,
            Ok(false) => {}
            Err(error) => log.warn(&format!(
                "Review {}: could not confirm import in the library: {error:#}",
                review.id
            )),
        }
    }
    if completed > 0 {
        log.info(&format!(
            "Completed {completed} reviews for audio verified in the library"
        ));
    }
    Ok(())
}
