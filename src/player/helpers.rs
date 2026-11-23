use crate::player::dto::UploaderSummary;
use crate::player::rows::ReleaseUploaderRow;

pub(crate) fn cover_variant_url(file_id: Option<i64>, variant: &str) -> Option<String> {
    file_id.map(|id| format!("/api/player/cover/{id}/{variant}"))
}

pub(super) fn track_cover_variant_url(
    track_cover: Option<i64>,
    release_cover: Option<i64>,
    variant: &str,
) -> Option<String> {
    cover_variant_url(track_cover.or(release_cover), variant)
}

pub(super) async fn load_release_uploaders(
    pool: &sqlx::PgPool,
    release_ids: &[i64],
) -> Result<std::collections::HashMap<i64, Vec<UploaderSummary>>, sqlx::Error> {
    if release_ids.is_empty() {
        return Ok(std::collections::HashMap::new());
    }

    let rows = sqlx::query_as::<_, ReleaseUploaderRow>(
        r#"SELECT t.release_id,
                  COALESCE(mf.uploader_name, 'UFO')::text AS uploader_name,
                  COUNT(*)::bigint AS track_count
           FROM furumusic__track t
           LEFT JOIN furumusic__media_file mf ON mf.id = t.audio_file_id
           WHERE t.release_id = ANY($1) AND t.is_hidden = false
           GROUP BY t.release_id, COALESCE(mf.uploader_name, 'UFO')
           ORDER BY t.release_id, track_count DESC, uploader_name"#,
    )
    .bind(release_ids)
    .fetch_all(pool)
    .await?;

    let mut map: std::collections::HashMap<i64, Vec<UploaderSummary>> =
        std::collections::HashMap::new();
    for row in rows {
        map.entry(row.release_id)
            .or_default()
            .push(UploaderSummary {
                name: row.uploader_name,
                track_count: row.track_count,
            });
    }
    Ok(map)
}
