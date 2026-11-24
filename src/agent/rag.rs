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

/// Verify the bytes on disk as well as the database hash before dropping an
/// inbox upload. Stale catalog hashes must not discard a replacement file.
pub async fn verified_audio_media(
    pool: &PgPool,
    sha256: &str,
    storage_dir: &str,
) -> anyhow::Result<Option<i64>> {
    if sha256.is_empty() || storage_dir.is_empty() {
        return Ok(None);
    }
    let paths: Vec<(i64, String)> = sqlx::query_as(
        "SELECT mf.id, mf.file_path FROM furumusic__media_file mf
         WHERE mf.file_type = 'audio' AND mf.sha256_hash = $1
           AND EXISTS (SELECT 1 FROM furumusic__track t WHERE t.audio_file_id = mf.id)
         ORDER BY mf.id",
    )
    .bind(sha256)
    .fetch_all(pool)
    .await?;
    if paths.is_empty() {
        return Ok(None);
    }
    let library = match tokio::fs::canonicalize(storage_dir).await {
        Ok(path) => path,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let mut read_error = None;
    for (id, path) in paths {
        let path = crate::media_paths::resolve_media_file_path(storage_dir, &path);
        // A legacy absolute path or symlink can point straight back to the
        // inbox. It does not provide an independent archived copy.
        let physical = match tokio::fs::canonicalize(&path).await {
            Ok(physical) if physical.starts_with(&library) => physical,
            Ok(_) => continue,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                read_error = Some(
                    anyhow::Error::from(error).context(format!("resolving {}", path.display())),
                );
                continue;
            }
        };
        match super::mover::sha256_file(&physical).await {
            Ok(actual) if actual == sha256 => return Ok(Some(id)),
            Ok(_) => {}
            Err(error)
                if error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) => {}
            Err(error) => read_error = Some(error.context(format!("reading {}", path.display()))),
        }
    }
    match read_error {
        Some(error) => Err(error),
        None => Ok(None),
    }
}
