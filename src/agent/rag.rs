use sqlx::PgPool;

use super::dto::{SimilarArtist, SimilarRelease};

/// Find artists with similar names using pg_trgm.
/// Short names (<3 chars) fall back to ILIKE prefix match.
pub async fn find_similar_artists(
    pool: &PgPool,
    name: &str,
    limit: i32,
) -> anyhow::Result<Vec<SimilarArtist>> {
    if name.chars().count() < 3 {
        let rows: Vec<(i64, String, f32)> = sqlx::query_as(
            "SELECT id, name, 1.0::real AS similarity FROM furumusic__artist \
             WHERE name_sort ILIKE $1 || '%' ORDER BY name LIMIT $2",
        )
        .bind(name.to_lowercase())
        .bind(limit)
        .fetch_all(pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|(id, name, similarity)| SimilarArtist {
                id,
                name,
                similarity,
            })
            .collect())
    } else {
        let rows: Vec<(i64, String, f32)> = sqlx::query_as(
            r#"SELECT id, name, MAX(sim) AS similarity FROM (
                SELECT id, name, similarity(name_sort, $1) AS sim
                FROM furumusic__artist WHERE name_sort % $1
                UNION ALL
                SELECT id, name, 0.01::real AS sim
                FROM furumusic__artist WHERE name_sort ILIKE '%' || $1 || '%'
            ) sub GROUP BY id, name ORDER BY similarity DESC LIMIT $2"#,
        )
        .bind(name.to_lowercase())
        .bind(limit)
        .fetch_all(pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|(id, name, similarity)| SimilarArtist {
                id,
                name,
                similarity,
            })
            .collect())
    }
}

/// Find releases with similar titles using pg_trgm.
pub async fn find_similar_releases(
    pool: &PgPool,
    title: &str,
    limit: i32,
) -> anyhow::Result<Vec<SimilarRelease>> {
    let rows: Vec<(i64, String, Option<i32>, f32)> = sqlx::query_as(
        "SELECT id, title, year, similarity(title_sort, $1) AS similarity \
         FROM furumusic__release WHERE title_sort % $1 \
         ORDER BY similarity DESC LIMIT $2",
    )
    .bind(title.to_lowercase())
    .bind(limit)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(id, title, year, similarity)| SimilarRelease {
            id,
            title,
            year,
            similarity,
        })
        .collect())
}

/// Check if a file with the given SHA-256 hash is actively used in the library.
/// Returns true only if a media_file with this hash exists AND at least one
/// track references it via audio_file_id.  Orphaned media_files (no track)
/// are ignored so that re-discovery is possible after the user deletes
/// artists/releases/tracks.
pub async fn file_hash_exists(pool: &PgPool, sha256: &str) -> anyhow::Result<bool> {
    sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM furumusic__media_file mf \
         JOIN furumusic__track t ON t.audio_file_id = mf.id \
         WHERE mf.sha256_hash = $1)",
    )
    .bind(sha256)
    .fetch_one(pool)
    .await
    .map_err(Into::into)
}

/// Verify the bytes on disk as well as the database hash before dropping an
/// inbox upload. Stale catalog hashes must not discard a replacement file.
pub async fn file_hash_exists_verified(
    pool: &PgPool,
    sha256: &str,
    storage_dir: &str,
) -> anyhow::Result<bool> {
    let paths: Vec<String> = sqlx::query_scalar(
        "SELECT DISTINCT mf.file_path FROM furumusic__media_file mf \
            JOIN furumusic__track t ON t.audio_file_id = mf.id \
            WHERE mf.sha256_hash = $1",
    )
    .bind(sha256)
    .fetch_all(pool)
    .await?;
    for path in paths {
        let path = crate::media_paths::resolve_media_file_path(storage_dir, &path);
        if super::mover::sha256_file(&path)
            .await
            .is_ok_and(|actual| actual == sha256)
        {
            return Ok(true);
        }
    }
    Ok(false)
}
